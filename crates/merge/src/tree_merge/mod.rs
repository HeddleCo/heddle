// SPDX-License-Identifier: Apache-2.0
//! Tree-level three-way merge with rename detection.

mod engine;
mod executor;
mod rename_matcher;
mod renames;

use std::{error::Error, fmt, path::Path};

use anyhow::{Result, anyhow};
use objects::{
    object::{ContentHash, Tree},
    store::ObjectStore,
};
pub use rename_matcher::RenameMatcherStats;

use crate::{ConflictMarkers, MergeOutcome};

/// Optional semantic content merge hook used when [`MergeStrategy::Semantic`]
/// is selected.
pub type SemanticMergeFn =
    for<'a> fn(&[u8], &[u8], &[u8], &Path, ConflictMarkers<'a>) -> MergeOutcome;

/// Optional semantic similarity hook used by rename detection.
pub type SemanticSimilarityFn = fn(&str, &str, &[u8], &[u8]) -> f64;

/// Source for blob contents used by tree merge content resolution.
pub trait MergeBlobSource {
    fn load_blob(&self, hash: &ContentHash, path: &str) -> Result<Vec<u8>>;
}

impl<T> MergeBlobSource for &T
where
    T: ObjectStore + ?Sized,
{
    fn load_blob(&self, hash: &ContentHash, path: &str) -> Result<Vec<u8>> {
        let blob = self.get_blob(hash)?.ok_or_else(|| {
            anyhow!(MergeError::repository_integrity_refusal(
                format!(
                    "merge input blob {hash} for path {path:?} is missing from the object store; \
                     aborting to avoid silently merging against empty content"
                ),
                format!("merge input path {path:?} references missing blob {hash} in the object store"),
                "the merge would use empty bytes for the missing blob and could choose the other side cleanly, committing silent content loss without conflict markers",
                "HEAD, refs, and worktree were left unchanged; any merge scratch objects written before this refusal are unreachable until a successful capture",
            ))
        })?;
        Ok(blob.content().to_vec())
    }
}

/// Typed merge-engine failures that callers can map to their own UX.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeError {
    RepositoryIntegrity {
        error: String,
        unsafe_condition: String,
        would_change: String,
        preserved: String,
    },
}

impl MergeError {
    pub fn repository_integrity_refusal(
        error: impl Into<String>,
        unsafe_condition: impl Into<String>,
        would_change: impl Into<String>,
        preserved: impl Into<String>,
    ) -> Self {
        Self::RepositoryIntegrity {
            error: error.into(),
            unsafe_condition: unsafe_condition.into(),
            would_change: would_change.into(),
            preserved: preserved.into(),
        }
    }
}

impl fmt::Display for MergeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RepositoryIntegrity {
                error,
                unsafe_condition,
                would_change,
                preserved,
            } => write!(
                formatter,
                "{error}\nUnsafe condition: {unsafe_condition}\nWould change: {would_change}\nPreserved: {preserved}\nNext: heddle fsck --full"
            ),
        }
    }
}

impl Error for MergeError {}

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
    blob_source: &impl MergeBlobSource,
    base_tree: &Tree,
    our_tree: &Tree,
    their_tree: &Tree,
    options: MergeOptions<'_>,
) -> Result<TreeMergeResult> {
    engine::merge_trees(store, blob_source, base_tree, our_tree, their_tree, options)
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
    directory_renames
        .sort_by(|left, right| left.from.cmp(&right.from).then(left.to.cmp(&right.to)));

    Ok(RenameDetectionResult {
        renames,
        directory_renames,
        stats: detection.stats,
    })
}

pub(crate) use rename_matcher::RenameMatch;
