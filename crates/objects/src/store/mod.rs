// SPDX-License-Identifier: Apache-2.0
//! Backend-neutral object storage abstractions and concrete implementations.

use std::path::PathBuf;

use crate::object::{
    Action, ActionId, AnnotatedTag, Blob, ContentHash, OpenedTreeBody, State, StateAttachment,
    StateAttachmentId, StateId, Tree, TreeEntry, TreeEntryReader, TreeResumeCursor,
    is_streamable_tree,
};

pub mod codec;
mod delta_source;
pub mod fs;
pub mod liveness;
#[cfg(any(test, feature = "memory-backend"))]
pub mod memory;
pub use heddle_pack::store::pack;
pub mod shallow;
mod snapshot_commit;
pub mod source;
pub mod store_compliance;
pub mod writer_lease;

pub use fs::{
    AutomaticRepackLock, DEFAULT_PACK_INSTALL_INTENT_TTL_SECS, FsRepackOperation, FsStore,
    PackInstallIntent, PackInstallMetricsSnapshot, PackInstallPhase, PackInstallRecoverReport,
    SnapshotPackFold, install_pack_bytes_journaled, pack_install_metrics_reset,
    pack_install_metrics_snapshot, recover_pack_install_intents,
    recover_pack_install_intents_with_ttl,
};
pub use heddle_format::compression::{CompressionConfig, CompressionError, compress, decompress};
pub use liveness::{
    AGENT_LEASE_DURATION, Liveness, current_boot_id, process_alive, reservation_liveness_at,
};
#[cfg(any(test, feature = "memory-backend"))]
pub use memory::InMemoryStore;
pub use pack::{
    CancellationToken as RepackCancellationToken, LoadMonitor as RepackLoadMonitor, PackBuilder,
    PackObjectId, PackReader, PackStats, RepackContext, RepackError, RepackHandle, RepackInventory,
    RepackOperation, RepackOutcome, RepackPolicy, RepackReason, RepackReport, RepackResourceLimits,
    RepackSchedule, RepackScheduler, StreamingPackBuilder, SyncData,
};
pub use shallow::ShallowInfo;
#[doc(hidden)]
pub use snapshot_commit::{
    SNAPSHOT_COMMIT_ARTIFACT_SCHEMA, SnapshotCommitArtifact, SnapshotCommitDescriptor,
    SnapshotPackManager,
};
#[cfg(feature = "async-source")]
pub use source::AsyncObjectSource;
pub use source::ObjectSource;
pub use writer_lease::{
    WriterLease, WriterLeaseAuthOutcome, WriterLeaseDraft, WriterLeaseGrant,
    WriterLeaseReserveOutcome, WriterLeaseStatus, WriterLeaseStore, generate_writer_lease_id,
    generate_writer_lease_token,
};

/// A newly-authored tree plus its immediate parent, when capture already knows
/// that relationship. Stores may use the hint for bounded HDC1 encoding; it
/// never changes the tree's semantic content hash.
#[derive(Clone, Debug)]
pub struct TreeWrite {
    pub tree: Tree,
    pub parent: Option<ContentHash>,
    pub anchor: Option<(ContentHash, Tree, u8)>,
}

impl TreeWrite {
    pub fn anchor(tree: Tree) -> Self {
        Self {
            tree,
            parent: None,
            anchor: None,
        }
    }

    pub fn descendant(tree: Tree, parent: ContentHash) -> Self {
        Self {
            tree,
            parent: Some(parent),
            anchor: None,
        }
    }

    /// Supply a materialized delta anchor and the immediate parent's depth
    /// within that anchor's epoch.
    pub fn with_anchor(mut self, hash: ContentHash, tree: Tree, parent_depth: u8) -> Self {
        self.anchor = Some((hash, tree, parent_depth));
        self
    }
}

/// Read-only objects whose authoritative representation lives outside the
/// native Heddle object directory. Git-overlay repositories use this seam to
/// translate objects directly from `.git` without importing a second copy.
pub trait ExternalObjectSource: Send + Sync {
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>>;
    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>>;
    fn get_state(&self, id: &StateId) -> Result<Option<State>>;
    fn list_states(&self) -> Result<Vec<StateId>>;
}

pub use crate::error::{HeddleError as StoreError, HeddleError, Result};

/// Sidecar records that live outside the content-addressed object graph —
/// signed redactions and state-visibility tiers. They never ride native packs
/// and are transferred out-of-band. Backends that do not model them can use
/// the default methods, while native stores override the relevant operations.
pub trait SidecarStore: Send + Sync {
    /// Whether the store holds any redaction record for the given blob.
    ///
    /// Redactions live in a sidecar (`<heddle_dir>/redactions/`) that is
    /// structurally outside the content-addressed object graph so GC
    /// can't reach them. The wire layer needs a cheap probe to decide
    /// whether to ship a redaction for a blob in the closure, so this
    /// is a separate method rather than a `get_*` + null check.
    ///
    /// Default impl returns `Ok(false)` — stores that don't model
    /// redactions silently report "no redactions," which is the
    /// correct behaviour for purely in-memory or remote-shim stores.
    fn has_redactions_for_blob(&self, _blob: &ContentHash) -> Result<bool> {
        Ok(false)
    }

    /// Return the raw rmp-encoded `RedactionsBlob` bytes for the given
    /// blob, or `Ok(None)` if no redaction record exists. The bytes
    /// are byte-identical to what was written by `put_redactions_bytes_for_blob`
    /// (or by `Repository::put_redaction`); this is the wire-transfer
    /// payload, not a re-serialized view.
    ///
    /// Default impl returns `Ok(None)`.
    fn get_redactions_bytes_for_blob(&self, _blob: &ContentHash) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    /// Persist the rmp-encoded `RedactionsBlob` bytes for the given
    /// blob. Receiver-side replay calls this after signature
    /// verification so the bytes land in the same sidecar that the
    /// sender's `Repository::put_redaction` writes to.
    ///
    /// Default impl returns an "unsupported" error — stores that don't
    /// model redactions (e.g. read-only shims) refuse rather than
    /// silently dropping the record.
    fn put_redactions_bytes_for_blob(&self, _blob: &ContentHash, _bytes: &[u8]) -> Result<()> {
        Err(HeddleError::InvalidObject(
            "this object store does not support persisting redactions".to_string(),
        ))
    }

    /// List every blob that has at least one redaction record. Used by
    /// the GC pin guard and by sync to enumerate redactions for the
    /// state closure. Order is unspecified; callers that need stable
    /// ordering should sort.
    ///
    /// Default impl returns `Ok(vec![])`.
    fn list_blobs_with_redactions(&self) -> Result<Vec<ContentHash>> {
        Ok(Vec::new())
    }

    /// Whether the store holds any state-visibility record for `state`.
    ///
    /// Like redactions, state-visibility records live in a sidecar outside
    /// the content-addressed object graph and cannot ride native packs.
    /// Sync uses this probe while enumerating a state closure so a non-public
    /// state can advertise the sidecar that must travel out-of-pack.
    ///
    /// Default impl returns `Ok(false)` for stores that do not model this
    /// sidecar.
    fn has_state_visibility_for_state(&self, _state: &StateId) -> Result<bool> {
        Ok(false)
    }

    /// Return the raw rmp-encoded `StateVisibilityBlob` bytes for `state`,
    /// or `Ok(None)` if no sidecar exists. The bytes are the wire-transfer
    /// payload for state visibility.
    ///
    /// Default impl returns `Ok(None)`.
    fn get_state_visibility_bytes_for_state(&self, _state: &StateId) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    /// Persist raw `StateVisibilityBlob` bytes for `state`.
    ///
    /// Default impl returns an "unsupported" error so stores that do not
    /// model the sidecar refuse instead of dropping it.
    fn put_state_visibility_bytes_for_state(&self, _state: &StateId, _bytes: &[u8]) -> Result<()> {
        Err(HeddleError::InvalidObject(
            "this object store does not support persisting state visibility".to_string(),
        ))
    }

    /// List every state with at least one state-visibility record.
    ///
    /// Default impl returns `Ok(vec![])`.
    fn list_states_with_visibility(&self) -> Result<Vec<StateId>> {
        Ok(Vec::new())
    }
}

/// Trait for object storage backends.
///
/// Sidecars remain a separate implementation seam, but every object store
/// exposes that seam. This preserves object-safe `dyn ObjectStore` consumers
/// such as Weft's local filesystem backend without coupling its S3 backend to
/// the native store implementation.
pub trait ObjectStore: SidecarStore + Send + Sync {
    fn get_annotated_tag(&self, _hash: &ContentHash) -> Result<Option<AnnotatedTag>> {
        Ok(None)
    }
    fn put_annotated_tag(&self, _tag: &AnnotatedTag) -> Result<ContentHash> {
        Err(HeddleError::InvalidObject(
            "object store does not support annotated tags".to_string(),
        ))
    }
    fn list_annotated_tags(&self) -> Result<Vec<ContentHash>> {
        Ok(Vec::new())
    }
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>>;
    fn put_blob(&self, blob: &Blob) -> Result<ContentHash>;

    /// Zero-copy variant of `get_blob`. Returns a [`bytes::Bytes`]
    /// view of the blob's content, which for `FsStore` reads is a
    /// slice into the pack file's mmap when the entry is non-delta
    /// and uncompressed — no allocation, no memcpy.
    ///
    /// Default impl wraps `get_blob`'s `Vec<u8>` in a `Bytes` (one
    /// Arc allocation, no body copy) so backends without a native
    /// fast path still satisfy the contract. The mount's hot read
    /// path goes through this method instead of `get_blob` so the
    /// pack-mmap fast path lights up automatically.
    fn get_blob_bytes(&self, hash: &ContentHash) -> Result<Option<bytes::Bytes>> {
        Ok(self
            .get_blob(hash)?
            .map(|blob| bytes::Bytes::from(blob.into_content())))
    }

    /// Return the *uncompressed* byte length of the blob identified by
    /// `hash`, or `Ok(None)` when the blob is not in the store.
    ///
    /// The contract is "size without paying for content": backends are
    /// expected to honour this with a header read or index lookup
    /// rather than a full decompression. This is the hot path for
    /// directory listings (`ls -l` over a thread mount) where loading
    /// every blob just to learn its size would dominate.
    ///
    /// The default implementation falls back to `get_blob` so backends
    /// without a cheap size accessor still satisfy the contract; native
    /// stores (`FsStore`, `InMemoryStore`) override this with a
    /// header- or hashmap-only path.
    fn blob_size(&self, hash: &ContentHash) -> Result<Option<u64>> {
        Ok(self.get_blob(hash)?.map(|blob| blob.content().len() as u64))
    }

    /// Filesystem path of the loose blob whose on-disk bytes are
    /// byte-identical to the blob's *uncompressed* content, suitable
    /// for `hard_link`/`clonefile` materialization without going
    /// through `get_blob`.
    ///
    /// Returns `None` when the blob is missing, is only available via
    /// a packfile, is stored compressed (the on-disk bytes wouldn't
    /// match what a worktree consumer needs to read), or the backend
    /// doesn't expose stable filesystem paths (e.g. `InMemoryStore`). The
    /// default impl returns `None` so non-`FsStore` backends silently fall
    /// through to the bytes path.
    fn loose_blob_path(&self, _hash: &ContentHash) -> Option<PathBuf> {
        None
    }

    /// Ensure the blob identified by `hash` is materialized as an
    /// uncompressed loose file at the canonical loose path so that
    /// `loose_blob_path` returns `Some(path)` on a subsequent call.
    ///
    /// This is the "warm canonical store" path that lets the
    /// hardlink-first materializer keep its 5–10× wall-clock and
    /// storage-allocation wins after `pack_objects + prune_loose_objects`
    /// has moved everything into a packfile. Without this, the lazy
    /// hardlink path silently degrades to `fs::write(decompressed)` on
    /// every materialize, because `loose_blob_path` returns `None` for
    /// pack-only and compressed-loose blobs.
    ///
    /// Cost-amortization: the first promotion of a blob pays
    /// `decompress + atomic write`. Every subsequent materialize of
    /// the same blob — into the same worktree on `goto`, or into a
    /// sibling worktree on `delegate` — is a single `link(2)`. Net
    /// win for any N > 1 materializations; break-even at N == 1.
    ///
    /// Pack invariants are preserved: this method does not remove the
    /// pack-resident copy. The blob lives in both pack and loose-
    /// uncompressed until the next `prune_loose_objects` cycle, at
    /// which point the loose mirror is discarded and a future
    /// materialize re-promotes on demand.
    ///
    /// Idempotent: a blob that's already loose-and-uncompressed is a
    /// no-op fast path. A blob that's loose-but-compressed is
    /// rewritten in place (atomically) with the uncompressed bytes.
    /// A blob that's pack-resident is decompressed out of the pack
    /// and written loose without touching the pack.
    ///
    /// Returns `Ok(true)` when the call did real work (a write
    /// happened), `Ok(false)` when it was a no-op (blob was already
    /// loose+uncompressed), and `Err` when the blob isn't in the
    /// store at all. The default impl returns `Ok(false)` for
    /// backends that don't expose loose paths (`InMemoryStore`), since the
    /// hardlink path is fundamentally inapplicable there.
    fn promote_to_loose_uncompressed(&self, _hash: &ContentHash) -> Result<bool> {
        Ok(false)
    }

    /// Drop any in-memory caches of decompressed blobs / trees /
    /// states. The next access to any object pays full I/O +
    /// decompression cost. No-op for stores that don't cache
    /// (`InMemoryStore` is already the source of truth).
    ///
    /// Exposed primarily for benchmarks that want to measure the
    /// true cold-cache path without rebuilding the store from
    /// scratch. Production callers don't need to invoke this.
    fn clear_recent_caches(&self) {}

    fn put_blob_with_hash(&self, blob: &Blob, hash: ContentHash) -> Result<ContentHash> {
        if blob.hash() != hash {
            return Err(HeddleError::InvalidObject("blob hash mismatch".to_string()));
        }
        self.put_blob(blob)
    }

    fn has_blob(&self, hash: &ContentHash) -> Result<bool>;
    /// Return whether the blob is owned by this store, excluding any configured
    /// read-through source. Snapshot builders use this to ensure a new native
    /// state owns its complete object closure.
    fn has_blob_locally(&self, hash: &ContentHash) -> Result<bool> {
        self.has_blob(hash)
    }
    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>>;
    /// Resolve one named tree entry. Pack-capable stores override this so a
    /// lookup can use a restartable packed record instead of materializing the
    /// complete tree.
    fn get_tree_entry(&self, hash: &ContentHash, name: &str) -> Result<Option<TreeEntry>> {
        Ok(self
            .get_tree(hash)?
            .and_then(|tree| tree.get(name).cloned()))
    }
    fn put_tree(&self, tree: &Tree) -> Result<ContentHash>;
    fn has_tree(&self, hash: &ContentHash) -> Result<bool>;
    /// Return whether the tree is owned by this store, excluding any configured
    /// read-through source.
    fn has_tree_locally(&self, hash: &ContentHash) -> Result<bool> {
        self.has_tree(hash)
    }
    /// Open a streamable HTR4 tree body. Store backends use sequential
    /// verify: resume at ordinal > 0 is refused until the bytes are hashed.
    fn open_tree(
        &self,
        tree_id: &ContentHash,
        cursor: Option<&TreeResumeCursor>,
    ) -> Result<Option<TreeEntryReader<OpenedTreeBody>>> {
        let Some(body) = self.get_tree_serialized(tree_id)? else {
            return Ok(None);
        };
        let body = if is_streamable_tree(&body) {
            body
        } else {
            let tree = self
                .get_tree(tree_id)?
                .ok_or_else(|| HeddleError::NotFound(format!("tree {tree_id}")))?;
            tree.encode_lean()?
        };
        Ok(Some(TreeEntryReader::open(
            OpenedTreeBody::Bytes(crate::object::BytesTreeSource::sequential_verify(body)),
            *tree_id,
            cursor,
        )?))
    }
    fn get_state(&self, id: &StateId) -> Result<Option<State>>;
    fn put_state(&self, state: &State) -> Result<()>;
    fn has_state(&self, id: &StateId) -> Result<bool>;
    fn list_states(&self) -> Result<Vec<StateId>>;
    fn get_state_attachment(
        &self,
        _state: &StateId,
        _id: &StateAttachmentId,
    ) -> Result<Option<StateAttachment>> {
        Ok(None)
    }
    fn put_state_attachment(&self, _attachment: &StateAttachment) -> Result<StateAttachmentId> {
        Err(HeddleError::InvalidObject(
            "object store does not support state attachments".to_string(),
        ))
    }
    fn list_state_attachments(&self, _state: &StateId) -> Result<Vec<StateAttachment>> {
        Ok(Vec::new())
    }
    fn get_action(&self, id: &ActionId) -> Result<Option<Action>>;
    fn put_action(&self, action: &mut Action) -> Result<ActionId>;
    fn list_actions(&self) -> Result<Vec<ActionId>>;
    fn list_blobs(&self) -> Result<Vec<ContentHash>>;
    fn list_trees(&self) -> Result<Vec<ContentHash>>;

    fn put_blob_bytes_with_hash(&self, data: &[u8], hash: ContentHash) -> Result<ContentHash> {
        self.put_blob_with_hash(&Blob::from_slice(data), hash)
    }

    /// Return the stored tree body for `hash`, without requiring HTR4.
    ///
    /// This is a migration seam, not a runtime compatibility reader: callers
    /// that need current tree semantics should use [`ObjectStore::get_tree`].
    /// Loose and packed backends must return the raw stored bytes so one-shot
    /// migrations can canonicalize older encodings without a current-decoder
    /// gate. Default impls that only have `get_tree` re-encode current trees.
    fn get_tree_serialized(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> {
        self.get_tree(hash)?
            .map(|tree| tree.encode_canonical().map_err(HeddleError::from))
            .transpose()
    }

    fn put_tree_serialized(&self, data: &[u8], hash: ContentHash) -> Result<ContentHash> {
        let tree = codec::decode_tree_serialized_with_key(data, hash, None)?;
        self.put_tree(&tree)
    }

    fn put_state_serialized(&self, data: &[u8], id: StateId) -> Result<()> {
        let state: State = rmp_serde::from_slice(data)?;
        if !state.accepts_stored_id(&id) {
            return Err(HeddleError::InvalidObject(format!(
                "state id mismatch: expected {id}, computed {}",
                state.id()
            )));
        }
        self.put_state(&state)
    }

    fn put_action_serialized(&self, data: &[u8], id: ActionId) -> Result<()> {
        let mut action: Action = rmp_serde::from_slice(data)?;
        let found_id = action.compute_id();
        if found_id != id {
            return Err(HeddleError::InvalidObject(format!(
                "action id mismatch: expected {}, found {}",
                id, found_id
            )));
        }
        let stored_id = self.put_action(&mut action)?;
        if stored_id != id {
            return Err(HeddleError::InvalidObject(format!(
                "action id mismatch after write: expected {}, found {}",
                id, stored_id
            )));
        }
        Ok(())
    }

    fn get_pack_object(
        &self,
        id: &pack::PackObjectId,
    ) -> Result<Option<(pack::ObjectType, Vec<u8>)>> {
        match id {
            pack::PackObjectId::AnnotatedTag(hash) => Ok(self
                .get_annotated_tag(hash)?
                .map(|tag| (pack::ObjectType::AnnotatedTag, tag.encode_current_msgpack()))),
            pack::PackObjectId::Hash(hash) => {
                if let Some(blob) = self.get_blob(hash)? {
                    return Ok(Some((pack::ObjectType::Blob, blob.content().to_vec())));
                }
                if let Some(tree) = self.get_tree(hash)? {
                    return Ok(Some((pack::ObjectType::Tree, tree.encode_canonical()?)));
                }
                if let Some(action) = self.get_action(&ActionId::from_hash(*hash))? {
                    return Ok(Some((
                        pack::ObjectType::Action,
                        rmp_serde::to_vec_named(&action)?,
                    )));
                }
                Ok(None)
            }
            pack::PackObjectId::StateId(change_id) => {
                if let Some(state) = self.get_state(change_id)? {
                    Ok(Some((
                        pack::ObjectType::State,
                        rmp_serde::to_vec_named(&state)?,
                    )))
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Bulk-write a batch of blobs as a single durable unit. The default
    /// implementation falls back to per-blob writes; backends that
    /// support packfiles (i.e. `FsStore`) override this to install one
    /// packfile + index — two fsyncs total instead of N. Used by the
    /// snapshot hot path so writing 1000 small files takes ~one fsync,
    /// not 1000.
    ///
    /// Blobs already present in the store are skipped on the way in
    /// (the caller would otherwise duplicate them in the pack).
    fn put_blobs_packed(&self, blobs: Vec<(ContentHash, Vec<u8>)>) -> Result<()> {
        for (hash, data) in blobs {
            if !self.has_blob(&hash)? {
                self.put_blob_bytes_with_hash(&data, hash)?;
            }
        }
        Ok(())
    }

    /// Durably install a snapshot's newly-authored immutable object closure as
    /// one storage batch. Pack-capable backends override this to share one pack
    /// installation across blobs, the root tree, and the state; other backends
    /// preserve the same ordering through their ordinary object methods.
    fn put_snapshot_objects_packed(
        &self,
        blobs: Vec<(ContentHash, Vec<u8>)>,
        tree: &Tree,
        state: &State,
    ) -> Result<()> {
        self.put_blobs_packed(blobs)?;
        self.put_tree(tree)?;
        self.put_state(state)
    }

    /// Snapshot closure variant that also durably installs immutable authored
    /// attachments. The separate method preserves the existing backend API;
    /// pack-capable stores override it to share the snapshot pack barrier.
    fn put_snapshot_objects_and_attachments_packed(
        &self,
        blobs: Vec<(ContentHash, Vec<u8>)>,
        tree: &Tree,
        state: &State,
        attachments: Vec<StateAttachment>,
    ) -> Result<()> {
        self.put_snapshot_objects_packed(blobs, tree, state)?;
        for attachment in attachments {
            self.put_state_attachment(&attachment)?;
        }
        Ok(())
    }
    fn install_pack(&self, pack_data: &[u8], index_data: &[u8]) -> Result<Vec<pack::PackObjectId>> {
        let reader = pack::PackReader::from_slice(pack_data, index_data)?;
        let ids = reader.list_ids()?;
        for id in &ids {
            let Some((obj_type, data)) = reader.get_object(id)? else {
                continue;
            };
            match (id, obj_type) {
                (pack::PackObjectId::Hash(hash), pack::ObjectType::Blob) => {
                    self.put_blob_bytes_with_hash(&data, *hash)?;
                }
                (pack::PackObjectId::AnnotatedTag(hash), pack::ObjectType::AnnotatedTag) => {
                    let tag = AnnotatedTag::decode_current_msgpack(&data)
                        .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
                    if tag.hash() != *hash {
                        return Err(HeddleError::InvalidObject(
                            "annotated tag hash mismatch".to_string(),
                        ));
                    }
                    self.put_annotated_tag(&tag)?;
                }
                (pack::PackObjectId::Hash(hash), pack::ObjectType::Tree) => {
                    self.put_tree_serialized(&data, *hash)?;
                }
                (pack::PackObjectId::Hash(hash), pack::ObjectType::Action) => {
                    self.put_action_serialized(&data, ActionId::from_hash(*hash))?;
                }
                (pack::PackObjectId::StateId(change_id), pack::ObjectType::State) => {
                    self.put_state_serialized(&data, *change_id)?;
                }
                (_, pack::ObjectType::TimelineOperation) => {
                    return Err(HeddleError::InvalidObject(
                        "timeline operations belong in the timeline pack store".to_string(),
                    ));
                }
                _ => {
                    return Err(HeddleError::InvalidObject(format!(
                        "unsupported native pack object: {:?} {:?}",
                        id, obj_type
                    )));
                }
            }
        }
        Ok(ids)
    }

    /// Install a pack and its index from on-disk files
    /// (typically produced by `StreamingPackBuilder`). The default
    /// impl reads both files fully and delegates to `install_pack`,
    /// so any backend that doesn't override this still works (at the
    /// cost of giving back the bounded-memory promise). Real fs-
    /// backed stores override this to `rename(2)` both files into the
    /// pack directory without ever loading them.
    ///
    /// On success, the source files at `pack_path`/`index_path` may
    /// have been moved or removed depending on the backend; callers
    /// shouldn't continue to rely on them.
    ///
    /// Returns the ids of the installed objects — the same set
    /// `install_pack` reports for the equivalent byte-buffer install,
    /// so callers (e.g. native sync) read the installed ids off the
    /// install result instead of tracking them out-of-band.
    fn install_pack_streaming(
        &self,
        pack_path: &std::path::Path,
        index_path: &std::path::Path,
    ) -> Result<Vec<pack::PackObjectId>> {
        let pack_data = std::fs::read(pack_path).map_err(StoreError::from)?;
        let index_data = std::fs::read(index_path).map_err(StoreError::from)?;
        let ids = self.install_pack(&pack_data, &index_data)?;
        // Default impl: clean up the staged files. Override
        // implementations that move/rename should not call super and
        // should manage the file lifecycle themselves.
        let _ = std::fs::remove_file(pack_path);
        let _ = std::fs::remove_file(index_path);
        Ok(ids)
    }

    fn pack_objects(&self, delta_search: bool) -> Result<(u64, u64)> {
        let _ = delta_search;
        Ok((0, 0))
    }

    fn prune_loose_objects(&self) -> Result<(u64, u64)> {
        Ok((0, 0))
    }

    /// Remove only pack/index pairs that fail checksum, index, or object-hash
    /// validation so a clone repair pull advertises their objects as missing.
    fn discard_corrupt_clone_packs(&self) -> Result<usize> {
        Ok(0)
    }

    fn begin_snapshot_write_batch(&self) -> Result<()> {
        Ok(())
    }

    fn flush_snapshot_write_batch(&self) -> Result<()> {
        Ok(())
    }

    fn abort_snapshot_write_batch(&self) {}
}
