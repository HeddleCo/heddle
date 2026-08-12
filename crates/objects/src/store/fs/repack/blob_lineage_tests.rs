// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use super::{blob_lineage::blob_lineage_order, tests::create_store};
use crate::{
    object::{Attribution, Blob, Principal, State, Tree, TreeEntry},
    store::ObjectStore,
};

#[test]
fn lineage_tracks_exact_rename_and_orders_versions_newest_first() {
    let (_temp, store) = create_store();
    let old_blob = Blob::from("shared body\nold ending\n");
    let new_blob = Blob::from("shared body\nnew ending\n");
    let old_hash = store.put_blob(&old_blob).unwrap();
    let new_hash = store.put_blob(&new_blob).unwrap();
    let root_tree =
        Tree::from_entries(vec![TreeEntry::file("before.rs", old_hash, false).unwrap()]);
    let renamed_tree =
        Tree::from_entries(vec![TreeEntry::file("after.rs", old_hash, false).unwrap()]);
    let head_tree = Tree::from_entries(vec![TreeEntry::file("after.rs", new_hash, false).unwrap()]);
    let root_tree_hash = store.put_tree(&root_tree).unwrap();
    let renamed_tree_hash = store.put_tree(&renamed_tree).unwrap();
    let head_tree_hash = store.put_tree(&head_tree).unwrap();
    let attribution = Attribution::human(Principal::new("Lineage", "lineage@example.com"));
    let root = State::new(root_tree_hash, vec![], attribution.clone());
    let renamed = State::new(renamed_tree_hash, vec![root.id()], attribution.clone());
    let head = State::new(head_tree_hash, vec![renamed.id()], attribution);
    let root_id = root.id();
    let renamed_id = renamed.id();
    let head_id = head.id();
    let states = HashMap::from([(root_id, root), (renamed_id, renamed), (head_id, head)]);

    let order = blob_lineage_order(
        &store,
        &states,
        &[head_id, renamed_id, root_id],
        &[old_hash, new_hash],
    )
    .unwrap();

    assert_eq!(order, vec![new_hash, old_hash]);
}

#[test]
fn lineage_tracks_similarity_rename_with_content_change() {
    let (_temp, store) = create_store();
    let old_blob = Blob::from("shared\nbody\nkept\nold ending\n");
    let new_blob = Blob::from("shared\nbody\nkept\nnew ending\n");
    let old_hash = store.put_blob(&old_blob).unwrap();
    let new_hash = store.put_blob(&new_blob).unwrap();
    let root_tree =
        Tree::from_entries(vec![TreeEntry::file("before.rs", old_hash, false).unwrap()]);
    let head_tree = Tree::from_entries(vec![TreeEntry::file("after.rs", new_hash, false).unwrap()]);
    let root_tree_hash = store.put_tree(&root_tree).unwrap();
    let head_tree_hash = store.put_tree(&head_tree).unwrap();
    let attribution = Attribution::human(Principal::new("Lineage", "lineage@example.com"));
    let root = State::new(root_tree_hash, vec![], attribution.clone());
    let head = State::new(head_tree_hash, vec![root.id()], attribution);
    let root_id = root.id();
    let head_id = head.id();
    let states = HashMap::from([(root_id, root), (head_id, head)]);

    let order =
        blob_lineage_order(&store, &states, &[head_id, root_id], &[old_hash, new_hash]).unwrap();

    assert_eq!(order, vec![new_hash, old_hash]);
}
