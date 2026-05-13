// SPDX-License-Identifier: Apache-2.0
//! Working directory management.

mod worktree_compare;
mod worktree_diff;
pub mod worktree_ignore;
mod worktree_types;

#[cfg(test)]
mod worktree_tests;

pub use worktree_compare::compare_worktree;
pub use worktree_diff::{DiffLine, diff_blobs};
pub use worktree_ignore::should_ignore;
pub use worktree_types::{FileStatus, WorktreeChange, WorktreeStatus};