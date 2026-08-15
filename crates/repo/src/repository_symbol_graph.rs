// SPDX-License-Identifier: Apache-2.0
//! Cross-file resolution and state-scoped binding-delta persistence.

#![cfg(feature = "tree-sitter-symbols")]

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use objects::{
    object::{
        BindingDelta, ContentHash, FileBindingDelta, SemanticEntryKind, SemanticFileNode,
        SemanticIndexRoot, SemanticTreeEntry, SemanticTreeNode,
    },
    store::ObjectStore,
};
use semantic::cross_file_resolution::{
    RESOLVER_VERSION, RepositorySemanticFile, resolve_repository,
};

use crate::{HeddleError, Repository, Result};

type PendingSymbolGraphBlobs = Vec<(ContentHash, Vec<u8>)>;
type DeferredSymbolGraph = (ContentHash, PendingSymbolGraphBlobs);

impl Repository {
    /// Resolve the semantic root and persist a delta over the first parent's
    /// edge set, returning a replacement semantic-root blob hash.
    pub(crate) fn persist_resolved_semantic_edges(
        &self,
        parent_state: Option<&objects::object::State>,
        root: SemanticIndexRoot,
    ) -> Result<ContentHash> {
        let (root_hash, pending) =
            self.persist_resolved_semantic_edges_deferred(parent_state, root, &HashMap::new())?;
        self.store().put_blobs_packed(pending)?;
        Ok(root_hash)
    }

    /// Resolve semantic edges while leaving every newly-authored blob in
    /// memory for a caller-owned durability transaction.
    pub(crate) fn persist_resolved_semantic_edges_deferred(
        &self,
        parent_state: Option<&objects::object::State>,
        root: SemanticIndexRoot,
        pending_nodes: &HashMap<ContentHash, Vec<u8>>,
    ) -> Result<DeferredSymbolGraph> {
        let parent_root = parent_state
            .map(|state| self.attached_semantic_index(&state.id()))
            .transpose()?
            .flatten();
        let parent_delta = parent_root
            .as_ref()
            .filter(|root| root.resolver_version == RESOLVER_VERSION)
            .and_then(|root| root.binding_delta);

        if let (Some(parent_root), Some(parent_delta)) = (&parent_root, parent_delta)
            && !self.semantic_file_nodes_changed(parent_root.tree, root.tree, pending_nodes)?
        {
            return self.prepare_binding_delta(root, Some(parent_delta), Vec::new());
        }

        let current_files = self.semantic_files_with_pending(&root, pending_nodes)?;
        let current_resolution = resolve_repository(&current_files);

        let frontier = if parent_delta.is_some() {
            let parent_files = self.semantic_files(parent_root.as_ref().expect("checked above"))?;
            let parent_resolution = resolve_repository(&parent_files);
            invalidation_frontier(
                &parent_files,
                &current_files,
                &parent_resolution,
                &current_resolution,
            )
        } else {
            current_files.keys().cloned().collect()
        };

        let files = frontier
            .into_iter()
            .map(|path| match current_files.get(&path) {
                Some(file) => FileBindingDelta::new(
                    path.clone(),
                    Some(file.node_hash),
                    current_resolution
                        .get(&path)
                        .map(|resolution| resolution.edges.clone())
                        .unwrap_or_default(),
                ),
                None => FileBindingDelta::new(path, None, Vec::new()),
            })
            .collect();
        self.prepare_binding_delta(root, parent_delta, files)
    }

    fn prepare_binding_delta(
        &self,
        root: SemanticIndexRoot,
        parent_delta: Option<ContentHash>,
        files: Vec<FileBindingDelta>,
    ) -> Result<DeferredSymbolGraph> {
        let delta = BindingDelta::new(parent_delta, files);
        let delta_bytes = delta.encode()?;
        let delta_hash = ContentHash::compute_typed("blob", &delta_bytes);
        let rooted = root.with_binding_delta(delta_hash, RESOLVER_VERSION);
        let root_bytes = rooted.encode()?;
        let root_hash = ContentHash::compute_typed("blob", &root_bytes);
        Ok((
            root_hash,
            vec![(delta_hash, delta_bytes), (root_hash, root_bytes)],
        ))
    }

    fn semantic_file_nodes_changed(
        &self,
        parent_hash: ContentHash,
        current_hash: ContentHash,
        pending_nodes: &HashMap<ContentHash, Vec<u8>>,
    ) -> Result<bool> {
        let mut pairs = vec![(parent_hash, current_hash, 0usize)];
        while let Some((parent_hash, current_hash, depth)) = pairs.pop() {
            if parent_hash == current_hash {
                continue;
            }
            if depth > crate::repository_semantic_query::MAX_SEMANTIC_TREE_DEPTH {
                return Err(HeddleError::InvalidObject(
                    "semantic index tree exceeds max depth".to_string(),
                ));
            }
            let parent = self.load_semantic_tree_with_pending(&parent_hash, pending_nodes)?;
            let current = self.load_semantic_tree_with_pending(&current_hash, pending_nodes)?;
            let parent_by_name = parent
                .entries
                .iter()
                .map(|entry| (entry.name.as_str(), entry))
                .collect::<BTreeMap<_, _>>();
            let current_by_name = current
                .entries
                .iter()
                .map(|entry| (entry.name.as_str(), entry))
                .collect::<BTreeMap<_, _>>();
            let names = parent_by_name
                .keys()
                .chain(current_by_name.keys())
                .copied()
                .collect::<BTreeSet<_>>();

            for name in names {
                match (parent_by_name.get(name), current_by_name.get(name)) {
                    (Some(parent), Some(current)) => match (parent.kind, current.kind) {
                        (SemanticEntryKind::Opaque, SemanticEntryKind::Opaque) => {}
                        (SemanticEntryKind::File, SemanticEntryKind::File) => {
                            if parent.node != current.node {
                                return Ok(true);
                            }
                        }
                        (SemanticEntryKind::Dir, SemanticEntryKind::Dir) => {
                            pairs.push((parent.node, current.node, depth + 1));
                        }
                        _ => {
                            if self.semantic_entry_contains_file(parent, pending_nodes)?
                                || self.semantic_entry_contains_file(current, pending_nodes)?
                            {
                                return Ok(true);
                            }
                        }
                    },
                    (Some(entry), None) | (None, Some(entry)) => {
                        if self.semantic_entry_contains_file(entry, pending_nodes)? {
                            return Ok(true);
                        }
                    }
                    (None, None) => unreachable!("name came from one of the two trees"),
                }
            }
        }
        Ok(false)
    }

    fn semantic_entry_contains_file(
        &self,
        entry: &SemanticTreeEntry,
        pending_nodes: &HashMap<ContentHash, Vec<u8>>,
    ) -> Result<bool> {
        match entry.kind {
            SemanticEntryKind::File => Ok(true),
            SemanticEntryKind::Opaque => Ok(false),
            SemanticEntryKind::Dir => {
                let mut stack = vec![(entry.node, 0usize)];
                while let Some((hash, depth)) = stack.pop() {
                    if depth > crate::repository_semantic_query::MAX_SEMANTIC_TREE_DEPTH {
                        return Err(HeddleError::InvalidObject(
                            "semantic index tree exceeds max depth".to_string(),
                        ));
                    }
                    for child in self
                        .load_semantic_tree_with_pending(&hash, pending_nodes)?
                        .entries
                    {
                        match child.kind {
                            SemanticEntryKind::File => return Ok(true),
                            SemanticEntryKind::Dir => stack.push((child.node, depth + 1)),
                            SemanticEntryKind::Opaque => {}
                        }
                    }
                }
                Ok(false)
            }
        }
    }

    pub(crate) fn semantic_files(
        &self,
        root: &SemanticIndexRoot,
    ) -> Result<BTreeMap<String, RepositorySemanticFile>> {
        self.semantic_files_with_pending(root, &HashMap::new())
    }

    fn semantic_files_with_pending(
        &self,
        root: &SemanticIndexRoot,
        pending_nodes: &HashMap<ContentHash, Vec<u8>>,
    ) -> Result<BTreeMap<String, RepositorySemanticFile>> {
        let mut files = BTreeMap::new();
        let mut stack = vec![(String::new(), root.tree, 0usize)];
        while let Some((prefix, hash, depth)) = stack.pop() {
            if depth > crate::repository_semantic_query::MAX_SEMANTIC_TREE_DEPTH {
                return Err(HeddleError::InvalidObject(
                    "semantic index tree exceeds max depth".to_string(),
                ));
            }
            for entry in self
                .load_semantic_tree_with_pending(&hash, pending_nodes)?
                .entries
                .into_iter()
                .rev()
            {
                let path = if prefix.is_empty() {
                    entry.name
                } else {
                    format!("{prefix}/{}", entry.name)
                };
                match entry.kind {
                    SemanticEntryKind::Dir => stack.push((path, entry.node, depth + 1)),
                    SemanticEntryKind::File => {
                        let node = match pending_nodes.get(&entry.node) {
                            Some(bytes) => SemanticFileNode::decode(bytes)
                                .map_err(|err| HeddleError::InvalidObject(err.to_string()))?,
                            None => self.load_semantic_file(&entry.node)?,
                        };
                        files.insert(
                            path,
                            RepositorySemanticFile {
                                node_hash: entry.node,
                                node,
                            },
                        );
                    }
                    SemanticEntryKind::Opaque => {}
                }
            }
        }
        Ok(files)
    }

    fn load_semantic_tree_with_pending(
        &self,
        hash: &ContentHash,
        pending_nodes: &HashMap<ContentHash, Vec<u8>>,
    ) -> Result<SemanticTreeNode> {
        match pending_nodes.get(hash) {
            Some(bytes) => SemanticTreeNode::decode(bytes)
                .map_err(|err| HeddleError::InvalidObject(err.to_string())),
            None => self.load_semantic_tree(hash),
        }
    }
}

fn invalidation_frontier(
    parent_files: &BTreeMap<String, RepositorySemanticFile>,
    current_files: &BTreeMap<String, RepositorySemanticFile>,
    parent: &BTreeMap<String, semantic::cross_file_resolution::FileResolution>,
    current: &BTreeMap<String, semantic::cross_file_resolution::FileResolution>,
) -> BTreeSet<String> {
    let all_paths = parent_files
        .keys()
        .chain(current_files.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let changed = all_paths
        .into_iter()
        .filter(|path| {
            parent_files.get(path).map(|file| file.node_hash)
                != current_files.get(path).map(|file| file.node_hash)
        })
        .collect::<BTreeSet<_>>();
    let reverse = reverse_dependencies(parent, current);
    let mut frontier = changed.clone();
    let mut queue = VecDeque::from_iter(changed);
    while let Some(path) = queue.pop_front() {
        if let Some(importers) = reverse.get(&path) {
            for importer in importers {
                if frontier.insert(importer.clone()) {
                    queue.push_back(importer.clone());
                }
            }
        }
    }
    frontier
}

fn reverse_dependencies(
    parent: &BTreeMap<String, semantic::cross_file_resolution::FileResolution>,
    current: &BTreeMap<String, semantic::cross_file_resolution::FileResolution>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut reverse = BTreeMap::<String, BTreeSet<String>>::new();
    for (importer, resolution) in parent.iter().chain(current) {
        for dependency in &resolution.dependencies {
            reverse
                .entry(dependency.clone())
                .or_default()
                .insert(importer.clone());
        }
    }
    reverse
}
