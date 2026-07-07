// SPDX-License-Identifier: Apache-2.0
//! Merge-specific rename orchestration built on shared matcher primitives.

use std::collections::HashMap;

use anyhow::Result;
use objects::{object::Tree, store::ObjectStore};

use super::{
    SemanticSimilarityFn,
    rename_matcher::{
        DEFAULT_THRESHOLD, RenameMatch, RenameMatcherConfig, detect_renames, flatten_tree,
    },
};

#[cfg(test)]
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
    semantic_similarity: Option<SemanticSimilarityFn>,
) -> Result<MergeRenameMap> {
    let config = RenameMatcherConfig {
        threshold,
        semantic_similarity,
    };
    let base_flat = flatten_tree(store, base_tree, "")?;
    let our_flat = flatten_tree(store, our_tree, "")?;
    let their_flat = flatten_tree(store, their_tree, "")?;

    Ok(MergeRenameMap {
        our_renames: detect_renames(store, &base_flat, &our_flat, config)?,
        their_renames: detect_renames(store, &base_flat, &their_flat, config)?,
    })
}

pub(super) const DEFAULT_RENAME_THRESHOLD: f64 = DEFAULT_THRESHOLD;
