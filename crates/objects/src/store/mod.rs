// SPDX-License-Identifier: Apache-2.0
//! Backend-neutral object storage abstractions and concrete implementations.

use std::path::PathBuf;

use crate::object::{Action, ActionId, Blob, ChangeId, ContentHash, State, Tree};

pub mod actor_presence;
pub mod agent_task;
pub mod codec;
pub mod fs;
pub mod liveness;
#[cfg(any(test, feature = "memory-backend"))]
pub mod memory;
pub mod pack;
pub mod shallow;
pub mod source;
pub mod store_compliance;
pub mod writer_lease;

pub use actor_presence::{
    ActorChainNode, ActorPresence, ActorPresenceStatus, ActorPresenceStore, AgentUsageSummary,
    ContextQueryEntry, generate_actor_session_id,
};
pub use agent_task::{
    AGENT_TASK_SCHEMA_VERSION, AgentTaskRecord, AgentTaskStatus, AgentTaskStore,
    generate_agent_task_id, validate_task_id,
};
pub use fs::{
    DEFAULT_PACK_INSTALL_INTENT_TTL_SECS, FsStore, PackInstallIntent, PackInstallMetricsSnapshot,
    PackInstallPhase, PackInstallRecoverReport, install_pack_bytes_journaled,
    pack_install_metrics_reset, pack_install_metrics_snapshot, recover_pack_install_intents,
    recover_pack_install_intents_with_ttl,
};
pub use heddle_format::compression::{CompressionConfig, CompressionError, compress, decompress};
pub use liveness::{
    AGENT_LEASE_DURATION, Liveness, current_boot_id, process_alive, reservation_liveness_at,
};
#[cfg(any(test, feature = "memory-backend"))]
pub use memory::InMemoryStore;
pub use pack::{PackBuilder, PackObjectId, PackReader, PackStats, StreamingPackBuilder, SyncData};
pub use shallow::ShallowInfo;
#[cfg(feature = "async-source")]
pub use source::AsyncObjectSource;
pub use source::ObjectSource;
pub use writer_lease::{
    WriterLease, WriterLeaseAuthOutcome, WriterLeaseDraft, WriterLeaseGrant,
    WriterLeaseReserveOutcome, WriterLeaseStatus, WriterLeaseStore, generate_writer_lease_id,
    generate_writer_lease_token,
};

pub use crate::error::{HeddleError as StoreError, HeddleError, Result};

impl From<CompressionError> for HeddleError {
    fn from(e: CompressionError) -> Self {
        HeddleError::Compression(e.to_string())
    }
}

/// Static-dispatch enum over the concrete object stores Heddle ships.
///
/// This is the default `S` for [`Repository`](crate) so the store backend
/// remains compile-time-monomorphized — no vtable. Each [`ObjectStore`] method
/// `match`-dispatches to the inner variant, so the compiler inlines through
/// the enum to the concrete backend's implementation (including its overridden
/// default methods).
///
/// Sealed by construction: only the variants enumerated here are valid
/// stores. Heddle is the sole implementer (heddle#259 / #283) — `AnyStore`
/// is not a public extension point.
#[derive(Clone)]
pub enum AnyStore {
    Fs(FsStore),
}

/// Forward an [`ObjectStore`] call to the active [`AnyStore`] variant.
///
/// Every arm calls the *same* method on the inner concrete store, so a
/// backend's override of a defaulted trait method (e.g. `FsStore::blob_size`)
/// is preserved rather than falling back to the trait default.
macro_rules! any_store_dispatch {
    ($self:ident, $method:ident ( $($arg:expr),* )) => {
        match $self {
            AnyStore::Fs(inner) => inner.$method($($arg),*),
        }
    };
}

impl ObjectStore for AnyStore {
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>> {
        match self {
            AnyStore::Fs(inner) => ObjectStore::get_blob(inner, hash),
        }
    }
    fn put_blob(&self, blob: &Blob) -> Result<ContentHash> {
        any_store_dispatch!(self, put_blob(blob))
    }
    fn get_blob_bytes(&self, hash: &ContentHash) -> Result<Option<bytes::Bytes>> {
        match self {
            AnyStore::Fs(inner) => ObjectStore::get_blob_bytes(inner, hash),
        }
    }
    fn blob_size(&self, hash: &ContentHash) -> Result<Option<u64>> {
        any_store_dispatch!(self, blob_size(hash))
    }
    fn loose_blob_path(&self, hash: &ContentHash) -> Option<PathBuf> {
        any_store_dispatch!(self, loose_blob_path(hash))
    }
    fn promote_to_loose_uncompressed(&self, hash: &ContentHash) -> Result<bool> {
        any_store_dispatch!(self, promote_to_loose_uncompressed(hash))
    }
    fn clear_recent_caches(&self) {
        any_store_dispatch!(self, clear_recent_caches())
    }
    fn put_blob_with_hash(&self, blob: &Blob, hash: ContentHash) -> Result<ContentHash> {
        any_store_dispatch!(self, put_blob_with_hash(blob, hash))
    }
    fn has_blob(&self, hash: &ContentHash) -> Result<bool> {
        any_store_dispatch!(self, has_blob(hash))
    }
    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        match self {
            AnyStore::Fs(inner) => ObjectStore::get_tree(inner, hash),
        }
    }
    fn get_tree_serialized(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> {
        match self {
            AnyStore::Fs(inner) => ObjectStore::get_tree_serialized(inner, hash),
        }
    }
    fn put_tree(&self, tree: &Tree) -> Result<ContentHash> {
        any_store_dispatch!(self, put_tree(tree))
    }
    fn has_tree(&self, hash: &ContentHash) -> Result<bool> {
        any_store_dispatch!(self, has_tree(hash))
    }
    fn get_state(&self, id: &ChangeId) -> Result<Option<State>> {
        match self {
            AnyStore::Fs(inner) => ObjectStore::get_state(inner, id),
        }
    }
    fn put_state(&self, state: &State) -> Result<()> {
        any_store_dispatch!(self, put_state(state))
    }
    fn has_state(&self, id: &ChangeId) -> Result<bool> {
        any_store_dispatch!(self, has_state(id))
    }
    fn list_states(&self) -> Result<Vec<ChangeId>> {
        any_store_dispatch!(self, list_states())
    }
    fn get_action(&self, id: &ActionId) -> Result<Option<Action>> {
        any_store_dispatch!(self, get_action(id))
    }
    fn put_action(&self, action: &mut Action) -> Result<ActionId> {
        any_store_dispatch!(self, put_action(action))
    }
    fn list_actions(&self) -> Result<Vec<ActionId>> {
        any_store_dispatch!(self, list_actions())
    }
    fn list_blobs(&self) -> Result<Vec<ContentHash>> {
        any_store_dispatch!(self, list_blobs())
    }
    fn list_trees(&self) -> Result<Vec<ContentHash>> {
        any_store_dispatch!(self, list_trees())
    }
    fn put_blob_bytes_with_hash(&self, data: &[u8], hash: ContentHash) -> Result<ContentHash> {
        any_store_dispatch!(self, put_blob_bytes_with_hash(data, hash))
    }
    fn put_tree_serialized(&self, data: &[u8], hash: ContentHash) -> Result<ContentHash> {
        match self {
            AnyStore::Fs(inner) => ObjectStore::put_tree_serialized(inner, data, hash),
        }
    }
    fn put_state_serialized(&self, data: &[u8], id: ChangeId) -> Result<()> {
        any_store_dispatch!(self, put_state_serialized(data, id))
    }
    fn put_action_serialized(&self, data: &[u8], id: ActionId) -> Result<()> {
        any_store_dispatch!(self, put_action_serialized(data, id))
    }
    fn get_pack_object(
        &self,
        id: &pack::PackObjectId,
    ) -> Result<Option<(pack::ObjectType, Vec<u8>)>> {
        any_store_dispatch!(self, get_pack_object(id))
    }
    fn put_blobs_packed(&self, blobs: Vec<(ContentHash, Vec<u8>)>) -> Result<()> {
        any_store_dispatch!(self, put_blobs_packed(blobs))
    }
    fn install_pack(&self, pack_data: &[u8], index_data: &[u8]) -> Result<Vec<pack::PackObjectId>> {
        any_store_dispatch!(self, install_pack(pack_data, index_data))
    }
    fn install_pack_streaming(
        &self,
        pack_path: &std::path::Path,
        index_path: &std::path::Path,
    ) -> Result<Vec<pack::PackObjectId>> {
        any_store_dispatch!(self, install_pack_streaming(pack_path, index_path))
    }
    fn pack_objects(&self, aggressive: bool) -> Result<(u64, u64)> {
        any_store_dispatch!(self, pack_objects(aggressive))
    }
    fn prune_loose_objects(&self) -> Result<(u64, u64)> {
        any_store_dispatch!(self, prune_loose_objects())
    }
    fn begin_snapshot_write_batch(&self) -> Result<()> {
        any_store_dispatch!(self, begin_snapshot_write_batch())
    }
    fn flush_snapshot_write_batch(&self) -> Result<()> {
        any_store_dispatch!(self, flush_snapshot_write_batch())
    }
    fn abort_snapshot_write_batch(&self) {
        any_store_dispatch!(self, abort_snapshot_write_batch())
    }
    fn has_redactions_for_blob(&self, blob: &ContentHash) -> Result<bool> {
        any_store_dispatch!(self, has_redactions_for_blob(blob))
    }
    fn get_redactions_bytes_for_blob(&self, blob: &ContentHash) -> Result<Option<Vec<u8>>> {
        any_store_dispatch!(self, get_redactions_bytes_for_blob(blob))
    }
    fn put_redactions_bytes_for_blob(&self, blob: &ContentHash, bytes: &[u8]) -> Result<()> {
        any_store_dispatch!(self, put_redactions_bytes_for_blob(blob, bytes))
    }
    fn list_blobs_with_redactions(&self) -> Result<Vec<ContentHash>> {
        any_store_dispatch!(self, list_blobs_with_redactions())
    }
    fn has_state_visibility_for_state(&self, state: &ChangeId) -> Result<bool> {
        any_store_dispatch!(self, has_state_visibility_for_state(state))
    }
    fn get_state_visibility_bytes_for_state(&self, state: &ChangeId) -> Result<Option<Vec<u8>>> {
        any_store_dispatch!(self, get_state_visibility_bytes_for_state(state))
    }
    fn put_state_visibility_bytes_for_state(&self, state: &ChangeId, bytes: &[u8]) -> Result<()> {
        any_store_dispatch!(self, put_state_visibility_bytes_for_state(state, bytes))
    }
    fn list_states_with_visibility(&self) -> Result<Vec<ChangeId>> {
        any_store_dispatch!(self, list_states_with_visibility())
    }
}

/// Trait for object storage backends.
pub trait ObjectStore: Send + Sync {
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
    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>>;
    fn put_tree(&self, tree: &Tree) -> Result<ContentHash>;
    fn has_tree(&self, hash: &ContentHash) -> Result<bool>;
    fn get_state(&self, id: &ChangeId) -> Result<Option<State>>;
    fn put_state(&self, state: &State) -> Result<()>;
    fn has_state(&self, id: &ChangeId) -> Result<bool>;
    fn list_states(&self) -> Result<Vec<ChangeId>>;
    fn get_action(&self, id: &ActionId) -> Result<Option<Action>>;
    fn put_action(&self, action: &mut Action) -> Result<ActionId>;
    fn list_actions(&self) -> Result<Vec<ActionId>>;
    fn list_blobs(&self) -> Result<Vec<ContentHash>>;
    fn list_trees(&self) -> Result<Vec<ContentHash>>;

    fn put_blob_bytes_with_hash(&self, data: &[u8], hash: ContentHash) -> Result<ContentHash> {
        self.put_blob_with_hash(&Blob::from_slice(data), hash)
    }

    /// Return the raw rmp-encoded tree body for `hash`.
    ///
    /// This is a migration seam, not a runtime compatibility reader: callers
    /// that need current tree semantics should use [`ObjectStore::get_tree`].
    /// Backends with direct raw storage override this so one-shot migrations can
    /// canonicalize older tree encodings without reintroducing fallback decode
    /// into the durable `Tree` type.
    fn get_tree_serialized(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>> {
        Ok(self
            .get_tree(hash)?
            .map(|tree| rmp_serde::to_vec(&tree))
            .transpose()?)
    }

    fn put_tree_serialized(&self, data: &[u8], hash: ContentHash) -> Result<ContentHash> {
        let tree: Tree = rmp_serde::from_slice(data)?;
        tree.validate()?;
        if tree.hash() != hash {
            return Err(HeddleError::Corruption {
                expected: hash,
                found: tree.hash(),
            });
        }
        self.put_tree(&tree)
    }

    fn put_state_serialized(&self, data: &[u8], id: ChangeId) -> Result<()> {
        let state: State = rmp_serde::from_slice(data)?;
        if state.change_id != id {
            return Err(HeddleError::InvalidObject(format!(
                "state change_id mismatch: expected {}, found {}",
                id, state.change_id
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
            pack::PackObjectId::Hash(hash) => {
                if let Some(blob) = self.get_blob(hash)? {
                    return Ok(Some((pack::ObjectType::Blob, blob.content().to_vec())));
                }
                if let Some(tree) = self.get_tree(hash)? {
                    return Ok(Some((
                        pack::ObjectType::Tree,
                        rmp_serde::to_vec_named(&tree)?,
                    )));
                }
                if let Some(action) = self.get_action(&ActionId::from_hash(*hash))? {
                    return Ok(Some((
                        pack::ObjectType::Action,
                        rmp_serde::to_vec_named(&action)?,
                    )));
                }
                Ok(None)
            }
            pack::PackObjectId::ChangeId(change_id) => {
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

    fn install_pack(&self, pack_data: &[u8], index_data: &[u8]) -> Result<Vec<pack::PackObjectId>> {
        let reader = pack::PackReader::from_slice(pack_data, index_data)?;
        let ids = reader.list_ids();
        for id in &ids {
            let Some((obj_type, data)) = reader.get_object(id)? else {
                continue;
            };
            match (id, obj_type) {
                (pack::PackObjectId::Hash(hash), pack::ObjectType::Blob) => {
                    self.put_blob_bytes_with_hash(&data, *hash)?;
                }
                (pack::PackObjectId::Hash(hash), pack::ObjectType::Tree) => {
                    self.put_tree_serialized(&data, *hash)?;
                }
                (pack::PackObjectId::Hash(hash), pack::ObjectType::Action) => {
                    self.put_action_serialized(&data, ActionId::from_hash(*hash))?;
                }
                (pack::PackObjectId::ChangeId(change_id), pack::ObjectType::State) => {
                    self.put_state_serialized(&data, *change_id)?;
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

    fn pack_objects(&self, aggressive: bool) -> Result<(u64, u64)> {
        let _ = aggressive;
        Ok((0, 0))
    }

    fn prune_loose_objects(&self) -> Result<(u64, u64)> {
        Ok((0, 0))
    }

    fn begin_snapshot_write_batch(&self) -> Result<()> {
        Ok(())
    }

    fn flush_snapshot_write_batch(&self) -> Result<()> {
        Ok(())
    }

    fn abort_snapshot_write_batch(&self) {}

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
    fn has_state_visibility_for_state(&self, _state: &ChangeId) -> Result<bool> {
        Ok(false)
    }

    /// Return the raw rmp-encoded `StateVisibilityBlob` bytes for `state`,
    /// or `Ok(None)` if no sidecar exists. The bytes are the wire-transfer
    /// payload for state visibility.
    ///
    /// Default impl returns `Ok(None)`.
    fn get_state_visibility_bytes_for_state(&self, _state: &ChangeId) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    /// Persist raw `StateVisibilityBlob` bytes for `state`.
    ///
    /// Default impl returns an "unsupported" error so stores that do not
    /// model the sidecar refuse instead of dropping it.
    fn put_state_visibility_bytes_for_state(&self, _state: &ChangeId, _bytes: &[u8]) -> Result<()> {
        Err(HeddleError::InvalidObject(
            "this object store does not support persisting state visibility".to_string(),
        ))
    }

    /// List every state with at least one state-visibility record.
    ///
    /// Default impl returns `Ok(vec![])`.
    fn list_states_with_visibility(&self) -> Result<Vec<ChangeId>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod any_store_tests {
    use tempfile::TempDir;

    use super::*;
    use crate::object::{Attribution, Operation, Principal};

    fn fs_any_store() -> (TempDir, AnyStore) {
        let temp = TempDir::new().unwrap();
        let store = FsStore::new(temp.path().join(".heddle"));
        store.init().unwrap();
        (temp, AnyStore::Fs(store))
    }

    /// Drive every `ObjectStore` method through the `AnyStore::Fs` dispatch arm
    /// so the enum's match-dispatch is exercised end-to-end. This is the
    /// coverage seam for heddle#283: each arm forwards to the inner concrete
    /// store, and a missing arm would fail to compile or silently fall back to
    /// a trait default.
    #[test]
    fn fs_variant_dispatches_every_object_store_method() {
        let (_temp, store) = fs_any_store();

        // ── Blobs ──
        let blob = Blob::from("any-store dispatch blob");
        let blob_hash = store.put_blob(&blob).unwrap();
        assert_eq!(
            ObjectStore::get_blob(&store, &blob_hash)
                .unwrap()
                .unwrap()
                .content(),
            blob.content()
        );
        assert!(store.has_blob(&blob_hash).unwrap());
        assert_eq!(
            ObjectStore::get_blob_bytes(&store, &blob_hash)
                .unwrap()
                .unwrap()
                .as_ref(),
            blob.content()
        );
        assert_eq!(
            store.blob_size(&blob_hash).unwrap().unwrap(),
            blob.content().len() as u64
        );
        assert!(store.loose_blob_path(&blob_hash).is_some());
        store.promote_to_loose_uncompressed(&blob_hash).unwrap();
        assert!(store.list_blobs().unwrap().contains(&blob_hash));

        let bytes_blob = Blob::from("put-with-hash blob");
        let bytes_hash = bytes_blob.hash();
        assert_eq!(
            store.put_blob_with_hash(&bytes_blob, bytes_hash).unwrap(),
            bytes_hash
        );
        let raw_blob = Blob::from("raw bytes blob");
        let raw_hash = raw_blob.hash();
        assert_eq!(
            store
                .put_blob_bytes_with_hash(raw_blob.content(), raw_hash)
                .unwrap(),
            raw_hash
        );

        // ── Trees ──
        let tree = Tree::new();
        let tree_hash = store.put_tree(&tree).unwrap();
        assert!(ObjectStore::get_tree(&store, &tree_hash).unwrap().is_some());
        assert!(store.has_tree(&tree_hash).unwrap());
        assert!(store.list_trees().unwrap().contains(&tree_hash));
        let tree2 = Tree::new();
        let tree2_bytes = rmp_serde::to_vec_named(&tree2).unwrap();
        assert_eq!(
            store
                .put_tree_serialized(&tree2_bytes, tree2.hash())
                .unwrap(),
            tree2.hash()
        );

        // ── States ──
        let attribution =
            Attribution::human(Principal::new("AnyStore Test", "anystore@example.com"));
        let state = State::new(tree_hash, vec![], attribution.clone());
        let change_id = state.change_id;
        store.put_state(&state).unwrap();
        assert!(
            ObjectStore::get_state(&store, &change_id)
                .unwrap()
                .is_some()
        );
        assert!(store.has_state(&change_id).unwrap());
        assert!(store.list_states().unwrap().contains(&change_id));
        let state2 = State::new(tree2.hash(), vec![], attribution.clone());
        let state2_bytes = rmp_serde::to_vec_named(&state2).unwrap();
        store
            .put_state_serialized(&state2_bytes, state2.change_id)
            .unwrap();

        // ── Actions ──
        let mut action = Action::new(
            None,
            ChangeId::generate(),
            Operation::Snapshot,
            "any-store action",
            attribution,
        );
        let action_id = store.put_action(&mut action).unwrap();
        assert!(store.get_action(&action_id).unwrap().is_some());
        assert!(store.list_actions().unwrap().contains(&action_id));
        let action_bytes = rmp_serde::to_vec_named(&action).unwrap();
        store
            .put_action_serialized(&action_bytes, action_id)
            .unwrap();

        // ── Packs ──
        let packed = Blob::from("packed-via-any-store");
        let packed_hash = packed.hash();
        store
            .put_blobs_packed(vec![(packed_hash, packed.into_content())])
            .unwrap();
        assert!(
            store
                .get_pack_object(&pack::PackObjectId::Hash(packed_hash))
                .unwrap()
                .is_some()
        );
        store.pack_objects(false).unwrap();
        store.prune_loose_objects().unwrap();
        // install_pack / install_pack_streaming need valid packfile inputs;
        // exercising the dispatch arm with bogus data is enough — we only
        // assert the call routes through the enum, not the backend behaviour.
        let _ = store.install_pack(&[], &[]);
        let _ = store.install_pack_streaming(
            std::path::Path::new("/nonexistent/pack"),
            std::path::Path::new("/nonexistent/idx"),
        );

        // ── Snapshot write batch ──
        store.begin_snapshot_write_batch().unwrap();
        store.flush_snapshot_write_batch().unwrap();
        store.begin_snapshot_write_batch().unwrap();
        store.abort_snapshot_write_batch();

        // ── Redactions ──
        let redaction = b"any-store redaction bytes";
        store
            .put_redactions_bytes_for_blob(&blob_hash, redaction)
            .unwrap();
        assert!(store.has_redactions_for_blob(&blob_hash).unwrap());
        assert_eq!(
            store
                .get_redactions_bytes_for_blob(&blob_hash)
                .unwrap()
                .as_deref(),
            Some(redaction.as_slice())
        );
        assert!(
            store
                .list_blobs_with_redactions()
                .unwrap()
                .contains(&blob_hash)
        );

        // ── State visibility ──
        let state_visibility = b"any-store state visibility bytes";
        store
            .put_state_visibility_bytes_for_state(&change_id, state_visibility)
            .unwrap();
        assert!(store.has_state_visibility_for_state(&change_id).unwrap());
        assert_eq!(
            store
                .get_state_visibility_bytes_for_state(&change_id)
                .unwrap()
                .as_deref(),
            Some(state_visibility.as_slice())
        );
        assert!(
            store
                .list_states_with_visibility()
                .unwrap()
                .contains(&change_id)
        );

        // ── Caches ──
        store.clear_recent_caches();
    }
}
