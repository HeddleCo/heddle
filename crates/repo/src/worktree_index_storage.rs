// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    path::Path,
    time::Instant,
};

use objects::{
    fs_atomic::{sync_directory, sync_file, sync_file_data},
    object::ContentHash,
};
use tracing::{debug, warn};

use super::{
    DirectoryCacheEntry, GitlinkSummary, HEADER_SIZE_V4, HEADER_SIZE_V5, HEADER_SIZE_V6,
    INDEX_MAGIC, INDEX_VERSION, IndexEntry, IndexEntryKind, IndexError,
    MAX_JOURNAL_REPLAY_MS_BEFORE_COMPACT, UntrackedDirectoryCacheEntry, WorktreeIndex,
    WorktreeIndexLoadStats, WorktreeIndexSaveStats,
};

const JOURNAL_MAGIC: &[u8; 8] = super::JOURNAL_MAGIC;
const JOURNAL_VERSION: u32 = super::JOURNAL_VERSION;
const MAX_JOURNAL_OPS_BEFORE_COMPACT: usize = super::MAX_JOURNAL_OPS_BEFORE_COMPACT;
const MAX_JOURNAL_BYTES_BEFORE_COMPACT: u64 = super::MAX_JOURNAL_BYTES_BEFORE_COMPACT;
const HOT_RECORD_MAGIC: &[u8; 8] = b"HDLEHOT\0";
const HOT_RECORD_VERSION: u32 = 1;

fn read_u32_be(bytes: &[u8], context: &str) -> Result<u32, IndexError> {
    let array = read_be_at::<4>(bytes, context)?;
    Ok(u32::from_be_bytes(array))
}

fn read_u64_be(bytes: &[u8], context: &str) -> Result<u64, IndexError> {
    let array = read_be_at::<8>(bytes, context)?;
    Ok(u64::from_be_bytes(array))
}

fn read_i64_be(bytes: &[u8], context: &str) -> Result<i64, IndexError> {
    let array = read_be_at::<8>(bytes, context)?;
    Ok(i64::from_be_bytes(array))
}

fn read_be_at<const N: usize>(bytes: &[u8], context: &str) -> Result<[u8; N], IndexError> {
    if bytes.len() < N {
        return Err(IndexError::InvalidFormat(format!("truncated {context}")));
    }
    let mut array = [0u8; N];
    array.copy_from_slice(&bytes[..N]);
    Ok(array)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexEntryType {
    File = 0x01,
    Directory = 0x02,
    UntrackedDirectory = 0x03,
    Gitlink = 0x04,
}

impl IndexEntryType {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(Self::File),
            0x02 => Some(Self::Directory),
            0x03 => Some(Self::UntrackedDirectory),
            0x04 => Some(Self::Gitlink),
            _ => None,
        }
    }

    fn to_u8(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JournalOp {
    UpsertFile {
        path: String,
        entry: IndexEntry,
    },
    RemoveFile {
        path: String,
    },
    UpsertDirectory {
        path: String,
        entry: DirectoryCacheEntry,
    },
    RemoveDirectory {
        path: String,
    },
    UpsertUntrackedDirectory {
        path: String,
        entry: UntrackedDirectoryCacheEntry,
    },
    RemoveUntrackedDirectory {
        path: String,
    },
    UpsertGitlink {
        path: String,
        target: String,
    },
    RemoveGitlink {
        path: String,
    },
}

pub(crate) fn load(path: &Path) -> Result<WorktreeIndex, IndexError> {
    load_profiled(path).map(|(index, _)| index)
}

pub(crate) fn load_profiled(
    path: &Path,
) -> Result<(WorktreeIndex, WorktreeIndexLoadStats), IndexError> {
    load_profiled_inner(path, false)
}

pub(crate) fn load_hot_profiled_for_directories(
    path: &Path,
    directory_keys: &BTreeSet<String>,
) -> Result<(WorktreeIndex, WorktreeIndexLoadStats), IndexError> {
    let mut stats = WorktreeIndexLoadStats::default();
    if !path.exists() {
        return Ok((WorktreeIndex::new(), stats));
    }
    let load_start = Instant::now();
    let mut snapshot = File::open(path)?;
    let mut header = [0_u8; 12];
    snapshot.read_exact(&mut header)?;
    if &header[..8] != INDEX_MAGIC {
        return Err(IndexError::InvalidFormat("missing magic bytes".to_string()));
    }
    let version = read_u32_be(&header[8..12], "index version")?;
    if version != INDEX_VERSION {
        return Err(IndexError::VersionMismatch {
            expected: INDEX_VERSION,
            got: version,
        });
    }

    let mut index = WorktreeIndex::new();
    index.hot_loaded = true;
    for key in directory_keys {
        if let Some((directory, untracked, clean_tree, bytes)) =
            load_hot_directory_record(path, key, !key.is_empty())?
        {
            stats.snapshot_bytes = stats.snapshot_bytes.saturating_add(bytes);
            if let Some(directory) = directory {
                index.directories.insert(key.clone(), directory);
            }
            if let Some(untracked) = untracked {
                index.untracked_directories.insert(key.clone(), untracked);
            }
            if let Some(clean_tree) = clean_tree {
                index.clean_trees.insert(key.clone(), clean_tree);
            }
        }
    }

    let gitlinks_valid = load_hot_gitlinks(path, &mut index, &mut stats)?;
    if !gitlinks_valid {
        index.directories.remove("");
    }
    stats.snapshot_load_ms = load_start.elapsed().as_millis();

    let journal_path = journal_path(path);
    stats.journal_bytes = journal_path
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if directory_keys.iter().any(|key| !key.is_empty()) && journal_path.exists() {
        let replay_start = Instant::now();
        stats.journal_ops = apply_journal(&mut index, &journal_path)?;
        stats.journal_replay_ms = replay_start.elapsed().as_millis();
    }
    index.dirty = false;
    index.pending_ops.clear();
    index.set_last_load_stats(&stats);
    Ok((index, stats))
}

pub(crate) fn load_hot_gitlinks_summary(path: &Path) -> Result<Option<GitlinkSummary>, IndexError> {
    let mut index = WorktreeIndex::new();
    let mut stats = WorktreeIndexLoadStats::default();
    if !load_hot_gitlinks(path, &mut index, &mut stats)? {
        return Ok(None);
    }
    let Some(root) = index.gitlinks_tree else {
        return Ok(None);
    };
    Ok(Some((root, index.gitlinks.into_iter().collect())))
}

fn load_profiled_inner(
    path: &Path,
    hot_only: bool,
) -> Result<(WorktreeIndex, WorktreeIndexLoadStats), IndexError> {
    let mut stats = WorktreeIndexLoadStats::default();
    if !path.exists() {
        return Ok((WorktreeIndex::new(), stats));
    }

    let load_start = Instant::now();
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let file_size = metadata.len();
    stats.snapshot_bytes = file_size;

    if file_size < 12 {
        return Err(IndexError::InvalidFormat(
            "truncated index header".to_string(),
        ));
    }

    let mut header = [0u8; 12];
    file.read_exact(&mut header)?;

    if &header[..8] != INDEX_MAGIC {
        return Err(IndexError::InvalidFormat("missing magic bytes".to_string()));
    }

    let version = read_u32_be(&header[8..12], "index version")?;

    let mut index = match version {
        1 if file_size >= 16 => load_v1(&mut file, file_size),
        2 if file_size >= HEADER_SIZE_V4 as u64 + 4 => load_v2(&mut file, file_size),
        3 if file_size >= HEADER_SIZE_V4 as u64 + 4 => load_v3(&mut file, file_size),
        4 if file_size >= HEADER_SIZE_V4 as u64 + 4 => load_v4(&mut file, file_size),
        5 if !hot_only && file_size >= HEADER_SIZE_V5 as u64 + 4 => load_v5(&mut file, file_size),
        6 if file_size >= HEADER_SIZE_V6 as u64 + 4 => load_v6(&mut file, file_size, hot_only),
        v => Err(IndexError::VersionMismatch {
            expected: INDEX_VERSION,
            got: v,
        }),
    }?;
    stats.snapshot_load_ms = load_start.elapsed().as_millis();

    let journal_path = journal_path(path);
    let journal_ops = if journal_path.exists() {
        stats.journal_bytes = journal_path
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let replay_start = Instant::now();
        match apply_journal(&mut index, &journal_path) {
            Ok(op_count) => {
                stats.journal_replay_ms = replay_start.elapsed().as_millis();
                op_count
            }
            Err(error) => {
                return Err(error);
            }
        }
    } else {
        0
    };
    stats.journal_ops = journal_ops;
    index.dirty = false;
    index.pending_ops.clear();
    index.set_last_load_stats(&stats);

    debug!(
        snapshot_path = %path.display(),
        journal_path = %journal_path.display(),
        files = index.entries.len(),
        directories = index.directories.len(),
        untracked_directories = index.untracked_directories.len(),
        journal_ops,
        "Loaded worktree index"
    );

    Ok((index, stats))
}

pub(crate) fn save_profiled(
    index: &WorktreeIndex,
    path: &Path,
) -> Result<WorktreeIndexSaveStats, IndexError> {
    let mut stats = WorktreeIndexSaveStats {
        journal_ops: index.pending_ops.len(),
        ..WorktreeIndexSaveStats::default()
    };
    if index.pending_ops.is_empty() {
        return Ok(stats);
    }

    write_hot_sidecars(index, path)?;

    let journal_path = journal_path(path);
    let journal_exists = journal_path.exists();
    let journal_len = journal_path
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let compact_reason = if !path.exists() {
        Some("missing_snapshot")
    } else if !index.is_hot_loaded() && index.pending_ops.len() > MAX_JOURNAL_OPS_BEFORE_COMPACT {
        Some("pending_ops")
    } else if !index.is_hot_loaded() && journal_len > MAX_JOURNAL_BYTES_BEFORE_COMPACT {
        Some("journal_bytes")
    } else if !index.is_hot_loaded()
        && index.last_journal_replay_ms() > MAX_JOURNAL_REPLAY_MS_BEFORE_COMPACT
    {
        Some("replay_ms")
    } else {
        None
    };

    if let Some(compact_reason) = compact_reason {
        let write_start = Instant::now();
        write_snapshot(index, path)?;
        stats.snapshot_write_ms = write_start.elapsed().as_millis();
        stats.snapshot_bytes = path.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        stats.compacted = true;
        stats.compact_reason = Some(compact_reason);
        if journal_exists {
            let _ = fs::remove_file(&journal_path);
        }
        debug!(
            snapshot_path = %path.display(),
            journal_path = %journal_path.display(),
            strategy = "compact_snapshot",
            compact_reason,
            files = index.entries.len(),
            directories = index.directories.len(),
            untracked_directories = index.untracked_directories.len(),
            previous_journal_bytes = index.last_journal_bytes(),
            previous_journal_ops = index.last_journal_ops(),
            previous_journal_replay_ms = index.last_journal_replay_ms(),
            pending_ops = index.pending_ops.len(),
            "Persisted worktree index"
        );
        return Ok(stats);
    }

    let append_start = Instant::now();
    append_journal(index, &journal_path)?;
    stats.journal_append_ms = append_start.elapsed().as_millis();
    stats.journal_bytes = journal_path
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    debug!(
        snapshot_path = %path.display(),
        journal_path = %journal_path.display(),
        strategy = "append_journal",
        files = index.entries.len(),
        directories = index.directories.len(),
        untracked_directories = index.untracked_directories.len(),
        previous_journal_bytes = index.last_journal_bytes(),
        previous_journal_ops = index.last_journal_ops(),
        previous_journal_replay_ms = index.last_journal_replay_ms(),
        pending_ops = index.pending_ops.len(),
        "Persisted worktree index"
    );
    Ok(stats)
}

pub(crate) fn save_snapshot_profiled(
    index: &WorktreeIndex,
    path: &Path,
) -> Result<WorktreeIndexSaveStats, IndexError> {
    let journal_path = journal_path(path);
    let write_start = Instant::now();
    write_hot_sidecars(index, path)?;
    write_snapshot(index, path)?;
    let snapshot_write_ms = write_start.elapsed().as_millis();
    let snapshot_bytes = path.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    if journal_path.exists() {
        let _ = fs::remove_file(&journal_path);
    }
    debug!(
        snapshot_path = %path.display(),
        journal_path = %journal_path.display(),
        strategy = "snapshot_seed",
        files = index.entries.len(),
        directories = index.directories.len(),
        untracked_directories = index.untracked_directories.len(),
        "Persisted worktree index"
    );
    Ok(WorktreeIndexSaveStats {
        snapshot_bytes,
        snapshot_write_ms,
        compacted: true,
        compact_reason: Some("seeded_snapshot"),
        ..WorktreeIndexSaveStats::default()
    })
}

fn load_v1(file: &mut File, file_size: u64) -> Result<WorktreeIndex, IndexError> {
    file.seek(std::io::SeekFrom::Start(0))?;

    let mut header = [0u8; 16];
    file.read_exact(&mut header)?;

    let entry_count = read_u32_be(&header[12..16], "index entry count")?;
    let footer_size = 4u64;
    let entry_data_size = file_size.saturating_sub(16).saturating_sub(footer_size);

    if entry_data_size == 0 && entry_count > 0 {
        return Err(IndexError::InvalidFormat(
            "entry data size mismatch".to_string(),
        ));
    }

    let mut entries = BTreeMap::new();
    let mut data = vec![0u8; entry_data_size as usize];
    file.read_exact(&mut data)?;

    let mut offset = 0;
    for _ in 0..entry_count {
        if offset + 4 > data.len() {
            return Err(IndexError::InvalidFormat("truncated entry".to_string()));
        }

        let path_len = read_u32_be(&data[offset..], "legacy index path length")? as usize;
        offset += 4;

        if offset + path_len + 32 + 8 + 8 + 4 + 1 + 1 > data.len() {
            return Err(IndexError::InvalidFormat(
                "truncated entry data".to_string(),
            ));
        }

        let path = String::from_utf8(data[offset..offset + path_len].to_vec())
            .map_err(|_| IndexError::InvalidUtf8(format!("path at offset {}", offset)))?;
        offset += path_len;

        let mut hash_bytes = [0u8; 32];
        hash_bytes.copy_from_slice(&data[offset..offset + 32]);
        let hash = ContentHash::from_bytes(hash_bytes);
        offset += 32;

        let size = read_u64_be(&data[offset..], "legacy index file size")?;
        offset += 8;

        let modified_sec = read_i64_be(&data[offset..], "legacy index file mtime seconds")?;
        offset += 8;

        let modified_nsec = read_u32_be(&data[offset..], "legacy index file mtime nanos")?;
        offset += 4;

        let executable = data[offset] != 0;
        offset += 1;

        let kind = IndexEntryKind::from_u8(data[offset]);
        offset += 1;

        entries.insert(
            path.clone(),
            IndexEntry {
                hash,
                size,
                modified_sec,
                modified_nsec,
                executable,
                kind,
            },
        );
    }

    let mut checksum_bytes = [0u8; 4];
    file.read_exact(&mut checksum_bytes)?;
    let stored_checksum = u32::from_be_bytes(checksum_bytes);
    let computed_checksum = crc32(&data);

    if computed_checksum != stored_checksum {
        return Err(IndexError::ChecksumMismatch);
    }

    Ok(WorktreeIndex {
        entries,
        directories: BTreeMap::new(),
        untracked_directories: BTreeMap::new(),
        gitlinks: BTreeMap::new(),
        gitlinks_tree: None,
        clean_trees: BTreeMap::new(),
        dirty: false,
        pending_ops: Vec::new(),
        last_journal_bytes: 0,
        last_journal_ops: 0,
        last_journal_replay_ms: 0,
        hot_loaded: false,
    })
}

fn load_v2(file: &mut File, file_size: u64) -> Result<WorktreeIndex, IndexError> {
    load_legacy_versioned(file, file_size, false)
}

fn load_v3(file: &mut File, file_size: u64) -> Result<WorktreeIndex, IndexError> {
    load_legacy_versioned(file, file_size, true)
}

fn load_v4(file: &mut File, file_size: u64) -> Result<WorktreeIndex, IndexError> {
    load_compact_versioned(file, file_size)
}

fn load_v5(file: &mut File, file_size: u64) -> Result<WorktreeIndex, IndexError> {
    load_compact_versioned_with_untracked(file, file_size)
}

fn load_v6(file: &mut File, file_size: u64, hot_only: bool) -> Result<WorktreeIndex, IndexError> {
    file.seek(std::io::SeekFrom::Start(0))?;
    let mut header = [0u8; HEADER_SIZE_V6];
    file.read_exact(&mut header)?;
    let file_count = read_u32_be(&header[12..16], "index file count")?;
    let dir_count = read_u32_be(&header[16..20], "index directory count")?;
    let untracked_count = read_u32_be(&header[20..24], "index untracked directory count")?;
    let gitlink_count = read_u32_be(&header[24..28], "index gitlink count")?;
    let hot_len = read_u64_be(&header[28..36], "index hot section length")?;
    let hot_checksum = read_u32_be(&header[36..40], "index hot section checksum")?;
    let data_len = file_size
        .checked_sub(HEADER_SIZE_V6 as u64 + 4)
        .ok_or_else(|| IndexError::InvalidFormat("truncated v6 index".to_string()))?;
    if hot_len > data_len {
        return Err(IndexError::InvalidFormat(
            "hot section exceeds index data".to_string(),
        ));
    }

    let mut hot_data = vec![0_u8; hot_len as usize];
    file.read_exact(&mut hot_data)?;
    if crc32(&hot_data) != hot_checksum {
        return Err(IndexError::ChecksumMismatch);
    }
    let mut directories = BTreeMap::new();
    let mut untracked_directories = BTreeMap::new();
    let mut gitlinks = BTreeMap::new();
    let mut offset = 0;
    for _ in 0..dir_count {
        offset = expect_entry_type(&hot_data, offset, IndexEntryType::Directory)?;
        offset = read_compact_directory_entry(&hot_data, offset, &mut directories)?;
    }
    for _ in 0..untracked_count {
        offset = expect_entry_type(&hot_data, offset, IndexEntryType::UntrackedDirectory)?;
        offset = read_untracked_directory_entry(&hot_data, offset, &mut untracked_directories)?;
    }
    for _ in 0..gitlink_count {
        offset = expect_entry_type(&hot_data, offset, IndexEntryType::Gitlink)?;
        let path = read_string(&hot_data, &mut offset)?;
        let target = read_string(&hot_data, &mut offset)?;
        gitlinks.insert(path, target);
    }
    if offset != hot_data.len() {
        return Err(IndexError::InvalidFormat(
            "trailing bytes in hot index section".to_string(),
        ));
    }

    let mut entries = BTreeMap::new();
    if !hot_only {
        let file_data_len = data_len - hot_len;
        let mut file_data = vec![0_u8; file_data_len as usize];
        file.read_exact(&mut file_data)?;
        let mut file_offset = 0;
        for _ in 0..file_count {
            file_offset = expect_entry_type(&file_data, file_offset, IndexEntryType::File)?;
            file_offset = read_file_entry(&file_data, file_offset, &mut entries)?;
        }
        if file_offset != file_data.len() {
            return Err(IndexError::InvalidFormat(
                "trailing bytes in file index section".to_string(),
            ));
        }
        let mut checksum_bytes = [0_u8; 4];
        file.read_exact(&mut checksum_bytes)?;
        let mut all_data = hot_data.clone();
        all_data.extend_from_slice(&file_data);
        if crc32(&all_data) != u32::from_be_bytes(checksum_bytes) {
            return Err(IndexError::ChecksumMismatch);
        }
    }

    Ok(WorktreeIndex {
        entries,
        directories,
        untracked_directories,
        gitlinks,
        gitlinks_tree: None,
        clean_trees: BTreeMap::new(),
        dirty: false,
        pending_ops: Vec::new(),
        last_journal_bytes: 0,
        last_journal_ops: 0,
        last_journal_replay_ms: 0,
        hot_loaded: hot_only,
    })
}

fn expect_entry_type(
    data: &[u8],
    offset: usize,
    expected: IndexEntryType,
) -> Result<usize, IndexError> {
    let Some(value) = data.get(offset).copied() else {
        return Err(IndexError::InvalidFormat(
            "truncated entry type".to_string(),
        ));
    };
    if IndexEntryType::from_u8(value) != Some(expected) {
        return Err(IndexError::InvalidFormat(format!(
            "unexpected index entry type {value}"
        )));
    }
    Ok(offset + 1)
}

fn load_compact_versioned_with_untracked(
    file: &mut File,
    file_size: u64,
) -> Result<WorktreeIndex, IndexError> {
    file.seek(std::io::SeekFrom::Start(0))?;

    let mut header = [0u8; HEADER_SIZE_V5];
    file.read_exact(&mut header)?;

    let file_count = read_u32_be(&header[12..16], "compact index file count")?;
    let dir_count = read_u32_be(&header[16..20], "compact index directory count")?;
    let untracked_dir_count =
        read_u32_be(&header[20..24], "compact index untracked directory count")?;

    let footer_size = 4u64;
    let entry_data_size = file_size
        .saturating_sub(HEADER_SIZE_V5 as u64)
        .saturating_sub(footer_size);

    if entry_data_size == 0 && (file_count > 0 || dir_count > 0 || untracked_dir_count > 0) {
        return Err(IndexError::InvalidFormat(
            "entry data size mismatch".to_string(),
        ));
    }

    let mut entries = BTreeMap::new();
    let mut directories = BTreeMap::new();
    let mut untracked_directories = BTreeMap::new();
    let mut data = vec![0u8; entry_data_size as usize];
    file.read_exact(&mut data)?;

    let mut offset = 0;
    for _ in 0..file_count {
        if offset + 1 > data.len() {
            return Err(IndexError::InvalidFormat(
                "truncated entry type".to_string(),
            ));
        }

        let entry_type = match IndexEntryType::from_u8(data[offset]) {
            Some(et) => et,
            None => return Err(IndexError::InvalidFormat("invalid entry type".to_string())),
        };
        offset += 1;

        if entry_type != IndexEntryType::File {
            return Err(IndexError::InvalidFormat("expected file entry".to_string()));
        }

        offset = read_file_entry(&data, offset, &mut entries)?;
    }

    for _ in 0..dir_count {
        if offset + 1 > data.len() {
            return Err(IndexError::InvalidFormat(
                "truncated entry type".to_string(),
            ));
        }

        let entry_type = match IndexEntryType::from_u8(data[offset]) {
            Some(et) => et,
            None => return Err(IndexError::InvalidFormat("invalid entry type".to_string())),
        };
        offset += 1;

        if entry_type != IndexEntryType::Directory {
            return Err(IndexError::InvalidFormat(
                "expected directory entry".to_string(),
            ));
        }

        offset = read_compact_directory_entry(&data, offset, &mut directories)?;
    }

    for _ in 0..untracked_dir_count {
        if offset + 1 > data.len() {
            return Err(IndexError::InvalidFormat(
                "truncated entry type".to_string(),
            ));
        }

        let entry_type = match IndexEntryType::from_u8(data[offset]) {
            Some(et) => et,
            None => return Err(IndexError::InvalidFormat("invalid entry type".to_string())),
        };
        offset += 1;

        if entry_type != IndexEntryType::UntrackedDirectory {
            return Err(IndexError::InvalidFormat(
                "expected untracked directory entry".to_string(),
            ));
        }

        offset = read_untracked_directory_entry(&data, offset, &mut untracked_directories)?;
    }

    let mut checksum_bytes = [0u8; 4];
    file.read_exact(&mut checksum_bytes)?;
    let stored_checksum = u32::from_be_bytes(checksum_bytes);
    let computed_checksum = crc32(&data);

    if computed_checksum != stored_checksum {
        return Err(IndexError::ChecksumMismatch);
    }

    Ok(WorktreeIndex {
        entries,
        directories,
        untracked_directories,
        gitlinks: BTreeMap::new(),
        gitlinks_tree: None,
        clean_trees: BTreeMap::new(),
        dirty: false,
        pending_ops: Vec::new(),
        last_journal_bytes: 0,
        last_journal_ops: 0,
        last_journal_replay_ms: 0,
        hot_loaded: false,
    })
}

fn load_legacy_versioned(
    file: &mut File,
    file_size: u64,
    has_clean_tree_hash: bool,
) -> Result<WorktreeIndex, IndexError> {
    file.seek(std::io::SeekFrom::Start(0))?;

    let mut header = [0u8; HEADER_SIZE_V4];
    file.read_exact(&mut header)?;

    let file_count = read_u32_be(&header[12..16], "legacy index file count")?;
    let dir_count = read_u32_be(&header[16..20], "legacy index directory count")?;

    let footer_size = 4u64;
    let entry_data_size = file_size
        .saturating_sub(HEADER_SIZE_V4 as u64)
        .saturating_sub(footer_size);

    if entry_data_size == 0 && (file_count > 0 || dir_count > 0) {
        return Err(IndexError::InvalidFormat(
            "entry data size mismatch".to_string(),
        ));
    }

    let mut entries = BTreeMap::new();
    let mut directories = BTreeMap::new();
    let mut data = vec![0u8; entry_data_size as usize];
    file.read_exact(&mut data)?;

    let mut offset = 0;
    for _ in 0..file_count {
        if offset + 1 > data.len() {
            return Err(IndexError::InvalidFormat(
                "truncated entry type".to_string(),
            ));
        }

        let entry_type = match IndexEntryType::from_u8(data[offset]) {
            Some(et) => et,
            None => return Err(IndexError::InvalidFormat("invalid entry type".to_string())),
        };
        offset += 1;

        if entry_type != IndexEntryType::File {
            return Err(IndexError::InvalidFormat("expected file entry".to_string()));
        }

        offset = read_file_entry(&data, offset, &mut entries)?;
    }

    for _ in 0..dir_count {
        if offset + 1 > data.len() {
            return Err(IndexError::InvalidFormat(
                "truncated entry type".to_string(),
            ));
        }

        let entry_type = match IndexEntryType::from_u8(data[offset]) {
            Some(et) => et,
            None => return Err(IndexError::InvalidFormat("invalid entry type".to_string())),
        };
        offset += 1;

        if entry_type != IndexEntryType::Directory {
            return Err(IndexError::InvalidFormat(
                "expected directory entry".to_string(),
            ));
        }

        offset = read_legacy_directory_entry(&data, offset, &mut directories, has_clean_tree_hash)?;
    }

    let mut checksum_bytes = [0u8; 4];
    file.read_exact(&mut checksum_bytes)?;
    let stored_checksum = u32::from_be_bytes(checksum_bytes);
    let computed_checksum = crc32(&data);

    if computed_checksum != stored_checksum {
        return Err(IndexError::ChecksumMismatch);
    }

    Ok(WorktreeIndex {
        entries,
        directories,
        untracked_directories: BTreeMap::new(),
        gitlinks: BTreeMap::new(),
        gitlinks_tree: None,
        clean_trees: BTreeMap::new(),
        dirty: false,
        pending_ops: Vec::new(),
        last_journal_bytes: 0,
        last_journal_ops: 0,
        last_journal_replay_ms: 0,
        hot_loaded: false,
    })
}

fn load_compact_versioned(file: &mut File, file_size: u64) -> Result<WorktreeIndex, IndexError> {
    file.seek(std::io::SeekFrom::Start(0))?;

    let mut header = [0u8; HEADER_SIZE_V4];
    file.read_exact(&mut header)?;

    let file_count = read_u32_be(&header[12..16], "compact index file count")?;
    let dir_count = read_u32_be(&header[16..20], "compact index directory count")?;
    let footer_size = 4u64;
    let entry_data_size = file_size
        .saturating_sub(HEADER_SIZE_V4 as u64)
        .saturating_sub(footer_size);

    if entry_data_size == 0 && (file_count > 0 || dir_count > 0) {
        return Err(IndexError::InvalidFormat(
            "entry data size mismatch".to_string(),
        ));
    }

    let mut entries = BTreeMap::new();
    let mut directories = BTreeMap::new();
    let mut data = vec![0u8; entry_data_size as usize];
    file.read_exact(&mut data)?;

    let mut offset = 0;
    for _ in 0..file_count {
        if offset + 1 > data.len() {
            return Err(IndexError::InvalidFormat(
                "truncated entry type".to_string(),
            ));
        }
        let entry_type = match IndexEntryType::from_u8(data[offset]) {
            Some(et) => et,
            None => return Err(IndexError::InvalidFormat("invalid entry type".to_string())),
        };
        offset += 1;
        if entry_type != IndexEntryType::File {
            return Err(IndexError::InvalidFormat("expected file entry".to_string()));
        }
        offset = read_file_entry(&data, offset, &mut entries)?;
    }

    for _ in 0..dir_count {
        if offset + 1 > data.len() {
            return Err(IndexError::InvalidFormat(
                "truncated entry type".to_string(),
            ));
        }
        let entry_type = match IndexEntryType::from_u8(data[offset]) {
            Some(et) => et,
            None => return Err(IndexError::InvalidFormat("invalid entry type".to_string())),
        };
        offset += 1;
        if entry_type != IndexEntryType::Directory {
            return Err(IndexError::InvalidFormat(
                "expected directory entry".to_string(),
            ));
        }
        offset = read_compact_directory_entry(&data, offset, &mut directories)?;
    }

    let mut checksum_bytes = [0u8; 4];
    file.read_exact(&mut checksum_bytes)?;
    let stored_checksum = u32::from_be_bytes(checksum_bytes);
    let computed_checksum = crc32(&data);

    if computed_checksum != stored_checksum {
        return Err(IndexError::ChecksumMismatch);
    }

    Ok(WorktreeIndex {
        entries,
        directories,
        untracked_directories: BTreeMap::new(),
        gitlinks: BTreeMap::new(),
        gitlinks_tree: None,
        clean_trees: BTreeMap::new(),
        dirty: false,
        pending_ops: Vec::new(),
        last_journal_bytes: 0,
        last_journal_ops: 0,
        last_journal_replay_ms: 0,
        hot_loaded: false,
    })
}

fn read_file_entry(
    data: &[u8],
    mut offset: usize,
    entries: &mut BTreeMap<String, IndexEntry>,
) -> Result<usize, IndexError> {
    if offset + 4 > data.len() {
        return Err(IndexError::InvalidFormat("truncated path len".to_string()));
    }

    let path_len = read_u32_be(&data[offset..], "journal file path length")? as usize;
    offset += 4;

    if offset + path_len + 32 + 8 + 8 + 4 + 1 + 1 > data.len() {
        return Err(IndexError::InvalidFormat(
            "truncated file entry data".to_string(),
        ));
    }

    let path = String::from_utf8(data[offset..offset + path_len].to_vec())
        .map_err(|_| IndexError::InvalidUtf8(format!("path at offset {}", offset)))?;
    offset += path_len;

    let mut hash_bytes = [0u8; 32];
    hash_bytes.copy_from_slice(&data[offset..offset + 32]);
    let hash = ContentHash::from_bytes(hash_bytes);
    offset += 32;

    let size = read_u64_be(&data[offset..], "journal file size")?;
    offset += 8;

    let modified_sec = read_i64_be(&data[offset..], "journal file modified seconds")?;
    offset += 8;

    let modified_nsec = read_u32_be(&data[offset..], "journal file modified nanos")?;
    offset += 4;

    let executable = data[offset] != 0;
    offset += 1;

    let kind = IndexEntryKind::from_u8(data[offset]);
    offset += 1;

    entries.insert(
        path.clone(),
        IndexEntry {
            hash,
            size,
            modified_sec,
            modified_nsec,
            executable,
            kind,
        },
    );

    Ok(offset)
}

fn read_legacy_directory_entry(
    data: &[u8],
    mut offset: usize,
    directories: &mut BTreeMap<String, DirectoryCacheEntry>,
    has_clean_tree_hash: bool,
) -> Result<usize, IndexError> {
    if offset + 4 > data.len() {
        return Err(IndexError::InvalidFormat("truncated path len".to_string()));
    }

    let path_len = read_u32_be(&data[offset..], "compact index path length")? as usize;
    offset += 4;

    let hash_bytes_len = if has_clean_tree_hash { 1 + 32 } else { 0 };
    if offset + path_len + 8 + 4 + 4 + hash_bytes_len + 4 > data.len() {
        return Err(IndexError::InvalidFormat(
            "truncated directory entry data".to_string(),
        ));
    }

    let path = String::from_utf8(data[offset..offset + path_len].to_vec())
        .map_err(|_| IndexError::InvalidUtf8(format!("path at offset {}", offset)))?;
    offset += path_len;

    let mtime_sec = read_i64_be(&data[offset..], "compact directory mtime seconds")?;
    offset += 8;

    let mtime_nsec = read_u32_be(&data[offset..], "compact directory mtime nanos")?;
    offset += 4;

    let child_count = read_u32_be(&data[offset..], "compact directory child count")?;
    offset += 4;

    let clean_tree_hash = if has_clean_tree_hash {
        let present = data[offset] != 0;
        offset += 1;
        if present {
            let mut hash_bytes = [0u8; 32];
            hash_bytes.copy_from_slice(&data[offset..offset + 32]);
            offset += 32;
            Some(ContentHash::from_bytes(hash_bytes))
        } else {
            offset += 32;
            None
        }
    } else {
        None
    };

    let children_len = read_u32_be(&data[offset..], "compact untracked children len")? as usize;
    offset += 4;

    if offset + children_len > data.len() {
        return Err(IndexError::InvalidFormat(
            "truncated children data".to_string(),
        ));
    }

    let mut children = Vec::new();
    let mut current = Vec::new();
    for &byte in &data[offset..offset + children_len] {
        if byte == 0 {
            if !current.is_empty() {
                children.push(
                    String::from_utf8(current.clone())
                        .map_err(|_| IndexError::InvalidUtf8("invalid child name".to_string()))?,
                );
                current.clear();
            }
        } else {
            current.push(byte);
        }
    }
    offset += children_len;

    directories.insert(
        path.clone(),
        DirectoryCacheEntry {
            mtime_sec,
            mtime_nsec,
            child_count,
            child_digest: super::digest_child_names(
                children.iter().map(String::as_str),
                child_count,
            ),
            clean_tree_hash,
        },
    );

    Ok(offset)
}

fn read_compact_directory_entry(
    data: &[u8],
    mut offset: usize,
    directories: &mut BTreeMap<String, DirectoryCacheEntry>,
) -> Result<usize, IndexError> {
    if offset + 4 > data.len() {
        return Err(IndexError::InvalidFormat("truncated path len".to_string()));
    }

    let path_len = read_u32_be(&data[offset..], "compact file path length")? as usize;
    offset += 4;

    if offset + path_len + 8 + 4 + 4 + 32 + 1 + 32 > data.len() {
        return Err(IndexError::InvalidFormat(
            "truncated directory entry data".to_string(),
        ));
    }

    let path = String::from_utf8(data[offset..offset + path_len].to_vec())
        .map_err(|_| IndexError::InvalidUtf8(format!("path at offset {}", offset)))?;
    offset += path_len;

    let mtime_sec = read_i64_be(&data[offset..], "compact file mtime seconds")?;
    offset += 8;
    let mtime_nsec = read_u32_be(&data[offset..], "compact file mtime nanos")?;
    offset += 4;
    let child_count = read_u32_be(&data[offset..], "compact file child count")?;
    offset += 4;

    let mut child_digest_bytes = [0u8; 32];
    child_digest_bytes.copy_from_slice(&data[offset..offset + 32]);
    let child_digest = ContentHash::from_bytes(child_digest_bytes);
    offset += 32;

    let clean_tree_hash = if data[offset] != 0 {
        offset += 1;
        let mut hash_bytes = [0u8; 32];
        hash_bytes.copy_from_slice(&data[offset..offset + 32]);
        offset += 32;
        Some(ContentHash::from_bytes(hash_bytes))
    } else {
        offset += 1 + 32;
        None
    };

    directories.insert(
        path,
        DirectoryCacheEntry {
            mtime_sec,
            mtime_nsec,
            child_count,
            child_digest,
            clean_tree_hash,
        },
    );

    Ok(offset)
}

fn read_untracked_directory_entry(
    data: &[u8],
    mut offset: usize,
    directories: &mut BTreeMap<String, UntrackedDirectoryCacheEntry>,
) -> Result<usize, IndexError> {
    if offset + 4 > data.len() {
        return Err(IndexError::InvalidFormat("truncated path len".to_string()));
    }
    let path_len = read_u32_be(&data[offset..], "legacy index path length")? as usize;
    offset += 4;

    if offset + path_len + 8 + 4 + 4 + 32 + 32 + 4 > data.len() {
        return Err(IndexError::InvalidFormat(
            "truncated untracked directory entry".to_string(),
        ));
    }

    let path = String::from_utf8(data[offset..offset + path_len].to_vec())
        .map_err(|_| IndexError::InvalidUtf8(format!("path at offset {}", offset)))?;
    offset += path_len;

    let entry = read_untracked_directory_entry_payload(data, &mut offset)?;
    directories.insert(path, entry);

    Ok(offset)
}

fn hot_sidecar_dir(snapshot_path: &Path) -> std::path::PathBuf {
    snapshot_path.with_extension("hot")
}

fn hot_directory_record_path(snapshot_path: &Path, key: &str) -> std::path::PathBuf {
    let hash = ContentHash::compute_typed("worktree-index-hot-path", key.as_bytes());
    hot_sidecar_dir(snapshot_path).join(format!("{hash}.bin"))
}

fn hot_gitlinks_path(snapshot_path: &Path) -> std::path::PathBuf {
    hot_sidecar_dir(snapshot_path).join("gitlinks.bin")
}

fn write_reconstructible_atomic(path: &Path, bytes: &[u8]) -> Result<(), IndexError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.flush()?;
    let (_file, temporary_path) = temporary
        .keep()
        .map_err(|error| IndexError::Io(error.error))?;
    match fs::rename(&temporary_path, path) {
        Ok(()) => Ok(()),
        Err(_error) if path.exists() => {
            fs::remove_file(path)?;
            fs::rename(temporary_path, path)?;
            Ok(())
        }
        Err(error) => Err(IndexError::Io(error)),
    }
}

fn frame_hot_record(mut body: Vec<u8>) -> Vec<u8> {
    let checksum = crc32(&body);
    body.extend_from_slice(&checksum.to_be_bytes());
    body
}

fn verified_hot_body(bytes: &[u8]) -> Option<&[u8]> {
    let body_len = bytes.len().checked_sub(4)?;
    let stored = u32::from_be_bytes(bytes[body_len..].try_into().ok()?);
    (crc32(&bytes[..body_len]) == stored).then_some(&bytes[..body_len])
}

fn write_hot_sidecars(index: &WorktreeIndex, snapshot_path: &Path) -> Result<(), IndexError> {
    let keys = index
        .directories
        .keys()
        .chain(index.untracked_directories.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in keys {
        let mut body = Vec::new();
        body.extend_from_slice(HOT_RECORD_MAGIC);
        body.extend_from_slice(&HOT_RECORD_VERSION.to_be_bytes());
        write_string(&mut body, &key)?;
        let directory = index.directories.get(&key);
        body.push(u8::from(directory.is_some()));
        if let Some(directory) = directory {
            write_directory_entry_payload(&mut body, directory)?;
        }
        let untracked = index.untracked_directories.get(&key);
        body.push(u8::from(untracked.is_some()));
        if let Some(untracked) = untracked {
            write_untracked_directory_entry_payload(&mut body, untracked)?;
        }
        let clean_tree = index.clean_trees.get(&key);
        body.push(u8::from(clean_tree.is_some()));
        if let Some(clean_tree) = clean_tree {
            let encoded = rmp_serde::to_vec_named(clean_tree)
                .map_err(|error| IndexError::InvalidFormat(error.to_string()))?;
            body.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
            body.extend_from_slice(&encoded);
        }
        write_reconstructible_atomic(
            &hot_directory_record_path(snapshot_path, &key),
            &frame_hot_record(body),
        )?;
    }

    let mut body = Vec::new();
    body.extend_from_slice(HOT_RECORD_MAGIC);
    body.extend_from_slice(&HOT_RECORD_VERSION.to_be_bytes());
    let root_tree_hash = index.gitlinks_tree;
    body.push(u8::from(root_tree_hash.is_some()));
    body.extend_from_slice(
        root_tree_hash
            .as_ref()
            .map(ContentHash::as_bytes)
            .unwrap_or(&[0_u8; 32]),
    );
    body.extend_from_slice(&(index.gitlinks.len() as u32).to_be_bytes());
    for (path, target) in &index.gitlinks {
        write_string(&mut body, path)?;
        write_string(&mut body, target)?;
    }
    write_reconstructible_atomic(&hot_gitlinks_path(snapshot_path), &frame_hot_record(body))
}

type HotDirectoryRecord = (
    Option<DirectoryCacheEntry>,
    Option<UntrackedDirectoryCacheEntry>,
    Option<objects::object::Tree>,
    u64,
);

fn load_hot_directory_record(
    snapshot_path: &Path,
    expected_key: &str,
    decode_clean_tree: bool,
) -> Result<Option<HotDirectoryRecord>, IndexError> {
    let bytes = match fs::read(hot_directory_record_path(snapshot_path, expected_key)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(IndexError::Io(error)),
    };
    let Some(body) = verified_hot_body(&bytes) else {
        return Ok(None);
    };
    if body.len() < 12 || &body[..8] != HOT_RECORD_MAGIC {
        return Ok(None);
    }
    if read_u32_be(&body[8..12], "hot record version")? != HOT_RECORD_VERSION {
        return Ok(None);
    }
    let mut offset = 12;
    let key = read_string(body, &mut offset)?;
    if key != expected_key {
        return Ok(None);
    }
    let Some(has_directory) = body.get(offset).copied() else {
        return Ok(None);
    };
    offset += 1;
    let directory = if has_directory != 0 {
        Some(read_directory_entry_payload(body, &mut offset)?)
    } else {
        None
    };
    let Some(has_untracked) = body.get(offset).copied() else {
        return Ok(None);
    };
    offset += 1;
    let untracked = if has_untracked != 0 {
        Some(read_untracked_directory_entry_payload(body, &mut offset)?)
    } else {
        None
    };
    let Some(has_tree) = body.get(offset).copied() else {
        return Ok(None);
    };
    offset += 1;
    let clean_tree = if has_tree != 0 {
        let len = read_u32_be(&body[offset..], "hot tree length")? as usize;
        offset += 4;
        let end = offset
            .checked_add(len)
            .filter(|end| *end <= body.len())
            .ok_or_else(|| IndexError::InvalidFormat("truncated hot tree".to_string()))?;
        let tree = if decode_clean_tree {
            Some(
                rmp_serde::from_slice(&body[offset..end])
                    .map_err(|error| IndexError::InvalidFormat(error.to_string()))?,
            )
        } else {
            None
        };
        offset = end;
        tree
    } else {
        None
    };
    if offset != body.len() {
        return Ok(None);
    }
    Ok(Some((directory, untracked, clean_tree, bytes.len() as u64)))
}

fn load_hot_gitlinks(
    snapshot_path: &Path,
    index: &mut WorktreeIndex,
    stats: &mut WorktreeIndexLoadStats,
) -> Result<bool, IndexError> {
    let bytes = match fs::read(hot_gitlinks_path(snapshot_path)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(IndexError::Io(error)),
    };
    stats.snapshot_bytes = stats.snapshot_bytes.saturating_add(bytes.len() as u64);
    let Some(body) = verified_hot_body(&bytes) else {
        return Ok(false);
    };
    if body.len() < 12 + 1 + 32 + 4 || &body[..8] != HOT_RECORD_MAGIC {
        return Ok(false);
    }
    if read_u32_be(&body[8..12], "hot gitlink version")? != HOT_RECORD_VERSION {
        return Ok(false);
    }
    let mut offset = 12;
    let has_root = body[offset] != 0;
    offset += 1;
    let root_hash = if has_root {
        let mut hash = [0_u8; 32];
        hash.copy_from_slice(&body[offset..offset + 32]);
        Some(ContentHash::from_bytes(hash))
    } else {
        None
    };
    offset += 32;
    if root_hash.is_none() {
        return Ok(false);
    }
    index.gitlinks_tree = root_hash;
    let count = read_u32_be(&body[offset..], "hot gitlink count")?;
    offset += 4;
    for _ in 0..count {
        let path = read_string(body, &mut offset)?;
        let target = read_string(body, &mut offset)?;
        index.gitlinks.insert(path, target);
    }
    Ok(offset == body.len())
}

fn write_snapshot(index: &WorktreeIndex, path: &Path) -> Result<(), IndexError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut hot_data = Vec::new();
    let mut file_data = Vec::new();

    for (path, entry) in &index.entries {
        let path_bytes = path.as_bytes();
        file_data.reserve_exact(1 + 4 + path_bytes.len() + 32 + 8 + 8 + 4 + 1 + 1);

        file_data.push(IndexEntryType::File.to_u8());
        file_data.extend_from_slice(&(path_bytes.len() as u32).to_be_bytes());
        file_data.extend_from_slice(path_bytes);
        file_data.extend_from_slice(entry.hash.as_bytes());
        file_data.extend_from_slice(&entry.size.to_be_bytes());
        file_data.extend_from_slice(&entry.modified_sec.to_be_bytes());
        file_data.extend_from_slice(&entry.modified_nsec.to_be_bytes());
        file_data.push(if entry.executable { 1 } else { 0 });
        file_data.push(entry.kind.to_u8());
    }

    for (path, dir) in &index.directories {
        let path_bytes = path.as_bytes();
        hot_data.reserve_exact(1 + 4 + path_bytes.len() + 8 + 4 + 4 + 32 + 1 + 32);

        hot_data.push(IndexEntryType::Directory.to_u8());
        hot_data.extend_from_slice(&(path_bytes.len() as u32).to_be_bytes());
        hot_data.extend_from_slice(path_bytes);
        hot_data.extend_from_slice(&dir.mtime_sec.to_be_bytes());
        hot_data.extend_from_slice(&dir.mtime_nsec.to_be_bytes());
        hot_data.extend_from_slice(&dir.child_count.to_be_bytes());
        hot_data.extend_from_slice(dir.child_digest.as_bytes());
        hot_data.push(u8::from(dir.clean_tree_hash.is_some()));
        hot_data.extend_from_slice(
            dir.clean_tree_hash
                .as_ref()
                .map(ContentHash::as_bytes)
                .unwrap_or(&[0; 32]),
        );
    }

    for (path, dir) in &index.untracked_directories {
        let path_bytes = path.as_bytes();
        hot_data.push(IndexEntryType::UntrackedDirectory.to_u8());
        hot_data.extend_from_slice(&(path_bytes.len() as u32).to_be_bytes());
        hot_data.extend_from_slice(path_bytes);
        write_untracked_directory_entry_payload(&mut hot_data, dir)?;
    }

    for (path, target) in &index.gitlinks {
        hot_data.push(IndexEntryType::Gitlink.to_u8());
        write_string(&mut hot_data, path)?;
        write_string(&mut hot_data, target)?;
    }

    let mut entry_data = hot_data.clone();
    entry_data.extend_from_slice(&file_data);
    let checksum = crc32(&entry_data);

    let mut encoded = Vec::with_capacity(HEADER_SIZE_V6 + entry_data.len() + 4);
    encoded.extend_from_slice(INDEX_MAGIC);
    encoded.extend_from_slice(&INDEX_VERSION.to_be_bytes());
    encoded.extend_from_slice(&(index.entries.len() as u32).to_be_bytes());
    encoded.extend_from_slice(&(index.directories.len() as u32).to_be_bytes());
    encoded.extend_from_slice(&(index.untracked_directories.len() as u32).to_be_bytes());
    encoded.extend_from_slice(&(index.gitlinks.len() as u32).to_be_bytes());
    encoded.extend_from_slice(&(hot_data.len() as u64).to_be_bytes());
    encoded.extend_from_slice(&crc32(&hot_data).to_be_bytes());
    encoded.extend_from_slice(&entry_data);
    encoded.extend_from_slice(&checksum.to_be_bytes());

    let mut temp_file = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
    temp_file.write_all(&encoded)?;
    temp_file.flush()?;
    sync_file(temp_file.as_file(), temp_file.path())?;
    let (_file, temp_path) = temp_file
        .keep()
        .map_err(|error| IndexError::Io(error.error))?;
    fs::rename(&temp_path, path)?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }

    Ok(())
}

fn append_journal(index: &WorktreeIndex, journal_path: &Path) -> Result<(), IndexError> {
    if let Some(parent) = journal_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let journal_existed = journal_path.exists();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(journal_path)?;

    if file.metadata()?.len() == 0 {
        file.write_all(JOURNAL_MAGIC)?;
        file.write_all(&JOURNAL_VERSION.to_be_bytes())?;
    }

    let payload = serialize_journal_ops(&index.pending_ops)?;
    file.write_all(&(payload.len() as u32).to_be_bytes())?;
    file.write_all(&crc32(&payload).to_be_bytes())?;
    file.write_all(&payload)?;
    file.flush()?;
    sync_file_data(&file, journal_path)?;
    if !journal_existed && let Some(parent) = journal_path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn apply_journal(index: &mut WorktreeIndex, journal_path: &Path) -> Result<usize, IndexError> {
    let mut file = File::open(journal_path)?;
    let file_len = file.metadata()?.len();
    if file_len == 0 {
        return Ok(0);
    }

    let mut header = [0u8; 12];
    if let Err(error) = file.read_exact(&mut header) {
        return if error.kind() == std::io::ErrorKind::UnexpectedEof {
            Err(IndexError::InvalidFormat(
                "truncated journal header".to_string(),
            ))
        } else {
            Err(IndexError::Io(error))
        };
    }
    if &header[..8] != JOURNAL_MAGIC {
        return Err(IndexError::InvalidFormat(
            "missing journal magic bytes".to_string(),
        ));
    }
    let version = read_u32_be(&header[8..12], "journal version")?;
    if version != JOURNAL_VERSION {
        return Err(IndexError::VersionMismatch {
            expected: JOURNAL_VERSION,
            got: version,
        });
    }

    let mut applied_ops = 0usize;
    loop {
        let mut frame_header = [0u8; 8];
        match file.read_exact(&mut frame_header) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(IndexError::Io(error)),
        }

        let frame_len = read_u32_be(&frame_header[..4], "journal frame length")? as usize;
        let expected_checksum = read_u32_be(&frame_header[4..8], "journal frame checksum")?;
        let mut payload = vec![0u8; frame_len];
        match file.read_exact(&mut payload) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(IndexError::Io(error)),
        }

        if crc32(&payload) != expected_checksum {
            warn!(journal_path = %journal_path.display(), "Rejecting corrupt worktree index journal");
            return Err(IndexError::ChecksumMismatch);
        }

        let ops = deserialize_journal_ops(&payload)?;
        applied_ops += ops.len();
        for op in ops {
            match op {
                JournalOp::UpsertFile { path, entry } => {
                    index.entries.insert(path, entry);
                }
                JournalOp::RemoveFile { path } => {
                    let _ = index.entries.remove(&path);
                }
                JournalOp::UpsertDirectory { path, entry } => {
                    index.directories.insert(path, entry);
                }
                JournalOp::RemoveDirectory { path } => {
                    let _ = index.directories.remove(&path);
                }
                JournalOp::UpsertUntrackedDirectory { path, entry } => {
                    index.untracked_directories.insert(path, entry);
                }
                JournalOp::RemoveUntrackedDirectory { path } => {
                    let _ = index.untracked_directories.remove(&path);
                }
                JournalOp::UpsertGitlink { path, target } => {
                    index.gitlinks.insert(path, target);
                }
                JournalOp::RemoveGitlink { path } => {
                    let _ = index.gitlinks.remove(&path);
                }
            }
        }
    }

    Ok(applied_ops)
}

fn journal_path(snapshot_path: &Path) -> std::path::PathBuf {
    snapshot_path.with_extension("journal")
}

fn write_string(writer: &mut impl Write, value: &str) -> Result<(), IndexError> {
    writer.write_all(&(value.len() as u32).to_be_bytes())?;
    writer.write_all(value.as_bytes())?;
    Ok(())
}

fn read_string(data: &[u8], offset: &mut usize) -> Result<String, IndexError> {
    if *offset + 4 > data.len() {
        return Err(IndexError::InvalidFormat(
            "truncated string len".to_string(),
        ));
    }
    let path_len = read_u32_be(&data[*offset..], "journal string length")? as usize;
    *offset += 4;
    if *offset + path_len > data.len() {
        return Err(IndexError::InvalidFormat(
            "truncated string data".to_string(),
        ));
    }
    let value = String::from_utf8(data[*offset..*offset + path_len].to_vec())
        .map_err(|_| IndexError::InvalidUtf8(format!("path at offset {}", *offset)))?;
    *offset += path_len;
    Ok(value)
}

fn write_file_entry_payload(writer: &mut impl Write, entry: &IndexEntry) -> Result<(), IndexError> {
    writer.write_all(entry.hash.as_bytes())?;
    writer.write_all(&entry.size.to_be_bytes())?;
    writer.write_all(&entry.modified_sec.to_be_bytes())?;
    writer.write_all(&entry.modified_nsec.to_be_bytes())?;
    writer.write_all(&[u8::from(entry.executable)])?;
    writer.write_all(&[entry.kind.to_u8()])?;
    Ok(())
}

fn read_file_entry_payload(data: &[u8], offset: &mut usize) -> Result<IndexEntry, IndexError> {
    if *offset + 32 + 8 + 8 + 4 + 1 + 1 > data.len() {
        return Err(IndexError::InvalidFormat(
            "truncated file journal payload".to_string(),
        ));
    }
    let mut hash_bytes = [0u8; 32];
    hash_bytes.copy_from_slice(&data[*offset..*offset + 32]);
    *offset += 32;
    let size = read_u64_be(&data[*offset..], "journal file size")?;
    *offset += 8;
    let modified_sec = read_i64_be(&data[*offset..], "journal file modified seconds")?;
    *offset += 8;
    let modified_nsec = read_u32_be(&data[*offset..], "journal file modified nanos")?;
    *offset += 4;
    let executable = data[*offset] != 0;
    *offset += 1;
    let kind = IndexEntryKind::from_u8(data[*offset]);
    *offset += 1;
    Ok(IndexEntry {
        hash: ContentHash::from_bytes(hash_bytes),
        size,
        modified_sec,
        modified_nsec,
        executable,
        kind,
    })
}

fn write_directory_entry_payload(
    writer: &mut impl Write,
    entry: &DirectoryCacheEntry,
) -> Result<(), IndexError> {
    writer.write_all(&entry.mtime_sec.to_be_bytes())?;
    writer.write_all(&entry.mtime_nsec.to_be_bytes())?;
    writer.write_all(&entry.child_count.to_be_bytes())?;
    writer.write_all(entry.child_digest.as_bytes())?;
    writer.write_all(&[u8::from(entry.clean_tree_hash.is_some())])?;
    writer.write_all(
        entry
            .clean_tree_hash
            .as_ref()
            .map(ContentHash::as_bytes)
            .unwrap_or(&[0; 32]),
    )?;
    Ok(())
}

fn read_directory_entry_payload(
    data: &[u8],
    offset: &mut usize,
) -> Result<DirectoryCacheEntry, IndexError> {
    if *offset + 8 + 4 + 4 + 32 + 1 + 32 > data.len() {
        return Err(IndexError::InvalidFormat(
            "truncated directory journal payload".to_string(),
        ));
    }
    let mtime_sec = read_i64_be(&data[*offset..], "journal directory mtime seconds")?;
    *offset += 8;
    let mtime_nsec = read_u32_be(&data[*offset..], "journal directory mtime nanos")?;
    *offset += 4;
    let child_count = read_u32_be(&data[*offset..], "journal directory child count")?;
    *offset += 4;
    let mut child_digest_bytes = [0u8; 32];
    child_digest_bytes.copy_from_slice(&data[*offset..*offset + 32]);
    *offset += 32;
    let child_digest = ContentHash::from_bytes(child_digest_bytes);
    let clean_tree_hash = if data[*offset] != 0 {
        *offset += 1;
        let mut hash_bytes = [0u8; 32];
        hash_bytes.copy_from_slice(&data[*offset..*offset + 32]);
        *offset += 32;
        Some(ContentHash::from_bytes(hash_bytes))
    } else {
        *offset += 1 + 32;
        None
    };
    Ok(DirectoryCacheEntry {
        mtime_sec,
        mtime_nsec,
        child_count,
        child_digest,
        clean_tree_hash,
    })
}

fn write_untracked_directory_entry_payload(
    writer: &mut impl Write,
    entry: &UntrackedDirectoryCacheEntry,
) -> Result<(), IndexError> {
    writer.write_all(&entry.mtime_sec.to_be_bytes())?;
    writer.write_all(&entry.mtime_nsec.to_be_bytes())?;
    writer.write_all(&entry.child_count.to_be_bytes())?;
    writer.write_all(entry.child_digest.as_bytes())?;
    writer.write_all(entry.ignore_fingerprint.as_bytes())?;
    writer.write_all(&(entry.added_paths.len() as u32).to_be_bytes())?;
    for path in &entry.added_paths {
        write_string(writer, path)?;
    }
    Ok(())
}

fn read_untracked_directory_entry_payload(
    data: &[u8],
    offset: &mut usize,
) -> Result<UntrackedDirectoryCacheEntry, IndexError> {
    if *offset + 8 + 4 + 4 + 32 + 32 + 4 > data.len() {
        return Err(IndexError::InvalidFormat(
            "truncated untracked directory payload".to_string(),
        ));
    }
    let mtime_sec = read_i64_be(
        &data[*offset..],
        "journal untracked directory mtime seconds",
    )?;
    *offset += 8;
    let mtime_nsec = read_u32_be(&data[*offset..], "journal untracked directory mtime nanos")?;
    *offset += 4;
    let child_count = read_u32_be(&data[*offset..], "journal untracked directory child count")?;
    *offset += 4;
    let mut child_digest_bytes = [0u8; 32];
    child_digest_bytes.copy_from_slice(&data[*offset..*offset + 32]);
    *offset += 32;
    let child_digest = ContentHash::from_bytes(child_digest_bytes);
    let mut ignore_fingerprint_bytes = [0u8; 32];
    ignore_fingerprint_bytes.copy_from_slice(&data[*offset..*offset + 32]);
    *offset += 32;
    let ignore_fingerprint = ContentHash::from_bytes(ignore_fingerprint_bytes);
    let added_path_count = read_u32_be(&data[*offset..], "journal added path count")?;
    *offset += 4;
    let mut added_paths = Vec::with_capacity(added_path_count as usize);
    for _ in 0..added_path_count {
        added_paths.push(read_string(data, offset)?);
    }
    Ok(UntrackedDirectoryCacheEntry {
        mtime_sec,
        mtime_nsec,
        child_count,
        child_digest,
        ignore_fingerprint,
        added_paths,
    })
}

fn write_journal_op(writer: &mut impl Write, op: &JournalOp) -> Result<(), IndexError> {
    match op {
        JournalOp::UpsertFile { path, entry } => {
            writer.write_all(&[0x01])?;
            write_string(writer, path)?;
            write_file_entry_payload(writer, entry)?;
        }
        JournalOp::RemoveFile { path } => {
            writer.write_all(&[0x02])?;
            write_string(writer, path)?;
        }
        JournalOp::UpsertDirectory { path, entry } => {
            writer.write_all(&[0x03])?;
            write_string(writer, path)?;
            write_directory_entry_payload(writer, entry)?;
        }
        JournalOp::RemoveDirectory { path } => {
            writer.write_all(&[0x04])?;
            write_string(writer, path)?;
        }
        JournalOp::UpsertUntrackedDirectory { path, entry } => {
            writer.write_all(&[0x05])?;
            write_string(writer, path)?;
            write_untracked_directory_entry_payload(writer, entry)?;
        }
        JournalOp::RemoveUntrackedDirectory { path } => {
            writer.write_all(&[0x06])?;
            write_string(writer, path)?;
        }
        JournalOp::UpsertGitlink { path, target } => {
            writer.write_all(&[0x07])?;
            write_string(writer, path)?;
            write_string(writer, target)?;
        }
        JournalOp::RemoveGitlink { path } => {
            writer.write_all(&[0x08])?;
            write_string(writer, path)?;
        }
    }
    Ok(())
}

fn serialize_journal_ops(ops: &[JournalOp]) -> Result<Vec<u8>, IndexError> {
    let mut payload = Vec::new();
    for op in ops {
        write_journal_op(&mut payload, op)?;
    }
    Ok(payload)
}

fn deserialize_journal_ops(payload: &[u8]) -> Result<Vec<JournalOp>, IndexError> {
    let mut ops = Vec::new();
    let mut offset = 0usize;
    while offset < payload.len() {
        let op_type = *payload
            .get(offset)
            .ok_or_else(|| IndexError::InvalidFormat("truncated journal op".to_string()))?;
        offset += 1;
        match op_type {
            0x01 => {
                let path = read_string(payload, &mut offset)?;
                let entry = read_file_entry_payload(payload, &mut offset)?;
                ops.push(JournalOp::UpsertFile { path, entry });
            }
            0x02 => {
                let path = read_string(payload, &mut offset)?;
                ops.push(JournalOp::RemoveFile { path });
            }
            0x03 => {
                let path = read_string(payload, &mut offset)?;
                let entry = read_directory_entry_payload(payload, &mut offset)?;
                ops.push(JournalOp::UpsertDirectory { path, entry });
            }
            0x04 => {
                let path = read_string(payload, &mut offset)?;
                ops.push(JournalOp::RemoveDirectory { path });
            }
            0x05 => {
                let path = read_string(payload, &mut offset)?;
                let entry = read_untracked_directory_entry_payload(payload, &mut offset)?;
                ops.push(JournalOp::UpsertUntrackedDirectory { path, entry });
            }
            0x06 => {
                let path = read_string(payload, &mut offset)?;
                ops.push(JournalOp::RemoveUntrackedDirectory { path });
            }
            0x07 => {
                let path = read_string(payload, &mut offset)?;
                let target = read_string(payload, &mut offset)?;
                ops.push(JournalOp::UpsertGitlink { path, target });
            }
            0x08 => {
                let path = read_string(payload, &mut offset)?;
                ops.push(JournalOp::RemoveGitlink { path });
            }
            _ => {
                return Err(IndexError::InvalidFormat(
                    "invalid journal op type".to_string(),
                ));
            }
        }
    }
    Ok(ops)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn sample_file_entry(label: &str) -> IndexEntry {
        IndexEntry {
            hash: ContentHash::compute(label.as_bytes()),
            size: label.len() as u64,
            modified_sec: 1_700_000_000,
            modified_nsec: 123,
            executable: false,
            kind: IndexEntryKind::File,
        }
    }

    fn sample_untracked_directory_entry() -> UntrackedDirectoryCacheEntry {
        UntrackedDirectoryCacheEntry {
            mtime_sec: 1_700_000_010,
            mtime_nsec: 456,
            child_count: 2,
            child_digest: ContentHash::compute_typed("dirnames", b"children"),
            ignore_fingerprint: ContentHash::compute_typed("heddle.ignore", b"*.log\n"),
            added_paths: vec!["nested/one.txt".to_string(), "nested/two.txt".to_string()],
        }
    }

    fn write_raw_v5_snapshot(
        path: &Path,
        file_count: u32,
        dir_count: u32,
        untracked_dir_count: u32,
        entry_data: &[u8],
    ) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(INDEX_MAGIC);
        bytes.extend_from_slice(&5_u32.to_be_bytes());
        bytes.extend_from_slice(&file_count.to_be_bytes());
        bytes.extend_from_slice(&dir_count.to_be_bytes());
        bytes.extend_from_slice(&untracked_dir_count.to_be_bytes());
        bytes.extend_from_slice(entry_data);
        bytes.extend_from_slice(&crc32(entry_data).to_be_bytes());
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn load_profiled_rejects_truncated_index_header() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("index.bin");
        fs::write(&path, b"GALE").unwrap();

        let err = load_profiled(&path).unwrap_err();
        assert!(
            matches!(err, IndexError::InvalidFormat(message) if message.contains("truncated index header"))
        );
    }

    #[test]
    fn apply_journal_rejects_truncated_header() {
        let temp = TempDir::new().unwrap();
        let journal_path = temp.path().join("index.journal");
        fs::write(&journal_path, b"GALE").unwrap();

        let mut index = WorktreeIndex::new();
        let err = apply_journal(&mut index, &journal_path).unwrap_err();
        assert!(
            matches!(err, IndexError::InvalidFormat(message) if message.contains("truncated journal header"))
        );
    }

    #[test]
    fn load_profiled_rejects_invalid_magic_and_version() {
        let temp = TempDir::new().unwrap();
        let bad_magic_path = temp.path().join("bad-magic.bin");
        let mut bad_magic = Vec::new();
        bad_magic.extend_from_slice(b"NOPEIDX\0");
        bad_magic.extend_from_slice(&INDEX_VERSION.to_be_bytes());
        fs::write(&bad_magic_path, bad_magic).unwrap();

        let err = load_profiled(&bad_magic_path).unwrap_err();
        assert!(
            matches!(err, IndexError::InvalidFormat(message) if message.contains("missing magic"))
        );

        let bad_version_path = temp.path().join("bad-version.bin");
        let mut bad_version = Vec::new();
        bad_version.extend_from_slice(INDEX_MAGIC);
        bad_version.extend_from_slice(&(INDEX_VERSION + 1).to_be_bytes());
        fs::write(&bad_version_path, bad_version).unwrap();

        let err = load_profiled(&bad_version_path).unwrap_err();
        assert!(matches!(err, IndexError::VersionMismatch { expected, got }
                if expected == INDEX_VERSION && got == INDEX_VERSION + 1));
    }

    #[test]
    fn v6_snapshot_round_trips_untracked_directories() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("index.bin");

        let mut index = WorktreeIndex::new();
        index.insert_untracked_directory("scratch".to_string(), sample_untracked_directory_entry());

        let save_stats = save_snapshot_profiled(&index, &path).unwrap();
        let (loaded, load_stats) = load_profiled(&path).unwrap();

        assert!(save_stats.compacted);
        assert!(load_stats.snapshot_bytes > 0);
        assert_eq!(load_stats.journal_ops, 0);
        assert_eq!(loaded.len(), 0);
        assert_eq!(loaded.directory_len(), 0);
        assert_eq!(loaded.untracked_directory_len(), 1);
        let entry = loaded
            .get_untracked_directory("scratch")
            .expect("untracked directory entry should survive v6 roundtrip");
        assert_eq!(
            entry.added_paths,
            vec!["nested/one.txt".to_string(), "nested/two.txt".to_string()]
        );
        assert_eq!(
            entry.ignore_fingerprint,
            sample_untracked_directory_entry().ignore_fingerprint
        );
    }

    #[test]
    fn v6_hot_load_reads_directory_proofs_without_file_table() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("index.bin");
        let mut index = WorktreeIndex::new();
        for name in ["a.txt", "b.txt"] {
            index.insert(name.to_string(), sample_file_entry(name));
        }
        let root_hash = ContentHash::compute_typed("tree", b"root");
        index.insert_directory(
            String::new(),
            DirectoryCacheEntry {
                mtime_sec: 1,
                mtime_nsec: 2,
                child_count: 2,
                child_digest: ContentHash::compute_typed("dirnames", b"a.txt\0b.txt"),
                clean_tree_hash: Some(root_hash),
            },
        );
        index.insert_directory(
            "unchanged-sibling".to_string(),
            DirectoryCacheEntry {
                mtime_sec: 3,
                mtime_nsec: 4,
                child_count: 0,
                child_digest: ContentHash::compute_typed("dirnames", b""),
                clean_tree_hash: Some(ContentHash::compute_typed("tree", b"sibling")),
            },
        );
        index.set_gitlinks_tree(root_hash);
        index.insert_gitlink(
            "vendor/library".to_string(),
            "0123456789012345678901234567890123456789".to_string(),
        );
        save_snapshot_profiled(&index, &path).unwrap();

        let (mut hot, _) =
            load_hot_profiled_for_directories(&path, &BTreeSet::from([String::new()])).unwrap();
        assert_eq!(hot.len(), 0);
        assert_eq!(hot.directory_len(), 1);
        assert!(hot.get_directory("unchanged-sibling").is_none());
        assert_eq!(
            hot.gitlinks().get("vendor/library").map(String::as_str),
            Some("0123456789012345678901234567890123456789")
        );
        assert!(hot.is_hot_loaded());
        let (full, _) = load_profiled(&path).unwrap();
        assert_eq!(full.len(), 2);
        assert_eq!(full.directory_len(), 2);
        assert!(!full.is_hot_loaded());

        hot.insert(
            "journal-only.txt".to_string(),
            sample_file_entry("journal-only.txt"),
        );
        save_profiled(&hot, &path).unwrap();
        let (hot_after_journal, hot_stats) =
            load_hot_profiled_for_directories(&path, &BTreeSet::from([String::new()])).unwrap();
        assert!(hot_stats.journal_bytes > 0);
        assert_eq!(hot_stats.journal_ops, 0);
        assert_eq!(hot_stats.journal_replay_ms, 0);
        assert!(hot_after_journal.get("journal-only.txt").is_none());
        assert!(hot_after_journal.get_directory("").is_some());
        let (targeted_after_journal, targeted_stats) = load_hot_profiled_for_directories(
            &path,
            &BTreeSet::from([String::new(), "journal-only.txt".to_string()]),
        )
        .unwrap();
        assert!(targeted_stats.journal_ops > 0);
        assert!(targeted_after_journal.get("journal-only.txt").is_some());
        let (full_after_journal, full_stats) = load_profiled(&path).unwrap();
        assert!(full_stats.journal_ops > 0);
        assert!(full_after_journal.get("journal-only.txt").is_some());

        fs::write(hot_directory_record_path(&path, ""), b"corrupt").unwrap();
        let (self_healed, _) =
            load_hot_profiled_for_directories(&path, &BTreeSet::from([String::new()])).unwrap();
        assert!(self_healed.get_directory("").is_none());
    }

    #[test]
    fn load_profiled_rejects_malformed_entry_type_for_declared_section() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("index.bin");

        write_raw_v5_snapshot(&path, 1, 0, 0, &[IndexEntryType::Directory.to_u8()]);

        let err = load_profiled(&path).unwrap_err();
        assert!(
            matches!(err, IndexError::InvalidFormat(message) if message.contains("expected file entry"))
        );
    }

    #[test]
    fn load_profiled_rejects_truncated_compact_file_entry_payload() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("index.bin");

        write_raw_v5_snapshot(&path, 1, 0, 0, &[IndexEntryType::File.to_u8()]);

        let err = load_profiled(&path).unwrap_err();
        assert!(matches!(err, IndexError::InvalidFormat(message)
            if message.contains("truncated path len")
                || message.contains("truncated compact file path length")
                || message.contains("truncated journal file path length")));
    }

    #[test]
    fn journal_checksum_corruption_invalidates_the_whole_index() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("index.bin");
        let journal_path = journal_path(&path);

        let mut index = WorktreeIndex::new();
        index.insert("base.txt".to_string(), sample_file_entry("base"));
        save_profiled(&index, &path).unwrap();
        index.mark_clean();

        index.insert("good.txt".to_string(), sample_file_entry("good"));
        save_profiled(&index, &path).unwrap();
        index.mark_clean();

        index.insert("tail.txt".to_string(), sample_file_entry("tail"));
        save_profiled(&index, &path).unwrap();
        index.mark_clean();

        let mut journal = fs::read(&journal_path).unwrap();
        let first_frame_len = u32::from_be_bytes(journal[12..16].try_into().unwrap()) as usize;
        let second_frame_start = 12 + 8 + first_frame_len;
        assert!(
            journal.len() >= second_frame_start + 8,
            "test fixture must contain a second journal frame"
        );
        journal[second_frame_start + 4] ^= 0xFF;
        fs::write(&journal_path, journal).unwrap();

        assert!(matches!(
            load_profiled(&path),
            Err(IndexError::ChecksumMismatch)
        ));
    }
}
