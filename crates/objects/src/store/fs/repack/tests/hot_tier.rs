// SPDX-License-Identifier: Apache-2.0

use std::{sync::Arc, time::Duration};

use super::{GateLoad, create_store, direct_pack_names, scheduler, started_handle};
use crate::{
    object::{Attribution, ContentHash, Principal, State, Tree, TreeEntry},
    store::{
        FsRepackOperation, FsStore, ObjectStore,
        pack::{ObjectType, PackObjectId, PackReadTier},
    },
};

fn metadata(seed: usize, parent: Option<crate::object::StateId>) -> (Tree, State) {
    let contents = format!("hot-tier-{seed}");
    let blob = ContentHash::compute_typed("blob", contents.as_bytes());
    let tree = Tree::from_entries(vec![
        TreeEntry::file(format!("file-{seed}.txt"), blob, seed.is_multiple_of(2)).unwrap(),
    ]);
    let parents = parent.into_iter().collect();
    let state = State::new(
        tree.hash(),
        parents,
        Attribution::human(Principal::new("Hot Tier", "hot-tier@example.com")),
    )
    .with_intent(format!("metadata version {seed}"));
    (tree, state)
}

fn read_tier(store: &FsStore, id: PackObjectId) -> PackReadTier {
    store
        .pack_manager()
        .read()
        .unwrap()
        .object_read_tier(&id)
        .unwrap()
        .expect("object tier")
}

fn is_settled_tree(store: &FsStore, hash: ContentHash) -> bool {
    store
        .npk1_manager()
        .read()
        .unwrap()
        .has_tree(&hash)
        .unwrap()
}

#[test]
fn recent_metadata_stays_random_access_until_solidification() {
    let (_temp, store) = create_store();
    let (tree_one, state_one) = metadata(1, None);
    let (tree_two, state_two) = metadata(2, Some(state_one.id()));
    store
        .put_snapshot_objects_packed(Vec::new(), &tree_one, &state_one)
        .unwrap();
    store
        .put_snapshot_objects_packed(Vec::new(), &tree_two, &state_two)
        .unwrap();

    let tree_one_id = PackObjectId::Hash(tree_one.hash());
    let state_one_id = PackObjectId::StateId(state_one.id());
    assert_eq!(read_tier(&store, tree_one_id), PackReadTier::Hot);
    assert_eq!(read_tier(&store, state_one_id), PackReadTier::Hot);

    let operation = Arc::new(FsRepackOperation::new(store.clone()));
    let report = started_handle(scheduler(None).repack_now(operation).unwrap())
        .wait()
        .unwrap();
    assert_eq!(report.objects_repacked, 4);
    let solid_store = FsStore::new(store.root());
    assert!(is_settled_tree(&solid_store, tree_one.hash()));
    assert_eq!(
        read_tier(&solid_store, state_one_id),
        PackReadTier::SolidFrame
    );

    solid_store.clear_recent_object_caches();
    assert_eq!(
        solid_store.get_tree(&tree_one.hash()).unwrap().unwrap(),
        tree_one
    );
    let (_, state_bytes) = solid_store
        .get_pack_object(&state_one_id)
        .unwrap()
        .expect("solid state frame");
    assert_eq!(
        state_bytes,
        rmp_serde::to_vec_named(&state_one).unwrap(),
        "solid-frame extraction must preserve canonical native bytes"
    );

    let (recent_tree, recent_state) = metadata(3, Some(state_two.id()));
    solid_store
        .put_snapshot_objects_packed(Vec::new(), &recent_tree, &recent_state)
        .unwrap();
    let recent_tree_id = PackObjectId::Hash(recent_tree.hash());
    let recent_state_id = PackObjectId::StateId(recent_state.id());
    assert_eq!(read_tier(&solid_store, recent_tree_id), PackReadTier::Hot);
    assert_eq!(read_tier(&solid_store, recent_state_id), PackReadTier::Hot);

    solid_store.clear_recent_object_caches();
    assert_eq!(
        solid_store.get_tree(&recent_tree.hash()).unwrap().unwrap(),
        recent_tree
    );
    let (object_type, recent_state_bytes) = solid_store
        .get_pack_object(&recent_state_id)
        .unwrap()
        .expect("hot state record");
    assert_eq!(object_type, ObjectType::State);
    assert_eq!(
        recent_state_bytes,
        rmp_serde::to_vec_named(&recent_state).unwrap()
    );
}

#[test]
fn metadata_installed_during_repack_survives_as_hot_tier() {
    let (_temp, store) = create_store();
    let mut parent = None;
    let mut first_tree = None;
    for seed in 0..16 {
        let (tree, state) = metadata(seed, parent);
        first_tree.get_or_insert_with(|| tree.clone());
        store
            .put_snapshot_objects_packed(Vec::new(), &tree, &state)
            .unwrap();
        parent = Some(state.id());
    }

    let (load, paused) = GateLoad::new(8);
    let operation = Arc::new(FsRepackOperation::new(store.clone()));
    let handle = started_handle(scheduler(Some(load.clone())).repack_now(operation).unwrap());
    paused.recv_timeout(Duration::from_secs(5)).unwrap();

    let (concurrent_tree, concurrent_state) = metadata(99, parent);
    store
        .put_snapshot_objects_packed(Vec::new(), &concurrent_tree, &concurrent_state)
        .unwrap();
    load.release();
    handle.wait().unwrap();

    let reopened = FsStore::new(store.root());
    let old_tree_id = PackObjectId::Hash(first_tree.unwrap().hash());
    let concurrent_tree_id = PackObjectId::Hash(concurrent_tree.hash());
    let concurrent_state_id = PackObjectId::StateId(concurrent_state.id());
    let PackObjectId::Hash(old_tree_hash) = old_tree_id else {
        unreachable!();
    };
    assert!(is_settled_tree(&reopened, old_tree_hash));
    assert_eq!(read_tier(&reopened, concurrent_tree_id), PackReadTier::Hot);
    assert_eq!(read_tier(&reopened, concurrent_state_id), PackReadTier::Hot);
    assert_eq!(
        direct_pack_names(store.root())
            .iter()
            .filter(|name| name.ends_with(".pack"))
            .count(),
        2,
        "cutover must retain the concurrently installed hot pack"
    );

    reopened.clear_recent_object_caches();
    assert_eq!(
        reopened.get_tree(&concurrent_tree.hash()).unwrap().unwrap(),
        concurrent_tree
    );
    assert_eq!(
        reopened
            .get_state(&concurrent_state.id())
            .unwrap()
            .unwrap()
            .id(),
        concurrent_state.id()
    );
}
