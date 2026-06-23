// SPDX-License-Identifier: Apache-2.0
//! Rename detection and merge rename planning.
//!
//! The matcher combines exact object identity, path metadata, byte-delta
//! similarity, and an optional caller-provided semantic similarity scorer.
//! By default no semantic score is used, keeping this crate independent of
//! parser-heavy semantic analysis.

use std::collections::HashMap;

use anyhow::Result;
use objects::{object::ContentHash, object::Tree, store::ObjectStore};

mod candidates;
mod matcher;
mod scoring;

#[cfg(test)]
mod tests;

pub use matcher::{detect_renames, detect_renames_with_stats, flatten_tree};
pub use scoring::infer_directory_renames;

/// Default rename similarity threshold.
const DEFAULT_THRESHOLD: f64 = 0.55;

type SemanticScorer = fn(&str, &str, &[u8], &[u8]) -> f64;

/// A detected file rename.
#[derive(Debug, Clone)]
pub struct RenameMatch {
    /// Path of the file in the base tree.
    pub from_path: String,
    /// Path of the file in the branch tree.
    pub to_path: String,
    /// Combined similarity score in the range `0.0..=1.0`.
    pub score: f64,
    /// Content hash of the base-side file.
    pub from_hash: ContentHash,
    /// Content hash of the branch-side file.
    pub to_hash: ContentHash,
}

/// Counters emitted by rename detection for diagnostics and benchmarks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenameMatcherStats {
    /// Files removed from the base tree.
    pub deleted_files: usize,
    /// Files added in the branch tree.
    pub added_files: usize,
    /// Rename pairs matched by identical content hash.
    pub exact_hash_matches: usize,
    /// Cartesian size before metadata pruning.
    pub total_possible_pairs: usize,
    /// Candidate pairs retained by metadata pruning.
    pub metadata_candidate_pairs: usize,
    /// Candidate pairs that received a final score.
    pub scored_pairs: usize,
    /// Candidate pairs that invoked the optional semantic scorer.
    pub semantic_scored_pairs: usize,
    /// Candidate pairs that skipped semantic scoring due to high delta score.
    pub high_confidence_delta_pairs: usize,
    /// Candidate pairs whose score crossed the configured threshold.
    pub threshold_matches: usize,
    /// Greedily selected rename pairs.
    pub matched_pairs: usize,
    /// Blob loads performed while preparing candidates.
    pub blob_loads: usize,
    /// Total blob bytes loaded while preparing candidates.
    pub blob_bytes_loaded: usize,
    /// Whether content scoring was enabled for this run.
    pub used_content: bool,
}

/// Rename detection output plus diagnostic counters.
#[derive(Debug)]
pub struct RenameDetection {
    /// Matched renames keyed by base path.
    pub matches: HashMap<String, RenameMatch>,
    /// Diagnostic counters for the matcher run.
    pub stats: RenameMatcherStats,
}

/// Configuration for rename detection.
#[derive(Clone, Copy, Debug)]
pub struct RenameMatcherConfig {
    /// Minimum score required for a candidate pair to be accepted.
    pub threshold: f64,
    semantic_scorer: Option<SemanticScorer>,
}

impl RenameMatcherConfig {
    /// Create a config with a custom threshold and no semantic scorer.
    pub fn new(threshold: f64) -> Self {
        Self {
            threshold,
            ..Self::default()
        }
    }

    /// Return a config that calls `scorer` for eligible content pairs.
    ///
    /// The scorer should return a value in `0.0..=1.0`; non-finite or
    /// out-of-range values are treated conservatively by clamping to that
    /// range, with non-finite scores becoming `0.0`.
    pub fn with_semantic_scorer(mut self, scorer: SemanticScorer) -> Self {
        self.semantic_scorer = Some(scorer);
        self
    }

    pub(crate) fn semantic_score(
        &self,
        from_path: &str,
        to_path: &str,
        from_content: &[u8],
        to_content: &[u8],
    ) -> f64 {
        let Some(scorer) = self.semantic_scorer else {
            return 0.0;
        };
        let score = scorer(from_path, to_path, from_content, to_content);
        if score.is_finite() {
            score.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

impl Default for RenameMatcherConfig {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            semantic_scorer: None,
        }
    }
}

/// Bidirectional rename plan for a three-way merge.
#[derive(Debug, Default)]
pub struct MergeRenameMap {
    /// Renames detected between base and our tree.
    pub our_renames: HashMap<String, RenameMatch>,
    /// Renames detected between base and their tree.
    pub their_renames: HashMap<String, RenameMatch>,
}

/// Detect renames on both sides of a three-way merge.
pub fn detect_merge_renames(
    store: &impl ObjectStore,
    base_tree: &Tree,
    our_tree: &Tree,
    their_tree: &Tree,
    config: RenameMatcherConfig,
) -> Result<MergeRenameMap> {
    let base_flat = flatten_tree(store, base_tree, "")?;
    let our_flat = flatten_tree(store, our_tree, "")?;
    let their_flat = flatten_tree(store, their_tree, "")?;

    Ok(MergeRenameMap {
        our_renames: detect_renames(store, &base_flat, &our_flat, config)?,
        their_renames: detect_renames(store, &base_flat, &their_flat, config)?,
    })
}
