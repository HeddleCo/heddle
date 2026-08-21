// SPDX-License-Identifier: Apache-2.0
//! Shared utilities used across the objects crate.

pub mod budget;
pub mod git_tree_name;
pub mod gitlink;
pub mod line_diff;
pub mod symlink;

pub use budget::{BudgetExceeded, ResourceBudget, ResourceKind, ResourceUsage};
pub use git_tree_name::{
    GitTreeNameClassification, GitTreeNameLossy, GitTreeNameLossyAction, classify_git_tree_name,
};
pub use gitlink::gitlink_placeholder_bytes;
pub use line_diff::{
    EqualRun, LineDiffError, LineDiffLimits, scratch_bytes_for_line_counts, split_text_lines,
    visit_lcs_equal_runs,
};
pub use symlink::symlink_target_bytes;
