// SPDX-License-Identifier: Apache-2.0
//! ObjectStore implementation for FsStore.

use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
};

use fs2::FileExt;
use heddle_format::compression::{header_uncompressed_size, is_compressed};
use tracing::{debug, instrument, trace};

use super::{
    FsStore,
    fs_io::{list_hashes_from_dir, read_file_bytes, read_file_header},
    fs_paths::{
        action_path, actions_dir, blobs_dir, hash_path, redaction_path, redactions_dir,
        state_attachment_index_lock_path, state_attachment_index_path, state_attachment_path,
        state_attachments_dir, state_path, state_visibility_dir, state_visibility_path, states_dir,
        trees_dir,
    },
};
use crate::{
    object::{
        Action, ActionId, Blob, ContentHash, State, StateAttachment, StateAttachmentId, StateId,
        Tree,
    },
    store::{
        HeddleError, ObjectStore, Result, codec,
        pack::{ObjectType, PackManager, PackObjectId},
    },
};

/// Bytes we read off disk to recover a blob's uncompressed size.
/// Must cover the 9-byte modern header **plus** the 4-byte ZSTD
/// magic that `header_uncompressed_size` uses to disambiguate
/// modern from legacy (5-byte) headers — without the magic in the
/// peek buffer the lookup silently returns the on-disk byte length
/// instead of the recorded uncompressed size, which left `stat`
/// reporting the compressed size of every loose blob.
const BLOB_HEADER_PEEK: usize = 13;

fn validate_loaded_tree(tree: Tree) -> Result<Tree> {
    tree.validate()?;
    Ok(tree)
}

fn validate_blob_bytes(data: &[u8], hash: ContentHash) -> Result<()> {
    let mut hasher = ContentHash::typed_hasher("blob", data.len() as u64);
    hasher.update(data);
    let found = ContentHash::from_bytes(hasher.finalize().into());
    if found != hash {
        return Err(HeddleError::Corruption {
            expected: hash,
            found,
        });
    }

    Ok(())
}

fn validate_tree_serialized(data: &[u8], hash: ContentHash) -> Result<Tree> {
    let tree = codec::decode_tree_serialized(data)?;
    let tree = validate_loaded_tree(tree)?;
    let found = tree.hash();
    if found != hash {
        return Err(HeddleError::Corruption {
            expected: hash,
            found,
        });
    }

    Ok(tree)
}

fn validate_loaded_state(requested_id: &StateId, mut state: State) -> Result<State> {
    let computed = state.id();
    if computed != *requested_id {
        return Err(HeddleError::InvalidObject(format!(
            "state id mismatch: requested {requested_id}, computed {computed}"
        )));
    }
    state.state_id = computed;
    Ok(state)
}

fn validate_state_serialized(data: &[u8], id: StateId) -> Result<State> {
    let state: State = rmp_serde::from_slice(data)?;
    validate_loaded_state(&id, state)
}

fn validate_loaded_action(requested_id: &ActionId, action: Action) -> Result<Action> {
    let found_id = action.compute_id();
    if found_id != *requested_id {
        return Err(HeddleError::InvalidObject(format!(
            "action id mismatch: requested {}, found {}",
            requested_id, found_id
        )));
    }

    Ok(action)
}

fn validate_action_serialized(data: &[u8], id: ActionId) -> Result<Action> {
    let action: Action = rmp_serde::from_slice(data)?;
    validate_loaded_action(&id, action)
}

impl FsStore {
    fn with_state_attachment_index_lock<T>(
        &self,
        state: &StateId,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let path = state_attachment_index_lock_path(&self.root, state);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.lock_exclusive()?;
        let result = operation();
        file.unlock()?;
        result
    }

    fn rebuild_state_attachment_index(&self, state: &StateId) -> Result<Vec<StateAttachmentId>> {
        #[cfg(test)]
        fs::write(
            state_attachment_index_path(&self.root, state).with_extension("rebuild-marker"),
            b"rebuilt",
        )?;
        let mut ids = Vec::new();
        let dir = state_attachments_dir(&self.root, state);
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries {
                let attachment: StateAttachment = rmp_serde::from_slice(&fs::read(entry?.path())?)?;
                if attachment.state_id != *state {
                    return Err(HeddleError::InvalidObject(
                        "state attachment stored under wrong state".to_string(),
                    ));
                }
                ids.push(attachment.id());
            }
        }
        if let Ok(manager) = self.pack_manager().read() {
            for pack_id in manager.list_all_ids()? {
                let PackObjectId::Hash(hash) = pack_id else {
                    continue;
                };
                let Some((ObjectType::StateAttachment, bytes)) =
                    manager.get_hashed_object(&hash)?
                else {
                    continue;
                };
                let attachment: StateAttachment = rmp_serde::from_slice(&bytes)?;
                if attachment.state_id == *state {
                    ids.push(attachment.id());
                }
            }
        }
        ids.sort();
        ids.dedup();
        let path = state_attachment_index_path(&self.root, state);
        self.write_loose_object_atomic(&path, &rmp_serde::to_vec_named(&ids)?)?;
        Ok(ids)
    }
}

/// Validate every entry in a pack against its tagged id (checksum
/// validation) and return the installed id list. This is the shared
/// validated core for both install seams: the byte-buffer install
/// (`install_pack`) and the memory-bounded temp-file install
/// (`install_pack_streaming`) both run their pack through here, so
/// both apply the same checksum validation and report the same
/// installed ids regardless of how the bytes reach the store.
fn validate_and_list_pack(reader: &crate::store::pack::PackReader) -> Result<Vec<PackObjectId>> {
    let ids = reader.list_ids();
    for id in &ids {
        let Some((obj_type, data)) = reader.get_object_bytes(id)? else {
            continue;
        };
        validate_pack_entry(id, obj_type, data.as_ref())?;
    }
    Ok(ids)
}

fn state_entries_from_pack(
    reader: &crate::store::pack::PackReader,
    ids: &[PackObjectId],
) -> Result<Vec<(StateId, Vec<u8>)>> {
    let mut states = Vec::new();
    for id in ids {
        let PackObjectId::StateId(change_id) = id else {
            continue;
        };
        let Some((obj_type, data)) = reader.get_object(id)? else {
            continue;
        };
        if obj_type != ObjectType::State {
            return Err(HeddleError::InvalidObject(format!(
                "pack id {} is indexed as {:?}, expected State",
                change_id.to_string_full(),
                obj_type
            )));
        }
        validate_state_serialized(&data, *change_id)?;
        states.push((*change_id, data));
    }
    Ok(states)
}

fn attachment_entries_from_pack(
    reader: &crate::store::pack::PackReader,
    ids: &[PackObjectId],
) -> Result<Vec<StateAttachment>> {
    let mut attachments = Vec::new();
    for id in ids {
        let Some((ObjectType::StateAttachment, data)) = reader.get_object(id)? else {
            continue;
        };
        attachments.push(rmp_serde::from_slice(&data)?);
    }
    Ok(attachments)
}

fn validate_pack_entry(id: &PackObjectId, obj_type: ObjectType, data: &[u8]) -> Result<()> {
    match (id, obj_type) {
        (PackObjectId::Hash(hash), ObjectType::Blob) => validate_blob_bytes(data, *hash),
        (PackObjectId::Hash(hash), ObjectType::Tree) => {
            validate_tree_serialized(data, *hash).map(|_| ())
        }
        (PackObjectId::Hash(hash), ObjectType::Action) => {
            validate_action_serialized(data, ActionId::from_hash(*hash)).map(|_| ())
        }
        (PackObjectId::StateId(change_id), ObjectType::State) => {
            validate_state_serialized(data, *change_id).map(|_| ())
        }
        (PackObjectId::Hash(hash), ObjectType::StateAttachment) => {
            let attachment: StateAttachment = rmp_serde::from_slice(data)?;
            if attachment.id().as_hash() != hash {
                return Err(HeddleError::InvalidObject(
                    "state attachment pack id mismatch".to_string(),
                ));
            }
            Ok(())
        }
        _ => Err(HeddleError::InvalidObject(format!(
            "unsupported native pack object: {:?} {:?}",
            id, obj_type
        ))),
    }
}

impl FsStore {
    /// Insert into the recent-blob cache when the payload fits the size gate.
    fn cache_recent_blob(&self, hash: ContentHash, blob: &Blob) {
        if blob.content().len() > super::fs_store::RECENT_BLOB_CACHE_MAX_BYTES {
            return;
        }
        if let Ok(mut cache) = self.recent_blobs.write() {
            cache.insert(hash, blob.clone());
        }
    }

    /// Single-pass blob lookup. The wrapper in `ObjectStore::get_blob`
    /// retries this once after a stale-reload on miss.
    fn try_get_blob_once(&self, hash: &ContentHash) -> Result<Option<Blob>> {
        // Cache first — avoid `path.exists()` / pack probes on warm hits.
        // Write lock: LRU promotion mutates the order list.
        if let Ok(mut cache) = self.recent_blobs.write()
            && let Some(blob) = cache.get(hash)
        {
            trace!("Found blob in recent object cache");
            return Ok(Some(blob.clone()));
        }

        if let Ok(manager) = self.pack_manager().read()
            && let Some((obj_type, data)) = manager.get_hashed_object(hash)?
            && obj_type == ObjectType::Blob
        {
            trace!("Found blob in packfile");
            // Step 2: skip the BLAKE3 re-hash. The pack reader already
            // located this entry by its content-addressed key in the
            // pack index — anything served here either matches or
            // means the pack itself is corrupted in ways a per-read
            // hash check can't recover from cleanly. For multi-MB
            // blobs the verify was the dominant tail of the cold
            // read (~3GB/s × 10MB ≈ 3.3ms per call).
            let blob = Blob::new(data);
            self.cache_recent_blob(*hash, &blob);
            return Ok(Some(blob));
        }

        let path = hash_path(&blobs_dir(&self.root), hash);
        match read_file_bytes(&path)? {
            Some(data) => {
                trace!(size = data.as_slice().len(), "Blob data read");
                let content = codec::decode_blob_content(data.as_slice())?;
                let blob = Blob::new(content);
                // Loose blobs are bare bytes on disk: a half-written
                // file or bit-rot inside the payload would slip past
                // the path-is-the-hash invariant. Keep the verify on
                // this path. Pack-resident reads above skip it because
                // pack entries are framed with offset + length records
                // that fail to parse if the pack is corrupt.
                if blob.hash() != *hash {
                    return Err(HeddleError::Corruption {
                        expected: *hash,
                        found: blob.hash(),
                    });
                }
                self.cache_recent_blob(*hash, &blob);
                Ok(Some(blob))
            }
            None => Ok(None),
        }
    }

    /// Shared body for `try_has_{blob,tree,state}_once`: object is
    /// present iff the loose path exists or the pack manager
    /// resolves it. Callers pass the loose path and the
    /// pack-manager probe; the helper handles the lock.
    fn loose_or_packed(
        &self,
        loose_path: &Path,
        in_pack: impl FnOnce(&PackManager) -> bool,
    ) -> Result<bool> {
        if loose_path.exists() {
            return Ok(true);
        }
        if let Ok(manager) = self.pack_manager().read() {
            return Ok(in_pack(&manager));
        }
        Ok(false)
    }

    fn try_has_blob_once(&self, hash: &ContentHash) -> Result<bool> {
        // Keep `has_blob` coherent with cache-first `get_blob`. A pure
        // existence check needs no LRU promotion, so take the *read*
        // lock and use `contains` — concurrent `has_blob` calls in
        // heddled/mount don't serialize on the exclusive write lock.
        if let Ok(cache) = self.recent_blobs.read()
            && cache.contains(hash)
        {
            return Ok(true);
        }
        let path = hash_path(&blobs_dir(&self.root), hash);
        self.loose_or_packed(&path, |m| m.has_object(hash))
    }

    /// Header-only size lookup for a single attempt. Tries:
    /// 1. The recent-blob cache (we already have the bytes in
    ///    memory — `len()` is free).
    /// 2. The loose blob: peek the 9-byte compression header. For a
    ///    compressed blob the recorded uncompressed size lives in the
    ///    header. For an uncompressed blob (no recognised header) the
    ///    on-disk file length IS the blob size.
    /// 3. Any loaded pack: the pack format records the uncompressed
    ///    size as a varint right after the tagged id, so we can decode
    ///    it without touching the body.
    ///
    /// Cost: one short read (typically 9 bytes) for loose blobs, or a
    /// pure in-memory varint decode for packed blobs. *No*
    /// decompression.
    fn try_get_blob_size_once(&self, hash: &ContentHash) -> Result<Option<u64>> {
        if let Ok(mut cache) = self.recent_blobs.write()
            && let Some(blob) = cache.get(hash)
        {
            return Ok(Some(blob.content().len() as u64));
        }

        let path = hash_path(&blobs_dir(&self.root), hash);
        if let Some((header, file_len)) = read_file_header(&path, BLOB_HEADER_PEEK)? {
            if let Some(size) = header_uncompressed_size(&header) {
                return Ok(Some(size));
            }
            // No recognised compression header — the file is raw
            // blob bytes. The on-disk length is the blob size.
            return Ok(Some(file_len));
        }

        if let Ok(manager) = self.pack_manager().read()
            && let Some(size) = manager.get_hashed_object_size(hash)?
        {
            return Ok(Some(size));
        }
        Ok(None)
    }

    fn try_get_tree_once(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        // Cache first. The recent-object cache only ever holds trees we
        // wrote or read this process, so a hit is authoritative for a
        // read. Write lock: LRU promotion mutates the order list.
        if let Ok(mut cache) = self.recent_trees.write()
            && let Some(tree) = cache.get(hash)
        {
            trace!("Found tree in recent object cache");
            return Ok(Some(tree.clone()));
        }

        // Loose trees may be migration-promoted V2 shadows of an older packed
        // V1 encoding at the same semantic tree hash. Prefer the loose copy
        // when it exists, then fall through to pack lookup.
        let path = hash_path(&trees_dir(&self.root), hash);
        if path.exists()
            && let Some(data) = read_file_bytes(&path)?
        {
            trace!(size = data.as_slice().len(), "Tree data read");
            let tree = validate_loaded_tree(codec::decode_tree(data.as_slice())?)?;
            if tree.hash() != *hash {
                return Err(HeddleError::Corruption {
                    expected: *hash,
                    found: tree.hash(),
                });
            }
            if let Ok(mut cache) = self.recent_trees.write() {
                cache.insert(*hash, tree.clone());
            }
            return Ok(Some(tree));
        }

        if let Ok(manager) = self.pack_manager().read()
            && let Some((obj_type, data)) = manager.get_hashed_object(hash)?
            && obj_type == ObjectType::Tree
        {
            trace!("Found tree in packfile");
            let tree = validate_loaded_tree(codec::decode_tree_serialized(&data)?)?;
            if tree.hash() != *hash {
                return Err(HeddleError::Corruption {
                    expected: *hash,
                    found: tree.hash(),
                });
            }
            if let Ok(mut cache) = self.recent_trees.write() {
                cache.insert(*hash, tree.clone());
            }
            return Ok(Some(tree));
        }
        Ok(None)
    }

    fn try_get_tree_serialized_once(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> {
        let path = hash_path(&trees_dir(&self.root), hash);
        if path.exists()
            && let Some(data) = read_file_bytes(&path)?
        {
            return Ok(Some(codec::decode_tree_body(data.as_slice())?));
        }

        if let Ok(manager) = self.pack_manager().read()
            && let Some((obj_type, data)) = manager.get_hashed_object(hash)?
            && obj_type == ObjectType::Tree
        {
            return Ok(Some(data));
        }

        Ok(None)
    }

    fn try_has_tree_once(&self, hash: &ContentHash) -> Result<bool> {
        // Read-lock `contains`: an existence check needs no LRU
        // promotion, so it must not serialize on the write lock.
        if let Ok(cache) = self.recent_trees.read()
            && cache.contains(hash)
        {
            return Ok(true);
        }
        let path = hash_path(&trees_dir(&self.root), hash);
        self.loose_or_packed(&path, |m| m.has_object(hash))
    }

    fn try_get_state_once(&self, id: &StateId) -> Result<Option<State>> {
        // Cache first — avoid `path.exists()` / pack probes on warm hits.
        // Write lock: LRU promotion mutates the order list. Put paths and
        // successful reads below keep this coherent for the process.
        if let Ok(mut cache) = self.recent_states.write()
            && let Some(state) = cache.get(id)
        {
            trace!("Found state in recent object cache");
            return Ok(Some(state.clone()));
        }

        let path = state_path(&self.root, id);
        if let Some(data) = read_file_bytes(&path)? {
            trace!(size = data.as_slice().len(), "State read from loose object");
            let state = validate_loaded_state(id, codec::decode_state(data.as_slice())?)?;
            if let Ok(mut cache) = self.recent_states.write() {
                cache.insert(*id, state.clone());
            }
            return Ok(Some(state));
        }

        if let Ok(manager) = self.pack_manager().read()
            && let Some((obj_type, data)) = manager.get_object(&PackObjectId::StateId(*id))?
            && obj_type == ObjectType::State
        {
            trace!("Found state in packfile");
            let state = validate_loaded_state(id, rmp_serde::from_slice(&data)?)?;
            if let Ok(mut cache) = self.recent_states.write() {
                cache.insert(*id, state.clone());
            }
            return Ok(Some(state));
        }

        Ok(None)
    }

    fn try_has_state_once(&self, id: &StateId) -> Result<bool> {
        // Read-lock `contains`: an existence check needs no LRU
        // promotion, so it must not serialize on the write lock.
        if let Ok(cache) = self.recent_states.read()
            && cache.contains(id)
        {
            return Ok(true);
        }
        let path = state_path(&self.root, id);
        self.loose_or_packed(&path, |m| m.has_object_id(&PackObjectId::StateId(*id)))
    }
}

impl ObjectStore for FsStore {
    fn clear_recent_caches(&self) {
        self.clear_recent_object_caches();
    }

    /// Zero-copy pack fast path. When the blob lives in a packfile
    /// and is non-delta + uncompressed, returns a `Bytes::slice`
    /// view of the pack's mmap — no decompression, no allocation,
    /// no memcpy. Compressed pack entries, delta entries, and
    /// loose blobs fall back to `get_blob` and wrap the result in a
    /// `Bytes` (the `Vec` → `Bytes` conversion is itself zero-copy).
    fn get_blob_bytes(&self, hash: &ContentHash) -> Result<Option<bytes::Bytes>> {
        if let Ok(manager) = self.pack_manager().read()
            && let Some((obj_type, data)) = manager.get_hashed_object_bytes(hash)?
            && obj_type == crate::store::pack::ObjectType::Blob
        {
            return Ok(Some(data));
        }
        Ok(self
            .get_blob(hash)?
            .map(|blob| bytes::Bytes::from(blob.into_content())))
    }

    #[instrument(skip(self), fields(hash = %hash.short()))]
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>> {
        if let Some(blob) = self.try_get_blob_once(hash)? {
            return Ok(Some(blob));
        }
        // Miss path: a sibling FsStore (e.g. the worktree's repo
        // backing the same `.heddle/`) may have installed a new pack
        // since we loaded ours. Cheap disk-count check first; full
        // reload only when the count grew.
        if self.reload_packs_if_stale()?
            && let Some(blob) = self.try_get_blob_once(hash)?
        {
            return Ok(Some(blob));
        }
        trace!("Blob not found");
        match &self.external_source {
            Some(source) => source.get_blob(hash),
            None => Ok(None),
        }
    }

    #[instrument(skip(self, blob), fields(size = blob.content().len()))]
    fn put_blob(&self, blob: &Blob) -> Result<ContentHash> {
        let hash = blob.hash();
        let path = hash_path(&blobs_dir(&self.root), &hash);

        if !path.exists() {
            let data = codec::encode_blob_content(blob.content(), &self.compression)?;
            trace!(compressed_size = data.len(), "Writing blob");
            self.write_loose_object_atomic(&path, &data)?;
        } else {
            trace!("Blob already exists, skipping write");
        }
        self.cache_recent_blob(hash, blob);

        Ok(hash)
    }

    #[instrument(skip(self, blob), fields(hash = %hash.short()))]
    fn put_blob_with_hash(&self, blob: &Blob, hash: ContentHash) -> Result<ContentHash> {
        if blob.hash() != hash {
            return Err(HeddleError::Corruption {
                expected: hash,
                found: blob.hash(),
            });
        }

        let path = hash_path(&blobs_dir(&self.root), &hash);

        if !path.exists() {
            let data = codec::encode_blob_content(blob.content(), &self.compression)?;
            trace!(
                compressed_size = data.len(),
                "Writing blob with precomputed hash"
            );
            self.write_loose_object_atomic(&path, &data)?;
        }
        self.cache_recent_blob(hash, blob);

        Ok(hash)
    }

    #[instrument(skip(self, data), fields(hash = %hash.short(), size = data.len()))]
    fn put_blob_bytes_with_hash(&self, data: &[u8], hash: ContentHash) -> Result<ContentHash> {
        validate_blob_bytes(data, hash)?;

        let path = hash_path(&blobs_dir(&self.root), &hash);
        if !path.exists() {
            trace!(
                size = data.len(),
                "Writing raw blob bytes with precomputed hash"
            );
            self.write_loose_object_atomic(&path, data)?;
        }
        self.cache_recent_blob(hash, &Blob::from_slice(data));

        Ok(hash)
    }

    #[instrument(skip(self), fields(hash = %hash.short()))]
    fn has_blob(&self, hash: &ContentHash) -> Result<bool> {
        if self.try_has_blob_once(hash)? {
            return Ok(true);
        }
        if self.reload_packs_if_stale()? && self.try_has_blob_once(hash)? {
            return Ok(true);
        }
        match &self.external_source {
            Some(source) => Ok(source.get_blob(hash)?.is_some()),
            None => Ok(false),
        }
    }

    /// Loose blob path safe for clonefile/copy materialization.
    ///
    /// Returns `Some(path)` only when the loose file exists, is
    /// stored uncompressed, *and* its bytes hash to the expected
    /// content hash. Compressed blobs and pack-only blobs fall
    /// through to `None`; so do *torn* cache-mirror files (the
    /// `AtomicWriteMode::NoSync` write side may leave one if the
    /// host crashed during a previous promote). On the torn case
    /// the caller re-promotes from the authoritative pack copy.
    ///
    /// Verification is amortised: a hash that passes the check once
    /// in this process is recorded in `verified_loose_blobs` and
    /// subsequent calls skip the read+hash. So the cost on the
    /// materialize hot path is at most one BLAKE3 over each unique
    /// blob per process lifetime — negligible for tiny blobs,
    /// bounded by working-set size for huge ones.
    fn loose_blob_path(&self, hash: &ContentHash) -> Option<PathBuf> {
        let path = hash_path(&blobs_dir(&self.root), hash);
        // Fast path: this process already verified (or wrote) this
        // hash's loose mirror in `promote_to_loose_uncompressed`.
        // Trust without re-hashing — `path.exists()` is the only
        // I/O we need.
        if let Ok(verified) = self.verified_loose_blobs.read()
            && verified.contains(hash)
            && path.exists()
        {
            return Some(path);
        }

        // First-time-this-process check: peek the header to filter
        // out compressed-loose files cheaply, then verify the
        // body's hash matches what the caller expects. A torn-write
        // (post-crash) cache mirror fails this and the caller
        // re-promotes from the pack.
        //
        // Header peek must cover the 9-byte modern header **plus**
        // the 4-byte ZSTD magic that `is_compressed` checks —
        // peeking only 9 bytes makes `is_compressed` falsely
        // return `false` on a properly-compressed blob, and we'd
        // hand the caller the compressed file path. Same off-by-4
        // we fixed in `BLOB_HEADER_PEEK`.
        let (header, _) = read_file_header(&path, BLOB_HEADER_PEEK).ok().flatten()?;
        if is_compressed(&header) {
            return None;
        }
        let bytes = read_file_bytes(&path).ok().flatten()?;
        let actual = ContentHash::compute_typed("blob", bytes.as_slice());
        if actual != *hash {
            // Torn write or unrelated corruption. Leave the file on
            // disk; the caller's `promote_to_loose_uncompressed`
            // will overwrite it via the standard temp+rename path.
            return None;
        }
        if let Ok(mut verified) = self.verified_loose_blobs.write() {
            verified.insert(*hash, ());
        }
        Some(path)
    }

    /// Promote a blob to its uncompressed-loose canonical path so
    /// `loose_blob_path` returns `Some(path)` and hardlink-first
    /// materialization fires.
    ///
    /// Three cases:
    /// 1. Already loose+uncompressed: peek the header, no-op.
    /// 2. Loose but compressed: read+decompress, atomically rewrite
    ///    the canonical path with raw bytes.
    /// 3. Pack-only: read out of the pack via `get_blob`, atomically
    ///    write to the canonical loose path. Pack copy is left in
    ///    place — the next prune cycle will discard the loose mirror
    ///    and a future materialize will re-promote.
    #[instrument(skip(self), fields(hash = %hash.short()))]
    fn promote_to_loose_uncompressed(&self, hash: &ContentHash) -> Result<bool> {
        let path = hash_path(&blobs_dir(&self.root), hash);

        // An external Git-overlay blob must stay external: materialization can
        // read its bytes through `get_blob_bytes`, but promotion would turn a
        // read-through into an accidental native copy and violate the source-
        // authority boundary.
        if !self.try_has_blob_once(hash)?
            && let Some(source) = &self.external_source
            && source.get_blob(hash)?.is_some()
        {
            return Ok(false);
        }

        // Idempotent fast path: already loose AND uncompressed.
        if let Some((header, _)) = read_file_header(&path, 9)?
            && !is_compressed(&header)
        {
            trace!("Blob already loose+uncompressed; skipping promotion");
            return Ok(false);
        }

        // Either compressed-loose or pack-only. Reading via
        // `get_blob` covers both: compressed-loose decompresses on
        // the way out, pack-only reads from the loaded pack manager.
        let blob = self.get_blob(hash)?.ok_or_else(|| {
            HeddleError::NotFound(format!(
                "blob {} not found in store; cannot promote to loose-uncompressed",
                hash
            ))
        })?;

        // Install the uncompressed bytes at the canonical loose path
        // via the cache-mirror atomic-write variant: no fsync, just
        // temp+rename. The fsync skip is what makes promotion fast
        // (measured: ~5 ms/blob with `sync_data` vs ~0.2 ms without
        // on macOS APFS); the safety comes from the read-side hash
        // check in `loose_blob_path`. A torn write after a crash
        // produces a file whose content hash doesn't match, so the
        // next reader rejects it and re-promotes from the pack.
        //
        // Record the hash in this process's verified-blobs cache:
        // we just wrote the bytes ourselves, so the subsequent read
        // path can trust them without re-hashing.
        debug!(
            size = blob.content().len(),
            "Promoting blob to loose-uncompressed canonical store"
        );
        self.write_loose_object_cache(&path, blob.content())?;
        if let Ok(mut verified) = self.verified_loose_blobs.write() {
            verified.insert(*hash, ());
        }
        Ok(true)
    }

    #[instrument(skip(self), fields(hash = %hash.short()))]
    fn blob_size(&self, hash: &ContentHash) -> Result<Option<u64>> {
        if let Some(size) = self.try_get_blob_size_once(hash)? {
            return Ok(Some(size));
        }
        // Sibling-store recovery, mirroring the read path: if a
        // concurrent writer just installed a pack we don't know about,
        // reload and retry once before reporting a miss.
        if self.reload_packs_if_stale()?
            && let Some(size) = self.try_get_blob_size_once(hash)?
        {
            return Ok(Some(size));
        }
        match &self.external_source {
            Some(source) => Ok(source
                .get_blob(hash)?
                .map(|blob| blob.content().len() as u64)),
            None => Ok(None),
        }
    }

    #[instrument(skip(self), fields(hash = %hash.short()))]
    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        if let Some(tree) = self.try_get_tree_once(hash)? {
            return Ok(Some(tree));
        }
        if self.reload_packs_if_stale()?
            && let Some(tree) = self.try_get_tree_once(hash)?
        {
            return Ok(Some(tree));
        }
        trace!("Tree not found");
        match &self.external_source {
            Some(source) => source.get_tree(hash),
            None => Ok(None),
        }
    }

    #[instrument(skip(self), fields(hash = %hash.short()))]
    fn get_tree_serialized(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> {
        if let Some(data) = self.try_get_tree_serialized_once(hash)? {
            return Ok(Some(data));
        }
        if self.reload_packs_if_stale()?
            && let Some(data) = self.try_get_tree_serialized_once(hash)?
        {
            return Ok(Some(data));
        }
        match &self.external_source {
            Some(source) => source
                .get_tree(hash)?
                .map(|tree| rmp_serde::to_vec_named(&tree))
                .transpose()
                .map_err(|error| HeddleError::InvalidObject(error.to_string())),
            None => Ok(None),
        }
    }

    #[instrument(skip(self, tree), fields(entry_count = tree.entries().len()))]
    fn put_tree(&self, tree: &Tree) -> Result<ContentHash> {
        let hash = tree.hash();
        let path = hash_path(&trees_dir(&self.root), &hash);

        // Overlay snapshots rebuild the worktree shape through this shared
        // chokepoint. If the exact tree is already authoritative in Git,
        // retain only its content identity and keep the native store sparse.
        let externally_available = if !path.exists() {
            match &self.external_source {
                Some(source) => source.get_tree(&hash)?.is_some(),
                None => false,
            }
        } else {
            false
        };
        if !path.exists() && !externally_available {
            let (_, data) = codec::encode_tree(tree, &self.compression)?;
            trace!(compressed_size = data.len(), "Writing tree");
            self.write_loose_object_atomic(&path, &data)?;
        } else if externally_available {
            trace!("Tree remains in authoritative external object source");
        } else {
            trace!("Tree already exists, skipping write");
        }
        if let Ok(mut cache) = self.recent_trees.write() {
            cache.insert(hash, tree.clone());
        }

        Ok(hash)
    }

    #[instrument(skip(self, data), fields(hash = %hash.short(), size = data.len()))]
    fn put_tree_serialized(&self, data: &[u8], hash: ContentHash) -> Result<ContentHash> {
        let tree = validate_tree_serialized(data, hash)?;

        let path = hash_path(&trees_dir(&self.root), &hash);
        let should_write = match read_file_bytes(&path)? {
            Some(existing) => codec::decode_tree_body(existing.as_slice())? != data,
            None => true,
        };
        if should_write {
            trace!(size = data.len(), "Writing raw serialized tree");
            self.write_loose_object_atomic(&path, data)?;
        }
        if let Ok(mut cache) = self.recent_trees.write() {
            cache.insert(hash, tree);
        }

        Ok(hash)
    }

    #[instrument(skip(self), fields(hash = %hash.short()))]
    fn has_tree(&self, hash: &ContentHash) -> Result<bool> {
        if self.try_has_tree_once(hash)? {
            return Ok(true);
        }
        if self.reload_packs_if_stale()? && self.try_has_tree_once(hash)? {
            return Ok(true);
        }
        match &self.external_source {
            Some(source) => Ok(source.get_tree(hash)?.is_some()),
            None => Ok(false),
        }
    }

    #[instrument(skip(self), fields(id = %id.short()))]
    fn get_state(&self, id: &StateId) -> Result<Option<State>> {
        if let Some(state) = self.try_get_state_once(id)? {
            return Ok(Some(state));
        }
        if self.reload_packs_if_stale()?
            && let Some(state) = self.try_get_state_once(id)?
        {
            return Ok(Some(state));
        }
        trace!("State not found");
        match &self.external_source {
            Some(source) => source.get_state(id),
            None => Ok(None),
        }
    }

    #[instrument(skip(self, state), fields(id = %state.id().short()))]
    fn put_state(&self, state: &State) -> Result<()> {
        let state_id = state.id();
        let path = state_path(&self.root, &state_id);
        let data = codec::encode_state(state, &self.compression)?;
        trace!(compressed_size = data.len(), "Writing state");
        self.write_loose_object_atomic(&path, &data)?;
        if let Ok(mut cache) = self.recent_states.write() {
            let mut cached = state.clone();
            cached.state_id = state_id;
            cache.insert(state_id, cached);
        }
        Ok(())
    }

    #[instrument(skip(self, data), fields(id = %id.short(), size = data.len()))]
    fn put_state_serialized(&self, data: &[u8], id: StateId) -> Result<()> {
        let state = validate_state_serialized(data, id)?;
        let path = state_path(&self.root, &id);
        trace!(size = data.len(), "Writing raw serialized state");
        self.write_loose_object_atomic(&path, data)?;
        if let Ok(mut cache) = self.recent_states.write() {
            cache.insert(id, state);
        }
        Ok(())
    }

    #[instrument(skip(self), fields(id = %id.short()))]
    fn has_state(&self, id: &StateId) -> Result<bool> {
        if self.try_has_state_once(id)? {
            return Ok(true);
        }
        if self.reload_packs_if_stale()? && self.try_has_state_once(id)? {
            return Ok(true);
        }
        match &self.external_source {
            Some(source) => Ok(source.get_state(id)?.is_some()),
            None => Ok(false),
        }
    }

    #[instrument(skip(self))]
    fn list_states(&self) -> Result<Vec<StateId>> {
        self.reload_packs_if_stale()?;

        let dir = states_dir(&self.root);
        let mut states = Vec::new();
        if dir.exists() {
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                if let Some(name) = path.file_stem()
                    && let Some(name_str) = name.to_str()
                    && let Ok(id) = StateId::parse(name_str)
                {
                    states.push(id);
                }
            }
        }
        if let Ok(manager) = self.pack_manager().read() {
            for id in manager.list_all_ids()? {
                if let PackObjectId::StateId(change_id) = id
                    && !states.contains(&change_id)
                {
                    states.push(change_id);
                }
            }
        }
        if let Some(source) = &self.external_source {
            for id in source.list_states()? {
                if !states.contains(&id) {
                    states.push(id);
                }
            }
        }
        debug!(count = states.len(), "Listed states");
        Ok(states)
    }

    fn get_state_attachment(
        &self,
        state: &StateId,
        id: &StateAttachmentId,
    ) -> Result<Option<StateAttachment>> {
        let path = state_attachment_path(&self.root, state, id);
        let bytes = if let Some(bytes) = read_file_bytes(&path)? {
            bytes.as_slice().to_vec()
        } else if let Ok(manager) = self.pack_manager().read()
            && let Some((ObjectType::StateAttachment, bytes)) =
                manager.get_hashed_object(id.as_hash())?
        {
            bytes
        } else {
            return Ok(None);
        };
        let attachment: StateAttachment = rmp_serde::from_slice(&bytes)?;
        if attachment.state_id != *state || attachment.id() != *id {
            return Err(HeddleError::InvalidObject(
                "state attachment address does not match content".to_string(),
            ));
        }
        Ok(Some(attachment))
    }

    fn put_state_attachment(&self, attachment: &StateAttachment) -> Result<StateAttachmentId> {
        let id = attachment.id();
        self.with_state_attachment_index_lock(&attachment.state_id, || {
            let index_path = state_attachment_index_path(&self.root, &attachment.state_id);
            let mut ids: Vec<StateAttachmentId> = match read_file_bytes(&index_path)? {
                Some(bytes) => rmp_serde::from_slice(bytes.as_slice())?,
                None => self.rebuild_state_attachment_index(&attachment.state_id)?,
            };
            if !ids.contains(&id) {
                ids.push(id);
                ids.sort();
                self.write_loose_object_atomic(&index_path, &rmp_serde::to_vec_named(&ids)?)?;
            }
            let path = state_attachment_path(&self.root, &attachment.state_id, &id);
            self.write_loose_object_atomic(&path, &rmp_serde::to_vec_named(attachment)?)?;
            Ok(id)
        })
    }

    fn list_state_attachments(&self, state: &StateId) -> Result<Vec<StateAttachment>> {
        self.with_state_attachment_index_lock(state, || {
            let index_path = state_attachment_index_path(&self.root, state);
            let mut ids: Vec<StateAttachmentId> = match read_file_bytes(&index_path)? {
                Some(bytes) => rmp_serde::from_slice(bytes.as_slice())?,
                None => self.rebuild_state_attachment_index(state)?,
            };
            let mut attachments = Vec::new();
            let mut stale = false;
            for id in &ids {
                match self.get_state_attachment(state, id)? {
                    Some(attachment) => attachments.push(attachment),
                    None => stale = true,
                }
            }
            if stale {
                ids = self.rebuild_state_attachment_index(state)?;
                attachments.clear();
                for id in ids {
                    let attachment = self.get_state_attachment(state, &id)?.ok_or_else(|| {
                        HeddleError::InvalidObject(format!(
                            "rebuilt state attachment index references missing {id}"
                        ))
                    })?;
                    attachments.push(attachment);
                }
            }
            Ok(attachments)
        })
    }

    #[instrument(skip(self), fields(id = %id))]
    fn get_action(&self, id: &ActionId) -> Result<Option<Action>> {
        let path = action_path(&self.root, id);
        if !path.exists()
            && let Ok(manager) = self.pack_manager().read()
            && let Some((obj_type, data)) = manager.get_hashed_object(id.as_hash())?
            && obj_type == ObjectType::Action
        {
            trace!("Found action in packfile");
            let action = validate_loaded_action(id, rmp_serde::from_slice(&data)?)?;
            return Ok(Some(action));
        }
        match read_file_bytes(&path)? {
            Some(data) => {
                trace!(size = data.as_slice().len(), "Action data read");
                let action = validate_loaded_action(id, codec::decode_action(data.as_slice())?)?;
                Ok(Some(action))
            }
            None => {
                trace!("Action not found");
                Ok(None)
            }
        }
    }

    #[instrument(skip(self, action))]
    fn put_action(&self, action: &mut Action) -> Result<ActionId> {
        let id = action.id();
        let path = action_path(&self.root, &id);

        if !path.exists() {
            let (_, data) = codec::encode_action(action, &self.compression)?;
            trace!(id = %id, compressed_size = data.len(), "Writing action");
            self.write_loose_object_atomic(&path, &data)?;
        }

        Ok(id)
    }

    #[instrument(skip(self))]
    fn list_actions(&self) -> Result<Vec<ActionId>> {
        let dir = actions_dir(&self.root);
        let mut actions = Vec::new();
        if dir.exists() {
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                if let Some(name) = path.file_stem()
                    && let Some(name_str) = name.to_str()
                    && let Ok(hash) = ContentHash::from_hex(name_str)
                {
                    actions.push(ActionId::from_hash(hash));
                }
            }
        }
        if let Ok(manager) = self.pack_manager().read() {
            for id in manager.list_all_ids()? {
                if let PackObjectId::Hash(hash) = id
                    && !actions.iter().any(|action_id| action_id.as_hash() == &hash)
                    && let Some((obj_type, _)) = manager.get_hashed_object(&hash)?
                    && obj_type == ObjectType::Action
                {
                    actions.push(ActionId::from_hash(hash));
                }
            }
        }
        debug!(count = actions.len(), "Listed actions");
        Ok(actions)
    }

    #[instrument(skip(self))]
    fn list_blobs(&self) -> Result<Vec<ContentHash>> {
        let dir = blobs_dir(&self.root);
        let mut blobs = list_hashes_from_dir(&dir)?;
        if let Ok(manager) = self.pack_manager().read() {
            for id in manager.list_all_ids()? {
                if let PackObjectId::Hash(hash) = id
                    && !blobs.contains(&hash)
                    && let Some((obj_type, _)) = manager.get_hashed_object(&hash)?
                    && obj_type == ObjectType::Blob
                {
                    blobs.push(hash);
                }
            }
        }
        Ok(blobs)
    }

    #[instrument(skip(self))]
    fn list_trees(&self) -> Result<Vec<ContentHash>> {
        let dir = trees_dir(&self.root);
        let mut trees = list_hashes_from_dir(&dir)?;
        if let Ok(manager) = self.pack_manager().read() {
            for id in manager.list_all_ids()? {
                if let PackObjectId::Hash(hash) = id
                    && !trees.contains(&hash)
                    && let Some((obj_type, _)) = manager.get_hashed_object(&hash)?
                    && obj_type == ObjectType::Tree
                {
                    trees.push(hash);
                }
            }
        }
        Ok(trees)
    }

    #[instrument(skip(self))]
    fn pack_objects(&self, aggressive: bool) -> Result<(u64, u64)> {
        self.pack_objects_impl(aggressive)
    }

    #[instrument(skip(self), fields(id = ?id))]
    fn get_pack_object(&self, id: &PackObjectId) -> Result<Option<(ObjectType, Vec<u8>)>> {
        if let Ok(manager) = self.pack_manager().read()
            && let Some((obj_type, data)) = manager.get_object(id)?
        {
            return Ok(Some((obj_type, data)));
        }

        match id {
            PackObjectId::Hash(hash) => {
                if let Some(blob) = self.get_blob(hash)? {
                    return Ok(Some((ObjectType::Blob, blob.content().to_vec())));
                }
                if let Some(tree) = self.get_tree(hash)? {
                    return Ok(Some((ObjectType::Tree, rmp_serde::to_vec_named(&tree)?)));
                }
                if let Some(action) = self.get_action(&ActionId::from_hash(*hash))? {
                    return Ok(Some((
                        ObjectType::Action,
                        rmp_serde::to_vec_named(&action)?,
                    )));
                }
                Ok(None)
            }
            PackObjectId::StateId(change_id) => {
                if let Some(state) = self.get_state(change_id)? {
                    Ok(Some((ObjectType::State, rmp_serde::to_vec_named(&state)?)))
                } else {
                    Ok(None)
                }
            }
        }
    }

    #[instrument(skip(self, pack_data, index_data))]
    fn install_pack(&self, pack_data: &[u8], index_data: &[u8]) -> Result<Vec<PackObjectId>> {
        let reader = crate::store::pack::PackReader::from_slice(pack_data, index_data)?;
        let ids = validate_and_list_pack(&reader)?;
        let state_entries = state_entries_from_pack(&reader, &ids)?;
        self.install_pack_files(pack_data, index_data)?;
        for (id, data) in state_entries {
            self.put_state_serialized(&data, id)?;
        }
        for id in &ids {
            let Some((ObjectType::StateAttachment, data)) = reader.get_object(id)? else {
                continue;
            };
            let attachment: StateAttachment = rmp_serde::from_slice(&data)?;
            self.put_state_attachment(&attachment)?;
        }
        self.clear_recent_object_caches();
        Ok(ids)
    }

    #[instrument(skip(self, blobs), fields(count = blobs.len()))]
    fn put_blobs_packed(&self, blobs: Vec<(crate::object::ContentHash, Vec<u8>)>) -> Result<()> {
        self.put_blobs_packed_impl(blobs)
    }

    #[instrument(skip(self))]
    fn install_pack_streaming(
        &self,
        pack_path: &std::path::Path,
        index_path: &std::path::Path,
    ) -> Result<Vec<PackObjectId>> {
        // Validate + list ids through the same core as the byte-buffer
        // seam, but via an mmap-backed reader so the pack is never
        // copied into the heap — the memory-bounded promise survives.
        // Drop the reader (releasing the mmap) before the rename so
        // the file move isn't racing an open mapping.
        let ids = {
            let reader = crate::store::pack::PackReader::open(pack_path, index_path)?;
            validate_and_list_pack(&reader)?
        };
        let state_entries = {
            let reader = crate::store::pack::PackReader::open(pack_path, index_path)?;
            state_entries_from_pack(&reader, &ids)?
        };
        let attachment_entries = {
            let reader = crate::store::pack::PackReader::open(pack_path, index_path)?;
            attachment_entries_from_pack(&reader, &ids)?
        };
        self.install_pack_files_streaming(pack_path, index_path)?;
        for (id, data) in state_entries {
            self.put_state_serialized(&data, id)?;
        }
        for attachment in attachment_entries {
            self.put_state_attachment(&attachment)?;
        }
        Ok(ids)
    }

    #[instrument(skip(self))]
    fn prune_loose_objects(&self) -> Result<(u64, u64)> {
        self.prune_loose_objects_impl()
    }

    #[instrument(skip(self))]
    fn begin_snapshot_write_batch(&self) -> Result<()> {
        self.begin_snapshot_write_batch_impl()
    }

    #[instrument(skip(self))]
    fn flush_snapshot_write_batch(&self) -> Result<()> {
        self.flush_snapshot_write_batch_impl()
    }

    #[instrument(skip(self))]
    fn abort_snapshot_write_batch(&self) {
        self.abort_snapshot_write_batch_impl();
    }

    fn has_redactions_for_blob(&self, blob: &ContentHash) -> Result<bool> {
        Ok(redaction_path(&self.root, blob).exists())
    }

    fn get_redactions_bytes_for_blob(&self, blob: &ContentHash) -> Result<Option<Vec<u8>>> {
        let path = redaction_path(&self.root, blob);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(HeddleError::Io(err)),
        }
    }

    fn put_redactions_bytes_for_blob(&self, blob: &ContentHash, bytes: &[u8]) -> Result<()> {
        let dir = redactions_dir(&self.root);
        if !dir.exists() {
            crate::fs_atomic::create_dir_all_durable(&dir)?;
        }
        let path = redaction_path(&self.root, blob);
        crate::fs_atomic::write_file_atomic(&path, bytes)?;
        Ok(())
    }

    fn list_blobs_with_redactions(&self) -> Result<Vec<ContentHash>> {
        let dir = redactions_dir(&self.root);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("bin") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Ok(hash) = ContentHash::from_hex(stem) {
                out.push(hash);
            }
        }
        Ok(out)
    }

    fn has_state_visibility_for_state(&self, state: &StateId) -> Result<bool> {
        Ok(state_visibility_path(&self.root, state).exists())
    }

    fn get_state_visibility_bytes_for_state(&self, state: &StateId) -> Result<Option<Vec<u8>>> {
        let path = state_visibility_path(&self.root, state);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(HeddleError::Io(err)),
        }
    }

    fn put_state_visibility_bytes_for_state(&self, state: &StateId, bytes: &[u8]) -> Result<()> {
        let dir = state_visibility_dir(&self.root);
        if !dir.exists() {
            crate::fs_atomic::create_dir_all_durable(&dir)?;
        }
        let path = state_visibility_path(&self.root, state);
        crate::fs_atomic::write_file_atomic(&path, bytes)?;
        Ok(())
    }

    fn list_states_with_visibility(&self) -> Result<Vec<StateId>> {
        let dir = state_visibility_dir(&self.root);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("bin") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Ok(state) = StateId::parse(stem) {
                out.push(state);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod state_attachment_tests {
    use std::sync::Arc;

    use chrono::Utc;

    use super::*;
    use crate::{
        object::{Attribution, Principal, StateAttachmentBody},
        store::{CompressionConfig, pack::PackBuilder},
    };

    fn fixture(store: &FsStore) -> (State, StateAttachment) {
        let tree = store.put_tree(&Tree::new()).unwrap();
        let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
        let state = State::new(tree, vec![], attribution.clone());
        store.put_state(&state).unwrap();
        let attachment = StateAttachment {
            state_id: state.id(),
            body: StateAttachmentBody::Context(ContentHash::compute(b"context")),
            attribution,
            created_at: Utc::now(),
            supersedes: None,
        };
        (state, attachment)
    }

    #[test]
    fn concurrent_attachment_writes_keep_every_index_entry() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = Arc::new(FsStore::new(temp.path()));
        let (state, base) = fixture(&store);
        let mut threads = Vec::new();
        for byte in 0..16u8 {
            let store = Arc::clone(&store);
            let mut attachment = base.clone();
            attachment.body = StateAttachmentBody::Context(ContentHash::compute(&[byte]));
            threads.push(std::thread::spawn(move || {
                store.put_state_attachment(&attachment).unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(store.list_state_attachments(&state.id()).unwrap().len(), 16);
    }

    #[test]
    fn missing_index_rebuilds_from_loose_objects() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = FsStore::new(temp.path());
        let (state, attachment) = fixture(&store);
        store.put_state_attachment(&attachment).unwrap();
        fs::remove_file(state_attachment_index_path(&store.root, &state.id())).unwrap();
        assert_eq!(
            store.list_state_attachments(&state.id()).unwrap(),
            vec![attachment]
        );
    }

    #[test]
    fn packed_attachment_uses_state_index_for_lookup() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = FsStore::new(temp.path());
        let (state, attachment) = fixture(&store);
        let mut builder = PackBuilder::new(CompressionConfig::default());
        builder.add(
            *attachment.id().as_hash(),
            ObjectType::StateAttachment,
            rmp_serde::to_vec_named(&attachment).unwrap(),
        );
        let (pack, index, _) = builder.build().unwrap();
        store.install_pack(&pack, &index).unwrap();
        fs::remove_file(state_attachment_path(
            &store.root,
            &state.id(),
            &attachment.id(),
        ))
        .unwrap();
        let rebuild_marker =
            state_attachment_index_path(&store.root, &state.id()).with_extension("rebuild-marker");
        let _ = fs::remove_file(&rebuild_marker);
        assert_eq!(
            store.list_state_attachments(&state.id()).unwrap(),
            vec![attachment.clone()]
        );
        assert_eq!(
            store.list_state_attachments(&state.id()).unwrap(),
            vec![attachment]
        );
        assert!(!rebuild_marker.exists());
    }
}
