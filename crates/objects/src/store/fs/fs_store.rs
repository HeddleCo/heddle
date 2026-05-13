// SPDX-License-Identifier: Apache-2.0
//! Core FsStore structure.

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    hash::Hash,
    path::{Path, PathBuf},
    sync::{Mutex, RwLock},
};

use super::{
    fs_io::{AtomicWriteMode, write_atomic},
    fs_paths::{actions_dir, blobs_dir, packs_dir, states_dir, trees_dir},
};
use crate::{
    fs_atomic::sync_directory,
    object::{Blob, ChangeId, ContentHash, State, Tree},
    store::{
        CompressionConfig, Result,
        pack::{PackManager, PackObjectId},
    },
};

const RECENT_BLOB_CACHE_CAPACITY: usize = 2_048;
const RECENT_TREE_CACHE_CAPACITY: usize = 1_024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LooseObjectWriteMode {
    Durable,
    BatchDirectorySync,
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
    fn with_capacity(capacity: usize) -> Self {
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
    snapshot_write_batch_depth: Mutex<usize>,
    pending_directory_syncs: Mutex<BTreeSet<PathBuf>>,
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
            snapshot_write_batch_depth: Mutex::new(0),
            pending_directory_syncs: Mutex::new(BTreeSet::new()),
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
            snapshot_write_batch_depth: Mutex::new(0),
            pending_directory_syncs: Mutex::new(BTreeSet::new()),
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

    pub(super) fn write_pack_atomic(&self, path: &Path, data: &[u8]) -> Result<()> {
        write_atomic(path, data, AtomicWriteMode::Durable, None)
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