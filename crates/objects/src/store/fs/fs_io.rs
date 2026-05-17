// SPDX-License-Identifier: Apache-2.0
//! IO helpers for FsStore.

use std::{
    collections::BTreeSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
};

use bytes::Bytes;

use crate::{
    error::HeddleError,
    fs_atomic::{enrich_fs_error, enrich_rename_error, sync_directory},
    store::{atomic::temp_path, Result},
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
    BatchDirectorySync,
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

pub(super) fn write_atomic(
    path: &Path,
    data: &[u8],
    mode: AtomicWriteMode,
    pending_directory_syncs: Option<&Mutex<BTreeSet<PathBuf>>>,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("invalid atomic write path"))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| HeddleError::Io(enrich_fs_error(parent, "creating", e)))?;

    let temp_path = temp_path(path);
    // Tag each fallible op with the verb that should appear in the
    // user-facing message if it trips. We compute the wrapped error at
    // the boundary so an EXDEV from `rename` gets the src+dst-aware
    // message via `enrich_rename_error`, while a write into the temp
    // file gets the "writing" verb against the destination path. The
    // deferred-directory-sync lock-poison case is non-IO and gets a
    // synthetic `io::Error::other`.
    enum Op {
        Write,
        Rename,
        SyncDir,
    }
    let mut failing_op = Op::Write;
    let write_result: std::io::Result<()> = (|| {
        // Open with explicit mode 0o644 instead of relying on the
        // process umask. This makes loose objects byte-and-mode
        // deterministic: clonefile on macOS preserves source mode,
        // so a worktree materialised from a loose blob inherits
        // 0o644 *without* an extra chmod. `repository_materialization`
        // skips `set_file_mode` on non-executable files because of
        // this contract — see `materialize_blob`'s comment near the
        // `set_file_mode(dest, true)` call.
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o644);
        }
        let mut file = opts.open(&temp_path)?;
        use std::io::Write as _;
        file.write_all(data)?;
        match mode {
            // `Durable` is the strongest mode: data + metadata fsync
            // before rename, then directory fsync after — so the file
            // is fully on disk and discoverable through the parent
            // directory before this returns.
            AtomicWriteMode::Durable => file.sync_all()?,
            // `BatchDirectorySync` keeps per-file content durability
            // (so a crash mid-batch can't leave a renamed-but-empty
            // file behind) but defers parent-directory fsyncs to
            // `flush_snapshot_write_batch`. The trees + state file
            // written during a snapshot rely on this mode for
            // durability of their *contents*; the deferred dir fsync
            // is what makes the rename observable to a fresh process.
            // Without `sync_data` here, a crash after rename + before
            // flush could leave a file that "exists" in the directory
            // but whose data blocks weren't flushed — exactly the
            // ACID violation we want to avoid for state/tree writes.
            AtomicWriteMode::BatchDirectorySync => file.sync_data()?,
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
            AtomicWriteMode::BatchDirectorySync => {
                if let Some(pending) = pending_directory_syncs {
                    let mut dirs = pending.lock().map_err(|_| {
                        std::io::Error::other("failed to acquire pending directory sync lock")
                    })?;
                    dirs.insert(parent.to_path_buf());
                }
            }
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
