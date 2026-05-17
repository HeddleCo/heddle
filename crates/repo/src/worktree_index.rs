// SPDX-License-Identifier: Apache-2.0
//! Binary worktree index for fast stat cache operations.
//!
//! This module provides a binary format for the worktree stat cache,
//! offering faster load/save compared to TOML while maintaining
//! debuggability via the `dump()` function.
//!
//! ## Binary Format (Version 5)
//!
//! ```text
//! Header (24 bytes):
//!   - magic: "HDLEIDX\0" (8 bytes)
//!   - version: u32 (4 bytes)
//!   - file_entry_count: u32 (4 bytes)
//!   - directory_entry_count: u32 (4 bytes) [new in v2]
//!   - untracked_directory_entry_count: u32 (4 bytes) [new in v5]
//!
//! File Entries (variable):
//!   - type: u8 (1 byte: 0x01 = file)
//!   - path_len: u32 (4 bytes)
//!   - path: [u8; path_len] (UTF-8)
//!   - hash: [u8; 32] (ContentHash)
//!   - size: u64 (8 bytes)
//!   - modified_sec: i8 (8 bytes)
//!   - modified_nsec: u32 (4 bytes)
//!   - executable: u8 (1 byte: 0 or 1)
//!   - kind: u8 (1 byte: 0=File, 1=Symlink)
//!
//! Directory Entries (variable) [new in v2, extended in v4]:
//!   - type: u8 (1 byte: 0x02 = directory)
//!   - path_len: u32 (4 bytes)
//!   - path: [u8; path_len] (UTF-8)
//!   - mtime_sec: i64 (8 bytes)
//!   - mtime_nsec: u32 (4 bytes)
//!   - child_count: u32 (4 bytes)
//!   - child_digest: [u8; 32]
//!   - has_clean_tree_hash: u8 (1 byte)
//!   - clean_tree_hash: [u8; 32] (present when has_clean_tree_hash == 1)
//!
//! Untracked Directory Entries (variable) [new in v5]:
//!   - type: u8 (1 byte: 0x03 = untracked directory)
//!   - path_len: u32 (4 bytes)
//!   - path: [u8; path_len] (UTF-8)
//!   - mtime_sec: i64 (8 bytes)
//!   - mtime_nsec: u32 (4 bytes)
//!   - child_count: u32 (4 bytes)
//!   - child_digest: [u8; 32]
//!   - ignore_fingerprint: [u8; 32]
//!   - added_path_count: u32 (4 bytes)
//!   - repeated added paths:
//!     - added_path_len: u32
//!     - added_path bytes: [u8; added_path_len]
//!
//! Footer (4 bytes):
//!   - checksum: u32 (CRC32 of all entries)
//! ```

use std::{collections::BTreeMap, fs, path::Path};

use objects::object::ContentHash;
use thiserror::Error;

#[path = "worktree_index_storage.rs"]
mod worktree_index_storage;

use self::worktree_index_storage::{self as storage, JournalOp};

/// Magic bytes for the index file format.
pub(crate) const INDEX_MAGIC: &[u8; 8] = b"HDLEIDX\x00";
pub(crate) const JOURNAL_MAGIC: &[u8; 8] = b"HDLEJNL\x00";
pub(crate) const JOURNAL_VERSION: u32 = 1;
pub(crate) const MAX_JOURNAL_OPS_BEFORE_COMPACT: usize = 256;
pub(crate) const MAX_JOURNAL_BYTES_BEFORE_COMPACT: u64 = 256 * 1024;
pub(crate) const MAX_JOURNAL_REPLAY_MS_BEFORE_COMPACT: u128 = 16;

/// Current format version.
pub(crate) const INDEX_VERSION: u32 = 5;

/// Size of the v4 fixed header (magic + version + file_count + dir_count).
pub(crate) const HEADER_SIZE_V4: usize = 8 + 4 + 4 + 4;

/// Size of the v5 fixed header (magic + version + file_count + dir_count + untracked_count).
pub(crate) const HEADER_SIZE_V5: usize = HEADER_SIZE_V4 + 4;

/// Index errors.
#[derive(Debug, Error)]
pub enum IndexError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid index file: {0}")]
    InvalidFormat(String),

    #[error("version mismatch: expected {expected}, got {got}")]
    VersionMismatch { expected: u32, got: u32 },

    #[error("checksum mismatch")]
    ChecksumMismatch,

    #[error("invalid UTF-8 in path: {0}")]
    InvalidUtf8(String),
}

#[derive(Debug, Clone, Default)]
pub struct WorktreeIndexLoadStats {
    pub snapshot_bytes: u64,
    pub snapshot_load_ms: u128,
    pub journal_bytes: u64,
    pub journal_replay_ms: u128,
    pub journal_ops: usize,
}

#[derive(Debug, Clone, Default)]
pub struct WorktreeIndexSaveStats {
    pub snapshot_bytes: u64,
    pub snapshot_write_ms: u128,
    pub journal_bytes: u64,
    pub journal_append_ms: u128,
    pub journal_ops: usize,
    pub compacted: bool,
    pub compact_reason: Option<&'static str>,
}

/// Cached entry kind (mirrors CachedEntryKind for the index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexEntryKind {
    File,
    Symlink,
}

impl IndexEntryKind {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::File,
            1 => Self::Symlink,
            _ => Self::File,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Self::File => 0,
            Self::Symlink => 1,
        }
    }
}

/// A directory cache entry for skipping unchanged directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryCacheEntry {
    /// Directory mtime (for invalidation).
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    /// Child count (for quick invalidation check).
    pub child_count: u32,
    /// Stable digest of the sorted child-name list.
    pub child_digest: ContentHash,
    /// Tree hash for a subtree last confirmed clean against this directory state.
    pub clean_tree_hash: Option<ContentHash>,
}

/// A cached pure-untracked subtree summary for cross-run reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrackedDirectoryCacheEntry {
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub child_count: u32,
    pub child_digest: ContentHash,
    pub ignore_fingerprint: ContentHash,
    /// Added file paths relative to the cached directory root.
    pub added_paths: Vec<String>,
}

impl UntrackedDirectoryCacheEntry {
    pub fn from_relative_added_paths<'a>(
        metadata: &fs::Metadata,
        child_names: impl Iterator<Item = &'a str>,
        child_count: usize,
        ignore_fingerprint: ContentHash,
        added_paths: Vec<String>,
    ) -> Option<Self> {
        let (mtime_sec, mtime_nsec) = metadata_mtime_parts(metadata)?;
        let child_count = u32::try_from(child_count).ok()?;
        Some(Self {
            mtime_sec,
            mtime_nsec,
            child_count,
            child_digest: DirectoryCacheEntry::digest_for_child_names(
                child_names,
                child_count as usize,
            )?,
            ignore_fingerprint,
            added_paths,
        })
    }

    pub fn matches_current_directory<'a>(
        &self,
        metadata: &fs::Metadata,
        child_names: impl Iterator<Item = &'a str>,
        child_count: usize,
        ignore_fingerprint: ContentHash,
    ) -> bool {
        let Some((mtime_sec, mtime_nsec)) = metadata_mtime_parts(metadata) else {
            return false;
        };
        let Ok(child_count) = u32::try_from(child_count) else {
            return false;
        };
        let Some(child_digest) =
            DirectoryCacheEntry::digest_for_child_names(child_names, child_count as usize)
        else {
            return false;
        };

        self.mtime_sec == mtime_sec
            && self.mtime_nsec == mtime_nsec
            && self.child_count == child_count
            && self.child_digest == child_digest
            && self.ignore_fingerprint == ignore_fingerprint
    }
}

impl DirectoryCacheEntry {
    pub fn digest_for_child_names<'a>(
        child_names: impl Iterator<Item = &'a str>,
        child_count: usize,
    ) -> Option<ContentHash> {
        let child_count = u32::try_from(child_count).ok()?;
        Some(digest_child_names(child_names, child_count))
    }

    pub fn from_child_names<'a>(
        metadata: &fs::Metadata,
        child_names: impl Iterator<Item = &'a str>,
        child_count: usize,
        clean_tree_hash: Option<ContentHash>,
    ) -> Option<Self> {
        let (mtime_sec, mtime_nsec) = metadata_mtime_parts(metadata)?;
        let child_count = u32::try_from(child_count).ok()?;
        Some(Self {
            mtime_sec,
            mtime_nsec,
            child_count,
            child_digest: Self::digest_for_child_names(child_names, child_count as usize)?,
            clean_tree_hash,
        })
    }

    /// Check if we can skip descending into a directory based on cache.
    pub fn is_fresh(&self, metadata: &fs::Metadata) -> bool {
        metadata_mtime_parts(metadata).is_some_and(|(modified_sec, modified_nsec)| {
            self.mtime_sec == modified_sec && self.mtime_nsec == modified_nsec
        })
    }

    pub fn child_names_match<'a>(
        &self,
        current_children: impl Iterator<Item = &'a str>,
        child_count: usize,
    ) -> bool {
        let Ok(child_count) = u32::try_from(child_count) else {
            return false;
        };
        if self.child_count != child_count {
            return false;
        }

        self.child_digest == digest_child_names(current_children, child_count)
    }
}

/// A cached worktree entry in the binary index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub hash: ContentHash,
    pub size: u64,
    pub modified_sec: i64,
    pub modified_nsec: u32,
    pub executable: bool,
    pub kind: IndexEntryKind,
}

/// The worktree index - a binary cache for file metadata.
#[derive(Debug, Clone, Default)]
pub struct WorktreeIndex {
    entries: BTreeMap<String, IndexEntry>,
    directories: BTreeMap<String, DirectoryCacheEntry>,
    untracked_directories: BTreeMap<String, UntrackedDirectoryCacheEntry>,
    dirty: bool,
    pending_ops: Vec<JournalOp>,
    last_journal_bytes: u64,
    last_journal_ops: usize,
    last_journal_replay_ms: u128,
}

impl WorktreeIndex {
    /// Create a new empty index.
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            directories: BTreeMap::new(),
            untracked_directories: BTreeMap::new(),
            dirty: false,
            pending_ops: Vec::new(),
            last_journal_bytes: 0,
            last_journal_ops: 0,
            last_journal_replay_ms: 0,
        }
    }

    /// Load an index from a binary file.
    pub fn load(path: &Path) -> Result<Self, IndexError> {
        storage::load_profiled(path).map(|(index, _)| index)
    }

    pub fn load_profiled(path: &Path) -> Result<(Self, WorktreeIndexLoadStats), IndexError> {
        storage::load_profiled(path)
    }

    /// Save the index to a binary file.
    pub fn save(&self, path: &Path) -> Result<(), IndexError> {
        storage::save_profiled(self, path).map(|_| ())
    }

    pub fn save_profiled(&self, path: &Path) -> Result<WorktreeIndexSaveStats, IndexError> {
        storage::save_profiled(self, path)
    }

    pub(crate) fn save_snapshot_profiled(
        &self,
        path: &Path,
    ) -> Result<WorktreeIndexSaveStats, IndexError> {
        storage::save_snapshot_profiled(self, path)
    }

    /// Load an index and return its human-readable dump.
    pub fn dump_from_path(path: &Path) -> Result<String, IndexError> {
        let index = storage::load(path)?;
        Ok(index.dump())
    }

    /// Get an entry by path.
    pub fn get(&self, path: &str) -> Option<&IndexEntry> {
        self.entries.get(path)
    }

    /// Insert or update an entry.
    pub fn insert(&mut self, path: String, entry: IndexEntry) {
        let changed = self.entries.get(&path) != Some(&entry);
        if changed {
            self.entries.insert(path.clone(), entry.clone());
            self.pending_ops.push(JournalOp::UpsertFile { path, entry });
            self.dirty = true;
        }
    }

    pub(crate) fn insert_seeded(&mut self, path: String, entry: IndexEntry) {
        self.entries.insert(path, entry);
    }

    /// Remove an entry by path.
    pub fn remove(&mut self, path: &str) -> Option<IndexEntry> {
        let removed = self.entries.remove(path);
        if removed.is_some() {
            self.pending_ops.push(JournalOp::RemoveFile {
                path: path.to_string(),
            });
            self.dirty = true;
        }
        removed
    }

    /// Remove descendants of a path and any conflicting directory entry.
    pub fn remove_descendants(&mut self, path: &str) {
        let _ = self.remove_matching_paths(path, false);
    }

    /// Remove a path plus all descendants from both file and directory caches.
    pub fn remove_path_and_descendants(&mut self, path: &str) {
        let _ = self.remove_matching_paths(path, true);
    }

    /// Check if a cached entry is still fresh compared to filesystem metadata.
    pub fn is_fresh(&self, path: &str, metadata: &fs::Metadata) -> bool {
        let entry = match self.entries.get(path) {
            Some(e) => e,
            None => return false,
        };

        let modified = match metadata.modified() {
            Ok(m) => m,
            Err(_) => return false,
        };

        let duration = match modified.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d,
            Err(_) => return false,
        };

        let modified_sec = match i64::try_from(duration.as_secs()) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let modified_nsec = duration.subsec_nanos();

        let size = metadata.len();

        let kind = if metadata.is_symlink() {
            IndexEntryKind::Symlink
        } else {
            IndexEntryKind::File
        };

        let executable_matches = match kind {
            IndexEntryKind::File => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    entry.executable == (metadata.permissions().mode() & 0o111 != 0)
                }
                #[cfg(not(unix))]
                {
                    !entry.executable
                }
            }
            IndexEntryKind::Symlink => !entry.executable,
        };

        entry.size == size
            && entry.modified_sec == modified_sec
            && entry.modified_nsec == modified_nsec
            && executable_matches
            && entry.kind == kind
    }

    /// Get a directory entry by path.
    pub fn get_directory(&self, path: &str) -> Option<&DirectoryCacheEntry> {
        self.directories.get(path)
    }

    pub fn get_untracked_directory(&self, path: &str) -> Option<&UntrackedDirectoryCacheEntry> {
        self.untracked_directories.get(path)
    }

    /// Insert or update a directory entry.
    pub fn insert_directory(&mut self, path: String, entry: DirectoryCacheEntry) {
        let changed = self.directories.get(&path) != Some(&entry);
        if changed {
            self.directories.insert(path.clone(), entry.clone());
            self.pending_ops
                .push(JournalOp::UpsertDirectory { path, entry });
            self.dirty = true;
        }
    }

    pub(crate) fn insert_seeded_directory(&mut self, path: String, entry: DirectoryCacheEntry) {
        self.directories.insert(path, entry);
    }

    /// Remove a directory entry by path.
    pub fn remove_directory(&mut self, path: &str) -> Option<DirectoryCacheEntry> {
        let removed = self.directories.remove(path);
        if removed.is_some() {
            self.pending_ops.push(JournalOp::RemoveDirectory {
                path: path.to_string(),
            });
            self.dirty = true;
        }
        removed
    }

    pub fn insert_untracked_directory(
        &mut self,
        path: String,
        entry: UntrackedDirectoryCacheEntry,
    ) {
        let changed = self.untracked_directories.get(&path) != Some(&entry);
        if changed {
            self.untracked_directories
                .insert(path.clone(), entry.clone());
            self.pending_ops
                .push(JournalOp::UpsertUntrackedDirectory { path, entry });
            self.dirty = true;
        }
    }

    pub fn remove_untracked_directory(
        &mut self,
        path: &str,
    ) -> Option<UntrackedDirectoryCacheEntry> {
        let removed = self.untracked_directories.remove(path);
        if removed.is_some() {
            self.pending_ops.push(JournalOp::RemoveUntrackedDirectory {
                path: path.to_string(),
            });
            self.dirty = true;
        }
        removed
    }

    pub fn remove_untracked_directory_descendants(&mut self, path: &str) -> bool {
        let mut removed_any = false;
        for key in remove_directory_range(&mut self.untracked_directories, path) {
            self.pending_ops
                .push(JournalOp::RemoveUntrackedDirectory { path: key });
            removed_any = true;
        }
        if removed_any {
            self.dirty = true;
        }
        removed_any
    }

    /// Returns true if index contents changed since load/new.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Marks the current contents as persisted.
    pub fn mark_clean(&mut self) {
        self.dirty = false;
        self.pending_ops.clear();
    }

    pub(crate) fn set_last_load_stats(&mut self, stats: &WorktreeIndexLoadStats) {
        self.last_journal_bytes = stats.journal_bytes;
        self.last_journal_ops = stats.journal_ops;
        self.last_journal_replay_ms = stats.journal_replay_ms;
    }

    pub(crate) fn last_journal_bytes(&self) -> u64 {
        self.last_journal_bytes
    }

    pub(crate) fn last_journal_ops(&self) -> usize {
        self.last_journal_ops
    }

    pub(crate) fn last_journal_replay_ms(&self) -> u128 {
        self.last_journal_replay_ms
    }

    fn remove_matching_paths(&mut self, path: &str, remove_file_at_path: bool) -> bool {
        let descendant_start = descendant_range_start(path);
        let mut removed_any = false;

        if remove_file_at_path && self.entries.remove(path).is_some() {
            self.pending_ops.push(JournalOp::RemoveFile {
                path: path.to_string(),
            });
            removed_any = true;
        }

        for key in remove_prefixed_range(&mut self.entries, &descendant_start) {
            self.pending_ops.push(JournalOp::RemoveFile { path: key });
            removed_any = true;
        }

        for key in remove_directory_range(&mut self.directories, path) {
            self.pending_ops
                .push(JournalOp::RemoveDirectory { path: key });
            removed_any = true;
        }

        for key in remove_directory_range(&mut self.untracked_directories, path) {
            self.pending_ops
                .push(JournalOp::RemoveUntrackedDirectory { path: key });
            removed_any = true;
        }

        if removed_any {
            self.dirty = true;
        }

        removed_any
    }

    /// Check if we can skip descending into a directory based on cache.
    ///
    /// Returns true if the directory hasn't changed and can be safely skipped.
    pub fn can_skip_directory(
        &self,
        dir_path: &str,
        metadata: &fs::Metadata,
        children: &[String],
        tree_hash: &ContentHash,
    ) -> bool {
        let dir_entry = match self.directories.get(dir_path) {
            Some(e) => e,
            None => return false,
        };

        // Check if mtime matches
        if !dir_entry.is_fresh(metadata) {
            return false;
        }

        dir_entry.child_names_match(children.iter().map(String::as_str), children.len())
            && dir_entry.clean_tree_hash.as_ref() == Some(tree_hash)
    }

    pub fn can_skip_directory_names<'a>(
        &self,
        dir_path: &str,
        metadata: &fs::Metadata,
        children: impl Iterator<Item = &'a str>,
        child_count: usize,
        tree_hash: &ContentHash,
    ) -> bool {
        let dir_entry = match self.directories.get(dir_path) {
            Some(e) => e,
            None => return false,
        };

        if !dir_entry.is_fresh(metadata) {
            return false;
        }

        dir_entry.child_names_match(children, child_count)
            && dir_entry.clean_tree_hash.as_ref() == Some(tree_hash)
    }

    /// Check if children match (for more precise caching).
    pub fn directory_children_match(&self, dir_path: &str, children: &[String]) -> bool {
        let dir_entry = match self.directories.get(dir_path) {
            Some(e) => e,
            None => return false,
        };

        dir_entry.child_names_match(children.iter().map(String::as_str), children.len())
    }

    /// Get the number of directory entries.
    pub fn directory_len(&self) -> usize {
        self.directories.len()
    }

    /// Get the number of cached pure-untracked directory summaries.
    pub fn untracked_directory_len(&self) -> usize {
        self.untracked_directories.len()
    }

    /// Iterate over cached directory entries.
    pub fn directory_iter(&self) -> impl Iterator<Item = (&str, &DirectoryCacheEntry)> {
        self.directories.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn pending_op_count(&self) -> usize {
        self.pending_ops.len()
    }

    /// Get the number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate over entries.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &IndexEntry)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Produce a human-readable debug dump.
    pub fn dump(&self) -> String {
        if self.entries.is_empty()
            && self.directories.is_empty()
            && self.untracked_directories.is_empty()
        {
            return "WorktreeIndex (empty)".to_string();
        }

        let mut lines = vec![format!(
            "WorktreeIndex ({} files, {} directories, {} untracked directories):",
            self.entries.len(),
            self.directories.len(),
            self.untracked_directories.len()
        )];
        lines.push(String::new());

        // Sort for deterministic output
        let mut entries: Vec<_> = self.entries.iter().collect();
        entries.sort_by_key(|(left, _)| *left);

        // Dump file entries
        for (path, entry) in entries {
            let kind_str = match entry.kind {
                IndexEntryKind::File => "file",
                IndexEntryKind::Symlink => "symlink",
            };
            let exe_str = if entry.executable {
                " [executable]"
            } else {
                ""
            };
            lines.push(format!(
                "  {} {} {} size={}{}",
                entry.hash.short(),
                kind_str,
                path,
                entry.size,
                exe_str
            ));
        }

        // Dump directory entries
        if !self.directories.is_empty() {
            lines.push(String::new());
            lines.push("Directories:".to_string());

            let mut dirs: Vec<_> = self.directories.iter().collect();
            dirs.sort_by_key(|(left, _)| *left);

            for (path, dir) in dirs {
                lines.push(format!(
                    "    {} children digest={} path={} mtime={}.{:09}",
                    dir.child_count,
                    dir.child_digest.short(),
                    path,
                    dir.mtime_sec,
                    dir.mtime_nsec
                ));
            }
        }

        if !self.untracked_directories.is_empty() {
            lines.push(String::new());
            lines.push("Untracked Directories:".to_string());

            let mut dirs: Vec<_> = self.untracked_directories.iter().collect();
            dirs.sort_by_key(|(left, _)| *left);

            for (path, dir) in dirs {
                lines.push(format!(
                    "    {} children digest={} added={} path={} mtime={}.{:09}",
                    dir.child_count,
                    dir.child_digest.short(),
                    dir.added_paths.len(),
                    path,
                    dir.mtime_sec,
                    dir.mtime_nsec
                ));
            }
        }

        lines.join("\n")
    }
}

fn is_descendant_path(candidate: &str, path: &str) -> bool {
    candidate
        .strip_prefix(path)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn descendant_range_start(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }

    let mut prefix = String::with_capacity(path.len() + 1);
    prefix.push_str(path);
    prefix.push('/');
    prefix
}

fn remove_prefixed_range<T>(entries: &mut BTreeMap<String, T>, prefix: &str) -> Vec<String> {
    let keys: Vec<String> = entries
        .range(prefix.to_string()..)
        .map(|(candidate, _)| candidate)
        .take_while(|candidate| candidate.starts_with(prefix))
        .cloned()
        .collect();

    for key in &keys {
        let _ = entries.remove(key);
    }

    keys
}

fn remove_directory_range<T>(entries: &mut BTreeMap<String, T>, path: &str) -> Vec<String> {
    if path.is_empty() {
        let keys = entries.keys().cloned().collect::<Vec<_>>();
        entries.clear();
        return keys;
    }

    let keys: Vec<String> = entries
        .range(path.to_string()..)
        .map(|(candidate, _)| candidate)
        .take_while(|candidate| *candidate == path || is_descendant_path(candidate, path))
        .cloned()
        .collect();

    for key in &keys {
        let _ = entries.remove(key);
    }

    keys
}

fn metadata_mtime_parts(metadata: &fs::Metadata) -> Option<(i64, u32)> {
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((
        i64::try_from(duration.as_secs()).ok()?,
        duration.subsec_nanos(),
    ))
}

fn digest_child_names<'a>(
    child_names: impl Iterator<Item = &'a str>,
    child_count: u32,
) -> ContentHash {
    let mut payload = Vec::new();
    payload.extend_from_slice(&child_count.to_be_bytes());
    for child_name in child_names {
        payload.extend_from_slice(child_name.as_bytes());
        payload.push(0);
    }
    ContentHash::compute_typed("dirnames", &payload)
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs as unix_fs;

    use tempfile::TempDir;

    use super::*;

    fn create_sample_entry(path: &str) -> IndexEntry {
        let _ = path;
        IndexEntry {
            hash: ContentHash::compute(b"test content"),
            size: 12,
            modified_sec: 1000000,
            modified_nsec: 0,
            executable: false,
            kind: IndexEntryKind::File,
        }
    }

    fn create_sample_directory_entry(children: &[&str]) -> DirectoryCacheEntry {
        DirectoryCacheEntry {
            mtime_sec: 1,
            mtime_nsec: 0,
            child_count: children.len() as u32,
            child_digest: DirectoryCacheEntry::digest_for_child_names(
                children.iter().copied(),
                children.len(),
            )
            .unwrap(),
            clean_tree_hash: None,
        }
    }

    fn create_sample_untracked_directory_entry(
        children: &[&str],
        added_paths: &[&str],
    ) -> UntrackedDirectoryCacheEntry {
        UntrackedDirectoryCacheEntry {
            mtime_sec: 2,
            mtime_nsec: 0,
            child_count: children.len() as u32,
            child_digest: DirectoryCacheEntry::digest_for_child_names(
                children.iter().copied(),
                children.len(),
            )
            .unwrap(),
            ignore_fingerprint: ContentHash::compute_typed("heddle.ignore", b"sample"),
            added_paths: added_paths.iter().map(|path| (*path).to_string()).collect(),
        }
    }

    #[test]
    fn test_empty_index_when_missing() {
        let temp = TempDir::new().unwrap();
        let index = WorktreeIndex::load(&temp.path().join("index.bin")).unwrap();

        assert!(index.is_empty());
    }

    #[test]
    fn test_save_and_load() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("index.bin");

        let mut index = WorktreeIndex::new();
        index.insert(
            "src/main.rs".to_string(),
            create_sample_entry("src/main.rs"),
        );
        index.insert("src/lib.rs".to_string(), create_sample_entry("src/lib.rs"));
        index.insert_directory(
            "src".to_string(),
            create_sample_directory_entry(&["lib.rs", "main.rs"]),
        );
        index.insert_untracked_directory(
            "scratch".to_string(),
            create_sample_untracked_directory_entry(&["nested"], &["nested/file.txt"]),
        );
        index.save(&path).unwrap();

        let loaded = WorktreeIndex::load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.directory_len(), 1);
        assert!(loaded.get("src/main.rs").is_some());
        assert!(loaded.get("src/lib.rs").is_some());
        assert_eq!(
            loaded.get_directory("src").map(|entry| entry.child_count),
            Some(2)
        );
        assert_eq!(
            loaded
                .get_untracked_directory("scratch")
                .map(|entry| entry.added_paths.clone()),
            Some(vec!["nested/file.txt".to_string()])
        );
    }

    #[test]
    fn test_save_appends_journal_and_load_replays_it() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("index.bin");
        let journal_path = path.with_extension("journal");

        let mut index = WorktreeIndex::new();
        index.insert(
            "src/main.rs".to_string(),
            create_sample_entry("src/main.rs"),
        );
        index.save(&path).unwrap();
        index.mark_clean();
        assert!(path.exists());
        assert!(!journal_path.exists());

        index.insert("src/lib.rs".to_string(), create_sample_entry("src/lib.rs"));
        index.insert_untracked_directory(
            "scratch".to_string(),
            create_sample_untracked_directory_entry(&["nested"], &["nested/file.txt"]),
        );
        index.save(&path).unwrap();
        index.mark_clean();

        assert!(journal_path.exists());

        let loaded = WorktreeIndex::load(&path).unwrap();
        assert!(loaded.get("src/main.rs").is_some());
        assert!(loaded.get("src/lib.rs").is_some());
        assert_eq!(
            loaded
                .get_untracked_directory("scratch")
                .map(|entry| entry.added_paths.len()),
            Some(1)
        );
        assert_eq!(loaded.pending_op_count(), 0);
    }

    #[test]
    fn test_save_compacts_when_previous_replay_cost_is_high() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("index.bin");
        let journal_path = path.with_extension("journal");

        let mut index = WorktreeIndex::new();
        index.insert(
            "src/main.rs".to_string(),
            create_sample_entry("src/main.rs"),
        );
        index.save(&path).unwrap();
        index.mark_clean();

        index.insert("src/lib.rs".to_string(), create_sample_entry("src/lib.rs"));
        index.save(&path).unwrap();
        index.mark_clean();
        assert!(journal_path.exists());

        let (mut loaded, _) = WorktreeIndex::load_profiled(&path).unwrap();
        loaded.last_journal_replay_ms = MAX_JOURNAL_REPLAY_MS_BEFORE_COMPACT + 1;
        loaded.insert("src/bin.rs".to_string(), create_sample_entry("src/bin.rs"));

        let save_stats = loaded.save_profiled(&path).unwrap();
        assert!(save_stats.compacted);
        assert_eq!(save_stats.compact_reason, Some("replay_ms"));
        assert!(!journal_path.exists());

        let reloaded = WorktreeIndex::load(&path).unwrap();
        assert!(reloaded.get("src/main.rs").is_some());
        assert!(reloaded.get("src/lib.rs").is_some());
        assert!(reloaded.get("src/bin.rs").is_some());
    }

    #[test]
    fn test_load_ignores_truncated_journal_tail() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("index.bin");
        let journal_path = path.with_extension("journal");

        let mut index = WorktreeIndex::new();
        index.insert(
            "src/main.rs".to_string(),
            create_sample_entry("src/main.rs"),
        );
        index.save(&path).unwrap();
        index.mark_clean();

        index.insert("src/lib.rs".to_string(), create_sample_entry("src/lib.rs"));
        index.save(&path).unwrap();
        index.mark_clean();

        let mut journal = fs::read(&journal_path).unwrap();
        journal.pop();
        fs::write(&journal_path, journal).unwrap();

        let loaded = WorktreeIndex::load(&path).unwrap();
        assert!(loaded.get("src/main.rs").is_some());
        assert!(loaded.get("src/lib.rs").is_none());
    }

    #[test]
    fn test_insert_and_get() {
        let mut index = WorktreeIndex::new();
        index.insert("test.txt".to_string(), create_sample_entry("test.txt"));

        let entry = index.get("test.txt").expect("should exist");
        assert_eq!(entry.size, 12);
    }

    #[test]
    fn test_remove() {
        let mut index = WorktreeIndex::new();
        index.insert("test.txt".to_string(), create_sample_entry("test.txt"));
        assert!(index.get("test.txt").is_some());

        let removed = index.remove("test.txt");
        assert!(removed.is_some());
        assert!(index.get("test.txt").is_none());
    }

    #[test]
    fn test_dump_format() {
        let mut index = WorktreeIndex::new();
        index.insert(
            "src/main.rs".to_string(),
            create_sample_entry("src/main.rs"),
        );

        let dump = index.dump();
        assert!(dump.contains("src/main.rs"));
        assert!(dump.contains("WorktreeIndex"));
    }

    #[test]
    fn test_dump_from_path_uses_storage_seam() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("index.bin");

        let mut index = WorktreeIndex::new();
        index.insert(
            "src/main.rs".to_string(),
            create_sample_entry("src/main.rs"),
        );
        index.save(&path).unwrap();
        index.mark_clean();

        let dump = WorktreeIndex::dump_from_path(&path).unwrap();
        assert!(dump.contains("src/main.rs"));
        assert!(dump.contains("WorktreeIndex"));
    }

    #[test]
    fn test_dirty_tracking_only_marks_real_changes() {
        let mut index = WorktreeIndex::new();
        let entry = create_sample_entry("src/main.rs");

        assert!(!index.is_dirty());

        index.insert("src/main.rs".to_string(), entry.clone());
        assert!(index.is_dirty());

        index.mark_clean();
        assert!(!index.is_dirty());

        index.insert("src/main.rs".to_string(), entry.clone());
        assert!(!index.is_dirty());

        index.insert_directory(
            "src".to_string(),
            DirectoryCacheEntry {
                mtime_sec: 1,
                mtime_nsec: 2,
                ..create_sample_directory_entry(&["main.rs"])
            },
        );
        assert!(index.is_dirty());
    }

    #[test]
    fn test_remove_descendants_keeps_exact_file_path() {
        let mut index = WorktreeIndex::new();
        index.insert("src".to_string(), create_sample_entry("src"));
        index.insert(
            "src/main.rs".to_string(),
            create_sample_entry("src/main.rs"),
        );
        index.insert_directory(
            "src".to_string(),
            create_sample_directory_entry(&["main.rs"]),
        );
        index.mark_clean();

        index.remove_descendants("src");

        assert!(index.get("src").is_some());
        assert!(index.get("src/main.rs").is_none());
        assert!(index.get_directory("src").is_none());
        assert!(index.is_dirty());
    }

    #[test]
    fn test_remove_path_and_descendants_removes_exact_file_path() {
        let mut index = WorktreeIndex::new();
        index.insert("src".to_string(), create_sample_entry("src"));
        index.insert(
            "src/main.rs".to_string(),
            create_sample_entry("src/main.rs"),
        );
        index.insert_directory(
            "src".to_string(),
            create_sample_directory_entry(&["main.rs"]),
        );
        index.mark_clean();

        index.remove_path_and_descendants("src");

        assert!(index.get("src").is_none());
        assert!(index.get("src/main.rs").is_none());
        assert!(index.get_directory("src").is_none());
        assert!(index.is_dirty());
    }

    #[test]
    fn test_remove_descendants_does_not_remove_similar_prefixes() {
        let mut index = WorktreeIndex::new();
        index.insert(
            "src/main.rs".to_string(),
            create_sample_entry("src/main.rs"),
        );
        index.insert(
            "src2/main.rs".to_string(),
            create_sample_entry("src2/main.rs"),
        );
        index.insert_directory(
            "src".to_string(),
            create_sample_directory_entry(&["main.rs"]),
        );
        index.mark_clean();

        index.remove_descendants("src");

        assert!(index.get("src/main.rs").is_none());
        assert!(index.get("src2/main.rs").is_some());
        assert!(index.get_directory("src").is_none());
        assert!(index.is_dirty());
    }

    #[test]
    fn test_remove_descendants_noop_does_not_mark_dirty() {
        let mut index = WorktreeIndex::new();
        index.insert(
            "src/main.rs".to_string(),
            create_sample_entry("src/main.rs"),
        );
        index.mark_clean();

        index.remove_descendants("missing");

        assert!(!index.is_dirty());
        assert!(index.get("src/main.rs").is_some());
    }

    #[test]
    fn test_remove_path_and_descendants_from_root_removes_all_entries() {
        let mut index = WorktreeIndex::new();
        index.insert(
            "src/main.rs".to_string(),
            create_sample_entry("src/main.rs"),
        );
        index.insert_directory(String::new(), create_sample_directory_entry(&["src"]));
        index.insert_directory(
            "src".to_string(),
            create_sample_directory_entry(&["main.rs"]),
        );
        index.mark_clean();

        index.remove_path_and_descendants("");

        assert!(index.is_empty());
        assert_eq!(index.directory_len(), 0);
        assert!(index.is_dirty());
    }

    #[test]
    fn test_corrupted_checksum() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("index.bin");

        // Write a valid-looking v2 file with wrong checksum
        let mut data = Vec::new();
        data.extend_from_slice(INDEX_MAGIC);
        data.extend_from_slice(&INDEX_VERSION.to_be_bytes());
        data.extend_from_slice(&1u32.to_be_bytes()); // 1 file entry
        data.extend_from_slice(&0u32.to_be_bytes()); // 0 directory entries

        // File entry: type(1) + path_len(4) + path + hash(32) + size(8) + modified_sec(8) + modified_nsec(4) + executable(1) + kind(1)
        data.push(0x01); // file entry type
        data.extend_from_slice(&4u32.to_be_bytes()); // path_len
        data.extend_from_slice(b"test"); // path
        data.extend_from_slice(ContentHash::compute(b"test").as_bytes()); // hash
        data.extend_from_slice(&12u64.to_be_bytes()); // size
        data.extend_from_slice(&1000i64.to_be_bytes()); // modified_sec
        data.extend_from_slice(&0u32.to_be_bytes()); // modified_nsec
        data.push(0u8); // not executable
        data.push(0u8); // file kind

        // Wrong checksum
        data.extend_from_slice(&12345u32.to_be_bytes());

        std::fs::write(&path, &data).unwrap();

        let result = WorktreeIndex::load(&path);
        assert!(result.is_err());
    }

    #[test]
    #[cfg(unix)]
    fn test_symlink_entry_is_fresh_without_executable_check() {
        let temp = TempDir::new().unwrap();
        let target_path = temp.path().join("target.txt");
        fs::write(&target_path, "target").unwrap();

        let link_path = temp.path().join("link.txt");
        unix_fs::symlink("target.txt", &link_path).unwrap();

        let metadata = fs::symlink_metadata(&link_path).unwrap();
        let modified = metadata.modified().unwrap();
        let duration = modified.duration_since(std::time::UNIX_EPOCH).unwrap();

        let mut index = WorktreeIndex::new();
        index.insert(
            "link.txt".to_string(),
            IndexEntry {
                hash: ContentHash::compute(b"target.txt"),
                size: metadata.len(),
                modified_sec: i64::try_from(duration.as_secs()).unwrap(),
                modified_nsec: duration.subsec_nanos(),
                executable: false,
                kind: IndexEntryKind::Symlink,
            },
        );

        assert!(index.is_fresh("link.txt", &metadata));
    }
}