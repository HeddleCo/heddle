// SPDX-License-Identifier: Apache-2.0
//! Path filters for diff reports.

use anyhow::Result;
use repo::ChangedPathFilters;

use super::types::{DiffReport, DiffStats, FileChange, FileContextEntry, SemanticChangeEntry};

/// Restrict a computed diff report to the given repository-relative paths.
///
/// Empty `paths` leaves the report unchanged. Filters match an exact path or
/// any descendant (`src` matches `src/lib.rs`).
pub fn apply_path_filters(report: &mut DiffReport, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let filters = ChangedPathFilters::try_from_paths(paths)?;
    report
        .changes
        .retain(|change| change_matches(change, &filters));
    if let Some(semantic) = report.semantic_changes.as_mut() {
        semantic.retain(|change| semantic_matches(change, &filters));
        if semantic.is_empty() {
            report.semantic_changes = None;
        }
    }
    if let Some(context) = report.context.as_mut() {
        context.retain(|entry| context_matches(entry, &filters));
        if context.is_empty() {
            report.context = None;
        }
    }
    report.stats = DiffStats::from_changes(&report.changes, report.semantic_changes.as_deref());
    report.changed_path_count = report.changes.len();
    report.patch = None;
    Ok(())
}

fn change_matches(change: &FileChange, filters: &ChangedPathFilters) -> bool {
    filters.matches(&change.path)
        || change
            .old_path
            .as_deref()
            .is_some_and(|old| filters.matches(old))
}

fn semantic_matches(change: &SemanticChangeEntry, filters: &ChangedPathFilters) -> bool {
    change
        .path
        .as_deref()
        .is_some_and(|path| filters.matches(path))
        || change
            .from_path
            .as_deref()
            .is_some_and(|path| filters.matches(path))
        || change
            .to_path
            .as_deref()
            .is_some_and(|path| filters.matches(path))
}

fn context_matches(entry: &FileContextEntry, filters: &ChangedPathFilters) -> bool {
    filters.matches(&entry.path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::types::FileChange;

    fn report_with(paths: &[&str]) -> DiffReport {
        let changes = paths
            .iter()
            .map(|path| FileChange {
                path: (*path).to_string(),
                kind: "modified".to_string(),
                ..FileChange::default()
            })
            .collect();
        DiffReport::new(Some("HEAD".to_string()), None, changes, None, None, None)
    }

    #[test]
    fn empty_filters_leave_report_unchanged() {
        let mut report = report_with(&["NOTES.md", "src/lib.rs"]);
        apply_path_filters(&mut report, &[]).expect("empty filters");
        assert_eq!(report.changed_path_count, 2);
    }

    #[test]
    fn exact_path_keeps_only_that_file() {
        let mut report = report_with(&["NOTES.md", "src/lib.rs"]);
        apply_path_filters(&mut report, &["NOTES.md".to_string()]).expect("filter");
        assert_eq!(
            report
                .changes
                .iter()
                .map(|change| change.path.as_str())
                .collect::<Vec<_>>(),
            ["NOTES.md"]
        );
        assert_eq!(report.changed_path_count, 1);
        assert_eq!(report.stats.files_changed, 1);
    }

    #[test]
    fn directory_filter_keeps_descendants() {
        let mut report = report_with(&["src/lib.rs", "NOTES.md", "src/cli.rs"]);
        apply_path_filters(&mut report, &["src".to_string()]).expect("filter");
        let kept: Vec<_> = report
            .changes
            .iter()
            .map(|change| change.path.as_str())
            .collect();
        assert_eq!(kept, ["src/lib.rs", "src/cli.rs"]);
    }
}
