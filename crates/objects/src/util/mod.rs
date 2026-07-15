// SPDX-License-Identifier: Apache-2.0
//! Shared utilities used across the objects crate.

pub mod git_tree_name;
pub mod gitlink;
pub mod line_diff;
pub mod symlink;

pub use git_tree_name::{
    GitTreeNameClassification, GitTreeNameLossy, GitTreeNameLossyAction, classify_git_tree_name,
};
pub use gitlink::gitlink_placeholder_bytes;
pub use line_diff::{lcs_line_matches, split_text_lines};
pub use symlink::symlink_target_bytes;
