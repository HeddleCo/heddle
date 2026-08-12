// SPDX-License-Identifier: Apache-2.0

use std::{fs, num::NonZeroUsize, sync::Arc};

use chrono::{TimeZone, Utc};
use heddle_format::compression::CompressionConfig;

use super::{create_store, direct_pack_names, scheduler, started_handle};
use crate::{
    object::{Action, Attribution, Blob, Operation, Principal, State, Tree, TreeEntry},
    store::{
        FsRepackOperation, ObjectStore, RepackError, RepackOperation, RepackPolicy,
        RepackResourceLimits, RepackScheduler,
        pack::{ObjectType, PackBuilder, PackObjectId, PackReader},
    },
};

#[test]
fn repack_preserves_every_typed_identity_byte_identically() {
    let (_temp, store) = create_store();
    let blob = Blob::from("typed integrity payload");
    let blob_hash = blob.hash();
    let tree = Tree::from_entries(vec![
        TreeEntry::file("payload.txt", blob_hash, false).unwrap(),
    ]);
    let tree_hash = tree.hash();
    let attribution = Attribution::human(Principal::new("Repack", "repack@example.com"));
    let state = State::new(tree_hash, vec![], attribution.clone()).with_intent("integrity");
    let state_id = state.id();
    let tree_two = Tree::from_entries(vec![
        TreeEntry::file("payload.txt", blob_hash, true).unwrap(),
    ]);
    let tree_two_hash = tree_two.hash();
    let state_two = State::new(tree_two_hash, vec![state_id], attribution.clone())
        .with_intent("second directory version");
    let state_two_id = state_two.id();
    let mut action = Action::new(
        None,
        state_id,
        Operation::Snapshot,
        "repack action",
        attribution,
    )
    .with_timestamp(Utc.timestamp_opt(1_700_000_000, 0).unwrap());
    let action_id = action.id();

    let expected = vec![
        (
            PackObjectId::Hash(blob_hash),
            ObjectType::Blob,
            blob.content().to_vec(),
        ),
        (
            PackObjectId::Hash(tree_hash),
            ObjectType::Tree,
            rmp_serde::to_vec_named(&tree).unwrap(),
        ),
        (
            PackObjectId::StateId(state_id),
            ObjectType::State,
            rmp_serde::to_vec_named(&state).unwrap(),
        ),
        (
            PackObjectId::Hash(tree_two_hash),
            ObjectType::Tree,
            rmp_serde::to_vec_named(&tree_two).unwrap(),
        ),
        (
            PackObjectId::StateId(state_two_id),
            ObjectType::State,
            rmp_serde::to_vec_named(&state_two).unwrap(),
        ),
        (
            PackObjectId::Hash(*action_id.as_hash()),
            ObjectType::Action,
            rmp_serde::to_vec_named(&action).unwrap(),
        ),
    ];
    let mut builder = PackBuilder::new(CompressionConfig::disabled());
    for (id, object_type, bytes) in &expected {
        builder.add_id(*id, *object_type, bytes.clone());
    }
    let (pack, index, _) = builder.build().unwrap();
    store.install_pack(&pack, &index).unwrap();
    let stale_reader = crate::store::FsStore::new(store.root());

    let operation = Arc::new(FsRepackOperation::new(store.clone()));
    let report = started_handle(scheduler(None).repack_now(operation).unwrap())
        .wait()
        .unwrap();
    assert_eq!(report.objects_repacked, 6);
    let pack_name = direct_pack_names(store.root())
        .into_iter()
        .find(|name| name.ends_with(".pack"))
        .expect("replacement pack");
    let pack_path = store.root().join("packs").join(pack_name);
    let index_path = pack_path.with_extension("idx");
    let pack_bytes = fs::read(&pack_path).unwrap();
    assert_eq!(u64::from_be_bytes(pack_bytes[8..16].try_into().unwrap()), 4);
    assert_eq!(
        PackReader::open(&pack_path, &index_path)
            .unwrap()
            .list_ids()
            .unwrap()
            .len(),
        expected.len()
    );
    stale_reader.clear_recent_object_caches();

    for (id, expected_type, expected_bytes) in &expected {
        let (actual_type, actual_bytes) = stale_reader
            .get_pack_object(id)
            .unwrap()
            .expect("every typed object must survive repack");
        assert_eq!(actual_type, *expected_type);
        assert_eq!(
            &actual_bytes, expected_bytes,
            "logical bytes changed for {id:?}"
        );
    }

    let loaded_blob = stale_reader.get_blob(&blob_hash).unwrap().unwrap();
    assert_eq!(loaded_blob.content(), blob.content());
    assert_eq!(loaded_blob.hash(), blob_hash);
    let loaded_tree = stale_reader.get_tree(&tree_hash).unwrap().unwrap();
    assert_eq!(loaded_tree, tree);
    assert_eq!(loaded_tree.hash(), tree_hash);
    let loaded_state = stale_reader.get_state(&state_id).unwrap().unwrap();
    assert_eq!(
        rmp_serde::to_vec_named(&loaded_state).unwrap(),
        rmp_serde::to_vec_named(&state).unwrap()
    );
    assert_eq!(loaded_state.id(), state_id);
    let mut loaded_action = stale_reader.get_action(&action_id).unwrap().unwrap();
    assert_eq!(
        rmp_serde::to_vec_named(&loaded_action).unwrap(),
        rmp_serde::to_vec_named(&action).unwrap()
    );
    assert_eq!(loaded_action.id(), action_id);
}

#[test]
fn loose_object_trigger_consolidates_and_reclaims_the_loose_copy() {
    let (_temp, store) = create_store();
    let blob = Blob::from("loose threshold payload");
    let hash = store.put_blob(&blob).unwrap();
    let policy = RepackPolicy {
        loose_object_threshold: Some(1),
        pack_count_threshold: None,
        pack_bytes_threshold: None,
        fragmentation_threshold_bps: None,
    };
    let limits = RepackResourceLimits::new(NonZeroUsize::MIN).with_io_rate(None);
    let scheduler = RepackScheduler::new(policy, limits);
    let operation = Arc::new(FsRepackOperation::new(store.clone()));
    assert_eq!(operation.inspect().unwrap().loose_objects, 1);

    started_handle(scheduler.schedule_if_needed(operation).unwrap())
        .wait()
        .unwrap();

    let inventory = FsRepackOperation::new(store.clone()).inspect().unwrap();
    assert_eq!(inventory.loose_objects, 0);
    let loaded = store.get_blob(&hash).unwrap().unwrap();
    assert_eq!(loaded.content(), blob.content());
    assert_eq!(loaded.hash(), hash);
}

#[test]
fn deliberately_corrupted_repack_output_is_rejected_before_cutover() {
    let (_temp, store) = create_store();
    let blob = Blob::from("authoritative bytes must survive rejection");
    let hash = blob.hash();
    store
        .put_blobs_packed(vec![(hash, blob.content().to_vec())])
        .unwrap();
    let before = direct_pack_names(store.root());

    let corrupt = Arc::new(FsRepackOperation::new(store.clone()).with_corrupted_output());
    let error = started_handle(scheduler(None).repack_now(corrupt).unwrap())
        .wait()
        .unwrap_err();
    assert!(
        matches!(error, RepackError::Operation(ref message)
            if message.contains("compact frame checksum mismatch")
                || message.contains("object corruption")),
        "typed-hash validation should reject the output, got {error:?}"
    );
    assert_eq!(direct_pack_names(store.root()), before);
    store.clear_recent_object_caches();
    assert_eq!(
        store.get_blob(&hash).unwrap().unwrap().content(),
        blob.content()
    );

    let valid = Arc::new(FsRepackOperation::new(store.clone()));
    started_handle(scheduler(None).repack_now(valid).unwrap())
        .wait()
        .unwrap();
    assert_eq!(store.get_blob(&hash).unwrap().unwrap().hash(), hash);
}

#[test]
fn corrupted_compact_metadata_frame_is_rejected_before_cutover() {
    let (_temp, store) = create_store();
    let tree = Tree::from_entries(Vec::new());
    let tree_hash = tree.hash();
    let state = State::new(
        tree_hash,
        Vec::new(),
        Attribution::human(Principal::new("Compact", "compact@example.com")),
    );
    let state_id = state.id();
    let mut builder = PackBuilder::new(CompressionConfig::disabled());
    builder.add_id(
        PackObjectId::Hash(tree_hash),
        ObjectType::Tree,
        rmp_serde::to_vec_named(&tree).unwrap(),
    );
    builder.add_id(
        PackObjectId::StateId(state_id),
        ObjectType::State,
        rmp_serde::to_vec_named(&state).unwrap(),
    );
    let (pack, index, _) = builder.build().unwrap();
    store.install_pack(&pack, &index).unwrap();
    let before = direct_pack_names(store.root());

    let corrupt = Arc::new(FsRepackOperation::new(store.clone()).with_corrupted_output());
    let error = started_handle(scheduler(None).repack_now(corrupt).unwrap())
        .wait()
        .unwrap_err();

    assert!(
        matches!(error, RepackError::Operation(ref message) if message.contains("compact frame checksum mismatch")),
        "whole-frame verification should reject corrupt compact metadata, got {error:?}"
    );
    assert_eq!(direct_pack_names(store.root()), before);
    store.clear_recent_object_caches();
    assert_eq!(store.get_tree(&tree_hash).unwrap().unwrap(), tree);
    assert_eq!(store.get_state(&state_id).unwrap().unwrap().id(), state_id);
}
