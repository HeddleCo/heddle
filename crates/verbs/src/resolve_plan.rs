// SPDX-License-Identifier: Apache-2.0
//! Pure resolve planning: conflict marker detection and unresolved path sets.
//!
//! Marker detection here is **line-start** oriented (git-style conflict
//! markers). That is intentionally stricter against false positives than
//! [`crate::contains_conflict_marker_bytes`], which looks for full triplets
//! anywhere in a file (refresh materialization).

/// Whether content still has line-start conflict markers (`<<<<<<<`,
/// `=======`, or `>>>>>>>`).
///
/// Used when marking a path resolved without `--ours`/`--theirs`/`--force`.
pub fn contains_line_start_conflict_markers(content: &[u8]) -> bool {
    content.split(|byte| *byte == b'\n').any(|line| {
        line.starts_with(b"<<<<<<<") || line.starts_with(b"=======") || line.starts_with(b">>>>>>>")
    })
}

/// Paths still unresolved: registered conflicts not yet marked resolved.
pub fn unresolved_conflict_paths(conflicts: &[String], resolved: &[String]) -> Vec<String> {
    conflicts
        .iter()
        .filter(|path| !resolved.iter().any(|r| r == *path))
        .cloned()
        .collect()
}

/// Whether a path is in the active conflict set.
pub fn path_is_active_conflict(conflicts: &[String], path: &str) -> bool {
    conflicts.iter().any(|c| c == path)
}

/// Side selection for resolve (CLI maps flags → this plan).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveSideSelection {
    /// Keep worktree content; only validate markers.
    Worktree,
    /// Take ours tree version.
    Ours,
    /// Take theirs tree version.
    Theirs,
}

/// Plan resolve side selection from CLI flags.
///
/// `ours` and `theirs` together is invalid and returns [`None`] so CLI can
/// surface its existing validation path (or treat as worktree).
pub fn plan_resolve_side(ours: bool, theirs: bool) -> Option<ResolveSideSelection> {
    match (ours, theirs) {
        (true, true) => None,
        (true, false) => Some(ResolveSideSelection::Ours),
        (false, true) => Some(ResolveSideSelection::Theirs),
        (false, false) => Some(ResolveSideSelection::Worktree),
    }
}

/// Whether marker validation is required before marking resolved.
pub fn resolve_requires_marker_check(side: ResolveSideSelection, force: bool) -> bool {
    matches!(side, ResolveSideSelection::Worktree) && !force
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_start_markers_detect_git_style_lines() {
        assert!(contains_line_start_conflict_markers(
            b"keep\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch\n"
        ));
        assert!(contains_line_start_conflict_markers(b"=======\n"));
        assert!(!contains_line_start_conflict_markers(b"no markers here\n"));
        // Not at line start — line-start detector ignores mid-line noise.
        assert!(!contains_line_start_conflict_markers(b"x<<<<<<<\n"));
    }

    #[test]
    fn unresolved_paths_filter_resolved() {
        let conflicts = vec!["a.rs".into(), "b.rs".into(), "c.rs".into()];
        let resolved = vec!["b.rs".into()];
        assert_eq!(
            unresolved_conflict_paths(&conflicts, &resolved),
            vec!["a.rs".to_string(), "c.rs".to_string()]
        );
        assert!(path_is_active_conflict(&conflicts, "a.rs"));
        assert!(!path_is_active_conflict(&conflicts, "z.rs"));
    }

    #[test]
    fn plan_resolve_side_and_marker_gate() {
        assert_eq!(
            plan_resolve_side(false, false),
            Some(ResolveSideSelection::Worktree)
        );
        assert_eq!(
            plan_resolve_side(true, false),
            Some(ResolveSideSelection::Ours)
        );
        assert_eq!(
            plan_resolve_side(false, true),
            Some(ResolveSideSelection::Theirs)
        );
        assert_eq!(plan_resolve_side(true, true), None);
        assert!(resolve_requires_marker_check(
            ResolveSideSelection::Worktree,
            false
        ));
        assert!(!resolve_requires_marker_check(
            ResolveSideSelection::Worktree,
            true
        ));
        assert!(!resolve_requires_marker_check(
            ResolveSideSelection::Ours,
            false
        ));
    }
}
