// SPDX-License-Identifier: Apache-2.0
//! Shared thread recommendation and health helpers.

use serde::Serialize;

use crate::{Thread, ThreadFreshness, ThreadState};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedAction {
    Capture,
    Ready,
    Refresh,
    MergePreview,
    MergeApply,
    Resolve,
    Promote,
}

impl RecommendedAction {
    pub fn command(&self, thread_id: &str) -> Option<String> {
        match self {
            Self::Capture => Some("heddle capture".to_string()),
            Self::Ready => Some(format!("heddle ready --thread {thread_id}")),
            Self::Refresh => Some(format!("heddle thread refresh {thread_id}")),
            Self::MergePreview => Some(format!("heddle merge {thread_id} --preview")),
            Self::MergeApply => Some(format!("heddle merge {thread_id}")),
            Self::Resolve => Some(format!("heddle thread resolve {thread_id}")),
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
    if fresh_and_idle && thread.freshness != ThreadFreshness::Current {
        return ThreadAdvice {
            thread_health: "clean".to_string(),
            blockers: Vec::new(),
            recommended_action: String::new(),
        };
    }

    let mut blockers = Vec::new();
    let action = if worktree_dirty {
        RecommendedAction::Capture
    } else if thread.freshness == ThreadFreshness::Stale {
        blockers.push(format!(
            "Thread '{}' is stale against '{}'",
            thread.id,
            thread
                .target_thread
                .as_deref()
                .unwrap_or("its target thread")
        ));
        RecommendedAction::Refresh
    } else if thread.promotion_suggested && !thread.heavy_impact_paths.is_empty() {
        blockers.push(format!(
            "Heavy-impact change: {} — review broader impact before merging",
            preview_paths(&thread.heavy_impact_paths)
        ));
        RecommendedAction::Promote
    } else if conflicts > 0 || thread.state == ThreadState::Blocked {
        if conflicts > 0 {
            blockers.push(format!(
                "{} path conflict(s) need manual resolution",
                conflicts
            ));
        } else if blockers.is_empty() {
            blockers.push("Thread needs attention before integration".to_string());
        }
        RecommendedAction::Resolve
    } else if clean_ready_merges_to_apply {
        RecommendedAction::MergeApply
    } else if thread.state == ThreadState::Ready {
        RecommendedAction::MergePreview
    } else {
        RecommendedAction::Ready
    };

    let thread_health = if worktree_dirty {
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