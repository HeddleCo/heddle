// SPDX-License-Identifier: Apache-2.0
//! File storage helpers for refs.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process,
    thread::{self},
    time::{Duration, Instant},
};

use chrono::Utc;
use objects::{
    error::{HeddleError, Result},
    object::ThreadName,
};

use super::{RefManager, name::validate_ref_name};
use crate::fs_atomic::write_file_atomic;

const STALE_LOCK_TIMEOUT_SECS: i64 = 300;
const MAX_LOCK_WAIT_SECS: u64 = 10;
const FLAT_THREADS_DIR_NAME: &str = "__heddle_flat";
const FLAT_THREAD_SUFFIX: &str = ".ref";

pub(super) struct RefsLock {
    path: PathBuf,
}

impl Drop for RefsLock {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_file(&self.path) {
            eprintln!(
                "Warning: failed to remove refs lock file {}: {}",
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
    /// Path of the heddle-internal pre-undo recovery pointer. Deliberately
    /// a root-level sibling of `HEAD` (ORIG_HEAD-style), OUTSIDE the
    /// user-writable ref namespaces under `refs/` (threads, markers,
    /// remotes). Keeping it here makes a collision with a user marker named
    /// `undo-recovery` impossible by construction: the marker CLI only ever
    /// touches `refs/markers/`.
    pub(super) fn undo_recovery_path(&self) -> PathBuf {
        self.root.join("UNDO_RECOVERY")
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
        let current_pid = process::id();
        let start_time = Instant::now();
        let mut delay = Duration::from_millis(5);

        loop {
            if start_time.elapsed() > Duration::from_secs(MAX_LOCK_WAIT_SECS) {
                return Err(HeddleError::Conflict(format!(
                    "timed out waiting for refs lock after {} seconds",
                    MAX_LOCK_WAIT_SECS
                )));
            }

            if let Ok(content) = self.read_string(&path)
                && let Some((pid, ts)) = Self::parse_lock_content(&content)
            {
                let now = Utc::now().timestamp();
                if pid != current_pid && now - ts > STALE_LOCK_TIMEOUT_SECS {
                    let _ = fs::remove_file(&path);
                }
            }

            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    let now = Utc::now().timestamp();
                    let content = format!("{} {}", current_pid, now);
                    let file = write_lock_body_or_cleanup(file, content.as_bytes(), &path)?;
                    if let Err(err) = file.sync_all() {
                        // Drop-before-remove is load-bearing on Windows.
                        // See `write_lock_body_or_cleanup` doc-comment.
                        drop(file);
                        let _ = fs::remove_file(&path);
                        return Err(err.into());
                    }
                    return Ok(RefsLock { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    let jitter_window = (delay.as_millis() as u64 / 2).max(1);
                    let jitter = rand::random::<u64>() % jitter_window;
                    thread::sleep(delay + Duration::from_millis(jitter));
                    delay = (delay * 2).min(Duration::from_millis(1000));
                }
                Err(err) => return Err(err.into()),
            }
        }
    }
    fn parse_lock_content(content: &str) -> Option<(u32, i64)> {
        let parts: Vec<&str> = content.split_whitespace().collect();
        if parts.len() == 2 {
            if let (Ok(pid), Ok(ts)) = (parts[0].parse::<u32>(), parts[1].parse::<i64>()) {
                Some((pid, ts))
            } else {
                None
            }
        } else {
            None
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

/// Write `body` to `writer`; on failure drop the writer (closing any
/// underlying OS handle) and remove the orphan at `cleanup_path`,
/// returning the original write error. On success, return the writer
/// so the caller can continue using it (e.g. for `sync_all`).
///
/// Generic over `Write` so tests can inject a failing writer to
/// exercise the cleanup branch — the production caller passes the
/// freshly-`create_new`'d lock file by value.
///
/// **Drop-before-remove is load-bearing on Windows.** Without
/// `FILE_SHARE_DELETE`, `DeleteFile` against a still-open handle fails
/// with `ERROR_SHARING_VIOLATION`. Taking the writer by value makes
/// ownership obvious at the call site and forces the close to happen
/// before `remove_file`. Lifted from the heddle#86 r2 helper in
/// `shared_target.rs`.
fn write_lock_body_or_cleanup<W: Write>(
    mut writer: W,
    body: &[u8],
    cleanup_path: &Path,
) -> Result<W> {
    match writer.write_all(body) {
        Ok(()) => Ok(writer),
        Err(err) => {
            drop(writer);
            let _ = fs::remove_file(cleanup_path);
            Err(err.into())
        }
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
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_parse_lock_content_valid() {
        let content = "1234 1640995200";
        let result = RefManager::parse_lock_content(content);
        assert_eq!(result, Some((1234, 1640995200)));
    }

    #[test]
    fn test_parse_lock_content_invalid() {
        assert_eq!(RefManager::parse_lock_content("invalid"), None);
        assert_eq!(RefManager::parse_lock_content("1234"), None);
        assert_eq!(RefManager::parse_lock_content("abc 123"), None);
    }

    #[test]
    fn test_lock_refs_basic() {
        let temp_dir = TempDir::new().unwrap();
        let repo = RefManager::new(temp_dir.path());
        let lock = repo.lock_refs().unwrap();
        assert!(repo.lock_path().exists());
        drop(lock);
        assert!(!repo.lock_path().exists());
    }

    /// `Write` wrapper that flips a flag from its `Drop` impl. The
    /// regression test below uses this to assert the writer is closed
    /// before the helper's `remove_file` call — load-bearing on Windows,
    /// where `DeleteFile` against a still-open handle fails with
    /// `ERROR_SHARING_VIOLATION` and the orphan would otherwise survive.
    /// Mirrors `DropTrackingFailingWriter` from
    /// `shared_target.rs` (heddle#86 r2).
    struct DropTrackingFailingWriter<'a> {
        dropped: &'a std::cell::Cell<bool>,
    }
    impl Write for DropTrackingFailingWriter<'_> {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("simulated write failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Drop for DropTrackingFailingWriter<'_> {
        fn drop(&mut self) {
            self.dropped.set(true);
        }
    }

    #[test]
    fn write_lock_body_or_cleanup_drops_writer_before_returning_on_failure() {
        // Regression for heddle#95 (sweep follow-up to heddle#86 r2). On
        // Windows, `remove_file` against a still-open handle fails with
        // `ERROR_SHARING_VIOLATION`; the lock file stays, and the next
        // operation waits the full 5-minute stale-lock timeout. The
        // helper must take the writer by value and drop it before
        // `remove_file`. POSIX would let the unlink succeed against an
        // open handle, so this test asserts the ownership-transfer
        // guarantee directly: by the time the helper returns on the
        // failure path, the writer has been dropped (i.e. on a real
        // `File`, the OS handle is closed).
        let temp = TempDir::new().unwrap();
        let orphan = temp.path().join("LOCK");
        fs::write(&orphan, b"").unwrap();

        let dropped = std::cell::Cell::new(false);
        let writer = DropTrackingFailingWriter { dropped: &dropped };
        let result = write_lock_body_or_cleanup(writer, b"would-be body", &orphan);

        assert!(result.is_err());
        assert!(
            dropped.get(),
            "writer must be dropped before the helper returns on failure — \
             on Windows, the file handle must be closed before remove_file"
        );
        assert!(!orphan.exists());
    }

    #[test]
    fn test_lock_refs_stale_removal() {
        let temp_dir = TempDir::new().unwrap();
        let repo = RefManager::new(temp_dir.path());
        let lock_path = repo.lock_path();
        fs::create_dir_all(repo.refs_dir()).unwrap();
        let fake_pid = 99999;
        let old_ts = Utc::now().timestamp() - STALE_LOCK_TIMEOUT_SECS - 1;
        let content = format!("{} {}", fake_pid, old_ts);
        fs::write(&lock_path, content).unwrap();
        let _lock = repo.lock_refs().unwrap();
        assert!(lock_path.exists());
    }
}
