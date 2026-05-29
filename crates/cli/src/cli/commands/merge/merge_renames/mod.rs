// SPDX-License-Identifier: Apache-2.0
//! Merge-specific rename orchestration built on shared matcher primitives.

use std::collections::HashMap;

use anyhow::Result;
use objects::{object::Tree, store::ObjectStore};

use super::rename_matcher::{
    DEFAULT_THRESHOLD, RenameMatch, RenameMatcherConfig, detect_renames, flatten_tree,
};

// Rename-matcher tests require AST-based semantic similarity to score
// modified-renames above the threshold. Without `--features semantic`
// `compute_semantic_similarity` short-circuits to 0 and the composite
// score never crosses 0.55, so several scenarios fail. Run them with
// `cargo test -p cli --features semantic` to exercise the full matrix.
#[cfg(all(test, feature = "semantic"))]
mod tests;

#[derive(Debug, Default)]
pub(super) struct MergeRenameMap {
    pub our_renames: HashMap<String, RenameMatch>,
    pub their_renames: HashMap<String, RenameMatch>,
}

pub(super) fn detect_merge_renames(
    store: &impl ObjectStore,
    base_tree: &Tree,
    our_tree: &Tree,
    their_tree: &Tree,
    threshold: f64,
) -> Result<MergeRenameMap> {
    let config = RenameMatcherConfig { threshold };
    let base_flat = flatten_tree(store, base_tree, "")?;
    let our_flat = flatten_tree(store, our_tree, "")?;
    let their_flat = flatten_tree(store, their_tree, "")?;

    Ok(MergeRenameMap {
        our_renames: detect_renames(store, &base_flat, &our_flat, config)?,
        their_renames: detect_renames(store, &base_flat, &their_flat, config)?,
    })
}

pub(super) const DEFAULT_RENAME_THRESHOLD: f64 = DEFAULT_THRESHOLD;
