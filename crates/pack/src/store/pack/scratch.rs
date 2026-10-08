// SPDX-License-Identifier: Apache-2.0
//! Crash-reclaimable scratch. Owners hold a shared OS lock until their files
//! close; store-open sweeping takes an exclusive lock before deleting an entry.
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::Path,
    time::{Duration, SystemTime},
};

pub(super) const LEASE_FILE: &str = ".lease";

/// A live scratch directory, also usable for caller-owned transfer staging.
#[derive(Debug)]
pub struct ScratchLease(File);
impl ScratchLease {
    pub fn acquire(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.join(LEASE_FILE))?;
        file.lock_shared()?;
        Ok(Self(file))
    }
}
impl Drop for ScratchLease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Removes its files on normal drop; the lease protects even old scratch from
/// another process's store-open sweep. The OS releases the lease after a crash.
#[derive(Debug)]
pub struct ScratchDir {
    _lease: ScratchLease,
    directory: tempfile::TempDir,
}
impl ScratchDir {
    pub fn new(root: &Path, prefix: &str) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let directory = tempfile::Builder::new().prefix(prefix).tempdir_in(root)?;
        let lease = ScratchLease::acquire(directory.path())?;
        Ok(Self {
            _lease: lease,
            directory,
        })
    }
    pub fn path(&self) -> &Path {
        self.directory.path()
    }
}

/// Best-effort reclamation at store open. Unknown ages and unavailable locks
/// are kept. Symlinks are never followed. Fresh entries have a full day to
/// finish publishing their lease before they can become sweep candidates.
pub fn sweep_scratch(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        let expired = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > Duration::from_secs(86400));
        if !expired {
            continue;
        }
        let lease_path = if metadata.is_dir() {
            path.join(LEASE_FILE)
        } else {
            path.clone()
        };
        // Old abandoned entries predate leases. New directories always contain
        // a lease before any transfer writes, and remain locked until drop.
        let lease = match File::open(&lease_path) {
            Ok(file) => Some(file),
            Err(error) if metadata.is_dir() && error.kind() == io::ErrorKind::NotFound => None,
            Err(_) => continue,
        };
        if lease.as_ref().is_some_and(|file| file.try_lock().is_err()) {
            continue;
        }
        if metadata.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
}
