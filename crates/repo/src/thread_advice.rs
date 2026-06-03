// SPDX-License-Identifier: Apache-2.0
//! Shared thread recommendation and health helpers.

use serde::Serialize;

use crate::{Thread, ThreadFreshness, ThreadState};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedAction {
    Commit,
    Ready,
    Sync,
    Land,
    Resolve,
    Review,
    Promote,
}

impl RecommendedAction {
    pub fn command(&self, thread_id: &str) -> Option<String> {
        match self {
            Self::Commit => Some("heddle commit -m \"...\"".to_string()),
            Self::Ready => Some(format!("heddle ready --thread {thread_id}")),
            Self::Sync => Some(format!("heddle sync --thread {thread_id}")),
            Self::Land => Some(format!("heddle land --thread {thread_id} --no-push")),
            Self::Resolve => Some("heddle resolve --list".to_string()),
            Self::Review => None,
            Self::Promote => Some(format!("heddle thread promote {thread_id}")),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadAdvice {
    pub thread_health: String,
    pub blockers: Vec<String>,
    pub recommended_action: String,
}

pub fn describe_thread_advice(
    thread: &Thread,
    worktree_dirty: bool,
    conflicts: usize,
    clean_ready_merges_to_apply: bool,
) -> ThreadAdvice {
    describe_thread_advice_with_initial(
        thread,
        worktree_dirty,
        conflicts,
        clean_ready_merges_to_apply,
        false,
    )
}

/// Variant that distinguishes a worktree that diverges from the
/// seeded genesis state (no user capture has happened yet) from one
/// that has accumulated changes since a real capture.
///
/// When `initial_state` is true and the worktree is dirty, the thread
/// is labeled `"uncaptured"` instead of `"dirty_worktree"`. The
/// recommended action stays `heddle commit` — only the label
/// changes, so the user-facing first impression matches the actual
/// situation (nothing has been captured yet) rather than implying
/// that something has drifted. See heddle#160.
pub fn describe_thread_advice_with_initial(
    thread: &Thread,
    worktree_dirty: bool,
    conflicts: usize,
    clean_ready_merges_to_apply: bool,
    initial_state: bool,
) -> ThreadAdvice {
    if matches!(thread.state, ThreadState::Abandoned | ThreadState::Merged) {
        return ThreadAdvice {
            thread_health: thread.state.to_string(),
            blockers: Vec::new(),
            recommended_action: String::new(),
        };
    }

    // A freshly-initialized active thread with no work, no conflicts, no
    // merges pending, and no promotion warning is healthy. The advice
    // cascade below otherwise falls through to a misleading
    // "needs_attention" + "heddle ready" recommendation for repos that have
    // genuinely nothing to do yet.
    let fresh_and_idle = !worktree_dirty
        && conflicts == 0
        && !clean_ready_merges_to_apply
        && thread.state == ThreadState::Active
        && thread.freshness != ThreadFreshness::Stale
        && thread.changed_paths.is_empty()
        && !thread.promotion_suggested;
    if fresh_and_idle {
        return ThreadAdvice {
            thread_health: "clean".to_string(),
            blockers: Vec::new(),
            recommended_action: String::new(),
        };
    }

    let mut blockers = Vec::new();
    let action = if worktree_dirty {
        RecommendedAction::Commit
    } else if thread.freshness == ThreadFreshness::Stale {
        blockers.push(format!(
            "Thread '{}' is stale against '{}'",
            thread.id,
            thread
                .target_thread
                .as_deref()
                .unwrap_or("its target thread")
        ));
        if conflicts > 0 {
            blockers.push(format!(
                "{} path conflict(s) need manual resolution after refresh",
                conflicts
            ));
        }
        RecommendedAction::Sync
    } else if thread.promotion_suggested && !thread.heavy_impact_paths.is_empty() {
        blockers.push(format!(
            "Heavy-impact change: {} — review broader impact before merging",
            preview_paths(&thread.heavy_impact_paths)
        ));
        RecommendedAction::Review
    } else if conflicts > 0 || thread.state == ThreadState::Blocked {
        if conflicts > 0 {
            blockers.push(format!(
                "{} path conflict(s) need manual resolution",
                conflicts
            ));
        } else if blockers.is_empty() {
            blockers.push("Thread needs attention before integration".to_string());
        }
        // `land` — not `resolve --list`. This is a metadata-only function; it
        // is always called from non-materialized contexts (status passes
        // conflicts=0, the only conflicts>0 caller is the merge dry-run
        // preview), so no merge state exists for `resolve` to read here and a
        // `resolve --list` breadcrumb dies with `no_merge_in_progress`. `land`
        // re-drives the thread: it materializes a real conflict (then surfaces
        // `continue`) or re-reports the specific blocker with its own
        // recommendation. (heddle#464 close-the-class.)
        RecommendedAction::Land
    } else if thread.state == ThreadState::Ready
        && thread.integration_policy_result.status.as_deref() == Some("previewed")
    {
        return ThreadAdvice {
            thread_health: "ready".to_string(),
            blockers,
            recommended_action: format!("heddle land --thread {} --no-push", thread.id),
        };
    } else if clean_ready_merges_to_apply || thread.state == ThreadState::Ready {
        RecommendedAction::Land
    } else {
        RecommendedAction::Ready
    };

    let thread_health = if worktree_dirty && initial_state {
        "uncaptured"
    } else if worktree_dirty {
        "dirty_worktree"
    } else if !blockers.is_empty() {
        "blocked"
    } else if thread.state == ThreadState::Ready {
        "ready"
    } else if thread.freshness == ThreadFreshness::Current {
        "active"
    } else {
        "needs_attention"
    }
    .to_string();

    ThreadAdvice {
        thread_health,
        blockers,
        recommended_action: action.command(&thread.id).unwrap_or_default(),
    }
}

/// Format a path list for inclusion in a one-line blocker message.
///
/// Keeps the first few names and tags the rest as `… +N more`. Without this,
/// a repo with hundreds of changed files would push a 1.5-screen-wide line
/// into `heddle status` / `heddle thread drop` / `heddle merge --preview`.
/// The full list still lives in the JSON form of every advice-emitting verb.
fn preview_paths(paths: &[String]) -> String {
    const PREVIEW: usize = 3;
    let visible: Vec<&str> = paths.iter().take(PREVIEW).map(String::as_str).collect();
    let suffix = if paths.len() > visible.len() {
        format!(", … +{} more", paths.len() - visible.len())
    } else {
        String::new()
    };
    format!("{}{suffix}", visible.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread_json(state: &str) -> Thread {
        serde_json::from_value(serde_json::json!({
            "id": "feature/x",
            "thread": "feature/x",
            "target_thread": "main",
            "mode": "materialized",
            "state": state,
            "base_state": "aaaa",
            "base_root": "bbbb",
            "freshness": "current",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        }))
        .expect("thread fixture should deserialize")
    }

    // heddle#464 close-the-class: `describe_thread_advice` is metadata-only —
    // it is never called from a context that has materialized a merge (status
    // passes conflicts=0; the lone conflicts>0 caller is the dry-run merge
    // preview). So it must never emit `heddle resolve --list`, which would die
    // with `no_merge_in_progress`. A blocked thread re-drives through `land`.
    #[test]
    fn blocked_thread_recommends_land_not_dead_resolve_breadcrumb() {
        let advice = describe_thread_advice(&thread_json("blocked"), false, 0, false);
        assert_eq!(advice.thread_health, "blocked");
        assert_ne!(advice.recommended_action, "heddle resolve --list");
        assert_eq!(
            advice.recommended_action,
            "heddle land --thread feature/x --no-push"
        );
    }

    // Even when a preview reports conflicts, the merge is a dry run with no
    // materialized state, so the breadcrumb must drive materialization (land),
    // never a dead `resolve --list`.
    #[test]
    fn previewed_conflicts_recommend_land_not_dead_resolve_breadcrumb() {
        let advice = describe_thread_advice(&thread_json("active"), false, 2, false);
        assert_ne!(advice.recommended_action, "heddle resolve --list");
        assert_eq!(
            advice.recommended_action,
            "heddle land --thread feature/x --no-push"
        );
    }
}
