// SPDX-License-Identifier: Apache-2.0
//! Shared tree-to-tree diffing implementation.
//!
//! This module provides a generic tree diffing algorithm that can be used
//! by both `repo` and `semantic` crates.

use std::ops::ControlFlow;

use super::FileChangeSet;
use crate::{
    object::{DiffKind, EntryType, FileChange, Tree, TreeEntry},
    store::ObjectSource,
};

#[cfg(feature = "async-source")]
use crate::store::AsyncObjectSource;

/// Collect all file changes between two trees.
///
/// This is the materializing variant: it walks the trees via
/// [`diff_trees_visit`] and collects every [`FileChange`] into a
/// [`FileChangeSet`]. Streaming or early-exit consumers should prefer
/// [`diff_trees_visit`], which avoids allocating the full change list.
pub fn diff_trees<S: ObjectSource + ?Sized>(
    store: &S,
    from: &crate::object::ContentHash,
    to: &crate::object::ContentHash,
) -> Result<FileChangeSet, anyhow::Error> {
    let mut changes = FileChangeSet::new();
    // The visitor never short-circuits here, so the `ControlFlow` result is
    // always `Continue(())`; we ignore it and return the collected set.
    let _ = diff_trees_visit(store, from, to, |change| {
        changes.push(change);
        ControlFlow::<()>::Continue(())
    })?;
    Ok(changes)
}

/// Diff two trees with internal iteration, invoking `visitor` for each
/// [`FileChange`] in traversal order.
///
/// This is the streaming counterpart to [`diff_trees`]. The visitor returns a
/// [`ControlFlow`]: `Continue(())` keeps walking, while `Break(value)` stops
/// the traversal immediately — no further subtrees are loaded and no further
/// changes are produced. Early-exit consumers (e.g. "does anything under path
/// X differ?", first-N, quick-status checks) use this to avoid materializing
/// the entire change list.
///
/// On early exit the carried `B` is returned as `Ok(ControlFlow::Break(b))`;
/// on full completion it returns `Ok(ControlFlow::Continue(()))`. Changes are
/// emitted in exactly the same order as [`diff_trees`] collects them, so the
/// two paths are behavior-identical.
pub fn diff_trees_visit<S, V, B>(
    store: &S,
    from: &crate::object::ContentHash,
    to: &crate::object::ContentHash,
    mut visitor: V,
) -> Result<ControlFlow<B>, anyhow::Error>
where
    S: ObjectSource + ?Sized,
    V: FnMut(FileChange) -> ControlFlow<B>,
{
    if from == to {
        return Ok(ControlFlow::Continue(()));
    }
    let from_tree = store.get_tree(from)?;
    let to_tree = store.get_tree(to)?;
    diff_trees_recursive(store, &from_tree, &to_tree, "", &mut visitor)
}

/// Recursively diff two trees, invoking `visitor` for each change.
///
/// Returns `Ok(ControlFlow::Break(b))` as soon as the visitor breaks, so the
/// caller can propagate the short-circuit up the recursion without walking the
/// remaining entries or loading further subtrees.
fn diff_trees_recursive<S, V, B>(
    store: &S,
    from: &Option<Tree>,
    to: &Option<Tree>,
    prefix: &str,
    visitor: &mut V,
) -> Result<ControlFlow<B>, anyhow::Error>
where
    S: ObjectSource + ?Sized,
    V: FnMut(FileChange) -> ControlFlow<B>,
{
    let from_entries = from.as_ref().map_or(&[][..], Tree::entries);
    let to_entries = to.as_ref().map_or(&[][..], Tree::entries);

    let mut from_index = 0;
    let mut to_index = 0;

    while let (Some(from_entry), Some(to_entry)) =
        (from_entries.get(from_index), to_entries.get(to_index))
    {
        match from_entry.name.cmp(&to_entry.name) {
            std::cmp::Ordering::Less => {
                if let ControlFlow::Break(b) =
                    visit_deleted_entry(store, prefix, from_entry, visitor)?
                {
                    return Ok(ControlFlow::Break(b));
                }
                from_index += 1;
            }
            std::cmp::Ordering::Greater => {
                if let ControlFlow::Break(b) = visit_added_entry(store, prefix, to_entry, visitor)?
                {
                    return Ok(ControlFlow::Break(b));
                }
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
                        if let ControlFlow::Break(b) =
                            diff_trees_recursive(store, &from_subtree, &to_subtree, &path, visitor)?
                        {
                            return Ok(ControlFlow::Break(b));
                        }
                    } else {
                        let path = child_path(prefix, &to_entry.name);
                        if let ControlFlow::Break(b) =
                            visitor(FileChange::new(path, DiffKind::Modified))
                        {
                            return Ok(ControlFlow::Break(b));
                        }
                    }
                }
                from_index += 1;
                to_index += 1;
            }
        }
    }

    for from_entry in &from_entries[from_index..] {
        if let ControlFlow::Break(b) = visit_deleted_entry(store, prefix, from_entry, visitor)? {
            return Ok(ControlFlow::Break(b));
        }
    }

    for to_entry in &to_entries[to_index..] {
        if let ControlFlow::Break(b) = visit_added_entry(store, prefix, to_entry, visitor)? {
            return Ok(ControlFlow::Break(b));
        }
    }

    Ok(ControlFlow::Continue(()))
}

fn visit_added_entry<S, V, B>(
    store: &S,
    prefix: &str,
    to_entry: &TreeEntry,
    visitor: &mut V,
) -> Result<ControlFlow<B>, anyhow::Error>
where
    S: ObjectSource + ?Sized,
    V: FnMut(FileChange) -> ControlFlow<B>,
{
    // Symmetric with the delete branch below: if the added entry is itself a
    // directory, recurse into it so callers see per-leaf `added` entries.
    let path = child_path(prefix, &to_entry.name);
    if to_entry.entry_type == EntryType::Tree {
        let to_subtree = store.get_tree(&to_entry.hash)?;
        diff_trees_recursive(store, &None, &to_subtree, &path, visitor)
    } else {
        Ok(visitor(FileChange::new(path, DiffKind::Added)))
    }
}

fn visit_deleted_entry<S, V, B>(
    store: &S,
    prefix: &str,
    from_entry: &TreeEntry,
    visitor: &mut V,
) -> Result<ControlFlow<B>, anyhow::Error>
where
    S: ObjectSource + ?Sized,
    V: FnMut(FileChange) -> ControlFlow<B>,
{
    let path = child_path(prefix, &from_entry.name);
    if from_entry.entry_type == EntryType::Tree {
        let from_subtree = store.get_tree(&from_entry.hash)?;
        diff_trees_recursive(store, &from_subtree, &None, &path, visitor)
    } else {
        Ok(visitor(FileChange::new(path, DiffKind::Deleted)))
    }
}

#[cfg(feature = "async-source")]
pub async fn diff_trees_visit_async<S, V, B>(
    store: &S,
    from: &crate::object::ContentHash,
    to: &crate::object::ContentHash,
    mut visitor: V,
) -> Result<ControlFlow<B>, anyhow::Error>
where
    S: AsyncObjectSource + Sync + ?Sized,
    V: FnMut(FileChange) -> ControlFlow<B> + Send,
    B: Send,
{
    if from == to {
        return Ok(ControlFlow::Continue(()));
    }
    let from_tree = store.get_tree(from).await?;
    let to_tree = store.get_tree(to).await?;
    diff_trees_recursive_async(store, &from_tree, &to_tree, "", &mut visitor).await
}

#[cfg(feature = "async-source")]
async fn diff_trees_recursive_async<S, V, B>(
    store: &S,
    from: &Option<Tree>,
    to: &Option<Tree>,
    prefix: &str,
    visitor: &mut V,
) -> Result<ControlFlow<B>, anyhow::Error>
where
    S: AsyncObjectSource + Sync + ?Sized,
    V: FnMut(FileChange) -> ControlFlow<B> + Send,
    B: Send,
{
    let from_entries = from.as_ref().map_or(&[][..], Tree::entries);
    let to_entries = to.as_ref().map_or(&[][..], Tree::entries);

    let mut from_index = 0;
    let mut to_index = 0;

    while let (Some(from_entry), Some(to_entry)) =
        (from_entries.get(from_index), to_entries.get(to_index))
    {
        match from_entry.name.cmp(&to_entry.name) {
            std::cmp::Ordering::Less => {
                if let ControlFlow::Break(b) =
                    visit_deleted_entry_async(store, prefix, from_entry, visitor).await?
                {
                    return Ok(ControlFlow::Break(b));
                }
                from_index += 1;
            }
            std::cmp::Ordering::Greater => {
                if let ControlFlow::Break(b) =
                    visit_added_entry_async(store, prefix, to_entry, visitor).await?
                {
                    return Ok(ControlFlow::Break(b));
                }
                to_index += 1;
            }
            std::cmp::Ordering::Equal => {
                if from_entry.hash != to_entry.hash {
                    if from_entry.entry_type == EntryType::Tree
                        && to_entry.entry_type == EntryType::Tree
                    {
                        let from_subtree = store.get_tree(&from_entry.hash).await?;
                        let to_subtree = store.get_tree(&to_entry.hash).await?;
                        let path = child_path(prefix, &to_entry.name);
                        if let ControlFlow::Break(b) = Box::pin(diff_trees_recursive_async(
                            store,
                            &from_subtree,
                            &to_subtree,
                            &path,
                            visitor,
                        ))
                        .await?
                        {
                            return Ok(ControlFlow::Break(b));
                        }
                    } else {
                        let path = child_path(prefix, &to_entry.name);
                        if let ControlFlow::Break(b) =
                            visitor(FileChange::new(path, DiffKind::Modified))
                        {
                            return Ok(ControlFlow::Break(b));
                        }
                    }
                }
                from_index += 1;
                to_index += 1;
            }
        }
    }

    for from_entry in &from_entries[from_index..] {
        if let ControlFlow::Break(b) =
            visit_deleted_entry_async(store, prefix, from_entry, visitor).await?
        {
            return Ok(ControlFlow::Break(b));
        }
    }

    for to_entry in &to_entries[to_index..] {
        if let ControlFlow::Break(b) =
            visit_added_entry_async(store, prefix, to_entry, visitor).await?
        {
            return Ok(ControlFlow::Break(b));
        }
    }

    Ok(ControlFlow::Continue(()))
}

#[cfg(feature = "async-source")]
async fn visit_added_entry_async<S, V, B>(
    store: &S,
    prefix: &str,
    to_entry: &TreeEntry,
    visitor: &mut V,
) -> Result<ControlFlow<B>, anyhow::Error>
where
    S: AsyncObjectSource + Sync + ?Sized,
    V: FnMut(FileChange) -> ControlFlow<B> + Send,
    B: Send,
{
    let path = child_path(prefix, &to_entry.name);
    if to_entry.entry_type == EntryType::Tree {
        let to_subtree = store.get_tree(&to_entry.hash).await?;
        Box::pin(diff_trees_recursive_async(
            store,
            &None,
            &to_subtree,
            &path,
            visitor,
        ))
        .await
    } else {
        Ok(visitor(FileChange::new(path, DiffKind::Added)))
    }
}

#[cfg(feature = "async-source")]
async fn visit_deleted_entry_async<S, V, B>(
    store: &S,
    prefix: &str,
    from_entry: &TreeEntry,
    visitor: &mut V,
) -> Result<ControlFlow<B>, anyhow::Error>
where
    S: AsyncObjectSource + Sync + ?Sized,
    V: FnMut(FileChange) -> ControlFlow<B> + Send,
    B: Send,
{
    let path = child_path(prefix, &from_entry.name);
    if from_entry.entry_type == EntryType::Tree {
        let from_subtree = store.get_tree(&from_entry.hash).await?;
        Box::pin(diff_trees_recursive_async(
            store,
            &from_subtree,
            &None,
            &path,
            visitor,
        ))
        .await
    } else {
        Ok(visitor(FileChange::new(path, DiffKind::Deleted)))
    }
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
        store::{InMemoryStore, ObjectStore},
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

    /// The visitor variant must emit changes in exactly the same order as
    /// `diff_trees` collects them. This is the byte-identical guarantee that
    /// lets `diff_trees` delegate to `diff_trees_visit` without changing
    /// observable output.
    #[test]
    fn test_visit_matches_collect_order() {
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

        let collected: Vec<_> = diff_trees(&store, &from_hash, &to_hash)
            .unwrap()
            .into_iter()
            .map(FileChange::into_tuple)
            .collect();

        let mut visited = Vec::new();
        let flow = diff_trees_visit(&store, &from_hash, &to_hash, |change| {
            visited.push(change.into_tuple());
            ControlFlow::<()>::Continue(())
        })
        .unwrap();

        assert!(flow.is_continue());
        assert_eq!(visited, collected);
    }

    #[test]
    fn test_visit_identical_trees_never_calls_visitor() {
        let store = InMemoryStore::new();
        let hash = create_tree(
            &store,
            vec![("a.txt", create_blob(&store, "content"), EntryType::Blob)],
        );
        let mut count = 0usize;
        let flow = diff_trees_visit(&store, &hash, &hash, |_change| {
            count += 1;
            ControlFlow::<()>::Continue(())
        })
        .unwrap();
        assert!(flow.is_continue());
        assert_eq!(count, 0);
    }

    /// Early-exit: breaking from the visitor stops the walk and stops loading
    /// further subtrees. We assert both the carried `Break` payload and that
    /// the visitor saw strictly fewer changes than the full diff.
    #[test]
    fn test_visit_early_exit_stops_walk() {
        let store = InMemoryStore::new();
        // Five distinct top-level files all added → five `added` changes in
        // sorted order: a, b, c, d, e.
        let from_hash = create_tree(&store, vec![]);
        let to_hash = create_tree(
            &store,
            vec![
                ("a.txt", create_blob(&store, "a"), EntryType::Blob),
                ("b.txt", create_blob(&store, "b"), EntryType::Blob),
                ("c.txt", create_blob(&store, "c"), EntryType::Blob),
                ("d.txt", create_blob(&store, "d"), EntryType::Blob),
                ("e.txt", create_blob(&store, "e"), EntryType::Blob),
            ],
        );

        let mut seen = Vec::new();
        let flow = diff_trees_visit(&store, &from_hash, &to_hash, |change| {
            seen.push(change.path.clone());
            if change.path == "c.txt" {
                ControlFlow::Break("found c")
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap();

        assert_eq!(flow, ControlFlow::Break("found c"));
        // Stopped at c.txt — never visited d.txt or e.txt.
        assert_eq!(seen, vec!["a.txt", "b.txt", "c.txt"]);
    }

    /// Early-exit must also short-circuit out of nested-subtree recursion, not
    /// just the top level.
    #[test]
    fn test_visit_early_exit_inside_subtree() {
        let store = InMemoryStore::new();
        let sub_tree = Tree::from_entries(vec![
            TreeEntry {
                name: "x.txt".to_string(),
                mode: FileMode::Normal,
                hash: create_blob(&store, "x"),
                entry_type: EntryType::Blob,
            },
            TreeEntry {
                name: "y.txt".to_string(),
                mode: FileMode::Normal,
                hash: create_blob(&store, "y"),
                entry_type: EntryType::Blob,
            },
        ]);
        let sub_hash = store.put_tree(&sub_tree).unwrap();
        let from_hash = create_tree(&store, vec![]);
        let to_hash = create_tree(
            &store,
            vec![
                ("dir", sub_hash, EntryType::Tree),
                ("z.txt", create_blob(&store, "z"), EntryType::Blob),
            ],
        );

        let mut seen = Vec::new();
        let flow = diff_trees_visit(&store, &from_hash, &to_hash, |change| {
            seen.push(change.path.clone());
            ControlFlow::Break(())
        })
        .unwrap();

        assert_eq!(flow, ControlFlow::Break(()));
        // Broke on the very first leaf inside `dir`; `dir/y.txt` and `z.txt`
        // were never visited.
        assert_eq!(seen, vec!["dir/x.txt"]);
    }
}
