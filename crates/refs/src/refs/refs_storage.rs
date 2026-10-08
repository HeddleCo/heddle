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

use super::{RefManager, name::require_user_ref_name};
use crate::fs_atomic::{create_dir_all_durable, write_file_atomic};

const MAX_LOCK_WAIT_SECS: u64 = 10;

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
    /// v6 stores every name with the same portable, bounded encoding.
    pub(super) fn thread_path(&self, name: &ThreadName) -> Result<PathBuf> {
        require_user_ref_name(name)?;
        ThreadName::from_git_branch(&objects::name_encoding::git_name(name))
            .map_err(|error| HeddleError::InvalidRefName(error.to_string()))?;
        objects::name_encoding::verify_name_entry(&self.threads_dir(), name)?;
        Ok(self
            .threads_dir()
            .join(objects::name_encoding::name_path(name))
            .join("value"))
    }
    pub(super) fn marker_path(&self, name: &str) -> Result<PathBuf> {
        require_user_ref_name(name)?;
        objects::object::MarkerName::from_git_tag(&objects::name_encoding::git_name(name))
            .map_err(|error| HeddleError::InvalidRefName(error.to_string()))?;
        objects::name_encoding::verify_name_entry(&self.markers_dir(), name)?;
        Ok(self
            .markers_dir()
            .join(objects::name_encoding::name_path(name))
            .join("value"))
    }
    pub(super) fn remote_dir(&self, remote: &str) -> Result<PathBuf> {
        require_user_ref_name(remote)?;
        objects::name_encoding::verify_name_entry(&self.remotes_dir(), remote)?;
        Ok(self
            .remotes_dir()
            .join(objects::name_encoding::name_path(remote)))
    }
    pub(super) fn remote_thread_path(&self, remote: &str, thread: &str) -> Result<PathBuf> {
        require_user_ref_name(thread)?;
        ThreadName::from_git_branch(&objects::name_encoding::git_name(thread))
            .map_err(|error| HeddleError::InvalidRefName(error.to_string()))?;
        objects::name_encoding::verify_name_entry(&self.remote_dir(remote)?, thread)?;
        Ok(self
            .remote_dir(remote)?
            .join(objects::name_encoding::name_path(thread))
            .join("value"))
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
        create_dir_all_durable(&self.refs_dir())?;
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
        create_dir_all_durable(&self.refs_dir())?;
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

    /// Atomically replace a rebuildable materialized view without forcing it
    /// to stable storage. The authoritative oplog and its deliberately lagging
    /// watermark provide crash recovery for these bytes.
    pub(super) fn write_string_reconstructible(&self, path: &Path, contents: &str) -> Result<()> {
        let temp_path = self.alloc_temp_path(path)?;
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            file.write_all(contents.as_bytes())?;
            fs::rename(&temp_path, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result
    }

    /// Allocate the temp path a canonical ref file will be staged into
    /// (ensuring its parent directory exists), WITHOUT writing anything.
    ///
    /// Staging + fsync is done in one overlapped-writeback batch by
    /// [`stage_temp_files_durable`](objects::fs_atomic::stage_temp_files_durable)
    /// so publishing N refs pays ~1 fsync barrier's worth of latency instead of
    /// N serial ones (the `heddle import local` bulk-ref hot path).
    pub(super) fn alloc_temp_path(&self, path: &Path) -> Result<PathBuf> {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("invalid ref path"))?;
        create_dir_all_durable(parent)?;

        let suffix: u64 = rand::random();
        Ok(path.with_extension(format!("tmp-{}", suffix)))
    }
    /// Traverse encoding chunks only; `entry` is a terminal name directory.
    pub(super) fn list_refs_recursive(&self, dir: &Path, prefix: &str) -> Result<Vec<ThreadName>> {
        self.list_refs_recursive_at(dir, dir, prefix)
    }

    fn list_refs_recursive_at(
        &self,
        root: &Path,
        dir: &Path,
        prefix: &str,
    ) -> Result<Vec<ThreadName>> {
        let mut refs = Vec::new();
        if !dir.exists() {
            return Ok(refs);
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let segment = entry.file_name();
            let Some(segment) = segment.to_str() else {
                continue;
            };
            let relative = if prefix.is_empty() {
                segment.to_owned()
            } else {
                format!("{prefix}/{segment}")
            };
            if segment == "entry" {
                if let Some(name) =
                    objects::name_encoding::read_name_entry(root, Path::new(&relative))?
                {
                    refs.push(ThreadName::new(name));
                }
            } else if segment.starts_with("n-") || segment.starts_with("h-") || segment == "git" {
                refs.extend(self.list_refs_recursive_at(root, &entry.path(), &relative)?);
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

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn digest_ref_identity_is_verified_before_read_write_or_delete() {
        let temp = TempDir::new().expect("refs");
        let refs = RefManager::new(temp.path());
        refs.init().expect("init");
        let name = ThreadName::new("界".repeat(337));
        let state = crate::refs::fresh_state_id();
        refs.set_thread(&name, &state).expect("long thread");
        assert_eq!(refs.get_thread(&name).expect("read"), Some(state));
        let identity = refs
            .threads_dir()
            .join(objects::name_encoding::name_path(&name))
            .join("name");
        fs::write(identity, "another name").expect("corrupt identity");
        assert!(refs.get_thread(&name).is_err());
        assert!(refs.set_thread(&name, &state).is_err());
        assert!(refs.delete_thread(&name).is_err());
        assert!(refs.list_refs_recursive(&refs.threads_dir(), "").is_err());
    }

    #[test]
    fn longest_valid_multibyte_tag_round_trips_exactly() {
        let temp = TempDir::new().expect("refs");
        let refs = RefManager::new(temp.path());
        refs.init().expect("init");
        let git = "界".repeat(338);
        assert_eq!(format!("refs/tags/{git}").len(), 1024);
        let marker = objects::object::MarkerName::from_git_tag(&git).expect("longest full ref");
        let state = crate::refs::fresh_state_id();
        refs.create_marker(&marker, &state).expect("long tag");
        let reopened = RefManager::new(temp.path());
        assert_eq!(reopened.get_marker(&marker).expect("read"), Some(state));
        assert_eq!(reopened.list_markers().expect("list"), vec![marker]);
    }

    #[test]
    fn all_ref_namespaces_use_portable_exact_storage() {
        let temp = TempDir::new().expect("temp");
        let refs = RefManager::new(temp.path());
        refs.init().expect("refs");
        let state = crate::refs::fresh_state_id();
        let long = "界".repeat(337);
        let names = [
            "a,b",
            "ünicode/ブランチ",
            "CON",
            "con",
            "x'$(true)",
            "trailing\u{a0}",
            long.as_str(),
        ];
        for name in names {
            refs.set_thread(&ThreadName::new(name), &state)
                .expect("thread");
            refs.create_marker(&objects::object::MarkerName::new(name), &state)
                .expect("marker");
            refs.set_remote_thread("Origin", &ThreadName::new(name), &state)
                .expect("remote thread");
        }
        refs.set_remote_thread("origin", &ThreadName::new("other"), &state)
            .expect("case-distinct remote");
        let reopened = RefManager::new(temp.path());
        assert_eq!(reopened.list_threads().expect("threads").len(), names.len());
        assert_eq!(reopened.list_markers().expect("markers").len(), names.len());
        assert_eq!(
            reopened
                .list_remote_threads("Origin")
                .expect("remote names")
                .len(),
            names.len()
        );
        assert_eq!(
            reopened.list_remotes().expect("remotes"),
            ["Origin", "origin"]
        );
        for name in names {
            assert_eq!(
                reopened
                    .get_thread(&ThreadName::new(name))
                    .expect("read thread"),
                Some(state)
            );
            assert_eq!(
                reopened
                    .get_marker(&objects::object::MarkerName::new(name))
                    .expect("read marker"),
                Some(state)
            );
        }
    }

    #[test]
    fn marker_storage_uses_the_full_tag_ref_byte_limit() {
        let temp = TempDir::new().expect("temp");
        let refs = RefManager::new(temp.path());
        refs.init().expect("refs");
        let state = crate::refs::fresh_state_id();
        let name = "界".repeat(338);
        assert_eq!(format!("refs/tags/{name}").len(), 1024);
        let marker = objects::object::MarkerName::from_git_tag(&name).expect("full tag limit");
        refs.create_marker(&marker, &state).expect("marker storage");
        assert_eq!(
            refs.list_markers().expect("list").as_slice(),
            std::slice::from_ref(&marker)
        );
        assert_eq!(refs.get_marker(&marker).expect("read"), Some(state));
        let too_long = objects::object::MarkerName::new(format!("{name}x"));
        assert!(refs.create_marker(&too_long, &state).is_err());
    }

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
