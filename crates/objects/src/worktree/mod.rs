// SPDX-License-Identifier: Apache-2.0
//! Working directory management.

mod source_line_map;
mod worktree_compare;
mod worktree_diff;
pub mod worktree_ignore;
mod worktree_reserved;
mod worktree_types;

#[cfg(test)]
mod worktree_tests;

pub use source_line_map::{SourceLineMapBuild, source_line_edit_map};
pub use worktree_compare::compare_worktree;
pub use worktree_diff::{DiffLine, diff_blobs};
pub use worktree_ignore::{
    WorktreeIgnoreMatcher, build_matcher, build_worktree_ignore, should_ignore,
};
pub use worktree_reserved::{is_reserved_directory_child, is_reserved_worktree_path};
pub use worktree_types::{FileStatus, WorktreeChange, WorktreeStatus};
