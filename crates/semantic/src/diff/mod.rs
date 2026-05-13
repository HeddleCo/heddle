// SPDX-License-Identifier: Apache-2.0
//! Semantic diff: detect high-level code changes.

mod diff_core;
mod diff_engine;
mod diff_helpers;
mod diff_options;
mod diff_support;
mod diff_types;

#[cfg(test)]
mod diff_tests;

pub use diff_core::{
    semantic_check_only, semantic_check_only_with_cache, semantic_check_only_worktree,
    semantic_check_only_worktree_with_cache, semantic_diff, semantic_diff_summary,
    semantic_diff_summary_with_cache, semantic_diff_summary_worktree,
    semantic_diff_summary_worktree_with_cache, semantic_diff_with_cache, semantic_diff_worktree,
    semantic_diff_worktree_with_cache,
};
pub use diff_options::SemanticDiffOptions;
pub use diff_types::{
    SemanticBudget, SemanticCheckOnlyResult, SemanticCheckStatus, SemanticDiffResult,
    SemanticFallbackReason, SemanticSummaryResult,
};
pub use objects::{object::DiffKind, worktree::WorktreeStatus};