// SPDX-License-Identifier: Apache-2.0
//! Core FsStore structure.

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    hash::Hash,
    path::{Path, PathBuf},
    sync::{Mutex, RwLock},
};

use heddle_format::compression::CompressionConfig;

use super::{
    fs_io::{AtomicWriteMode, write_atomic},
    fs_paths::{actions_dir, blobs_dir, packs_dir, states_dir, trees_dir},
};
use crate::{
    fs_atomic::sync_directory,
    object::{Blob, ContentHash, State, StateId, Tree},
    store::{
        Result,
        pack::{PackManager, PackObjectId},
    },
};

const RECENT_BLOB_CACHE_CAPACITY: usize = 2_048;
const RECENT_TREE_CACHE_CAPACITY: usize = 1_024;
/// Soft cap on the in-process loose-blob verification cache. Each
/// entry is one `ContentHash` (~32 bytes) so this is ≈2 MB of memory
/// for the upper bound, and the LRU eviction is bounded by hash
/// hits rather than store size. 65k entries covers the typical hot
/// working set for million-blob monorepos; a daemon that materialises
/// dozens of unrelated trees won't drift toward unbounded growth.
const VERIFIED_LOOSE_BLOB_CACHE_CAPACITY: usize = 65_536;
/// Blobs larger than this are not stored in `recent_blobs` so a single
/// multi-MB read cannot thrash the hot working set. 4 MiB matches the
/// typical "large file" boundary used elsewhere in the object path.
pub(super) const RECENT_BLOB_CACHE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Total-byte budget for `recent_blobs`. Without it, populate-on-read
/// could retain `RECENT_BLOB_CACHE_CAPACITY` (2048) × the 4 MiB
/// per-entry gate ≈ 8 GiB of deep-cloned blob bytes for a read-only
/// workload (mount / `heddled`) that streams many cold blobs. 256 MiB
/// caps the resident blob-cache footprint while still holding a deep
/// hot working set of small objects (the common case).
pub(super) const RECENT_BLOB_CACHE_MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LooseObjectWriteMode {
    Durable,
    BatchDirectorySync,
}

/// Bounded in-process object cache with true LRU eviction.
///
/// Two independent caps are enforced on every [`insert`](Self::insert):
///
/// * `capacity` — the maximum entry *count*.
/// * `byte_budget` — a soft cap on the cumulative *bytes* of the
///   cached values, sized by the per-entry `sizer` closure. `None`
///   disables the byte cap (caches whose values are effectively
///   fixed-size, e.g. the `()`-valued verified-loose cache).
///
/// The byte budget is what keeps populate-on-read bounded: a read-only
/// workload (mount / `heddled`) that streams many multi-MB blobs
/// through `get_blob` can otherwise retain `capacity × max-entry-bytes`
/// of deep-cloned `Vec`s. With the budget, inserting a new large blob
/// evicts LRU entries until the total fits.
///
/// [`get`](Self::get) promotes the key to MRU; [`insert`](Self::insert)
/// treats re-insert as a touch. Evicts from the front of `order` when
/// over either cap.
#[derive(Debug)]
pub(super) struct RecentObjectCache<K, V> {
    entries: HashMap<K, V>,
    order: VecDeque<K>,
    capacity: usize,
    /// Soft cap on cumulative cached bytes; `None` = count-only.
    byte_budget: Option<usize>,
    /// `sizer(value)` in bytes. Only consulted when `byte_budget`
    /// is `Some`.
    sizer: fn(&V) -> usize,
    /// Running sum of `sizer(v)` over all `entries`.
    cached_bytes: usize,
}

impl<K, V> RecentObjectCache<K, V>
where
    K: Copy + Eq + Hash,
{
    /// Count-capped cache with no byte budget. Used for caches whose
    /// values are effectively fixed-size (e.g. the verified-loose
    /// marker cache).
    pub(super) fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity,
            byte_budget: None,
            sizer: |_| 0,
            cached_bytes: 0,
        }
    }

    /// Cache capped by *both* entry count and cumulative bytes.
    /// `sizer` reports each value's heap-ish footprint; the cache
    /// evicts LRU entries until both caps hold.
    pub(super) fn with_byte_budget(
        capacity: usize,
        byte_budget: usize,
        sizer: fn(&V) -> usize,
    ) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity,
            byte_budget: Some(byte_budget),
            sizer,
            cached_bytes: 0,
        }
    }

    /// Lookup with LRU promotion. Callers that hold only a read lock must
    /// upgrade to a write lock before calling this (promotion mutates
    /// `order`).
    pub(super) fn get(&mut self, key: &K) -> Option<&V> {
        if !self.entries.contains_key(key) {
            return None;
        }
        self.promote(key);
        self.entries.get(key)
    }

    /// Presence check without promotion. Cheap enough to run under a
    /// read lock — used both by verified-loose probes and by `has_*`
    /// existence checks that must not serialize concurrent readers on
    /// the exclusive write lock a promoting `get` would need.
    pub(super) fn contains(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Drop `key` from the cache entirely. Returns the evicted value if
    /// present. Targeted counterpart to the redaction-`purge` cache
    /// drop: a purged blob's bytes must not linger in `recent_blobs`
    /// where a long-lived process would keep serving (or reporting
    /// present) the destroyed content. The production purge path drops
    /// the whole cache via `clear_recent_caches` (it crosses the
    /// generic `ObjectStore` seam); this per-key variant backs the
    /// store-level `evict_recent_blob` used in tests.
    #[cfg(test)]
    pub(super) fn remove(&mut self, key: &K) -> Option<V> {
        let removed = self.entries.remove(key)?;
        if let Some(position) = self.order.iter().position(|existing| existing == key) {
            self.order.remove(position);
        }
        self.cached_bytes = self.cached_bytes.saturating_sub((self.sizer)(&removed));
        Some(removed)
    }

    pub(super) fn insert(&mut self, key: K, value: V) {
        if self.capacity == 0 {
            return;
        }
        let new_bytes = self.byte_budget.map(|_| (self.sizer)(&value)).unwrap_or(0);
        if let Some(old) = self.entries.insert(key, value) {
            self.cached_bytes = self
                .cached_bytes
                .saturating_sub(self.byte_budget.map(|_| (self.sizer)(&old)).unwrap_or(0));
            self.promote(&key);
        } else {
            self.order.push_back(key);
        }
        self.cached_bytes += new_bytes;
        self.evict_to_fit();
    }

    /// Evict from the LRU front until both the count cap and the byte
    /// budget hold. The freshly-inserted entry is at the MRU back, so
    /// it is never the eviction target (a single entry larger than the
    /// whole budget is kept — the budget is a soft cap, not a hard
    /// per-entry gate; the per-entry `RECENT_BLOB_CACHE_MAX_BYTES` gate
    /// already bounds the largest thing that reaches here).
    fn evict_to_fit(&mut self) {
        loop {
            let over_count = self.entries.len() > self.capacity;
            let over_bytes = self
                .byte_budget
                .is_some_and(|budget| self.cached_bytes > budget && self.entries.len() > 1);
            if !over_count && !over_bytes {
                break;
            }
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            // A key appears at most once in `order`; if it was already
            // removed by a concurrent logical path we just skip.
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.cached_bytes = self.cached_bytes.saturating_sub(
                    self.byte_budget
                        .map(|_| (self.sizer)(&evicted))
                        .unwrap_or(0),
                );
            }
        }
    }

    fn promote(&mut self, key: &K) {
        if let Some(position) = self.order.iter().position(|existing| existing == key) {
            let key = self.order.remove(position).expect("position in range");
            self.order.push_back(key);
        }
    }
}

/// Filesystem-based storage for Heddle objects.
///
/// Layout:
/// ```text
/// .heddle/
///   objects/
///     blobs/
///       ab/
///         cdef1234...
///     trees/
///       ab/
///         cdef1234...
///     states/
///       <state_id>.state
///   actions/
///     <action_id>.action
///   packs/
///     <hash>.pack
///     <hash>.idx
/// ```
pub struct FsStore {
    pub(super) root: PathBuf,
    pub(super) compression: CompressionConfig,
    pack_manager: RwLock<PackManager>,
    pub(super) recent_blobs: RwLock<RecentObjectCache<ContentHash, Blob>>,
    pub(super) recent_trees: RwLock<RecentObjectCache<ContentHash, Tree>>,
    pub(super) recent_states: RwLock<RecentObjectCache<StateId, State>>,
    loose_object_write_mode: LooseObjectWriteMode,
    snapshot_write_batch_depth: Mutex<usize>,
    pending_directory_syncs: Mutex<BTreeSet<PathBuf>>,
    /// In-process trust cache for loose-blob cache mirrors. A hash
    /// enters this LRU when this process either (a) wrote the blob
    /// itself via `promote_to_loose_uncompressed` or (b) successfully
    /// hash-verified it on first read. Bytes-on-disk for any entry
    /// in this cache can be trusted without a re-hash by subsequent
    /// `loose_blob_path` calls within the same process.
    ///
    /// Capped at [`VERIFIED_LOOSE_BLOB_CACHE_CAPACITY`] entries so a
    /// long-lived process (`heddled`) materialising many unrelated
    /// trees doesn't drift into unbounded memory growth. LRU
    /// eviction; an evicted hash pays one extra BLAKE3 on its next
    /// read (cost-of-evict ≈ working-set-size BLAKE3 ops). Stored as
    /// `RecentObjectCache<…, ()>` to share the LRU-eviction
    /// machinery with the other on-store caches; the unit value is
    /// a marker that the corresponding loose mirror was verified.
    ///
    /// Pairs with `AtomicWriteMode::NoSync` on the write side: a
    /// crashed promote leaves a torn cache-mirror file, but its
    /// hash won't match on the next process's first-read verify,
    /// so the reader falls through to a fresh promote off the pack.
    pub(super) verified_loose_blobs: RwLock<RecentObjectCache<ContentHash, ()>>,
}

impl Clone for FsStore {
    fn clone(&self) -> Self {
        let mut cloned = Self::with_compression(&self.root, self.compression);
        cloned.loose_object_write_mode = self.loose_object_write_mode;
        cloned
    }
}

impl FsStore {
    /// Create a new filesystem store rooted at the given path.
    ///
    /// The path should be the `.heddle` directory.
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let pack_manager = PackManager::new(packs_dir(&root));
        Self {
            root,
            compression: CompressionConfig::default(),
            pack_manager: RwLock::new(pack_manager),
            recent_blobs: RwLock::new(RecentObjectCache::with_byte_budget(
                RECENT_BLOB_CACHE_CAPACITY,
                RECENT_BLOB_CACHE_MAX_TOTAL_BYTES,
                |blob: &Blob| blob.content().len(),
            )),
            recent_trees: RwLock::new(RecentObjectCache::with_capacity(RECENT_TREE_CACHE_CAPACITY)),
            recent_states: RwLock::new(RecentObjectCache::with_capacity(
                RECENT_TREE_CACHE_CAPACITY,
            )),
            loose_object_write_mode: LooseObjectWriteMode::Durable,
            snapshot_write_batch_depth: Mutex::new(0),
            pending_directory_syncs: Mutex::new(BTreeSet::new()),
            verified_loose_blobs: RwLock::new(RecentObjectCache::with_capacity(
                VERIFIED_LOOSE_BLOB_CACHE_CAPACITY,
            )),
        }
    }

    /// Create a new filesystem store with custom compression settings.
    pub fn with_compression(root: impl AsRef<Path>, compression: CompressionConfig) -> Self {
        let root = root.as_ref().to_path_buf();
        let pack_manager = PackManager::new(packs_dir(&root));
        Self {
            root,
            compression,
            pack_manager: RwLock::new(pack_manager),
            recent_blobs: RwLock::new(RecentObjectCache::with_byte_budget(
                RECENT_BLOB_CACHE_CAPACITY,
                RECENT_BLOB_CACHE_MAX_TOTAL_BYTES,
                |blob: &Blob| blob.content().len(),
            )),
            recent_trees: RwLock::new(RecentObjectCache::with_capacity(RECENT_TREE_CACHE_CAPACITY)),
            recent_states: RwLock::new(RecentObjectCache::with_capacity(
                RECENT_TREE_CACHE_CAPACITY,
            )),
            loose_object_write_mode: LooseObjectWriteMode::Durable,
            snapshot_write_batch_depth: Mutex::new(0),
            pending_directory_syncs: Mutex::new(BTreeSet::new()),
            verified_loose_blobs: RwLock::new(RecentObjectCache::with_capacity(
                VERIFIED_LOOSE_BLOB_CACHE_CAPACITY,
            )),
        }
    }

    /// Initialize the directory structure.
    pub fn init(&self) -> Result<()> {
        // Durable create so the object-store layout dirs survive crash
        // between mkdir and first object write (L6 residual migration).
        crate::fs_atomic::create_dir_all_durable(&blobs_dir(&self.root))?;
        crate::fs_atomic::create_dir_all_durable(&trees_dir(&self.root))?;
        crate::fs_atomic::create_dir_all_durable(&states_dir(&self.root))?;
        crate::fs_atomic::create_dir_all_durable(&actions_dir(&self.root))?;
        crate::fs_atomic::create_dir_all_durable(&packs_dir(&self.root))?;
        Ok(())
    }

    /// Get the root path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get the compression configuration.
    pub fn compression(&self) -> CompressionConfig {
        self.compression
    }

    /// Set the compression configuration.
    pub fn set_compression(&mut self, compression: CompressionConfig) {
        self.compression = compression;
    }

    pub fn loose_object_write_mode(&self) -> LooseObjectWriteMode {
        self.loose_object_write_mode
    }

    pub fn set_loose_object_write_mode(&mut self, mode: LooseObjectWriteMode) {
        self.loose_object_write_mode = mode;
    }

    fn flush_pending_directory_syncs(&self) -> Result<usize> {
        let pending_dirs = {
            let mut guard = self.pending_directory_syncs.lock().map_err(|_| {
                crate::store::HeddleError::Config(
                    "Failed to acquire pending directory sync lock".to_string(),
                )
            })?;
            if guard.is_empty() {
                return Ok(0);
            }
            let dirs = guard.iter().cloned().collect::<Vec<_>>();
            guard.clear();
            dirs
        };

        for (index, dir) in pending_dirs.iter().enumerate() {
            if let Err(error) = sync_directory(dir) {
                if let Ok(mut guard) = self.pending_directory_syncs.lock() {
                    guard.extend(pending_dirs[index..].iter().cloned());
                }
                return Err(error.into());
            }
        }

        Ok(pending_dirs.len())
    }

    /// Reload pack files from disk.
    ///
    /// Runs L8 install-intent recovery first so crash windows between pack
    /// and index publish are finished or aborted before packs are loaded.
    /// Uses the default intent TTL so abandoned staging is swept.
    pub fn reload_packs(&self) -> Result<()> {
        let packs = packs_dir(&self.root);
        let _ = super::pack_install_journal::recover_pack_install_intents_with_ttl(
            &packs,
            Some(super::pack_install_journal::DEFAULT_PACK_INSTALL_INTENT_TTL_SECS),
        )?;
        // Option D backstop: remove any legacy unpaired packs without intent.
        let _ = super::fs_pack::prune_unpaired_pack_files(&packs)?;
        let mut manager = self.pack_manager.write().map_err(|_| {
            crate::store::HeddleError::Config("Failed to acquire pack manager lock".to_string())
        })?;
        manager.reload()
    }

    /// Reload pack files only if the packs directory has grown on
    /// disk since we last read it. Cheap (one `read_dir` + count)
    /// when nothing changed; full reload only when a sibling
    /// `FsStore` has installed a new pack.
    ///
    /// Returns `true` when a reload happened. Used by `get_*` and
    /// `has_*` paths after an in-memory miss to recover from the
    /// "two FsStores backing the same `.heddle/` directory" case
    /// (typical for lightweight thread worktrees).
    ///
    /// Double-checked locking: the read-lock fast path means a
    /// thundering herd of concurrent misses doesn't serialize on
    /// the write lock; only the first thread that observes a stale
    /// view escalates and does the reload.
    pub(super) fn reload_packs_if_stale(&self) -> Result<bool> {
        // Fast path: read-lock and bail out if disk hasn't grown.
        {
            let manager = self.pack_manager.read().map_err(|_| {
                crate::store::HeddleError::Config("Failed to acquire pack manager lock".to_string())
            })?;
            if !manager.needs_reload()? {
                return Ok(false);
            }
        }
        // Slow path: take the write lock and re-check (another
        // thread may have already reloaded between our drop and
        // re-acquire).
        let mut manager = self.pack_manager.write().map_err(|_| {
            crate::store::HeddleError::Config("Failed to acquire pack manager lock".to_string())
        })?;
        manager.reload_if_disk_grew()
    }

    /// Get the pack manager for pack operations.
    pub fn pack_manager(&self) -> &RwLock<PackManager> {
        &self.pack_manager
    }

    pub fn clear_recent_object_caches(&self) {
        if let Ok(mut blobs) = self.recent_blobs.write() {
            *blobs = RecentObjectCache::with_byte_budget(
                RECENT_BLOB_CACHE_CAPACITY,
                RECENT_BLOB_CACHE_MAX_TOTAL_BYTES,
                |blob: &Blob| blob.content().len(),
            );
        }
        if let Ok(mut trees) = self.recent_trees.write() {
            *trees = RecentObjectCache::with_capacity(RECENT_TREE_CACHE_CAPACITY);
        }
        if let Ok(mut states) = self.recent_states.write() {
            *states = RecentObjectCache::with_capacity(RECENT_TREE_CACHE_CAPACITY);
        }
    }

    /// Drop a single blob hash from the in-process `recent_blobs`
    /// cache. Targeted counterpart to the redaction-`purge` cache drop:
    /// after the loose bytes are physically deleted, a long-lived
    /// process must not keep serving (or reporting present) the purged
    /// content from cache. Idempotent — a miss is a no-op. Test-only:
    /// the production purge path crosses the generic `ObjectStore` seam
    /// and drops the whole cache via `clear_recent_caches`.
    #[cfg(test)]
    pub(super) fn evict_recent_blob(&self, hash: &ContentHash) {
        if let Ok(mut cache) = self.recent_blobs.write() {
            cache.remove(hash);
        }
    }

    pub fn pack_ids(&self) -> Result<Vec<PackObjectId>> {
        let manager = self.pack_manager.read().map_err(|_| {
            crate::store::HeddleError::Config("Failed to acquire pack manager lock".to_string())
        })?;
        manager.list_all_ids()
    }

    pub(super) fn write_loose_object_atomic(&self, path: &Path, data: &[u8]) -> Result<()> {
        let batch_active = self.snapshot_write_batch_depth.lock().map_err(|_| {
            crate::store::HeddleError::Config("Failed to acquire snapshot batch lock".to_string())
        })?;
        let configured_mode = if *batch_active > 0 {
            LooseObjectWriteMode::BatchDirectorySync
        } else {
            self.loose_object_write_mode
        };
        drop(batch_active);

        let mode = match configured_mode {
            LooseObjectWriteMode::Durable => AtomicWriteMode::Durable,
            LooseObjectWriteMode::BatchDirectorySync => AtomicWriteMode::BatchDirectorySync,
        };
        write_atomic(path, data, mode, Some(&self.pending_directory_syncs))
    }

    /// Durable atomic write for pack/index bytes when not going through the
    /// L8 journal (tests / rare call sites). Prefer
    /// [`super::pack_install_journal::install_pack_bytes_journaled`].
    #[allow(dead_code)]
    pub(super) fn write_pack_atomic(&self, path: &Path, data: &[u8]) -> Result<()> {
        write_atomic(path, data, AtomicWriteMode::Durable, None)
    }

    /// Atomic write tuned for *cache-mirror* loose objects: no fsync
    /// at any level. The authoritative copy lives in a pack; if a
    /// crash leaves the cache mirror torn, the read-side hash check
    /// catches it and `promote_to_loose_uncompressed` rebuilds it
    /// from the pack on the next access.
    ///
    /// On macOS APFS, `sync_data` alone costs ~5 ms per call (it
    /// behaves like `F_FULLFSYNC` for tiny writes), and the parent
    /// directory fsync is ~3-10 ms on top. For 1k blobs, that's
    /// 5-15 seconds of pure fsync wallclock — the dominant cost in
    /// the cold materialize path. Dropping both pays back ~30× on
    /// raw create+rename throughput (measured: 200/s with sync_data
    /// vs 5500/s without).
    ///
    /// Safety contract: this is only valid for files whose authority
    /// lives elsewhere. Used by `promote_to_loose_uncompressed`; the
    /// matching `loose_blob_path` reader hash-verifies before
    /// trusting the bytes. Do *not* use for `put_blob` / `put_tree`
    /// / `put_state` — those are the authoritative copy and must
    /// survive a crash.
    pub(super) fn write_loose_object_cache(&self, path: &Path, data: &[u8]) -> Result<()> {
        write_atomic(path, data, AtomicWriteMode::NoSync, None)
    }

    pub(super) fn begin_snapshot_write_batch_impl(&self) -> Result<()> {
        let mut depth = self.snapshot_write_batch_depth.lock().map_err(|_| {
            crate::store::HeddleError::Config("Failed to acquire snapshot batch lock".to_string())
        })?;
        *depth += 1;
        Ok(())
    }

    pub(super) fn flush_snapshot_write_batch_impl(&self) -> Result<()> {
        let should_flush = {
            let mut depth = self.snapshot_write_batch_depth.lock().map_err(|_| {
                crate::store::HeddleError::Config(
                    "Failed to acquire snapshot batch lock".to_string(),
                )
            })?;
            if *depth == 0 {
                return Ok(());
            }
            *depth -= 1;
            *depth == 0
        };

        if should_flush {
            let _ = self.flush_pending_directory_syncs()?;
        }

        Ok(())
    }

    pub(super) fn abort_snapshot_write_batch_impl(&self) {
        if let Ok(mut depth) = self.snapshot_write_batch_depth.lock() {
            *depth = 0;
        }
        if let Ok(mut pending) = self.pending_directory_syncs.lock() {
            pending.clear();
        }
    }

    #[cfg(test)]
    pub(super) fn pending_directory_sync_count(&self) -> usize {
        self.pending_directory_syncs
            .lock()
            .map(|pending| pending.len())
            .unwrap_or(0)
    }
}
