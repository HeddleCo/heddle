// SPDX-License-Identifier: Apache-2.0
//! Parse-free reconstruction and lookup for persisted semantic edge deltas.

use std::collections::{BTreeMap, HashSet};

#[cfg(feature = "tree-sitter-symbols")]
use objects::object::ReverseDependencyIndex;
use objects::{
    object::{BindingDelta, ContentHash, ResolvedSemanticEdge, StateId},
    store::ObjectStore,
};

use crate::{HeddleError, Repository, Result};

/// Fully reconstructed source-file → resolved-edge set for one state.
pub type ResolvedSemanticEdgeSet = BTreeMap<String, Vec<ResolvedSemanticEdge>>;

const MAX_BINDING_DELTA_DEPTH: usize = 65_536;

impl Repository {
    /// Load the edge delta directly attached through a state's semantic root.
    pub fn semantic_edge_delta(&self, state_id: &StateId) -> Result<Option<BindingDelta>> {
        let Some(root) = self.attached_semantic_index(state_id)? else {
            return Ok(None);
        };
        root.binding_delta
            .map(|hash| self.load_binding_delta(&hash))
            .transpose()
    }

    /// Reconstruct the complete resolved edge set by overlaying parent deltas.
    pub fn resolved_semantic_edges(
        &self,
        state_id: &StateId,
    ) -> Result<Option<ResolvedSemanticEdgeSet>> {
        let Some(root) = self.attached_semantic_index(state_id)? else {
            return Ok(None);
        };
        root.binding_delta
            .map(|hash| self.materialize_binding_delta(&hash))
            .transpose()
    }

    /// Resolve one source-local occurrence from the persisted graph.
    pub fn resolved_semantic_occurrence(
        &self,
        state_id: &StateId,
        source_path: &str,
        source_occurrence: u32,
    ) -> Result<Option<ResolvedSemanticEdge>> {
        Ok(self
            .resolved_semantic_edges(state_id)?
            .and_then(|edges| edges.get(source_path).cloned())
            .and_then(|edges| {
                edges
                    .into_iter()
                    .find(|edge| edge.source_occurrence == source_occurrence)
            }))
    }

    #[cfg(feature = "tree-sitter-symbols")]
    pub(crate) fn load_reverse_dependency_index(
        &self,
        hash: &ContentHash,
    ) -> Result<ReverseDependencyIndex> {
        let blob = self.store().get_blob(hash)?.ok_or_else(|| {
            HeddleError::NotFound(format!("semantic reverse-dependency index {hash}"))
        })?;
        ReverseDependencyIndex::decode(blob.content())
            .map_err(|err| HeddleError::InvalidObject(err.to_string()))
    }

    pub(crate) fn load_binding_delta(&self, hash: &ContentHash) -> Result<BindingDelta> {
        let blob = self
            .store()
            .get_blob(hash)?
            .ok_or_else(|| HeddleError::NotFound(format!("semantic binding delta {hash}")))?;
        BindingDelta::decode(blob.content())
            .map_err(|err| HeddleError::InvalidObject(err.to_string()))
    }

    pub(crate) fn materialize_binding_delta(
        &self,
        head: &ContentHash,
    ) -> Result<ResolvedSemanticEdgeSet> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut cursor = Some(*head);
        while let Some(hash) = cursor {
            if chain.len() >= MAX_BINDING_DELTA_DEPTH {
                return Err(HeddleError::InvalidObject(format!(
                    "semantic binding delta chain exceeds max depth {MAX_BINDING_DELTA_DEPTH}"
                )));
            }
            if !seen.insert(hash) {
                return Err(HeddleError::InvalidObject(
                    "semantic binding delta chain contains a cycle".to_string(),
                ));
            }
            let delta = self.load_binding_delta(&hash)?;
            cursor = delta.parent;
            chain.push(delta);
        }

        let mut edges = BTreeMap::new();
        for delta in chain.into_iter().rev() {
            for file in delta.files {
                if file.file_node.is_some() {
                    edges.insert(file.path, file.replace_edges);
                } else {
                    edges.remove(&file.path);
                }
            }
        }
        Ok(edges)
    }
}
