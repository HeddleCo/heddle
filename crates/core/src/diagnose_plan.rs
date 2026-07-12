// SPDX-License-Identifier: Apache-2.0
//! Pure diagnose/doctor display helpers (no repo I/O).
//!
//! Owns path previews, visibility labels, and human section headers for
//! `heddle doctor` / diagnose that can be decided from already-collected
//! facts. Worktree status, thread advice, and RecoveryAdvice stay CLI-owned.

/// Human title for the diagnose text header (`Doctor`).
pub const DIAGNOSE_SECTION_DOCTOR: &str = "Doctor";

/// Human label when no thread is attached.
pub const DIAGNOSE_THREAD_DETACHED: &str = "Thread: detached";

/// Human label when HEAD has no state yet.
pub const DIAGNOSE_STATE_INITIAL: &str = "State: (initial)";

/// Preview up to five changed paths, then `+N more` when `total` exceeds shown.
///
/// Paths are taken in order: modified, then added, then deleted.
pub fn changed_path_preview(
    modified: &[String],
    added: &[String],
    deleted: &[String],
    total: usize,
) -> String {
    let mut paths = modified
        .iter()
        .chain(added.iter())
        .chain(deleted.iter())
        .take(5)
        .cloned()
        .collect::<Vec<_>>();
    if total > paths.len() {
        paths.push(format!("+{} more", total - paths.len()));
    }
    paths.join(", ")
}

/// Visibility label for a diagnose thread line.
///
/// Prefer a resolved workspace `mode_label` (e.g. `"main checkout"`) when
/// present; otherwise fall back to the thread's visibility string.
pub fn diagnose_thread_visibility_label<'a>(
    mode_label: Option<&'a str>,
    visibility: &'a str,
) -> &'a str {
    mode_label.unwrap_or(visibility)
}

/// Health status tokens used when no current thread summary is available.
pub fn diagnose_detached_health_status(worktree_dirty: bool, initial_state: bool) -> &'static str {
    if worktree_dirty && initial_state {
        "uncaptured"
    } else if worktree_dirty {
        "dirty_worktree"
    } else {
        "detached"
    }
}

/// Changes summary line: `N modified, M added, D deleted`.
pub fn diagnose_changes_summary(modified: usize, added: usize, deleted: usize) -> String {
    format!("{modified} modified, {added} added, {deleted} deleted")
}

/// Workspace summary line from counts.
pub fn diagnose_workspace_summary(
    thread_count: usize,
    parallel_count: usize,
    ready_count: usize,
    blocked_count: usize,
    active_actor_count: usize,
) -> String {
    format!(
        "{thread_count} thread(s), {parallel_count} parallel, {ready_count} ready, {blocked_count} blocked, {active_actor_count} actor(s)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_path_preview_takes_five_and_more() {
        let modified = vec!["a".into(), "b".into(), "c".into()];
        let added = vec!["d".into(), "e".into(), "f".into()];
        let deleted = vec!["g".into()];
        let preview = changed_path_preview(&modified, &added, &deleted, 7);
        assert_eq!(preview, "a, b, c, d, e, +2 more");
    }

    #[test]
    fn changed_path_preview_no_more_when_total_fits() {
        let modified = vec!["only".into()];
        assert_eq!(changed_path_preview(&modified, &[], &[], 1), "only");
    }

    #[test]
    fn visibility_label_prefers_mode() {
        assert_eq!(
            diagnose_thread_visibility_label(Some("main checkout"), "visible"),
            "main checkout"
        );
        assert_eq!(
            diagnose_thread_visibility_label(None, "no dedicated checkout"),
            "no dedicated checkout"
        );
    }

    #[test]
    fn detached_health_and_summaries() {
        assert_eq!(diagnose_detached_health_status(true, true), "uncaptured");
        assert_eq!(
            diagnose_detached_health_status(true, false),
            "dirty_worktree"
        );
        assert_eq!(diagnose_detached_health_status(false, true), "detached");
        assert_eq!(
            diagnose_changes_summary(1, 2, 3),
            "1 modified, 2 added, 3 deleted"
        );
        assert!(diagnose_workspace_summary(3, 1, 1, 0, 2).contains("3 thread(s)"));
        assert_eq!(DIAGNOSE_SECTION_DOCTOR, "Doctor");
    }
}
