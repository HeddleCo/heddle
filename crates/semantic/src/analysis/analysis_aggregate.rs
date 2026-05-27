// SPDX-License-Identifier: Apache-2.0
//! Change aggregation — groups related semantic changes into logical review units.
//!
//! When an agent renames a symbol across 50 files, the raw change list has 50 entries.
//! Aggregation collapses those into one "Renamed X → Y across 50 files" group.

use std::{collections::HashMap, path::PathBuf};

use objects::object::{ChangeImportance, ModificationKind, SemanticChange};

/// What kind of aggregate group this is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AggregateKind {
    /// Formatting/whitespace pass across many files.
    FormattingPass,
    /// Import updates across many files.
    ImportUpdates,
    /// Comment updates across many files.
    CommentUpdates,
    /// Cross-file function rename (same old→new name in multiple files).
    FunctionRename,
    /// Same dependency added/removed across multiple files.
    DependencyChange,
}

/// A group of related semantic changes collapsed into one review unit.
#[derive(Clone, Debug)]
pub struct AggregatedChange {
    /// Human-readable label, e.g. "Formatting pass: 38 files".
    pub label: String,
    /// What kind of aggregate.
    pub kind: AggregateKind,
    /// Files involved.
    pub files: Vec<PathBuf>,
    /// Overall importance of the group.
    pub importance: ChangeImportance,
    /// The individual changes that were collapsed.
    pub children: Vec<SemanticChange>,
}

/// Result of aggregation: ungrouped changes + aggregate groups.
#[derive(Clone, Debug, Default)]
pub struct AggregationResult {
    /// Changes that didn't fit any aggregation pattern (shown individually).
    pub individual: Vec<SemanticChange>,
    /// Aggregated groups.
    pub groups: Vec<AggregatedChange>,
}

/// Aggregate a flat list of semantic changes into groups where possible.
pub fn aggregate_changes(changes: Vec<SemanticChange>) -> AggregationResult {
    let mut formatting_files: Vec<(PathBuf, SemanticChange)> = Vec::new();
    let mut import_files: Vec<(PathBuf, SemanticChange)> = Vec::new();
    let mut comment_files: Vec<(PathBuf, SemanticChange)> = Vec::new();
    // Key: (old_name, new_name) → list of (file, change)
    let mut fn_renames: HashMap<(String, String), Vec<(PathBuf, SemanticChange)>> = HashMap::new();
    // Key: (dep name, version) → list of changes. Version is part of the key so
    // that two files adding `serde 1.0` vs `serde 2.0` don't silently collapse
    // into one group (heddle#119; sibling of heddle#68 r1, fixed in PR #114
    // commit c5a2f75 by keying ItemKey on more than just name).
    let mut dep_added: HashMap<(String, String), Vec<SemanticChange>> = HashMap::new();
    let mut dep_removed: HashMap<String, Vec<SemanticChange>> = HashMap::new();

    let mut individual: Vec<SemanticChange> = Vec::new();

    for change in changes {
        match &change {
            SemanticChange::FileModified {
                path,
                classification: Some(cls),
                ..
            } => match cls {
                ModificationKind::FormattingOnly | ModificationKind::WhitespaceOnly => {
                    formatting_files.push((path.clone(), change));
                }
                ModificationKind::ImportsOnly => {
                    import_files.push((path.clone(), change));
                }
                ModificationKind::CommentsOnly => {
                    comment_files.push((path.clone(), change));
                }
                _ => {
                    individual.push(change);
                }
            },
            SemanticChange::FunctionRenamed {
                file,
                old_name,
                new_name,
                ..
            } => {
                fn_renames
                    .entry((old_name.clone(), new_name.clone()))
                    .or_default()
                    .push((file.clone(), change));
            }
            SemanticChange::DependencyAdded { name, version } => {
                dep_added
                    .entry((name.clone(), version.clone()))
                    .or_default()
                    .push(change);
            }
            SemanticChange::DependencyRemoved { name } => {
                dep_removed.entry(name.clone()).or_default().push(change);
            }
            _ => {
                individual.push(change);
            }
        }
    }

    let mut groups: Vec<AggregatedChange> = Vec::new();

    // Formatting pass group (only aggregate if 2+ files).
    if formatting_files.len() >= 2 {
        let count = formatting_files.len();
        let files: Vec<PathBuf> = formatting_files.iter().map(|(p, _)| p.clone()).collect();
        let children: Vec<SemanticChange> = formatting_files.into_iter().map(|(_, c)| c).collect();
        groups.push(AggregatedChange {
            label: format!("Formatting pass: {} files", count),
            kind: AggregateKind::FormattingPass,
            files,
            importance: ChangeImportance::Noise,
            children,
        });
    } else {
        individual.extend(formatting_files.into_iter().map(|(_, c)| c));
    }

    // Import updates group.
    if import_files.len() >= 2 {
        let count = import_files.len();
        let files: Vec<PathBuf> = import_files.iter().map(|(p, _)| p.clone()).collect();
        let children: Vec<SemanticChange> = import_files.into_iter().map(|(_, c)| c).collect();
        groups.push(AggregatedChange {
            label: format!("Import updates: {} files", count),
            kind: AggregateKind::ImportUpdates,
            files,
            importance: ChangeImportance::Low,
            children,
        });
    } else {
        individual.extend(import_files.into_iter().map(|(_, c)| c));
    }

    // Comment updates group.
    if comment_files.len() >= 2 {
        let count = comment_files.len();
        let files: Vec<PathBuf> = comment_files.iter().map(|(p, _)| p.clone()).collect();
        let children: Vec<SemanticChange> = comment_files.into_iter().map(|(_, c)| c).collect();
        groups.push(AggregatedChange {
            label: format!("Comment updates: {} files", count),
            kind: AggregateKind::CommentUpdates,
            files,
            importance: ChangeImportance::Low,
            children,
        });
    } else {
        individual.extend(comment_files.into_iter().map(|(_, c)| c));
    }

    // Cross-file function renames (only aggregate if 2+ files share the same rename).
    for ((old_name, new_name), entries) in fn_renames {
        if entries.len() >= 2 {
            let count = entries.len();
            let files: Vec<PathBuf> = entries.iter().map(|(p, _)| p.clone()).collect();
            let children: Vec<SemanticChange> = entries.into_iter().map(|(_, c)| c).collect();
            groups.push(AggregatedChange {
                label: format!("Renamed {} → {} across {} files", old_name, new_name, count),
                kind: AggregateKind::FunctionRename,
                files,
                importance: ChangeImportance::Low,
                children,
            });
        } else {
            individual.extend(entries.into_iter().map(|(_, c)| c));
        }
    }

    // Dependency groups (only aggregate if same dep+version appears 2+ times).
    for ((name, version), entries) in dep_added {
        if entries.len() >= 2 {
            let count = entries.len();
            groups.push(AggregatedChange {
                label: format!("Added dependency {} {} ({} files)", name, version, count),
                kind: AggregateKind::DependencyChange,
                files: Vec::new(),
                importance: ChangeImportance::Low,
                children: entries,
            });
        } else {
            individual.extend(entries);
        }
    }
    for (name, entries) in dep_removed {
        if entries.len() >= 2 {
            let count = entries.len();
            groups.push(AggregatedChange {
                label: format!("Removed dependency {} ({} files)", name, count),
                kind: AggregateKind::DependencyChange,
                files: Vec::new(),
                importance: ChangeImportance::Low,
                children: entries,
            });
        } else {
            individual.extend(entries);
        }
    }

    AggregationResult { individual, groups }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_formatting_files_aggregate() {
        let changes = vec![
            SemanticChange::FileModified {
                path: "a.rs".into(),
                classification: Some(ModificationKind::FormattingOnly),
                importance: Some(ChangeImportance::Noise),
                confidence: None,
            },
            SemanticChange::FileModified {
                path: "b.rs".into(),
                classification: Some(ModificationKind::FormattingOnly),
                importance: Some(ChangeImportance::Noise),
                confidence: None,
            },
            SemanticChange::FileModified {
                path: "c.rs".into(),
                classification: Some(ModificationKind::FormattingOnly),
                importance: Some(ChangeImportance::Noise),
                confidence: None,
            },
            SemanticChange::FileModified {
                path: "logic.rs".into(),
                classification: Some(ModificationKind::Logic),
                importance: Some(ChangeImportance::High),
                confidence: None,
            },
        ];

        let result = aggregate_changes(changes);
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.groups[0].kind, AggregateKind::FormattingPass);
        assert_eq!(result.groups[0].files.len(), 3);
        assert_eq!(result.groups[0].children.len(), 3);
        // The logic file stays individual.
        assert_eq!(result.individual.len(), 1);
    }

    #[test]
    fn test_single_formatting_file_not_aggregated() {
        let changes = vec![SemanticChange::FileModified {
            path: "a.rs".into(),
            classification: Some(ModificationKind::FormattingOnly),
            importance: Some(ChangeImportance::Noise),
            confidence: None,
        }];
        let result = aggregate_changes(changes);
        assert_eq!(result.groups.len(), 0);
        assert_eq!(result.individual.len(), 1);
    }

    #[test]
    fn test_cross_file_rename_aggregates() {
        let changes = vec![
            SemanticChange::FunctionRenamed {
                file: "a.rs".into(),
                old_name: "foo".into(),
                new_name: "bar".into(),
                importance: Some(ChangeImportance::Low),
            },
            SemanticChange::FunctionRenamed {
                file: "b.rs".into(),
                old_name: "foo".into(),
                new_name: "bar".into(),
                importance: Some(ChangeImportance::Low),
            },
            SemanticChange::FunctionRenamed {
                file: "c.rs".into(),
                old_name: "baz".into(),
                new_name: "qux".into(),
                importance: Some(ChangeImportance::Low),
            },
        ];

        let result = aggregate_changes(changes);
        // foo→bar aggregates (2 files), baz→qux stays individual (1 file).
        assert_eq!(result.groups.len(), 1);
        assert!(result.groups[0].label.contains("foo"));
        assert_eq!(result.groups[0].files.len(), 2);
        assert_eq!(result.individual.len(), 1);
    }

    #[test]
    fn test_dep_added_distinguishes_versions() {
        // Same dep name at two different versions must NOT collapse into one group.
        // Pre-fix the key was just `name`, so `serde 1.0` and `serde 2.0` merged.
        let changes = vec![
            SemanticChange::DependencyAdded {
                name: "serde".into(),
                version: "1.0".into(),
            },
            SemanticChange::DependencyAdded {
                name: "serde".into(),
                version: "1.0".into(),
            },
            SemanticChange::DependencyAdded {
                name: "serde".into(),
                version: "2.0".into(),
            },
            SemanticChange::DependencyAdded {
                name: "serde".into(),
                version: "2.0".into(),
            },
        ];

        let result = aggregate_changes(changes);
        assert_eq!(
            result.groups.len(),
            2,
            "expected separate groups for serde 1.0 and serde 2.0, got {:?}",
            result.groups.iter().map(|g| &g.label).collect::<Vec<_>>()
        );
        for g in &result.groups {
            assert_eq!(g.kind, AggregateKind::DependencyChange);
            assert_eq!(g.children.len(), 2);
        }
        let labels: Vec<&String> = result.groups.iter().map(|g| &g.label).collect();
        assert!(
            labels.iter().any(|l| l.contains("1.0")),
            "expected a label mentioning 1.0, got {:?}",
            labels
        );
        assert!(
            labels.iter().any(|l| l.contains("2.0")),
            "expected a label mentioning 2.0, got {:?}",
            labels
        );
    }

    #[test]
    fn test_mixed_aggregation() {
        let changes = vec![
            SemanticChange::FileModified {
                path: "fmt1.rs".into(),
                classification: Some(ModificationKind::FormattingOnly),
                importance: Some(ChangeImportance::Noise),
                confidence: None,
            },
            SemanticChange::FileModified {
                path: "fmt2.rs".into(),
                classification: Some(ModificationKind::WhitespaceOnly),
                importance: Some(ChangeImportance::Noise),
                confidence: None,
            },
            SemanticChange::FileModified {
                path: "imp1.rs".into(),
                classification: Some(ModificationKind::ImportsOnly),
                importance: Some(ChangeImportance::Low),
                confidence: None,
            },
            SemanticChange::FileModified {
                path: "imp2.rs".into(),
                classification: Some(ModificationKind::ImportsOnly),
                importance: Some(ChangeImportance::Low),
                confidence: None,
            },
            SemanticChange::FileAdded {
                path: "new.rs".into(),
            },
        ];

        let result = aggregate_changes(changes);
        assert_eq!(result.groups.len(), 2); // formatting + imports
        assert_eq!(result.individual.len(), 1); // FileAdded
    }
}
