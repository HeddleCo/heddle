// SPDX-License-Identifier: Apache-2.0
//! Shared tree-to-tree diffing implementation.
//!
//! This module provides a generic tree diffing algorithm that can be used
//! by both `repo` and `semantic` crates.

use std::ops::ControlFlow;

use super::FileChangeSet;
#[cfg(feature = "async-source")]
use crate::store::AsyncObjectSource;
use crate::{
    object::{ContentHash, DiffKind, FileChange, Tree},
    store::ObjectSource,
};

struct DiffFrame {
    from: Option<Tree>,
    to: Option<Tree>,
    prefix: String,
    from_index: usize,
    to_index: usize,
}

enum DiffStep {
    Emit(FileChange),
    Descend {
        from_hash: Option<ContentHash>,
        to_hash: Option<ContentHash>,
        name: String,
    },
    Done,
}

fn advance_merge(frame: &mut DiffFrame) -> DiffStep {
    let from_entries = frame.from.as_ref().map_or(&[][..], Tree::entries);
    let to_entries = frame.to.as_ref().map_or(&[][..], Tree::entries);

    loop {
        match (
            from_entries.get(frame.from_index),
            to_entries.get(frame.to_index),
        ) {
            (Some(from_entry), Some(to_entry)) => match from_entry.name().cmp(to_entry.name()) {
                std::cmp::Ordering::Less => {
                    frame.from_index += 1;
                    if let Some(from_hash) = from_entry.tree_hash() {
                        return DiffStep::Descend {
                            from_hash: Some(from_hash),
                            to_hash: None,
                            name: from_entry.name().to_owned(),
                        };
                    }
                    return DiffStep::Emit(FileChange::new(
                        child_path(&frame.prefix, from_entry.name()),
                        DiffKind::Deleted,
                    ));
                }
                std::cmp::Ordering::Greater => {
                    frame.to_index += 1;
                    if let Some(to_hash) = to_entry.tree_hash() {
                        return DiffStep::Descend {
                            from_hash: None,
                            to_hash: Some(to_hash),
                            name: to_entry.name().to_owned(),
                        };
                    }
                    return DiffStep::Emit(FileChange::new(
                        child_path(&frame.prefix, to_entry.name()),
                        DiffKind::Added,
                    ));
                }
                std::cmp::Ordering::Equal => {
                    frame.from_index += 1;
                    frame.to_index += 1;
                    if from_entry.target() == to_entry.target() {
                        continue;
                    }
                    if let (Some(from_hash), Some(to_hash)) =
                        (from_entry.tree_hash(), to_entry.tree_hash())
                    {
                        return DiffStep::Descend {
                            from_hash: Some(from_hash),
                            to_hash: Some(to_hash),
                            name: to_entry.name().to_owned(),
                        };
                    }
                    return DiffStep::Emit(FileChange::new(
                        child_path(&frame.prefix, to_entry.name()),
                        DiffKind::Modified,
                    ));
                }
            },
            (Some(from_entry), None) => {
                frame.from_index += 1;
                if let Some(from_hash) = from_entry.tree_hash() {
                    return DiffStep::Descend {
                        from_hash: Some(from_hash),
                        to_hash: None,
                        name: from_entry.name().to_owned(),
                    };
                }
                return DiffStep::Emit(FileChange::new(
                    child_path(&frame.prefix, from_entry.name()),
                    DiffKind::Deleted,
                ));
            }
            (None, Some(to_entry)) => {
                frame.to_index += 1;
                if let Some(to_hash) = to_entry.tree_hash() {
                    return DiffStep::Descend {
                        from_hash: None,
                        to_hash: Some(to_hash),
                        name: to_entry.name().to_owned(),
                    };
                }
                return DiffStep::Emit(FileChange::new(
                    child_path(&frame.prefix, to_entry.name()),
                    DiffKind::Added,
                ));
            }
            (None, None) => return DiffStep::Done,
        }
    }
}

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
    let mut stack = vec![DiffFrame {
        from: from_tree,
        to: to_tree,
        prefix: String::new(),
        from_index: 0,
        to_index: 0,
    }];

    while !stack.is_empty() {
        match advance_merge(stack.last_mut().expect("stack is not empty")) {
            DiffStep::Emit(change) => {
                if let ControlFlow::Break(b) = visitor(change) {
                    return Ok(ControlFlow::Break(b));
                }
            }
            DiffStep::Descend {
                from_hash,
                to_hash,
                name,
            } => {
                let from_subtree = from_hash
                    .map(|hash| store.get_tree(&hash))
                    .transpose()?
                    .flatten();
                let to_subtree = to_hash
                    .map(|hash| store.get_tree(&hash))
                    .transpose()?
                    .flatten();
                let prefix = child_path(
                    &stack.last().expect("parent frame remains on stack").prefix,
                    &name,
                );
                stack.push(DiffFrame {
                    from: from_subtree,
                    to: to_subtree,
                    prefix,
                    from_index: 0,
                    to_index: 0,
                });
            }
            DiffStep::Done => {
                stack.pop();
            }
        }
    }

    Ok(ControlFlow::Continue(()))
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
    let mut stack = vec![DiffFrame {
        from: from_tree,
        to: to_tree,
        prefix: String::new(),
        from_index: 0,
        to_index: 0,
    }];

    while !stack.is_empty() {
        match advance_merge(stack.last_mut().expect("stack is not empty")) {
            DiffStep::Emit(change) => {
                if let ControlFlow::Break(b) = visitor(change) {
                    return Ok(ControlFlow::Break(b));
                }
            }
            DiffStep::Descend {
                from_hash,
                to_hash,
                name,
            } => {
                let from_subtree = match from_hash {
                    Some(hash) => store.get_tree(&hash).await?,
                    None => None,
                };
                let to_subtree = match to_hash {
                    Some(hash) => store.get_tree(&hash).await?,
                    None => None,
                };
                let prefix = child_path(
                    &stack.last().expect("parent frame remains on stack").prefix,
                    &name,
                );
                stack.push(DiffFrame {
                    from: from_subtree,
                    to: to_subtree,
                    prefix,
                    from_index: 0,
                    to_index: 0,
                });
            }
            DiffStep::Done => {
                stack.pop();
            }
        }
    }

    Ok(ControlFlow::Continue(()))
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
        object::{Blob, ContentHash, EntryType, Tree, TreeEntry},
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
            .map(|(name, hash, entry_type)| match entry_type {
                EntryType::Blob => TreeEntry::file(name, hash, false).unwrap(),
                EntryType::Tree => TreeEntry::directory(name, hash).unwrap(),
                EntryType::Symlink => TreeEntry::symlink(name, hash).unwrap(),
                EntryType::Gitlink => panic!("use TreeEntry::gitlink for gitlink tests"),
                EntryType::Spoollink => {
                    panic!("use TreeEntry::spoollink for spoollink tests")
                }
            })
            .collect();
        let tree = Tree::from_entries(tree_entries);
        store.put_tree(&tree).unwrap()
    }

    fn create_deep_changed_trees(
        store: &InMemoryStore,
        depth: usize,
    ) -> (ContentHash, ContentHash, String) {
        let mut from_hash = create_tree(
            store,
            vec![("leaf.txt", create_blob(store, "old"), EntryType::Blob)],
        );
        let mut to_hash = create_tree(
            store,
            vec![("leaf.txt", create_blob(store, "new"), EntryType::Blob)],
        );

        for _ in 0..depth {
            from_hash = create_tree(store, vec![("d", from_hash, EntryType::Tree)]);
            to_hash = create_tree(store, vec![("d", to_hash, EntryType::Tree)]);
        }

        let expected_path = format!("{}leaf.txt", "d/".repeat(depth));
        (from_hash, to_hash, expected_path)
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
        let sub_tree = Tree::from_entries(vec![
            TreeEntry::file("nested.txt", sub_blob, false).unwrap(),
        ]);
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
        let sub_tree = Tree::from_entries(vec![
            TreeEntry::file("nested.txt", sub_blob, false).unwrap(),
        ]);
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
        let c_tree = Tree::from_entries(vec![TreeEntry::file("c.txt", leaf_blob, false).unwrap()]);
        let c_hash = store.put_tree(&c_tree).unwrap();
        let b_tree = Tree::from_entries(vec![TreeEntry::directory("b", c_hash).unwrap()]);
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
        let from_sub_tree = Tree::from_entries(vec![
            TreeEntry::file("c.txt", from_sub_blob, false).unwrap(),
        ]);
        let from_sub_hash = store.put_tree(&from_sub_tree).unwrap();
        let to_sub_blob = create_blob(&store, "new nested");
        let to_sub_tree =
            Tree::from_entries(vec![TreeEntry::file("b.txt", to_sub_blob, false).unwrap()]);
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
        let from_sub_tree = Tree::from_entries(vec![
            TreeEntry::file("c.txt", from_sub_blob, false).unwrap(),
        ]);
        let from_sub_hash = store.put_tree(&from_sub_tree).unwrap();
        let to_sub_blob = create_blob(&store, "new nested");
        let to_sub_tree =
            Tree::from_entries(vec![TreeEntry::file("b.txt", to_sub_blob, false).unwrap()]);
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
            TreeEntry::file("x.txt", create_blob(&store, "x"), false).unwrap(),
            TreeEntry::file("y.txt", create_blob(&store, "y"), false).unwrap(),
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

    #[test]
    fn test_deep_tree_diff_uses_constant_native_stack_sync() {
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(|| {
                let store = InMemoryStore::new();
                let (from_hash, to_hash, expected_path) = create_deep_changed_trees(&store, 10_000);

                let changes = diff_trees(&store, &from_hash, &to_hash).unwrap();
                assert_eq!(
                    changes
                        .into_iter()
                        .map(FileChange::into_tuple)
                        .collect::<Vec<_>>(),
                    vec![(expected_path, DiffKind::Modified)]
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[cfg(feature = "async-source")]
    struct AsyncInMemorySource(InMemoryStore);

    #[cfg(feature = "async-source")]
    impl AsyncObjectSource for AsyncInMemorySource {
        async fn get_tree(
            &self,
            hash: &ContentHash,
        ) -> crate::error::Result<Option<crate::object::Tree>> {
            ObjectStore::get_tree(&self.0, hash)
        }

        async fn get_state(
            &self,
            id: &crate::object::StateId,
        ) -> crate::error::Result<Option<crate::object::State>> {
            ObjectStore::get_state(&self.0, id)
        }

        async fn get_blob(
            &self,
            hash: &ContentHash,
        ) -> crate::error::Result<Option<crate::object::Blob>> {
            ObjectStore::get_blob(&self.0, hash)
        }
    }

    #[cfg(feature = "async-source")]
    fn block_on_current_thread<F: std::future::Future>(future: F) -> F::Output {
        struct ThreadWaker(std::thread::Thread);

        impl std::task::Wake for ThreadWaker {
            fn wake(self: std::sync::Arc<Self>) {
                self.0.unpark();
            }
        }

        let waker =
            std::task::Waker::from(std::sync::Arc::new(ThreadWaker(std::thread::current())));
        let mut context = std::task::Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            match future.as_mut().poll(&mut context) {
                std::task::Poll::Ready(output) => return output,
                std::task::Poll::Pending => std::thread::park(),
            }
        }
    }

    #[cfg(feature = "async-source")]
    #[test]
    fn test_deep_tree_diff_uses_constant_native_stack_async() {
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(|| {
                let store = InMemoryStore::new();
                let (from_hash, to_hash, expected_path) = create_deep_changed_trees(&store, 10_000);
                let store = AsyncInMemorySource(store);
                let mut changes = Vec::new();

                let flow = block_on_current_thread(diff_trees_visit_async(
                    &store,
                    &from_hash,
                    &to_hash,
                    |change| {
                        changes.push(change.into_tuple());
                        ControlFlow::<()>::Continue(())
                    },
                ))
                .unwrap();

                assert!(flow.is_continue());
                assert_eq!(changes, vec![(expected_path, DiffKind::Modified)]);
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
