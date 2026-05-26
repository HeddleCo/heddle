// SPDX-License-Identifier: Apache-2.0
//! Backend-neutral object storage abstractions and concrete implementations.

use std::{path::PathBuf, sync::Arc};

use crate::object::{Action, ActionId, Blob, ChangeId, ContentHash, State, Tree};

pub mod agent_registry;
pub mod atomic;
pub mod compression;
pub mod fs;
pub mod liveness;
#[cfg(any(test, feature = "memory-backend"))]
pub mod memory;
pub mod pack;
pub mod shallow;
pub mod store_compliance;

#[cfg(feature = "s3")]
mod s3;

pub use agent_registry::{
    ActorChainNode, AgentEntry, AgentRegistry, AgentStatus, AgentUsageSummary, ContextQueryEntry,
    ReserveOutcome, generate_agent_id,
};
pub use compression::{CompressionConfig, CompressionError, compress, decompress};
pub use fs::FsStore;
pub use liveness::{Liveness, current_boot_id, is_owner_alive, process_alive};
#[cfg(any(test, feature = "memory-backend"))]
pub use memory::InMemoryStore;
pub use pack::{PackBuilder, PackObjectId, PackReader, PackStats};
#[cfg(feature = "s3")]
pub use s3::{S3Store, S3StoreBuilder};
pub use shallow::ShallowInfo;

pub use crate::error::{HeddleError as StoreError, HeddleError, Result};

impl From<CompressionError> for HeddleError {
    fn from(e: CompressionError) -> Self {
        HeddleError::Compression(e.to_string())
    }
}

#[derive(Clone)]
pub struct SharedStore(pub Arc<dyn ObjectStore>);

impl ObjectStore for SharedStore {
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>> {
        self.0.get_blob(hash)
    }
    fn put_blob(&self, blob: &Blob) -> Result<ContentHash> {
        self.0.put_blob(blob)
    }
    fn put_blob_with_hash(&self, blob: &Blob, hash: ContentHash) -> Result<ContentHash> {
        self.0.put_blob_with_hash(blob, hash)
    }
    fn has_blob(&self, hash: &ContentHash) -> Result<bool> {
        self.0.has_blob(hash)
    }
    fn blob_size(&self, hash: &ContentHash) -> Result<Option<u64>> {
        self.0.blob_size(hash)
    }
    fn loose_blob_path(&self, hash: &ContentHash) -> Option<PathBuf> {
        self.0.loose_blob_path(hash)
    }
    fn promote_to_loose_uncompressed(&self, hash: &ContentHash) -> Result<bool> {
        self.0.promote_to_loose_uncompressed(hash)
    }
    fn clear_recent_caches(&self) {
        self.0.clear_recent_caches()
    }
    fn get_blob_bytes(&self, hash: &ContentHash) -> Result<Option<bytes::Bytes>> {
        self.0.get_blob_bytes(hash)
    }
    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        self.0.get_tree(hash)
    }
    fn put_tree(&self, tree: &Tree) -> Result<ContentHash> {
        self.0.put_tree(tree)
    }
    fn has_tree(&self, hash: &ContentHash) -> Result<bool> {
        self.0.has_tree(hash)
    }
    fn get_state(&self, id: &ChangeId) -> Result<Option<State>> {
        self.0.get_state(id)
    }
    fn put_state(&self, state: &State) -> Result<()> {
        self.0.put_state(state)
    }
    fn has_state(&self, id: &ChangeId) -> Result<bool> {
        self.0.has_state(id)
    }
    fn list_states(&self) -> Result<Vec<ChangeId>> {
        self.0.list_states()
    }
    fn get_action(&self, id: &ActionId) -> Result<Option<Action>> {
        self.0.get_action(id)
    }
    fn put_action(&self, action: &mut Action) -> Result<ActionId> {
        self.0.put_action(action)
    }
    fn list_actions(&self) -> Result<Vec<ActionId>> {
        self.0.list_actions()
    }
    fn list_blobs(&self) -> Result<Vec<ContentHash>> {
        self.0.list_blobs()
    }
    fn list_trees(&self) -> Result<Vec<ContentHash>> {
        self.0.list_trees()
    }
    fn get_pack_object(
        &self,
        id: &pack::PackObjectId,
    ) -> Result<Option<(pack::ObjectType, Vec<u8>)>> {
        self.0.get_pack_object(id)
    }
    fn install_pack(&self, pack_data: &[u8], index_data: &[u8]) -> Result<Vec<pack::PackObjectId>> {
        self.0.install_pack(pack_data, index_data)
    }
    fn install_pack_streaming(
        &self,
        pack_path: &std::path::Path,
        index_path: &std::path::Path,
    ) -> Result<()> {
        self.0.install_pack_streaming(pack_path, index_path)
    }
    fn put_blobs_packed(&self, blobs: Vec<(ContentHash, Vec<u8>)>) -> Result<()> {
        self.0.put_blobs_packed(blobs)
    }
    fn put_trees_packed(&self, trees: Vec<(ContentHash, Vec<u8>)>) -> Result<()> {
        self.0.put_trees_packed(trees)
    }
    fn begin_snapshot_write_batch(&self) -> Result<()> {
        self.0.begin_snapshot_write_batch()
    }
    fn flush_snapshot_write_batch(&self) -> Result<()> {
        self.0.flush_snapshot_write_batch()
    }
    fn abort_snapshot_write_batch(&self) {
        self.0.abort_snapshot_write_batch()
    }
    fn has_redactions_for_blob(&self, blob: &ContentHash) -> Result<bool> {
        self.0.has_redactions_for_blob(blob)
    }
    fn get_redactions_bytes_for_blob(&self, blob: &ContentHash) -> Result<Option<Vec<u8>>> {
        self.0.get_redactions_bytes_for_blob(blob)
    }
    fn put_redactions_bytes_for_blob(&self, blob: &ContentHash, bytes: &[u8]) -> Result<()> {
        self.0.put_redactions_bytes_for_blob(blob, bytes)
    }
    fn list_blobs_with_redactions(&self) -> Result<Vec<ContentHash>> {
        self.0.list_blobs_with_redactions()
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
    /// doesn't expose stable filesystem paths (e.g. `InMemoryStore`,
    /// `S3Store`). The default impl returns `None` so non-`FsStore`
    /// backends silently fall through to the bytes path.
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
    /// backends that don't expose loose paths (`InMemoryStore`,
    /// `S3Store`), since the hardlink path is fundamentally
    /// inapplicable there.
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

    /// Trees analogue of [`put_blobs_packed`]. Backends with native
    /// pack support coalesce N tree writes into one fsync-pair (one
    /// for `.pack`, one for `.idx`). Default impl falls through to
    /// per-tree writes via `put_tree_serialized` so backends without
    /// pack support still satisfy the contract.
    ///
    /// Trees are supplied as pre-serialized rmp bytes plus the
    /// pre-computed `Tree::hash()`. The caller is responsible for the
    /// (hash, bytes) round-trip — backends will validate corruption
    /// the same way `put_tree_serialized` does.
    fn put_trees_packed(&self, trees: Vec<(ContentHash, Vec<u8>)>) -> Result<()> {
        for (hash, data) in trees {
            if !self.has_tree(&hash)? {
                self.put_tree_serialized(&data, hash)?;
            }
        }
        Ok(())
    }

    fn install_pack(&self, pack_data: &[u8], index_data: &[u8]) -> Result<Vec<pack::PackObjectId>> {
        let reader = pack::PackReader::from_bytes(pack_data.to_vec(), index_data.to_vec())?;
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
    /// Returns nothing — callers that need the list of installed ids
    /// can read the freshly-installed pack via the store. Most
    /// callers (including `Importer`) already track inserted ids
    /// out-of-band via the sha map and don't need a return value.
    fn install_pack_streaming(
        &self,
        pack_path: &std::path::Path,
        index_path: &std::path::Path,
    ) -> Result<()> {
        let pack_data = std::fs::read(pack_path).map_err(StoreError::from)?;
        let index_data = std::fs::read(index_path).map_err(StoreError::from)?;
        self.install_pack(&pack_data, &index_data)?;
        // Default impl: clean up the staged files. Override
        // implementations that move/rename should not call super and
        // should manage the file lifecycle themselves.
        let _ = std::fs::remove_file(pack_path);
        let _ = std::fs::remove_file(index_path);
        Ok(())
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
}
