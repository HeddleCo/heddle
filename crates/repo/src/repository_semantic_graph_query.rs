// SPDX-License-Identifier: Apache-2.0
//! Always-compiled, tree-sitter-free graph query primitives (heddle#1276).
//!
//! `refs_of` / `callers_of` / `importers_of` walk the attached semantic index
//! and the persisted importer index. None of them parse source or rebuild a
//! graph. Salsa-style memoization is deferred; repeat queries short-circuit
//! on the same content-addressed blobs.

use objects::object::{
    ReverseDependencyIndex, SemanticEdgeKind, SemanticGraphRef, StateId, SymbolAnchor,
};

use crate::{HeddleError, Repository, Result};

impl Repository {
    /// Load a state's attached file → importers index. Missing attachment or
    /// missing index blob is ABSENT (`Ok(None)`); this never recomputes.
    pub fn attached_reverse_dependency_index(
        &self,
        state_id: &StateId,
    ) -> Result<Option<ReverseDependencyIndex>> {
        let Some(root) = self.attached_semantic_index(state_id)? else {
            return Ok(None);
        };
        let Some(hash) = root.importer_index else {
            return Ok(None);
        };
        match self.load_reverse_dependency_index(&hash) {
            Ok(index) => Ok(Some(index)),
            Err(HeddleError::NotFound(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Files that import `path` at `state_id`, from the persisted importer index.
    pub fn importers_of(&self, state_id: &StateId, path: &str) -> Result<Option<Vec<String>>> {
        Ok(self
            .attached_reverse_dependency_index(state_id)?
            .map(|index| index.importers_of(path).to_vec()))
    }

    /// Occurrences that resolve to `anchor` at `state_id`.
    pub fn refs_of(
        &self,
        state_id: &StateId,
        anchor: &SymbolAnchor,
    ) -> Result<Option<Vec<SemanticGraphRef>>> {
        self.graph_refs(state_id, anchor, None)
    }

    /// Call-edge subset of [`Self::refs_of`].
    pub fn callers_of(
        &self,
        state_id: &StateId,
        anchor: &SymbolAnchor,
    ) -> Result<Option<Vec<SemanticGraphRef>>> {
        self.graph_refs(state_id, anchor, Some(SemanticEdgeKind::Calls))
    }

    fn graph_refs(
        &self,
        state_id: &StateId,
        anchor: &SymbolAnchor,
        kind: Option<SemanticEdgeKind>,
    ) -> Result<Option<Vec<SemanticGraphRef>>> {
        let Some(root) = self.attached_semantic_index(state_id)? else {
            return Ok(None);
        };
        let Some(edges) = root
            .binding_delta
            .map(|hash| self.materialize_binding_delta(&hash))
            .transpose()?
        else {
            return Ok(Some(Vec::new()));
        };
        let Some(target_definition) = self.definition_index(state_id, anchor)? else {
            return Ok(Some(Vec::new()));
        };
        let mut refs = Vec::new();
        for (source_path, file_edges) in edges {
            for edge in file_edges {
                if edge.target_path != anchor.file || edge.target_definition != target_definition {
                    continue;
                }
                if kind.is_some_and(|want| edge.kind != want) {
                    continue;
                }
                refs.push(self.graph_ref(state_id, &source_path, &edge, anchor)?);
            }
        }
        refs.sort_by(|left, right| {
            (
                &left.source_path,
                left.source_occurrence,
                left.kind,
                &left.name,
            )
                .cmp(&(
                    &right.source_path,
                    right.source_occurrence,
                    right.kind,
                    &right.name,
                ))
        });
        Ok(Some(refs))
    }

    fn definition_index(&self, state_id: &StateId, anchor: &SymbolAnchor) -> Result<Option<u32>> {
        let Some(file) = self.semantic_file_node(state_id, &anchor.file)? else {
            return Ok(None);
        };
        Ok(file
            .symbols
            .iter()
            .position(|symbol| symbol.address() == anchor.symbol)
            .map(|index| index as u32))
    }

    fn graph_ref(
        &self,
        state_id: &StateId,
        source_path: &str,
        edge: &objects::object::ResolvedSemanticEdge,
        target: &SymbolAnchor,
    ) -> Result<SemanticGraphRef> {
        let occurrence = self
            .semantic_file_node(state_id, source_path)?
            .and_then(|file| {
                file.occurrences
                    .into_iter()
                    .find(|occurrence| occurrence.local_id == edge.source_occurrence)
            });
        Ok(SemanticGraphRef {
            source_path: source_path.to_string(),
            source_occurrence: edge.source_occurrence,
            name: occurrence
                .as_ref()
                .map(|occurrence| occurrence.name.clone())
                .unwrap_or_default(),
            role: occurrence.as_ref().map(|occurrence| occurrence.role),
            kind: edge.kind,
            span: occurrence.as_ref().map(|occurrence| occurrence.span),
            target: target.clone(),
            target_definition: edge.target_definition,
        })
    }
}
