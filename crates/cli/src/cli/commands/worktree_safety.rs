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
//! `heddle capture -m "..."` to capture work first.

use anyhow::{Result, anyhow};
use repo::{Repository, WorktreeStatusDetailed};

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
    let detailed: WorktreeStatusDetailed = repo.compare_worktree_cached_detailed(&tree)?;
    if detailed.is_clean() {
        return Ok(());
    }

    let mut reasons = Vec::new();
    if !detailed.modified.is_empty() {
        reasons.push(format!("{} modified file(s)", detailed.modified.len()));
    }
    if !detailed.deleted.is_empty() {
        reasons.push(format!("{} deleted file(s)", detailed.deleted.len()));
    }
    let untracked_count = detailed.untracked.flattened_path_count();
    if untracked_count > 0 {
        reasons.push(format!("{} untracked file(s)", untracked_count));
    }

    Err(anyhow!(
        "Refusing to {action}: worktree has uncommitted changes ({}). \
         {Action} would destroy them — no prior snapshot exists to restore from. \
         Capture them with `heddle capture -m \"...\"` (or remove them) and retry.",
        reasons.join(", "),
        action = action,
        Action = capitalize(action),
    ))
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}