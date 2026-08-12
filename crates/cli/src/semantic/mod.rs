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

#[cfg(test)]
mod tests {
    use std::fs;

    use objects::object::ContentHash;
    use repo::{Repository, WorktreeStatusOptions};
    use semantic::diff::SemanticDiffOptions;

    use super::*;

    fn two_tree_repo() -> (tempfile::TempDir, Repository, ContentHash, ContentHash) {
        let temp = tempfile::TempDir::new().expect("temp");
        let repo = Repository::init_default(temp.path()).expect("init");
        fs::write(temp.path().join("a.rs"), b"fn a() {}\n").unwrap();
        let base = repo
            .snapshot(Some("base".into()), None)
            .expect("base snapshot");
        fs::write(temp.path().join("a.rs"), b"fn a() { let x = 1; }\n").unwrap();
        fs::write(temp.path().join("b.rs"), b"fn b() {}\n").unwrap();
        let head = repo
            .snapshot(Some("head".into()), None)
            .expect("head snapshot");
        (temp, repo, base.tree, head.tree)
    }

    #[test]
    fn semantic_diff_and_check_and_summary_cover_facade() {
        let (_temp, repo, from, to) = two_tree_repo();
        let options = SemanticDiffOptions::default();

        let diff = semantic_diff(&repo, &from, &to, &options).expect("diff");
        assert!(
            !diff.file_changes.is_empty() || !diff.changes.is_empty(),
            "expected some semantic or file-level change"
        );

        let check = semantic_check_only(&repo, &from, &to, &options).expect("check");
        let _ = check;

        let summary = semantic_diff_summary(&repo, &from, &to, &options).expect("summary");
        let _ = summary;
    }

    #[test]
    fn semantic_worktree_facades_cover_dirty_paths() {
        let (temp, repo, from, _to) = two_tree_repo();
        // Dirty the worktree relative to `from`.
        fs::write(temp.path().join("a.rs"), b"fn a() { dirty(); }\n").unwrap();
        fs::write(temp.path().join("c.rs"), b"fn c() {}\n").unwrap();
        let options = SemanticDiffOptions::default();
        let status_options = WorktreeStatusOptions::default();

        let diff =
            semantic_diff_worktree(&repo, &from, &options, &status_options).expect("wt diff");
        let _ = diff;
        let check = semantic_check_only_worktree(&repo, &from, &options, &status_options)
            .expect("wt check");
        let _ = check;
        let summary = semantic_diff_summary_worktree(&repo, &from, &options, &status_options)
            .expect("wt summary");
        let _ = summary;
    }
}
