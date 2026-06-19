// SPDX-License-Identifier: Apache-2.0
//! Write [`ReasoningPoint`]s onto imported states as context annotations.
//!
//! The extractor produces notecards; the matcher pins each session to a
//! git commit; this module lands the notecards on the Heddle [`State`] that
//! was already written for that commit.
//!
//! # Why in-place mutation
//!
//! Live annotation flows (see `cli`'s `context annotate`) create a
//! *new* state whose parent is the one being annotated — the annotation
//! is itself a commit in Heddle's graph. That's the right model for a user
//! at the keyboard.
//!
//! Ingest is retrospective. We're filling in reasoning that conceptually
//! existed *at the time the git commit was made*. Threading a ghost
//! state between every imported commit and its children would:
//!
//! 1. Warp the commit graph — every thread tip would land on a synthetic
//!    annotation state rather than the user's actual commit.
//! 2. Explode the state count (one extra per annotation-bearing commit).
//! 3. Make a second import pass non-idempotent — re-run would double.
//!
//! So ingest mutates state content in place. The state's
//! [`ChangeId`](objects::object::ChangeId) is stable; only its
//! `context` field and derived `content_hash` change. Consumers that
//! compare states by `change_id` (parent chains, refs, oplog) are
//! unaffected. Consumers that compare by content hash (merkle
//! verification) see the context-bearing version.
//!
//! # Dedup
//!
//! `emit_for_commit` is idempotent in the face of identical points. If
//! the target's existing [`ContextBlob`] already contains an annotation
//! with the same `(content, kind, attribution)` triple, the new one is
//! dropped. This makes re-imports stable — running the matcher +
//! extractor twice doesn't duplicate notecards.
//!
//! # Attribution
//!
//! Annotations are tagged `ingest:{provider}:{session_short}` so the
//! reviewer can see at a glance that the text came from a transcript
//! mining pass (not a human). The full session id survives in tags
//! (`session:{session_id}`), so operators can traverse from an
//! annotation back to the transcript on disk.

use std::collections::HashMap;

use chrono::Utc;
use objects::{
    object::{Annotation, AnnotationScope, AnnotationStatus, ContextBlob, ContextTarget},
    store::LocalObjectStore,
};
use repo::Repository;
use tracing::debug;

use crate::{
    reasoning::{ReasoningPoint, ReasoningTarget},
    sha_map::ShaMap,
};

/// Counters for an emission pass. All cumulative across `emit_for_commit`
/// calls on the same emitter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReasoningEmitStats {
    /// Annotations actually appended to a `ContextBlob`.
    pub annotations_written: usize,
    /// Distinct states that received at least one new annotation.
    pub states_updated: usize,
    /// `(content, kind, attribution)` matched an existing annotation on
    /// the target — skipped to keep re-imports idempotent.
    pub deduped: usize,
    /// Point's `git_sha` wasn't in the sha map (commit wasn't imported,
    /// or the map is stale). Point dropped.
    pub skipped_unmapped: usize,
    /// Point mapped to a ChangeId but no state exists at that id. The
    /// sha map and the store have drifted — bug-level condition.
    pub skipped_missing_state: usize,
    /// Point's text was empty or over the 140-char budget. Dropped.
    pub skipped_malformed: usize,
}

/// Land `ReasoningPoint`s as annotations on the corresponding Heddle states.
pub struct ReasoningEmitter<'a> {
    repo: &'a Repository,
    sha_map: &'a ShaMap,
    stats: ReasoningEmitStats,
}

impl<'a> ReasoningEmitter<'a> {
    pub fn new(repo: &'a Repository, sha_map: &'a ShaMap) -> Self {
        Self {
            repo,
            sha_map,
            stats: ReasoningEmitStats::default(),
        }
    }

    /// Emit a batch of points that all target the same git commit.
    ///
    /// Points are grouped by their `target.file`; each distinct file
    /// becomes one `ContextTarget::File` entry in the state's context
    /// tree. Points with an empty `target.file` become a single
    /// state-level entry (`ContextTarget::State { change_id }`).
    ///
    /// Returns the new `context_root` hash on success. Returns `Ok(None)`
    /// only if every point in the batch was skipped (malformed, unmapped,
    /// or a dupe) — the state wasn't mutated.
    pub fn emit_for_commit(
        &mut self,
        git_sha: &str,
        points: &[ReasoningPoint],
    ) -> crate::Result<Option<objects::object::ContentHash>> {
        if points.is_empty() {
            return Ok(None);
        }

        // Filter out malformed points up front. Extractor stage-2 should
        // already have enforced the budget, but the emitter is the last
        // guard before write — better to count them than to produce
        // oversized blobs.
        let mut clean: Vec<&ReasoningPoint> = Vec::with_capacity(points.len());
        for p in points {
            if !p.is_well_formed() {
                self.stats.skipped_malformed += 1;
                continue;
            }
            clean.push(p);
        }
        if clean.is_empty() {
            return Ok(None);
        }

        // Resolve the git commit to its Heddle change_id. If the ShaMap
        // doesn't know this SHA, the commit wasn't imported; count the
        // whole batch as unmapped and bail.
        let Some(change_id) = self.sha_map.get_commit(git_sha) else {
            self.stats.skipped_unmapped += clean.len();
            return Ok(None);
        };

        let store = self.repo.store();
        let Some(mut state) = store.get_state(&change_id)? else {
            // ShaMap promises this change_id exists; if the store
            // disagrees, something (prune? bad map?) has gone wrong.
            self.stats.skipped_missing_state += clean.len();
            return Ok(None);
        };

        // Bucket by target path. Empty-path points share one state-level
        // bucket keyed on "". The HashMap preserves insertion order well
        // enough for the write loop; we don't rely on it for determinism
        // of the stored tree (the tree itself is sorted by path).
        let mut by_file: HashMap<String, Vec<&ReasoningPoint>> = HashMap::new();
        for p in &clean {
            by_file
                .entry(normalize_worktree_path(&p.target.file))
                .or_default()
                .push(p);
        }

        let mut context_root = state.context;
        let mut any_written = false;

        for (file, bucket) in by_file {
            let target = if file.is_empty() {
                // State-scope: only file-scope annotations are allowed on
                // this target kind (validate_scope enforces it).
                ContextTarget::state(change_id)
            } else {
                match ContextTarget::file(file.clone()) {
                    Ok(t) => t,
                    Err(e) => {
                        debug!(file = %file, error = %e, "skipping point with unusable file path");
                        self.stats.skipped_malformed += bucket.len();
                        continue;
                    }
                }
            };

            // Load the existing blob at this target (if any) so we can
            // dedup and append rather than overwriting a live history.
            let mut blob = match &context_root {
                Some(root) => self
                    .repo
                    .get_context_blob(root, &target)?
                    .unwrap_or_else(|| ContextBlob::new(vec![])),
                None => ContextBlob::new(vec![]),
            };

            let mut wrote_here = false;

            for p in bucket {
                let annotation_kind = p.kind;
                let scope = match target_to_scope(&target, &p.target) {
                    Ok(s) => s,
                    Err(_) => {
                        // State-scope targets only accept file-scope
                        // annotations; drop symbol/line points that
                        // somehow lost their file path.
                        self.stats.skipped_malformed += 1;
                        continue;
                    }
                };
                let this_attribution = attribution_for(p);

                if blob.annotations.iter().any(|a| {
                    a.status == AnnotationStatus::Active
                        && a.current_revision().is_some_and(|r| {
                            r.kind == annotation_kind
                                && r.content == p.text
                                && r.attribution == this_attribution
                        })
                }) {
                    self.stats.deduped += 1;
                    continue;
                }

                let tags = tags_for(p);
                let annotation = Annotation::new(
                    scope,
                    annotation_kind,
                    p.text.clone(),
                    tags,
                    this_attribution,
                    Utc::now().timestamp(),
                    None, // source_hash: we don't resolve bytes at ingest time
                    Some(change_id),
                );
                blob.annotations.push(annotation);
                self.stats.annotations_written += 1;
                wrote_here = true;
                any_written = true;
            }

            if wrote_here {
                // Keep chronology readable: newest annotations at the
                // end. We appended in order above, so no sort needed.
                let new_root = self
                    .repo
                    .set_context_blob(context_root.as_ref(), &target, &blob)?;
                context_root = Some(new_root);
            }
        }

        if any_written {
            state.context = context_root;
            store.put_state(&state)?;
            self.stats.states_updated += 1;
            Ok(state.context)
        } else {
            Ok(None)
        }
    }

    pub fn stats(&self) -> ReasoningEmitStats {
        self.stats
    }
}

/// Compute the `AnnotationScope` for a point, validated against the
/// target kind. State-scope targets reject symbol/line scopes — the
/// schema only allows file-level guidance on a state.
fn target_to_scope(
    target: &ContextTarget,
    rt: &ReasoningTarget,
) -> Result<AnnotationScope, objects::object::ContextError> {
    let scope = if let Some(sym) = rt.symbol.as_ref().filter(|s| !s.is_empty()) {
        AnnotationScope::Symbol {
            name: sym.clone(),
            resolved_lines: rt.line_range,
        }
    } else if let Some((start, end)) = rt.line_range {
        AnnotationScope::Lines(start, end)
    } else {
        AnnotationScope::File
    };
    target.validate_scope(&scope)?;
    Ok(scope)
}

fn attribution_for(p: &ReasoningPoint) -> String {
    let short = p
        .evidence
        .session_id
        .get(..8)
        .unwrap_or(p.evidence.session_id.as_str());
    format!("ingest:{}:{}", p.evidence.provider, short)
}

fn tags_for(p: &ReasoningPoint) -> Vec<String> {
    let mut tags = vec![
        "ingest".to_string(),
        format!("provider:{}", p.evidence.provider),
        format!("session:{}", p.evidence.session_id),
        format!("confidence:{:.2}", p.confidence),
    ];
    if let Some((slug, _)) = strip_worktree_prefix(&p.target.file) {
        tags.push(format!("worktree:{slug}"));
    }
    tags
}

/// Strip a leading `.claude/worktrees/<slug>/` or `.codex/worktrees/<slug>/`
/// prefix from a reasoning target path, falling back to the raw path when
/// no worktree prefix is present.
///
/// Agent sessions that run inside a worktree checkout record tool-use file
/// paths relative to the worktree root (e.g.
/// `.claude/worktrees/<slug>/crates/x.rs`). The transcript matcher already
/// tail-matches those against commit files via
/// `touch_matches_commit_file`, so the emitter receives a valid point —
/// but without normalization it would land the annotation under the
/// worktree-shaped path rather than the repo-native one, and
/// `context get --path crates/x.rs` at HEAD would miss it.
fn normalize_worktree_path(path: &str) -> String {
    strip_worktree_prefix(path)
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| path.to_string())
}

/// Returns `(slug, repo_relative_path)` when `path` begins with a
/// recognized agent worktree prefix. Used by both `normalize_worktree_path`
/// (for target bucketing) and `tags_for` (to stamp the session's slug onto
/// the annotation).
fn strip_worktree_prefix(path: &str) -> Option<(String, String)> {
    for root in [".claude/worktrees/", ".codex/worktrees/"] {
        let Some(rest) = path.strip_prefix(root) else {
            continue;
        };
        let Some(sep) = rest.find('/') else { continue };
        let (slug, tail_with_slash) = rest.split_at(sep);
        let tail = &tail_with_slash[1..];
        if !slug.is_empty() && !tail.is_empty() {
            return Some((slug.to_string(), tail.to_string()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use objects::object::AnnotationKind;
    use repo::Repository;
    use tempfile::TempDir;

    use super::*;
    use crate::reasoning::{ReasoningEvidence, ReasoningTarget};

    /// Build a minimal repo, mint one state, record it in the shamap,
    /// and return (repo, map, git_sha, change_id). Gives every test a
    /// clean slate without repeating boilerplate.
    fn seed_repo_with_one_state() -> (
        TempDir,
        Repository,
        ShaMap,
        String,
        objects::object::ChangeId,
    ) {
        use objects::object::{Attribution, Blob, Principal, State, Tree};

        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        // An empty tree + trivial state so the store isn't barren.
        let tree_hash = repo.store().put_tree(&Tree::new()).unwrap();
        let state = State::new(
            tree_hash,
            vec![],
            Attribution::human(Principal::new("Test", "test@example.com")),
        );
        let change_id = state.change_id;
        repo.store().put_state(&state).unwrap();

        // Flush a blob so the store's blob path is exercised under
        // context encode/decode.
        let _ = repo.store().put_blob(&Blob::new(b"sentinel".to_vec()));

        let mut map = ShaMap::new();
        let git_sha = "a".repeat(40);
        map.insert_commit(&git_sha, change_id).unwrap();

        (dir, repo, map, git_sha, change_id)
    }

    fn point(
        kind: AnnotationKind,
        text: &str,
        file: &str,
        symbol: Option<&str>,
        lines: Option<(u32, u32)>,
        session: &str,
    ) -> ReasoningPoint {
        ReasoningPoint {
            kind,
            text: text.to_string(),
            target: ReasoningTarget {
                file: file.to_string(),
                symbol: symbol.map(String::from),
                line_range: lines,
            },
            evidence: ReasoningEvidence {
                session_id: session.to_string(),
                turn_range: (0, 0),
                commit_sha: "a".repeat(40),
                provider: "claude".into(),
            },
            confidence: 0.8,
        }
    }

    #[test]
    fn writes_file_scope_annotation_onto_state() {
        let (_dir, repo, map, sha, change_id) = seed_repo_with_one_state();
        let mut emit = ReasoningEmitter::new(&repo, &map);

        let pts = vec![point(
            AnnotationKind::Invariant,
            "never bypass the tenant scope",
            "src/auth.rs",
            None,
            None,
            "session-a",
        )];
        let root = emit.emit_for_commit(&sha, &pts).unwrap();
        assert!(root.is_some());

        let s = emit.stats();
        assert_eq!(s.annotations_written, 1);
        assert_eq!(s.states_updated, 1);
        assert_eq!(s.skipped_unmapped, 0);

        // Read back through the repo to confirm the blob landed on the
        // right target.
        let state = repo.store().get_state(&change_id).unwrap().unwrap();
        let context_root = state.context.unwrap();
        let target = ContextTarget::file("src/auth.rs").unwrap();
        let blob = repo
            .get_context_blob(&context_root, &target)
            .unwrap()
            .unwrap();
        assert_eq!(blob.annotations.len(), 1);
        let rev = blob.annotations[0].current_revision().unwrap();
        assert_eq!(rev.kind, AnnotationKind::Invariant);
        assert_eq!(rev.content, "never bypass the tenant scope");
        assert!(rev.attribution.starts_with("ingest:claude:"));
        assert!(rev.tags.iter().any(|t| t.starts_with("session:")));
    }

    #[test]
    fn symbol_scope_preserved_with_resolved_lines() {
        let (_dir, repo, map, sha, change_id) = seed_repo_with_one_state();
        let mut emit = ReasoningEmitter::new(&repo, &map);

        let pts = vec![point(
            AnnotationKind::Constraint,
            "parseToken returns None for empty audience, not an Err",
            "src/auth.rs",
            Some("parseToken"),
            Some((42, 88)),
            "session-b",
        )];
        emit.emit_for_commit(&sha, &pts).unwrap();

        let state = repo.store().get_state(&change_id).unwrap().unwrap();
        let blob = repo
            .get_context_blob(
                &state.context.unwrap(),
                &ContextTarget::file("src/auth.rs").unwrap(),
            )
            .unwrap()
            .unwrap();
        match &blob.annotations[0].scope {
            AnnotationScope::Symbol {
                name,
                resolved_lines,
            } => {
                assert_eq!(name, "parseToken");
                assert_eq!(*resolved_lines, Some((42, 88)));
            }
            other => panic!("expected Symbol scope, got {other:?}"),
        }
    }

    #[test]
    fn multiple_files_become_distinct_targets() {
        let (_dir, repo, map, sha, change_id) = seed_repo_with_one_state();
        let mut emit = ReasoningEmitter::new(&repo, &map);

        let pts = vec![
            point(
                AnnotationKind::Invariant,
                "rule A",
                "src/a.rs",
                None,
                None,
                "s",
            ),
            point(
                AnnotationKind::Rationale,
                "why B",
                "src/b.rs",
                None,
                None,
                "s",
            ),
        ];
        emit.emit_for_commit(&sha, &pts).unwrap();

        let state = repo.store().get_state(&change_id).unwrap().unwrap();
        let root = state.context.unwrap();
        assert!(
            repo.get_context_blob(&root, &ContextTarget::file("src/a.rs").unwrap())
                .unwrap()
                .is_some()
        );
        assert!(
            repo.get_context_blob(&root, &ContextTarget::file("src/b.rs").unwrap())
                .unwrap()
                .is_some()
        );
        assert_eq!(emit.stats().annotations_written, 2);
        // Only one state_update even though two targets were touched.
        assert_eq!(emit.stats().states_updated, 1);
    }

    #[test]
    fn empty_file_path_becomes_state_scope_target() {
        let (_dir, repo, map, sha, change_id) = seed_repo_with_one_state();
        let mut emit = ReasoningEmitter::new(&repo, &map);

        let pts = vec![point(
            AnnotationKind::Rationale,
            "the whole commit was a rescue revert",
            "", // no file → state-level annotation
            None,
            None,
            "s",
        )];
        emit.emit_for_commit(&sha, &pts).unwrap();

        let state = repo.store().get_state(&change_id).unwrap().unwrap();
        let target = ContextTarget::state(change_id);
        assert!(
            repo.get_context_blob(&state.context.unwrap(), &target)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn state_scope_rejects_symbol_scope_points() {
        let (_dir, repo, map, sha, _change_id) = seed_repo_with_one_state();
        let mut emit = ReasoningEmitter::new(&repo, &map);

        // file="" bucket → state target; symbol would violate the
        // state-target/file-scope invariant. Emitter counts it as
        // malformed rather than panicking.
        let pts = vec![point(
            AnnotationKind::Invariant,
            "bad combo: state target + symbol scope",
            "",
            Some("some_fn"),
            None,
            "s",
        )];
        let root = emit.emit_for_commit(&sha, &pts).unwrap();
        assert!(root.is_none(), "no write should have happened");
        assert_eq!(emit.stats().skipped_malformed, 1);
        assert_eq!(emit.stats().annotations_written, 0);
    }

    #[test]
    fn unmapped_git_sha_is_counted_not_errored() {
        let (_dir, repo, map, _sha, _change_id) = seed_repo_with_one_state();
        let mut emit = ReasoningEmitter::new(&repo, &map);
        let pts = vec![point(
            AnnotationKind::Invariant,
            "x",
            "src/a.rs",
            None,
            None,
            "s",
        )];
        let root = emit.emit_for_commit(&"b".repeat(40), &pts).unwrap();
        assert!(root.is_none());
        assert_eq!(emit.stats().skipped_unmapped, 1);
        assert_eq!(emit.stats().annotations_written, 0);
    }

    #[test]
    fn dedups_identical_point_on_rerun() {
        let (_dir, repo, map, sha, _change_id) = seed_repo_with_one_state();
        let mut emit = ReasoningEmitter::new(&repo, &map);

        let pts = vec![point(
            AnnotationKind::Invariant,
            "never bypass the tenant scope",
            "src/auth.rs",
            None,
            None,
            "session-a",
        )];
        emit.emit_for_commit(&sha, &pts).unwrap();
        // Re-emit same point: (content, kind, attribution) all match, so
        // dedup fires and no new annotation lands.
        emit.emit_for_commit(&sha, &pts).unwrap();

        let s = emit.stats();
        assert_eq!(s.annotations_written, 1);
        assert_eq!(s.deduped, 1);
        assert_eq!(s.states_updated, 1);
    }

    #[test]
    fn malformed_points_counted_and_skipped() {
        let (_dir, repo, map, sha, _change_id) = seed_repo_with_one_state();
        let mut emit = ReasoningEmitter::new(&repo, &map);

        // Oversize text → not well_formed → dropped before write.
        let mut bad = point(AnnotationKind::Invariant, "x", "src/a.rs", None, None, "s");
        bad.text = "x".repeat(200);

        let root = emit.emit_for_commit(&sha, &[bad]).unwrap();
        assert!(root.is_none());
        assert_eq!(emit.stats().skipped_malformed, 1);
        assert_eq!(emit.stats().annotations_written, 0);
    }

    #[test]
    fn empty_batch_is_a_noop() {
        let (_dir, repo, map, sha, _change_id) = seed_repo_with_one_state();
        let mut emit = ReasoningEmitter::new(&repo, &map);
        let root = emit.emit_for_commit(&sha, &[]).unwrap();
        assert!(root.is_none());
        assert_eq!(emit.stats(), ReasoningEmitStats::default());
    }

    #[test]
    fn second_batch_appends_without_overwriting() {
        let (_dir, repo, map, sha, change_id) = seed_repo_with_one_state();
        let mut emit = ReasoningEmitter::new(&repo, &map);

        emit.emit_for_commit(
            &sha,
            &[point(
                AnnotationKind::Invariant,
                "first",
                "src/a.rs",
                None,
                None,
                "s1",
            )],
        )
        .unwrap();
        emit.emit_for_commit(
            &sha,
            &[point(
                AnnotationKind::Rationale,
                "second",
                "src/a.rs",
                None,
                None,
                "s2",
            )],
        )
        .unwrap();

        let state = repo.store().get_state(&change_id).unwrap().unwrap();
        let blob = repo
            .get_context_blob(
                &state.context.unwrap(),
                &ContextTarget::file("src/a.rs").unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(blob.annotations.len(), 2);
        assert_eq!(emit.stats().annotations_written, 2);
        // States updated counts distinct state mutations — two batches
        // to the same state counts as two.
        assert_eq!(emit.stats().states_updated, 2);
    }

    #[test]
    fn normalize_strips_claude_worktree_prefix() {
        assert_eq!(
            normalize_worktree_path(".claude/worktrees/foo-bar-ac9/crates/x.rs"),
            "crates/x.rs"
        );
    }

    #[test]
    fn normalize_strips_codex_worktree_prefix() {
        assert_eq!(
            normalize_worktree_path(".codex/worktrees/xyz/src/lib.rs"),
            "src/lib.rs"
        );
    }

    #[test]
    fn normalize_leaves_plain_path_unchanged() {
        assert_eq!(normalize_worktree_path("crates/x.rs"), "crates/x.rs");
    }

    #[test]
    fn worktree_prefixed_file_lands_on_repo_relative_target() {
        let (_dir, repo, map, sha, change_id) = seed_repo_with_one_state();
        let mut emit = ReasoningEmitter::new(&repo, &map);

        let pts = vec![point(
            AnnotationKind::Invariant,
            "claude worktree session",
            ".claude/worktrees/stupefied-curran-ac963d/crates/cli/src/cli/commands/workspace.rs",
            None,
            None,
            "session-a",
        )];
        emit.emit_for_commit(&sha, &pts).unwrap();

        let state = repo.store().get_state(&change_id).unwrap().unwrap();
        let target = ContextTarget::file("crates/cli/src/cli/commands/workspace.rs").unwrap();
        let blob = repo
            .get_context_blob(&state.context.unwrap(), &target)
            .unwrap()
            .expect("annotation should land under the repo-native path");
        assert_eq!(blob.annotations.len(), 1);
        let rev = blob.annotations[0].current_revision().unwrap();
        assert!(
            rev.tags
                .iter()
                .any(|t| t == "worktree:stupefied-curran-ac963d"),
            "expected worktree provenance tag, got {:?}",
            rev.tags
        );
    }
}
