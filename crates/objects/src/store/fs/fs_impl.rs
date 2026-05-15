// SPDX-License-Identifier: Apache-2.0
//! ObjectStore implementation for FsStore.

use std::{
    fs,
    path::{Path, PathBuf},
};

use tracing::{debug, instrument, trace};

use super::{
    FsStore,
    fs_io::{list_hashes_from_dir, read_file_bytes, read_file_header},
    fs_paths::{
        action_path, actions_dir, blobs_dir, hash_path, redaction_path, redactions_dir, state_path,
        states_dir, trees_dir,
    },
};
use crate::{
    object::{Action, ActionId, Blob, ChangeId, ContentHash, State, Tree},
    store::{
        HeddleError, ObjectStore, Result,
        compression::{compress, decompress, header_uncompressed_size, is_compressed},
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

fn validate_loaded_state(requested_id: &ChangeId, state: State) -> Result<State> {
    if state.change_id != *requested_id {
        return Err(HeddleError::InvalidObject(format!(
            "state change_id mismatch: requested {}, found {}",
            requested_id, state.change_id
        )));
    }

    Ok(state)
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

impl FsStore {
    /// Single-pass blob lookup. The wrapper in `ObjectStore::get_blob`
    /// retries this once after a stale-reload on miss.
    fn try_get_blob_once(&self, hash: &ContentHash) -> Result<Option<Blob>> {
        let path = hash_path(&blobs_dir(&self.root), hash);
        let loose_exists = path.exists();
        let pack_has = if loose_exists {
            false
        } else if let Ok(manager) = self.pack_manager().read() {
            manager.has_object(hash)
        } else {
            false
        };
        if (loose_exists || pack_has)
            && let Ok(cache) = self.recent_blobs.read()
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
            let blob = Blob::new(data);
            if blob.hash() != *hash {
                return Err(HeddleError::Corruption {
                    expected: *hash,
                    found: blob.hash(),
                });
            }
            return Ok(Some(blob));
        }

        match read_file_bytes(&path)? {
            Some(data) => {
                trace!(size = data.as_slice().len(), "Blob data read");
                let content = if is_compressed(data.as_slice()) {
                    decompress(data.as_slice())?
                } else {
                    data.into_vec()
                };
                let blob = Blob::new(content);
                if blob.hash() != *hash {
                    return Err(HeddleError::Corruption {
                        expected: *hash,
                        found: blob.hash(),
                    });
                }
                if let Ok(mut cache) = self.recent_blobs.write() {
                    cache.insert(*hash, blob.clone());
                }
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
        if let Ok(cache) = self.recent_blobs.read()
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
        let path = hash_path(&trees_dir(&self.root), hash);
        let loose_exists = path.exists();
        let pack_has = if loose_exists {
            false
        } else if let Ok(manager) = self.pack_manager().read() {
            manager.has_object(hash)
        } else {
            false
        };
        if (loose_exists || pack_has)
            && let Ok(cache) = self.recent_trees.read()
            && let Some(tree) = cache.get(hash)
        {
            trace!("Found tree in recent object cache");
            return Ok(Some(tree.clone()));
        }

        if let Ok(manager) = self.pack_manager().read()
            && let Some((obj_type, data)) = manager.get_hashed_object(hash)?
            && obj_type == ObjectType::Tree
        {
            trace!("Found tree in packfile");
            let tree = validate_loaded_tree(rmp_serde::from_slice(&data)?)?;
            if tree.hash() != *hash {
                return Err(HeddleError::Corruption {
                    expected: *hash,
                    found: tree.hash(),
                });
            }
            return Ok(Some(tree));
        }

        match read_file_bytes(&path)? {
            Some(data) => {
                trace!(size = data.as_slice().len(), "Tree data read");
                let decoded = if is_compressed(data.as_slice()) {
                    decompress(data.as_slice())?
                } else {
                    data.into_vec()
                };
                let tree = validate_loaded_tree(rmp_serde::from_slice(&decoded)?)?;
                if tree.hash() != *hash {
                    return Err(HeddleError::Corruption {
                        expected: *hash,
                        found: tree.hash(),
                    });
                }
                if let Ok(mut cache) = self.recent_trees.write() {
                    cache.insert(*hash, tree.clone());
                }
                Ok(Some(tree))
            }
            None => Ok(None),
        }
    }

    fn try_has_tree_once(&self, hash: &ContentHash) -> Result<bool> {
        let path = hash_path(&trees_dir(&self.root), hash);
        self.loose_or_packed(&path, |m| m.has_object(hash))
    }

    fn try_get_state_once(&self, id: &ChangeId) -> Result<Option<State>> {
        let path = state_path(&self.root, id);
        let loose_exists = path.exists();
        let pack_has = if loose_exists {
            false
        } else if let Ok(manager) = self.pack_manager().read() {
            manager.has_object_id(&PackObjectId::ChangeId(*id))
        } else {
            false
        };
        if (loose_exists || pack_has)
            && let Ok(cache) = self.recent_states.read()
            && let Some(state) = cache.get(id)
        {
            trace!("Found state in recent object cache");
            return Ok(Some(state.clone()));
        }

        if let Ok(manager) = self.pack_manager().read()
            && let Some((obj_type, data)) = manager.get_object(&PackObjectId::ChangeId(*id))?
            && obj_type == ObjectType::State
        {
            trace!("Found state in packfile");
            let state = validate_loaded_state(id, rmp_serde::from_slice(&data)?)?;
            if let Ok(mut cache) = self.recent_states.write() {
                cache.insert(*id, state.clone());
            }
            return Ok(Some(state));
        }

        match read_file_bytes(&path)? {
            Some(data) => {
                trace!(size = data.as_slice().len(), "State data read");
                let decoded = if is_compressed(data.as_slice()) {
                    decompress(data.as_slice())?
                } else {
                    data.into_vec()
                };
                let state = validate_loaded_state(id, rmp_serde::from_slice(&decoded)?)?;
                if let Ok(mut cache) = self.recent_states.write() {
                    cache.insert(*id, state.clone());
                }
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    fn try_has_state_once(&self, id: &ChangeId) -> Result<bool> {
        let path = state_path(&self.root, id);
        self.loose_or_packed(&path, |m| m.has_object_id(&PackObjectId::ChangeId(*id)))
    }
}

impl ObjectStore for FsStore {
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
        Ok(None)
    }

    #[instrument(skip(self, blob), fields(size = blob.content().len()))]
    fn put_blob(&self, blob: &Blob) -> Result<ContentHash> {
        let hash = blob.hash();
        let path = hash_path(&blobs_dir(&self.root), &hash);

        if !path.exists() {
            let content = blob.content();
            let data = compress(content, &self.compression)?.unwrap_or_else(|| content.to_vec());
            trace!(compressed_size = data.len(), "Writing blob");
            self.write_loose_object_atomic(&path, &data)?;
        } else {
            trace!("Blob already exists, skipping write");
        }
        if let Ok(mut cache) = self.recent_blobs.write() {
            cache.insert(hash, blob.clone());
        }

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
            let content = blob.content();
            let data = compress(content, &self.compression)?.unwrap_or_else(|| content.to_vec());
            trace!(
                compressed_size = data.len(),
                "Writing blob with precomputed hash"
            );
            self.write_loose_object_atomic(&path, &data)?;
        }
        if let Ok(mut cache) = self.recent_blobs.write() {
            cache.insert(hash, blob.clone());
        }

        Ok(hash)
    }

    #[instrument(skip(self, data), fields(hash = %hash.short(), size = data.len()))]
    fn put_blob_bytes_with_hash(&self, data: &[u8], hash: ContentHash) -> Result<ContentHash> {
        let found = ContentHash::compute_typed("blob", data);
        if found != hash {
            return Err(HeddleError::Corruption {
                expected: hash,
                found,
            });
        }

        let path = hash_path(&blobs_dir(&self.root), &hash);
        if !path.exists() {
            trace!(
                size = data.len(),
                "Writing raw blob bytes with precomputed hash"
            );
            self.write_loose_object_atomic(&path, data)?;
        }
        if let Ok(mut cache) = self.recent_blobs.write() {
            cache.insert(hash, Blob::from_slice(data));
        }

        Ok(hash)
    }

    #[instrument(skip(self), fields(hash = %hash.short()))]
    fn has_blob(&self, hash: &ContentHash) -> Result<bool> {
        if self.try_has_blob_once(hash)? {
            return Ok(true);
        }
        if self.reload_packs_if_stale()? {
            return self.try_has_blob_once(hash);
        }
        Ok(false)
    }

    /// Loose blob path safe for hardlink/clonefile materialization.
    ///
    /// Returns `Some(path)` only when the loose file exists *and* is
    /// stored uncompressed — then the on-disk bytes are byte-identical
    /// to the blob's content, so a hard link materializes the worktree
    /// file without an extra copy. Compressed blobs and pack-only blobs
    /// fall through to `None` and the caller writes decompressed bytes
    /// the slow way.
    fn loose_blob_path(&self, hash: &ContentHash) -> Option<PathBuf> {
        let path = hash_path(&blobs_dir(&self.root), hash);
        // 9 bytes is enough to recognise the modern compression header
        // (LEGACY_COMPRESSED_HEADER_LEN = 5 also fits inside).
        let header = read_file_header(&path, 9).ok().flatten()?;
        if is_compressed(&header.0) {
            return None;
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

        // Atomically install the uncompressed bytes at the canonical
        // loose path. `write_loose_object_atomic` writes to a temp
        // path in the same parent dir and `rename(2)`s — so a
        // concurrent reader either sees the old contents (compressed
        // header → falls through to `get_blob` → still correct) or
        // the new contents (uncompressed → safe to hardlink).
        debug!(
            size = blob.content().len(),
            "Promoting blob to loose-uncompressed canonical store"
        );
        self.write_loose_object_atomic(&path, blob.content())?;
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
        Ok(None)
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
        Ok(None)
    }

    #[instrument(skip(self, tree), fields(entry_count = tree.entries().len()))]
    fn put_tree(&self, tree: &Tree) -> Result<ContentHash> {
        let hash = tree.hash();
        let path = hash_path(&trees_dir(&self.root), &hash);

        if !path.exists() {
            let serialized = rmp_serde::to_vec(tree)?;
            let data = compress(&serialized, &self.compression)?.unwrap_or(serialized);
            trace!(compressed_size = data.len(), "Writing tree");
            self.write_loose_object_atomic(&path, &data)?;
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
        let tree: Tree = rmp_serde::from_slice(data)?;
        validate_loaded_tree(tree.clone())?;
        let found = tree.hash();
        if found != hash {
            return Err(HeddleError::Corruption {
                expected: hash,
                found,
            });
        }

        let path = hash_path(&trees_dir(&self.root), &hash);
        if !path.exists() {
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
        if self.reload_packs_if_stale()? {
            return self.try_has_tree_once(hash);
        }
        Ok(false)
    }

    #[instrument(skip(self), fields(id = %id.short()))]
    fn get_state(&self, id: &ChangeId) -> Result<Option<State>> {
        if let Some(state) = self.try_get_state_once(id)? {
            return Ok(Some(state));
        }
        if self.reload_packs_if_stale()?
            && let Some(state) = self.try_get_state_once(id)?
        {
            return Ok(Some(state));
        }
        trace!("State not found");
        Ok(None)
    }

    #[instrument(skip(self, state), fields(id = %state.change_id.short()))]
    fn put_state(&self, state: &State) -> Result<()> {
        let path = state_path(&self.root, &state.change_id);
        let serialized = rmp_serde::to_vec(state)?;
        let data = compress(&serialized, &self.compression)?.unwrap_or(serialized);
        trace!(compressed_size = data.len(), "Writing state");
        self.write_loose_object_atomic(&path, &data)?;
        if let Ok(mut cache) = self.recent_states.write() {
            cache.insert(state.change_id, state.clone());
        }
        Ok(())
    }

    #[instrument(skip(self, data), fields(id = %id.short(), size = data.len()))]
    fn put_state_serialized(&self, data: &[u8], id: ChangeId) -> Result<()> {
        let state: State = rmp_serde::from_slice(data)?;
        if state.change_id != id {
            return Err(HeddleError::InvalidObject(format!(
                "state change_id mismatch: expected {}, found {}",
                id, state.change_id
            )));
        }
        let path = state_path(&self.root, &id);
        trace!(size = data.len(), "Writing raw serialized state");
        self.write_loose_object_atomic(&path, data)?;
        if let Ok(mut cache) = self.recent_states.write() {
            cache.insert(id, state);
        }
        Ok(())
    }

    #[instrument(skip(self), fields(id = %id.short()))]
    fn has_state(&self, id: &ChangeId) -> Result<bool> {
        if self.try_has_state_once(id)? {
            return Ok(true);
        }
        if self.reload_packs_if_stale()? {
            return self.try_has_state_once(id);
        }
        Ok(false)
    }

    #[instrument(skip(self))]
    fn list_states(&self) -> Result<Vec<ChangeId>> {
        let dir = states_dir(&self.root);
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut states = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if let Some(name) = path.file_stem()
                && let Some(name_str) = name.to_str()
                && let Ok(id) = ChangeId::parse(name_str)
            {
                states.push(id);
            }
        }
        if let Ok(manager) = self.pack_manager().read() {
            for id in manager.list_all_ids()? {
                if let PackObjectId::ChangeId(change_id) = id
                    && !states.contains(&change_id)
                {
                    states.push(change_id);
                }
            }
        }
        debug!(count = states.len(), "Listed states");
        Ok(states)
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
                let decoded = if is_compressed(data.as_slice()) {
                    decompress(data.as_slice())?
                } else {
                    data.into_vec()
                };
                let action = validate_loaded_action(id, rmp_serde::from_slice(&decoded)?)?;
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
            let serialized = rmp_serde::to_vec(action)?;
            let data = compress(&serialized, &self.compression)?.unwrap_or(serialized);
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
            PackObjectId::ChangeId(change_id) => {
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
        let reader =
            crate::store::pack::PackReader::from_bytes(pack_data.to_vec(), index_data.to_vec())?;
        let ids = reader.list_ids();
        self.install_pack_files(pack_data, index_data)?;
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
    ) -> Result<()> {
        self.install_pack_files_streaming(pack_path, index_path)
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
            fs::create_dir_all(&dir)?;
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
}
