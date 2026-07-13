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
    HeddleError::recovery(
        RecoveryDetails::safety_refusal(
            "dirty_worktree",
            // Established wording (`main`): the blocker leads with the fix
            // ("Save or stash …"), not a bare refusal. The reparent rewrote
            // this to "Refusing to … with a dirty worktree", dropping the
            // recovery-first phrasing the typed advice contract asserts.
            format!("Save or stash worktree changes before {action}"),
            "Save Heddle provenance with `heddle capture -m \"...\"`, then commit Git-owned source history with `git commit -m \"...\"` before retrying.",
            unsafe_condition,
            format!(
                "{action} would write another tree into the worktree; saving first prevents those path changes from being overwritten"
            ),
            already_preserved,
        )
        .with_recovery_commands(vec![
            "heddle capture -m \"...\"".to_string(),
            "heddle capture -m \"...\"".to_string(),
            "heddle stash push -m \"...\"".to_string(),
        ]),
    )
}

pub(crate) fn source_thread_uncaptured_work(
    thread_id: &str,
    checkout_path: &str,
    dirty_paths: &[String],
    preview: bool,
) -> HeddleError {
    // `uncaptured path(s): …` summary mirrors `main`'s CLI-side wording so both
    // the machine `unsafe_condition` and the text-mode `Paths:` line read the same.
    let path_summary = uncaptured_path_summary(dirty_paths);
    let unsafe_condition =
        format!("source thread '{thread_id}' has {path_summary} in {checkout_path}");
    let verb = if preview { "preview merge" } else { "merge" };
    // Build path-specific recovery commands from the source checkout path so the
    // machine envelope's `primary_command`/`recovery_commands` point back at the
    // exact checkout (mirrors `main`'s CLI-side `RecoveryAdvice`, HeddleCo/heddle#981).
    let repo_arg = recommended_action_quote(checkout_path);
    let ready = format!("heddle --repo {repo_arg} ready -m \"Save source work\"");
    let capture = format!("heddle --repo {repo_arg} capture -m \"Save source work\"");
    let stash = format!("heddle --repo {repo_arg} stash push -m \"Save source work\"");
    // Error copy mirrors `main`: signal the preview/merge did not run so callers
    // don't mistake the refusal for an up-to-date result.
    let did_not_run = if preview {
        "merge preview did not run"
    } else {
        "merge did not run"
    };
    let error = format!(
        "Thread '{thread_id}' has uncaptured work in {checkout_path} ({path_summary}); {did_not_run}"
    );
    HeddleError::recovery(
        RecoveryDetails::safety_refusal(
            "source_thread_uncaptured_work",
            error,
            format!("Run `{ready}` in the source checkout, then retry the merge."),
            unsafe_condition,
            format!("{verb} would integrate a source tip that does not include uncaptured worktree changes"),
            "repository state, refs, metadata, and worktree files were left unchanged",
        )
        .with_recovery_commands(vec![ready, capture, stash]),
    )
}

/// Summarize uncaptured worktree paths as `uncaptured path(s): a, b, …`,
/// mirroring the CLI-side wording so machine and text envelopes stay aligned.
fn uncaptured_path_summary(paths: &[String]) -> String {
    if paths.is_empty() {
        return "uncaptured worktree paths".to_string();
    }
    let shown = paths
        .iter()
        .take(12)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let overflow = paths.len().saturating_sub(12);
    if overflow == 0 {
        format!("uncaptured path(s): {shown}")
    } else {
        format!("uncaptured path(s): {shown}, and {overflow} more")
    }
}

/// Quote a value for embedding in a recommended-action command line, matching
/// the CLI-side quoting rules so validation and templating stay consistent.
fn recommended_action_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+'));
    if safe {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}
