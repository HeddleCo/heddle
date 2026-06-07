// SPDX-License-Identifier: Apache-2.0
use chrono::{TimeZone, Utc};
use tempfile::TempDir;

use super::{
    FsStore, LooseObjectWriteMode,
    fs_paths::{blobs_dir, hash_path, packs_dir},
};
use crate::{
    object::{
        Action, Attribution, Blob, ChangeId, ContentHash, Operation, Principal, State, Tree,
        TreeEntry,
    },
    store::{
        HeddleError, ObjectStore,
        atomic::temp_path,
        compression::CompressionConfig,
        pack::{ObjectType as PackObjectType, PackBuilder, PackObjectId},
    },
};

fn create_test_store() -> (TempDir, FsStore) {
    let temp_dir = TempDir::new().unwrap();
    let heddle_dir = temp_dir.path().join(".heddle");
    let store = FsStore::new(&heddle_dir);
    store.init().unwrap();
    (temp_dir, store)
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
        state.change_id,
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
        rmp_serde::to_vec_named(&tree).unwrap(),
    );
    builder.add_id(
        PackObjectId::ChangeId(state.change_id),
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
        store.get_state(&state.change_id).unwrap().unwrap().intent,
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
    assert_eq!(retrieved.get("foo.txt").unwrap().hash, blob_hash);
}

#[test]
fn test_state_roundtrip() {
    let (_temp, store) = create_test_store();

    let tree_hash = ContentHash::compute(b"tree");
    let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
    let state = State::new(tree_hash, vec![], attribution).with_intent("Test state");

    store.put_state(&state).unwrap();

    let retrieved = store.get_state(&state.change_id).unwrap().unwrap();
    assert_eq!(retrieved.change_id, state.change_id);
    assert_eq!(retrieved.intent, Some("Test state".to_string()));
}

/// #564 step 1: a non-UTF8 git commit message (latin-1 `café` = `caf\xe9`)
/// must survive a real store→load round-trip byte-identically. This is why
/// `raw_message` is `Vec<u8>` and not `String`.
#[test]
fn test_state_roundtrip_preserves_non_utf8_raw_message() {
    let (_temp, store) = create_test_store();

    let raw = b"caf\xe9\n".to_vec();
    assert!(String::from_utf8(raw.clone()).is_err(), "fixture must be non-UTF8");

    let tree_hash = ContentHash::compute(b"tree");
    let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
    let state = State::new(tree_hash, vec![], attribution).with_raw_message(&raw);

    store.put_state(&state).unwrap();

    let retrieved = store.get_state(&state.change_id).unwrap().unwrap();
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

#[test]
fn test_recent_blob_cache_does_not_hide_deleted_loose_object() {
    let (_temp, store) = create_test_store();

    let blob = Blob::from("cached content");
    let hash = store.put_blob(&blob).unwrap();

    let path = hash_path(&blobs_dir(store.root()), &hash);
    std::fs::remove_file(path).unwrap();

    let retrieved = store.get_blob(&hash).unwrap();
    assert!(retrieved.is_none());
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
        .join(format!("{}.state", state1.change_id.to_string_full()));
    std::fs::write(&swapped_path, rmp_serde::to_vec(&state2).unwrap()).unwrap();
    store.clear_recent_object_caches();

    let error = store
        .get_state(&state1.change_id)
        .expect_err("swapped state should be rejected");
    assert!(
        matches!(error, HeddleError::InvalidObject(message) if message.contains("state change_id mismatch"))
    );
}

#[test]
fn test_get_action_rejects_wrong_object_swap() {
    let (_temp, store) = create_test_store();

    let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
    let mut action1 = Action::new(
        None,
        ChangeId::generate(),
        Operation::Snapshot,
        "first action",
        attribution.clone(),
    )
    .with_timestamp(Utc.timestamp_opt(1_700_000_000, 0).unwrap());
    let mut action2 = Action::new(
        None,
        ChangeId::generate(),
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

    let invalid_tree = Tree::from_entries(vec![TreeEntry {
        name: "bad/name".to_string(),
        mode: crate::object::FileMode::Normal,
        entry_type: crate::object::EntryType::Blob,
        hash: ContentHash::compute(b"blob"),
    }]);
    let tree_hash = invalid_tree.hash();
    let tree_path = store
        .root
        .join("objects/trees")
        .join(&tree_hash.to_hex()[..2])
        .join(&tree_hash.to_hex()[2..]);
    let parent = tree_path.parent().unwrap();
    std::fs::create_dir_all(parent).unwrap();
    std::fs::write(&tree_path, rmp_serde::to_vec(&invalid_tree).unwrap()).unwrap();

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
        store
            .verified_loose_blobs
            .read()
            .unwrap()
            .get(&hash)
            .is_some(),
        "verified cache should pick up the hash after first probe"
    );

    // Drop the in-process verified cache and corrupt the file. This
    // is the post-crash state we're guarding against: cache empty,
    // file's bytes don't match the hash any more.
    *store.verified_loose_blobs.write().unwrap() =
        super::fs_store::RecentObjectCache::with_capacity(65_536);
    std::fs::write(&path, b"torn-write garbage").unwrap();

    let probed = store.loose_blob_path(&hash);
    assert!(
        probed.is_none(),
        "corrupted loose blob must not be served as canonical bytes"
    );
}
