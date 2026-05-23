// SPDX-License-Identifier: Apache-2.0
//! Worktree status types.

use std::path::PathBuf;

/// Status of a file in the worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    /// File is unchanged.
    Unchanged,
    /// File has been modified.
    Modified,
    /// File has been added (not in state).
    Added,
    /// File has been deleted (in state but not in worktree).
    Deleted,
}

/// A change in the worktree.
#[derive(Debug, Clone)]
pub struct WorktreeChange {
    /// Path relative to repository root.
    pub path: PathBuf,
    /// Status of the file.
    pub status: FileStatus,
}

/// Worktree status summary.
#[derive(Debug, Default)]
pub struct WorktreeStatus {
    /// Files that have been modified.
    pub modified: Vec<PathBuf>,
    /// Files that have been added.
    pub added: Vec<PathBuf>,
    /// Files that have been deleted.
    pub deleted: Vec<PathBuf>,
}

impl WorktreeStatus {
    /// Check if the worktree is clean (no changes).
    pub fn is_clean(&self) -> bool {
        self.modified.is_empty() && self.added.is_empty() && self.deleted.is_empty()
    }

    /// Get total number of changes.
    pub fn change_count(&self) -> usize {
        self.modified.len() + self.added.len() + self.deleted.len()
    }
}
