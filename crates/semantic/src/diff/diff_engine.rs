// SPDX-License-Identifier: Apache-2.0
//! Shared semantic engine used by check-only, summary, and full diff APIs.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use objects::object::{DiffKind, FileChangeSet};

use super::{
    diff_options::SemanticDiffOptions,
    diff_support::{
        EngineOutput, LoadedChange, ParsedChangeSet, apply_renames, build_file_level_changes,
        detect_renames, fallback_file_changes, function_and_import_changes, load_manifest,
        suppress_redundant_file_modified,
    },
    diff_types::{
        SemanticCheckOnlyResult, SemanticCheckStatus, SemanticDiffResult, SemanticFallbackReason,
        SemanticSummaryResult,
    },
};
use crate::{
    analysis::aggregate_changes,
    cache::SemanticParseCache,
    parser::{Language, ParsedFile},
};

pub(crate) struct SemanticEngine<'a, F, G>
where
    F: FnMut(&Path) -> Result<Option<String>, anyhow::Error>,
    G: FnMut(&Path) -> Result<Option<String>, anyhow::Error>,
{
    file_changes: FileChangeSet,
    load_old: F,
    load_new: G,
    options: &'a SemanticDiffOptions,
    cache: &'a SemanticParseCache,
}

impl<'a, F, G> SemanticEngine<'a, F, G>
where
    F: FnMut(&Path) -> Result<Option<String>, anyhow::Error>,
    G: FnMut(&Path) -> Result<Option<String>, anyhow::Error>,
{
    pub(crate) fn new(
        file_changes: FileChangeSet,
        load_old: F,
        load_new: G,
        options: &'a SemanticDiffOptions,
        cache: &'a SemanticParseCache,
    ) -> Self {
        Self {
            file_changes,
            load_old,
            load_new,
            options,
            cache,
        }
    }

    pub(crate) fn check_only(mut self) -> Result<SemanticCheckOnlyResult, anyhow::Error> {
        let mut fallback_reasons = self.budget_file_changes();
        if !fallback_reasons.is_empty() {
            return Ok(self.finish_check(SemanticCheckStatus::Fallback, fallback_reasons));
        }

        let changed_paths: Vec<PathBuf> = self
            .file_changes
            .iter()
            .filter(|change| change.kind == DiffKind::Modified)
            .map(|change| PathBuf::from(&change.path))
            .collect();

        if self
            .file_changes
            .iter()
            .any(|change| matches!(change.kind, DiffKind::Added | DiffKind::Deleted))
        {
            return Ok(self.finish_check(SemanticCheckStatus::HasChanges, fallback_reasons));
        }

        for path in changed_paths {
            if self.modified_contents_differ(&path, &mut fallback_reasons)? {
                let status = if fallback_reasons.is_empty() {
                    SemanticCheckStatus::HasChanges
                } else {
                    SemanticCheckStatus::Fallback
                };
                return Ok(self.finish_check(status, fallback_reasons));
            }
        }

        Ok(self.finish_check(SemanticCheckStatus::NoChanges, fallback_reasons))
    }

    pub(crate) fn summary(self) -> Result<SemanticSummaryResult, anyhow::Error> {
        let execution = self.execute(true)?;
        Ok(SemanticSummaryResult {
            file_renames: execution.file_renames,
            file_changes: execution.file_changes,
            aggregated: execution.aggregated,
            fallback_reasons: execution.fallback_reasons,
        })
    }

    pub(crate) fn full(self) -> Result<SemanticDiffResult, anyhow::Error> {
        let execution = self.execute(true)?;
        Ok(SemanticDiffResult {
            changes: execution.changes,
            file_renames: execution.file_renames,
            file_changes: execution.file_changes,
            aggregated: execution.aggregated,
            fallback_reasons: execution.fallback_reasons,
        })
    }

    fn execute(mut self, aggregate: bool) -> Result<EngineOutput, anyhow::Error> {
        let mut output = EngineOutput::new(self.file_changes.clone());
        output.fallback_reasons = self.budget_file_changes();
        if !output.fallback_reasons.is_empty() {
            output.changes = fallback_file_changes(&self.file_changes);
            output.aggregated = aggregate.then(|| aggregate_changes(output.changes.clone()));
            return Ok(output);
        }

        let loaded = self.load_changes(&mut output.fallback_reasons)?;
        output.changes = build_file_level_changes(&loaded);
        output.file_renames = detect_renames(&loaded, self.options);
        apply_renames(&mut output.changes, &output.file_renames);

        if self.options.analyze_functions {
            let parsed = self.parse_files(&loaded, &mut output.fallback_reasons);
            let manifest = if self.options.analyze_dependencies
                && super::diff_support::dependency_manifest_may_be_needed(&loaded, &parsed)
            {
                load_manifest(&loaded, &mut self.load_new, self.options)?
            } else {
                None
            };
            output.changes.extend(function_and_import_changes(
                &loaded,
                &parsed,
                self.options,
                manifest.as_deref(),
                &output.file_renames,
            ));
            suppress_redundant_file_modified(&mut output.changes);
        }

        output.aggregated = aggregate.then(|| aggregate_changes(output.changes.clone()));
        Ok(output)
    }

    fn finish_check(
        self,
        status: SemanticCheckStatus,
        fallback_reasons: Vec<SemanticFallbackReason>,
    ) -> SemanticCheckOnlyResult {
        SemanticCheckOnlyResult {
            status,
            file_changes: self.file_changes,
            fallback_reasons,
        }
    }

    fn budget_file_changes(&self) -> Vec<SemanticFallbackReason> {
        if self.file_changes.len() > self.options.budget.max_changed_files {
            return vec![SemanticFallbackReason::ChangedFileBudgetExceeded {
                limit: self.options.budget.max_changed_files,
                actual: self.file_changes.len(),
            }];
        }
        Vec::new()
    }

    fn modified_contents_differ(
        &mut self,
        path: &Path,
        fallback_reasons: &mut Vec<SemanticFallbackReason>,
    ) -> Result<bool, anyhow::Error> {
        let old_content = (self.load_old)(path)?;
        let new_content = (self.load_new)(path)?;
        let total_bytes = old_content.as_ref().map_or(0, String::len)
            + new_content.as_ref().map_or(0, String::len);
        if total_bytes > self.options.budget.max_total_bytes {
            fallback_reasons.push(SemanticFallbackReason::TotalByteBudgetExceeded {
                limit: self.options.budget.max_total_bytes,
                actual: total_bytes,
            });
            return Ok(true);
        }
        Ok(old_content != new_content)
    }

    fn load_changes(
        &mut self,
        fallback_reasons: &mut Vec<SemanticFallbackReason>,
    ) -> Result<Vec<LoadedChange>, anyhow::Error> {
        let mut loaded = Vec::with_capacity(self.file_changes.len());
        let mut total_bytes = 0usize;
        for change in &self.file_changes {
            let path = PathBuf::from(&change.path);
            let old_content = match change.kind {
                DiffKind::Deleted | DiffKind::Modified => (self.load_old)(&path)?,
                _ => None,
            };
            let new_content = match change.kind {
                DiffKind::Added | DiffKind::Modified => (self.load_new)(&path)?,
                _ => None,
            };
            total_bytes += old_content.as_ref().map_or(0, String::len)
                + new_content.as_ref().map_or(0, String::len);
            loaded.push(LoadedChange::new(
                change.clone(),
                path,
                old_content,
                new_content,
            ));
        }

        if total_bytes > self.options.budget.max_total_bytes {
            fallback_reasons.push(SemanticFallbackReason::TotalByteBudgetExceeded {
                limit: self.options.budget.max_total_bytes,
                actual: total_bytes,
            });
        }
        Ok(loaded)
    }

    fn parse_files(
        &self,
        loaded: &[LoadedChange],
        fallback_reasons: &mut Vec<SemanticFallbackReason>,
    ) -> ParsedChangeSet {
        let mut parsed = ParsedChangeSet::default();
        for change in loaded {
            self.record_parse(
                &mut parsed.old,
                &change.path,
                change.old_content.as_deref(),
                &mut parsed.parsed_count,
                fallback_reasons,
            );
            self.record_parse(
                &mut parsed.new,
                &change.path,
                change.new_content.as_deref(),
                &mut parsed.parsed_count,
                fallback_reasons,
            );
        }
        parsed
    }

    fn record_parse(
        &self,
        target: &mut std::collections::HashMap<PathBuf, Option<Arc<ParsedFile>>>,
        path: &Path,
        content: Option<&str>,
        parsed_count: &mut usize,
        fallback_reasons: &mut Vec<SemanticFallbackReason>,
    ) {
        if let Some(content) = content {
            target.insert(
                path.to_path_buf(),
                self.parse_for_path(path, content, parsed_count, fallback_reasons),
            );
        }
    }

    fn parse_for_path(
        &self,
        path: &Path,
        content: &str,
        parsed_count: &mut usize,
        fallback_reasons: &mut Vec<SemanticFallbackReason>,
    ) -> Option<Arc<ParsedFile>> {
        if content.len() > self.options.budget.max_file_bytes {
            fallback_reasons.push(SemanticFallbackReason::FileTooLarge {
                path: path.to_path_buf(),
                limit: self.options.budget.max_file_bytes,
                actual: content.len(),
            });
            return None;
        }

        let language = Language::from_path(path);
        if language == Language::Unknown {
            fallback_reasons.push(SemanticFallbackReason::UnsupportedLanguage {
                path: path.to_path_buf(),
            });
            return None;
        }
        if *parsed_count >= self.options.budget.max_parsed_files {
            fallback_reasons.push(SemanticFallbackReason::ParseBudgetExceeded {
                limit: self.options.budget.max_parsed_files,
                attempted: *parsed_count + 1,
            });
            return None;
        }

        *parsed_count += 1;
        let parsed = self.cache.parse(content, language);
        if parsed.is_none() {
            fallback_reasons.push(SemanticFallbackReason::ParseFailed {
                path: path.to_path_buf(),
            });
        }
        parsed
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use objects::object::{DiffKind, FileChangeSet};

    use super::*;
    use crate::cache::SemanticParseCache;

    #[test]
    fn pure_function_body_diff_does_not_load_dependency_manifest() {
        let file_changes = FileChangeSet::from(vec![("lib.rs".to_string(), DiffKind::Modified)]);
        let cache = SemanticParseCache::default();
        let options = SemanticDiffOptions::default();
        let engine = SemanticEngine::new(
            file_changes,
            |path| {
                assert_eq!(path, Path::new("lib.rs"));
                Ok(Some("fn compute() -> i32 {\n    1\n}\n".to_string()))
            },
            |path| {
                assert_eq!(
                    path,
                    Path::new("lib.rs"),
                    "pure body edits should not load Cargo.toml/package.json"
                );
                Ok(Some("fn compute() -> i32 {\n    2\n}\n".to_string()))
            },
            &options,
            &cache,
        );

        let result = engine.full().expect("semantic diff should succeed");
        assert!(
            result
                .changes
                .iter()
                .any(|change| matches!(change, objects::object::SemanticChange::FunctionModified { name, .. } if name == "compute")),
            "expected function modification: {:?}",
            result.changes
        );
    }

    #[test]
    fn dependency_import_diff_loads_manifest_only_when_imports_change() {
        let file_changes = FileChangeSet::from(vec![("lib.rs".to_string(), DiffKind::Modified)]);
        let cache = SemanticParseCache::default();
        let options = SemanticDiffOptions::default();
        let engine = SemanticEngine::new(
            file_changes,
            |path| {
                assert_eq!(path, Path::new("lib.rs"));
                Ok(Some("use std::fmt;\nfn render() {}\n".to_string()))
            },
            |path| match path {
                p if p == Path::new("lib.rs") => {
                    Ok(Some("use serde::Serialize;\nfn render() {}\n".to_string()))
                }
                p if p == Path::new("Cargo.toml") => {
                    Ok(Some("[dependencies]\nserde = \"1.0\"\n".to_string()))
                }
                other => panic!("unexpected manifest lookup: {}", other.display()),
            },
            &options,
            &cache,
        );

        let result = engine.full().expect("semantic diff should succeed");
        assert!(
            result.changes.iter().any(|change| {
                matches!(
                    change,
                    objects::object::SemanticChange::DependencyAdded { name, version }
                        if name == "serde" && version == "1.0"
                )
            }),
            "expected dependency add with manifest version: {:?}",
            result.changes
        );
    }
}
