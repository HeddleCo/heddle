// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    path::Path,
    time::Instant,
};

use objects::object::ContentHash;
use tracing::{debug, warn};

use super::{
    DirectoryCacheEntry, HEADER_SIZE_V4, HEADER_SIZE_V5, INDEX_MAGIC, INDEX_VERSION, IndexEntry,
    IndexEntryKind, IndexError, MAX_JOURNAL_REPLAY_MS_BEFORE_COMPACT, UntrackedDirectoryCacheEntry,
    WorktreeIndex, WorktreeIndexLoadStats, WorktreeIndexSaveStats,
};

const JOURNAL_MAGIC: &[u8; 8] = super::JOURNAL_MAGIC;
const JOURNAL_VERSION: u32 = super::JOURNAL_VERSION;
const MAX_JOURNAL_OPS_BEFORE_COMPACT: usize = super::MAX_JOURNAL_OPS_BEFORE_COMPACT;
const MAX_JOURNAL_BYTES_BEFORE_COMPACT: u64 = super::MAX_JOURNAL_BYTES_BEFORE_COMPACT;

fn read_u32_be(bytes: &[u8], context: &str) -> Result<u32, IndexError> {
    let array: [u8; 4] = bytes
        .try_into()
        .map_err(|_| IndexError::InvalidFormat(format!("truncated {context}")))?;
    Ok(u32::from_be_bytes(array))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexEntryType {
    File = 0x01,
    Directory = 0x02,
    UntrackedDirectory = 0x03,
}

impl IndexEntryType {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(Self::File),
            0x02 => Some(Self::Directory),
            0x03 => Some(Self::UntrackedDirectory),
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
}

pub(crate) fn load(path: &Path) -> Result<WorktreeIndex, IndexError> {
    load_profiled(path).map(|(index, _)| index)
}

pub(crate) fn load_profiled(
    path: &Path,
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
        5 if file_size >= HEADER_SIZE_V5 as u64 + 4 => load_v5(&mut file, file_size),
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
                stats.journal_replay_ms = replay_start.elapsed().as_millis();
                warn!(
                    journal_path = %journal_path.display(),
                    %error,
                    "Ignoring unreadable worktree index journal"
                );
                0
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

    let journal_path = journal_path(path);
    let journal_exists = journal_path.exists();
    let journal_len = journal_path
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let compact_reason = if !path.exists() {
        Some("missing_snapshot")
    } else if index.pending_ops.len() > MAX_JOURNAL_OPS_BEFORE_COMPACT {
        Some("pending_ops")
    } else if journal_len > MAX_JOURNAL_BYTES_BEFORE_COMPACT {
        Some("journal_bytes")
    } else if index.last_journal_replay_ms() > MAX_JOURNAL_REPLAY_MS_BEFORE_COMPACT {
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

        let path_len = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
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

        let size = u64::from_be_bytes(data[offset..offset + 8].try_into().unwrap());
        offset += 8;

        let modified_sec = i64::from_be_bytes(data[offset..offset + 8].try_into().unwrap());
        offset += 8;

        let modified_nsec = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());
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
        dirty: false,
        pending_ops: Vec::new(),
        last_journal_bytes: 0,
        last_journal_ops: 0,
        last_journal_replay_ms: 0,
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
        dirty: false,
        pending_ops: Vec::new(),
        last_journal_bytes: 0,
        last_journal_ops: 0,
        last_journal_replay_ms: 0,
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
        dirty: false,
        pending_ops: Vec::new(),
        last_journal_bytes: 0,
        last_journal_ops: 0,
        last_journal_replay_ms: 0,
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
        dirty: false,
        pending_ops: Vec::new(),
        last_journal_bytes: 0,
        last_journal_ops: 0,
        last_journal_replay_ms: 0,
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

    let path_len = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
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

    let size = u64::from_be_bytes(data[offset..offset + 8].try_into().unwrap());
    offset += 8;

    let modified_sec = i64::from_be_bytes(data[offset..offset + 8].try_into().unwrap());
    offset += 8;

    let modified_nsec = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());
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

    let path_len = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
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

    let mtime_sec = i64::from_be_bytes(data[offset..offset + 8].try_into().unwrap());
    offset += 8;

    let mtime_nsec = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());
    offset += 4;

    let child_count = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());
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

    let children_len = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
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

    let path_len = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
    offset += 4;

    if offset + path_len + 8 + 4 + 4 + 32 + 1 + 32 > data.len() {
        return Err(IndexError::InvalidFormat(
            "truncated directory entry data".to_string(),
        ));
    }

    let path = String::from_utf8(data[offset..offset + path_len].to_vec())
        .map_err(|_| IndexError::InvalidUtf8(format!("path at offset {}", offset)))?;
    offset += path_len;

    let mtime_sec = i64::from_be_bytes(data[offset..offset + 8].try_into().unwrap());
    offset += 8;
    let mtime_nsec = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());
    offset += 4;
    let child_count = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());
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
    let path_len = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
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

fn write_snapshot(index: &WorktreeIndex, path: &Path) -> Result<(), IndexError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut entry_data = Vec::new();

    for (path, entry) in &index.entries {
        let path_bytes = path.as_bytes();
        entry_data.reserve_exact(1 + 4 + path_bytes.len() + 32 + 8 + 8 + 4 + 1 + 1);

        entry_data.push(IndexEntryType::File.to_u8());
        entry_data.extend_from_slice(&(path_bytes.len() as u32).to_be_bytes());
        entry_data.extend_from_slice(path_bytes);
        entry_data.extend_from_slice(entry.hash.as_bytes());
        entry_data.extend_from_slice(&entry.size.to_be_bytes());
        entry_data.extend_from_slice(&entry.modified_sec.to_be_bytes());
        entry_data.extend_from_slice(&entry.modified_nsec.to_be_bytes());
        entry_data.push(if entry.executable { 1 } else { 0 });
        entry_data.push(entry.kind.to_u8());
    }

    for (path, dir) in &index.directories {
        let path_bytes = path.as_bytes();
        entry_data.reserve_exact(1 + 4 + path_bytes.len() + 8 + 4 + 4 + 32 + 1 + 32);

        entry_data.push(IndexEntryType::Directory.to_u8());
        entry_data.extend_from_slice(&(path_bytes.len() as u32).to_be_bytes());
        entry_data.extend_from_slice(path_bytes);
        entry_data.extend_from_slice(&dir.mtime_sec.to_be_bytes());
        entry_data.extend_from_slice(&dir.mtime_nsec.to_be_bytes());
        entry_data.extend_from_slice(&dir.child_count.to_be_bytes());
        entry_data.extend_from_slice(dir.child_digest.as_bytes());
        entry_data.push(u8::from(dir.clean_tree_hash.is_some()));
        entry_data.extend_from_slice(
            dir.clean_tree_hash
                .as_ref()
                .map(ContentHash::as_bytes)
                .unwrap_or(&[0; 32]),
        );
    }

    for (path, dir) in &index.untracked_directories {
        let path_bytes = path.as_bytes();
        entry_data.push(IndexEntryType::UntrackedDirectory.to_u8());
        entry_data.extend_from_slice(&(path_bytes.len() as u32).to_be_bytes());
        entry_data.extend_from_slice(path_bytes);
        write_untracked_directory_entry_payload(&mut entry_data, dir)?;
    }

    let checksum = crc32(&entry_data);

    let mut file_data = Vec::with_capacity(HEADER_SIZE_V5 + entry_data.len() + 4);
    file_data.extend_from_slice(INDEX_MAGIC);
    file_data.extend_from_slice(&INDEX_VERSION.to_be_bytes());
    file_data.extend_from_slice(&(index.entries.len() as u32).to_be_bytes());
    file_data.extend_from_slice(&(index.directories.len() as u32).to_be_bytes());
    file_data.extend_from_slice(&(index.untracked_directories.len() as u32).to_be_bytes());
    file_data.extend_from_slice(&entry_data);
    file_data.extend_from_slice(&checksum.to_be_bytes());

    let mut temp_file = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
    temp_file.write_all(&file_data)?;
    temp_file.flush()?;
    let (_file, temp_path) = temp_file
        .keep()
        .map_err(|error| IndexError::Io(error.error))?;
    fs::rename(&temp_path, path)?;

    Ok(())
}

fn append_journal(index: &WorktreeIndex, journal_path: &Path) -> Result<(), IndexError> {
    if let Some(parent) = journal_path.parent() {
        fs::create_dir_all(parent)?;
    }

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
            warn!(
                journal_path = %journal_path.display(),
                "Stopping worktree index journal replay at corrupt frame"
            );
            break;
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
    let path_len = u32::from_be_bytes(data[*offset..*offset + 4].try_into().unwrap()) as usize;
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
    let size = u64::from_be_bytes(data[*offset..*offset + 8].try_into().unwrap());
    *offset += 8;
    let modified_sec = i64::from_be_bytes(data[*offset..*offset + 8].try_into().unwrap());
    *offset += 8;
    let modified_nsec = u32::from_be_bytes(data[*offset..*offset + 4].try_into().unwrap());
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
    let mtime_sec = i64::from_be_bytes(data[*offset..*offset + 8].try_into().unwrap());
    *offset += 8;
    let mtime_nsec = u32::from_be_bytes(data[*offset..*offset + 4].try_into().unwrap());
    *offset += 4;
    let child_count = u32::from_be_bytes(data[*offset..*offset + 4].try_into().unwrap());
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
    let mtime_sec = i64::from_be_bytes(data[*offset..*offset + 8].try_into().unwrap());
    *offset += 8;
    let mtime_nsec = u32::from_be_bytes(data[*offset..*offset + 4].try_into().unwrap());
    *offset += 4;
    let child_count = u32::from_be_bytes(data[*offset..*offset + 4].try_into().unwrap());
    *offset += 4;
    let mut child_digest_bytes = [0u8; 32];
    child_digest_bytes.copy_from_slice(&data[*offset..*offset + 32]);
    *offset += 32;
    let child_digest = ContentHash::from_bytes(child_digest_bytes);
    let mut ignore_fingerprint_bytes = [0u8; 32];
    ignore_fingerprint_bytes.copy_from_slice(&data[*offset..*offset + 32]);
    *offset += 32;
    let ignore_fingerprint = ContentHash::from_bytes(ignore_fingerprint_bytes);
    let added_path_count = u32::from_be_bytes(data[*offset..*offset + 4].try_into().unwrap());
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
}