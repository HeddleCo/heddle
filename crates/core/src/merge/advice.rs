// SPDX-License-Identifier: Apache-2.0
//! Structured recovery details for merge orchestration errors.

use objects::{HeddleError, RecoveryDetails};

pub(crate) fn merge_integrity_refusal(
    error: impl Into<String>,
    unsafe_condition: impl Into<String>,
    would_change: impl Into<String>,
    preserved: impl Into<String>,
) -> HeddleError {
    HeddleError::recovery(RecoveryDetails::safety_refusal(
        "repository_integrity_error",
        error,
        "Inspect repository integrity with `heddle fsck --full`, then restore or repair the reported object/ref.",
        unsafe_condition,
        would_change,
        preserved,
    ))
}

pub(crate) fn merge_no_common_ancestor(current_ref: &str, target_ref: &str) -> HeddleError {
    let current_show = format!("heddle thread show {current_ref}");
    let target_show = format!("heddle thread show {target_ref}");
    HeddleError::recovery(RecoveryDetails::safety_refusal(
        "merge_no_common_ancestor",
        format!(
            "No common ancestor between '{current_ref}' and '{target_ref}' — the two histories are disjoint"
        ),
        format!(
            "Inspect each side with `{current_show}` and `{target_show}` to confirm whether one history was imported separately, then choose an integration path that doesn't require a shared base."
        ),
        format!(
            "merge planning needs a shared base commit, but the commit graph for '{current_ref}' and '{target_ref}' has no common ancestor"
        ),
        "merging two disjoint histories without an explicit reconciliation strategy could overwrite one side's commits",
        "repository state, refs, metadata, and worktree files were left unchanged",
    ))
}

pub(crate) fn merge_already_in_progress() -> HeddleError {
    HeddleError::recovery(RecoveryDetails::safety_refusal(
        "merge_already_in_progress",
        "A merge is already in progress",
        "Inspect the active operation with `heddle status`; resolve it with `heddle continue` or abort it with `heddle resolve --abort`.",
        "merge state is already present for this repository",
        "starting another merge would overwrite or obscure the in-progress conflict state",
        "existing merge state and worktree were left unchanged",
    ))
}

pub(crate) fn thread_not_found(thread_id: &str, action: &str) -> HeddleError {
    HeddleError::recovery(RecoveryDetails::safety_refusal(
        "thread_not_found",
        format!("Thread '{thread_id}' not found"),
        "Inspect available threads with `heddle thread list`, then retry with an existing thread.",
        format!("{action} was requested for missing thread '{thread_id}'"),
        "the command cannot safely change or remove thread metadata that does not exist",
        "no thread refs, checkout directories, mounts, or agent reservations were changed",
    ))
}

pub(crate) fn dirty_worktree(
    action: &str,
    dirty_paths: Vec<String>,
    already_preserved: impl Into<String>,
) -> HeddleError {
    let path_list = if dirty_paths.is_empty() {
        "uncommitted paths were detected".to_string()
    } else {
        dirty_paths
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    };
    let overflow = dirty_paths.len().saturating_sub(12);
    let unsafe_condition = if overflow == 0 {
        format!("unsaved worktree path(s): {path_list}")
    } else {
        format!("unsaved worktree path(s): {path_list}, and {overflow} more")
    };
    HeddleError::recovery(RecoveryDetails::safety_refusal(
        "dirty_worktree",
        format!("Refusing to {action} with a dirty worktree"),
        "Preserve work with `heddle capture -m \"...\"`, `heddle commit -m \"...\"`, or `heddle stash push -m \"...\"`, then retry.",
        unsafe_condition,
        format!("{action} would overwrite uncommitted worktree content"),
        already_preserved,
    ))
}

pub(crate) fn source_thread_uncaptured_work(
    thread_id: &str,
    checkout_path: &str,
    dirty_paths: &[String],
    preview: bool,
) -> HeddleError {
    let path_list = if dirty_paths.is_empty() {
        "uncommitted paths were detected".to_string()
    } else {
        dirty_paths
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    };
    let overflow = dirty_paths.len().saturating_sub(12);
    let unsafe_condition = if overflow == 0 {
        format!("source thread '{thread_id}' has unsaved worktree path(s) at {checkout_path}: {path_list}")
    } else {
        format!(
            "source thread '{thread_id}' has unsaved worktree path(s) at {checkout_path}: {path_list}, and {overflow} more"
        )
    };
    let verb = if preview { "preview merge" } else { "merge" };
    HeddleError::recovery(RecoveryDetails::safety_refusal(
        "source_thread_uncaptured_work",
        format!("Refusing to {verb} thread '{thread_id}' with uncaptured source-thread work"),
        format!(
            "Switch to the source checkout, capture or stash the unsaved work, then retry. Checkout: {checkout_path}"
        ),
        unsafe_condition,
        format!("{verb} would integrate a source tip that does not include uncaptured worktree changes"),
        "repository state, refs, metadata, and worktree files were left unchanged",
    ))
}
