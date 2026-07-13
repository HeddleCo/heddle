// SPDX-License-Identifier: Apache-2.0
//! Shared dirty-worktree guard for destructive worktree-mutation commands.
//!
//! Several Heddle commands (`undo`, `redo`, `cherry-pick`, `rebase`) materialize
//! a different tree into the worktree before HEAD advances. If the worktree
//! holds modified-but-unsnapshotted tracked content or untracked files, that
//! mutation will silently destroy them — the planner has no record they ever
//! existed, so there is no snapshot to recover from.
//!
//! This module provides a single guard that callers invoke before mutating the
//! worktree. It mirrors `git checkout`'s default of protecting the working copy
//! and produces a precise error message that points the user at
//! `heddle capture -m "..."` or
//! `heddle capture -m "..."` to preserve work first.

use anyhow::{Result, anyhow};
use repo::{Repository, WorktreeStatusDetailed};

use super::advice::RecoveryAdvice;
use crate::cli::worktree_status_options;

/// Refuse to perform a destructive worktree apply when uncommitted changes
/// exist.
///
/// `action` is the imperative verb shown to the user (e.g. "undo", "redo",
/// "cherry-pick", "rebase"). It is interpolated into the error message.
///
/// Returns `Ok(())` when:
/// - The repository has no HEAD (nothing to compare against).
/// - The HEAD state's tree is missing (degraded but not actionable here).
/// - The worktree matches HEAD exactly.
///
/// Returns `Err` with a precise list of dirty paths otherwise.
pub(crate) fn ensure_worktree_clean(repo: &Repository, action: &str) -> Result<()> {
    let Some(head) = repo.head()? else {
        return Ok(());
    };
    let Some(tree) = repo.get_tree_for_state(&head)? else {
        return Ok(());
    };
    let detailed: WorktreeStatusDetailed = repo.compare_worktree_cached_detailed_with_options(
        &tree,
        &worktree_status_options(Some(repo.config())),
    )?;
    if detailed.is_clean() {
        return Ok(());
    }

    Err(anyhow!(dirty_worktree_advice(
        action,
        &detailed,
        "repository state and worktree files were left unchanged; no snapshot has been written for these paths",
    )))
}

pub(crate) fn dirty_worktree_advice(
    action: &str,
    detailed: &WorktreeStatusDetailed,
    already_preserved: impl Into<String>,
) -> RecoveryAdvice {
    RecoveryAdvice::dirty_worktree(action, dirty_paths(detailed), already_preserved)
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
