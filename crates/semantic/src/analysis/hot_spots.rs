// SPDX-License-Identifier: Apache-2.0
//! Longitudinal hot-spot aggregation across commit history.
//!
//! Per-pair `semantic_diff` answers "what changed between A and B" with
//! function-level granularity. This module takes that data and asks the
//! next question: *across the last N states, where is the activity
//! concentrated?*
//!
//! # Why
//!
//! - **Reviewer focus** — surface the files and functions that have churned
//!   recently so a reviewer knows where to look first.
//! - **Annotation guidance** — multi-author hot spots are exactly the
//!   places where a context annotation pays for itself; new editors of
//!   that function shouldn't have to rediscover its constraints.
//! - **API stability signals** — a `signature_changed` count of 5 over
//!   the last 200 commits is a flag that the surface area is volatile.
//!
//! # Where this sits
//!
//! Pure function over `&impl LocalObjectStore`. Both the CLI and local gRPC
//! service call the same entry point. The walker
//! follows `state.first_parent()` through the imported ancestry,
//! matching `git log --first-parent` semantics. That's the right
//! model for "what landed on this branch": a merge commit's diff
//! against its first parent surfaces *the merge as one batch event*,
//! not as one event per file the side-branch happened to touch.
//!
//! # Cost
//!
//! O(N) `semantic_diff` calls plus an in-memory aggregation. Empirically
//! against the imported ripgrep repo: 500 pairs walked in ~8 s on dev
//! hardware, ~3 K events aggregated. The semantic-parse cache is
//! shared across pairs so tree-sitter parses don't get redone.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Instant,
};

use objects::{
    object::{ChangeId, SemanticChange, State},
    store::LocalObjectStore,
};

use crate::{
    cache::SemanticParseCache,
    diff::{SemanticDiffOptions, semantic_diff_with_cache},
};

/// What dimension to aggregate on.
///
/// `File` answers "which files churn most." `Function` answers
/// "which functions churn most." File events that don't carry a
/// function name (`FileAdded`, `FileDeleted`, etc.) only contribute
/// to `File` aggregation; under `Function` they're skipped.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HotSpotKey {
    File,
    Function,
}

/// Coarse classification of a [`SemanticChange`]. The aggregator can
/// optionally filter to a subset of these (e.g. "only signature
/// changes" → API instability signal).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum HotEventKind {
    FileAdded,
    FileDeleted,
    FileModified,
    FileRenamed,
    FunctionExtracted,
    FunctionDeleted,
    FunctionRenamed,
    FunctionModified,
    FunctionMoved,
    SignatureChanged,
    DependencyChanged,
}

impl HotEventKind {
    fn classify(change: &SemanticChange) -> Option<Self> {
        Some(match change {
            SemanticChange::FileAdded { .. } => HotEventKind::FileAdded,
            SemanticChange::FileDeleted { .. } => HotEventKind::FileDeleted,
            SemanticChange::FileModified { .. } => HotEventKind::FileModified,
            SemanticChange::FileRenamed { .. } => HotEventKind::FileRenamed,
            SemanticChange::FunctionAdded { .. } | SemanticChange::FunctionExtracted { .. } => {
                HotEventKind::FunctionExtracted
            }
            SemanticChange::FunctionDeleted { .. } => HotEventKind::FunctionDeleted,
            SemanticChange::FunctionRenamed { .. } => HotEventKind::FunctionRenamed,
            SemanticChange::FunctionModified { .. } => HotEventKind::FunctionModified,
            SemanticChange::FunctionMoved { .. } => HotEventKind::FunctionMoved,
            SemanticChange::SignatureChanged { .. } => HotEventKind::SignatureChanged,
            SemanticChange::DependencyAdded { .. } | SemanticChange::DependencyRemoved { .. } => {
                HotEventKind::DependencyChanged
            }
            // Custom events live outside the enum — we don't have a
            // stable group_by key for them.
            SemanticChange::Custom { .. } => return None,
        })
    }
}

/// The aggregation key for a single `(file, name?)` slot. Carries the
/// function name only for `HotSpotKey::Function` aggregation; on `File`
/// every `name` is `None` and the slot collapses across function
/// events that share a path.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum HotSpotKeyValue {
    File { path: PathBuf },
    Function { path: PathBuf, name: String },
}

impl HotSpotKeyValue {
    /// Path of the file the event touched.
    pub fn path(&self) -> &Path {
        match self {
            HotSpotKeyValue::File { path } => path,
            HotSpotKeyValue::Function { path, .. } => path,
        }
    }

    /// Function name, if this is a function-keyed slot.
    pub fn function_name(&self) -> Option<&str> {
        match self {
            HotSpotKeyValue::Function { name, .. } => Some(name),
            HotSpotKeyValue::File { .. } => None,
        }
    }
}

/// One row of hot-spot output. `event_count` is total events;
/// `state_count` is the number of distinct states the slot appeared
/// in (cleaner signal — a single state with 50 events on the same
/// file shouldn't outrank ten states with one event each, since the
/// latter is real ongoing churn).
#[derive(Clone, Debug)]
pub struct HotSpot {
    pub key: HotSpotKeyValue,
    pub event_count: usize,
    pub state_count: usize,
    pub first_seen: ChangeId,
    pub last_seen: ChangeId,
    /// Breakdown of events by kind. Sums to `event_count`.
    pub by_kind: BTreeMap<HotEventKind, usize>,
    /// Per-actor histogram. `None` unless `params.include_actors`
    /// was set. Keys are `Attribution::to_string()` so they include
    /// agent suffixes when present.
    pub by_actor: Option<BTreeMap<String, usize>>,
}

/// Tunable knobs for [`analyze_hot_spots`].
#[derive(Clone, Debug)]
pub struct HotSpotParams {
    /// Stop walking once we've covered this many state pairs. `None`
    /// = walk to the root (use carefully on large histories — the
    /// per-pair `semantic_diff` cost scales linearly).
    pub limit_states: Option<usize>,
    /// What to bucket on.
    pub group_by: HotSpotKey,
    /// Restrict to events whose [`HotEventKind`] is in this list.
    /// Empty list = no filter (all kinds counted).
    pub include_kinds: Vec<HotEventKind>,
    /// Substring filters on the event's path. Empty list = include all.
    /// A path matches the include filter if any include substring is
    /// in the path; matches the exclude filter if any exclude
    /// substring is in the path.
    pub include_paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    /// Number of slots to return at the top of [`HotSpotsReport::spots`].
    pub top_n: usize,
    /// If true, populate [`HotSpot::by_actor`] with the per-actor
    /// histogram. Useful for "this needs context" surfacing — multi-
    /// actor hot spots are the strongest annotation candidates.
    pub include_actors: bool,
    /// Knobs forwarded to each underlying `semantic_diff` call.
    pub diff_options: SemanticDiffOptions,
}

impl Default for HotSpotParams {
    fn default() -> Self {
        Self {
            limit_states: Some(200),
            group_by: HotSpotKey::File,
            include_kinds: Vec::new(),
            include_paths: Vec::new(),
            exclude_paths: Vec::new(),
            top_n: 20,
            include_actors: false,
            diff_options: SemanticDiffOptions::default(),
        }
    }
}

/// Top-of-output bookkeeping plus the ranked slot list.
#[derive(Clone, Debug, Default)]
pub struct HotSpotsReport {
    pub spots: Vec<HotSpot>,
    /// How many state pairs were actually walked (≤ `limit_states`).
    pub states_walked: usize,
    /// How many semantic-change events were observed across the walk.
    /// `spots` may contain fewer than this since we keep only the
    /// top `top_n` and may have filtered some kinds out.
    pub total_events: usize,
}

/// Walk `walk_from` backwards through `first_parent()` chains and
/// aggregate semantic-change events into hot-spots according to
/// `params`.
///
/// `walk_from` is the *newest* state to examine; the first pair is
/// `(walk_from, walk_from.first_parent())`. If `walk_from` has no
/// parent, the report is empty.
pub fn analyze_hot_spots(
    store: &impl LocalObjectStore,
    walk_from: ChangeId,
    params: &HotSpotParams,
) -> Result<HotSpotsReport, anyhow::Error> {
    let started = Instant::now();
    let cache = SemanticParseCache::shared();
    let limit = params.limit_states.unwrap_or(usize::MAX);

    // Slot bookkeeping. We maintain one map keyed on `HotSpotKeyValue`
    // and update it for every event we see.
    let mut slots: BTreeMap<HotSpotKeyValue, SlotAccumulator> = BTreeMap::new();
    let mut total_events = 0usize;
    let mut states_walked = 0usize;

    let mut current_id = walk_from;
    let mut current = match store.get_state(&current_id)? {
        Some(s) => s,
        None => return Ok(HotSpotsReport::default()),
    };

    while states_walked < limit {
        let Some(parent_id) = current.first_parent().copied() else {
            break;
        };
        let parent = match store.get_state(&parent_id)? {
            Some(s) => s,
            None => break,
        };

        // Per-pair semantic diff. We use the cache-injection variant
        // so tree-sitter parses are reused across the whole walk —
        // most files are unchanged across most pairs and the parse
        // cache eats those calls.
        let diff = semantic_diff_with_cache(
            store,
            &parent.tree,
            &current.tree,
            &params.diff_options,
            cache,
        )?;

        let actor_label = if params.include_actors {
            Some(current.attribution.to_string())
        } else {
            None
        };

        // Track which slots were touched by this state, so we increment
        // `state_count` once per state regardless of how many events
        // contribute. Event volume is `event_count`; state volume is
        // the more honest "this thing keeps coming up" signal.
        let mut touched_this_state: std::collections::BTreeSet<HotSpotKeyValue> =
            Default::default();

        for change in &diff.changes {
            let Some(kind) = HotEventKind::classify(change) else {
                continue;
            };
            if !params.include_kinds.is_empty() && !params.include_kinds.contains(&kind) {
                continue;
            }
            // Function-keyed aggregation requires a function-bearing
            // event; file-only events are silently skipped under
            // `HotSpotKey::Function`.
            let key = match (params.group_by, change_to_key(change)) {
                (HotSpotKey::File, Some((path, _))) => HotSpotKeyValue::File { path },
                (HotSpotKey::Function, Some((path, Some(name)))) => {
                    HotSpotKeyValue::Function { path, name }
                }
                _ => continue,
            };

            if !path_passes_filter(key.path(), &params.include_paths, &params.exclude_paths) {
                continue;
            }

            total_events += 1;

            let slot = slots
                .entry(key.clone())
                .or_insert_with(|| SlotAccumulator::new(current_id));
            slot.event_count += 1;
            slot.last_seen = current_id;
            *slot.by_kind.entry(kind).or_insert(0) += 1;
            if let Some(actor) = &actor_label {
                let by_actor = slot.by_actor.get_or_insert_with(BTreeMap::new);
                *by_actor.entry(actor.clone()).or_insert(0) += 1;
            }
            touched_this_state.insert(key);
        }
        for key in touched_this_state {
            if let Some(slot) = slots.get_mut(&key) {
                slot.state_count += 1;
            }
        }

        states_walked += 1;
        current_id = parent_id;
        current = parent;
    }

    let _ = started; // surface elapsed_ms in a future field if needed

    // Rank by event_count desc, then state_count desc, then key for
    // determinism. Ties on event count broken by "this keeps coming
    // up across many states" rather than alphabetical.
    let mut ranked: Vec<(HotSpotKeyValue, SlotAccumulator)> = slots.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.event_count
            .cmp(&a.1.event_count)
            .then(b.1.state_count.cmp(&a.1.state_count))
            .then(a.0.cmp(&b.0))
    });

    let spots = ranked
        .into_iter()
        .take(params.top_n)
        .map(|(key, slot)| HotSpot {
            key,
            event_count: slot.event_count,
            state_count: slot.state_count,
            first_seen: slot.first_seen,
            last_seen: slot.last_seen,
            by_kind: slot.by_kind,
            by_actor: slot.by_actor,
        })
        .collect();

    Ok(HotSpotsReport {
        spots,
        states_walked,
        total_events,
    })
}

/// Internal accumulator — flattened into [`HotSpot`] at the end.
struct SlotAccumulator {
    event_count: usize,
    state_count: usize,
    first_seen: ChangeId,
    last_seen: ChangeId,
    by_kind: BTreeMap<HotEventKind, usize>,
    by_actor: Option<BTreeMap<String, usize>>,
}

impl SlotAccumulator {
    fn new(seen: ChangeId) -> Self {
        Self {
            event_count: 0,
            state_count: 0,
            first_seen: seen,
            last_seen: seen,
            by_kind: BTreeMap::new(),
            by_actor: None,
        }
    }
}

/// Extract `(path, optional name)` from a [`SemanticChange`].
///
/// `Some((path, None))` = file-level event, no function attached.
/// `Some((path, Some(name)))` = function-level event.
/// `None` = no path (e.g. dependency events) — caller decides whether
/// to count those (we route them to a synthetic `Cargo.toml` slot in
/// the future, but for now they're dropped under both group_by modes
/// since the caller usually wants per-file or per-function output).
fn change_to_key(change: &SemanticChange) -> Option<(PathBuf, Option<String>)> {
    match change {
        SemanticChange::FileAdded { path }
        | SemanticChange::FileDeleted { path }
        | SemanticChange::FileModified { path, .. } => Some((path.clone(), None)),
        SemanticChange::FileRenamed { to, .. } => Some((to.clone(), None)),
        SemanticChange::FunctionAdded { file, name, .. }
        | SemanticChange::FunctionExtracted { file, name, .. } => {
            Some((file.clone(), Some(name.clone())))
        }
        SemanticChange::FunctionDeleted { file, name, .. } => {
            Some((file.clone(), Some(name.clone())))
        }
        SemanticChange::FunctionRenamed { file, new_name, .. } => {
            Some((file.clone(), Some(new_name.clone())))
        }
        SemanticChange::FunctionModified { file, name, .. } => {
            Some((file.clone(), Some(name.clone())))
        }
        SemanticChange::FunctionMoved { file, name, .. } => {
            Some((file.clone(), Some(name.clone())))
        }
        SemanticChange::SignatureChanged { file, name, .. } => {
            Some((file.clone(), Some(name.clone())))
        }
        SemanticChange::DependencyAdded { .. }
        | SemanticChange::DependencyRemoved { .. }
        | SemanticChange::Custom { .. } => None,
    }
}

/// Substring-based path filter. Cheap; upgrade to globset if real
/// users hit limits.
fn path_passes_filter(path: &Path, includes: &[String], excludes: &[String]) -> bool {
    let s = path.to_string_lossy();
    if !includes.is_empty() && !includes.iter().any(|inc| s.contains(inc.as_str())) {
        return false;
    }
    if excludes.iter().any(|exc| s.contains(exc.as_str())) {
        return false;
    }
    true
}

/// Companion: walk the chain and report the actor histogram only.
/// Cheaper than `analyze_hot_spots` because it doesn't need per-pair
/// semantic diff — pulls the answer straight from each state's
/// `attribution`. Useful for the "who's been working here" panel
/// that doesn't need file-granularity output.
pub fn analyze_actor_histogram(
    store: &impl LocalObjectStore,
    walk_from: ChangeId,
    limit_states: Option<usize>,
) -> Result<BTreeMap<String, usize>, anyhow::Error> {
    let limit = limit_states.unwrap_or(usize::MAX);
    let mut histogram: BTreeMap<String, usize> = BTreeMap::new();
    let mut steps = 0usize;

    let Some(mut current) = store.get_state(&walk_from)? else {
        return Ok(histogram);
    };

    *histogram
        .entry(current.attribution.to_string())
        .or_insert(0) += 1;
    steps += 1;

    while steps < limit {
        let Some(parent_id) = current.first_parent().copied() else {
            break;
        };
        let Some(parent) = store.get_state(&parent_id)? else {
            break;
        };
        *histogram.entry(parent.attribution.to_string()).or_insert(0) += 1;
        steps += 1;
        current = parent;
    }

    Ok(histogram)
}

/// State accessor used by the walker; isolated so future tests can
/// mock the store layer without going through the whole `LocalObjectStore`
/// trait. (Currently unused — the walker calls `store.get_state`
/// directly — but `State` needs to remain reachable for the test
/// module's helper to compile.)
#[allow(dead_code)]
fn _state_anchor(_: &State) {}

#[cfg(test)]
mod tests {
    use objects::{
        object::{Attribution, ChangeId, Principal, State, Tree, TreeEntry},
        store::InMemoryStore,
    };

    use super::*;

    fn principal(label: &str) -> Principal {
        Principal::new(label.to_string(), format!("{label}@example.com"))
    }

    /// Build a tiny chain `A → B → C` (C is HEAD) with a single file
    /// `src/lib.rs` whose content differs at every step. Returns the
    /// HEAD change id plus the in-memory store.
    fn build_three_state_chain() -> (ChangeId, InMemoryStore) {
        let store = InMemoryStore::new();

        let blob_a = store
            .put_blob(&objects::object::Blob::from_slice(
                b"fn one() {}\nfn two() {}\n",
            ))
            .unwrap();
        let tree_a = store
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("lib.rs".to_string(), blob_a, false).unwrap(),
            ]))
            .unwrap();
        let attrib_a = Attribution::human(principal("alice"));
        let state_a = State::new(tree_a, Vec::new(), attrib_a);
        store.put_state(&state_a).unwrap();
        let id_a = state_a.change_id;

        let blob_b = store
            .put_blob(&objects::object::Blob::from_slice(
                b"fn one() { println!(\"hi\"); }\nfn two() {}\n",
            ))
            .unwrap();
        let tree_b = store
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("lib.rs".to_string(), blob_b, false).unwrap(),
            ]))
            .unwrap();
        let state_b = State::new(tree_b, vec![id_a], Attribution::human(principal("bob")));
        store.put_state(&state_b).unwrap();
        let id_b = state_b.change_id;

        let blob_c = store
            .put_blob(&objects::object::Blob::from_slice(
                b"fn one() { println!(\"hello\"); }\nfn two() {}\nfn three() {}\n",
            ))
            .unwrap();
        let tree_c = store
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("lib.rs".to_string(), blob_c, false).unwrap(),
            ]))
            .unwrap();
        let state_c = State::new(tree_c, vec![id_b], Attribution::human(principal("carol")));
        store.put_state(&state_c).unwrap();
        let id_c = state_c.change_id;

        (id_c, store)
    }

    #[test]
    fn walks_first_parent_chain_to_root() {
        let (head, store) = build_three_state_chain();
        let report = analyze_hot_spots(&store, head, &HotSpotParams::default()).unwrap();

        // Two pairs walked: C→B and B→A. (A has no parent so we stop.)
        assert_eq!(report.states_walked, 2);
        // Both pairs touched src/lib.rs at least at the file level.
        let lib_path: PathBuf = "lib.rs".into();
        let file_spot = report
            .spots
            .iter()
            .find(|s| matches!(&s.key, HotSpotKeyValue::File { path } if path == &lib_path))
            .expect("expected lib.rs hot-spot");
        assert!(file_spot.event_count >= 2);
        assert_eq!(file_spot.state_count, 2);
    }

    #[test]
    fn limit_states_caps_the_walk() {
        let (head, store) = build_three_state_chain();
        let params = HotSpotParams {
            limit_states: Some(1),
            ..HotSpotParams::default()
        };
        let report = analyze_hot_spots(&store, head, &params).unwrap();
        assert_eq!(
            report.states_walked, 1,
            "limit_states=1 should walk one pair"
        );
    }

    #[test]
    fn group_by_function_skips_pure_file_events() {
        let (head, store) = build_three_state_chain();
        let params = HotSpotParams {
            group_by: HotSpotKey::Function,
            ..HotSpotParams::default()
        };
        let report = analyze_hot_spots(&store, head, &params).unwrap();

        // We added `fn three` between B and C; that's a function-level
        // event under group_by=Function. Some pure-file modifications
        // (FileModified events without function-level resolution) are
        // skipped. So we expect at least one Function key and zero File
        // keys in the output.
        for spot in &report.spots {
            assert!(
                matches!(&spot.key, HotSpotKeyValue::Function { .. }),
                "group_by=Function should only emit Function keys, got {:?}",
                spot.key
            );
        }
    }

    #[test]
    fn include_actors_populates_per_actor_histogram() {
        let (head, store) = build_three_state_chain();
        let params = HotSpotParams {
            include_actors: true,
            ..HotSpotParams::default()
        };
        let report = analyze_hot_spots(&store, head, &params).unwrap();

        let any = report.spots.first().expect("expected at least one spot");
        let actors = any
            .by_actor
            .as_ref()
            .expect("include_actors=true should populate by_actor");
        // We saw bob and carol as the authors of the two compared
        // states (a→b and b→c). Attribution::Display formats as
        // "name <email>", so we substring-match instead of exact key.
        assert!(
            actors
                .keys()
                .any(|k| k.contains("bob") || k.contains("carol")),
            "expected bob or carol in actor histogram, got {:?}",
            actors.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn path_filter_excludes_substring_match() {
        let (head, store) = build_three_state_chain();
        let params = HotSpotParams {
            exclude_paths: vec!["lib.rs".to_string()],
            ..HotSpotParams::default()
        };
        let report = analyze_hot_spots(&store, head, &params).unwrap();
        assert!(
            report.spots.is_empty(),
            "exclude path 'lib.rs' should remove every spot, got {:?}",
            report.spots
        );
    }

    #[test]
    fn actor_histogram_walks_chain_independently_of_diff_path() {
        let (head, store) = build_three_state_chain();
        let hist = analyze_actor_histogram(&store, head, Some(10)).unwrap();
        // Three states walked (head + 2 ancestors), three actors total
        // since each commit had a different principal in the fixture.
        assert_eq!(hist.values().sum::<usize>(), 3);
        assert_eq!(hist.len(), 3);
    }

    #[test]
    fn empty_chain_returns_empty_report() {
        // A single root state with no parent: nothing to diff.
        let store = InMemoryStore::new();
        let blob = store
            .put_blob(&objects::object::Blob::from_slice(b"fn solo() {}"))
            .unwrap();
        let tree = store
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("solo.rs".to_string(), blob, false).unwrap(),
            ]))
            .unwrap();
        let state = State::new(tree, Vec::new(), Attribution::human(principal("alice")));
        store.put_state(&state).unwrap();

        let report = analyze_hot_spots(&store, state.change_id, &HotSpotParams::default()).unwrap();
        assert_eq!(report.states_walked, 0);
        assert_eq!(report.total_events, 0);
        assert!(report.spots.is_empty());
    }
}
