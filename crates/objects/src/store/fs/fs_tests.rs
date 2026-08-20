// SPDX-License-Identifier: Apache-2.0
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use chrono::{TimeZone, Utc};
use heddle_format::compression::CompressionConfig;
use serde::Serialize;
use tempfile::TempDir;

use super::{
    FsStore, LooseObjectWriteMode,
    fs_paths::{blobs_dir, hash_path, packs_dir, state_path, trees_dir},
};
use crate::{
    fs_atomic::temp_path,
    object::{
        Action, Attribution, Blob, ContentHash, Operation, Principal, State, StateId, Tree,
        TreeEntry,
    },
    store::{
        ExternalObjectSource, HeddleError, ObjectStore, Result as StoreResult,
        pack::{
            ObjectType as PackObjectType, PackBuilder, PackIndex, PackObjectId,
            decode_tagged_entry_header,
        },
    },
    sync::RwLockExt,
};

struct CountingBlobSource {
    blob: Blob,
    reads: AtomicUsize,
}

impl ExternalObjectSource for CountingBlobSource {
    fn get_blob(&self, hash: &ContentHash) -> StoreResult<Option<Blob>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok((self.blob.hash() == *hash).then(|| self.blob.clone()))
    }

    fn get_tree(&self, _hash: &ContentHash) -> StoreResult<Option<Tree>> {
        Ok(None)
    }

    fn get_state(&self, _id: &StateId) -> StoreResult<Option<State>> {
        Ok(None)
    }

    fn list_states(&self) -> StoreResult<Vec<StateId>> {
        Ok(Vec::new())
    }
}

fn create_test_store() -> (TempDir, FsStore) {
    let temp_dir = TempDir::new().unwrap();
    let heddle_dir = temp_dir.path().join(".heddle");
    let store = FsStore::new(&heddle_dir);
    store.init().unwrap();
    (temp_dir, store)
}

#[test]
fn external_blob_hits_are_memory_cached_without_native_materialization() {
    let (_temp, mut store) = create_test_store();
    let blob = Blob::from("authoritative Git bytes");
    let hash = blob.hash();
    let source = Arc::new(CountingBlobSource {
        blob: blob.clone(),
        reads: AtomicUsize::new(0),
    });
    store.set_external_source(source.clone());

    assert_eq!(store.get_blob(&hash).unwrap(), Some(blob.clone()));
    assert_eq!(store.get_blob(&hash).unwrap(), Some(blob.clone()));
    assert!(store.has_blob(&hash).unwrap());
    assert_eq!(
        store.blob_size(&hash).unwrap(),
        Some(blob.content().len() as u64)
    );
    assert_eq!(
        store.get_blob_bytes(&hash).unwrap().unwrap().as_ref(),
        blob.content()
    );
    assert_eq!(
        source.reads.load(Ordering::Relaxed),
        1,
        "warm authoritative reads must stay in the in-process cache"
    );
    assert!(
        !hash_path(&blobs_dir(store.root()), &hash).exists(),
        "read-through cache hits must not create native loose objects"
    );
}

#[test]
fn native_blob_wins_over_external_source_without_external_io() {
    let (_temp, mut store) = create_test_store();
    let blob = Blob::from("Heddle-native bytes");
    let hash = store.put_blob(&blob).unwrap();
    let source = Arc::new(CountingBlobSource {
        blob: blob.clone(),
        reads: AtomicUsize::new(0),
    });
    store.set_external_source(source.clone());
    store.clear_recent_caches();

    assert_eq!(store.get_blob(&hash).unwrap(), Some(blob.clone()));
    store.clear_recent_caches();
    assert!(store.has_blob(&hash).unwrap());
    assert_eq!(
        store.blob_size(&hash).unwrap(),
        Some(blob.content().len() as u64)
    );
    assert_eq!(source.reads.load(Ordering::Relaxed), 0);
}

#[test]
fn native_pack_blob_can_be_promoted_when_external_source_also_has_it() {
    let (_temp, mut store) = create_test_store();
    let blob = Blob::from("shared native and external bytes");
    let hash = blob.hash();
    let mut builder = PackBuilder::new(CompressionConfig::disabled());
    builder.add(hash, PackObjectType::Blob, blob.content().to_vec());
    let (pack_data, index_data, _) = builder.build().unwrap();
    store.install_pack(&pack_data, &index_data).unwrap();

    let source = Arc::new(CountingBlobSource {
        blob,
        reads: AtomicUsize::new(0),
    });
    store.set_external_source(source.clone());
    store.clear_recent_caches();

    assert!(store.loose_blob_path(&hash).is_none());
    assert!(store.promote_to_loose_uncompressed(&hash).unwrap());
    assert!(store.loose_blob_path(&hash).is_some());
    assert_eq!(source.reads.load(Ordering::Relaxed), 0);
}

#[test]
fn test_blob_roundtrip() {
    let (_temp, store) = create_test_store();

    let blob = Blob::from("hello world");
    let hash = store.put_blob(&blob).unwrap();

    let retrieved = store.get_blob(&hash).unwrap().unwrap();
    assert_eq!(retrieved.content(), blob.content());
}

#[test]
fn test_default_loose_object_write_mode_is_durable_outside_snapshot_batch() {
    let (_temp, store) = create_test_store();

    assert_eq!(
        store.loose_object_write_mode(),
        LooseObjectWriteMode::Durable
    );

    let blob = Blob::from("durable default");
    store.put_blob(&blob).unwrap();

    assert_eq!(store.pending_directory_sync_count(), 0);
}

#[test]
fn test_durable_loose_object_write_mode_does_not_queue_directory_syncs() {
    let temp_dir = TempDir::new().unwrap();
    let heddle_dir = temp_dir.path().join(".heddle");
    let mut store = FsStore::new(&heddle_dir);
    store.set_loose_object_write_mode(LooseObjectWriteMode::Durable);
    store.init().unwrap();

    let blob = Blob::from("durable sync");
    store.put_blob(&blob).unwrap();

    assert_eq!(store.pending_directory_sync_count(), 0);
}

#[test]
fn test_snapshot_write_batch_defers_directory_sync_until_flush() {
    let (_temp, store) = create_test_store();

    store.begin_snapshot_write_batch().unwrap();

    let blob = Blob::from("batched sync");
    let hash = store.put_blob(&blob).unwrap();

    assert_eq!(store.pending_directory_sync_count(), 1);
    assert!(store.get_blob(&hash).unwrap().is_some());

    store.flush_snapshot_write_batch().unwrap();
    assert_eq!(store.pending_directory_sync_count(), 0);
}

#[test]
fn overlapping_snapshot_batch_flush_is_independently_durable() {
    let (_temp, store) = create_test_store();

    store.begin_snapshot_write_batch().unwrap();
    store.begin_snapshot_write_batch().unwrap();
    store.put_blob(&Blob::from("overlapping batch")).unwrap();
    assert_eq!(store.pending_directory_sync_count(), 1);

    // One preparer can proceed to its commit section without waiting for the
    // other preparer to close its batch.
    store.flush_snapshot_write_batch().unwrap();
    assert_eq!(store.pending_directory_sync_count(), 0);

    store.abort_snapshot_write_batch();
}

#[test]
fn snapshot_batch_does_not_demote_an_unrelated_thread_writer() {
    let (_temp, store) = create_test_store();
    let store = std::sync::Arc::new(store);

    store.begin_snapshot_write_batch().unwrap();
    let writer = std::sync::Arc::clone(&store);
    std::thread::spawn(move || {
        writer
            .put_blob(&Blob::from("unrelated durable writer"))
            .unwrap();
    })
    .join()
    .unwrap();

    assert_eq!(
        store.pending_directory_sync_count(),
        0,
        "another thread must retain the store's durable write mode"
    );
    store.abort_snapshot_write_batch();
}

#[test]
fn test_abort_snapshot_write_batch_clears_pending_directory_syncs() {
    let (_temp, store) = create_test_store();

    store.begin_snapshot_write_batch().unwrap();
    store.put_blob(&Blob::from("aborted batch")).unwrap();
    assert_eq!(store.pending_directory_sync_count(), 1);

    store.abort_snapshot_write_batch();
    assert_eq!(store.pending_directory_sync_count(), 0);
}

#[test]
fn put_blobs_packed_writes_a_single_packfile_no_loose_blobs() {
    // ACID + perf invariant: bulk-installing N blobs as a pack must
    // touch exactly one .pack + .idx pair and *zero* loose blob
    // files. If a regression reverts to per-blob loose writes, the
    // snapshot hot path silently goes back to N×fsync.
    use crate::store::pack::PackObjectId;
    let (_temp, store) = create_test_store();

    let blobs: Vec<(ContentHash, Vec<u8>)> = (0..50)
        .map(|i| {
            let blob = Blob::from(format!("packed blob {i}"));
            (blob.hash(), blob.into_content())
        })
        .collect();

    store.put_blobs_packed(blobs.clone()).unwrap();

    // Loose-blobs dir is empty (everything went into a pack).
    let loose_count = std::fs::read_dir(blobs_dir(store.root()))
        .map(|iter| iter.count())
        .unwrap_or(0);
    assert_eq!(loose_count, 0, "expected zero loose blob shards");

    // Every input hash is reachable through the pack manager.
    for (hash, _) in &blobs {
        let id = PackObjectId::Hash(*hash);
        assert!(
            store.get_pack_object(&id).unwrap().is_some(),
            "blob {hash:?} not visible after put_blobs_packed",
        );
    }
}

#[test]
fn put_blobs_packed_skips_blobs_already_present() {
    // Pre-existing blobs (loose or packed) shouldn't be re-added to
    // the pack — the bulk path is meant to be idempotent on repeated
    // snapshots of the same content.
    let (_temp, store) = create_test_store();

    // Pre-populate one blob via the loose path.
    let preexisting = Blob::from("already here");
    let pre_hash = store.put_blob(&preexisting).unwrap();

    // Try to pack-install the same blob plus a fresh one.
    let fresh = Blob::from("brand new");
    let fresh_hash = fresh.hash();
    store
        .put_blobs_packed(vec![
            (pre_hash, preexisting.into_content()),
            (fresh_hash, fresh.into_content()),
        ])
        .unwrap();

    // Both reachable; the new one through the pack, the old one
    // still resolved via the loose object that was already there.
    assert!(store.get_blob(&pre_hash).unwrap().is_some());
    assert!(store.get_blob(&fresh_hash).unwrap().is_some());
}

#[test]
fn second_fs_store_sees_packs_installed_after_its_construction() {
    // Lightweight thread worktrees open their own `FsStore` against
    // the *same* `.heddle/` directory the main repo points at. Each
    // store's `PackManager` snapshots the disk at construction
    // time. When the worktree's store installs a new pack, the main
    // repo's already-open store has a stale in-memory pack list —
    // and previously failed lookups for objects in the new pack with
    // "object not found".
    //
    // This test exercises the recovery path: stale store sees the
    // new pack via `reload_packs_if_stale` triggered by its own
    // miss-and-retry path inside `get_blob`/`has_blob`.
    let temp_dir = TempDir::new().unwrap();
    let heddle_dir = temp_dir.path().join(".heddle");

    // Store A (the "main repo" store) opens first.
    let store_a = FsStore::new(&heddle_dir);
    store_a.init().unwrap();

    // Store B (the "worktree" store) opens second, against the
    // same directory.
    let store_b = FsStore::new(&heddle_dir);

    // Sanity: both stores see no blobs initially.
    let new_blob = Blob::from("worktree-installed content");
    let new_hash = new_blob.hash();
    assert!(!store_a.has_blob(&new_hash).unwrap());
    assert!(!store_b.has_blob(&new_hash).unwrap());

    // Store B installs a pack containing the blob.
    store_b
        .put_blobs_packed(vec![(new_hash, new_blob.clone().into_content())])
        .unwrap();

    // Store B can find it (its own pack manager just reloaded).
    assert!(store_b.has_blob(&new_hash).unwrap());

    // Store A's in-memory pack list is stale — but `has_blob` and
    // `get_blob` MUST recover via the on-miss reload path.
    assert!(
        store_a.has_blob(&new_hash).unwrap(),
        "stale pack manager must recover via reload-on-miss",
    );
    let recovered = store_a.get_blob(&new_hash).unwrap();
    assert_eq!(
        recovered.as_ref().map(|b| b.content().to_vec()),
        Some(new_blob.into_content())
    );
}

#[test]
fn list_states_sees_packs_installed_after_store_construction() {
    let temp_dir = TempDir::new().unwrap();
    let heddle_dir = temp_dir.path().join(".heddle");

    let store_a = FsStore::new(&heddle_dir);
    store_a.init().unwrap();
    let store_b = FsStore::new(&heddle_dir);

    assert!(store_a.list_states().unwrap().is_empty());

    let tree_hash = ContentHash::compute(b"packed tree");
    let attribution = Attribution::human(Principal::new("Pack Test", "pack@example.com"));
    let state = State::new(tree_hash, vec![], attribution).with_intent("packed state");

    let mut builder = PackBuilder::new(CompressionConfig::disabled());
    builder.add_id(
        PackObjectId::StateId(state.id()),
        PackObjectType::State,
        rmp_serde::to_vec_named(&state).unwrap(),
    );
    let (pack_data, index_data, _) = builder.build().unwrap();
    store_b.install_pack(&pack_data, &index_data).unwrap();

    assert_eq!(
        store_a.list_states().unwrap(),
        vec![state.id()],
        "stale pack manager must refresh before enumerating packed states"
    );
}

#[test]
fn put_blobs_packed_with_empty_input_is_a_noop() {
    // Snapshots that re-snapshot an unchanged worktree end up with
    // an empty pending list. Calling through must not write a
    // zero-object pack file (which would be wasted I/O) or fail.
    let (_temp, store) = create_test_store();
    store.put_blobs_packed(Vec::new()).unwrap();

    let pack_dir = store.root().join("objects").join("packs");
    let pack_count = std::fs::read_dir(&pack_dir)
        .map(|iter| iter.count())
        .unwrap_or(0);
    assert_eq!(pack_count, 0, "empty bulk install should not touch packs/");
}

#[test]
fn install_pack_streaming_publishes_via_durable_rename_and_loads_objects() {
    // Regression for streaming pack install crash-consistency: the old
    // path raw-renamed staged files without fsync and fell back to
    // in-place `fs::copy` on *any* rename error. The durable path must
    // still consume the stage files and make objects readable.
    let (_temp, store) = create_test_store();
    let stage = TempDir::new().unwrap();

    let blob = Blob::from("streaming durable pack blob");
    let blob_hash = blob.hash();
    let mut builder = PackBuilder::new(CompressionConfig::disabled());
    builder.add(blob_hash, PackObjectType::Blob, blob.clone().into_content());
    let (pack_data, index_data, _) = builder.build().unwrap();

    let staged_pack = stage.path().join("staged.pack");
    let staged_index = stage.path().join("staged.idx");
    std::fs::write(&staged_pack, &pack_data).unwrap();
    std::fs::write(&staged_index, &index_data).unwrap();

    let ids = store
        .install_pack_streaming(&staged_pack, &staged_index)
        .expect("streaming install");
    assert!(ids.contains(&PackObjectId::Hash(blob_hash)));
    assert!(
        !staged_pack.exists(),
        "durable publish must consume pack stage"
    );
    assert!(
        !staged_index.exists(),
        "durable publish must consume index stage"
    );

    let loaded = store.get_blob(&blob_hash).unwrap().expect("packed blob");
    assert_eq!(loaded.content(), blob.content());
}

#[test]
fn install_pack_batches_authoritative_loose_state_writes() {
    let (_temp, store) = create_test_store();
    let attribution = Attribution::human(Principal::new("Batch", "batch@example.com"));
    let first = State::new(
        ContentHash::compute(b"first tree"),
        vec![],
        attribution.clone(),
    );
    let second = State::new(
        ContentHash::compute(b"second tree"),
        vec![first.id()],
        attribution,
    );
    let states = [&first, &second];
    let mut builder = PackBuilder::new(CompressionConfig::disabled());
    for state in states {
        builder.add_id(
            PackObjectId::StateId(state.id()),
            PackObjectType::State,
            rmp_serde::to_vec_named(state).unwrap(),
        );
    }
    let (pack_data, index_data, _) = builder.build().unwrap();

    store.install_pack(&pack_data, &index_data).unwrap();

    assert_eq!(
        store.snapshot_batch_flush_count(),
        1,
        "one pack must publish every loose state behind one directory-sync flush"
    );
    assert_eq!(store.pending_directory_sync_count(), 0);
    for state in states {
        assert!(
            state_path(store.root(), &state.id()).is_file(),
            "packed state must retain its authoritative loose copy"
        );
        assert_eq!(store.get_state(&state.id()).unwrap().as_ref(), Some(state));
    }
}

#[test]
fn interrupted_clone_discards_only_short_pack_and_accepts_remote_repair() {
    let (_temp, store) = create_test_store();
    let blob = Blob::from("authoritative remote clone payload");
    let hash = blob.hash();
    let mut builder = PackBuilder::new(CompressionConfig::disabled());
    builder.add(hash, PackObjectType::Blob, blob.content().to_vec());
    let (pack_data, index_data, _) = builder.build().unwrap();
    store.install_pack(&pack_data, &index_data).unwrap();

    let pack_path = std::fs::read_dir(packs_dir(store.root()))
        .unwrap()
        .map(Result::unwrap)
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("pack"))
        .unwrap();
    std::fs::write(&pack_path, &pack_data[..pack_data.len() / 2]).unwrap();

    assert_eq!(store.discard_corrupt_clone_packs().unwrap(), 1);
    assert!(store.get_blob(&hash).unwrap().is_none());

    // The incremental pull installs only the pair that contained the corrupt
    // object. Reusing the authoritative bytes makes the object byte-identical.
    store.install_pack(&pack_data, &index_data).unwrap();
    let repaired = store.get_blob(&hash).unwrap().unwrap();
    assert_eq!(repaired.content(), blob.content());
}

#[test]
fn install_pack_rejects_hash_mismatch_without_partial_commit() {
    let (_temp, store) = create_test_store();

    let valid_blob = Blob::from("valid object that must not be committed");
    let valid_hash = valid_blob.hash();
    let claimed_hash = Blob::from("claimed object bytes").hash();
    let poisoned_bytes = b"different object bytes".to_vec();
    assert_ne!(
        ContentHash::compute_typed("blob", &poisoned_bytes),
        claimed_hash
    );

    let mut builder = PackBuilder::new(CompressionConfig::disabled());
    builder.add(
        valid_hash,
        PackObjectType::Blob,
        valid_blob.clone().into_content(),
    );
    builder.add(claimed_hash, PackObjectType::Blob, poisoned_bytes);
    let (pack_data, index_data, _) = builder.build().unwrap();

    let error = store
        .install_pack(&pack_data, &index_data)
        .expect_err("poisoned native pack must be rejected");
    assert!(
        matches!(error, HeddleError::Corruption { expected, .. } if expected == claimed_hash),
        "expected claimed-hash mismatch corruption, got {error:?}",
    );

    assert!(
        store.get_blob(&valid_hash).unwrap().is_none(),
        "valid entry before the poisoned entry must not be partially committed",
    );
    assert!(
        store.get_blob(&claimed_hash).unwrap().is_none(),
        "poisoned object must not be readable under its claimed hash",
    );
    let pack_count = std::fs::read_dir(packs_dir(store.root()))
        .map(|iter| iter.count())
        .unwrap_or(0);
    assert_eq!(pack_count, 0, "rejected pack must not commit pack files");
}

#[test]
fn install_pack_accepts_valid_mixed_native_pack() {
    let (_temp, store) = create_test_store();

    let blob = Blob::from("native pack blob");
    let blob_hash = blob.hash();
    let tree = Tree::from_entries(vec![TreeEntry::file("file.txt", blob_hash, false).unwrap()]);
    let tree_hash = tree.hash();
    let attribution = Attribution::human(Principal::new("Pack Test", "pack@example.com"));
    let state = State::new(tree_hash, vec![], attribution.clone()).with_intent("packed state");
    let mut action = Action::new(
        None,
        state.id(),
        Operation::Snapshot,
        "packed action",
        attribution,
    )
    .with_timestamp(Utc.timestamp_opt(1_700_000_000, 0).unwrap());
    let action_id = action.id();

    let mut builder = PackBuilder::new(CompressionConfig::disabled());
    builder.add(blob_hash, PackObjectType::Blob, blob.clone().into_content());
    builder.add(
        tree_hash,
        PackObjectType::Tree,
        tree.encode_canonical().unwrap(),
    );
    builder.add_id(
        PackObjectId::StateId(state.id()),
        PackObjectType::State,
        rmp_serde::to_vec_named(&state).unwrap(),
    );
    builder.add(
        *action_id.as_hash(),
        PackObjectType::Action,
        rmp_serde::to_vec_named(&action).unwrap(),
    );
    let (pack_data, index_data, _) = builder.build().unwrap();

    let ids = store.install_pack(&pack_data, &index_data).unwrap();
    assert_eq!(ids.len(), 4);
    assert_eq!(
        store.get_blob(&blob_hash).unwrap().unwrap().content(),
        blob.content(),
    );
    assert_eq!(
        store.get_tree(&tree_hash).unwrap().unwrap().entries(),
        tree.entries(),
    );
    assert_eq!(
        store.get_state(&state.id()).unwrap().unwrap().intent,
        Some("packed state".to_string()),
    );
    assert_eq!(
        store.get_action(&action_id).unwrap().unwrap().description,
        "packed action",
    );
}

#[cfg(feature = "zstd")]
#[test]
fn install_pack_accepts_valid_compressed_blob_pack() {
    let (_temp, store) = create_test_store();

    let content = b"compressible native pack blob\n".repeat(512);
    let blob = Blob::from(content);
    let blob_hash = blob.hash();
    let mut builder = PackBuilder::new(CompressionConfig {
        enabled: true,
        min_size: 0,
        max_delta_size: 0,
        ..CompressionConfig::default()
    });
    builder.add(blob_hash, PackObjectType::Blob, blob.clone().into_content());
    let (pack_data, index_data, stats) = builder.build().unwrap();
    assert!(
        stats.total_compressed < stats.total_uncompressed,
        "test pack must exercise the compressed get_object_bytes fallback",
    );

    let ids = store.install_pack(&pack_data, &index_data).unwrap();
    assert_eq!(ids, vec![PackObjectId::Hash(blob_hash)]);
    assert_eq!(
        store.get_blob(&blob_hash).unwrap().unwrap().content(),
        blob.content(),
    );
}

fn count_packs(store: &FsStore) -> usize {
    std::fs::read_dir(packs_dir(store.root()))
        .map(|iter| {
            iter.flatten()
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| ext == "pack")
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

fn count_pack_deltas(store: &FsStore) -> usize {
    let manager = store.pack_manager().read().unwrap();
    manager
        .pack_file_paths()
        .into_iter()
        .map(|(pack_path, index_path)| {
            let pack = std::fs::read(pack_path).unwrap();
            let index = PackIndex::from_bytes(&std::fs::read(index_path).unwrap()).unwrap();
            index
                .ids()
                .unwrap()
                .into_iter()
                .filter(|id| {
                    let offset = index.find(id).unwrap().unwrap() as usize;
                    decode_tagged_entry_header(&pack[offset..])
                        .is_ok_and(|header| header.obj_type == PackObjectType::Delta)
                })
                .count()
        })
        .sum()
}

fn similar_blobs(count: usize) -> Vec<(ContentHash, Vec<u8>)> {
    (0..count)
        .map(|version| {
            let mut data = vec![b'x'; 8 * 1024];
            data[version * 31] = b'A' + version as u8;
            let blob = Blob::from(data.clone());
            (blob.hash(), data)
        })
        .collect()
}

#[test]
fn snapshot_delta_search_policy_controls_pack_emission() {
    let (_off_temp, off_store) = create_test_store();
    let (_on_temp, mut on_store) = create_test_store();
    let blobs = similar_blobs(12);

    off_store.put_blobs_packed(blobs.clone()).unwrap();
    on_store.set_snapshot_delta_search(true);
    on_store.put_blobs_packed(blobs.clone()).unwrap();

    assert_eq!(count_pack_deltas(&off_store), 0);
    assert!(count_pack_deltas(&on_store) > 0);
    for (hash, expected) in blobs {
        let actual = on_store.get_blob(&hash).unwrap().unwrap();
        assert_eq!(actual.content(), expected);
    }
}

#[test]
fn gc_delta_search_policy_controls_pack_emission() {
    let (_off_temp, off_store) = create_test_store();
    let (_on_temp, on_store) = create_test_store();
    let blobs = similar_blobs(12);
    for (_, data) in &blobs {
        let blob = Blob::from(data.clone());
        off_store.put_blob(&blob).unwrap();
        on_store.put_blob(&blob).unwrap();
    }

    off_store.pack_objects(false).unwrap();
    on_store.pack_objects(true).unwrap();

    assert_eq!(count_pack_deltas(&off_store), 0);
    assert!(count_pack_deltas(&on_store) > 0);
    for (hash, expected) in blobs {
        let actual = on_store.get_blob(&hash).unwrap().unwrap();
        assert_eq!(actual.content(), expected);
    }
}

fn count_loose_objects(store: &FsStore) -> usize {
    // Loose objects are sharded into 2-char prefix subdirectories
    // (see `hash_path`), so count files recursively.
    fn count_files_recursive(dir: &std::path::Path) -> usize {
        let mut total = 0;
        if let Ok(iter) = std::fs::read_dir(dir) {
            for entry in iter.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    total += count_files_recursive(&path);
                } else if path.is_file() {
                    total += 1;
                }
            }
        }
        total
    }
    count_files_recursive(&blobs_dir(store.root()))
        + count_files_recursive(&trees_dir(store.root()))
}

/// GC's contract is to *consolidate* the object store: pack the loose
/// objects AND prune the now-redundant loose copies, so the ODB shrinks
/// rather than gaining a duplicate pack alongside the still-loose
/// originals. The pre-fix bug (heddle maintenance gc regression) left
/// every packed object ALSO loose, so the object store had more sources
/// to search and read commands ran ~2x slower. This test pins the
/// invariant: after `pack_objects` + `prune_loose_objects` no object is
/// simultaneously loose and packed.
#[test]
fn pack_objects_then_prune_leaves_no_loose_packed_duplicates() {
    let (_temp, store) = create_test_store();

    // Write a handful of loose blobs + trees.
    let mut hashes = Vec::new();
    for i in 0..16 {
        let blob = Blob::from(format!("gc-consolidation-blob-{i}"));
        hashes.push(store.put_blob(&blob).unwrap());
    }
    for (i, hash) in hashes.iter().take(4).enumerate() {
        let tree = Tree::from_entries(vec![
            TreeEntry::file(format!("file-{i}.txt"), *hash, false).unwrap(),
        ]);
        store.put_tree(&tree).unwrap();
    }

    let loose_before = count_loose_objects(&store);
    assert!(
        loose_before >= 20,
        "expected the test corpus to be written loose, got {loose_before}"
    );

    let (packed, _saved) = store.pack_objects(false).unwrap();
    assert!(packed >= 20, "pack_objects should pack the loose corpus");
    let (pruned, _freed) = store.prune_loose_objects().unwrap();
    assert!(
        pruned >= 20,
        "prune should remove the now-packed loose copies"
    );

    // The consolidation invariant: nothing is left both loose and packed.
    let loose_after = count_loose_objects(&store);
    assert_eq!(
        loose_after, 0,
        "after gc (pack + prune) the object store must not retain loose copies \
         of objects that now live in a pack (regression: pack-without-prune)"
    );

    // Every object is still readable (no data lost to the consolidation).
    for hash in &hashes {
        assert!(
            store.get_blob(hash).unwrap().is_some(),
            "blob {hash:?} must survive consolidation"
        );
    }
}

#[derive(Serialize)]
struct LegacyTreeV1ForTest {
    entries: Vec<LegacyTreeEntryV1ForTest>,
}

#[derive(Serialize)]
struct LegacyTreeEntryV1ForTest {
    name: String,
    mode: LegacyFileModeForTest,
    entry_type: LegacyEntryTypeForTest,
    hash: ContentHash,
}

#[derive(Serialize)]
enum LegacyFileModeForTest {
    Normal,
}

#[derive(Serialize)]
enum LegacyEntryTypeForTest {
    Blob,
}

fn legacy_tree_v1_bytes_for_test(name: &str, hash: ContentHash) -> Vec<u8> {
    rmp_serde::to_vec(&LegacyTreeV1ForTest {
        entries: vec![LegacyTreeEntryV1ForTest {
            name: name.to_string(),
            mode: LegacyFileModeForTest::Normal,
            entry_type: LegacyEntryTypeForTest::Blob,
            hash,
        }],
    })
    .unwrap()
}

#[test]
fn gc_preserves_and_repacks_loose_v2_tree_shadow_over_packed_v1_body() {
    let (_temp, store) = create_test_store();
    let blob_hash = ContentHash::compute(b"shadowed-tree-blob");
    let tree = Tree::from_entries(vec![TreeEntry::file("file.txt", blob_hash, false).unwrap()]);
    let tree_hash = tree.hash();
    let legacy_bytes = legacy_tree_v1_bytes_for_test("file.txt", blob_hash);
    let current_bytes = tree.encode_canonical().unwrap();

    let mut builder = PackBuilder::new(CompressionConfig::default());
    builder.add(tree_hash, PackObjectType::Tree, legacy_bytes);
    let (pack_data, index_data, _) = builder.build().unwrap();
    store.install_pack_files(&pack_data, &index_data).unwrap();
    store
        .put_tree_serialized(&current_bytes, tree_hash)
        .expect("write migrated loose V2 shadow");

    let tree_path = hash_path(&trees_dir(store.root()), &tree_hash);
    assert!(tree_path.exists(), "test must have a loose V2 shadow");
    assert!(store.get_tree(&tree_hash).unwrap().is_some());

    let (pruned, _) = store.prune_loose_objects().unwrap();
    assert_eq!(
        pruned, 0,
        "prune must keep a loose migrated V2 tree when the packed body differs"
    );
    assert!(tree_path.exists(), "loose V2 shadow must survive prune");

    store.pack_objects(false).unwrap();
    let Some((obj_type, packed_after)) = store
        .get_pack_object(&PackObjectId::Hash(tree_hash))
        .unwrap()
    else {
        panic!("tree should remain packed after GC");
    };
    assert_eq!(obj_type, PackObjectType::Tree);
    assert_eq!(
        packed_after, current_bytes,
        "GC must carry the loose migrated V2 body, not the old packed V1 body"
    );

    let (pruned, _) = store.prune_loose_objects().unwrap();
    assert_eq!(pruned, 1);
    assert!(
        !tree_path.exists(),
        "once the pack carries V2 bytes, the loose duplicate can be pruned"
    );
}

/// Running gc twice must not keep growing the pack count. The pre-fix
/// `pack_objects` always wrote a *new* pack of the loose objects without
/// consolidating the existing packs, so a repo that is gc'd repeatedly
/// accumulated packs and got slower on every run. After the fix a
/// second gc over an already-consolidated store is a no-op: it neither
/// re-packs already-packed content into yet another pack nor leaves the
/// pack count climbing.
#[test]
fn repeated_gc_does_not_grow_pack_count() {
    let (_temp, store) = create_test_store();

    for i in 0..16 {
        let blob = Blob::from(format!("repeated-gc-blob-{i}"));
        store.put_blob(&blob).unwrap();
    }

    store.pack_objects(false).unwrap();
    store.prune_loose_objects().unwrap();
    let packs_after_first = count_packs(&store);
    assert_eq!(
        packs_after_first, 1,
        "a single gc should consolidate the corpus into exactly one pack"
    );

    // Second gc with nothing loose: must not mint another pack.
    let (packed_second, _) = store.pack_objects(false).unwrap();
    store.prune_loose_objects().unwrap();
    let packs_after_second = count_packs(&store);
    assert_eq!(
        packed_second, 0,
        "a second gc with no loose objects must not re-pack already-packed content"
    );
    assert_eq!(
        packs_after_second, 1,
        "repeated gc must not grow the pack count (regression: unbounded pack accumulation)"
    );
}

/// The decisive regression: a store that ALREADY has a pack, then
/// accumulates new loose objects, then is gc'd. The pre-fix
/// `pack_objects` wrote the new loose objects into a *second* pack and
/// left the first pack in place, so the pack count grew 1 -> 2. Read
/// commands probe every pack linearly (`PackManager::get_object`), so on
/// a real repo each extra pack roughly doubled status time even after
/// the loose copies were pruned. GC must *consolidate*: fold every
/// object (loose + already-packed) into a single pack so the pack count
/// does not grow.
#[test]
fn gc_consolidates_into_single_pack_over_existing_pack() {
    let (_temp, store) = create_test_store();

    // Round 1: write objects and pack them -> one pack.
    for i in 0..12 {
        store.put_blob(&Blob::from(format!("round1-{i}"))).unwrap();
    }
    store.pack_objects(false).unwrap();
    store.prune_loose_objects().unwrap();
    assert_eq!(count_packs(&store), 1, "round 1 produces one pack");

    // Round 2: more loose objects land (a later snapshot), then gc again.
    let mut round2 = Vec::new();
    for i in 0..12 {
        round2.push(store.put_blob(&Blob::from(format!("round2-{i}"))).unwrap());
    }
    assert!(count_loose_objects(&store) >= 12);

    store.pack_objects(false).unwrap();
    store.prune_loose_objects().unwrap();

    assert_eq!(
        count_packs(&store),
        1,
        "gc must consolidate loose + existing pack into a SINGLE pack \
         (regression: a second gc minted a new pack, growing pack count and \
         slowing every object lookup)"
    );
    assert_eq!(
        count_loose_objects(&store),
        0,
        "gc must prune the loose copies it just packed"
    );

    // No object lost across the consolidation.
    for hash in &round2 {
        assert!(store.get_blob(hash).unwrap().is_some());
    }
    // And the round-1 objects, which only ever lived in the first pack,
    // must survive being folded into the consolidated pack.
    assert!(
        store
            .get_blob(&Blob::from("round1-0").hash())
            .unwrap()
            .is_some(),
        "objects from the pre-existing pack must survive consolidation"
    );
}

#[test]
fn test_blob_deduplication() {
    let (_temp, store) = create_test_store();

    let blob1 = Blob::from("same content");
    let blob2 = Blob::from("same content");

    let hash1 = store.put_blob(&blob1).unwrap();
    let hash2 = store.put_blob(&blob2).unwrap();

    assert_eq!(hash1, hash2);
}

#[test]
fn test_blob_not_found() {
    let (_temp, store) = create_test_store();

    let hash = ContentHash::compute(b"nonexistent");
    let result = store.get_blob(&hash).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_tree_roundtrip() {
    let (_temp, store) = create_test_store();

    let blob_hash = ContentHash::compute(b"file content");
    let tree = Tree::from_entries(vec![TreeEntry::file("foo.txt", blob_hash, false).unwrap()]);

    let hash = store.put_tree(&tree).unwrap();
    let retrieved = store.get_tree(&hash).unwrap().unwrap();

    assert_eq!(retrieved.entries().len(), 1);
    assert_eq!(
        retrieved.get("foo.txt").unwrap().blob_hash(),
        Some(blob_hash)
    );
}

#[test]
fn loose_htr4_resume_skips_the_encoded_prefix() {
    use crate::object::{TREE_HEADER_LEN, TreePageLimits};

    let (_temp, store) = create_test_store();
    let blob = ContentHash::compute(b"resume-leaf");
    let tree = Tree::from_entries(vec![
        TreeEntry::file("a.txt", blob, false).unwrap(),
        TreeEntry::file("b.txt", blob, true).unwrap(),
    ]);
    let hash = store.put_tree(&tree).unwrap();
    let encoded_len = store.get_tree_serialized(&hash).unwrap().unwrap().len() as u64;
    let mut reader = store.open_tree(&hash, None).unwrap().unwrap();
    let first = reader
        .next_page(TreePageLimits::new(1, usize::MAX).unwrap())
        .unwrap()
        .unwrap();
    let prefix_end = first.resume_cursor.byte_offset();
    drop(reader);

    let mut resumed = store
        .open_tree(&hash, Some(&first.resume_cursor))
        .unwrap()
        .unwrap();
    let rest = resumed
        .next_page(TreePageLimits::new(8, usize::MAX).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(rest.entries[0].name(), "b.txt");
    resumed.finish_and_verify().unwrap();
    let suffix = encoded_len.saturating_sub(prefix_end);
    assert!(resumed.bytes_read() <= TREE_HEADER_LEN as u64 + suffix);
    assert!(resumed.bytes_read() < encoded_len);
}

#[test]
fn test_state_roundtrip() {
    let (_temp, store) = create_test_store();

    let tree_hash = ContentHash::compute(b"tree");
    let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
    let state = State::new(tree_hash, vec![], attribution).with_intent("Test state");

    store.put_state(&state).unwrap();

    let retrieved = store.get_state(&state.id()).unwrap().unwrap();
    assert_eq!(retrieved.state_id, state.state_id);
    assert_eq!(retrieved.intent, Some("Test state".to_string()));
}

/// #564 step 1: a non-UTF8 git commit message (latin-1 `café` = `caf\xe9`)
/// must survive a real store→load round-trip byte-identically. This is why
/// `raw_message` is `Vec<u8>` and not `String`.
#[test]
fn test_state_roundtrip_preserves_non_utf8_raw_message() {
    let (_temp, store) = create_test_store();

    let raw = b"caf\xe9\n".to_vec();
    assert!(
        String::from_utf8(raw.clone()).is_err(),
        "fixture must be non-UTF8"
    );

    let tree_hash = ContentHash::compute(b"tree");
    let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
    let state = State::new(tree_hash, vec![], attribution).with_raw_message(&raw);

    store.put_state(&state).unwrap();

    let retrieved = store.get_state(&state.id()).unwrap().unwrap();
    assert_eq!(
        retrieved.raw_message.as_deref(),
        Some(raw.as_slice()),
        "raw message bytes must round-trip verbatim through the store"
    );
}

#[test]
fn test_list_states() {
    let (_temp, store) = create_test_store();

    let tree_hash = ContentHash::compute(b"tree");
    let attribution = Attribution::human(Principal::new("Test", "test@example.com"));

    let state1 = State::new(tree_hash, vec![], attribution.clone());
    let state2 = State::new(tree_hash, vec![], attribution);

    store.put_state(&state1).unwrap();
    store.put_state(&state2).unwrap();

    let states = store.list_states().unwrap();
    assert_eq!(states.len(), 2);
}

#[test]
fn test_has_blob() {
    let (_temp, store) = create_test_store();

    let blob = Blob::from("test");
    let hash = store.put_blob(&blob).unwrap();

    assert!(store.has_blob(&hash).unwrap());
    assert!(!store.has_blob(&ContentHash::compute(b"other")).unwrap());
}

#[test]
fn test_empty_blob() {
    let (_temp, store) = create_test_store();

    let blob = Blob::from("");
    let hash = store.put_blob(&blob).unwrap();

    let retrieved = store.get_blob(&hash).unwrap().unwrap();
    assert_eq!(retrieved.content(), b"");
}

#[test]
fn test_large_blob() {
    let (_temp, store) = create_test_store();

    // Create a blob larger than MMAP_THRESHOLD_BYTES (256KB)
    let large_content = vec![b'a'; 300 * 1024];
    let blob = Blob::from(large_content.as_slice());
    let hash = store.put_blob(&blob).unwrap();

    let retrieved = store.get_blob(&hash).unwrap().unwrap();
    assert_eq!(retrieved.content(), large_content.as_slice());
}

/// SECURITY regression: a purged blob must be gone from the in-process
/// cache too, not just from disk. `purge` (redaction) deletes the loose
/// bytes and then drops the cache; after that sequence a long-lived
/// process must neither serve the content nor report it present. This
/// mirrors the store-level half of the redaction `purge_blob` path
/// (delete loose bytes + `clear_recent_caches`).
#[test]
fn test_recent_blob_cache_evicted_on_purge_neither_served_nor_present() {
    let (_temp, store) = create_test_store();

    let blob = Blob::from("cached content");
    let hash = store.put_blob(&blob).unwrap();

    // Sanity: the blob is cached (put-time populate) and present.
    assert!(store.get_blob(&hash).unwrap().is_some());
    assert!(store.has_blob(&hash).unwrap());

    // Purge sequence: delete the loose bytes, then drop the cache the
    // way `remove_loose_blob_bytes` does. Deleting the file alone is
    // NOT enough — the cache would keep serving the destroyed content.
    let path = hash_path(&blobs_dir(store.root()), &hash);
    std::fs::remove_file(&path).unwrap();
    store.clear_recent_caches();

    // Post-purge the blob must be neither served nor reported present.
    assert!(
        store.get_blob(&hash).unwrap().is_none(),
        "purged blob must not be served from cache after its bytes are deleted"
    );
    assert!(
        !store.has_blob(&hash).unwrap(),
        "purged blob must not be reported present after its bytes are deleted"
    );
}

/// The targeted single-hash eviction (`evict_recent_blob`) must also
/// stop a purged blob from being served/reported present, without
/// requiring a full cache flush. Other cached blobs must survive.
#[test]
fn test_evict_recent_blob_removes_only_target() {
    let (_temp, store) = create_test_store();

    let purged = Blob::from("secret to destroy");
    let kept = Blob::from("innocent bystander");
    let purged_hash = store.put_blob(&purged).unwrap();
    let kept_hash = store.put_blob(&kept).unwrap();

    // Delete the purged blob's loose bytes and evict just that hash.
    let path = hash_path(&blobs_dir(store.root()), &purged_hash);
    std::fs::remove_file(&path).unwrap();
    store.evict_recent_blob(&purged_hash);

    assert!(
        store.get_blob(&purged_hash).unwrap().is_none(),
        "evicted+deleted blob must not be served from cache"
    );
    assert!(
        !store.has_blob(&purged_hash).unwrap(),
        "evicted+deleted blob must not be reported present"
    );
    // The unrelated blob is still cached and served.
    assert!(
        store.get_blob(&kept_hash).unwrap().is_some(),
        "single-hash eviction must not drop unrelated cached blobs"
    );
}

#[test]
fn test_orphaned_temp_blob_file_is_ignored() {
    let (_temp, store) = create_test_store();

    let blob = Blob::from("orphan temp");
    let hash = blob.hash();
    let path = hash_path(&blobs_dir(store.root()), &hash);
    let temp = temp_path(&path);

    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&temp, b"partial blob data").unwrap();

    assert!(!store.has_blob(&hash).unwrap());
    assert!(store.get_blob(&hash).unwrap().is_none());
    assert!(!store.list_blobs().unwrap().contains(&hash));
}

#[test]
fn test_truncated_blob_file_is_rejected() {
    let (_temp, store) = create_test_store();

    let blob = Blob::from("full blob payload");
    let hash = blob.hash();
    let path = hash_path(&blobs_dir(store.root()), &hash);
    let parent = path.parent().unwrap();
    std::fs::create_dir_all(parent).unwrap();
    std::fs::write(&path, b"truncated").unwrap();

    let error = store
        .get_blob(&hash)
        .expect_err("truncated blob should be rejected");
    assert!(matches!(
        error,
        HeddleError::Corruption { .. } | HeddleError::InvalidObject(_)
    ));
}

#[test]
fn test_get_state_rejects_wrong_object_swap() {
    let (_temp, store) = create_test_store();

    let tree_hash = ContentHash::compute(b"tree");
    let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
    let state1 = State::new(tree_hash, vec![], attribution.clone());
    let state2 = State::new(tree_hash, vec![], attribution);

    store.put_state(&state1).unwrap();
    store.put_state(&state2).unwrap();

    let swapped_path = store
        .root
        .join("objects/states")
        .join(format!("{}.state", state1.id().to_string_full()));
    std::fs::write(&swapped_path, rmp_serde::to_vec(&state2).unwrap()).unwrap();
    store.clear_recent_object_caches();

    let error = store
        .get_state(&state1.id())
        .expect_err("swapped state should be rejected");
    assert!(
        matches!(error, HeddleError::InvalidObject(message) if message.contains("state id mismatch"))
    );
}

#[test]
fn test_get_action_rejects_wrong_object_swap() {
    let (_temp, store) = create_test_store();

    let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
    let mut action1 = Action::new(
        None,
        StateId::from_bytes([3; 32]),
        Operation::Snapshot,
        "first action",
        attribution.clone(),
    )
    .with_timestamp(Utc.timestamp_opt(1_700_000_000, 0).unwrap());
    let mut action2 = Action::new(
        None,
        StateId::from_bytes([4; 32]),
        Operation::Snapshot,
        "second action",
        attribution,
    )
    .with_timestamp(Utc.timestamp_opt(1_700_000_001, 0).unwrap());

    let action1_id = store.put_action(&mut action1).unwrap();
    store.put_action(&mut action2).unwrap();

    let swapped_path = store
        .root
        .join("actions")
        .join(format!("{}.action", action1_id.as_hash().to_hex()));
    std::fs::write(&swapped_path, rmp_serde::to_vec(&action2).unwrap()).unwrap();

    let error = store
        .get_action(&action1_id)
        .expect_err("swapped action should be rejected");
    assert!(
        matches!(error, HeddleError::InvalidObject(message) if message.contains("action id mismatch"))
    );
}

#[test]
fn test_get_tree_rejects_invalid_deserialized_entry_name() {
    let (_temp, store) = create_test_store();
    let tree = Tree::from_entries(vec![
        TreeEntry::file("ab", ContentHash::compute(b"blob"), false).unwrap(),
    ]);
    let tree_hash = tree.hash();
    let mut bytes = tree.encode_canonical().unwrap();
    let name_at = crate::object::TREE_HEADER_LEN + 4 + 1 + 1 + 2;
    bytes[name_at + 1] = b'/';
    let tree_path = store
        .root
        .join("objects/trees")
        .join(&tree_hash.to_hex()[..2])
        .join(&tree_hash.to_hex()[2..]);
    let parent = tree_path.parent().unwrap();
    std::fs::create_dir_all(parent).unwrap();
    std::fs::write(&tree_path, bytes).unwrap();

    let error = store
        .get_tree(&tree_hash)
        .expect_err("invalid tree should be rejected");
    assert!(matches!(error, HeddleError::InvalidTreeEntry(_)));
}

/// Belt-and-suspenders for the no-fsync cache-mirror promote path:
/// when a loose blob's on-disk bytes don't hash to the expected
/// content hash (the failure mode of a torn write after a crash),
/// `loose_blob_path` must report `None` so the caller re-promotes
/// from the authoritative pack instead of silently materializing
/// garbage. This is what makes `AtomicWriteMode::NoSync` safe.
#[test]
fn loose_blob_path_rejects_torn_cache_mirror() {
    let (_temp, store) = create_test_store();

    // Put a blob via the normal path → ends up loose+uncompressed.
    let blob = Blob::from("authoritative bytes");
    let hash = store.put_blob(&blob).unwrap();

    let path = hash_path(&blobs_dir(store.root()), &hash);
    assert!(path.exists(), "blob should be loose on disk");

    // Sanity: first call hashes the bytes, finds them valid, returns
    // the path *and* records the hash in the verified set.
    let probed = store.loose_blob_path(&hash);
    assert_eq!(probed, Some(path.clone()));
    assert!(
        store.verified_loose_blobs.read().unwrap().contains(&hash),
        "verified cache should pick up the hash after first probe"
    );

    // Drop the in-process verified cache and corrupt the file. This
    // is the post-crash state we're guarding against: cache empty,
    // file's bytes don't match the hash any more.
    *store.verified_loose_blobs.write_or_poisoned() =
        super::fs_store::RecentObjectCache::with_capacity(65_536);
    std::fs::write(&path, b"torn-write garbage").unwrap();

    let probed = store.loose_blob_path(&hash);
    assert!(
        probed.is_none(),
        "corrupted loose blob must not be served as canonical bytes"
    );
}

/// The byte budget must evict cold entries once cumulative cached bytes
/// exceed the cap, so a read-only workload streaming many blobs can't
/// retain `capacity × max-entry-bytes` of deep-cloned Vecs.
#[test]
fn test_recent_object_cache_byte_budget_evicts_cold_entry() {
    // Large per-entry count cap, tiny byte budget: eviction is driven
    // purely by bytes. Each value is `len` bytes.
    let mut cache = super::fs_store::RecentObjectCache::<u32, Vec<u8>>::with_byte_budget(
        1_000,
        100,
        |v: &Vec<u8>| v.len(),
    );

    cache.insert(1, vec![0u8; 40]); // total 40
    cache.insert(2, vec![0u8; 40]); // total 80
    // Touch key 1 so the clock gives it a second chance; key 2 is cold.
    assert!(cache.contains(&1));
    cache.get(&1);
    cache.insert(3, vec![0u8; 40]); // total would be 120 > 100 → evict key 2

    assert!(cache.contains(&1), "recently-touched entry must survive");
    assert!(
        !cache.contains(&2),
        "cold entry must be evicted when the byte budget is exceeded"
    );
    assert!(
        cache.contains(&3),
        "freshly inserted entry must be retained"
    );
}

/// A single entry larger than the whole budget is kept (soft cap): the
/// budget bounds aggregate retention, and the per-entry size gate is
/// enforced upstream, not here.
#[test]
fn test_recent_object_cache_byte_budget_keeps_single_oversize_entry() {
    let mut cache = super::fs_store::RecentObjectCache::<u32, Vec<u8>>::with_byte_budget(
        1_000,
        100,
        |v: &Vec<u8>| v.len(),
    );
    cache.insert(1, vec![0u8; 500]);
    assert!(
        cache.contains(&1),
        "a lone oversize entry is retained; the budget is a soft aggregate cap"
    );
}

#[test]
fn test_recent_object_cache_all_hot_residents_do_not_reject_new_entry() {
    let mut cache = super::fs_store::RecentObjectCache::<u32, ()>::with_capacity(2);
    cache.insert(1, ());
    cache.insert(2, ());
    cache.get(&1);
    cache.get(&2);

    cache.insert(3, ());

    assert!(
        cache.contains(&3),
        "the admitted entry must survive its first pass"
    );
    assert_ne!(
        cache.contains(&1),
        cache.contains(&2),
        "exactly one prior resident must be evicted"
    );
}

/// Cache hits at the production verification-cache scale must remain shared
/// operations while still influencing second-chance eviction order.
#[test]
fn test_recent_object_cache_large_shared_read_gets_second_chance() {
    const CAPACITY: u32 = 65_536;
    let mut cache = super::fs_store::RecentObjectCache::<u32, ()>::with_capacity(CAPACITY as usize);
    for key in 0..CAPACITY {
        cache.insert(key, ());
    }

    let shared = &cache;
    for _ in 0..1_000 {
        assert!(shared.get(&0).is_some());
    }

    cache.insert(CAPACITY, ());
    assert!(cache.contains(&0), "hot entry must remain cached");
    assert!(
        !cache.contains(&1),
        "oldest untouched entry must be evicted"
    );
    assert!(cache.contains(&CAPACITY), "new entry must be retained");
}
