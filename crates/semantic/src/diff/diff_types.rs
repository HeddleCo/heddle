// SPDX-License-Identifier: Apache-2.0
//! Shared semantic diff result and budget types.

use std::path::PathBuf;

use objects::object::{FileChangeSet, SemanticChange};

use crate::analysis::AggregationResult;

/// Resource limits used by semantic analysis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticBudget {
    /// Maximum number of changed files to analyze semantically.
    pub max_changed_files: usize,
    /// Maximum total bytes to load across changed files.
    pub max_total_bytes: usize,
    /// Maximum number of files that may be parsed structurally.
    pub max_parsed_files: usize,
    /// Maximum file size eligible for parsing.
    pub max_file_bytes: usize,
}

impl SemanticBudget {
    /// Returns a budget with no practical limits.
    pub fn unlimited() -> Self {
        Self {
            max_changed_files: usize::MAX,
            max_total_bytes: usize::MAX,
            max_parsed_files: usize::MAX,
            max_file_bytes: usize::MAX,
        }
    }
}

impl Default for SemanticBudget {
    fn default() -> Self {
        Self {
            max_changed_files: 2_048,
            max_total_bytes: 16 * 1024 * 1024,
            max_parsed_files: 512,
            max_file_bytes: 1024 * 1024,
        }
    }
}

/// Conservative reason that semantic analysis skipped or degraded work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SemanticFallbackReason {
    /// The changed-file set exceeded the configured limit.
    ChangedFileBudgetExceeded { limit: usize, actual: usize },
    /// Loaded content exceeded the configured byte budget.
    TotalByteBudgetExceeded { limit: usize, actual: usize },
    /// A file was too large for structural parsing.
    FileTooLarge {
        path: PathBuf,
        limit: usize,
        actual: usize,
    },
    /// Structural parsing hit the configured file-count budget.
    ParseBudgetExceeded { limit: usize, attempted: usize },
    /// The file language is unsupported for structural parsing.
    UnsupportedLanguage { path: PathBuf },
    /// Tree-sitter failed to produce a clean parse.
    ParseFailed { path: PathBuf },
}

/// Result classification for the cheap semantic check path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticCheckStatus {
    /// The requested change-set is semantically empty.
    NoChanges,
    /// The requested change-set definitely contains changes.
    HasChanges,
    /// The fast path could not complete within the configured budget.
    Fallback,
}

/// Result of the cheap semantic no-op check.
#[derive(Clone, Debug)]
pub struct SemanticCheckOnlyResult {
    /// Final status of the fast path.
    pub status: SemanticCheckStatus,
    /// Raw file changes considered by the engine.
    pub file_changes: FileChangeSet,
    /// Explicit reasons why the fast path degraded.
    pub fallback_reasons: Vec<SemanticFallbackReason>,
}

/// Aggregated semantic summary without the full detailed change list.
#[derive(Clone, Debug, Default)]
pub struct SemanticSummaryResult {
    /// Files that were classified as renames.
    pub file_renames: Vec<(PathBuf, PathBuf)>,
    /// Raw file-level changes.
    pub file_changes: FileChangeSet,
    /// Aggregated semantic groups.
    pub aggregated: Option<AggregationResult>,
    /// Explicit reasons why parts of semantic analysis degraded.
    pub fallback_reasons: Vec<SemanticFallbackReason>,
}

/// Result of full semantic diff analysis.
#[derive(Clone, Debug, Default)]
pub struct SemanticDiffResult {
    /// High-level semantic changes detected.
    pub changes: Vec<SemanticChange>,
    /// Files that were renamed (old -> new).
    pub file_renames: Vec<(PathBuf, PathBuf)>,
    /// Raw file-level changes.
    pub file_changes: FileChangeSet,
    /// Aggregated changes (groups formatting passes, cross-file renames, etc.)
    pub aggregated: Option<AggregationResult>,
    /// Explicit reasons why parts of semantic analysis degraded.
    pub fallback_reasons: Vec<SemanticFallbackReason>,
}