// SPDX-License-Identifier: Apache-2.0
//! Git index stat cache and entry helpers (#597 P2).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

use sley_core::ObjectFormat;
use sley_index::{Index, IndexEntry};
use sley_worktree::read_repository_index;

use crate::worktree::index_path;
use crate::{GitSubstrateError, Result};

pub const INDEX_FLAG_EXTENDED: u16 = 0x4000;
pub const INDEX_FLAG_INTENT_TO_ADD: u16 = 0x2000;
const INDEX_STAGE_SHIFT: u16 = 12;

/// Cached stat fields stored in a git index entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexCachedStat {
    pub mtime_secs: u32,
    pub mtime_nsec: u32,
    pub ctime_secs: u32,
    pub ctime_nsec: u32,
    pub dev: u32,
    pub ino: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
}

/// Index-recorded stat plus the index file mtime for git's racy-clean window.
#[derive(Debug, Clone, Copy)]
pub struct IndexStatProbe {
    pub stat: IndexCachedStat,
    pub index_timestamp_secs: i64,
}

/// One stage-0 tracked path from the on-disk index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedIndexEntry {
    pub oid: sley_core::ObjectId,
    pub mode: u32,
    pub stat: IndexCachedStat,
}

impl IndexCachedStat {
    pub fn from_index_entry(entry: &IndexEntry) -> Self {
        Self {
            mtime_secs: entry.mtime_seconds,
            mtime_nsec: entry.mtime_nanoseconds,
            ctime_secs: entry.ctime_seconds,
            ctime_nsec: entry.ctime_nanoseconds,
            dev: entry.dev,
            ino: entry.ino,
            uid: entry.uid,
            gid: entry.gid,
            size: entry.size,
        }
    }

    /// Whether `other` matches this cached stat the way git's index compare does
    /// with `trust_ctime` enabled (Linux default).
    pub fn matches_worktree(&self, other: &Self) -> bool {
        self.mtime_secs == other.mtime_secs
            && self.size == other.size
            && self.ctime_secs == other.ctime_secs
    }
}

impl IndexStatProbe {
    /// Whether the worktree file at `absolute` is provably unchanged versus the
    /// index — stat matches and the entry is outside the racy-clean window.
    pub fn proves_clean(&self, absolute: &Path) -> bool {
        let Some(worktree_stat) = index_cached_stat_from_path(absolute) else {
            return false;
        };
        if !self.stat.matches_worktree(&worktree_stat) {
            return false;
        }
        self.index_timestamp_secs > i64::from(self.stat.mtime_secs)
    }
}

/// Merge stage encoded in the index entry flags (0 = unconflicted).
pub fn index_entry_stage(flags: u16) -> u16 {
    (flags >> INDEX_STAGE_SHIFT) & 0b11
}

/// Whether the entry carries git's `CE_INTENT_TO_ADD` extended flag.
pub fn index_entry_is_intent_to_add(entry: &IndexEntry) -> bool {
    entry.flags_extended & INDEX_FLAG_INTENT_TO_ADD != 0
}

/// Read the repository index when present.
pub fn read_disk_index(git_dir: &Path, format: ObjectFormat) -> Result<Option<Index>> {
    read_repository_index(git_dir, format).map_err(GitSubstrateError::from)
}

/// Whole seconds since epoch for the index file's mtime.
pub fn index_file_mtime_secs(git_dir: &Path) -> Option<i64> {
    let metadata = std::fs::metadata(index_path(git_dir)).ok()?;
    metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
}

/// Stage-0 tracked entries keyed by repository-relative path (`/` separators).
pub fn tracked_index_entries(index: &Index) -> BTreeMap<String, TrackedIndexEntry> {
    let mut entries = BTreeMap::new();
    for entry in &index.entries {
        if index_entry_stage(entry.flags) != 0 {
            continue;
        }
        let path = String::from_utf8_lossy(&entry.path).into_owned();
        entries.insert(
            path,
            TrackedIndexEntry {
                oid: entry.oid.clone(),
                mode: entry.mode,
                stat: IndexCachedStat::from_index_entry(entry),
            },
        );
    }
    entries
}

/// Tree-shaped index entries (oid + mode) keyed by path.
pub fn tree_index_entry_map(index: &Index) -> BTreeMap<String, (sley_core::ObjectId, u32)> {
    index
        .entries
        .iter()
        .map(|entry| {
            let path = String::from_utf8_lossy(&entry.path).into_owned();
            (path, (entry.oid.clone(), entry.mode))
        })
        .collect()
}

/// `lstat` the worktree path into git's cached stat shape.
pub fn index_cached_stat_from_path(path: &Path) -> Option<IndexCachedStat> {
    stat_from_path(path)
}

#[cfg(unix)]
fn stat_from_path(path: &Path) -> Option<IndexCachedStat> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    let c_path = CString::new(bytes).ok()?;
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::lstat(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let (mtime_secs, mtime_nsec, ctime_secs, ctime_nsec) = stat_times(&stat);
    Some(IndexCachedStat {
        mtime_secs,
        mtime_nsec,
        ctime_secs,
        ctime_nsec,
        dev: stat.st_dev as u32,
        ino: stat.st_ino as u32,
        uid: stat.st_uid,
        gid: stat.st_gid,
        size: stat.st_size as u32,
    })
}

#[cfg(unix)]
fn stat_times(stat: &libc::stat) -> (u32, u32, u32, u32) {
    #[cfg(target_os = "linux")]
    {
        (
            stat.st_mtime as u32,
            stat.st_mtim.tv_nsec as u32,
            stat.st_ctime as u32,
            stat.st_ctim.tv_nsec as u32,
        )
    }
    #[cfg(target_os = "macos")]
    {
        (
            stat.st_mtime as u32,
            stat.st_mtime_nsec as u32,
            stat.st_ctime as u32,
            stat.st_ctime_nsec as u32,
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        (
            stat.st_mtime as u32,
            0,
            stat.st_ctime as u32,
            0,
        )
    }
}

#[cfg(not(unix))]
fn stat_from_path(_path: &Path) -> Option<IndexCachedStat> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::empty_tree_sha1;
    use sley_index::Index;
    use tempfile::TempDir;

    #[test]
    fn tracked_index_entries_skip_conflict_stages() {
        let oid = empty_tree_sha1();
        let index = Index {
            version: 2,
            entries: vec![
                IndexEntry {
                    ctime_seconds: 0,
                    ctime_nanoseconds: 0,
                    mtime_seconds: 0,
                    mtime_nanoseconds: 0,
                    dev: 0,
                    ino: 0,
                    mode: 0o100644,
                    uid: 0,
                    gid: 0,
                    size: 0,
                    oid: oid.clone(),
                    flags: 4,
                    flags_extended: 0,
                    path: b"a.txt".into(),
                },
                IndexEntry {
                    ctime_seconds: 0,
                    ctime_nanoseconds: 0,
                    mtime_seconds: 0,
                    mtime_nanoseconds: 0,
                    dev: 0,
                    ino: 0,
                    mode: 0o100644,
                    uid: 0,
                    gid: 0,
                    size: 0,
                    oid,
                    flags: (1 << INDEX_STAGE_SHIFT) | 4,
                    flags_extended: 0,
                    path: b"b.txt".into(),
                },
            ],
            extensions: Vec::new(),
            checksum: None,
        };
        let tracked = tracked_index_entries(&index);
        assert_eq!(tracked.len(), 1);
        assert!(tracked.contains_key("a.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn stat_probe_skips_hash_when_provably_clean() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("a.txt");
        std::fs::write(&path, b"hello").expect("write");
        let stat = index_cached_stat_from_path(&path).expect("stat");
        let probe = IndexStatProbe {
            stat,
            index_timestamp_secs: i64::from(stat.mtime_secs) + 5,
        };
        assert!(probe.proves_clean(&path));
    }
}