// SPDX-License-Identifier: Apache-2.0
//! File storage helpers for refs.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    thread::{self},
    time::{Duration, Instant},
};

use fs2::FileExt;
use objects::{
    error::{HeddleError, Result},
    object::ThreadName,
};

use super::{RefManager, name::validate_ref_name};
use crate::fs_atomic::write_file_atomic;

const MAX_LOCK_WAIT_SECS: u64 = 10;
const FLAT_THREADS_DIR_NAME: &str = "__heddle_flat";
const FLAT_THREAD_SUFFIX: &str = ".ref";

pub(super) struct RefsLock {
    file: File,
    path: PathBuf,
}

impl Drop for RefsLock {
    fn drop(&mut self) {
        if let Err(err) = self.file.unlock() {
            eprintln!(
                "Warning: failed to unlock refs lock file {}: {}",
                self.path.display(),
                err
            );
        }
    }
}

impl RefManager {
    pub(super) fn refs_dir(&self) -> PathBuf {
        self.root.join("refs")
    }
    pub(super) fn lock_path(&self) -> PathBuf {
        self.refs_dir().join("LOCK")
    }
    pub(super) fn threads_dir(&self) -> PathBuf {
        self.refs_dir().join("threads")
    }
    pub(super) fn flat_threads_dir(&self) -> PathBuf {
        self.threads_dir().join(FLAT_THREADS_DIR_NAME)
    }
    pub(super) fn legacy_tracks_dir(&self) -> PathBuf {
        self.refs_dir().join("tracks")
    }
    pub(super) fn legacy_track_path(&self, name: &str) -> Result<PathBuf> {
        validate_ref_name(name).map_err(|error| HeddleError::InvalidRefName(error.name))?;
        Ok(self.legacy_tracks_dir().join(name))
    }
    pub(super) fn markers_dir(&self) -> PathBuf {
        self.refs_dir().join("markers")
    }
    pub(super) fn remotes_dir(&self) -> PathBuf {
        self.refs_dir().join("remotes")
    }
    pub(super) fn head_path(&self) -> PathBuf {
        self.local_head
            .as_ref()
            .cloned()
            .unwrap_or_else(|| self.root.join("HEAD"))
    }
    /// Path of the heddle-internal pre-undo recovery pointer. A sibling of the
    /// per-checkout `HEAD` (ORIG_HEAD-style), OUTSIDE the user-writable ref
    /// namespaces under `refs/` (threads, markers, remotes). Keeping it out of
    /// `refs/` makes a collision with a user marker named `undo-recovery`
    /// impossible by construction (the marker CLI only ever touches
    /// `refs/markers/`).
    ///
    /// **Invariant: undo/redo recovery state is scoped to the same checkout as
    /// the history it recovers — never the shared ref root.** In
    /// objectstore-pointer worktrees the ref root is shared across sibling
    /// checkouts but `local_head` (and `op_scope`) is per-worktree; pinning the
    /// recovery pointer beside the local `HEAD` keeps a `heddle undo` in one
    /// checkout from clobbering a sibling checkout's recovery pointer. Tracks
    /// `head_path` so both land in the same directory.
    pub(super) fn undo_recovery_path(&self) -> PathBuf {
        self.head_path()
            .parent()
            .map(|dir| dir.join("UNDO_RECOVERY"))
            .unwrap_or_else(|| self.root.join("UNDO_RECOVERY"))
    }
    pub(super) fn packed_refs_path(&self) -> PathBuf {
        self.refs_dir().join("packed-refs")
    }
    pub(crate) fn ref_summary_index_path(&self) -> PathBuf {
        self.refs_dir().join("ref-summary-index")
    }
    pub(super) fn thread_path(&self, name: &ThreadName) -> Result<PathBuf> {
        validate_ref_name(name).map_err(|error| HeddleError::InvalidRefName(error.name))?;
        if name.contains('/') {
            let flat = self.flat_thread_path(name)?;
            if flat.exists() {
                return Ok(flat);
            }
            let legacy = self.legacy_thread_path(name)?;
            if legacy.exists() {
                return Ok(legacy);
            }
            Ok(flat)
        } else {
            self.legacy_thread_path(name)
        }
    }
    pub(super) fn legacy_thread_path(&self, name: &str) -> Result<PathBuf> {
        validate_ref_name(name).map_err(|error| HeddleError::InvalidRefName(error.name))?;
        Ok(self.threads_dir().join(name))
    }
    pub(super) fn flat_thread_path(&self, name: &str) -> Result<PathBuf> {
        validate_ref_name(name).map_err(|error| HeddleError::InvalidRefName(error.name))?;
        Ok(self.flat_threads_dir().join(encode_flat_thread_name(name)))
    }
    pub(super) fn decode_flat_thread_entry(&self, entry: &str) -> Option<String> {
        let (prefix, encoded) = entry.split_once('/')?;
        if prefix != FLAT_THREADS_DIR_NAME || encoded.contains('/') {
            return None;
        }
        decode_flat_thread_name(encoded)
    }
    pub(super) fn marker_path(&self, name: &str) -> Result<PathBuf> {
        validate_ref_name(name).map_err(|error| HeddleError::InvalidRefName(error.name))?;
        Ok(self.markers_dir().join(name))
    }
    pub(super) fn remote_thread_path(&self, remote: &str, thread: &str) -> Result<PathBuf> {
        validate_ref_name(remote).map_err(|error| HeddleError::InvalidRefName(error.name))?;
        validate_ref_name(thread).map_err(|error| HeddleError::InvalidRefName(error.name))?;
        Ok(self.remotes_dir().join(remote).join(thread))
    }
    pub(super) fn read_string(&self, path: &Path) -> Result<String> {
        let mut file = File::open(path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Ok(contents)
    }
    pub(super) fn read_optional_string(&self, path: &Path) -> Result<Option<String>> {
        if !path.exists() {
            return Ok(None);
        }
        self.read_string(path).map(Some)
    }
    pub(super) fn lock_refs(&self) -> Result<RefsLock> {
        std::fs::create_dir_all(self.refs_dir())?;
        let path = self.lock_path();
        let file = Self::open_lock_file(&path)?;
        let start_time = Instant::now();
        let mut delay = Duration::from_millis(5);

        loop {
            if start_time.elapsed() > Duration::from_secs(MAX_LOCK_WAIT_SECS) {
                return Err(HeddleError::Conflict(format!(
                    "timed out waiting for refs lock after {} seconds",
                    MAX_LOCK_WAIT_SECS
                )));
            }

            match file.try_lock_exclusive() {
                Ok(()) => return Ok(RefsLock { file, path }),
                Err(err) if is_lock_contended(&err) => {
                    let jitter_window = (delay.as_millis() as u64 / 2).max(1);
                    let jitter = rand::random::<u64>() % jitter_window;
                    thread::sleep(delay + Duration::from_millis(jitter));
                    delay = (delay * 2).min(Duration::from_millis(1000));
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    fn open_lock_file(path: &Path) -> Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(Into::into)
    }

    #[cfg(test)]
    fn try_lock_refs_for_test(&self) -> Result<Option<RefsLock>> {
        std::fs::create_dir_all(self.refs_dir())?;
        let path = self.lock_path();
        let file = Self::open_lock_file(&path)?;

        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(RefsLock { file, path })),
            Err(err) if is_lock_contended(&err) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub(super) fn write_string(&self, path: &Path, contents: &str) -> Result<()> {
        Ok(write_file_atomic(path, contents.as_bytes())?)
    }
    pub(super) fn write_string_temp(&self, path: &Path, contents: &str) -> Result<PathBuf> {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("invalid ref path"))?;
        std::fs::create_dir_all(parent)?;

        let suffix: u64 = rand::random();
        let temp_path = path.with_extension(format!("tmp-{}", suffix));
        let mut file = File::create(&temp_path)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        Ok(temp_path)
    }
    pub(super) fn list_refs_recursive(&self, dir: &Path, prefix: &str) -> Result<Vec<ThreadName>> {
        let mut refs = Vec::new();

        if !dir.exists() {
            return Ok(refs);
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };

            let full_name = ThreadName::from(if prefix.is_empty() {
                name.into()
            } else {
                format!("{}/{}", prefix, name)
            });

            if path.is_dir() {
                refs.extend(self.list_refs_recursive(&path, &full_name)?);
            } else if path.is_file() {
                refs.push(full_name);
            }
        }

        refs.sort();
        Ok(refs)
    }
}

fn is_lock_contended(err: &io::Error) -> bool {
    let lock_error = fs2::lock_contended_error();
    match lock_error.raw_os_error() {
        Some(code) => err.raw_os_error() == Some(code),
        None => err.kind() == lock_error.kind(),
    }
}

fn encode_flat_thread_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len() * 2 + FLAT_THREAD_SUFFIX.len());
    for byte in name.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{:02x}", byte);
    }
    out.push_str(FLAT_THREAD_SUFFIX);
    out
}

fn decode_flat_thread_name(file_name: &str) -> Option<String> {
    let encoded = file_name.strip_suffix(FLAT_THREAD_SUFFIX)?;
    if encoded.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(encoded.len() / 2);
    for idx in (0..encoded.len()).step_by(2) {
        let byte = u8::from_str_radix(&encoded[idx..idx + 2], 16).ok()?;
        bytes.push(byte);
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_lock_refs_basic() {
        let temp_dir = TempDir::new().unwrap();
        let repo = RefManager::new(temp_dir.path());
        let lock = repo.lock_refs().unwrap();
        assert!(repo.lock_path().exists());
        assert!(repo.try_lock_refs_for_test().unwrap().is_none());
        drop(lock);
        assert!(repo.lock_path().exists());
        assert!(repo.try_lock_refs_for_test().unwrap().is_some());
    }

    #[test]
    fn lock_refs_does_not_reap_old_lock_body_while_holder_is_alive() {
        let temp_dir = TempDir::new().unwrap();
        let repo = RefManager::new(temp_dir.path());
        let lock_path = repo.lock_path();
        fs::create_dir_all(repo.refs_dir()).unwrap();
        let old_lock_body = "99999 0";
        fs::write(&lock_path, old_lock_body).unwrap();

        let holder = repo.lock_refs().unwrap();
        assert!(repo.try_lock_refs_for_test().unwrap().is_none());

        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let root = temp_dir.path().to_path_buf();
        let waiter = thread::spawn(move || {
            let repo = RefManager::new(&root);
            started_tx.send(()).unwrap();
            let _lock = repo.lock_refs().unwrap();
            acquired_tx.send(()).unwrap();
        });

        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            acquired_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
        assert!(lock_path.exists());
        assert_eq!(fs::read_to_string(&lock_path).unwrap(), old_lock_body);

        drop(holder);
        acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        waiter.join().unwrap();
        assert!(lock_path.exists());
    }

    #[test]
    fn lock_refs_reclaims_when_owner_fd_closes() {
        let temp_dir = TempDir::new().unwrap();
        let repo = RefManager::new(temp_dir.path());

        let holder = repo.lock_refs().unwrap();
        assert!(repo.try_lock_refs_for_test().unwrap().is_none());

        drop(holder);
        let successor = repo.try_lock_refs_for_test().unwrap();
        assert!(successor.is_some());
        assert!(repo.lock_path().exists());
    }
}
