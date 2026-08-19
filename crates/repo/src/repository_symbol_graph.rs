// SPDX-License-Identifier: Apache-2.0
//! Cross-file resolution and state-scoped binding-delta persistence.

#![cfg(feature = "tree-sitter-symbols")]

use std::collections::{BTreeSet, HashMap};

use objects::{
    object::{
        BindingDelta, ContentHash, FileBindingDelta, ReverseDependencyIndex, SemanticIndexRoot,
    },
    store::ObjectStore,
};
use semantic::cross_file_resolution::{RESOLVER_VERSION, resolve_paths};

use crate::{
    HeddleError, Repository, Result,
    repository_symbol_graph_frontier::{invalidation_frontier, patch_importer_index},
};

type PendingSymbolGraphBlobs = Vec<(ContentHash, Vec<u8>)>;
type DeferredSymbolGraph = (ContentHash, PendingSymbolGraphBlobs);

/// Outcome of binding one semantic root, including how many files re-resolved.
pub(crate) struct SemanticGraphBind {
    pub root_hash: ContentHash,
    pub pending: PendingSymbolGraphBlobs,
    pub resolve_count: usize,
    pub frontier: BTreeSet<String>,
}

impl Repository {
    /// Resolve the semantic root and persist a delta over the first parent's
    /// edge set, returning a replacement semantic-root blob hash.
    pub(crate) fn persist_resolved_semantic_edges(
        &self,
        parent_state: Option<&objects::object::State>,
        root: SemanticIndexRoot,
    ) -> Result<ContentHash> {
        let bound =
            self.persist_resolved_semantic_edges_deferred(parent_state, root, &HashMap::new())?;
        self.store().put_blobs_packed(bound.1)?;
        Ok(bound.0)
    }

    /// Resolve semantic edges while leaving every newly-authored blob in
    /// memory for a caller-owned durability transaction.
    pub(crate) fn persist_resolved_semantic_edges_deferred(
        &self,
        parent_state: Option<&objects::object::State>,
        root: SemanticIndexRoot,
        pending_nodes: &HashMap<ContentHash, Vec<u8>>,
    ) -> Result<DeferredSymbolGraph> {
        let bound = self.bind_semantic_graph(parent_state, root, pending_nodes)?;
        Ok((bound.root_hash, bound.pending))
    }

    pub(crate) fn bind_semantic_graph(
        &self,
        parent_state: Option<&objects::object::State>,
        root: SemanticIndexRoot,
        pending_nodes: &HashMap<ContentHash, Vec<u8>>,
    ) -> Result<SemanticGraphBind> {
        let parent_root = parent_state
            .map(|state| self.attached_semantic_index(&state.id()))
            .transpose()?
            .flatten();
        let parent_root = parent_root.filter(|root| {
            root.resolver_version == RESOLVER_VERSION
                && root.binding_delta.is_some()
                && root.importer_index.is_some()
        });
        let parent_delta = parent_root.as_ref().and_then(|root| root.binding_delta);
        let parent_index = match parent_root.as_ref().and_then(|root| root.importer_index) {
            Some(hash) => Some(self.load_reverse_dependency_index(&hash)?),
            None => None,
        };

        if let (Some(parent_root), Some(parent_delta), Some(parent_index_hash)) = (
            &parent_root,
            parent_delta,
            parent_root.as_ref().and_then(|root| root.importer_index),
        ) {
            let changed =
                self.changed_semantic_file_paths(parent_root.tree, root.tree, pending_nodes)?;
            if changed.is_empty() {
                return self.finish_bind(
                    root,
                    Some(parent_delta),
                    Vec::new(),
                    Some(parent_index_hash),
                    None,
                );
            }
            return self.bind_frontier(
                root,
                Some(parent_delta),
                parent_index.as_ref(),
                changed,
                pending_nodes,
            );
        }

        self.bind_frontier(
            root,
            parent_delta,
            parent_index.as_ref(),
            BTreeSet::new(),
            pending_nodes,
        )
    }

    fn bind_frontier(
        &self,
        root: SemanticIndexRoot,
        parent_delta: Option<ContentHash>,
        parent_index: Option<&ReverseDependencyIndex>,
        changed: BTreeSet<String>,
        pending_nodes: &HashMap<ContentHash, Vec<u8>>,
    ) -> Result<SemanticGraphBind> {
        let current_files = self.semantic_files_with_pending(&root, pending_nodes)?;
        let frontier = match parent_index {
            Some(index) if !changed.is_empty() || parent_delta.is_some() => {
                invalidation_frontier(&changed, index)
            }
            _ => current_files.keys().cloned().collect(),
        };
        let resolutions = resolve_paths(&current_files, frontier.iter().map(String::as_str));
        let resolve_count = resolutions.len();
        let files = frontier
            .iter()
            .map(|path| match current_files.get(path) {
                Some(file) => FileBindingDelta::new(
                    path.clone(),
                    Some(file.node_hash),
                    resolutions
                        .get(path)
                        .map(|resolution| resolution.edges.clone())
                        .unwrap_or_default(),
                ),
                None => FileBindingDelta::new(path, None, Vec::new()),
            })
            .collect();
        let index = patch_importer_index(parent_index, &frontier, &resolutions, &current_files);
        let mut bound = self.finish_bind(root, parent_delta, files, None, Some(index))?;
        bound.resolve_count = resolve_count;
        bound.frontier = frontier;
        Ok(bound)
    }

    fn finish_bind(
        &self,
        root: SemanticIndexRoot,
        parent_delta: Option<ContentHash>,
        files: Vec<FileBindingDelta>,
        reuse_index: Option<ContentHash>,
        index: Option<ReverseDependencyIndex>,
    ) -> Result<SemanticGraphBind> {
        let delta = BindingDelta::new(parent_delta, files);
        let delta_bytes = delta.encode()?;
        let delta_hash = ContentHash::compute_typed("blob", &delta_bytes);
        let (index_hash, index_bytes) = match (reuse_index, index) {
            (Some(hash), _) => (hash, None),
            (None, Some(index)) => {
                let bytes = index.encode()?;
                (ContentHash::compute_typed("blob", &bytes), Some(bytes))
            }
            (None, None) => {
                return Err(HeddleError::InvalidObject(
                    "semantic graph bind missing reverse-dependency index".to_string(),
                ));
            }
        };
        let rooted = root
            .with_binding_delta(delta_hash, RESOLVER_VERSION)
            .with_importer_index(index_hash);
        let root_bytes = rooted.encode()?;
        let root_hash = ContentHash::compute_typed("blob", &root_bytes);
        let mut pending = vec![(delta_hash, delta_bytes), (root_hash, root_bytes)];
        if let Some(index_bytes) = index_bytes {
            pending.push((index_hash, index_bytes));
        }
        Ok(SemanticGraphBind {
            root_hash,
            pending,
            resolve_count: 0,
            frontier: BTreeSet::new(),
        })
    }
}
