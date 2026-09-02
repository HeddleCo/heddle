// SPDX-License-Identifier: Apache-2.0
//! ObjectStore implementation for FsStore.

use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use fs2::FileExt;
use heddle_format::compression::{header_uncompressed_size, is_compressed};
use tracing::{debug, instrument, trace};

use super::{
    FsStore,
    fs_io::{list_hashes_from_dir, read_file_bytes, read_file_header},
    fs_paths::{
        action_path, actions_dir, annotated_tags_dir, blobs_dir, hash_path, redaction_path,
        redactions_dir, state_attachment_index_lock_path, state_attachment_index_path,
        state_attachment_path, state_attachments_dir, state_path, state_visibility_dir,
        state_visibility_path, states_dir, tree_lineage_path, trees_dir,
    },
};
use crate::{
    object::{
        Action, ActionId, AnnotatedTag, Blob, BytesTreeSource, ContentHash, FileTreeSource,
        OpenedTreeBody, State, StateAttachment, StateAttachmentId, StateId, TREE_CANONICAL_MAGIC,
        TREE_DELTA_HEADER_LEN, TREE_DELTA_MAGIC, TREE_LEAN_MAGIC, Tree, TreeByteSource, TreeEntry,
        TreeEntryReader, TreeResumeCursor, decode_tree_delta_header,
        decode_tree_delta_header_prefix, is_delta_tree, is_streamable_tree,
    },
    store::{
        HeddleError, ObjectStore, Result, SidecarStore, SnapshotCommitDescriptor, TreeWrite, codec,
        codec::{EncodedTree, TreeDeltaBase, TreeEncodingKind, TreeLineage},
        delta_source::DeltaTreeSource,
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
    let tree = codec::decode_tree_serialized_with_key(data, hash, None)?;
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

fn validate_annotated_tag(data: &[u8], hash: ContentHash) -> Result<AnnotatedTag> {
    let tag = AnnotatedTag::decode_current_msgpack(data)
        .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
    if tag.hash() != hash {
        return Err(HeddleError::Corruption {
            expected: hash,
            found: tag.hash(),
        });
    }
    Ok(tag)
}

fn validate_loaded_state(requested_id: &StateId, mut state: State) -> Result<State> {
    if !state.accepts_stored_id(requested_id) {
        return Err(HeddleError::InvalidObject(format!(
            "state id mismatch: requested {requested_id}, computed {}",
            state.id()
        )));
    }
    state.state_id = *requested_id;
    Ok(state)
}

pub(super) fn validate_state_serialized(data: &[u8], id: StateId) -> Result<State> {
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

trait EnumerationCounter {
    fn membership_check(&mut self);
    fn header_read(&mut self);
}

struct NoopEnumerationCounter;

impl EnumerationCounter for NoopEnumerationCounter {
    fn membership_check(&mut self) {}
    fn header_read(&mut self) {}
}

fn append_packed_hashes_with_counter(
    hashes: &mut Vec<ContentHash>,
    manager: &PackManager,
    expected_type: ObjectType,
    counter: &mut impl EnumerationCounter,
) -> Result<()> {
    let mut known: HashSet<_> = hashes.iter().copied().collect();
    for id in manager.list_all_ids()? {
        let hash = match id {
            PackObjectId::Hash(hash) if expected_type != ObjectType::AnnotatedTag => hash,
            PackObjectId::AnnotatedTag(hash) if expected_type == ObjectType::AnnotatedTag => hash,
            PackObjectId::Hash(_) | PackObjectId::StateId(_) | PackObjectId::AnnotatedTag(_) => {
                continue;
            }
        };
        counter.membership_check();
        if known.contains(&hash) {
            continue;
        }
        counter.header_read();
        let found_type = if expected_type == ObjectType::AnnotatedTag {
            manager
                .get_object(&PackObjectId::AnnotatedTag(hash))?
                .map(|(object_type, _)| object_type)
        } else {
            manager.get_hashed_object_type(&hash)?
        };
        if found_type == Some(expected_type) {
            known.insert(hash);
            hashes.push(hash);
        }
    }
    Ok(())
}

fn append_packed_hashes(
    hashes: &mut Vec<ContentHash>,
    manager: &PackManager,
    expected_type: ObjectType,
) -> Result<()> {
    append_packed_hashes_with_counter(hashes, manager, expected_type, &mut NoopEnumerationCounter)
}

fn append_unique_states(
    states: &mut Vec<StateId>,
    known: &mut HashSet<StateId>,
    incoming: impl IntoIterator<Item = StateId>,
) {
    for id in incoming {
        if known.insert(id) {
            states.push(id);
        }
    }
}

impl FsStore {
    /// Publish the authoritative loose copies of packed states behind one
    /// parent-directory durability barrier. A later pack may refresh mutable
    /// tail fields under the same StateId, so the loose bodies cannot be
    /// dropped even though the pack already contains each state.
    fn write_packed_state_mirrors_batch(&self, states: Vec<(StateId, Vec<u8>)>) -> Result<()> {
        if states.is_empty() {
            return Ok(());
        }

        self.begin_snapshot_write_batch_impl()?;
        for (id, data) in states {
            if let Err(error) = ObjectStore::put_state_serialized(self, &data, id) {
                self.abort_snapshot_write_batch_impl();
                return Err(error);
            }
        }
        if let Err(error) = self.flush_snapshot_write_batch_impl() {
            self.abort_snapshot_write_batch_impl();
            return Err(error);
        }
        Ok(())
    }

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

    fn collect_state_attachment_ids(&self, state: &StateId) -> Result<Vec<StateAttachmentId>> {
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
        Ok(ids)
    }

    fn rebuild_state_attachment_index(&self, state: &StateId) -> Result<Vec<StateAttachmentId>> {
        #[cfg(test)]
        fs::write(
            state_attachment_index_path(&self.root, state).with_extension("rebuild-marker"),
            b"rebuilt",
        )?;
        let ids = self.collect_state_attachment_ids(state)?;
        let path = state_attachment_index_path(&self.root, state);
        self.write_loose_object_atomic(&path, &rmp_serde::to_vec_named(&ids)?)?;
        Ok(ids)
    }

    /// Publish the attachment index for objects already made durable in a
    /// snapshot pack. This sidecar is only a materialized view: if a crash
    /// loses it, [`rebuild_state_attachment_index`](Self::rebuild_state_attachment_index)
    /// reconstructs it by scanning authoritative loose objects and packs.
    pub(super) fn materialize_packed_attachment_index(
        &self,
        state: &StateId,
        packed_ids: &[StateAttachmentId],
        state_was_present: bool,
    ) -> Result<()> {
        if packed_ids.is_empty() {
            return Ok(());
        }
        self.with_state_attachment_index_lock(state, || {
            let path = state_attachment_index_path(&self.root, state);
            let mut ids = if state_was_present {
                match read_file_bytes(&path)? {
                    Some(bytes) => rmp_serde::from_slice(bytes.as_slice())?,
                    None => self.collect_state_attachment_ids(state)?,
                }
            } else {
                Vec::new()
            };
            ids.extend_from_slice(packed_ids);
            ids.sort();
            ids.dedup();
            self.write_reconstructible_cache(&path, &rmp_serde::to_vec_named(&ids)?)?;
            Ok(())
        })
    }
}

/// Validate every entry in a pack against its tagged id (checksum
/// validation) and return the installed id list. This is the shared
/// validated core for both install seams: the byte-buffer install
/// (`install_pack`) and the memory-bounded temp-file install
/// (`install_pack_streaming`) both run their pack through here, so
/// both apply the same checksum validation and report the same
/// installed ids regardless of how the bytes reach the store.
fn validate_and_list_pack(
    store: &FsStore,
    reader: &crate::store::pack::PackReader,
) -> Result<Vec<PackObjectId>> {
    let ids = reader.list_ids()?;
    reader.visit_objects(|id, object_type, data| {
        if let (PackObjectId::Hash(hash), ObjectType::Tree) = (id, object_type)
            && is_delta_tree(data)
        {
            let header = decode_tree_delta_header(data)?;
            if header.anchor == hash {
                return Err(HeddleError::InvalidObject(
                    "HDC1 result id must differ from its anchor id".to_string(),
                ));
            }
            let anchor_body = match reader.get_object(&PackObjectId::Hash(header.anchor))? {
                Some((ObjectType::Tree, body)) => Some(body),
                Some((kind, _)) => {
                    return Err(HeddleError::InvalidObject(format!(
                        "HDC1 anchor {} is indexed as {kind:?}, expected Tree",
                        header.anchor
                    )));
                }
                None => store.try_get_tree_serialized_once(&header.anchor)?,
            }
            .ok_or_else(|| HeddleError::NotFound(format!("tree delta anchor {}", header.anchor)))?;
            if is_delta_tree(&anchor_body) {
                return Err(HeddleError::InvalidObject(
                    "HDC1 anchor must be materialized; delta chains are forbidden".to_string(),
                ));
            }
            let anchor = codec::decode_tree_serialized_with_key(&anchor_body, header.anchor, None)?;
            codec::decode_tree_serialized_with_key(data, hash, Some(&anchor))?;
            return Ok(());
        }
        validate_pack_entry(&id, object_type, data)
    })?;
    Ok(ids)
}

fn state_entries_from_pack(
    reader: &crate::store::pack::PackReader,
    ids: &[PackObjectId],
) -> Result<Vec<(StateId, Vec<u8>)>> {
    let mut states = Vec::new();
    let expected = ids.iter().copied().collect::<HashSet<_>>();
    reader.visit_objects(|id, object_type, data| {
        if !expected.contains(&id) {
            return Err(HeddleError::InvalidObject(
                "pack visitor yielded an unindexed object".into(),
            ));
        }
        if let PackObjectId::StateId(state_id) = id {
            if object_type != ObjectType::State {
                return Err(HeddleError::InvalidObject(format!(
                    "pack id {} is indexed as {object_type:?}, expected State",
                    state_id.to_string_full()
                )));
            }
            validate_state_serialized(data, state_id)?;
            states.push((state_id, data.to_vec()));
        }
        Ok(())
    })?;
    Ok(states)
}

fn attachment_entries_from_pack(
    reader: &crate::store::pack::PackReader,
    ids: &[PackObjectId],
) -> Result<Vec<StateAttachment>> {
    let mut attachments = Vec::new();
    let expected = ids.iter().copied().collect::<HashSet<_>>();
    reader.visit_objects(|id, object_type, data| {
        if expected.contains(&id) && object_type == ObjectType::StateAttachment {
            attachments.push(rmp_serde::from_slice(data)?);
        }
        Ok(())
    })?;
    Ok(attachments)
}

pub(super) fn validate_pack_entry(
    id: &PackObjectId,
    obj_type: ObjectType,
    data: &[u8],
) -> Result<()> {
    match (id, obj_type) {
        (PackObjectId::Hash(hash), ObjectType::Blob) => validate_blob_bytes(data, *hash),
        (PackObjectId::AnnotatedTag(hash), ObjectType::AnnotatedTag) => {
            validate_annotated_tag(data, *hash).map(|_| ())
        }
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
        (PackObjectId::Hash(hash), ObjectType::SnapshotCommit) => {
            let artifact: crate::store::SnapshotCommitArtifact = rmp_serde::from_slice(data)?;
            artifact.validate()?;
            if artifact.id() != *hash {
                return Err(HeddleError::InvalidObject(
                    "snapshot commit artifact pack id mismatch".to_string(),
                ));
            }
            Ok(())
        }
        (_, ObjectType::TimelineOperation) => Err(HeddleError::InvalidObject(
            "timeline operations belong in the timeline pack store".to_string(),
        )),
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

    fn cache_recent_tree(&self, hash: ContentHash, tree: &Tree) {
        if let Ok(mut cache) = self.recent_trees.write() {
            cache.insert(hash, tree.clone());
        }
    }

    fn cache_recent_state(&self, id: StateId, state: &State) {
        if let Ok(mut cache) = self.recent_states.write() {
            cache.insert(id, state.clone());
        }
    }

    fn recent_blob(&self, hash: &ContentHash) -> Option<Blob> {
        self.recent_blobs
            .read()
            .ok()
            .and_then(|cache| cache.get(hash).cloned())
    }

    fn recent_tree(&self, hash: &ContentHash) -> Option<Tree> {
        self.recent_trees
            .read()
            .ok()
            .and_then(|cache| cache.get(hash).cloned())
    }

    fn recent_state(&self, id: &StateId) -> Option<State> {
        self.recent_states
            .read()
            .ok()
            .and_then(|cache| cache.get(id).cloned())
    }

    /// Single-pass blob lookup. The wrapper in `ObjectStore::get_blob`
    /// retries this once after a stale-reload on miss.
    fn try_get_blob_once(&self, hash: &ContentHash) -> Result<Option<Blob>> {
        // Cache first — avoid `path.exists()` / pack probes on warm hits.
        // Access bits are atomic, so hits remain concurrent under a read lock.
        if let Ok(cache) = self.recent_blobs.read()
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
            validate_blob_bytes(&data, *hash)?;
            let blob = Blob::new(data);
            heddle_perf_contract::record_object_decode();
            self.cache_recent_blob(*hash, &blob);
            return Ok(Some(blob));
        }

        let path = hash_path(&blobs_dir(&self.root), hash);
        match read_file_bytes(&path)? {
            Some(data) => {
                trace!(size = data.as_slice().len(), "Blob data read");
                let content = codec::decode_blob_content(data.as_slice())?;
                let blob = Blob::new(content);
                heddle_perf_contract::record_object_decode();
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
        // This is the native-ownership probe used by `has_blob_locally`.
        // Recent-object entries may be read-through values from an external
        // Git overlay, so cache presence cannot establish local durability.
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

    fn try_open_tree_once(
        &self,
        tree_id: &ContentHash,
        cursor: Option<&TreeResumeCursor>,
    ) -> Result<Option<TreeEntryReader<OpenedTreeBody>>> {
        let path = hash_path(&trees_dir(&self.root), tree_id);
        if path.exists()
            && let Some((header, len)) = read_file_header(&path, TREE_CANONICAL_MAGIC.len())?
        {
            if header.starts_with(TREE_CANONICAL_MAGIC) || header.starts_with(TREE_LEAN_MAGIC) {
                let file = File::open(&path)?;
                return Ok(Some(TreeEntryReader::open(
                    OpenedTreeBody::File(FileTreeSource::sequential_verify(file, len)),
                    *tree_id,
                    cursor,
                )?));
            }
            if header.starts_with(TREE_DELTA_MAGIC) {
                let file = File::open(&path)?;
                return self.open_delta_tree_source(
                    *tree_id,
                    cursor,
                    OpenedTreeBody::File(FileTreeSource::sequential_verify(file, len)),
                );
            }
        }
        if path.exists()
            && let Some(data) = read_file_bytes(&path)?
        {
            let body = codec::decode_tree_body(data.as_slice())?;
            if is_streamable_tree(&body) {
                return Ok(Some(TreeEntryReader::open(
                    OpenedTreeBody::Bytes(BytesTreeSource::sequential_verify(body)),
                    *tree_id,
                    cursor,
                )?));
            }
            if is_delta_tree(&body) {
                return self.open_delta_tree_source(
                    *tree_id,
                    cursor,
                    OpenedTreeBody::Bytes(BytesTreeSource::sequential_verify(body)),
                );
            }
        }
        let packed = if let Ok(manager) = self.pack_manager().read() {
            manager.get_hashed_object(tree_id)?
        } else {
            None
        };
        if let Some((ObjectType::Tree, data)) = packed {
            if is_streamable_tree(&data) {
                return Ok(Some(TreeEntryReader::open(
                    OpenedTreeBody::Bytes(BytesTreeSource::sequential_verify(data)),
                    *tree_id,
                    cursor,
                )?));
            }
            if is_delta_tree(&data) {
                return self.open_delta_tree_source(
                    *tree_id,
                    cursor,
                    OpenedTreeBody::Bytes(BytesTreeSource::sequential_verify(data)),
                );
            }
        }
        let npk_tree = if let Ok(manager) = self.npk1_manager().read() {
            manager.get_tree(tree_id)?
        } else {
            None
        };
        if let Some(tree) = npk_tree {
            return Ok(Some(TreeEntryReader::open(
                OpenedTreeBody::Bytes(BytesTreeSource::sequential_verify(tree.encode_lean()?)),
                *tree_id,
                cursor,
            )?));
        }
        Ok(None)
    }

    fn open_delta_tree_source(
        &self,
        tree_id: ContentHash,
        cursor: Option<&TreeResumeCursor>,
        mut delta: OpenedTreeBody,
    ) -> Result<Option<TreeEntryReader<OpenedTreeBody>>> {
        let object_len = usize::try_from(delta.len())
            .map_err(|_| HeddleError::InvalidObject("HDC1 body exceeds usize".to_string()))?;
        let mut header_bytes = [0u8; TREE_DELTA_HEADER_LEN];
        delta.read_exact_at(0, &mut header_bytes)?;
        let header = decode_tree_delta_header_prefix(&header_bytes, object_len)?;
        let anchor = self
            .try_open_materialized_tree_once(&header.anchor)?
            .ok_or_else(|| HeddleError::NotFound(format!("tree delta anchor {}", header.anchor)))?;
        let source = DeltaTreeSource::open(delta, anchor)?;
        Ok(Some(TreeEntryReader::open(
            OpenedTreeBody::Dynamic(Box::new(source)),
            tree_id,
            cursor,
        )?))
    }

    fn try_open_materialized_tree_once(
        &self,
        tree_id: &ContentHash,
    ) -> Result<Option<TreeEntryReader<OpenedTreeBody>>> {
        let path = hash_path(&trees_dir(&self.root), tree_id);
        if path.exists()
            && let Some((header, len)) = read_file_header(&path, TREE_CANONICAL_MAGIC.len())?
        {
            if header.starts_with(TREE_DELTA_MAGIC) {
                return Err(HeddleError::InvalidObject(
                    "HDC1 anchor must be materialized; delta chains are forbidden".to_string(),
                ));
            }
            if header.starts_with(TREE_CANONICAL_MAGIC) || header.starts_with(TREE_LEAN_MAGIC) {
                let file = File::open(&path)?;
                return Ok(Some(TreeEntryReader::open(
                    OpenedTreeBody::File(FileTreeSource::sequential_verify(file, len)),
                    *tree_id,
                    None,
                )?));
            }
        }
        if path.exists()
            && let Some(data) = read_file_bytes(&path)?
        {
            let body = codec::decode_tree_body(data.as_slice())?;
            if is_delta_tree(&body) {
                return Err(HeddleError::InvalidObject(
                    "HDC1 anchor must be materialized; delta chains are forbidden".to_string(),
                ));
            }
            if is_streamable_tree(&body) {
                return Ok(Some(TreeEntryReader::open(
                    OpenedTreeBody::Bytes(BytesTreeSource::sequential_verify(body)),
                    *tree_id,
                    None,
                )?));
            }
        }
        let packed = if let Ok(manager) = self.pack_manager().read() {
            manager.get_hashed_object(tree_id)?
        } else {
            None
        };
        if let Some((ObjectType::Tree, data)) = packed {
            if is_delta_tree(&data) {
                return Err(HeddleError::InvalidObject(
                    "HDC1 anchor must be materialized; delta chains are forbidden".to_string(),
                ));
            }
            if is_streamable_tree(&data) {
                return Ok(Some(TreeEntryReader::open(
                    OpenedTreeBody::Bytes(BytesTreeSource::sequential_verify(data)),
                    *tree_id,
                    None,
                )?));
            }
        }
        let npk_tree = if let Ok(manager) = self.npk1_manager().read() {
            manager.get_tree(tree_id)?
        } else {
            None
        };
        if let Some(tree) = npk_tree {
            return Ok(Some(TreeEntryReader::open(
                OpenedTreeBody::Bytes(BytesTreeSource::sequential_verify(tree.encode_lean()?)),
                *tree_id,
                None,
            )?));
        }
        if let Some(source) = &self.external_source
            && let Some(tree) = source.get_tree(tree_id)?
        {
            return Ok(Some(TreeEntryReader::open(
                OpenedTreeBody::Bytes(BytesTreeSource::sequential_verify(tree.encode_lean()?)),
                *tree_id,
                None,
            )?));
        }
        Ok(None)
    }

    fn try_get_tree_once(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        // Cache first. The recent-object cache only ever holds trees we
        // wrote or read this process, so a hit is authoritative for a
        // read. Atomic second-chance marking keeps the map under a shared lock.
        if let Ok(cache) = self.recent_trees.read()
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
            let body = codec::decode_tree_body(data.as_slice())?;
            let tree = validate_loaded_tree(self.decode_tree_storage_body(*hash, &body)?)?;
            heddle_perf_contract::record_object_decode();
            if let Ok(mut cache) = self.recent_trees.write() {
                cache.insert(*hash, tree.clone());
            }
            return Ok(Some(tree));
        }

        if let Ok(manager) = self.npk1_manager().read()
            && let Some(tree) = manager.get_tree(hash)?
        {
            trace!("Found tree in NPK1 pack");
            heddle_perf_contract::record_object_decode();
            self.cache_recent_tree(*hash, &tree);
            return Ok(Some(tree));
        }
        if let Ok(manager) = self.pack_manager().read()
            && let Some((obj_type, data)) = manager.get_hashed_object(hash)?
            && obj_type == ObjectType::Tree
        {
            trace!("Found tree in packfile");
            let tree = validate_loaded_tree(self.decode_tree_storage_body(*hash, &data)?)?;
            heddle_perf_contract::record_object_decode();
            if let Ok(mut cache) = self.recent_trees.write() {
                cache.insert(*hash, tree.clone());
            }
            return Ok(Some(tree));
        }
        Ok(None)
    }

    fn try_get_tree_entry_once(&self, hash: &ContentHash, name: &str) -> Result<Option<TreeEntry>> {
        if let Some(tree) = self.recent_tree(hash) {
            return Ok(tree.get(name).cloned());
        }
        let path = hash_path(&trees_dir(&self.root), hash);
        if path.exists() {
            return Ok(self
                .try_get_tree_once(hash)?
                .and_then(|tree| tree.get(name).cloned()));
        }
        if let Ok(manager) = self.npk1_manager().read()
            && manager.has_tree(hash)?
        {
            return manager.get_entry(hash, name);
        }
        if let Ok(manager) = self.pack_manager().read()
            && manager.has_object(hash)
        {
            return Ok(self
                .try_get_tree_once(hash)?
                .and_then(|tree| tree.get(name).cloned()));
        }
        Ok(None)
    }

    pub(super) fn try_get_tree_serialized_once(
        &self,
        hash: &ContentHash,
    ) -> Result<Option<Vec<u8>>> {
        let path = hash_path(&trees_dir(&self.root), hash);
        if path.exists()
            && let Some(data) = read_file_bytes(&path)?
        {
            return Ok(Some(codec::decode_tree_body(data.as_slice())?));
        }

        if let Ok(manager) = self.npk1_manager().read()
            && let Some(tree) = manager.get_tree(hash)?
        {
            return tree.encode_lean().map(Some).map_err(HeddleError::from);
        }

        if let Ok(manager) = self.pack_manager().read()
            && let Some((obj_type, data)) = manager.get_hashed_object(hash)?
            && obj_type == ObjectType::Tree
        {
            return Ok(Some(data));
        }

        Ok(None)
    }

    pub(super) fn decode_tree_storage_body(&self, hash: ContentHash, data: &[u8]) -> Result<Tree> {
        let anchor = if is_delta_tree(data) {
            let header = decode_tree_delta_header(data)?;
            let anchor = if let Some(anchor_body) =
                self.try_get_tree_serialized_once(&header.anchor)?
            {
                if is_delta_tree(&anchor_body) {
                    return Err(HeddleError::InvalidObject(
                        "HDC1 anchor must be materialized; delta chains are forbidden".to_string(),
                    ));
                }
                let tree =
                    codec::decode_tree_serialized_with_key(&anchor_body, header.anchor, None)?;
                self.cache_recent_tree(header.anchor, &tree);
                Some(tree)
            } else if let Some(tree) = self.recent_tree(&header.anchor) {
                // This can only be a read-through external tree: native bodies
                // were checked above so a cached delta cannot hide a chain.
                Some(tree)
            } else if let Some(source) = &self.external_source {
                source.get_tree(&header.anchor)?
            } else {
                None
            };
            Some(anchor.ok_or_else(|| {
                HeddleError::NotFound(format!("tree delta anchor {}", header.anchor))
            })?)
        } else {
            None
        };
        codec::decode_tree_serialized_with_key(data, hash, anchor.as_ref())
    }

    pub(super) fn encode_tree_write(&self, write: &TreeWrite) -> Result<EncodedTree> {
        let Some(parent) = write.parent else {
            return codec::encode_tree_hot(&write.tree, None);
        };
        let Some(parent_body) = self.try_get_tree_serialized_once(&parent)? else {
            return codec::encode_tree_hot(&write.tree, None);
        };
        let base = if is_delta_tree(&parent_body) {
            let header = decode_tree_delta_header(&parent_body)?;
            let Some(lineage) = self.read_tree_lineage(&parent)? else {
                return codec::encode_tree_hot(&write.tree, None);
            };
            if lineage.anchor != header.anchor || lineage.depth == 0 {
                return codec::encode_tree_hot(&write.tree, None);
            }
            let anchor = if let Some((_, anchor, _)) =
                write
                    .anchor
                    .as_ref()
                    .filter(|(anchor_id, _, parent_depth)| {
                        *anchor_id == lineage.anchor && *parent_depth == lineage.depth
                    }) {
                anchor.clone()
            } else if let Some(anchor) = self.recent_tree(&lineage.anchor) {
                anchor
            } else {
                let Some(anchor_body) = self.try_get_tree_serialized_once(&lineage.anchor)? else {
                    return codec::encode_tree_hot(&write.tree, None);
                };
                if is_delta_tree(&anchor_body) {
                    return Err(HeddleError::InvalidObject(
                        "HDC1 lineage points to another delta".to_string(),
                    ));
                }
                codec::decode_tree_serialized_with_key(&anchor_body, lineage.anchor, None)?
            };
            Some((lineage.anchor, anchor, lineage.depth))
        } else {
            let anchor = match write.anchor.as_ref() {
                Some((anchor_id, anchor, 0)) if *anchor_id == parent => anchor.clone(),
                _ => codec::decode_tree_serialized_with_key(&parent_body, parent, None)?,
            };
            Some((parent, anchor, 0))
        };
        let Some((anchor_id, anchor, parent_depth)) = base else {
            return codec::encode_tree_hot(&write.tree, None);
        };
        codec::encode_tree_hot(
            &write.tree,
            Some(TreeDeltaBase {
                anchor_id,
                anchor: &anchor,
                parent_depth,
            }),
        )
    }

    fn read_tree_lineage(&self, hash: &ContentHash) -> Result<Option<TreeLineage>> {
        let Some(bytes) = read_file_bytes(&tree_lineage_path(&self.root, hash))? else {
            return Ok(None);
        };
        let data = bytes.as_slice();
        if data.len() != 33 {
            return Ok(None);
        }
        let anchor = match data[..32].try_into() {
            Ok(bytes) => ContentHash::from_bytes(bytes),
            Err(_) => return Ok(None),
        };
        let depth = data[32];
        if depth == 0 || depth >= crate::object::TREE_DELTA_ANCHOR_INTERVAL {
            return Ok(None);
        }
        Ok(Some(TreeLineage { anchor, depth }))
    }

    pub(super) fn remember_tree_encoding(
        &self,
        hash: ContentHash,
        kind: TreeEncodingKind,
    ) -> Result<()> {
        let TreeEncodingKind::Delta { anchor, depth, .. } = kind else {
            return Ok(());
        };
        let mut bytes = Vec::with_capacity(33);
        bytes.extend_from_slice(anchor.as_bytes());
        bytes.push(depth);
        self.write_reconstructible_cache(&tree_lineage_path(&self.root, &hash), &bytes)
    }

    fn try_has_tree_once(&self, hash: &ContentHash) -> Result<bool> {
        // This is the native-ownership probe used by `has_tree_locally`.
        // Recent-object entries may be read-through values from an external
        // Git overlay, so cache presence cannot establish local durability.
        let path = hash_path(&trees_dir(&self.root), hash);
        if self.loose_or_packed(&path, |m| m.has_object(hash))? {
            return Ok(true);
        }
        if let Ok(manager) = self.npk1_manager().read() {
            return manager.has_tree(hash);
        }
        Ok(false)
    }

    fn try_get_state_once(&self, id: &StateId) -> Result<Option<State>> {
        // Cache first — avoid `path.exists()` / pack probes on warm hits.
        // Atomic second-chance marking keeps hits under a shared lock. Put
        // paths and successful reads below keep the cache coherent for the
        // process.
        if let Ok(cache) = self.recent_states.read()
            && let Some(state) = cache.get(id)
        {
            trace!("Found state in recent object cache");
            return Ok(Some(state.clone()));
        }

        let path = state_path(&self.root, id);
        if let Some(data) = read_file_bytes(&path)? {
            trace!(size = data.as_slice().len(), "State read from loose object");
            let state = validate_loaded_state(id, codec::decode_state(data.as_slice())?)?;
            heddle_perf_contract::record_object_decode();
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
            heddle_perf_contract::record_object_decode();
            if let Ok(mut cache) = self.recent_states.write() {
                cache.insert(*id, state.clone());
            }
            return Ok(Some(state));
        }

        Ok(None)
    }

    fn try_has_state_once(&self, id: &StateId) -> Result<bool> {
        // Read-lock `contains`: an existence check needs no clock
        // promotion, so it must not serialize on the write lock.
        if let Ok(cache) = self.recent_states.read()
            && cache.contains(id)
        {
            return Ok(true);
        }
        let path = state_path(&self.root, id);
        self.loose_or_packed(&path, |m| m.has_object_id(&PackObjectId::StateId(*id)))
    }

    fn try_get_action_once(&self, id: &ActionId) -> Result<Option<Action>> {
        let path = action_path(&self.root, id);
        if let Some(data) = read_file_bytes(&path)? {
            trace!(size = data.as_slice().len(), "Action data read");
            return Ok(Some(validate_loaded_action(
                id,
                codec::decode_action(data.as_slice())?,
            )?));
        }
        if let Ok(manager) = self.pack_manager().read()
            && let Some((ObjectType::Action, data)) = manager.get_hashed_object(id.as_hash())?
        {
            trace!("Found action in packfile");
            return Ok(Some(validate_loaded_action(
                id,
                rmp_serde::from_slice(&data)?,
            )?));
        }
        Ok(None)
    }

    fn try_get_state_attachment_once(
        &self,
        state: &StateId,
        id: &StateAttachmentId,
    ) -> Result<Option<StateAttachment>> {
        let path = state_attachment_path(&self.root, state, id);
        let file_bytes = read_file_bytes(&path)?;
        if let Some(bytes) = file_bytes.as_ref() {
            let attachment: StateAttachment = rmp_serde::from_slice(bytes.as_slice())?;
            return Self::validate_state_attachment(attachment, state, id).map(Some);
        }
        if let Ok(manager) = self.pack_manager().read()
            && let Some((ObjectType::StateAttachment, pack_bytes)) =
                manager.get_hashed_object(id.as_hash())?
        {
            let attachment: StateAttachment = rmp_serde::from_slice(&pack_bytes)?;
            return Self::validate_state_attachment(attachment, state, id).map(Some);
        }
        Ok(None)
    }

    fn validate_state_attachment(
        attachment: StateAttachment,
        state: &StateId,
        id: &StateAttachmentId,
    ) -> Result<StateAttachment> {
        if attachment.state_id != *state || attachment.id() != *id {
            return Err(HeddleError::InvalidObject(
                "state attachment address does not match content".to_string(),
            ));
        }
        Ok(attachment)
    }
}

impl FsStore {
    /// Lightweight repository-open seam for authoritative snapshot recovery.
    #[doc(hidden)]
    pub fn snapshot_commit_recovery_descriptors(&self) -> Result<Vec<SnapshotCommitDescriptor>> {
        self.reload_packs_if_stale()?;
        let manager = self
            .pack_manager()
            .read()
            .map_err(|_| HeddleError::Config("Failed to acquire pack manager lock".to_string()))?;
        manager.snapshot_commit_recovery_descriptors()
    }

    /// Internal repository seam for the local authoritative snapshot artifact.
    /// Kept off [`ObjectStore`] so other stores do not acquire a filesystem
    /// recovery contract.
    #[doc(hidden)]
    pub fn snapshot_commit_descriptors(&self) -> Result<Vec<SnapshotCommitDescriptor>> {
        self.reload_packs_if_stale()?;
        let manager = self
            .pack_manager()
            .read()
            .map_err(|_| HeddleError::Config("Failed to acquire pack manager lock".to_string()))?;
        manager.snapshot_commit_descriptors()
    }

    /// O(1) lookup for the authoritative snapshot pack associated with a
    /// pushed state.
    #[doc(hidden)]
    pub fn snapshot_commit_descriptor_for_state(
        &self,
        state: &StateId,
    ) -> Result<Option<SnapshotCommitDescriptor>> {
        self.reload_packs_if_stale()?;
        let manager = self
            .pack_manager()
            .read()
            .map_err(|_| HeddleError::Config("Failed to acquire pack manager lock".to_string()))?;
        manager.snapshot_commit_descriptor_for_state(state)
    }
}

impl ObjectStore for FsStore {
    fn get_annotated_tag(&self, hash: &ContentHash) -> Result<Option<AnnotatedTag>> {
        let path = hash_path(&annotated_tags_dir(&self.root), hash);
        if let Some(data) = read_file_bytes(&path)? {
            return validate_annotated_tag(data.as_slice(), *hash).map(Some);
        }
        self.reload_packs_if_stale()?;
        if let Ok(manager) = self.pack_manager().read()
            && let Some((ObjectType::AnnotatedTag, data)) =
                manager.get_object(&PackObjectId::AnnotatedTag(*hash))?
        {
            return validate_annotated_tag(&data, *hash).map(Some);
        }
        Ok(None)
    }

    fn put_annotated_tag(&self, tag: &AnnotatedTag) -> Result<ContentHash> {
        let hash = tag.hash();
        let path = hash_path(&annotated_tags_dir(&self.root), &hash);
        if !path.exists() {
            self.write_loose_object_atomic(&path, &tag.encode_current_msgpack())?;
        }
        Ok(hash)
    }

    fn list_annotated_tags(&self) -> Result<Vec<ContentHash>> {
        self.reload_packs_if_stale()?;
        let mut hashes = list_hashes_from_dir(&annotated_tags_dir(&self.root))?;
        if let Ok(manager) = self.pack_manager().read() {
            append_packed_hashes(&mut hashes, &manager, ObjectType::AnnotatedTag)?;
        }
        Ok(hashes)
    }

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
            validate_blob_bytes(data.as_ref(), *hash)?;
            return Ok(Some(data));
        }
        Ok(self
            .get_blob(hash)?
            .map(|blob| bytes::Bytes::from(blob.into_content())))
    }

    #[instrument(skip(self), fields(hash = %hash.short()))]
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>> {
        if let Some(blob) = self.recent_blob(hash) {
            return Ok(Some(blob));
        }
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
        if let Some(source) = &self.external_source
            && let Some(blob) = source.get_blob(hash)?
        {
            self.cache_recent_blob(*hash, &blob);
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
        if ObjectStore::has_blob_locally(self, hash)? {
            return Ok(true);
        }
        if let Some(source) = &self.external_source {
            if self.recent_blob(hash).is_some() {
                return Ok(true);
            }
            if let Some(blob) = source.get_blob(hash)? {
                self.cache_recent_blob(*hash, &blob);
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn has_blob_locally(&self, hash: &ContentHash) -> Result<bool> {
        if self.try_has_blob_once(hash)? {
            return Ok(true);
        }
        Ok(self.reload_packs_if_stale()? && self.try_has_blob_once(hash)?)
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

        // External-only overlay blobs stay external. If Heddle also owns a
        // native copy, promotion is a native storage optimization and does not
        // cross the source-authority boundary.
        if !ObjectStore::has_blob_locally(self, hash)?
            && let Some(source) = &self.external_source
            && (self.recent_blob(hash).is_some() || source.get_blob(hash)?.is_some())
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
        if let Some(source) = &self.external_source {
            if let Some(blob) = self.recent_blob(hash) {
                return Ok(Some(blob.content().len() as u64));
            }
            if let Some(blob) = source.get_blob(hash)? {
                let size = blob.content().len() as u64;
                self.cache_recent_blob(*hash, &blob);
                return Ok(Some(size));
            }
        }
        Ok(None)
    }

    #[instrument(skip(self), fields(hash = %hash.short()))]
    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        if let Some(tree) = self.recent_tree(hash) {
            return Ok(Some(tree));
        }
        if let Some(tree) = self.try_get_tree_once(hash)? {
            return Ok(Some(tree));
        }
        if self.reload_packs_if_stale()?
            && let Some(tree) = self.try_get_tree_once(hash)?
        {
            return Ok(Some(tree));
        }
        if let Some(source) = &self.external_source
            && let Some(tree) = source.get_tree(hash)?
        {
            self.cache_recent_tree(*hash, &tree);
            return Ok(Some(tree));
        }
        trace!("Tree not found");
        Ok(None)
    }

    #[instrument(skip(self), fields(hash = %hash.short(), name))]
    fn get_tree_entry(&self, hash: &ContentHash, name: &str) -> Result<Option<TreeEntry>> {
        if let Some(entry) = self.try_get_tree_entry_once(hash, name)? {
            return Ok(Some(entry));
        }
        if self.reload_packs_if_stale()?
            && let Some(entry) = self.try_get_tree_entry_once(hash, name)?
        {
            return Ok(Some(entry));
        }
        if let Some(source) = &self.external_source
            && let Some(tree) = source.get_tree(hash)?
        {
            let entry = tree.get(name).cloned();
            self.cache_recent_tree(*hash, &tree);
            return Ok(entry);
        }
        Ok(None)
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
        let external_tree = if let Some(tree) = self.recent_tree(hash) {
            Some(tree)
        } else if let Some(source) = &self.external_source {
            let tree = source.get_tree(hash)?;
            if let Some(tree) = &tree {
                self.cache_recent_tree(*hash, tree);
            }
            tree
        } else {
            None
        };
        if let Some(tree) = external_tree {
            return tree.encode_canonical().map(Some).map_err(HeddleError::from);
        }
        Ok(None)
    }

    fn open_tree(
        &self,
        tree_id: &ContentHash,
        cursor: Option<&TreeResumeCursor>,
    ) -> Result<Option<TreeEntryReader<OpenedTreeBody>>> {
        if let Some(reader) = self.try_open_tree_once(tree_id, cursor)? {
            return Ok(Some(reader));
        }
        if self.reload_packs_if_stale()?
            && let Some(reader) = self.try_open_tree_once(tree_id, cursor)?
        {
            return Ok(Some(reader));
        }
        if let Some(data) = ObjectStore::get_tree_serialized(self, tree_id)? {
            let body = if is_streamable_tree(&data) {
                data
            } else if data.starts_with(TREE_DELTA_MAGIC) {
                self.get_tree(tree_id)?
                    .ok_or_else(|| HeddleError::NotFound(format!("tree {tree_id}")))?
                    .encode_lean()?
            } else {
                return Ok(None);
            };
            return Ok(Some(TreeEntryReader::open(
                OpenedTreeBody::Bytes(BytesTreeSource::sequential_verify(body)),
                *tree_id,
                cursor,
            )?));
        }
        Ok(None)
    }

    #[instrument(skip(self, tree), fields(entry_count = tree.entries().len()))]
    fn put_tree(&self, tree: &Tree) -> Result<ContentHash> {
        let hash = tree.hash();
        let path = hash_path(&trees_dir(&self.root), &hash);

        // `put_tree` is an ownership boundary: a native state that references
        // this tree must survive loss or pruning of an overlay read-through
        // source. Descriptor-only states do not call this method; they retain
        // their explicit external-source semantics.
        if !ObjectStore::has_tree_locally(self, &hash)? {
            let (_, data) = codec::encode_tree(tree, &self.compression)?;
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
        let tree = validate_loaded_tree(self.decode_tree_storage_body(hash, data)?)?;

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
        if ObjectStore::has_tree_locally(self, hash)? {
            return Ok(true);
        }
        if let Some(source) = &self.external_source {
            if self.recent_tree(hash).is_some() {
                return Ok(true);
            }
            if let Some(tree) = source.get_tree(hash)? {
                self.cache_recent_tree(*hash, &tree);
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn has_tree_locally(&self, hash: &ContentHash) -> Result<bool> {
        if self.try_has_tree_once(hash)? {
            return Ok(true);
        }
        Ok(self.reload_packs_if_stale()? && self.try_has_tree_once(hash)?)
    }

    #[instrument(skip(self), fields(id = %id.short()))]
    fn get_state(&self, id: &StateId) -> Result<Option<State>> {
        if let Some(state) = self.recent_state(id) {
            return Ok(Some(state));
        }
        if let Some(state) = self.try_get_state_once(id)? {
            return Ok(Some(state));
        }
        if self.reload_packs_if_stale()?
            && let Some(state) = self.try_get_state_once(id)?
        {
            return Ok(Some(state));
        }
        if let Some(source) = &self.external_source
            && let Some(state) = source.get_state(id)?
        {
            self.cache_recent_state(*id, &state);
            return Ok(Some(state));
        }
        trace!("State not found");
        Ok(None)
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
        if let Some(source) = &self.external_source {
            if self.recent_state(id).is_some() {
                return Ok(true);
            }
            if let Some(state) = source.get_state(id)? {
                self.cache_recent_state(*id, &state);
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[instrument(skip(self))]
    fn list_states(&self) -> Result<Vec<StateId>> {
        self.reload_packs_if_stale()?;

        let mut states = Vec::new();
        let mut known = HashSet::new();
        let dir = states_dir(&self.root);
        if dir.exists() {
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                if let Some(name) = path.file_stem()
                    && let Some(name_str) = name.to_str()
                    && let Ok(id) = StateId::parse(name_str)
                    && known.insert(id)
                {
                    states.push(id);
                }
            }
        }
        if let Ok(manager) = self.pack_manager().read() {
            append_unique_states(
                &mut states,
                &mut known,
                manager
                    .list_all_ids()?
                    .into_iter()
                    .filter_map(|id| match id {
                        PackObjectId::StateId(state) => Some(state),
                        PackObjectId::Hash(_) | PackObjectId::AnnotatedTag(_) => None,
                    }),
            );
        }
        if let Some(source) = &self.external_source {
            append_unique_states(&mut states, &mut known, source.list_states()?);
        }
        debug!(count = states.len(), "Listed states");
        Ok(states)
    }

    fn get_state_attachment(
        &self,
        state: &StateId,
        id: &StateAttachmentId,
    ) -> Result<Option<StateAttachment>> {
        if let Some(attachment) = self.try_get_state_attachment_once(state, id)? {
            return Ok(Some(attachment));
        }
        if self.reload_packs_if_stale()? {
            return self.try_get_state_attachment_once(state, id);
        }
        Ok(None)
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
        if let Some(action) = self.try_get_action_once(id)? {
            return Ok(Some(action));
        }
        if self.reload_packs_if_stale()? {
            return self.try_get_action_once(id);
        }
        trace!("Action not found");
        Ok(None)
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
        self.reload_packs_if_stale()?;
        let dir = actions_dir(&self.root);
        let mut action_hashes = Vec::new();
        if dir.exists() {
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                if let Some(name) = path.file_stem()
                    && let Some(name_str) = name.to_str()
                    && let Ok(hash) = ContentHash::from_hex(name_str)
                {
                    action_hashes.push(hash);
                }
            }
        }
        if let Ok(manager) = self.pack_manager().read() {
            append_packed_hashes(&mut action_hashes, &manager, ObjectType::Action)?;
        }
        let actions = action_hashes
            .into_iter()
            .map(ActionId::from_hash)
            .collect::<Vec<_>>();
        debug!(count = actions.len(), "Listed actions");
        Ok(actions)
    }

    #[instrument(skip(self))]
    fn list_blobs(&self) -> Result<Vec<ContentHash>> {
        self.reload_packs_if_stale()?;
        let dir = blobs_dir(&self.root);
        let mut blobs = list_hashes_from_dir(&dir)?;
        if let Ok(manager) = self.pack_manager().read() {
            append_packed_hashes(&mut blobs, &manager, ObjectType::Blob)?;
        }
        Ok(blobs)
    }

    #[instrument(skip(self))]
    fn list_trees(&self) -> Result<Vec<ContentHash>> {
        self.reload_packs_if_stale()?;
        let dir = trees_dir(&self.root);
        let mut trees = list_hashes_from_dir(&dir)?;
        if let Ok(manager) = self.pack_manager().read() {
            append_packed_hashes(&mut trees, &manager, ObjectType::Tree)?;
        }
        if let Ok(manager) = self.npk1_manager().read() {
            trees.extend(manager.list_ids()?);
        }
        trees.sort();
        trees.dedup();
        Ok(trees)
    }

    #[instrument(skip(self))]
    fn pack_objects(&self, delta_search: bool) -> Result<(u64, u64)> {
        self.pack_objects_impl(delta_search)
    }

    #[instrument(skip(self), fields(id = ?id))]
    fn get_pack_object(&self, id: &PackObjectId) -> Result<Option<(ObjectType, Vec<u8>)>> {
        if let Ok(manager) = self.pack_manager().read()
            && let Some((obj_type, data)) = manager.get_object(id)?
        {
            return Ok(Some((obj_type, data)));
        }

        match id {
            PackObjectId::AnnotatedTag(hash) => Ok(self
                .get_annotated_tag(hash)?
                .map(|tag| (ObjectType::AnnotatedTag, tag.encode_current_msgpack()))),
            PackObjectId::Hash(hash) => {
                if let Some(blob) = self.get_blob(hash)? {
                    return Ok(Some((ObjectType::Blob, blob.into_content())));
                }
                // Raw canonical storage body: skips a full tree decode +
                // re-encode on the pack-building path (the receiver installs
                // through `put_tree_serialized`).
                if let Some(tree_data) = self.get_tree_serialized(hash)? {
                    return Ok(Some((ObjectType::Tree, tree_data)));
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
        let ids = validate_and_list_pack(self, &reader)?;
        let state_entries = state_entries_from_pack(&reader, &ids)?;
        let attachment_entries = attachment_entries_from_pack(&reader, &ids)?;
        self.install_pack_files(pack_data, index_data)?;
        self.write_packed_state_mirrors_batch(state_entries)?;
        for attachment in attachment_entries {
            self.put_state_attachment(&attachment)?;
        }
        self.clear_recent_object_caches();
        Ok(ids)
    }

    #[instrument(skip(self, blobs), fields(count = blobs.len()))]
    fn put_blobs_packed(&self, blobs: Vec<(crate::object::ContentHash, Vec<u8>)>) -> Result<()> {
        self.put_blobs_packed_impl(blobs)
    }

    #[instrument(skip(self, blobs, tree, state), fields(blob_count = blobs.len()))]
    fn put_snapshot_objects_packed(
        &self,
        blobs: Vec<(ContentHash, Vec<u8>)>,
        tree: &Tree,
        state: &State,
    ) -> Result<()> {
        self.put_snapshot_objects_packed_impl(
            blobs,
            Vec::new(),
            &TreeWrite::anchor(tree.clone()),
            state,
            Vec::new(),
            None,
        )
        .map(|_| ())
    }

    fn put_snapshot_objects_and_attachments_packed(
        &self,
        blobs: Vec<(ContentHash, Vec<u8>)>,
        tree: &Tree,
        state: &State,
        attachments: Vec<StateAttachment>,
    ) -> Result<()> {
        self.put_snapshot_objects_packed_impl(
            blobs,
            Vec::new(),
            &TreeWrite::anchor(tree.clone()),
            state,
            attachments,
            None,
        )
        .map(|_| ())
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
            validate_and_list_pack(self, &reader)?
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
        self.write_packed_state_mirrors_batch(state_entries)?;
        for attachment in attachment_entries {
            self.put_state_attachment(&attachment)?;
        }
        Ok(ids)
    }

    #[instrument(skip(self))]
    fn prune_loose_objects(&self) -> Result<(u64, u64)> {
        self.prune_loose_objects_impl()
    }

    fn discard_corrupt_clone_packs(&self) -> Result<usize> {
        let packs = super::fs_paths::packs_dir(&self.root);
        let mut removed = 0;
        for entry in match fs::read_dir(&packs) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        } {
            let path = entry?.path();
            match path.extension().and_then(|value| value.to_str()) {
                Some("pack") => {
                    let index = path.with_extension("idx");
                    let valid = crate::store::pack::PackReader::open(&path, &index)
                        .and_then(|reader| validate_and_list_pack(self, &reader).map(|_| ()))
                        .is_ok();
                    if !valid {
                        let _ = fs::remove_file(&path);
                        let _ = fs::remove_file(&index);
                        removed += 1;
                    }
                }
                Some("npk") if super::npk1::Npk1Pack::open(&path).is_err() => {
                    let _ = fs::remove_file(&path);
                    removed += 1;
                }
                _ => {}
            }
        }
        if removed > 0 {
            self.reload_packs()?;
            self.clear_recent_object_caches();
        }
        Ok(removed)
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
}

impl SidecarStore for FsStore {
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

#[cfg(test)]
mod enumeration_tests {
    use heddle_format::{compression::CompressionConfig, delta::DeltaEncoder};
    use tempfile::TempDir;

    use super::*;
    use crate::store::pack::{
        PackBuilder, PackContainerSpec, PackIndex, append_container_checksum,
        encode_tagged_entry_parts, write_container_header,
    };

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    struct TestEnumerationMetrics {
        membership_checks: u64,
        header_reads: u64,
        full_object_decodes: u64,
    }

    impl EnumerationCounter for TestEnumerationMetrics {
        fn membership_check(&mut self) {
            self.membership_checks += 1;
        }

        fn header_read(&mut self) {
            self.header_reads += 1;
        }
    }

    fn install_pack_files(
        dir: &TempDir,
        name: &str,
        pack_data: &[u8],
        index_data: &[u8],
    ) -> PackManager {
        fs::write(dir.path().join(format!("{name}.pack")), pack_data).unwrap();
        fs::write(dir.path().join(format!("{name}.idx")), index_data).unwrap();
        PackManager::new(dir.path().to_path_buf())
    }

    fn raw_mixed_manager() -> (TempDir, PackManager, Vec<(ContentHash, ObjectType)>) {
        let dir = TempDir::new().unwrap();
        let objects = [
            (ObjectType::Blob, b"packed blob".as_slice()),
            (ObjectType::Tree, b"packed tree".as_slice()),
            (ObjectType::Action, b"packed action".as_slice()),
        ];
        let mut builder = PackBuilder::new(CompressionConfig::disabled());
        let mut classified = Vec::new();
        for (obj_type, data) in objects {
            let hash = ContentHash::compute_typed("enumeration-test", data);
            builder.add(hash, obj_type, data.to_vec());
            classified.push((hash, obj_type));
        }
        let (pack, index, _) = builder.build().unwrap();
        let manager = install_pack_files(&dir, "mixed", &pack, &index);
        (dir, manager, classified)
    }

    fn delta_chain_manager() -> (TempDir, PackManager, Vec<ContentHash>) {
        const SPEC: PackContainerSpec = PackContainerSpec {
            magic: b"LMPK",
            version: 4,
        };
        let dir = TempDir::new().unwrap();
        let base = b"delta-chain base payload ".repeat(64);
        let mut middle = base.clone();
        middle[200..208].copy_from_slice(b"middle!!");
        let mut tip = middle.clone();
        tip[900..908].copy_from_slice(b"tip!!!!!");
        let bodies = [&base, &middle, &tip];
        let hashes = bodies
            .iter()
            .map(|body| ContentHash::compute_typed("blob", body))
            .collect::<Vec<_>>();
        let middle_delta = DeltaEncoder::encode(&base, &middle);
        let tip_delta = DeltaEncoder::encode(&middle, &tip);

        let mut pack = Vec::new();
        let mut index = PackIndex::new();
        write_container_header(&mut pack, SPEC, 3);
        for (position, payload) in [
            base.as_slice(),
            middle_delta.as_slice(),
            tip_delta.as_slice(),
        ]
        .into_iter()
        .enumerate()
        {
            index.add(PackObjectId::Hash(hashes[position]), pack.len() as u64);
            let (stored_type, base_id) = if position == 0 {
                (ObjectType::Blob, None)
            } else {
                (
                    ObjectType::Delta,
                    Some(PackObjectId::Hash(hashes[position - 1])),
                )
            };
            encode_tagged_entry_parts(
                &mut pack,
                PackObjectId::Hash(hashes[position]),
                stored_type,
                bodies[position].len(),
                base_id,
                payload,
            )
            .unwrap();
        }
        index.sort();
        append_container_checksum(&mut pack);
        let manager = install_pack_files(&dir, "delta-chain", &pack, &index.to_bytes());
        (dir, manager, hashes)
    }

    fn legacy_append_packed_hashes(
        hashes: &mut Vec<ContentHash>,
        manager: &PackManager,
        expected_type: ObjectType,
    ) -> Result<TestEnumerationMetrics> {
        let mut metrics = TestEnumerationMetrics::default();
        for id in manager.list_all_ids()? {
            let PackObjectId::Hash(hash) = id else {
                continue;
            };
            let mut already_listed = false;
            for listed in hashes.iter() {
                metrics.membership_checks += 1;
                if listed == &hash {
                    already_listed = true;
                    break;
                }
            }
            if already_listed {
                continue;
            }
            metrics.full_object_decodes += 1;
            if let Some((obj_type, _)) = manager.get_hashed_object(&hash)?
                && obj_type == expected_type
            {
                hashes.push(hash);
            }
        }
        Ok(metrics)
    }

    fn assert_new_matches_legacy(
        label: &str,
        manager: &PackManager,
        loose: Vec<ContentHash>,
        expected_type: ObjectType,
    ) {
        let mut new = loose.clone();
        let mut new_metrics = TestEnumerationMetrics::default();
        append_packed_hashes_with_counter(&mut new, manager, expected_type, &mut new_metrics)
            .unwrap();
        let mut legacy = loose;
        legacy_append_packed_hashes(&mut legacy, manager, expected_type).unwrap();
        assert_eq!(new, legacy, "fixture {label} changed output or ordering");
        assert_eq!(new_metrics.full_object_decodes, 0, "fixture {label}");
    }

    #[test]
    fn type_only_enumeration_matches_full_decode_across_fixture_set() {
        let empty_dir = TempDir::new().unwrap();
        let empty = PackManager::new(empty_dir.path().to_path_buf());
        let loose_hash = ContentHash::compute(b"loose only");
        assert_new_matches_legacy("loose-only", &empty, vec![loose_hash], ObjectType::Blob);

        let (_raw_dir, raw, classified) = raw_mixed_manager();
        for expected_type in [ObjectType::Blob, ObjectType::Tree, ObjectType::Action] {
            assert_new_matches_legacy("packed-only", &raw, Vec::new(), expected_type);
            assert_new_matches_legacy(
                "mixed",
                &raw,
                vec![ContentHash::compute_typed("loose", &[expected_type as u8])],
                expected_type,
            );
            let duplicate = classified
                .iter()
                .find_map(|(hash, obj_type)| (*obj_type == expected_type).then_some(*hash))
                .unwrap();
            assert_new_matches_legacy(
                "duplicate-loose-packed",
                &raw,
                vec![duplicate],
                expected_type,
            );
        }
        for (hash, expected_type) in classified {
            assert_eq!(
                raw.get_hashed_object_type(&hash).unwrap(),
                raw.get_hashed_object(&hash)
                    .unwrap()
                    .map(|(obj_type, _)| obj_type)
            );
            assert_eq!(
                raw.get_hashed_object_type(&hash).unwrap(),
                Some(expected_type)
            );
        }

        let (_delta_dir, delta, delta_hashes) = delta_chain_manager();
        assert_new_matches_legacy("two-link-delta-chain", &delta, Vec::new(), ObjectType::Blob);
        for hash in delta_hashes {
            assert_eq!(
                delta.get_hashed_object_type(&hash).unwrap(),
                Some(ObjectType::Blob)
            );
            assert_eq!(
                delta.get_hashed_object_type(&hash).unwrap(),
                delta
                    .get_hashed_object(&hash)
                    .unwrap()
                    .map(|(obj_type, _)| obj_type)
            );
        }
    }

    #[test]
    fn structural_counter_rejects_vec_scan_and_full_decode_negative_control() {
        let (_dir, manager, _) = raw_mixed_manager();
        let loose = (0..64u8)
            .map(|byte| ContentHash::compute_typed("loose", &[byte]))
            .collect::<Vec<_>>();
        let packed_hashes = manager
            .list_all_ids()
            .unwrap()
            .into_iter()
            .filter(|id| matches!(id, PackObjectId::Hash(_)))
            .count() as u64;

        for expected_type in [ObjectType::Blob, ObjectType::Tree, ObjectType::Action] {
            let mut optimized = loose.clone();
            let mut optimized_metrics = TestEnumerationMetrics::default();
            append_packed_hashes_with_counter(
                &mut optimized,
                &manager,
                expected_type,
                &mut optimized_metrics,
            )
            .unwrap();
            assert_eq!(optimized_metrics.membership_checks, packed_hashes);
            assert_eq!(optimized_metrics.header_reads, packed_hashes);
            assert_eq!(optimized_metrics.full_object_decodes, 0);

            let mut legacy = loose.clone();
            let legacy_metrics =
                legacy_append_packed_hashes(&mut legacy, &manager, expected_type).unwrap();
            assert!(legacy_metrics.membership_checks >= loose.len() as u64 * packed_hashes);
            assert_eq!(legacy_metrics.full_object_decodes, packed_hashes);
            assert!(
                !(legacy_metrics.membership_checks <= packed_hashes
                    && legacy_metrics.full_object_decodes == 0),
                "negative control unexpectedly passed the structural contract: {legacy_metrics:?}"
            );
        }
    }

    #[test]
    fn state_union_preserves_first_seen_order_at_scale() {
        let first = (0..20_000u32)
            .map(|value| {
                StateId::from_bytes(*ContentHash::compute(&value.to_le_bytes()).as_bytes())
            })
            .collect::<Vec<_>>();
        let second = first[10_000..]
            .iter()
            .copied()
            .chain((20_000..30_000u32).map(|value| {
                StateId::from_bytes(*ContentHash::compute(&value.to_le_bytes()).as_bytes())
            }))
            .collect::<Vec<_>>();
        let mut states = Vec::new();
        let mut known = HashSet::new();

        append_unique_states(&mut states, &mut known, first.iter().copied());
        append_unique_states(&mut states, &mut known, second);

        assert_eq!(states.len(), 30_000);
        assert_eq!(&states[..20_000], first.as_slice());
    }
}
