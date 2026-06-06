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

use std::{fs, io, path::Path};

use objects::error::{HeddleError, Result};

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

/// The index-recorded stat for a tracked path plus the index's own write
/// timestamp (the `.git/index` file mtime, in whole seconds since epoch).
///
/// Passed into [`git_worktree_entry_state`] to enable git's stat fast-path:
/// when the worktree file's stat still matches the index's cached stat — and
/// the entry isn't inside the racy window — the content is provably unchanged
/// and the (10k-file) read+SHA-1 is skipped entirely, exactly like
/// `git status`. Anything uncertain falls back to hashing.
#[derive(Debug, Clone, Copy)]
pub struct IndexStatProbe {
    /// The stat git cached for this entry the last time it refreshed the index.
    pub stat: gix::index::entry::Stat,
    /// The index file's own mtime, in whole seconds since the unix epoch.
    pub index_timestamp_secs: i64,
}

impl IndexStatProbe {
    /// Whether the worktree file at `absolute` is *provably* unchanged versus
    /// the index — i.e. its stat matches the cached stat and it predates the
    /// index's write time (so it's outside the racy-clean window). Returns
    /// `false` on any uncertainty (stat mismatch, racy window, stat
    /// unavailable), leaving the caller to read + hash.
    fn proves_clean(&self, absolute: &Path) -> bool {
        let Ok(metadata) = gix::index::fs::Metadata::from_path_no_follow(absolute) else {
            return false;
        };
        let Ok(worktree_stat) = gix::index::entry::Stat::from_fs(&metadata) else {
            return false;
        };
        if !self
            .stat
            .matches(&worktree_stat, gix::index::entry::stat::Options::default())
        {
            return false;
        }
        // Racy-clean guard, mirroring `gix::index::entry::Stat::is_racy` with
        // `use_nsec = false` (git's Linux default): an entry whose mtime is at
        // or after the index's own write time could have been modified within
        // the same second without the stat changing, so it must be hashed.
        // `is_racy` is true when `index_timestamp <= mtime_secs`.
        self.index_timestamp_secs > i64::from(self.stat.mtime.secs)
    }
}

/// Compare the worktree file at `root/path` to its indexed
/// `expected_oid` + `mode` (a raw Git file mode such as `0o100644`).
/// See the module-level docs for the rules.
pub fn git_worktree_entry_state(
    root: &Path,
    path: &str,
    expected_oid: gix::ObjectId,
    mode: u32,
    index_probe: Option<IndexStatProbe>,
) -> Result<GitWorktreeEntryState> {
    let absolute = root.join(path);
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        // `NotFound`: the path is simply gone. `NotADirectory`: an ancestor
        // is no longer a directory (e.g. tracked `data/item.txt` after `data`
        // became a regular file — a dir→file type change). In both cases the
        // indexed path cannot exist in the worktree, which is exactly what
        // `git status` reports as a deletion; the new file arrives as its own
        // untracked `added` entry.
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
        // Type-change: indexed as symlink but worktree isn't one.
        if !file_type.is_symlink() {
            return Ok(GitWorktreeEntryState::Modified);
        }
        let target = fs::read_link(&absolute)?;
        let target_bytes = objects::util::symlink_target_bytes(&target);
        return hash_and_compare(expected_oid, &target_bytes);
    }

    // Indexed as file but worktree is a symlink or directory → type-change.
    if file_type.is_symlink() || file_type.is_dir() {
        return Ok(GitWorktreeEntryState::Modified);
    }

    // Regular file: exec-bit comparison (Unix only) catches `chmod +x`
    // flips that leave blob bytes identical. Git porcelain v1 reports
    // these as ` M f`; hash-only comparison would falsely return Clean.
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

    // Stat fast-path: if the index's cached stat still matches the worktree
    // (and we're outside the racy window), the blob is unchanged — skip the
    // read + SHA-1. The exec-bit divergence above is already ruled out, and a
    // chmod bumps ctime, which `matches` (trust_ctime) catches, so this never
    // masks a mode flip.
    if let Some(probe) = index_probe
        && probe.proves_clean(&absolute)
    {
        return Ok(GitWorktreeEntryState::Clean);
    }

    let bytes = fs::read(&absolute)?;
    hash_and_compare(expected_oid, &bytes)
}

fn hash_and_compare(expected_oid: gix::ObjectId, bytes: &[u8]) -> Result<GitWorktreeEntryState> {
    let actual_oid = gix::objs::compute_hash(expected_oid.kind(), gix::objs::Kind::Blob, bytes)
        .map_err(|error| {
            HeddleError::Config(format!("failed to hash worktree path: {error}"))
        })?;
    Ok(if actual_oid == expected_oid {
        GitWorktreeEntryState::Clean
    } else {
        GitWorktreeEntryState::Modified
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write_blob_hash(bytes: &[u8]) -> gix::ObjectId {
        gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)
            .expect("hash bytes")
    }

    #[test]
    fn missing_path_is_deleted() {
        let temp = tempfile::TempDir::new().unwrap();
        let oid = write_blob_hash(b"anything");
        let state = git_worktree_entry_state(temp.path(), "nope.txt", oid, GIT_MODE_REGULAR, None)
            .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Deleted);
    }

    /// A tracked path whose ancestor became a regular file (a dir→file type
    /// change: `data/item.txt` after `data` is replaced by a file) raises
    /// `ENOTDIR`, not `NotFound`. The indexed path still cannot exist, so it
    /// must report `Deleted` rather than propagating an io error.
    #[test]
    fn ancestor_turned_into_file_is_deleted() {
        let temp = tempfile::TempDir::new().unwrap();
        // `data` is a regular file; `data/item.txt` therefore has a non-dir
        // ancestor and cannot be statted.
        fs::write(temp.path().join("data"), b"now a file").unwrap();
        let oid = write_blob_hash(b"x\ny\n");
        let state = git_worktree_entry_state(temp.path(), "data/item.txt", oid, GIT_MODE_REGULAR, None)
            .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Deleted);
    }

    #[test]
    fn identical_content_is_clean() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::write(temp.path().join("a.txt"), b"hello").unwrap();
        let oid = write_blob_hash(b"hello");
        let state =
            git_worktree_entry_state(temp.path(), "a.txt", oid, GIT_MODE_REGULAR, None).expect("call");
        assert_eq!(state, GitWorktreeEntryState::Clean);
    }

    #[test]
    fn changed_content_is_modified() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::write(temp.path().join("a.txt"), b"new content").unwrap();
        let oid = write_blob_hash(b"old content");
        let state =
            git_worktree_entry_state(temp.path(), "a.txt", oid, GIT_MODE_REGULAR, None).expect("call");
        assert_eq!(state, GitWorktreeEntryState::Modified);
    }

    /// The defect Codex flagged: a chmod-only change leaves blob bytes
    /// identical, so hash-only comparison would return Clean. The
    /// exec-bit comparison catches it.
    #[cfg(unix)]
    #[test]
    fn chmod_only_change_is_modified() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("script.sh");
        fs::write(&path, b"#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let oid = write_blob_hash(b"#!/bin/sh\necho hi\n");
        // Indexed as regular (no exec bit) but worktree has it → Modified.
        let state = git_worktree_entry_state(temp.path(), "script.sh", oid, GIT_MODE_REGULAR, None)
            .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Modified);

        // Symmetric: indexed as executable but worktree is plain → Modified.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let state = git_worktree_entry_state(temp.path(), "script.sh", oid, GIT_MODE_EXECUTABLE, None)
            .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Modified);

        // Same exec bit on both sides → Clean.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let state = git_worktree_entry_state(temp.path(), "script.sh", oid, GIT_MODE_EXECUTABLE, None)
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
        // Index says it's a regular file, but worktree is a symlink.
        let oid = write_blob_hash(b"anything");
        let state =
            git_worktree_entry_state(temp.path(), "link", oid, GIT_MODE_REGULAR, None).expect("call");
        assert_eq!(state, GitWorktreeEntryState::Modified);
    }

    #[cfg(unix)]
    #[test]
    fn typechange_symlink_to_file_is_modified() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::write(temp.path().join("link"), b"now a file").unwrap();
        // Index says it's a symlink, but worktree is a regular file.
        let oid = write_blob_hash(b"old target");
        let state =
            git_worktree_entry_state(temp.path(), "link", oid, GIT_MODE_SYMLINK, None).expect("call");
        assert_eq!(state, GitWorktreeEntryState::Modified);
    }

    /// Symlink target with non-UTF-8 bytes. `to_string_lossy` would
    /// replace the bytes with U+FFFD and produce a hash that never
    /// matches; using raw OS bytes hashes to what git would compute.
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
        let state = git_worktree_entry_state(temp.path(), "link", oid, GIT_MODE_SYMLINK, None)
            .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Clean);
    }

    fn probe_for(path: &Path, index_timestamp_secs: i64) -> IndexStatProbe {
        let metadata = gix::index::fs::Metadata::from_path_no_follow(path).expect("lstat");
        let stat = gix::index::entry::Stat::from_fs(&metadata).expect("stat");
        IndexStatProbe {
            stat,
            index_timestamp_secs,
        }
    }

    /// When the worktree stat matches the index's cached stat and the entry is
    /// outside the racy window, the read + hash is skipped — proven here by
    /// passing a deliberately WRONG oid that would force `Modified` if it were
    /// ever consulted, yet the entry reads `Clean`.
    #[cfg(unix)]
    #[test]
    fn stat_fastpath_skips_hash_when_unchanged() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("a.txt");
        fs::write(&path, b"hello").unwrap();
        let stat = probe_for(&path, 0).stat;
        // index written strictly after the file's mtime → not racy.
        let probe = IndexStatProbe {
            stat,
            index_timestamp_secs: i64::from(stat.mtime.secs) + 5,
        };
        let wrong_oid = write_blob_hash(b"totally different bytes");
        let state =
            git_worktree_entry_state(temp.path(), "a.txt", wrong_oid, GIT_MODE_REGULAR, Some(probe))
                .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Clean);
    }

    /// The racy-clean window: when the index's own write time is at or before
    /// the file's mtime, a same-second modification could leave the stat
    /// unchanged, so the fast-path MUST fall back to hashing. Same setup as the
    /// test above (stat matches), but a racy timestamp — now the wrong oid is
    /// consulted and the change is caught as `Modified`.
    #[cfg(unix)]
    #[test]
    fn racy_window_falls_back_to_hashing() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("a.txt");
        fs::write(&path, b"hello").unwrap();
        let stat = probe_for(&path, 0).stat;
        // index timestamp == file mtime → racy: stat is untrustworthy.
        let probe = IndexStatProbe {
            stat,
            index_timestamp_secs: i64::from(stat.mtime.secs),
        };
        let stale_oid = write_blob_hash(b"totally different bytes");
        let state =
            git_worktree_entry_state(temp.path(), "a.txt", stale_oid, GIT_MODE_REGULAR, Some(probe))
                .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Modified);
    }

    /// A stat mismatch (here, a different size) bypasses the fast-path and
    /// hashes, so a genuine change is reported even when the index isn't racy.
    #[cfg(unix)]
    #[test]
    fn stat_mismatch_falls_back_to_hashing() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("a.txt");
        fs::write(&path, b"hello").unwrap();
        let mut probe = probe_for(&path, 0);
        probe.index_timestamp_secs = i64::from(probe.stat.mtime.secs) + 5;
        // Pretend the index cached a different size → stat can't match.
        probe.stat.size = probe.stat.size.wrapping_add(1);
        let old_oid = write_blob_hash(b"world");
        let state =
            git_worktree_entry_state(temp.path(), "a.txt", old_oid, GIT_MODE_REGULAR, Some(probe))
                .expect("call");
        assert_eq!(state, GitWorktreeEntryState::Modified);
    }
}
