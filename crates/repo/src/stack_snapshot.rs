// SPDX-License-Identifier: Apache-2.0
//! Point-in-time projection of a repo's threads + stacks.
//!
//! `RepositorySnapshot` is a value-type view over the thread corpus that
//! callers can serialize, ship to a remote tool, or diff against a later
//! capture to detect drift. It is **not** the worktree-capture pipeline
//! that lives in [`crate::repository_snapshot`] — that pipeline produces
//! a new `State` object on disk. This snapshot is a read-only summary
//! that pairs with `heddle stack snapshot` and the agentic harness hook.
//!
//! ## JSON schema
//!
//! The on-the-wire shape is the bare serde derivation:
//!
//! ```json
//! {
//!   "version": 1,
//!   "captured_at": "2026-05-23T17:08:00Z",
//!   "stacks": [
//!     {
//!       "root": {
//!         "name": "feature-a",
//!         "children": [{ "name": "feature-b", "children": [] }]
//!       }
//!     }
//!   ],
//!   "threads": [
//!     {
//!       "thread": "feature-a",
//!       "parent_thread": null,
//!       "base_state": "hs-...",
//!       "current_state": "hs-...",
//!       "state": "active",
//!       "freshness": "current"
//!     }
//!   ]
//! }
//! ```
//!
//! `version` is the schema major; bump it on any non-additive shape
//! change. Additive fields use `#[serde(default)]` so older readers stay
//! compatible.
//!
//! Conceptual ancestor: HeddleCo/weft#46. Adapted, not cherry-picked.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    Repository, Result, ThreadFreshness, ThreadState,
    thread_stack::{StackNode, ThreadStack, compute_stacks, stack_for},
    thread_storage::ThreadManager,
};

/// Current schema version emitted by [`RepositorySnapshot::capture`].
///
/// Bumping this is a wire-format change — callers consuming older
/// snapshots through `heddle stack snapshot` should expect to upgrade.
pub const REPOSITORY_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// Trimmed view of a single thread that's safe to ship. Mirrors the
/// fields a stack-aware agent or remote tool needs without dragging in
/// the heavier `ThreadRecord` payload (verification summaries,
/// confidence bands, ephemeral marker, etc).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSnapshot {
    pub thread: String,
    #[serde(default)]
    pub parent_thread: Option<String>,
    pub base_state: String,
    #[serde(default)]
    pub current_state: Option<String>,
    pub state: ThreadState,
    pub freshness: ThreadFreshness,
}

/// Point-in-time projection of the repo's threads + stacks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositorySnapshot {
    pub version: u32,
    pub captured_at: DateTime<Utc>,
    pub stacks: Vec<ThreadStack>,
    pub threads: Vec<ThreadSnapshot>,
}

/// Surface verdict for [`RepositorySnapshot::next_action_for`]. Mirrors
/// the three states the issue brief calls out:
///
/// * `Ready` — every member of the stack is in [`ThreadState::Ready`] or
///   already merged; the next action is to ship.
/// * `Blocked` — at least one member is in [`ThreadState::Blocked`];
///   that thread is named so the operator knows where to look.
/// * `WaitingOnReview` — the stack is otherwise clean but the named
///   thread is still in flight (Active/Draft) and is the top of the
///   chain. The leaf is the bottleneck.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StackNextAction {
    /// Every thread in the stack is Ready / Merged / Promoted.
    Ready,
    /// At least one thread is Blocked. `thread` names the first such
    /// thread encountered in stack-root-first DFS order.
    Blocked { thread: String },
    /// Every non-top thread is Ready but the top (leaf) is still Active
    /// or Draft — it's the thread waiting on review.
    WaitingOnReview { thread: String },
    /// The thread isn't part of any known stack — discovery returned None.
    Unknown,
}

impl RepositorySnapshot {
    /// Capture a snapshot of the repo's threads + stacks as of `now`.
    pub fn capture(repo: &Repository) -> Result<Self> {
        let manager = ThreadManager::new(repo.heddle_dir());
        let records = manager.list_records()?;
        let stacks = compute_stacks(&records);
        let threads: Vec<ThreadSnapshot> = records
            .iter()
            .map(|r| ThreadSnapshot {
                thread: r.thread.clone(),
                parent_thread: r.parent_thread.clone(),
                base_state: r.base_state.clone(),
                current_state: r.current_state.clone(),
                state: r.state.clone(),
                freshness: r.freshness.clone(),
            })
            .collect();
        Ok(Self {
            version: REPOSITORY_SNAPSHOT_SCHEMA_VERSION,
            captured_at: Utc::now(),
            stacks,
            threads,
        })
    }

    /// Capture a snapshot from raw thread records — useful for tooling
    /// that already has the records in hand (or for fixtures).
    pub fn from_records(records: &[crate::ThreadRecord]) -> Self {
        let stacks = compute_stacks(records);
        let threads = records
            .iter()
            .map(|r| ThreadSnapshot {
                thread: r.thread.clone(),
                parent_thread: r.parent_thread.clone(),
                base_state: r.base_state.clone(),
                current_state: r.current_state.clone(),
                state: r.state.clone(),
                freshness: r.freshness.clone(),
            })
            .collect();
        Self {
            version: REPOSITORY_SNAPSHOT_SCHEMA_VERSION,
            captured_at: Utc::now(),
            stacks,
            threads,
        }
    }

    /// Look up the stack containing `thread_name`. Returns `None` if the
    /// thread is unknown to this snapshot.
    pub fn stack_containing(&self, thread_name: &str) -> Option<&ThreadStack> {
        self.stacks.iter().find(|stack| stack.contains(thread_name))
    }

    /// Decide the next stack-level action for the stack containing
    /// `thread_name`. See [`StackNextAction`] for the four verdicts.
    pub fn next_action_for(&self, thread_name: &str) -> Result<StackNextAction> {
        let records = self.synthesize_records();
        let stack = match stack_for(&records, thread_name) {
            Some(s) => s,
            None => return Ok(StackNextAction::Unknown),
        };

        let members: Vec<String> = stack.member_names().iter().map(|s| s.to_string()).collect();
        let state_of = |name: &str| {
            self.threads
                .iter()
                .find(|t| t.thread == name)
                .map(|t| t.state.clone())
        };

        // Blocked wins over everything: a single blocked thread anywhere
        // in the stack means the operator must unblock before progress.
        if let Some(blocked) = members
            .iter()
            .find(|name| matches!(state_of(name), Some(ThreadState::Blocked)))
        {
            return Ok(StackNextAction::Blocked {
                thread: blocked.clone(),
            });
        }

        // If everything is shipped-or-shippable, the stack is Ready.
        let all_shippable = members.iter().all(|name| {
            matches!(
                state_of(name),
                Some(
                    ThreadState::Ready
                        | ThreadState::Merged
                        | ThreadState::Promoted
                        | ThreadState::Abandoned
                )
            )
        });
        if all_shippable {
            return Ok(StackNextAction::Ready);
        }

        // Otherwise the bottleneck is the deepest still-in-flight thread
        // in the stack — that's the one waiting on review. DFS-order
        // "last Active/Draft" is wrong for branched stacks (a deep
        // Active in an early subtree loses to a shallow Active in a
        // later sibling), so we recurse with explicit depth tracking
        // and pick the maximum.
        let bottleneck = deepest_in_flight(&stack.root, 0, &state_of).map(|(name, _)| name);
        match bottleneck {
            Some(thread) => Ok(StackNextAction::WaitingOnReview { thread }),
            // Defensive fallback: no blocked, not all shippable, no
            // active/draft. The thread states must be exotic — surface
            // as Unknown rather than guessing.
            None => Ok(StackNextAction::Unknown),
        }
    }

    /// Project the snapshot down to just the stack containing
    /// `thread_name`. Returns `None` if the thread isn't in any known
    /// stack. The returned snapshot has `version`/`captured_at` carried
    /// over but `stacks` and `threads` filtered to that one stack —
    /// suitable for serializing a per-thread view without leaking
    /// sibling stacks into the payload.
    pub fn for_stack(&self, thread_name: &str) -> Option<Self> {
        let stack = self.stack_containing(thread_name)?.clone();
        let members: std::collections::HashSet<String> = stack
            .member_names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let threads = self
            .threads
            .iter()
            .filter(|t| members.contains(&t.thread))
            .cloned()
            .collect();
        Some(Self {
            version: self.version,
            captured_at: self.captured_at,
            stacks: vec![stack],
            threads,
        })
    }

    /// Reconstruct a minimal `Vec<ThreadRecord>` from the snapshot so
    /// the existing stack walker can be reused. Fields not stored in
    /// the snapshot (timestamps, mode, etc.) get safe defaults; only
    /// the link/state shape matters for re-walking.
    fn synthesize_records(&self) -> Vec<crate::ThreadRecord> {
        self.threads
            .iter()
            .map(|t| crate::ThreadRecord {
                id: format!("synth-{}", t.thread),
                thread: t.thread.clone(),
                target_thread: t.parent_thread.clone(),
                parent_thread: t.parent_thread.clone(),
                mode: crate::ThreadMode::Materialized,
                state: t.state.clone(),
                base_state: t.base_state.clone(),
                base_root: t.base_state.clone(),
                current_state: t.current_state.clone(),
                merged_state: None,
                task: None,
                changed_paths: Vec::new(),
                impact_categories: Vec::new(),
                heavy_impact_paths: Vec::new(),
                promotion_suggested: false,
                freshness: t.freshness.clone(),
                verification_summary: Default::default(),
                confidence_summary: Default::default(),
                integration_policy_result: Default::default(),
                created_at: self.captured_at,
                updated_at: self.captured_at,
                ephemeral: None,
                auto: false,
                shared_target_dir: None,
            })
            .collect()
    }
}

/// Walk the stack tree from `node` (at `depth`) and return the deepest
/// thread whose state is `Active` or `Draft`, paired with its depth.
/// Ties prefer earlier (DFS-order) siblings — deterministic given the
/// child sort applied at stack-build time.
fn deepest_in_flight<F>(node: &StackNode, depth: usize, state_of: &F) -> Option<(String, usize)>
where
    F: Fn(&str) -> Option<ThreadState>,
{
    let mut best: Option<(String, usize)> = None;
    let consider = |slot: &mut Option<(String, usize)>, cand: Option<(String, usize)>| {
        if let Some((name, d)) = cand
            && slot.as_ref().is_none_or(|(_, best_d)| d > *best_d)
        {
            *slot = Some((name, d));
        }
    };

    if matches!(
        state_of(&node.name),
        Some(ThreadState::Active | ThreadState::Draft)
    ) {
        consider(&mut best, Some((node.name.clone(), depth)));
    }
    for child in &node.children {
        consider(&mut best, deepest_in_flight(child, depth + 1, state_of));
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ThreadFreshness, ThreadMode, ThreadRecord, ThreadState};

    fn snap(name: &str, parent: Option<&str>, state: ThreadState) -> ThreadSnapshot {
        ThreadSnapshot {
            thread: name.to_string(),
            parent_thread: parent.map(str::to_string),
            base_state: "base".to_string(),
            current_state: Some(format!("{name}-tip")),
            state,
            freshness: ThreadFreshness::Current,
        }
    }

    fn snapshot_with_threads(threads: Vec<ThreadSnapshot>) -> RepositorySnapshot {
        let records: Vec<ThreadRecord> = threads
            .iter()
            .map(|t| ThreadRecord {
                id: format!("id-{}", t.thread),
                thread: t.thread.clone(),
                target_thread: t.parent_thread.clone(),
                parent_thread: t.parent_thread.clone(),
                mode: ThreadMode::Materialized,
                state: t.state.clone(),
                base_state: t.base_state.clone(),
                base_root: t.base_state.clone(),
                current_state: t.current_state.clone(),
                merged_state: None,
                task: None,
                changed_paths: Vec::new(),
                impact_categories: Vec::new(),
                heavy_impact_paths: Vec::new(),
                promotion_suggested: false,
                freshness: t.freshness.clone(),
                verification_summary: Default::default(),
                confidence_summary: Default::default(),
                integration_policy_result: Default::default(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                ephemeral: None,
                auto: false,
                shared_target_dir: None,
            })
            .collect();
        let mut snapshot = RepositorySnapshot::from_records(&records);
        snapshot.threads = threads;
        snapshot
    }

    #[test]
    fn json_round_trip_preserves_payload() {
        let snapshot = snapshot_with_threads(vec![
            snap("feature-a", None, ThreadState::Ready),
            snap("feature-b", Some("feature-a"), ThreadState::Active),
        ]);
        let json = serde_json::to_string(&snapshot).unwrap();
        let parsed: RepositorySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snapshot, parsed);
    }

    #[test]
    fn next_action_blocked_takes_priority_over_everything() {
        let snapshot = snapshot_with_threads(vec![
            snap("a", None, ThreadState::Ready),
            snap("b", Some("a"), ThreadState::Blocked),
            snap("c", Some("b"), ThreadState::Ready),
        ]);
        let action = snapshot.next_action_for("c").unwrap();
        assert_eq!(action, StackNextAction::Blocked { thread: "b".into() });
    }

    #[test]
    fn next_action_all_ready_is_ready() {
        let snapshot = snapshot_with_threads(vec![
            snap("a", None, ThreadState::Ready),
            snap("b", Some("a"), ThreadState::Ready),
            snap("c", Some("b"), ThreadState::Merged),
        ]);
        assert_eq!(
            snapshot.next_action_for("a").unwrap(),
            StackNextAction::Ready
        );
    }

    #[test]
    fn next_action_top_active_is_waiting_on_review() {
        let snapshot = snapshot_with_threads(vec![
            snap("a", None, ThreadState::Ready),
            snap("b", Some("a"), ThreadState::Ready),
            snap("c", Some("b"), ThreadState::Active),
        ]);
        let action = snapshot.next_action_for("c").unwrap();
        assert_eq!(
            action,
            StackNextAction::WaitingOnReview { thread: "c".into() }
        );
    }

    #[test]
    fn next_action_picks_deepest_active_across_branches() {
        // a (Ready)
        // ├── b (Active) — depth 1
        // │   └── c (Active) — depth 2
        // │       └── d (Active) — depth 3
        // └── e (Active) — depth 1
        //
        // DFS-pre-order is [a, b, c, d, e]; the old "last Active in DFS
        // order" picked `e` even though `d` is the deepest in-flight
        // thread. Pin depth-correctness here.
        let snapshot = snapshot_with_threads(vec![
            snap("a", None, ThreadState::Ready),
            snap("b", Some("a"), ThreadState::Active),
            snap("c", Some("b"), ThreadState::Active),
            snap("d", Some("c"), ThreadState::Active),
            snap("e", Some("a"), ThreadState::Active),
        ]);
        assert_eq!(
            snapshot.next_action_for("a").unwrap(),
            StackNextAction::WaitingOnReview { thread: "d".into() }
        );
    }

    #[test]
    fn for_stack_filters_to_containing_stack_only() {
        // Two disjoint stacks; for_stack("x") should keep only x's stack.
        let snapshot = snapshot_with_threads(vec![
            snap("x", None, ThreadState::Active),
            snap("y", Some("x"), ThreadState::Active),
            snap("p", None, ThreadState::Active),
        ]);
        let scoped = snapshot.for_stack("x").expect("x belongs to a stack");
        assert_eq!(scoped.stacks.len(), 1);
        assert_eq!(scoped.stacks[0].root_name(), "x");
        let names: Vec<&str> = scoped.threads.iter().map(|t| t.thread.as_str()).collect();
        assert_eq!(names, vec!["x", "y"]);
        assert_eq!(scoped.version, snapshot.version);
        assert_eq!(scoped.captured_at, snapshot.captured_at);
    }

    #[test]
    fn for_stack_returns_none_for_unknown_thread() {
        let snapshot = snapshot_with_threads(vec![snap("a", None, ThreadState::Ready)]);
        assert!(snapshot.for_stack("nope").is_none());
    }

    #[test]
    fn next_action_unknown_thread_is_unknown() {
        let snapshot = snapshot_with_threads(vec![snap("a", None, ThreadState::Ready)]);
        assert_eq!(
            snapshot.next_action_for("missing").unwrap(),
            StackNextAction::Unknown
        );
    }
}
