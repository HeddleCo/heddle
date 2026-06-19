// SPDX-License-Identifier: Apache-2.0
//! Shared execution helpers for the semantic engine.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use objects::object::{DiffKind, FileChange, FileChangeSet, SemanticChange};

use super::{diff_options::SemanticDiffOptions, diff_types::SemanticFallbackReason};
use crate::{
    analysis::{
        AggregationResult, classify_modification_with_confidence, detect_file_renames,
        detect_function_changes_with_parsed, detect_import_changes_with_parsed,
    },
    parser::ParsedFile,
};

#[derive(Default)]
pub(super) struct EngineOutput {
    pub(super) changes: Vec<SemanticChange>,
    pub(super) file_renames: Vec<(PathBuf, PathBuf)>,
    pub(super) file_changes: FileChangeSet,
    pub(super) aggregated: Option<AggregationResult>,
    pub(super) fallback_reasons: Vec<SemanticFallbackReason>,
}

impl EngineOutput {
    pub(super) fn new(file_changes: FileChangeSet) -> Self {
        Self {
            file_changes,
            ..Self::default()
        }
    }
}

#[derive(Clone)]
pub(super) struct LoadedChange {
    pub(super) change: FileChange,
    pub(super) path: PathBuf,
    pub(super) old_content: Option<String>,
    pub(super) new_content: Option<String>,
}

impl LoadedChange {
    pub(super) fn new(
        change: FileChange,
        path: PathBuf,
        old_content: Option<String>,
        new_content: Option<String>,
    ) -> Self {
        Self {
            change,
            path,
            old_content,
            new_content,
        }
    }
}

#[derive(Default)]
pub(super) struct ParsedChangeSet {
    pub(super) old: HashMap<PathBuf, Option<Arc<ParsedFile>>>,
    pub(super) new: HashMap<PathBuf, Option<Arc<ParsedFile>>>,
    pub(super) parsed_count: usize,
}

pub(super) fn fallback_file_changes(file_changes: &FileChangeSet) -> Vec<SemanticChange> {
    file_changes
        .iter()
        .map(|change| match change.kind {
            DiffKind::Added => SemanticChange::FileAdded {
                path: PathBuf::from(&change.path),
            },
            DiffKind::Deleted => SemanticChange::FileDeleted {
                path: PathBuf::from(&change.path),
            },
            DiffKind::Modified | DiffKind::Unchanged => SemanticChange::FileModified {
                path: PathBuf::from(&change.path),
                classification: None,
                importance: None,
                confidence: None,
            },
        })
        .collect()
}

pub(super) fn build_file_level_changes(loaded: &[LoadedChange]) -> Vec<SemanticChange> {
    let mut changes = Vec::with_capacity(loaded.len());
    for change in loaded {
        match change.change.kind {
            DiffKind::Deleted => changes.push(SemanticChange::FileDeleted {
                path: change.path.clone(),
            }),
            DiffKind::Added => changes.push(SemanticChange::FileAdded {
                path: change.path.clone(),
            }),
            DiffKind::Modified => push_modified_change(&mut changes, change),
            DiffKind::Unchanged => {}
        }
    }
    changes
}

pub(super) fn detect_renames(
    loaded: &[LoadedChange],
    options: &SemanticDiffOptions,
) -> Vec<(PathBuf, PathBuf)> {
    let deleted: Vec<_> = loaded
        .iter()
        .filter_map(|change| {
            if change.change.kind == DiffKind::Deleted {
                change
                    .old_content
                    .clone()
                    .map(|content| (change.path.clone(), content))
            } else {
                None
            }
        })
        .collect();
    let added: Vec<_> = loaded
        .iter()
        .filter_map(|change| {
            if change.change.kind == DiffKind::Added {
                change
                    .new_content
                    .clone()
                    .map(|content| (change.path.clone(), content))
            } else {
                None
            }
        })
        .collect();
    detect_file_renames(
        &deleted,
        &added,
        options.rename_threshold,
        options.similarity_method,
    )
}

pub(super) fn apply_renames(changes: &mut Vec<SemanticChange>, renames: &[(PathBuf, PathBuf)]) {
    for (from, to) in renames {
        changes.retain(|change| {
            !matches!(change, SemanticChange::FileDeleted { path } if path == from)
                && !matches!(change, SemanticChange::FileAdded { path } if path == to)
        });
        changes.push(SemanticChange::FileRenamed {
            from: from.clone(),
            to: to.clone(),
        });
    }
}

pub(super) fn load_manifest<G>(
    loaded: &[LoadedChange],
    load_new: &mut G,
    options: &SemanticDiffOptions,
) -> Result<Option<String>, anyhow::Error>
where
    G: FnMut(&Path) -> Result<Option<String>, anyhow::Error>,
{
    if !options.analyze_dependencies {
        return Ok(None);
    }

    let manifest = loaded.iter().find_map(|change| {
        matches!(change.path.as_path(), path if path == Path::new("Cargo.toml") || path == Path::new("package.json"))
            .then(|| change.new_content.clone())
            .flatten()
    });
    if manifest.is_some() {
        return Ok(manifest);
    }

    let cargo = load_new(Path::new("Cargo.toml"))?;
    if cargo.is_some() {
        return Ok(cargo);
    }
    load_new(Path::new("package.json"))
}

pub(super) fn suppress_redundant_file_modified(changes: &mut Vec<SemanticChange>) {
    let specific_paths = changes
        .iter()
        .filter_map(|change| match change {
            SemanticChange::FunctionAdded { file, .. }
            | SemanticChange::FunctionExtracted { file, .. }
            | SemanticChange::FunctionDeleted { file, .. }
            | SemanticChange::FunctionRenamed { file, .. }
            | SemanticChange::FunctionModified { file, .. }
            | SemanticChange::FunctionMoved { file, .. }
            | SemanticChange::SignatureChanged { file, .. } => Some(file.clone()),
            _ => None,
        })
        .collect::<HashSet<_>>();

    if specific_paths.is_empty() {
        return;
    }

    changes.retain(|change| {
        !matches!(
            change,
            SemanticChange::FileModified { path, .. } if specific_paths.contains(path)
        )
    });
}

pub(super) fn function_and_import_changes(
    loaded: &[LoadedChange],
    parsed: &ParsedChangeSet,
    options: &SemanticDiffOptions,
    manifest_content: Option<&str>,
    renames: &[(PathBuf, PathBuf)],
) -> Vec<SemanticChange> {
    let mut changes = Vec::new();
    for change in loaded {
        match change.change.kind {
            DiffKind::Modified => {
                extend_modified_changes(&mut changes, change, parsed, options, manifest_content)
            }
            DiffKind::Added => extend_added_changes(&mut changes, change, parsed, renames),
            _ => {}
        }
    }
    changes
}

pub(super) fn dependency_manifest_may_be_needed(
    loaded: &[LoadedChange],
    parsed: &ParsedChangeSet,
) -> bool {
    loaded.iter().any(|change| {
        if change.change.kind != DiffKind::Modified {
            return false;
        }

        let old_imports = parsed
            .old
            .get(&change.path)
            .and_then(|value| value.as_deref())
            .map(import_dependency_names)
            .unwrap_or_default();
        let new_imports = parsed
            .new
            .get(&change.path)
            .and_then(|value| value.as_deref())
            .map(import_dependency_names)
            .unwrap_or_default();

        old_imports != new_imports
    })
}

fn import_dependency_names(parsed: &ParsedFile) -> HashSet<String> {
    parsed
        .extract_imports()
        .into_iter()
        .map(|import| import.raw)
        .collect()
}

fn push_modified_change(changes: &mut Vec<SemanticChange>, change: &LoadedChange) {
    let metadata = change
        .old_content
        .as_deref()
        .zip(change.new_content.as_deref())
        .map(|(old_content, new_content)| {
            classify_modification_with_confidence(&change.path, old_content, new_content)
        });
    changes.push(SemanticChange::FileModified {
        path: change.path.clone(),
        classification: metadata.map(|(classification, _, _)| classification),
        importance: metadata.map(|(_, importance, _)| importance),
        confidence: metadata.map(|(_, _, confidence)| confidence),
    });
}

fn extend_modified_changes(
    changes: &mut Vec<SemanticChange>,
    change: &LoadedChange,
    parsed: &ParsedChangeSet,
    options: &SemanticDiffOptions,
    manifest_content: Option<&str>,
) {
    let Some(old_parsed) = parsed
        .old
        .get(&change.path)
        .and_then(|value| value.as_deref())
    else {
        return;
    };
    let Some(new_parsed) = parsed
        .new
        .get(&change.path)
        .and_then(|value| value.as_deref())
    else {
        return;
    };
    changes.extend(detect_function_changes_with_parsed(
        &change.path,
        &change.path,
        Some(old_parsed),
        Some(new_parsed),
        options.similarity_method,
    ));
    if options.analyze_dependencies {
        changes.extend(detect_import_changes_with_parsed(
            &change.path,
            &change.path,
            Some(old_parsed),
            Some(new_parsed),
            manifest_content,
        ));
    }
}

fn extend_added_changes(
    changes: &mut Vec<SemanticChange>,
    change: &LoadedChange,
    parsed: &ParsedChangeSet,
    renames: &[(PathBuf, PathBuf)],
) {
    if renames.iter().any(|(_, to)| to == &change.path) {
        return;
    }

    if let Some(Some(parsed_new)) = parsed.new.get(&change.path) {
        for function in parsed_new.extract_functions() {
            changes.push(SemanticChange::FunctionAdded {
                file: change.path.clone(),
                name: function.name,
                importance: Some(objects::object::ChangeImportance::High),
            });
        }
    }
}
