// SPDX-License-Identifier: Apache-2.0
//! Shared dirty-worktree guard for destructive worktree-mutation commands.

use anyhow::{Result, anyhow};
use cli_shared::UserConfig;
use repo::{Repository, WorktreeStatusDetailed};

use super::advice;

/// Refuse to perform a destructive worktree apply when uncommitted changes exist.
pub fn ensure_worktree_clean(repo: &Repository, action: &str) -> Result<()> {
    let Some(head) = repo.head()? else {
        return Ok(());
    };
    let Some(tree) = repo.get_tree_for_state(&head)? else {
        return Ok(());
    };
    let options = UserConfig::default().worktree_status_options(Some(repo.config()));
    let detailed: WorktreeStatusDetailed =
        repo.compare_worktree_cached_detailed_with_options(&tree, &options)?;
    if detailed.is_clean() {
        return Ok(());
    }

    Err(anyhow!(advice::dirty_worktree(
        action,
        dirty_paths(&detailed),
        "repository state and worktree files were left unchanged; no snapshot has been written for these paths",
    )))
}

fn dirty_paths(detailed: &WorktreeStatusDetailed) -> Vec<String> {
    let untracked = detailed.untracked.flatten_paths();
    detailed
        .modified
        .iter()
        .map(|path| format!("modified: {}", path.display()))
        .chain(
            detailed
                .deleted
                .iter()
                .map(|path| format!("deleted: {}", path.display())),
        )
        .chain(
            untracked
                .iter()
                .map(|path| format!("untracked: {}", path.display())),
        )
        .collect()
}
