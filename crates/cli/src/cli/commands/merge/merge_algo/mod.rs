// SPDX-License-Identifier: Apache-2.0
//! Merge algorithms: ancestor finding, three-way merge, tree application.

mod apply;
mod engine;
mod executor;

use objects::object::Tree;

/// Result of a three-way merge, including rename information.
pub(crate) struct MergeResult {
    pub tree: Tree,
    pub conflicts: Vec<String>,
    /// Detected file renames: (from_path, to_path, similarity score).
    pub renames: Vec<(String, String, f64)>,
    /// Detected directory renames: (from_dir, to_dir).
    pub directory_renames: Vec<(String, String)>,
}

#[derive(Clone, Copy)]
pub(crate) struct ConflictLabels<'a> {
    pub current: &'a str,
    pub incoming: &'a str,
}

impl ConflictLabels<'_> {
    pub(crate) const DEFAULT: ConflictLabels<'static> = ConflictLabels {
        current: "CURRENT",
        incoming: "INCOMING",
    };
}

pub(crate) use apply::apply_merged_tree;
pub(crate) use engine::{three_way_merge, three_way_merge_with_labels};