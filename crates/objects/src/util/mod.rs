// SPDX-License-Identifier: Apache-2.0
//! Shared utilities used across the objects crate.

pub mod git_tree_name;
pub mod git_submodule;
pub mod symlink;

pub use git_submodule::{SUBMODULE_PREFIX, gitlink_blob_content};
pub use git_tree_name::{
    GitTreeNameClassification, GitTreeNameLossy, GitTreeNameLossyAction, classify_git_tree_name,
};
pub use symlink::symlink_target_bytes;
