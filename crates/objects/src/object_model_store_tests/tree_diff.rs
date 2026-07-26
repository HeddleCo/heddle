use crate::object::*;
use crate::object::{Blob, ContentHash, EntryType, Tree, TreeEntry};
use crate::store::{InMemoryStore, ObjectStore};
use std::ops::ControlFlow;

fn create_blob(store: &InMemoryStore, content: &str) -> ContentHash {
    let blob = Blob::from_slice(content.as_bytes());
    store.put_blob(&blob).unwrap()
}

fn create_tree(store: &InMemoryStore, entries: Vec<(&str, ContentHash, EntryType)>) -> ContentHash {
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

    let waker = std::task::Waker::from(std::sync::Arc::new(ThreadWaker(std::thread::current())));
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
