// SPDX-License-Identifier: Apache-2.0
//! Load semantic file facts and node hashes for graph binding.

#![cfg(feature = "tree-sitter-symbols")]

use std::collections::{BTreeMap, BTreeSet, HashMap};

use objects::object::{
    ContentHash, SemanticEntryKind, SemanticFileNode, SemanticIndexRoot, SemanticTreeNode,
};
use semantic::cross_file_resolution::RepositorySemanticFile;

use crate::{
    HeddleError, Repository, Result, repository_symbol_graph_frontier::changed_file_paths,
};

impl Repository {
    pub(crate) fn changed_semantic_file_paths(
        &self,
        parent_hash: ContentHash,
        current_hash: ContentHash,
        pending_nodes: &HashMap<ContentHash, Vec<u8>>,
    ) -> Result<BTreeSet<String>> {
        let parent = self.semantic_file_node_hashes(parent_hash, pending_nodes)?;
        let current = self.semantic_file_node_hashes(current_hash, pending_nodes)?;
        Ok(changed_file_paths(&parent, &current))
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

    fn semantic_file_node_hashes(
        &self,
        root_hash: ContentHash,
        pending_nodes: &HashMap<ContentHash, Vec<u8>>,
    ) -> Result<BTreeMap<String, ContentHash>> {
        let mut files = BTreeMap::new();
        self.walk_semantic_files(root_hash, pending_nodes, |path, entry| {
            if entry.kind == SemanticEntryKind::File {
                files.insert(path, entry.node);
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
