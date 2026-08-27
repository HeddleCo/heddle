// SPDX-License-Identifier: Apache-2.0
//! Load semantic file facts and node hashes for graph binding.

#![cfg(feature = "tree-sitter-symbols")]

use std::collections::{BTreeMap, BTreeSet, HashMap};

use objects::object::{
    ContentHash, SemanticEntryKind, SemanticFileNode, SemanticIndexRoot, SemanticTreeNode,
};
use semantic::cross_file_resolution::RepositorySemanticFile;

use crate::{HeddleError, Repository, Result};

impl Repository {
    pub(crate) fn changed_semantic_file_paths(
        &self,
        parent_hash: ContentHash,
        current_hash: ContentHash,
        pending_nodes: &HashMap<ContentHash, Vec<u8>>,
    ) -> Result<BTreeSet<String>> {
        let mut changed = BTreeSet::new();
        let mut stack = vec![(String::new(), Some(parent_hash), Some(current_hash), 0usize)];
        while let Some((prefix, parent, current, depth)) = stack.pop() {
            if parent == current {
                continue;
            }
            if depth > crate::repository_semantic_query::MAX_SEMANTIC_TREE_DEPTH {
                return Err(HeddleError::InvalidObject(
                    "semantic index tree exceeds max depth".to_string(),
                ));
            }
            let parent_entries = parent
                .map(|hash| self.load_semantic_tree_with_pending(&hash, pending_nodes))
                .transpose()?
                .map(|node| {
                    node.entries
                        .into_iter()
                        .map(|entry| (entry.name.clone(), entry))
                        .collect::<BTreeMap<_, _>>()
                })
                .unwrap_or_default();
            let current_entries = current
                .map(|hash| self.load_semantic_tree_with_pending(&hash, pending_nodes))
                .transpose()?
                .map(|node| {
                    node.entries
                        .into_iter()
                        .map(|entry| (entry.name.clone(), entry))
                        .collect::<BTreeMap<_, _>>()
                })
                .unwrap_or_default();
            let names = parent_entries
                .keys()
                .chain(current_entries.keys())
                .cloned()
                .collect::<BTreeSet<_>>();
            for name in names {
                let path = join_semantic_path(&prefix, &name);
                let parent_entry = parent_entries.get(&name);
                let current_entry = current_entries.get(&name);
                match (parent_entry, current_entry) {
                    (Some(parent), Some(current))
                        if parent.kind == SemanticEntryKind::Dir
                            && current.kind == SemanticEntryKind::Dir =>
                    {
                        stack.push((path, Some(parent.node), Some(current.node), depth + 1));
                    }
                    (Some(parent), Some(current)) => {
                        if parent.kind == SemanticEntryKind::Dir {
                            stack.push((path.clone(), Some(parent.node), None, depth + 1));
                        } else if parent.kind == SemanticEntryKind::File
                            && (current.kind != SemanticEntryKind::File
                                || parent.node != current.node)
                        {
                            changed.insert(path.clone());
                        }
                        if current.kind == SemanticEntryKind::Dir {
                            stack.push((path.clone(), None, Some(current.node), depth + 1));
                        } else if current.kind == SemanticEntryKind::File
                            && (parent.kind != SemanticEntryKind::File
                                || parent.node != current.node)
                        {
                            changed.insert(path);
                        }
                    }
                    (Some(parent), None) if parent.kind == SemanticEntryKind::Dir => {
                        stack.push((path, Some(parent.node), None, depth + 1));
                    }
                    (None, Some(current)) if current.kind == SemanticEntryKind::Dir => {
                        stack.push((path, None, Some(current.node), depth + 1));
                    }
                    (Some(parent), None) if parent.kind == SemanticEntryKind::File => {
                        changed.insert(path);
                    }
                    (None, Some(current)) if current.kind == SemanticEntryKind::File => {
                        changed.insert(path);
                    }
                    _ => {}
                }
            }
        }
        Ok(changed)
    }

    pub(crate) fn semantic_files_with_pending(
        &self,
        root: &SemanticIndexRoot,
        pending_nodes: &HashMap<ContentHash, Vec<u8>>,
    ) -> Result<BTreeMap<String, RepositorySemanticFile>> {
        let mut files = BTreeMap::new();
        self.walk_semantic_files(root.tree, pending_nodes, |path, entry| {
            if entry.kind == SemanticEntryKind::File {
                files.insert(path, self.load_repository_file(&entry.node, pending_nodes)?);
            }
            Ok(())
        })?;
        Ok(files)
    }

    fn walk_semantic_files(
        &self,
        root_hash: ContentHash,
        pending_nodes: &HashMap<ContentHash, Vec<u8>>,
        mut visit: impl FnMut(String, objects::object::SemanticTreeEntry) -> Result<()>,
    ) -> Result<()> {
        let mut stack = vec![(String::new(), root_hash, 0usize)];
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
                let path = join_semantic_path(&prefix, &entry.name);
                match entry.kind {
                    SemanticEntryKind::Dir => stack.push((path, entry.node, depth + 1)),
                    SemanticEntryKind::File | SemanticEntryKind::Opaque => visit(path, entry)?,
                }
            }
        }
        Ok(())
    }

    fn load_repository_file(
        &self,
        node: &ContentHash,
        pending_nodes: &HashMap<ContentHash, Vec<u8>>,
    ) -> Result<RepositorySemanticFile> {
        let decoded = match pending_nodes.get(node) {
            Some(bytes) => SemanticFileNode::decode(bytes)
                .map_err(|err| HeddleError::InvalidObject(err.to_string()))?,
            None => self.load_semantic_file(node)?,
        };
        Ok(RepositorySemanticFile {
            node_hash: *node,
            node: decoded,
        })
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

fn join_semantic_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}
