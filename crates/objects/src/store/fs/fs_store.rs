// SPDX-License-Identifier: Apache-2.0
//! Core FsStore structure.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    hash::Hash,
    path::{Path, PathBuf},
    sync::{Mutex, RwLock},
    thread::ThreadId,
};

use super::{
    fs_io::{AtomicWriteMode, promote_staged_object, stage_loose_object, write_atomic},
    fs_paths::{actions_dir, blobs_dir, packs_dir, states_dir, trees_dir},
};
use crate::{
    fs_atomic::{is_staged_temp_name, stage_temp_path},
    object::{Blob, ChangeId, ContentHash, State, Tree},
    store::{
        CompressionConfig, Result,
        pack::{PackManager, PackObjectId},
    },
};

/// Whether a loose-object write is content-addressed (its path is a
/// content hash, so a re-stage at the same path is byte-identical and
/// dedups safely) or mutable (keyed by an external id, so the same path
/// can be legitimately re-staged with DIFFERENT bytes within one batch
/// and the LATEST must win — heddle#550 Finding 1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LooseObjectKind {
    /// Blobs, trees, actions: path derived from the object's content
    /// hash. A repeated stage of the same path carries identical bytes,
    /// so it is safe to dedup-skip.
    ContentAddressed,
    /// State objects keyed by `ChangeId`. `put_state` intentionally
    /// overwrites the same path as a change mutates, so a later staged
    /// write at the same path must SUPERSEDE the earlier one rather than
    /// dedup-skip it; otherwise the promoted state would be stale.
    Mutable,
}

const RECENT_BLOB_CACHE_CAPACITY: usize = 2_048;
const RECENT_TREE_CACHE_CAPACITY: usize = 1_024;
/// Soft cap on the in-process loose-blob verification cache. Each
/// entry is one `ContentHash` (~32 bytes) so this is ≈2 MB of memory
/// for the upper bound, and the FIFO eviction is bounded by hash
/// hits rather than store size. 65k entries covers the typical hot
/// working set for million-blob monorepos; a daemon that materialises
/// dozens of unrelated trees won't drift toward unbounded growth.
const VERIFIED_LOOSE_BLOB_CACHE_CAPACITY: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LooseObjectWriteMode {
    Durable,
    /// Legacy alias for [`LooseObjectWriteMode::Durable`]. Snapshot-batch
    /// deferral is no longer driven by this store-wide mode field — it is
    /// scoped per-batch via `begin_snapshot_write_batch` (heddle#550
    /// Finding 3). Configured standalone via `set_loose_object_write_mode`
    /// with no active batch, a write must still be fully durable on its
    /// own (no `syncfs` is ever coming), so this is treated exactly like
    /// `Durable`. Retained so the public setter keeps its r1 durability
    /// guarantee without a behavioural surprise.
    BatchDirectorySync,
}

/// Per-thread snapshot write-batch state.
///
/// Scoping the batch to the thread that opened it (heddle#550 Finding 3)
/// is what keeps a concurrent unrelated write on the same `FsStore` —
/// e.g. a daemon servicing a second operation — out of this batch's
/// deferred `syncfs` barrier. Only writes issued by the batch-opening
/// thread are staged; every other caller gets normal per-write
/// durability even while this batch is open.
///
/// Each batch is single-threaded by construction (the import/snapshot
/// operation that opens it issues all its object writes from the opening
/// thread), so this entry is only ever touched by its owning thread.
#[derive(Debug, Default)]
struct SnapshotBatch {
    /// begin/flush nesting depth; the `syncfs` barrier + promotion run
    /// when a flush brings this back to 0.
    depth: usize,
    /// Objects staged during this batch: canonical destination -> the
    /// temp file holding the not-yet-promoted bytes. Keyed by canonical
    /// path so a repeated put of the same content-addressed object
    /// dedups instead of staging the bytes twice.
    staged: HashMap<PathBuf, PathBuf>,
}

#[derive(Debug)]
pub(super) struct RecentObjectCache<K, V> {
    entries: HashMap<K, V>,
    order: VecDeque<K>,
    capacity: usize,
}

impl<K, V> RecentObjectCache<K, V>
where
    K: Copy + Eq + Hash,
{
    pub(super) fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    pub(super) fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key)
    }

    pub(super) fn insert(&mut self, key: K, value: V) {
        if self.capacity == 0 {
            return;
        }
        if self.entries.insert(key, value).is_none() {
            self.order.push_back(key);
        }
        while self.entries.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
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
///       <change_id>.state
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
    pub(super) recent_states: RwLock<RecentObjectCache<ChangeId, State>>,
    loose_object_write_mode: LooseObjectWriteMode,
    /// Active snapshot write batches, keyed by the thread that opened
    /// each. See [`SnapshotBatch`] — per-thread scoping is the
    /// heddle#550 Finding 3 fix (a process-wide depth counter swept
    /// every concurrent caller's writes into one operation's batch).
    snapshot_batches: Mutex<HashMap<ThreadId, SnapshotBatch>>,
    /// In-process trust cache for loose-blob cache mirrors. A hash
    /// enters this LRU when this process either (a) wrote the blob
    /// itself via `promote_to_loose_uncompressed` or (b) successfully
    /// hash-verified it on first read. Bytes-on-disk for any entry
    /// in this cache can be trusted without a re-hash by subsequent
    /// `loose_blob_path` calls within the same process.
    ///
    /// Capped at [`VERIFIED_LOOSE_BLOB_CACHE_CAPACITY`] entries so a
    /// long-lived process (`heddled`) materialising many unrelated
    /// trees doesn't drift into unbounded memory growth. FIFO
    /// eviction; an evicted hash pays one extra BLAKE3 on its next
    /// read (cost-of-evict ≈ working-set-size BLAKE3 ops). Stored as
    /// `RecentObjectCache<…, ()>` to share the FIFO-eviction
    /// machinery with the other on-store caches; the unit value is
    /// a marker that the corresponding loose mirror was verified.
    ///
    /// Pairs with `AtomicWriteMode::NoSync` on the write side: a
    /// crashed promote leaves a torn cache-mirror file, but its
    /// hash won't match on the next process's first-read verify,
    /// so the reader falls through to a fresh promote off the pack.
    pub(super) verified_loose_blobs: RwLock<RecentObjectCache<ContentHash, ()>>,
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
            recent_blobs: RwLock::new(RecentObjectCache::with_capacity(RECENT_BLOB_CACHE_CAPACITY)),
            recent_trees: RwLock::new(RecentObjectCache::with_capacity(RECENT_TREE_CACHE_CAPACITY)),
            recent_states: RwLock::new(RecentObjectCache::with_capacity(
                RECENT_TREE_CACHE_CAPACITY,
            )),
            loose_object_write_mode: LooseObjectWriteMode::Durable,
            snapshot_batches: Mutex::new(HashMap::new()),
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
            recent_blobs: RwLock::new(RecentObjectCache::with_capacity(RECENT_BLOB_CACHE_CAPACITY)),
            recent_trees: RwLock::new(RecentObjectCache::with_capacity(RECENT_TREE_CACHE_CAPACITY)),
            recent_states: RwLock::new(RecentObjectCache::with_capacity(
                RECENT_TREE_CACHE_CAPACITY,
            )),
            loose_object_write_mode: LooseObjectWriteMode::Durable,
            snapshot_batches: Mutex::new(HashMap::new()),
            verified_loose_blobs: RwLock::new(RecentObjectCache::with_capacity(
                VERIFIED_LOOSE_BLOB_CACHE_CAPACITY,
            )),
        }
    }

    /// Initialize the directory structure.
    pub fn init(&self) -> Result<()> {
        std::fs::create_dir_all(blobs_dir(&self.root))?;
        std::fs::create_dir_all(trees_dir(&self.root))?;
        std::fs::create_dir_all(states_dir(&self.root))?;
        std::fs::create_dir_all(actions_dir(&self.root))?;
        std::fs::create_dir_all(packs_dir(&self.root))?;
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

    /// Reload pack files from disk.
    pub fn reload_packs(&self) -> Result<()> {
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
            *blobs = RecentObjectCache::with_capacity(RECENT_BLOB_CACHE_CAPACITY);
        }
        if let Ok(mut trees) = self.recent_trees.write() {
            *trees = RecentObjectCache::with_capacity(RECENT_TREE_CACHE_CAPACITY);
        }
        if let Ok(mut states) = self.recent_states.write() {
            *states = RecentObjectCache::with_capacity(RECENT_TREE_CACHE_CAPACITY);
        }
    }

    pub fn pack_ids(&self) -> Result<Vec<PackObjectId>> {
        let manager = self.pack_manager.read().map_err(|_| {
            crate::store::HeddleError::Config("Failed to acquire pack manager lock".to_string())
        })?;
        manager.list_all_ids()
    }

    pub(super) fn write_loose_object_atomic(
        &self,
        path: &Path,
        data: &[u8],
        kind: LooseObjectKind,
    ) -> Result<()> {
        // Is THIS thread inside an active snapshot batch? Only the
        // batch-opening thread's writes are staged for deferred-flush
        // promotion (heddle#550 Finding 3); a concurrent write from any
        // other thread — or any non-batch caller — falls through to a
        // normal fully-durable write below.
        let tid = std::thread::current().id();
        let in_batch_this_thread = {
            let batches = self.lock_snapshot_batches()?;
            batches.get(&tid).is_some_and(|batch| batch.depth > 0)
        };

        if !in_batch_this_thread {
            // No active batch on this thread. `BatchDirectorySync` set
            // standalone via the public setter still means "fully
            // durable" — no `syncfs` is ever coming to back a deferral.
            return write_atomic(path, data, AtomicWriteMode::Durable);
        }

        // Quarantine-then-promote (heddle#550): stage the bytes in a temp
        // file beside the canonical path but DON'T rename into place yet.
        // The canonical path therefore never holds bytes that aren't
        // durably flushed, so a crash before flush can only leave an
        // orphan staging temp (ignored by reads, reclaimed by the
        // abandoned-staging sweep) — never a present-but-torn object that
        // an exists-skip would refuse to rewrite. Promotion (the rename)
        // happens in `flush_snapshot_write_batch`, after the `syncfs`
        // barrier.
        //
        // Reserve the staging slot in `staged` UNDER THE LOCK before the
        // temp file is written, so a concurrent staging sweep (another
        // thread opening a batch on the same store) can never see this
        // temp on disk without also seeing it tracked as in-flight.
        let temp = stage_temp_path(path);
        let superseded = {
            let mut batches = self.lock_snapshot_batches()?;
            match batches.get_mut(&tid) {
                Some(batch) if batch.depth > 0 => {
                    // Dedup applies ONLY to content-addressed objects: a
                    // re-stage at the same path is byte-identical, so skip
                    // it. Mutable (`ChangeId`-keyed) state writes are
                    // exempt and fall through to stage + supersede so the
                    // LATEST version wins (heddle#550 Finding 1).
                    if kind == LooseObjectKind::ContentAddressed
                        && batch.staged.contains_key(path)
                    {
                        return Ok(());
                    }
                    batch.staged.insert(path.to_path_buf(), temp.clone())
                }
                // The batch closed out from under us between the depth
                // probe and re-acquiring the lock. Can't happen on the
                // owning thread, but if it did the bytes would be lost —
                // write durably so the object still lands.
                _ => return write_atomic(path, data, AtomicWriteMode::Durable),
            }
        };

        // Write the reserved temp with the lock released (keep file I/O
        // off the snapshot-batch mutex).
        if let Err(err) = stage_loose_object(path, &temp, data) {
            // The staged write failed (`stage_loose_object` already
            // removed its own partial temp). Roll the reservation back so
            // a later flush never tries to promote a temp that does not
            // exist: restore the version we superseded, or drop the slot
            // if there was none.
            let mut batches = self.lock_snapshot_batches()?;
            if let Some(batch) = batches.get_mut(&tid)
                && batch.staged.get(path) == Some(&temp)
            {
                match superseded {
                    Some(prev) => {
                        batch.staged.insert(path.to_path_buf(), prev);
                    }
                    None => {
                        batch.staged.remove(path);
                    }
                }
            }
            return Err(err);
        }

        // The new bytes are durably staged; the superseded temp (a
        // mutable re-stage, or an unreachable content-addressed dedup
        // race) is now garbage.
        if let Some(prev) = superseded {
            let _ = std::fs::remove_file(prev);
        }
        Ok(())
    }

    fn lock_snapshot_batches(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<ThreadId, SnapshotBatch>>> {
        self.snapshot_batches.lock().map_err(|_| {
            crate::store::HeddleError::Config("Failed to acquire snapshot batch lock".to_string())
        })
    }

    pub(super) fn write_pack_atomic(&self, path: &Path, data: &[u8]) -> Result<()> {
        write_atomic(path, data, AtomicWriteMode::Durable)
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
        write_atomic(path, data, AtomicWriteMode::NoSync)
    }

    pub(super) fn begin_snapshot_write_batch_impl(&self) -> Result<()> {
        let tid = std::thread::current().id();
        let opened_fresh = {
            let mut batches = self.lock_snapshot_batches()?;
            let batch = batches.entry(tid).or_default();
            batch.depth += 1;
            batch.depth == 1
        };
        // When this opens a fresh (not nested) batch, reclaim any staging
        // temps abandoned by a previously crashed import (heddle#550
        // Finding 3). Runs with the lock released — the sweep takes the
        // lock itself to read the live-batch tracked set it must skip.
        if opened_fresh {
            self.sweep_abandoned_staged_temps();
        }
        Ok(())
    }

    pub(super) fn flush_snapshot_write_batch_impl(&self) -> Result<()> {
        let tid = std::thread::current().id();
        // Snapshot the staged set iff this flush closes the (nested)
        // batch. The batch entry is LEFT in the map (at depth 0) while we
        // promote, so a concurrent staging sweep still sees these temps as
        // in-flight and won't reclaim them mid-promotion. It is removed
        // once promotion finishes.
        let staged = {
            let mut batches = self.lock_snapshot_batches()?;
            let Some(batch) = batches.get_mut(&tid) else {
                return Ok(());
            };
            if batch.depth == 0 {
                return Ok(());
            }
            batch.depth -= 1;
            if batch.depth > 0 {
                return Ok(());
            }
            batch.staged.clone()
        };

        let result = self.promote_staged_batch(&staged);

        // Promotion done (or failed): drop the now-quiescent batch entry.
        // A failed promote leaves un-promoted temps behind, but they are
        // no longer tracked once the entry is gone — the next import's
        // begin-time sweep reclaims them.
        {
            let mut batches = self.lock_snapshot_batches()?;
            if batches.get(&tid).is_some_and(|batch| batch.depth == 0) {
                batches.remove(&tid);
            }
        }

        result
    }

    /// Make every object staged during a snapshot batch durable, then
    /// promote (rename) it into its canonical content-addressed path.
    ///
    /// The ordering is what holds the heddle#550 durability invariant:
    /// the data barrier runs while the objects are still in their temp
    /// staging files, so after the promote a canonical path can only
    /// ever reference *durable* bytes. A crash at any point leaves either
    /// orphan temps (pre-barrier) or canonical objects whose data is
    /// already on disk (post-barrier) — never a present-but-torn object.
    /// A second barrier makes the new directory entries themselves
    /// durable before the caller writes any referencing artifact.
    fn promote_staged_batch(&self, staged: &HashMap<PathBuf, PathBuf>) -> Result<()> {
        if staged.is_empty() {
            return Ok(());
        }

        // Step 1: durability barrier for the staged temp *data*. On Linux
        // one `syncfs()` flushes every staged temp in a single
        // filesystem-wide barrier (git's `core.fsyncMethod=batch`) —
        // replacing the N per-object fsyncs that dominate large-history
        // import (heddle#550). Elsewhere each temp was already
        // `sync_data`'d in `stage_loose_object`.
        #[cfg(target_os = "linux")]
        self.syncfs_root()?;

        // Step 2: promote each temp into its canonical path. The bytes are
        // already durable, so a renamed-into-place object is never torn.
        for (canonical, temp) in staged {
            promote_staged_object(temp, canonical)?;
        }

        // Step 3: make the new directory entries durable before any
        // referencing artifact (mapping/oplog/refs) is written. On Linux
        // a second `syncfs()` covers every touched directory; elsewhere
        // fsync each distinct canonical parent directory.
        #[cfg(target_os = "linux")]
        self.syncfs_root()?;
        #[cfg(not(target_os = "linux"))]
        {
            use std::collections::BTreeSet;

            use crate::fs_atomic::sync_directory;
            let mut dirs: BTreeSet<&Path> = BTreeSet::new();
            for canonical in staged.keys() {
                if let Some(parent) = canonical.parent() {
                    dirs.insert(parent);
                }
            }
            for dir in dirs {
                sync_directory(dir)?;
            }
        }

        Ok(())
    }

    /// Single filesystem-wide durability barrier for a snapshot write
    /// batch on Linux (git's `core.fsyncMethod=batch`). `syncfs` flushes
    /// every dirty page on the filesystem backing the object store in one
    /// barrier, making all objects written during the batch durable
    /// without the per-file `sync_data` that dominated large-history
    /// import (heddle#550).
    #[cfg(target_os = "linux")]
    fn syncfs_root(&self) -> Result<()> {
        use std::os::fd::AsRawFd;
        let dir = std::fs::File::open(&self.root).map_err(crate::store::HeddleError::Io)?;
        // SAFETY: `dir` owns a valid open fd for the duration of the call;
        // `syncfs` only reads it to locate the filesystem to flush.
        let rc = unsafe { libc::syncfs(dir.as_raw_fd()) };
        if rc != 0 {
            return Err(crate::store::HeddleError::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    pub(super) fn abort_snapshot_write_batch_impl(&self) {
        let tid = std::thread::current().id();
        let staged = {
            let Ok(mut batches) = self.snapshot_batches.lock() else {
                return;
            };
            batches.remove(&tid).map(|batch| batch.staged)
        };
        // Discard the staged temps. They were never promoted to a
        // canonical path, so there are no torn objects to clean up —
        // just orphan temp files the next run would ignore anyway.
        if let Some(staged) = staged {
            for temp in staged.into_values() {
                let _ = std::fs::remove_file(temp);
            }
        }
        // Invalidate the in-memory object caches (heddle#550 Finding 2).
        // `put_blob`/`put_tree`/`put_state` populate `recent_blobs` /
        // `recent_trees` / `recent_states` eagerly, BEFORE the staged temp
        // is promoted. On abort the temp is discarded and the canonical
        // path is never written, so those cache entries now describe
        // objects that do not exist on disk. The existence-gated read
        // paths already refuse to serve them, but the cache-first
        // `blob_size` fast path would report a never-written object as
        // present — so purge the caches to force post-abort reads to
        // re-resolve from disk.
        self.clear_recent_object_caches();
    }

    /// Reclaim staging temp files abandoned by a crashed or killed import
    /// (heddle#550 Finding 3). A crash before `flush_snapshot_write_batch`
    /// leaves a `.stage-` temp beside every staged object; with no sweep
    /// they accumulate across repeated crashes.
    ///
    /// Safety: only files whose names carry [`STAGED_TEMP_MARKER`] are
    /// touched, so this never removes a canonical object or a concurrent
    /// `write_atomic`'s short-lived `.tmp-` temp. Temps still tracked by a
    /// live batch (in this store's `snapshot_batches`, including one
    /// mid-promotion in `flush`) are skipped, so a concurrent import's
    /// in-flight staging is never reclaimed out from under it.
    ///
    /// Cost: one `read_dir` pass over the loose-object directories,
    /// metadata-only, run once per fresh batch (i.e. once per import) —
    /// negligible beside the import it precedes.
    fn sweep_abandoned_staged_temps(&self) {
        let in_flight: HashSet<PathBuf> = match self.snapshot_batches.lock() {
            Ok(batches) => batches
                .values()
                .flat_map(|batch| batch.staged.values().cloned())
                .collect(),
            // Poisoned lock: skip the opportunistic sweep rather than risk
            // reclaiming a temp we can't prove is abandoned.
            Err(_) => return,
        };

        for dir in [
            blobs_dir(&self.root),
            trees_dir(&self.root),
            states_dir(&self.root),
            actions_dir(&self.root),
        ] {
            sweep_staged_temps_under(&dir, &in_flight);
        }
    }

    /// Number of objects currently staged (written but not yet promoted)
    /// in the calling thread's active snapshot batch.
    #[cfg(test)]
    pub(super) fn staged_object_count(&self) -> usize {
        let tid = std::thread::current().id();
        self.snapshot_batches
            .lock()
            .map(|batches| batches.get(&tid).map(|b| b.staged.len()).unwrap_or(0))
            .unwrap_or(0)
    }
}

/// Remove abandoned `.stage-` temps under `dir`, recursing into the
/// sharded sub-directories blobs/trees use. A temp in `in_flight` (still
/// tracked by a live batch) is left alone; every other staged temp is
/// crash garbage and is reclaimed. Best-effort: any I/O error (missing
/// dir on a fresh store, permission) simply leaves the temp for a later
/// sweep rather than failing the batch.
fn sweep_staged_temps_under(dir: &Path, in_flight: &HashSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            sweep_staged_temps_under(&path, in_flight);
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_staged_temp_name)
            && !in_flight.contains(&path)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}
