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
pub const SCHEMA_VERSION: u32 = 1;

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
    /// Per-file stat-cache. Key is the worktree-relative path with
    /// forward-slash separators (so a manifest moves between macOS
    /// and Linux without rewriting). Value is the snapshot of
    /// `(hash, inode, mtime_ns, ctime_ns, mode)` we recorded when
    /// the file landed in the worktree.
    #[serde(default)]
    pub files: BTreeMap<String, ManifestFile>,
}

impl ThreadManifest {
    /// Construct an empty manifest pointing at `state_id`. Caller is
    /// expected to populate `files` before persisting.
    pub fn new(state_id: ChangeId, tree_hash: ContentHash) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            state_id,
            tree_hash,
            materialized_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            files: BTreeMap::new(),
        }
    }
}

/// Per-file record inside a [`ThreadManifest`]. All four time/identity
/// fields are stored together so a `stat`-cache hit is a single
/// comparison and the user can never accidentally bypass it by
/// `touch`ing a file with a frozen clock.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ManifestFile {
    /// Content hash at materialize time. Reused as the file's blob
    /// hash on capture when the stat-cache matches.
    pub hash: ContentHash,
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
        self.inode == other.inode
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
        let manifest = ThreadManifest::new(cid(), h(1));
        write_manifest(dir.path(), "main", &manifest).unwrap();
        let loaded = read_manifest(dir.path(), "main").unwrap().expect("present");
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert_eq!(loaded.tree_hash, h(1));
        assert!(loaded.files.is_empty());
    }

    #[test]
    fn round_trip_with_files() {
        let dir = TempDir::new().unwrap();
        let mut manifest = ThreadManifest::new(cid(), h(7));
        manifest.files.insert(
            "Cargo.toml".to_string(),
            ManifestFile {
                hash: h(2),
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
    fn matches_compares_identity_fields() {
        let a = ManifestFile {
            hash: h(1),
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
    }
}
