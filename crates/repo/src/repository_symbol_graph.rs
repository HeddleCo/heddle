// SPDX-License-Identifier: Apache-2.0
//! Cross-file resolution and state-scoped binding-delta persistence.

#![cfg(feature = "tree-sitter-symbols")]

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use objects::{
    object::{
        BindingDelta, ContentHash, FileBindingDelta, SemanticEntryKind, SemanticFileNode,
        SemanticIndexRoot,
    },
    store::ObjectStore,
};
use semantic::cross_file_resolution::{
    RESOLVER_VERSION, RepositorySemanticFile, resolve_repository,
};

use crate::{HeddleError, Repository, Result};

impl Repository {
    /// Resolve the semantic root and persist a delta over the first parent's
    /// edge set, returning a replacement semantic-root blob hash.
    pub(crate) fn persist_resolved_semantic_edges(
        &self,
        parent_state: Option<&objects::object::State>,
        root: SemanticIndexRoot,
    ) -> Result<ContentHash> {
        let current_files = self.semantic_files(&root)?;
        let current_resolution = resolve_repository(&current_files);
        let parent_root = parent_state
            .map(|state| self.attached_semantic_index(&state.id()))
            .transpose()?
            .flatten();
        let parent_delta = parent_root
            .as_ref()
            .filter(|root| root.resolver_version == RESOLVER_VERSION)
            .and_then(|root| root.binding_delta);

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
        let delta = BindingDelta::new(parent_delta, files);
        let delta_bytes = delta.encode()?;
        let delta_hash = ContentHash::compute_typed("blob", &delta_bytes);
        let rooted = root.with_binding_delta(delta_hash, RESOLVER_VERSION);
        let root_bytes = rooted.encode()?;
        let root_hash = ContentHash::compute_typed("blob", &root_bytes);
        self.store()
            .put_blobs_packed(vec![(delta_hash, delta_bytes), (root_hash, root_bytes)])?;
        Ok(root_hash)
    }

    pub(crate) fn semantic_files(
        &self,
        root: &SemanticIndexRoot,
    ) -> Result<BTreeMap<String, RepositorySemanticFile>> {
        let mut files = BTreeMap::new();
        let mut stack = vec![(String::new(), root.tree, 0usize)];
        while let Some((prefix, hash, depth)) = stack.pop() {
            if depth > crate::repository_semantic_query::MAX_SEMANTIC_TREE_DEPTH {
                return Err(HeddleError::InvalidObject(
                    "semantic index tree exceeds max depth".to_string(),
                ));
            }
            for entry in self.load_semantic_tree(&hash)?.entries.into_iter().rev() {
                let path = if prefix.is_empty() {
                    entry.name
                } else {
                    format!("{prefix}/{}", entry.name)
                };
                match entry.kind {
                    SemanticEntryKind::Dir => stack.push((path, entry.node, depth + 1)),
                    SemanticEntryKind::File => {
                        let node: SemanticFileNode = self.load_semantic_file(&entry.node)?;
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
