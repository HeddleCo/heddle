// SPDX-License-Identifier: Apache-2.0
//! Compare a working-copy path against its indexed Git OID + mode.
//!
//! Used by every codepath that wants `git status`-equivalent dirtiness
//! for a single index entry: the plain-Git status probe in
//! `cli::commands::git_overlay_health`, the overlay status probe in
//! `repository`, and the worktree-vs-index check in
//! `cli::commands::git_compat`. Each used to carry its own near-
//! identical re-implementation, which drifted (the cli/repo copies
//! lost type-change checks the git_compat copy had, the git_compat
//! copy lost the chmod check the cli/repo copies needed). This module
//! is the single source of truth so future fixes land once.
//!
//! Semantics mirror `git status --porcelain=v1` for a single index
//! entry:
//!
//! - Path missing from the worktree → [`GitWorktreeEntryState::Deleted`].
//! - Indexed type doesn't match worktree type (file ↔ symlink, etc.)
//!   → [`GitWorktreeEntryState::Modified`]. Git would report this as
//!   a type-change.
//! - Submodule (`160000`) indexed as a directory present → `Clean`,
//!   else `Modified` (the actual SHA inside the submodule isn't
//!   inspected here — that's the caller's job).
//! - Symlink indexed and present: link target read as raw bytes (NOT
//!   `to_string_lossy`, which mangles non-UTF-8 paths), hashed,
//!   compared to `expected_oid`.
//! - Regular file: on Unix, the worktree exec bit is compared against
//!   the indexed mode (`100644` vs `100755`) so a `chmod +x` flip is
//!   caught — git porcelain v1 reports this as ` M f` and porcelain v2
//!   carries it in the `<mH>` mode field. Then bytes are hashed and
//!   compared to `expected_oid`.
//!
//! On platforms without an executable bit (Windows), the exec-bit
//! comparison is skipped — git itself ignores `core.filemode` there.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use git_substrate::{blob_object_id, IndexStatProbe, TrackedIndexEntry};
use objects::error::{HeddleError, Result};

pub use git_substrate::IndexCachedStat;

const GIT_MODE_REGULAR: u32 = 0o100644;
const GIT_MODE_EXECUTABLE: u32 = 0o100755;
const GIT_MODE_SYMLINK: u32 = 0o120000;
const GIT_MODE_COMMIT: u32 = 0o160000;

/// State of a single index entry relative to the worktree, mirroring
/// the three outcomes `git status` cares about for a known-tracked path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitWorktreeEntryState {
    /// Worktree matches the indexed blob.
    Clean,
    /// Worktree differs (content, mode, type, or submodule presence).
    Modified,
    /// Worktree path no longer exists.
    Deleted,
}

/// Compare the worktree file at `root/path` to its indexed
/// `expected_oid` + `mode` (a raw Git file mode such as `0o100644`).
/// See the module-level docs for the rules.
pub fn git_worktree_entry_state(
    root: &Path,
    path: &str,
    expected_oid: &git_substrate::ObjectId,
    mode: u32,
    index_probe: Option<IndexStatProbe>,
) -> Result<GitWorktreeEntryState> {
    let absolute = root.join(path);
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(GitWorktreeEntryState::Deleted);
        }
        Err(error) => return Err(error.into()),
    };
    let file_type = metadata.file_type();

    if mode == GIT_MODE_COMMIT {
        return Ok(if file_type.is_dir() {
            GitWorktreeEntryState::Clean
        } else {
            GitWorktreeEntryState::Modified
        });
    }

    if mode == GIT_MODE_SYMLINK {
        if !file_type.is_symlink() {
            return Ok(GitWorktreeEntryState::Modified);
        }
        let target = fs::read_link(&absolute)?;
        let target_bytes = objects::util::symlink_target_bytes(&target);
        return hash_and_compare(expected_oid, target_bytes.as_ref());
    }

    if file_type.is_symlink() || file_type.is_dir() {
        return Ok(GitWorktreeEntryState::Modified);
    }

    #[cfg(unix)]
    if matches!(mode, GIT_MODE_REGULAR | GIT_MODE_EXECUTABLE) {
        use std::os::unix::fs::PermissionsExt;
        let worktree_executable = metadata.permissions().mode() & 0o111 != 0;
        let indexed_executable = mode == GIT_MODE_EXECUTABLE;
        if worktree_executable != indexed_executable {
            return Ok(GitWorktreeEntryState::Modified);
        }
    }
    #[cfg(not(unix))]
    let _ = (GIT_MODE_REGULAR, GIT_MODE_EXECUTABLE);

    if let Some(probe) = index_probe
        && probe.proves_clean(&absolute)
    {
        return Ok(GitWorktreeEntryState::Clean);
    }

    let bytes = fs::read(&absolute)?;
    hash_and_compare(expected_oid, bytes.as_ref())
}

/// Index-vs-HEAD staging signals plus worktree-vs-index dirtiness for tracked paths.
pub fn collect_index_head_worktree_changes(
    root: &Path,
    index_timestamp_secs: i64,
    index_entries: &BTreeMap<String, TrackedIndexEntry>,
    head_entries: &BTreeMap<String, (git_substrate::ObjectId, u32)>,
    include_path: impl Fn(&str) -> bool,
) -> Result<(BTreeSet<PathBuf>, BTreeSet<PathBuf>, BTreeSet<PathBuf>)> {
    let mut added = BTreeSet::new();
    let mut modified = BTreeSet::new();
    let mut deleted = BTreeSet::new();

    for (path, entry) in index_entries {
        if !include_path(path) {
            continue;
        }
        match head_entries.get(path) {
            None => {
                added.insert(PathBuf::from(path));
            }
            Some((head_oid, head_mode))
                if *head_mode != entry.mode || head_oid != &entry.oid =>
            {
                modified.insert(PathBuf::from(path));
            }
            Some(_) => {}
        }
    }
    for path in head_entries.keys() {
        if include_path(path) && !index_entries.contains_key(path) {
            deleted.insert(PathBuf::from(path));
        }
    }

    for (path, entry) in index_entries {
        if !include_path(path) {
            continue;
        }
        let probe = IndexStatProbe {
            stat: entry.stat,
            index_timestamp_secs,
        };
        match git_worktree_entry_state(root, path, &entry.oid, entry.mode, Some(probe))? {
            GitWorktreeEntryState::Clean => {}
            GitWorktreeEntryState::Deleted => {
                deleted.insert(PathBuf::from(path));
            }
            GitWorktreeEntryState::Modified => {
                modified.insert(PathBuf::from(path));
            }
        }
    }

    Ok((added, modified, deleted))
}

fn hash_and_compare(
    expected_oid: &git_substrate::ObjectId,
    bytes: &[u8],
) -> Result<GitWorktreeEntryState> {
    let actual_oid = blob_object_id(bytes).map_err(|error| {
        HeddleError::Config(format!("failed to hash worktree path: {error}"))
    })?;
    Ok(if actual_oid == *expected_oid {
        GitWorktreeEntryState::Clean
    } else {
        GitWorktreeEntryState::Modified
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use git_substrate::index_cached_stat_from_path;

    use super::*;

    fn write_blob_hash(bytes: &[u8]) -> git_substrate::ObjectId {
        blob_object_id(bytes).expect("hash bytes")
    }

    #[test]
    fn missing_path_is_deleted() {
        let temp = tempfile::TempDir::new().unwrap();
        let oid = write_blob_hash(b"anything");
        let state = git_worktree_entry_state(temp.path(), "nope.txt", &oid, GIT_MODE_REGULAR, None)
            .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Deleted);
    }

    #[test]
    fn ancestor_turned_into_file_is_deleted() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::write(temp.path().join("data"), b"now a file").unwrap();
        let oid = write_blob_hash(b"x\ny\n");
        let state = git_worktree_entry_state(temp.path(), "data/item.txt", &oid, GIT_MODE_REGULAR, None)
            .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Deleted);
    }

    #[test]
    fn identical_content_is_clean() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::write(temp.path().join("a.txt"), b"hello").unwrap();
        let oid = write_blob_hash(b"hello");
        let state =
            git_worktree_entry_state(temp.path(), "a.txt", &oid, GIT_MODE_REGULAR, None).expect("call");
        assert_eq!(state, GitWorktreeEntryState::Clean);
    }

    #[test]
    fn changed_content_is_modified() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::write(temp.path().join("a.txt"), b"new content").unwrap();
        let oid = write_blob_hash(b"old content");
        let state =
            git_worktree_entry_state(temp.path(), "a.txt", &oid, GIT_MODE_REGULAR, None).expect("call");
        assert_eq!(state, GitWorktreeEntryState::Modified);
    }

    #[cfg(unix)]
    #[test]
    fn chmod_only_change_is_modified() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("script.sh");
        fs::write(&path, b"#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let oid = write_blob_hash(b"#!/bin/sh\necho hi\n");
        let state = git_worktree_entry_state(temp.path(), "script.sh", &oid, GIT_MODE_REGULAR, None)
            .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Modified);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let state = git_worktree_entry_state(temp.path(), "script.sh", &oid, GIT_MODE_EXECUTABLE, None)
            .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Modified);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let state = git_worktree_entry_state(temp.path(), "script.sh", &oid, GIT_MODE_EXECUTABLE, None)
            .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Clean);
    }

    #[cfg(unix)]
    #[test]
    fn typechange_file_to_symlink_is_modified() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("target.txt");
        fs::write(&target, b"target").unwrap();
        let link = temp.path().join("link");
        symlink(&target, &link).unwrap();
        let oid = write_blob_hash(b"anything");
        let state =
            git_worktree_entry_state(temp.path(), "link", &oid, GIT_MODE_REGULAR, None).expect("call");
        assert_eq!(state, GitWorktreeEntryState::Modified);
    }

    #[cfg(unix)]
    #[test]
    fn typechange_symlink_to_file_is_modified() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::write(temp.path().join("link"), b"now a file").unwrap();
        let oid = write_blob_hash(b"old target");
        let state =
            git_worktree_entry_state(temp.path(), "link", &oid, GIT_MODE_SYMLINK, None).expect("call");
        assert_eq!(state, GitWorktreeEntryState::Modified);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_with_non_utf8_target_compares_via_raw_bytes() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt, os::unix::fs::symlink};
        let temp = tempfile::TempDir::new().unwrap();
        let raw_target = b"target-\xff-bytes";
        let target_os = OsStr::from_bytes(raw_target);
        let link = temp.path().join("link");
        symlink(target_os, &link).unwrap();
        let oid = write_blob_hash(raw_target);
        let state = git_worktree_entry_state(temp.path(), "link", &oid, GIT_MODE_SYMLINK, None)
            .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Clean);
    }

    #[cfg(unix)]
    #[test]
    fn stat_fastpath_skips_hash_when_unchanged() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("a.txt");
        fs::write(&path, b"hello").unwrap();
        let stat = index_cached_stat_from_path(&path).expect("stat");
        let probe = IndexStatProbe {
            stat,
            index_timestamp_secs: i64::from(stat.mtime_secs) + 5,
        };
        let wrong_oid = write_blob_hash(b"totally different bytes");
        let state =
            git_worktree_entry_state(temp.path(), "a.txt", &wrong_oid, GIT_MODE_REGULAR, Some(probe))
                .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Clean);
    }

    #[cfg(unix)]
    #[test]
    fn racy_window_falls_back_to_hashing() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("a.txt");
        fs::write(&path, b"hello").unwrap();
        let stat = index_cached_stat_from_path(&path).expect("stat");
        let probe = IndexStatProbe {
            stat,
            index_timestamp_secs: i64::from(stat.mtime_secs),
        };
        let stale_oid = write_blob_hash(b"totally different bytes");
        let state =
            git_worktree_entry_state(temp.path(), "a.txt", &stale_oid, GIT_MODE_REGULAR, Some(probe))
                .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Modified);
    }

    #[cfg(unix)]
    #[test]
    fn stat_mismatch_falls_back_to_hashing() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("a.txt");
        fs::write(&path, b"hello").unwrap();
        let mut stat = index_cached_stat_from_path(&path).expect("stat");
        let probe = IndexStatProbe {
            stat: {
                stat.size = stat.size.wrapping_add(1);
                stat
            },
            index_timestamp_secs: i64::from(stat.mtime_secs) + 5,
        };
        let old_oid = write_blob_hash(b"world");
        let state =
            git_worktree_entry_state(temp.path(), "a.txt", &old_oid, GIT_MODE_REGULAR, Some(probe))
                .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Modified);
    }
}