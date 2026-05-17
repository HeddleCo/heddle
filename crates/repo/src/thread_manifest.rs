// SPDX-License-Identifier: Apache-2.0
//! Per-thread sidecar that records the result of materializing a
//! thread's captured tree onto disk.
//!
//! The manifest is the **stat-cache** that makes `heddle capture`
//! against a materialized thread fast: instead of re-hashing every
//! file, capture stats each file and compares
//! `(inode, mtime_ns, ctime_ns, mode)` to the manifest's record.
//! Matches → unchanged, reuse the stored hash. Misses → re-hash and
//! write the new blob. Same pattern git's index uses.
//!
//! Lives at `<heddle_dir>/threads/<thread>/manifest.toml`, outside
//! the thread's worktree root so `rm -rf .` inside the worktree
//! doesn't destroy it. Rewritten atomically (temp + rename) on
//! every successful capture.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use objects::{
    fs_atomic::{enrich_fs_error, enrich_rename_error},
    object::{ChangeId, ContentHash},
};
use serde::{Deserialize, Serialize};

/// Current schema version. Bumped only when the on-disk layout
/// changes incompatibly; readers should refuse to interpret a
/// manifest with a future version they don't recognise.
///
/// v2 adds [`ManifestFile::size`] — the prior schema had no size
/// field, so a v1 manifest read as v2 would default `size` to 0 and
/// silently mismatch every entry.
///
/// v3 adds [`ThreadManifest::worktree_path`] — the absolute path the
/// manifest's stats describe. Without it, `Repository::snapshot`
/// can't tell whether it's running inside the materialized worktree
/// (use the stat-cache + refresh the manifest after) or some other
/// directory the same thread is checked out at (do neither — the
/// stats won't match anyway, and a refresh would corrupt the
/// manifest by writing the wrong directory's stats into it).
///
/// Bumping the version forces a clean rematerialize for any thread
/// carrying a pre-current manifest.
pub const SCHEMA_VERSION: u32 = 3;

/// On-disk per-thread materialization record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadManifest {
    /// Manifest format version. See [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The state this manifest is a materialization of.
    pub state_id: ChangeId,
    /// Tree hash captured at materialize time. Future-proofing for a
    /// `refresh` operation that wants to know which tree the on-disk
    /// state corresponds to without resolving the thread head again.
    pub tree_hash: ContentHash,
    /// UNIX-epoch seconds at materialize time. Diagnostic only;
    /// nothing in the capture path consults this.
    pub materialized_at: u64,
    /// Absolute path to the worktree directory the per-file stat
    /// records describe. Used by `Repository::snapshot` to decide
    /// whether the running capture is happening inside this
    /// materialized worktree (refresh + use cache) or somewhere
    /// else (skip both). Canonicalized at write time so symlink /
    /// `./` traversal differences don't cause a false miss.
    pub worktree_path: PathBuf,
    /// Per-file stat-cache. Key is the worktree-relative path with
    /// forward-slash separators (so a manifest moves between macOS
    /// and Linux without rewriting). Value is the snapshot of
    /// `(hash, size, inode, mtime_ns, ctime_ns, mode)` we recorded
    /// when the file landed in the worktree.
    #[serde(default)]
    pub files: BTreeMap<String, ManifestFile>,
}

impl ThreadManifest {
    /// Construct an empty manifest pointing at `state_id`. Caller is
    /// expected to populate `files` before persisting.
    ///
    /// `worktree_path` should be the path the manifest's stat records
    /// will describe. Production callers canonicalize the path
    /// (`std::fs::canonicalize`) so a later `snapshot` from inside
    /// the worktree matches regardless of symlink / `./` indirection.
    pub fn new(state_id: ChangeId, tree_hash: ContentHash, worktree_path: PathBuf) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            state_id,
            tree_hash,
            materialized_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            worktree_path,
            files: BTreeMap::new(),
        }
    }
}

/// Per-file record inside a [`ThreadManifest`]. The stat fields are
/// stored together so a `stat`-cache hit is a single comparison and
/// the user can never accidentally bypass it by `touch`ing a file
/// with a frozen clock.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ManifestFile {
    /// Content hash at materialize time. Reused as the file's blob
    /// hash on capture when the stat-cache matches.
    pub hash: ContentHash,
    /// File size in bytes. Cheapest possible "content changed"
    /// detector — independent of mtime granularity. CI runners
    /// frequently mount filesystems (ext4 with various options,
    /// overlayfs over tmpfs) where two back-to-back writes share an
    /// mtime, so size is the safety net that catches "wrote
    /// different-length bytes" even when the timestamps collapse.
    pub size: u64,
    /// File inode. On reflink filesystems (APFS, btrfs, XFS-reflink),
    /// the inode is the per-clonefile identity — a re-clonefile of
    /// the same canonical blob gets a *different* inode, so this
    /// catches "the file was clobbered and re-cloned" out of band.
    pub inode: u64,
    /// Modification time in nanoseconds since UNIX epoch.
    pub mtime_ns: i64,
    /// Status-change time in nanoseconds since UNIX epoch. Catches
    /// metadata-only changes that don't bump `mtime` (e.g. `chmod`).
    pub ctime_ns: i64,
    /// Unix mode bits (including type bits). `0o100644` for a regular
    /// file, `0o100755` for executable, etc.
    pub mode: u32,
}

impl ManifestFile {
    /// `true` iff `other` describes the same file content + metadata
    /// as `self`. Hot-path comparison for the capture stat-cache.
    #[inline]
    pub fn matches(&self, other: &ManifestFile) -> bool {
        self.size == other.size
            && self.inode == other.inode
            && self.mtime_ns == other.mtime_ns
            && self.ctime_ns == other.ctime_ns
            && self.mode == other.mode
    }
}

/// Where this thread's manifest lives on disk, given the repo's
/// `heddle_dir` and a sanitised thread name.
pub fn manifest_path(heddle_dir: &Path, thread: &str) -> PathBuf {
    heddle_dir
        .join("threads")
        .join(thread)
        .join("manifest.toml")
}

/// Summary of a single materialized thread, gathered by walking
/// `<heddle_dir>/threads/`. Stable enough for `heddle daemon status`
/// and `heddle thread list` to render; expand as the daemon grows
/// new observability needs.
#[derive(Clone, Debug)]
pub struct MaterializedThreadSummary {
    /// Thread name (the directory under `threads/`).
    pub thread: String,
    /// Recorded state at materialize time.
    pub state_id: ChangeId,
    /// Recorded uncompressed tree hash at materialize time.
    pub tree_hash: ContentHash,
    /// UNIX-epoch seconds at materialize time.
    pub materialized_at: u64,
    /// Number of files tracked by the manifest.
    pub file_count: usize,
}

/// Walk `<heddle_dir>/threads/` and return one summary per thread
/// whose manifest parses. Threads with malformed or schema-mismatch
/// manifests are silently skipped — callers that care can re-read
/// individual manifests via [`read_manifest`].
///
/// Walks recursively. Thread names conventionally contain slashes
/// (`feature/m-thread`, `bugfix/issue-123`) which `manifest_path`
/// passes through to `Path::join`, so the on-disk layout mirrors
/// the conceptual hierarchy. A single-level read_dir would miss
/// every nested thread; recursing reconstructs the thread name from
/// the relative path of each `manifest.toml`'s parent directory.
pub fn list_thread_manifests(heddle_dir: &Path) -> io::Result<Vec<MaterializedThreadSummary>> {
    let threads_dir = heddle_dir.join("threads");
    let mut summaries = Vec::new();
    walk_thread_manifests(&threads_dir, &threads_dir, &mut summaries)?;
    summaries.sort_by(|a, b| a.thread.cmp(&b.thread));
    Ok(summaries)
}

/// Depth-first walk under `threads_dir`. For every directory found
/// to contain a `manifest.toml`, parse it and collect a summary; the
/// thread name is the relative path of that directory from the root
/// (`feature/m-thread` for a manifest at `threads/feature/m-thread/`).
/// Subdirectories of a manifest-bearing directory are *not* recursed
/// into — a thread directory shouldn't contain another thread.
fn walk_thread_manifests(
    threads_dir: &Path,
    cur: &Path,
    out: &mut Vec<MaterializedThreadSummary>,
) -> io::Result<()> {
    let entries = match fs::read_dir(cur) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(enrich_fs_error(cur, "listing", e)),
    };
    // Two-pass: first look for `manifest.toml` (treat this dir as a
    // thread root and stop), otherwise recurse into subdirectories.
    // Collected so we can short-circuit cheaply without re-stating.
    let mut subdirs = Vec::new();
    let mut has_manifest = false;
    for entry in entries {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            subdirs.push(entry.path());
        } else if ft.is_file() && entry.file_name() == "manifest.toml" {
            has_manifest = true;
        }
    }
    if has_manifest {
        // Reconstruct the thread name from the path relative to
        // `threads_dir`. On Windows the separator is `\`; the on-
        // disk schema is always `/`-joined per `manifest_path`'s use
        // of `Path::join` plus the convention that thread names use
        // forward slashes. Normalise here so the returned summary's
        // `thread` field round-trips through `read_manifest` and
        // `manifest_path` cleanly.
        let rel = cur.strip_prefix(threads_dir).unwrap_or(cur);
        let mut name_parts: Vec<String> = Vec::new();
        for component in rel.components() {
            if let std::path::Component::Normal(s) = component
                && let Some(s) = s.to_str()
            {
                name_parts.push(s.to_string());
            } else {
                // Non-utf8 or weird path component → skip silently.
                return Ok(());
            }
        }
        if name_parts.is_empty() {
            return Ok(());
        }
        let name = name_parts.join("/");
        if let Ok(Some(m)) = read_manifest(threads_dir.parent().unwrap_or(threads_dir), &name) {
            out.push(MaterializedThreadSummary {
                thread: name,
                state_id: m.state_id,
                tree_hash: m.tree_hash,
                materialized_at: m.materialized_at,
                file_count: m.files.len(),
            });
        }
        return Ok(());
    }
    for sub in subdirs {
        walk_thread_manifests(threads_dir, &sub, out)?;
    }
    Ok(())
}

/// Read the on-disk manifest for `thread`. Returns `Ok(None)` when no
/// manifest exists yet (thread has never been materialized through
/// this code path). Returns an error on malformed TOML or a
/// schema-version mismatch — callers should treat that as "rebuild
/// the manifest from scratch", not as a corruption hazard.
pub fn read_manifest(heddle_dir: &Path, thread: &str) -> io::Result<Option<ThreadManifest>> {
    let path = manifest_path(heddle_dir, thread);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(enrich_fs_error(&path, "reading", e)),
    };
    let manifest: ThreadManifest = toml::from_str(&text).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("malformed thread manifest at {}: {e}", path.display()),
        )
    })?;
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "thread manifest at {} uses schema {} but this binary speaks {}",
                path.display(),
                manifest.schema_version,
                SCHEMA_VERSION
            ),
        ));
    }
    Ok(Some(manifest))
}

/// Delete the on-disk manifest directory for `thread`. Used by
/// `heddle thread drop` to keep the materialized-thread inventory
/// (`heddle status` / `heddle daemon status`) in sync with the live
/// thread set. Idempotent: a missing directory is reported as
/// "deleted = false" rather than an error.
///
/// Removes the whole `<heddle_dir>/threads/<thread>/` directory —
/// not just `manifest.toml` — so future per-thread sidecars
/// (verification artefacts, capture journals, etc.) clean up with
/// the same call.
pub fn remove_thread_manifest_dir(heddle_dir: &Path, thread: &str) -> io::Result<bool> {
    let threads_root = heddle_dir.join("threads");
    let dir = threads_root.join(thread);
    let removed = match fs::remove_dir_all(&dir) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(enrich_fs_error(&dir, "removing", e)),
    };
    // Sweep empty parent directories left over by slash-namespaced
    // threads. Dropping `feature/m-thread` removes
    // `threads/feature/m-thread/` but `threads/feature/` would
    // otherwise linger — purely cosmetic, but after many dropped
    // threads the on-disk tree grows visually messy and the user
    // ends up with a `ls .heddle/threads/` full of empty husks.
    // Walk upward stopping at the first non-empty parent (another
    // thread might still live there) or at `threads/` itself
    // (we don't reap the root, that's heddle-managed).
    if removed
        && let Some(mut parent) = dir.parent()
    {
        while parent != threads_root {
            match fs::read_dir(parent).map(|mut it| it.next().is_some()) {
                Ok(true) => break,                  // sibling thread present
                Ok(false) => match fs::remove_dir(parent) {
                    Ok(()) => {}
                    // Race: another process refilled the dir
                    // between the read_dir check and the remove —
                    // treat as "stop, that's their problem now".
                    Err(_) => break,
                },
                Err(_) => break,
            }
            match parent.parent() {
                Some(next) => parent = next,
                None => break,
            }
        }
    }
    Ok(removed)
}

/// Atomically write `manifest` to disk for `thread`. Writes to a
/// sibling temp file first then renames into place so a torn write
/// can't leave a half-baked manifest visible to the next reader.
pub fn write_manifest(
    heddle_dir: &Path,
    thread: &str,
    manifest: &ThreadManifest,
) -> io::Result<()> {
    let path = manifest_path(heddle_dir, thread);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| enrich_fs_error(parent, "creating", e))?;
    }
    let text = toml::to_string_pretty(manifest).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("serialising thread manifest: {e}"),
        )
    })?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text).map_err(|e| enrich_fs_error(&tmp, "writing", e))?;
    fs::rename(&tmp, &path).map_err(|e| enrich_rename_error(&tmp, &path, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use objects::object::ContentHash;
    use tempfile::TempDir;

    use super::*;

    fn h(byte: u8) -> ContentHash {
        ContentHash::from_bytes([byte; 32])
    }

    fn cid() -> ChangeId {
        ChangeId::generate()
    }

    #[test]
    fn round_trip_empty() {
        let dir = TempDir::new().unwrap();
        let manifest = ThreadManifest::new(cid(), h(1), PathBuf::from("/tmp/test-worktree"));
        write_manifest(dir.path(), "main", &manifest).unwrap();
        let loaded = read_manifest(dir.path(), "main").unwrap().expect("present");
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert_eq!(loaded.tree_hash, h(1));
        assert!(loaded.files.is_empty());
    }

    #[test]
    fn round_trip_with_files() {
        let dir = TempDir::new().unwrap();
        let mut manifest = ThreadManifest::new(cid(), h(7), PathBuf::from("/tmp/test-worktree"));
        manifest.files.insert(
            "Cargo.toml".to_string(),
            ManifestFile {
                hash: h(2),
                size: 128,
                inode: 4242,
                mtime_ns: 1_700_000_000_000_000_000,
                ctime_ns: 1_700_000_000_000_000_000,
                mode: 0o100644,
            },
        );
        manifest.files.insert(
            "src/main.rs".to_string(),
            ManifestFile {
                hash: h(3),
                size: 512,
                inode: 4243,
                mtime_ns: 1_700_000_001_000_000_000,
                ctime_ns: 1_700_000_001_000_000_000,
                mode: 0o100644,
            },
        );
        write_manifest(dir.path(), "feature-x", &manifest).unwrap();
        let loaded = read_manifest(dir.path(), "feature-x")
            .unwrap()
            .expect("present");
        assert_eq!(loaded.files.len(), 2);
        assert_eq!(loaded.files["Cargo.toml"].hash, h(2));
        assert_eq!(loaded.files["src/main.rs"].inode, 4243);
    }

    #[test]
    fn read_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        assert!(read_manifest(dir.path(), "no-such").unwrap().is_none());
    }

    #[test]
    fn list_returns_sorted_summaries_for_present_manifests() {
        let dir = TempDir::new().unwrap();
        let m1 = ThreadManifest::new(cid(), h(1), PathBuf::from("/tmp/test-worktree"));
        let mut m2 = ThreadManifest::new(cid(), h(2), PathBuf::from("/tmp/test-worktree"));
        m2.files.insert(
            "a.txt".to_string(),
            ManifestFile {
                hash: h(9),
                size: 7,
                inode: 1,
                mtime_ns: 0,
                ctime_ns: 0,
                mode: 0o100644,
            },
        );
        write_manifest(dir.path(), "zebra", &m1).unwrap();
        write_manifest(dir.path(), "alpha", &m2).unwrap();
        let summaries = list_thread_manifests(dir.path()).unwrap();
        assert_eq!(summaries.len(), 2);
        // Sorted by name → alpha first.
        assert_eq!(summaries[0].thread, "alpha");
        assert_eq!(summaries[0].tree_hash, h(2));
        assert_eq!(summaries[0].file_count, 1);
        assert_eq!(summaries[1].thread, "zebra");
        assert_eq!(summaries[1].tree_hash, h(1));
        assert_eq!(summaries[1].file_count, 0);
    }

    #[test]
    fn list_empty_when_no_threads_dir() {
        let dir = TempDir::new().unwrap();
        let summaries = list_thread_manifests(dir.path()).unwrap();
        assert!(summaries.is_empty());
    }

    #[test]
    fn matches_compares_identity_fields() {
        let a = ManifestFile {
            hash: h(1),
            size: 64,
            inode: 100,
            mtime_ns: 200,
            ctime_ns: 300,
            mode: 0o100644,
        };
        let b = a;
        assert!(a.matches(&b));
        let mut c = a;
        c.mtime_ns = 999;
        assert!(!a.matches(&c));
        let mut d = a;
        d.mode = 0o100755;
        assert!(!a.matches(&d));
        let mut e = a;
        e.size = 65;
        assert!(!a.matches(&e), "size change must invalidate the cache");
    }

    /// `remove_thread_manifest_dir` must reap empty parent directories
    /// left behind by slash-namespaced thread names so the on-disk
    /// inventory doesn't grow visually messy after many drops. Stop
    /// reaping when a sibling thread is still around, and never reap
    /// the `threads/` root itself.
    #[test]
    fn remove_thread_manifest_dir_reaps_empty_parents() {
        let dir = TempDir::new().unwrap();
        // Two threads under the same `feature/` parent. Drop one;
        // `feature/` must survive because the other is still there.
        let m = ThreadManifest::new(cid(), h(1), PathBuf::from("/tmp/test-worktree"));
        write_manifest(dir.path(), "feature/keeper", &m).unwrap();
        write_manifest(dir.path(), "feature/dropme", &m).unwrap();

        assert!(remove_thread_manifest_dir(dir.path(), "feature/dropme").unwrap());
        assert!(
            dir.path().join("threads").join("feature").is_dir(),
            "sibling thread must keep the `feature/` parent alive"
        );
        assert!(
            dir.path()
                .join("threads")
                .join("feature")
                .join("keeper")
                .is_dir(),
            "keeper's directory must be untouched"
        );

        // Now drop the second one. `feature/` should be reaped, and
        // `threads/` itself must stay (heddle-managed root).
        assert!(remove_thread_manifest_dir(dir.path(), "feature/keeper").unwrap());
        assert!(
            !dir.path().join("threads").join("feature").exists(),
            "empty `feature/` parent must be reaped"
        );
        assert!(
            dir.path().join("threads").is_dir(),
            "the `threads/` root is heddle-managed and must never be reaped"
        );
    }

    /// Dropping a non-slash thread name is the trivial case: the
    /// thread dir vanishes and there are no parents to reap (the
    /// only ancestor inside `threads/` is `threads/` itself).
    #[test]
    fn remove_thread_manifest_dir_handles_flat_names() {
        let dir = TempDir::new().unwrap();
        let m = ThreadManifest::new(cid(), h(1), PathBuf::from("/tmp/test-worktree"));
        write_manifest(dir.path(), "main", &m).unwrap();
        assert!(remove_thread_manifest_dir(dir.path(), "main").unwrap());
        assert!(!dir.path().join("threads").join("main").exists());
        assert!(dir.path().join("threads").is_dir());
    }

    /// Missing thread reports `false` rather than erroring — drops
    /// are idempotent so a double-drop is a no-op success.
    #[test]
    fn remove_thread_manifest_dir_is_idempotent_on_missing() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("threads")).unwrap();
        assert!(!remove_thread_manifest_dir(dir.path(), "never-existed").unwrap());
    }
}
