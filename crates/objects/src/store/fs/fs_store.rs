// SPDX-License-Identifier: Apache-2.0
//! Core FsStore structure.

#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    hash::Hash,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

use heddle_format::compression::CompressionConfig;

use super::{
    fs_io::{AtomicWriteMode, write_atomic},
    fs_paths::{actions_dir, blobs_dir, packs_dir, states_dir, trees_dir},
    npk1::Npk1Manager,
};
use crate::{
    fs_atomic::sync_directory,
    object::{Blob, ContentHash, State, StateId, Tree},
    store::{Result, SnapshotPackManager, pack::PackObjectId},
};

const RECENT_BLOB_CACHE_CAPACITY: usize = 2_048;
const RECENT_TREE_CACHE_CAPACITY: usize = 1_024;
/// Soft cap on the in-process loose-blob verification cache. Each
/// entry is one `ContentHash` (~32 bytes) so this is ≈2 MB of memory
/// for the upper bound, and clock eviction is bounded by hash
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

fn pack_dir_modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

thread_local! {
    static SNAPSHOT_WRITE_BATCH_DEPTHS: std::cell::RefCell<HashMap<PathBuf, usize>> =
        std::cell::RefCell::new(HashMap::new());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LooseObjectWriteMode {
    Durable,
    BatchDirectorySync,
}

/// Bounded in-process object cache with second-chance clock eviction.
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
/// advances the second-chance clock until the total fits.
///
/// [`get`](Self::get) marks the entry recently used through an atomic bit, so
/// cache hits need only a shared map lock. Eviction advances a clock queue and
/// gives marked entries one additional chance before removal. Both hits and
/// amortized eviction stay O(1), including for the 65k-entry verification
/// cache.
#[derive(Debug)]
pub(super) struct RecentObjectCache<K, V> {
    entries: HashMap<K, RecentObjectCacheEntry<V>>,
    eviction_clock: VecDeque<K>,
    capacity: usize,
    /// Soft cap on cumulative cached bytes; `None` = count-only.
    byte_budget: Option<usize>,
    /// `sizer(value)` in bytes. Only consulted when `byte_budget`
    /// is `Some`.
    sizer: fn(&V) -> usize,
    /// Running sum of `sizer(v)` over all `entries`.
    cached_bytes: usize,
}

#[derive(Debug)]
struct RecentObjectCacheEntry<V> {
    value: V,
    recently_accessed: AtomicBool,
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
            eviction_clock: VecDeque::new(),
            capacity,
            byte_budget: None,
            sizer: |_| 0,
            cached_bytes: 0,
        }
    }

    /// Cache capped by *both* entry count and cumulative bytes.
    /// `sizer` reports each value's heap-ish footprint; the cache
    /// advances the second-chance clock until both caps hold.
    pub(super) fn with_byte_budget(
        capacity: usize,
        byte_budget: usize,
        sizer: fn(&V) -> usize,
    ) -> Self {
        Self {
            entries: HashMap::new(),
            eviction_clock: VecDeque::new(),
            capacity,
            byte_budget: Some(byte_budget),
            sizer,
            cached_bytes: 0,
        }
    }

    /// Lookup with lock-free second-chance promotion inside an already-held
    /// shared map lock. Only insertion and eviction require exclusive access.
    pub(super) fn get(&self, key: &K) -> Option<&V> {
        let entry = self.entries.get(key)?;
        entry.recently_accessed.store(true, Ordering::Relaxed);
        Some(&entry.value)
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
        let removed = self.entries.remove(key)?.value;
        self.cached_bytes = self.cached_bytes.saturating_sub((self.sizer)(&removed));
        Some(removed)
    }

    pub(super) fn insert(&mut self, key: K, value: V) {
        if self.capacity == 0 {
            return;
        }
        let new_bytes = self.byte_budget.map(|_| (self.sizer)(&value)).unwrap_or(0);
        let entry = RecentObjectCacheEntry {
            value,
            recently_accessed: AtomicBool::new(false),
        };
        if let Some(old) = self.entries.insert(key, entry) {
            self.cached_bytes = self.cached_bytes.saturating_sub(
                self.byte_budget
                    .map(|_| (self.sizer)(&old.value))
                    .unwrap_or(0),
            );
        } else {
            self.eviction_clock.push_back(key);
        }
        self.cached_bytes += new_bytes;
        self.evict_to_fit(key);
    }

    /// Advance the second-chance clock until both the count cap and the byte
    /// budget hold. A recently read entry is marked cold and moved to the back
    /// once before it can be evicted. The freshly inserted entry starts at the
    /// back, so it is not the first target (a single entry larger than the
    /// whole budget is kept — the budget is a soft cap, not a hard per-entry
    /// gate; the per-entry `RECENT_BLOB_CACHE_MAX_BYTES` gate already bounds
    /// the largest thing that reaches here).
    fn evict_to_fit(&mut self, admitted_key: K) {
        loop {
            let over_count = self.entries.len() > self.capacity;
            let over_bytes = self
                .byte_budget
                .is_some_and(|budget| self.cached_bytes > budget && self.entries.len() > 1);
            if !over_count && !over_bytes {
                break;
            }
            let Some(candidate) = self.eviction_clock.pop_front() else {
                break;
            };
            let Some(entry) = self.entries.get(&candidate) else {
                continue;
            };
            // The value that triggered this pass has not had an opportunity to
            // serve a read yet. Keep it for this pass when another victim
            // exists; otherwise an all-hot full cache would cycle through the
            // residents and evict the new external object immediately.
            if candidate == admitted_key && self.entries.len() > 1 {
                self.eviction_clock.push_back(candidate);
                continue;
            }
            if entry.recently_accessed.swap(false, Ordering::Relaxed) {
                self.eviction_clock.push_back(candidate);
                continue;
            }
            if let Some(evicted) = self.entries.remove(&candidate) {
                self.cached_bytes = self.cached_bytes.saturating_sub(
                    self.byte_budget
                        .map(|_| (self.sizer)(&evicted.value))
                        .unwrap_or(0),
                );
            }
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
    pub(super) snapshot_delta_search: bool,
    pack_manager: RwLock<SnapshotPackManager>,
    npk1_manager: RwLock<Npk1Manager>,
    /// Last pack-directory generation reflected by both in-memory managers.
    /// Immutable pack publication changes the directory entry set, so one
    /// metadata probe replaces the two full directory scans formerly paid by
    /// every read miss.
    pack_dir_modified: RwLock<Option<SystemTime>>,
    #[cfg(test)]
    pack_dir_generation_probes: AtomicUsize,
    pub(super) recent_blobs: RwLock<RecentObjectCache<ContentHash, Blob>>,
    pub(super) recent_trees: RwLock<RecentObjectCache<ContentHash, Tree>>,
    pub(super) recent_states: RwLock<RecentObjectCache<StateId, State>>,
    pub(super) external_source: Option<Arc<dyn super::super::ExternalObjectSource>>,
    loose_object_write_mode: LooseObjectWriteMode,
    pending_directory_syncs: Mutex<BTreeSet<PathBuf>>,
    #[cfg(test)]
    snapshot_batch_flushes: AtomicUsize,
    /// In-process trust cache for loose-blob cache mirrors. A hash
    /// enters this bounded clock cache when this process either (a) wrote the blob
    /// itself via `promote_to_loose_uncompressed` or (b) successfully
    /// hash-verified it on first read. Bytes-on-disk for any entry
    /// in this cache can be trusted without a re-hash by subsequent
    /// `loose_blob_path` calls within the same process.
    ///
    /// Capped at [`VERIFIED_LOOSE_BLOB_CACHE_CAPACITY`] entries so a
    /// long-lived process (`heddled`) materialising many unrelated
    /// trees doesn't drift into unbounded memory growth. Second-chance
    /// eviction; an evicted hash pays one extra BLAKE3 on its next
    /// read (cost-of-evict ≈ working-set-size BLAKE3 ops). Stored as
    /// `RecentObjectCache<…, ()>` to share the clock-eviction
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
        cloned.snapshot_delta_search = self.snapshot_delta_search;
        cloned.loose_object_write_mode = self.loose_object_write_mode;
        cloned.external_source = self.external_source.clone();
        cloned.pack_dir_modified = RwLock::new(pack_dir_modified(&packs_dir(&self.root)));
        #[cfg(test)]
        cloned
            .pack_dir_generation_probes
            .store(0, Ordering::Relaxed);
        cloned
    }
}

impl FsStore {
    /// Create a new filesystem store rooted at the given path.
    ///
    /// The path should be the `.heddle` directory.
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let pack_manager = SnapshotPackManager::new(packs_dir(&root));
        let npk1_manager = Npk1Manager::new(packs_dir(&root));
        let pack_dir_modified = pack_dir_modified(&packs_dir(&root));
        Self {
            root,
            compression: CompressionConfig::default(),
            snapshot_delta_search: false,
            pack_manager: RwLock::new(pack_manager),
            npk1_manager: RwLock::new(npk1_manager),
            pack_dir_modified: RwLock::new(pack_dir_modified),
            #[cfg(test)]
            pack_dir_generation_probes: AtomicUsize::new(0),
            recent_blobs: RwLock::new(RecentObjectCache::with_byte_budget(
                RECENT_BLOB_CACHE_CAPACITY,
                RECENT_BLOB_CACHE_MAX_TOTAL_BYTES,
                |blob: &Blob| blob.content().len(),
            )),
            recent_trees: RwLock::new(RecentObjectCache::with_capacity(RECENT_TREE_CACHE_CAPACITY)),
            recent_states: RwLock::new(RecentObjectCache::with_capacity(
                RECENT_TREE_CACHE_CAPACITY,
            )),
            external_source: None,
            loose_object_write_mode: LooseObjectWriteMode::Durable,
            pending_directory_syncs: Mutex::new(BTreeSet::new()),
            #[cfg(test)]
            snapshot_batch_flushes: AtomicUsize::new(0),
            verified_loose_blobs: RwLock::new(RecentObjectCache::with_capacity(
                VERIFIED_LOOSE_BLOB_CACHE_CAPACITY,
            )),
        }
    }

    /// Create a new filesystem store with custom compression settings.
    pub fn with_compression(root: impl AsRef<Path>, compression: CompressionConfig) -> Self {
        let root = root.as_ref().to_path_buf();
        let pack_manager = SnapshotPackManager::new(packs_dir(&root));
        let npk1_manager = Npk1Manager::new(packs_dir(&root));
        let pack_dir_modified = pack_dir_modified(&packs_dir(&root));
        Self {
            root,
            compression,
            snapshot_delta_search: false,
            pack_manager: RwLock::new(pack_manager),
            npk1_manager: RwLock::new(npk1_manager),
            pack_dir_modified: RwLock::new(pack_dir_modified),
            #[cfg(test)]
            pack_dir_generation_probes: AtomicUsize::new(0),
            recent_blobs: RwLock::new(RecentObjectCache::with_byte_budget(
                RECENT_BLOB_CACHE_CAPACITY,
                RECENT_BLOB_CACHE_MAX_TOTAL_BYTES,
                |blob: &Blob| blob.content().len(),
            )),
            recent_trees: RwLock::new(RecentObjectCache::with_capacity(RECENT_TREE_CACHE_CAPACITY)),
            recent_states: RwLock::new(RecentObjectCache::with_capacity(
                RECENT_TREE_CACHE_CAPACITY,
            )),
            external_source: None,
            loose_object_write_mode: LooseObjectWriteMode::Durable,
            pending_directory_syncs: Mutex::new(BTreeSet::new()),
            #[cfg(test)]
            snapshot_batch_flushes: AtomicUsize::new(0),
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
        self.remember_pack_dir_modified()?;
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

    /// Enable or disable sliding-window delta search for snapshot packs.
    pub fn set_snapshot_delta_search(&mut self, enabled: bool) {
        self.snapshot_delta_search = enabled;
    }

    pub fn loose_object_write_mode(&self) -> LooseObjectWriteMode {
        self.loose_object_write_mode
    }

    pub fn set_loose_object_write_mode(&mut self, mode: LooseObjectWriteMode) {
        self.loose_object_write_mode = mode;
    }

    /// Configure a read-through source for objects not present in the native
    /// store. Writes always remain native.
    pub fn set_external_source(&mut self, source: Arc<dyn super::super::ExternalObjectSource>) {
        self.external_source = Some(source);
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
        manager.reload()?;
        drop(manager);
        let mut npk1 = self.npk1_manager.write().map_err(|_| {
            crate::store::HeddleError::Config("Failed to acquire NPK1 manager lock".to_string())
        })?;
        npk1.reload()?;
        drop(npk1);
        self.remember_pack_dir_modified()
    }

    /// Reload pack files only if the immutable pack set changed on disk.
    /// Cheap discovery when nothing changed; full reload when a sibling
    /// `FsStore` installed a pack or atomically replaced a generation.
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
        let packs = packs_dir(&self.root);
        let disk_modified = self.current_pack_dir_modified(&packs);
        let observed = self.pack_dir_modified.read().map_err(|_| {
            crate::store::HeddleError::Config(
                "Failed to acquire pack directory generation lock".to_string(),
            )
        })?;
        if *observed == disk_modified {
            return Ok(false);
        }
        drop(observed);

        // Serialize the generation transition, then recheck in case another
        // reader refreshed both managers while this thread was waiting.
        let mut observed = self.pack_dir_modified.write().map_err(|_| {
            crate::store::HeddleError::Config(
                "Failed to acquire pack directory generation lock".to_string(),
            )
        })?;
        let disk_modified = self.current_pack_dir_modified(&packs);
        if *observed == disk_modified {
            return Ok(false);
        }
        let mut manager = self.pack_manager.write().map_err(|_| {
            crate::store::HeddleError::Config("Failed to acquire pack manager lock".to_string())
        })?;
        manager.reload()?;
        drop(manager);
        let mut npk1 = self.npk1_manager.write().map_err(|_| {
            crate::store::HeddleError::Config("Failed to acquire NPK1 manager lock".to_string())
        })?;
        npk1.reload()?;
        *observed = self.current_pack_dir_modified(&packs);
        Ok(true)
    }

    pub(super) fn remember_pack_dir_modified(&self) -> Result<()> {
        let mut observed = self.pack_dir_modified.write().map_err(|_| {
            crate::store::HeddleError::Config(
                "Failed to acquire pack directory generation lock".to_string(),
            )
        })?;
        *observed = self.current_pack_dir_modified(&packs_dir(&self.root));
        Ok(())
    }

    fn current_pack_dir_modified(&self, packs: &Path) -> Option<SystemTime> {
        #[cfg(test)]
        self.pack_dir_generation_probes
            .fetch_add(1, Ordering::Relaxed);
        pack_dir_modified(packs)
    }

    #[cfg(test)]
    pub(super) fn reset_pack_dir_generation_probes(&self) {
        self.pack_dir_generation_probes.store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(super) fn pack_dir_generation_probes(&self) -> usize {
        self.pack_dir_generation_probes.load(Ordering::Relaxed)
    }

    /// Get the pack manager for pack operations.
    pub fn pack_manager(&self) -> &RwLock<SnapshotPackManager> {
        &self.pack_manager
    }

    pub(super) fn npk1_manager(&self) -> &RwLock<Npk1Manager> {
        &self.npk1_manager
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
        let mut ids = manager.list_all_ids()?;
        drop(manager);
        let npk1 = self.npk1_manager.read().map_err(|_| {
            crate::store::HeddleError::Config("Failed to acquire NPK1 manager lock".to_string())
        })?;
        ids.extend(npk1.list_ids()?.into_iter().map(PackObjectId::Hash));
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    pub(super) fn write_loose_object_atomic(&self, path: &Path, data: &[u8]) -> Result<()> {
        let batch_active = SNAPSHOT_WRITE_BATCH_DEPTHS
            .with(|depths| depths.borrow().get(&self.root).copied().unwrap_or_default() > 0);
        let configured_mode = if batch_active {
            LooseObjectWriteMode::BatchDirectorySync
        } else {
            self.loose_object_write_mode
        };

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
        self.write_reconstructible_cache(path, data)
    }

    /// Atomically publish reconstructible cache bytes without a durability
    /// barrier. The caller must be able to rebuild the file from an
    /// authoritative object after a crash.
    pub(super) fn write_reconstructible_cache(&self, path: &Path, data: &[u8]) -> Result<()> {
        write_atomic(path, data, AtomicWriteMode::NoSync, None)
    }

    pub(super) fn begin_snapshot_write_batch_impl(&self) -> Result<()> {
        SNAPSHOT_WRITE_BATCH_DEPTHS.with(|depths| {
            *depths.borrow_mut().entry(self.root.clone()).or_default() += 1;
        });
        Ok(())
    }

    pub(super) fn flush_snapshot_write_batch_impl(&self) -> Result<()> {
        let had_batch = SNAPSHOT_WRITE_BATCH_DEPTHS.with(|depths| {
            let mut depths = depths.borrow_mut();
            let Some(depth) = depths.get_mut(&self.root) else {
                return false;
            };
            *depth -= 1;
            if *depth == 0 {
                depths.remove(&self.root);
            }
            true
        });
        if !had_batch {
            return Ok(());
        }

        #[cfg(test)]
        self.snapshot_batch_flushes.fetch_add(1, Ordering::Relaxed);

        // Batches may overlap across snapshot preparers. Each successful
        // preparer must establish durability for its own writes before it can
        // publish an oplog edge, even while another batch remains active.
        // Draining the shared set is safe: entries taken by another flush are
        // already durable, and every write from this batch was queued before
        // this call acquired the set.
        let _ = self.flush_pending_directory_syncs()?;
        Ok(())
    }

    pub(super) fn abort_snapshot_write_batch_impl(&self) {
        let should_flush = SNAPSHOT_WRITE_BATCH_DEPTHS.with(|depths| {
            let mut depths = depths.borrow_mut();
            let Some(depth) = depths.get_mut(&self.root) else {
                // A preceding flush may have removed the thread-local batch
                // before its directory sync failed. Preserve abort's
                // conservative retry of those pending syncs.
                return true;
            };
            *depth -= 1;
            if *depth == 0 {
                depths.remove(&self.root);
                true
            } else {
                false
            }
        });
        // Immutable objects staged by a failed snapshot are harmless orphans.
        // Never clear another concurrent preparation's pending directory syncs;
        // when this was the last batch, conservatively make every staged rename
        // durable before returning.
        if should_flush {
            let _ = self.flush_pending_directory_syncs();
        }
    }

    #[cfg(test)]
    pub(super) fn pending_directory_sync_count(&self) -> usize {
        self.pending_directory_syncs
            .lock()
            .map(|pending| pending.len())
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(super) fn snapshot_batch_flush_count(&self) -> usize {
        self.snapshot_batch_flushes.load(Ordering::Relaxed)
    }
}
