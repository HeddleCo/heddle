// SPDX-License-Identifier: Apache-2.0
//! IO helpers for FsStore.

use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use bytes::Bytes;

use crate::{
    error::HeddleError,
    fs_atomic::{enrich_fs_error, enrich_rename_error, sync_directory},
    store::{Result, atomic::temp_path},
};

const MMAP_THRESHOLD_BYTES: u64 = 256 * 1024;

pub(super) enum FileBytes {
    Vec(Vec<u8>),
    Mmap(memmap2::Mmap),
}

impl FileBytes {
    pub(super) fn as_slice(&self) -> &[u8] {
        match self {
            FileBytes::Vec(data) => data,
            FileBytes::Mmap(data) => data,
        }
    }

    pub(super) fn into_vec(self) -> Vec<u8> {
        match self {
            FileBytes::Vec(data) => data,
            FileBytes::Mmap(data) => data.to_vec(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AtomicWriteMode {
    Durable,
    /// No fsync at all. Caller asserts the file is a recoverable
    /// cache mirror — the authoritative copy lives elsewhere
    /// (typically a pack) and re-derivation on read is correct.
    /// On macOS APFS, `sync_data` alone is ~5 ms per call
    /// (`F_FULLFSYNC`-class cost); skipping it cuts cache-write
    /// throughput from ~200 writes/s to ~5500 writes/s. The price
    /// is that a torn write after a crash could leave the cached
    /// file with garbage bytes — readers must guard with a hash
    /// check before trusting the content.
    NoSync,
}

/// Whether a snapshot-batch staged object must issue its own per-file
/// `sync_data` for content durability, rather than deferring to the
/// batch's single `syncfs()` barrier at flush.
///
/// On Linux the staged temp's data sync is deferred to one `syncfs()`
/// in `flush_snapshot_write_batch` (git's `core.fsyncMethod=batch`):
/// one filesystem-wide flush replaces the N per-object fsyncs that
/// dominate large commit-history import (heddle#550). Non-Linux has no
/// `syncfs`, so each staged temp must sync its own data before the
/// flush promotes (renames) it into place.
fn batch_staged_object_needs_per_file_sync() -> bool {
    !cfg!(target_os = "linux")
}

/// Write `data` to a temp file beside `path` but DON'T rename it into
/// place. Used to *stage* a snapshot-batch object (heddle#550
/// quarantine-then-promote): the canonical content-addressed path never
/// holds bytes until [`promote_staged_object`] renames the temp in
/// AFTER the durability barrier, so a crash before flush can only leave
/// an orphan temp (ignored by reads) — never a present-but-torn object
/// that the exists-skip would refuse to rewrite.
///
/// On Linux the temp is left un-synced (the batch's `syncfs()` flushes
/// it); elsewhere it is `sync_data`'d so the bytes are durable before
/// the promote rename. Returns the temp path to record for promotion.
pub(super) fn stage_loose_object(path: &Path, data: &[u8]) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("invalid atomic write path"))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| HeddleError::Io(enrich_fs_error(parent, "creating", e)))?;

    let temp_path = temp_path(path);
    let write_result: std::io::Result<()> = (|| {
        let mut file = open_temp_0o644(&temp_path)?;
        use std::io::Write as _;
        file.write_all(data)?;
        if batch_staged_object_needs_per_file_sync() {
            file.sync_data()?;
        }
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(HeddleError::Io(enrich_fs_error(path, "writing", err)));
    }
    Ok(temp_path)
}

/// Promote a temp staged by [`stage_loose_object`] into its canonical
/// path. The caller MUST have run the batch durability barrier (Linux
/// `syncfs`, else the per-file `sync_data` in `stage_loose_object`)
/// BEFORE calling this, so the renamed bytes are already durable; the
/// only remaining durability step is making the new directory entry
/// itself durable, which the caller does once for the whole batch
/// (a second `syncfs` on Linux, per-directory fsyncs elsewhere).
///
/// A `rename` over an existing canonical file atomically replaces it
/// (a sibling writer may have installed the same content-addressed
/// object meanwhile — identical bytes, so the replace is a no-op in
/// effect). The temp is cleaned up on failure.
pub(super) fn promote_staged_object(temp_path: &Path, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| HeddleError::Io(enrich_fs_error(parent, "creating", e)))?;
    }
    if let Err(err) = std::fs::rename(temp_path, path) {
        let _ = std::fs::remove_file(temp_path);
        return Err(HeddleError::Io(enrich_rename_error(temp_path, path, err)));
    }
    Ok(())
}

fn open_temp_0o644(temp_path: &Path) -> std::io::Result<File> {
    // Open with explicit mode 0o644 instead of relying on the process
    // umask. This makes loose objects byte-and-mode deterministic:
    // clonefile on macOS preserves source mode, so a worktree
    // materialised from a loose blob inherits 0o644 *without* an extra
    // chmod. `repository_materialization` skips `set_file_mode` on
    // non-executable files because of this contract.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o644);
    }
    opts.open(temp_path)
}

pub(super) fn write_atomic(path: &Path, data: &[u8], mode: AtomicWriteMode) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("invalid atomic write path"))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| HeddleError::Io(enrich_fs_error(parent, "creating", e)))?;

    let temp_path = temp_path(path);
    // Tag each fallible op with the verb that should appear in the
    // user-facing message if it trips, so an EXDEV from `rename` gets
    // the src+dst-aware message via `enrich_rename_error`.
    enum Op {
        Write,
        Rename,
        SyncDir,
    }
    let mut failing_op = Op::Write;
    let write_result: std::io::Result<()> = (|| {
        let mut file = open_temp_0o644(&temp_path)?;
        use std::io::Write as _;
        file.write_all(data)?;
        match mode {
            // `Durable` is the strongest mode: data + metadata fsync
            // before rename, then directory fsync after — so the file
            // is fully on disk and discoverable through the parent
            // directory before this returns.
            AtomicWriteMode::Durable => file.sync_all()?,
            // Cache-mirror writes: no fsync. Caller guards reads
            // with a hash check, so torn-write corruption is
            // recoverable (re-promote from the authoritative copy).
            AtomicWriteMode::NoSync => {}
        }
        failing_op = Op::Rename;
        std::fs::rename(&temp_path, path)?;
        failing_op = Op::SyncDir;
        match mode {
            AtomicWriteMode::Durable => sync_directory(parent)?,
            AtomicWriteMode::NoSync => {}
        }
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        let wrapped = match failing_op {
            Op::Write => enrich_fs_error(path, "writing", err),
            Op::Rename => enrich_rename_error(&temp_path, path, err),
            Op::SyncDir => enrich_fs_error(parent, "syncing", err),
        };
        return Err(HeddleError::Io(wrapped));
    }

    Ok(())
}

/// Read the file's header (up to `header_len` bytes) and report its
/// total on-disk size, without loading the body. Returns `Ok(None)`
/// when the file is missing.
///
/// Used by [`crate::store::ObjectStore::blob_size`] on `FsStore` to
/// avoid pulling whole blobs through `get_blob` just to learn their
/// uncompressed size — the size is recorded in the compression header
/// for compressed blobs, and equals the file length for raw blobs.
pub(super) fn read_file_header(path: &Path, header_len: usize) -> Result<Option<(Vec<u8>, u64)>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let metadata = file.metadata()?;
    let len = metadata.len();
    let to_read = std::cmp::min(header_len as u64, len) as usize;
    let mut header = vec![0u8; to_read];
    if to_read > 0 {
        use std::io::Read as _;
        file.read_exact(&mut header)?;
    }
    Ok(Some((header, len)))
}

/// Read a pack file as zero-copy [`Bytes`]. For packs that clear the
/// mmap threshold, the underlying memory is the mmap'd region —
/// every `Bytes::slice` into it is a zero-copy view. Smaller packs
/// fall back to a heap read wrapped in `Bytes`. Public because the
/// pack reader lives in a sibling module and needs to bypass the
/// `pub(super)` gate on `read_file_bytes`.
pub fn read_file_bytes_for_pack(path: &Path) -> Result<Bytes> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(Bytes::new());
    }
    if len >= MMAP_THRESHOLD_BYTES {
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file)? };
        if mmap.len() != len as usize {
            return Err(HeddleError::InvalidObject(
                "pack file size changed during memory mapping".to_string(),
            ));
        }
        return Ok(Bytes::from_owner(mmap));
    }
    let mut data = Vec::with_capacity(len as usize);
    let mut reader = file;
    reader.read_to_end(&mut data)?;
    Ok(Bytes::from(data))
}

pub(super) fn read_file_bytes(path: &Path) -> Result<Option<FileBytes>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let metadata = file.metadata()?;
    let len = metadata.len();
    if len == 0 {
        return Ok(Some(FileBytes::Vec(vec![])));
    }
    if len >= MMAP_THRESHOLD_BYTES {
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file)? };
        if mmap.len() != len as usize {
            return Err(crate::store::HeddleError::InvalidObject(
                "file size changed during memory mapping".to_string(),
            ));
        }
        return Ok(Some(FileBytes::Mmap(mmap)));
    }

    let mut data = Vec::with_capacity(len as usize);
    let mut reader = file;
    reader.read_to_end(&mut data)?;
    Ok(Some(FileBytes::Vec(data)))
}

/// List all content hashes from a sharded directory structure (aa/bbcc... → aabbcc...).
pub(super) fn list_hashes_from_dir(
    dir: &std::path::Path,
) -> Result<Vec<crate::object::ContentHash>> {
    use std::fs;

    use tracing::debug;

    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut hashes = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let prefix = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if prefix.len() == 2 {
                for sub_entry in fs::read_dir(&path)? {
                    let sub_entry = sub_entry?;
                    let sub_path = sub_entry.path();
                    if let Some(name) = sub_path.file_name().and_then(|n| n.to_str()) {
                        let full_hash = format!("{}{}", prefix, name);
                        if let Ok(hash) = crate::object::ContentHash::from_hex(&full_hash) {
                            hashes.push(hash);
                        }
                    }
                }
            }
        }
    }
    debug!(count = hashes.len(), "Listed hashes");
    Ok(hashes)
}

#[cfg(test)]
mod tests {
    use super::batch_staged_object_needs_per_file_sync;

    #[test]
    fn staged_object_defers_per_file_fsync_to_syncfs_only_on_linux() {
        // A snapshot-batch staged object defers its data sync to the
        // batch's single `syncfs()` barrier ONLY on Linux (the #550
        // perf win — N per-object fsyncs collapse to one syncfs). Every
        // other platform has no `syncfs`, so each staged temp must
        // `sync_data` itself before the flush promotes it into place.
        let needs_sync = batch_staged_object_needs_per_file_sync();
        if cfg!(target_os = "linux") {
            assert!(
                !needs_sync,
                "Linux must defer the staged-object data sync to the batch syncfs barrier",
            );
        } else {
            assert!(
                needs_sync,
                "non-Linux has no syncfs and must sync each staged object per file",
            );
        }
    }
}
