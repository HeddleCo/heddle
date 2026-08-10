use crate::{
    object::{Blob, Tree, TreeEntry, *},
    store::{InMemoryStore, ObjectStore},
};

#[test]
fn walk_tree_integrity_dedups_shared_subtrees() {
    let store = InMemoryStore::new();
    let blob = Blob::from("shared\n");
    let blob_hash = store.put_blob(&blob).unwrap();
    let shared = Tree::from_entries(vec![TreeEntry::file("leaf.txt", blob_hash, false).unwrap()]);
    let shared_hash = store.put_tree(&shared).unwrap();
    let root_a = Tree::from_entries(vec![
        TreeEntry::directory("shared", shared_hash).unwrap(),
        TreeEntry::file("a.txt", blob_hash, false).unwrap(),
    ]);
    let root_b = Tree::from_entries(vec![TreeEntry::directory("shared", shared_hash).unwrap()]);
    let root_a_hash = store.put_tree(&root_a).unwrap();
    let root_b_hash = store.put_tree(&root_b).unwrap();

    let mut enter_count = 0;
    let mut blob_leaves = Vec::new();

    walk_tree_integrity(&store, [root_a_hash, root_b_hash], &mut |event| {
        match event {
            TreeIntegrityEvent::EnterTree { .. } => enter_count += 1,
            TreeIntegrityEvent::BlobLeaf { path, .. } => blob_leaves.push(path),
            TreeIntegrityEvent::TreeRef { .. } => {}
            TreeIntegrityEvent::MissingTree { .. } => {}
        }
        Ok(())
    })
    .unwrap();

    assert_eq!(enter_count, 3, "shared subtree must be visited once");
    assert_eq!(
        blob_leaves,
        vec!["a.txt".to_string(), "shared/leaf.txt".to_string()]
    );
}

#[test]
fn walk_tree_integrity_reports_missing_subtree() {
    let store = InMemoryStore::new();
    let missing = ContentHash::compute(b"missing-tree");
    let root = Tree::from_entries(vec![TreeEntry::directory("gone", missing).unwrap()]);
    let root_hash = store.put_tree(&root).unwrap();

    let mut events = Vec::new();
    walk_tree_integrity(&store, [root_hash], &mut |event| {
        match event {
            TreeIntegrityEvent::EnterTree { .. } => events.push("enter"),
            TreeIntegrityEvent::TreeRef { .. } => events.push("ref"),
            TreeIntegrityEvent::MissingTree {
                hash,
                parent_hash,
                path,
            } => {
                assert_eq!(hash, missing);
                assert_eq!(parent_hash, Some(root_hash));
                assert_eq!(path, "gone");
                events.push("missing");
            }
            TreeIntegrityEvent::BlobLeaf { .. } => events.push("blob"),
        }
        Ok(())
    })
    .unwrap();

    assert_eq!(events, vec!["enter", "ref", "missing"]);
}

#[test]
fn walk_tree_integrity_handles_deep_trees_iteratively() {
    let store = InMemoryStore::new();
    let mut child_hash = store.put_tree(&Tree::new()).unwrap();
    for depth in 0..10_000 {
        let tree = Tree::from_entries(vec![
            TreeEntry::directory(format!("d{depth}"), child_hash).unwrap(),
        ]);
        child_hash = store.put_tree(&tree).unwrap();
    }

    let mut entered = 0;
    walk_tree_integrity(&store, [child_hash], &mut |event| {
        if matches!(event, TreeIntegrityEvent::EnterTree { .. }) {
            entered += 1;
        }
        Ok(())
    })
    .unwrap();

    assert_eq!(entered, 10_001);
}
