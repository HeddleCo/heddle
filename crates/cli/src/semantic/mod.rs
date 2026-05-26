// SPDX-License-Identifier: Apache-2.0
//! Semantic facade for the CLI crate.

use std::path::PathBuf;

use objects::object::{ContentHash, FileChangeSet};
use repo::{Repository, WorktreeStatusOptions};
use semantic::diff::WorktreeStatus;
pub use semantic::diff::{
    SemanticCheckOnlyResult, SemanticCheckStatus, SemanticDiffOptions, SemanticFallbackReason,
    SemanticSummaryResult,
};

#[derive(Clone, Debug, Default)]
pub struct SemanticDiffResult {
    pub changes: Vec<objects::object::SemanticChange>,
    pub file_renames: Vec<(PathBuf, PathBuf)>,
    pub file_changes: FileChangeSet,
}

pub fn semantic_diff(
    repo: &Repository,
    from_tree_hash: &ContentHash,
    to_tree_hash: &ContentHash,
    options: &SemanticDiffOptions,
) -> Result<SemanticDiffResult, anyhow::Error> {
    let result =
        semantic::diff::semantic_diff(repo.store(), from_tree_hash, to_tree_hash, options)?;
    Ok(map_result(result))
}

pub fn semantic_check_only(
    repo: &Repository,
    from_tree_hash: &ContentHash,
    to_tree_hash: &ContentHash,
    options: &SemanticDiffOptions,
) -> Result<SemanticCheckOnlyResult, anyhow::Error> {
    semantic::diff::semantic_check_only(repo.store(), from_tree_hash, to_tree_hash, options)
}

pub fn semantic_diff_summary(
    repo: &Repository,
    from_tree_hash: &ContentHash,
    to_tree_hash: &ContentHash,
    options: &SemanticDiffOptions,
) -> Result<SemanticSummaryResult, anyhow::Error> {
    semantic::diff::semantic_diff_summary(repo.store(), from_tree_hash, to_tree_hash, options)
}

pub fn semantic_diff_worktree(
    repo: &Repository,
    from_tree_hash: &ContentHash,
    options: &SemanticDiffOptions,
    status_options: &WorktreeStatusOptions,
) -> Result<SemanticDiffResult, anyhow::Error> {
    let from_tree = repo.require_tree(from_tree_hash)?;
    let status = repo.compare_worktree_cached_with_options(&from_tree, status_options)?;

    let status = WorktreeStatus {
        modified: status.modified,
        added: status.added,
        deleted: status.deleted,
    };

    let result = semantic::diff::semantic_diff_worktree(
        repo.store(),
        from_tree_hash,
        repo.root(),
        &status,
        options,
    )?;

    Ok(map_result(result))
}

pub fn semantic_check_only_worktree(
    repo: &Repository,
    from_tree_hash: &ContentHash,
    options: &SemanticDiffOptions,
    status_options: &WorktreeStatusOptions,
) -> Result<SemanticCheckOnlyResult, anyhow::Error> {
    let from_tree = repo.require_tree(from_tree_hash)?;
    let status = repo.compare_worktree_cached_with_options(&from_tree, status_options)?;

    let status = WorktreeStatus {
        modified: status.modified,
        added: status.added,
        deleted: status.deleted,
    };

    semantic::diff::semantic_check_only_worktree(
        repo.store(),
        from_tree_hash,
        repo.root(),
        &status,
        options,
    )
}

pub fn semantic_diff_summary_worktree(
    repo: &Repository,
    from_tree_hash: &ContentHash,
    options: &SemanticDiffOptions,
    status_options: &WorktreeStatusOptions,
) -> Result<SemanticSummaryResult, anyhow::Error> {
    let from_tree = repo.require_tree(from_tree_hash)?;
    let status = repo.compare_worktree_cached_with_options(&from_tree, status_options)?;

    let status = WorktreeStatus {
        modified: status.modified,
        added: status.added,
        deleted: status.deleted,
    };

    semantic::diff::semantic_diff_summary_worktree(
        repo.store(),
        from_tree_hash,
        repo.root(),
        &status,
        options,
    )
}

fn map_result(result: semantic::diff::SemanticDiffResult) -> SemanticDiffResult {
    SemanticDiffResult {
        changes: result.changes,
        file_renames: result.file_renames,
        file_changes: result.file_changes,
    }
}
