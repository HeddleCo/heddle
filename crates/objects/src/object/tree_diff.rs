// SPDX-License-Identifier: Apache-2.0
//! Shared tree-to-tree diffing implementation.
//!
//! This module provides a generic tree diffing algorithm that can be used
//! by both `repo` and `semantic` crates.

use super::FileChangeSet;
use crate::{
    object::{EntryType, Tree, TreeEntry},
    store::ObjectStore,
};

/// Collect all file changes between two trees.
pub fn diff_trees<S: ObjectStore + ?Sized>(
    store: &S,
    from: &crate::object::ContentHash,
    to: &crate::object::ContentHash,
) -> Result<FileChangeSet, anyhow::Error> {
    if from == to {
        return Ok(FileChangeSet::new());
    }
    let from_tree = store.get_tree(from)?;
    let to_tree = store.get_tree(to)?;
    let mut changes = FileChangeSet::new();
    diff_trees_recursive(store, &from_tree, &to_tree, "", &mut changes)?;
    Ok(changes)
}

/// Recursively diff two trees, collecting file changes.
fn diff_trees_recursive<S: ObjectStore + ?Sized>(
    store: &S,
    from: &Option<Tree>,
    to: &Option<Tree>,
    prefix: &str,
    changes: &mut FileChangeSet,
) -> Result<(), anyhow::Error> {
    let from_entries = from.as_ref().map_or(&[][..], Tree::entries);
    let to_entries = to.as_ref().map_or(&[][..], Tree::entries);

    let mut from_index = 0;
    let mut to_index = 0;

    while let (Some(from_entry), Some(to_entry)) =
        (from_entries.get(from_index), to_entries.get(to_index))
    {
        match from_entry.name.cmp(&to_entry.name) {
            std::cmp::Ordering::Less => {
                push_deleted_entry(store, prefix, from_entry, changes)?;
                from_index += 1;
            }
            std::cmp::Ordering::Greater => {
                push_added_entry(store, prefix, to_entry, changes)?;
                to_index += 1;
            }
            std::cmp::Ordering::Equal => {
                if from_entry.hash != to_entry.hash {
                    if from_entry.entry_type == EntryType::Tree
                        && to_entry.entry_type == EntryType::Tree
                    {
                        let from_subtree = store.get_tree(&from_entry.hash)?;
                        let to_subtree = store.get_tree(&to_entry.hash)?;
                        let path = child_path(prefix, &to_entry.name);
                        diff_trees_recursive(store, &from_subtree, &to_subtree, &path, changes)?;
                    } else {
                        let path = child_path(prefix, &to_entry.name);
                        changes.push_modified(&path);
                    }
                }
                from_index += 1;
                to_index += 1;
            }
        }
    }

    for from_entry in &from_entries[from_index..] {
        push_deleted_entry(store, prefix, from_entry, changes)?;
    }

    for to_entry in &to_entries[to_index..] {
        push_added_entry(store, prefix, to_entry, changes)?;
    }

    Ok(())
}

fn push_added_entry<S: ObjectStore + ?Sized>(
    store: &S,
    prefix: &str,
    to_entry: &TreeEntry,
    changes: &mut FileChangeSet,
) -> Result<(), anyhow::Error> {
    // Symmetric with the delete branch below: if the added entry is itself a
    // directory, recurse into it so callers see per-leaf `added` entries.
    let path = child_path(prefix, &to_entry.name);
    if to_entry.entry_type == EntryType::Tree {
        let to_subtree = store.get_tree(&to_entry.hash)?;
        diff_trees_recursive(store, &None, &to_subtree, &path, changes)?;
    } else {
        changes.push_added(&path);
    }
    Ok(())
}

fn push_deleted_entry<S: ObjectStore + ?Sized>(
    store: &S,
    prefix: &str,
    from_entry: &TreeEntry,
    changes: &mut FileChangeSet,
) -> Result<(), anyhow::Error> {
    let path = child_path(prefix, &from_entry.name);
    if from_entry.entry_type == EntryType::Tree {
        let from_subtree = store.get_tree(&from_entry.hash)?;
        diff_trees_recursive(store, &from_subtree, &None, &path, changes)?;
    } else {
        changes.push_deleted(&path);
    }
    Ok(())
}

fn child_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        let mut path = String::with_capacity(prefix.len() + 1 + name.len());
        path.push_str(prefix);
        path.push('/');
        path.push_str(name);
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        object::{Blob, ContentHash, FileMode, Tree, TreeEntry},
        store::InMemoryStore,
    };

    fn create_blob(store: &InMemoryStore, content: &str) -> ContentHash {
        let blob = Blob::from_slice(content.as_bytes());
        store.put_blob(&blob).unwrap()
    }

    fn create_tree(
        store: &InMemoryStore,
        entries: Vec<(&str, ContentHash, EntryType)>,
    ) -> ContentHash {
        let tree_entries: Vec<TreeEntry> = entries
            .into_iter()
            .map(|(name, hash, entry_type)| TreeEntry {
                name: name.to_string(),
                mode: FileMode::Normal,
                hash,
                entry_type,
            })
            .collect();
        let tree = Tree::from_entries(tree_entries);
        store.put_tree(&tree).unwrap()
    }

    #[test]
    fn test_diff_identical_trees() {
        let store = InMemoryStore::new();
        let hash = create_tree(
            &store,
            vec![("a.txt", create_blob(&store, "content"), EntryType::Blob)],
        );
        let changes = diff_trees(&store, &hash, &hash).unwrap();
        assert!(changes.is_empty());
    }

    #[test]
    fn test_diff_added_file() {
        let store = InMemoryStore::new();
        let from_hash = create_tree(&store, vec![]);
        let to_hash = create_tree(
            &store,
            vec![("a.txt", create_blob(&store, "content"), EntryType::Blob)],
        );
        let changes = diff_trees(&store, &from_hash, &to_hash).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes.added_count(), 1);

        let added: Vec<_> = changes.added().collect();
        assert_eq!(added[0].path, "a.txt");
    }

    #[test]
    fn test_diff_deleted_file() {
        let store = InMemoryStore::new();
        let blob_hash = create_blob(&store, "content");
        let from_hash = create_tree(&store, vec![("a.txt", blob_hash, EntryType::Blob)]);
        let to_hash = create_tree(&store, vec![]);
        let changes = diff_trees(&store, &from_hash, &to_hash).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes.deleted_count(), 1);

        let deleted: Vec<_> = changes.deleted().collect();
        assert_eq!(deleted[0].path, "a.txt");
    }

    #[test]
    fn test_diff_modified_file() {
        let store = InMemoryStore::new();
        let blob1_hash = create_blob(&store, "original");
        let blob2_hash = create_blob(&store, "modified");
        let from_hash = create_tree(&store, vec![("a.txt", blob1_hash, EntryType::Blob)]);
        let to_hash = create_tree(&store, vec![("a.txt", blob2_hash, EntryType::Blob)]);
        let changes = diff_trees(&store, &from_hash, &to_hash).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes.modified_count(), 1);

        let modified: Vec<_> = changes.modified().collect();
        assert_eq!(modified[0].path, "a.txt");
    }

    #[test]
    fn test_diff_nested_directories() {
        let store = InMemoryStore::new();
        let sub_blob = create_blob(&store, "sub content");
        let sub_tree = Tree::from_entries(vec![TreeEntry {
            name: "nested.txt".to_string(),
            mode: FileMode::Normal,
            hash: sub_blob,
            entry_type: EntryType::Blob,
        }]);
        let sub_hash = store.put_tree(&sub_tree).unwrap();

        let from_hash = create_tree(&store, vec![("subdir", sub_hash, EntryType::Tree)]);
        let to_hash = create_tree(&store, vec![]);
        let changes = diff_trees(&store, &from_hash, &to_hash).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes.deleted_count(), 1);

        let deleted: Vec<_> = changes.deleted().collect();
        assert_eq!(deleted[0].path, "subdir/nested.txt");
    }

    #[test]
    fn test_diff_added_directory_recurses() {
        // Mirror of `test_diff_nested_directories` for the add side.
        // An added subdirectory should surface each leaf file it
        // contains — not just the directory name. Previously the add
        // branch was asymmetric with the delete branch and returned a
        // single `"subdir"` entry; the root-commit case (empty →
        // full) hit this every time and broke downstream code that
        // expected leaf paths.
        let store = InMemoryStore::new();
        let sub_blob = create_blob(&store, "sub content");
        let sub_tree = Tree::from_entries(vec![TreeEntry {
            name: "nested.txt".to_string(),
            mode: FileMode::Normal,
            hash: sub_blob,
            entry_type: EntryType::Blob,
        }]);
        let sub_hash = store.put_tree(&sub_tree).unwrap();

        let from_hash = create_tree(&store, vec![]);
        let to_hash = create_tree(&store, vec![("subdir", sub_hash, EntryType::Tree)]);
        let changes = diff_trees(&store, &from_hash, &to_hash).unwrap();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes.added_count(), 1);

        let added: Vec<_> = changes.added().collect();
        assert_eq!(added[0].path, "subdir/nested.txt");
    }

    #[test]
    fn test_diff_added_directory_deep_nesting() {
        // `a/b/c.txt` added to an empty tree should produce one `added`
        // entry with the full slash-joined path. Exercises multi-level
        // recursion on the add side.
        let store = InMemoryStore::new();
        let leaf_blob = create_blob(&store, "leaf");
        let c_tree = Tree::from_entries(vec![TreeEntry {
            name: "c.txt".to_string(),
            mode: FileMode::Normal,
            hash: leaf_blob,
            entry_type: EntryType::Blob,
        }]);
        let c_hash = store.put_tree(&c_tree).unwrap();
        let b_tree = Tree::from_entries(vec![TreeEntry {
            name: "b".to_string(),
            mode: FileMode::Normal,
            hash: c_hash,
            entry_type: EntryType::Tree,
        }]);
        let b_hash = store.put_tree(&b_tree).unwrap();
        let from_hash = create_tree(&store, vec![]);
        let to_hash = create_tree(&store, vec![("a", b_hash, EntryType::Tree)]);

        let changes = diff_trees(&store, &from_hash, &to_hash).unwrap();
        assert_eq!(changes.added_count(), 1);
        let added: Vec<_> = changes.added().collect();
        assert_eq!(added[0].path, "a/b/c.txt");
    }

    #[test]
    fn test_diff_changes_follow_sorted_tree_entry_order() {
        let store = InMemoryStore::new();
        let from_sub_blob = create_blob(&store, "old nested");
        let from_sub_tree = Tree::from_entries(vec![TreeEntry {
            name: "c.txt".to_string(),
            mode: FileMode::Normal,
            hash: from_sub_blob,
            entry_type: EntryType::Blob,
        }]);
        let from_sub_hash = store.put_tree(&from_sub_tree).unwrap();
        let to_sub_blob = create_blob(&store, "new nested");
        let to_sub_tree = Tree::from_entries(vec![TreeEntry {
            name: "b.txt".to_string(),
            mode: FileMode::Normal,
            hash: to_sub_blob,
            entry_type: EntryType::Blob,
        }]);
        let to_sub_hash = store.put_tree(&to_sub_tree).unwrap();

        let from_hash = create_tree(
            &store,
            vec![
                ("z.txt", create_blob(&store, "old z"), EntryType::Blob),
                ("dir", from_sub_hash, EntryType::Tree),
                ("m.txt", create_blob(&store, "same"), EntryType::Blob),
                ("a.txt", create_blob(&store, "old a"), EntryType::Blob),
            ],
        );
        let to_hash = create_tree(
            &store,
            vec![
                ("b.txt", create_blob(&store, "new b"), EntryType::Blob),
                ("dir", to_sub_hash, EntryType::Tree),
                ("m.txt", create_blob(&store, "same"), EntryType::Blob),
                ("z.txt", create_blob(&store, "new z"), EntryType::Blob),
            ],
        );

        let changes: Vec<_> = diff_trees(&store, &from_hash, &to_hash)
            .unwrap()
            .into_iter()
            .map(|change| (change.path, change.kind))
            .collect();

        assert_eq!(
            changes,
            vec![
                ("a.txt".to_string(), crate::object::DiffKind::Deleted),
                ("b.txt".to_string(), crate::object::DiffKind::Added),
                ("dir/b.txt".to_string(), crate::object::DiffKind::Added),
                ("dir/c.txt".to_string(), crate::object::DiffKind::Deleted),
                ("z.txt".to_string(), crate::object::DiffKind::Modified),
            ]
        );
    }
}
