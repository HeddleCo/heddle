// SPDX-License-Identifier: Apache-2.0
//! Tree-level three-way merge with rename detection.

mod engine;
mod executor;
mod rename_matcher;
mod renames;

use std::path::Path;

use anyhow::Result;
use objects::{object::Tree, store::ObjectStore};

use crate::{ConflictMarkers, MergeOutcome};

pub use rename_matcher::RenameMatcherStats;

/// Optional semantic content merge hook used when [`MergeStrategy::Semantic`]
/// is selected.
pub type SemanticMergeFn =
    for<'a> fn(&[u8], &[u8], &[u8], &Path, ConflictMarkers<'a>) -> MergeOutcome;

/// Optional semantic similarity hook used by rename detection.
pub type SemanticSimilarityFn = fn(&str, &str, &[u8], &[u8]) -> f64;

/// A file rename detected during tree merge planning.
#[derive(Clone, Debug, PartialEq)]
pub struct DetectedRename {
    pub from: String,
    pub to: String,
    pub score: f64,
}

/// A directory rename inferred from file rename evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryRename {
    pub from: String,
    pub to: String,
}

/// Result of a tree-level three-way merge.
pub struct TreeMergeResult {
    pub tree: Tree,
    pub conflicts: Vec<String>,
    pub renames: Vec<DetectedRename>,
    pub directory_renames: Vec<DirectoryRename>,
}

/// Labels and strategy used when writing conflict markers.
#[derive(Clone, Copy, Debug)]
pub struct ConflictLabels<'a> {
    pub current: &'a str,
    pub incoming: &'a str,
    pub strategy: MergeStrategy,
}

/// Content-merge strategy used by the tree merge engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeStrategy {
    HunkOnly,
    Semantic,
}

impl ConflictLabels<'_> {
    pub const DEFAULT: ConflictLabels<'static> = ConflictLabels {
        current: "CURRENT",
        incoming: "INCOMING",
        strategy: MergeStrategy::HunkOnly,
    };
}

/// Rename detection knobs for tree merging.
#[derive(Clone, Copy, Debug)]
pub struct RenameOptions {
    pub threshold: f64,
    pub semantic_similarity: Option<SemanticSimilarityFn>,
}

impl Default for RenameOptions {
    fn default() -> Self {
        Self {
            threshold: renames::DEFAULT_RENAME_THRESHOLD,
            semantic_similarity: None,
        }
    }
}

/// Options for a tree-level merge.
#[derive(Clone, Copy, Debug)]
pub struct MergeOptions<'a> {
    pub labels: ConflictLabels<'a>,
    pub rename_options: RenameOptions,
    pub semantic_merge: Option<SemanticMergeFn>,
}

impl Default for MergeOptions<'static> {
    fn default() -> Self {
        Self {
            labels: ConflictLabels::DEFAULT,
            rename_options: RenameOptions::default(),
            semantic_merge: None,
        }
    }
}

/// Result of comparing two trees for renames.
pub struct RenameDetectionResult {
    pub renames: Vec<DetectedRename>,
    pub directory_renames: Vec<DirectoryRename>,
    pub stats: RenameMatcherStats,
}

/// Merge three trees using the supplied object store.
pub fn merge_trees(
    store: &impl ObjectStore,
    base_tree: &Tree,
    our_tree: &Tree,
    their_tree: &Tree,
    options: MergeOptions<'_>,
) -> Result<TreeMergeResult> {
    engine::merge_trees(store, base_tree, our_tree, their_tree, options)
}

/// Detect file and directory renames between two trees.
pub fn detect_renames_between_trees(
    store: &impl ObjectStore,
    from_tree: &Tree,
    to_tree: &Tree,
    options: RenameOptions,
) -> Result<RenameDetectionResult> {
    let from_flat = rename_matcher::flatten_tree(store, from_tree, "")?;
    let to_flat = rename_matcher::flatten_tree(store, to_tree, "")?;
    let detection = rename_matcher::detect_renames_with_stats(
        store,
        &from_flat,
        &to_flat,
        rename_matcher::RenameMatcherConfig {
            threshold: options.threshold,
            semantic_similarity: options.semantic_similarity,
        },
    )?;
    let mut renames: Vec<DetectedRename> = detection
        .matches
        .values()
        .map(|rename| DetectedRename {
            from: rename.from_path.clone(),
            to: rename.to_path.clone(),
            score: rename.score,
        })
        .collect();
    renames.sort_by(|left, right| left.from.cmp(&right.from).then(left.to.cmp(&right.to)));

    let mut directory_renames: Vec<DirectoryRename> =
        rename_matcher::infer_directory_renames(&detection.matches)
            .into_iter()
            .map(|(from, to)| DirectoryRename { from, to })
            .collect();
    directory_renames.sort_by(|left, right| left.from.cmp(&right.from).then(left.to.cmp(&right.to)));

    Ok(RenameDetectionResult {
        renames,
        directory_renames,
        stats: detection.stats,
    })
}

pub(crate) use rename_matcher::RenameMatch;
