// SPDX-License-Identifier: Apache-2.0
//! Helper utilities for semantic diff.

use std::{cell::RefCell, collections::HashMap, path::Path};

use objects::{
    object::{Blob, ContentHash, Tree},
    store::LocalObjectStore,
};

pub(super) struct TreeBlobContentLoader<'a, S: LocalObjectStore + ?Sized> {
    store: &'a S,
    root_hash: ContentHash,
    trees: RefCell<HashMap<ContentHash, Option<Tree>>>,
}

impl<'a, S: LocalObjectStore + ?Sized> TreeBlobContentLoader<'a, S> {
    pub(super) fn new(store: &'a S, root_hash: ContentHash) -> Self {
        Self {
            store,
            root_hash,
            trees: RefCell::new(HashMap::new()),
        }
    }

    pub(super) fn load_content(&self, path: &Path) -> Result<Option<String>, anyhow::Error> {
        let Some(root) = self.get_tree(&self.root_hash)? else {
            return Ok(None);
        };
        let Some(blob) = self.get_blob_at_path(&root, &path.display().to_string())? else {
            return Ok(None);
        };
        Ok(blob.content_str().map(ToOwned::to_owned))
    }

    #[cfg(test)]
    fn cached_tree_count(&self) -> usize {
        self.trees.borrow().len()
    }

    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>, anyhow::Error> {
        if let Some(tree) = self.trees.borrow().get(hash).cloned() {
            return Ok(tree);
        }

        let tree = self.store.get_tree(hash)?;
        self.trees.borrow_mut().insert(*hash, tree.clone());
        Ok(tree)
    }

    fn get_blob_at_path(&self, tree: &Tree, path: &str) -> Result<Option<Blob>, anyhow::Error> {
        let parts: Vec<&str> = path.split('/').collect();
        self.get_blob_recursive(tree, &parts)
    }

    fn get_blob_recursive(
        &self,
        tree: &Tree,
        parts: &[&str],
    ) -> Result<Option<Blob>, anyhow::Error> {
        if parts.is_empty() {
            return Ok(None);
        }

        let name = parts[0];
        let entry = match tree.get(name) {
            Some(e) => e,
            None => return Ok(None),
        };

        if parts.len() == 1 {
            if entry.is_blob() {
                return Ok(self.store.get_blob(&entry.hash)?);
            }
        } else if entry.is_tree()
            && let Some(subtree) = self.get_tree(&entry.hash)?
        {
            return self.get_blob_recursive(&subtree, &parts[1..]);
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use objects::{
        object::{Blob, EntryType, FileMode, TreeEntry},
        store::{InMemoryStore, LocalObjectStore},
    };

    use super::*;

    fn put_blob(store: &InMemoryStore, content: &str) -> ContentHash {
        store.put_blob(&Blob::from(content)).unwrap()
    }

    fn put_tree(
        store: &InMemoryStore,
        entries: Vec<(&str, ContentHash, EntryType)>,
    ) -> ContentHash {
        let tree = Tree::from_entries(
            entries
                .into_iter()
                .map(|(name, hash, entry_type)| TreeEntry {
                    name: name.to_string(),
                    mode: FileMode::Normal,
                    hash,
                    entry_type,
                })
                .collect(),
        );
        store.put_tree(&tree).unwrap()
    }

    #[test]
    fn tree_blob_loader_reuses_cached_subtrees_for_sibling_paths() {
        let store = InMemoryStore::new();
        let nested = put_tree(
            &store,
            vec![
                (
                    "first.rs",
                    put_blob(&store, "fn first() {}\n"),
                    EntryType::Blob,
                ),
                (
                    "second.rs",
                    put_blob(&store, "fn second() {}\n"),
                    EntryType::Blob,
                ),
            ],
        );
        let root = put_tree(&store, vec![("src", nested, EntryType::Tree)]);
        let loader = TreeBlobContentLoader::new(&store, root);

        assert_eq!(
            loader
                .load_content(Path::new("src/first.rs"))
                .unwrap()
                .as_deref(),
            Some("fn first() {}\n")
        );
        assert_eq!(
            loader
                .load_content(Path::new("src/second.rs"))
                .unwrap()
                .as_deref(),
            Some("fn second() {}\n")
        );
        assert_eq!(
            loader.cached_tree_count(),
            2,
            "loader should cache the root and shared src subtree instead of resolving them per path"
        );
    }
}
