// SPDX-License-Identifier: Apache-2.0
//! Repository locking for concurrent access.
//!
//! [`RepoLock`] guarantees three invariants:
//! - **Cross-process** exclusion via `flock(2)` on a lock file.
//! - **Cross-thread, same-process** exclusion: two threads never both hold the
//!   write lock.
//! - **Same-thread reentrancy**: the owning thread may re-acquire the write lock
//!   any number of times without blocking.
//!
//! The reentrancy invariant matters because `flock(2)` locks attach to the open
//! file description, not the process: a single thread that opens the lock file
//! twice and calls `flock` on the second fd blocks forever on its own first
//! lock. The canonical write lock is taken at the top of an import and then
//! re-taken by downstream writers on the same thread, so a non-reentrant
//! primitive self-deadlocks. We therefore hold the `flock` once on the outermost
//! acquisition and gate intra-process access through a per-lock-path registry.

use std::{
    collections::HashMap,
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock},
    thread::{self, ThreadId},
};

use fs2::FileExt;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LockError {
    #[error("failed to acquire lock: {0}")]
    Acquire(#[source] io::Error),
    #[error("lock file not accessible: {0}")]
    Io(#[source] io::Error),
}

pub type Result<T> = std::result::Result<T, LockError>;

/// Intra-process state for a single lock path. `flock` holds the OS-level lock
/// file while owned; dropping it releases the cross-process lock.
struct GateState {
    owner: Option<ThreadId>,
    depth: usize,
    flock: Option<File>,
}

struct Entry {
    gate: Mutex<GateState>,
    cv: Condvar,
}

impl Entry {
    fn new() -> Self {
        Self {
            gate: Mutex::new(GateState {
                owner: None,
                depth: 0,
                flock: None,
            }),
            cv: Condvar::new(),
        }
    }
}

/// Process-global registry of per-lock-path gates, keyed by the canonical lock
/// path. Entries are created on first use and never removed (one small entry per
/// distinct lock path per process lifetime).
static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Arc<Entry>>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Entry>>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn entry_for(key: PathBuf) -> Arc<Entry> {
    let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
    Arc::clone(map.entry(key).or_insert_with(|| Arc::new(Entry::new())))
}

fn lock_gate(entry: &Entry) -> MutexGuard<'_, GateState> {
    entry.gate.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct ReadLockGuard {
    // `None` when this is a no-op guard: the current thread already holds the
    // write lock, whose exclusive flock subsumes a shared read.
    _file: Option<File>,
}

impl Drop for ReadLockGuard {
    fn drop(&mut self) {
        if let Some(file) = &self._file {
            let _ = file.unlock();
        }
    }
}

pub struct WriteLockGuard {
    entry: Arc<Entry>,
}

impl Drop for WriteLockGuard {
    fn drop(&mut self) {
        let mut state = lock_gate(&self.entry);
        if state.depth > 0 {
            state.depth -= 1;
        }
        if state.depth == 0 {
            state.owner = None;
            // Dropping the File releases the cross-process flock.
            state.flock = None;
            self.entry.cv.notify_one();
        }
    }
}

pub struct RepoLock {
    lock_path: PathBuf,
}

impl RepoLock {
    pub fn new(repo_root: &Path) -> Self {
        let lock_path = repo_root.join(".heddle/locks/repo.lock");
        Self { lock_path }
    }

    pub fn at(lock_path: PathBuf) -> Self {
        Self { lock_path }
    }

    pub fn read(&self) -> Result<ReadLockGuard> {
        self.ensure_lock_dir()?;
        let entry = entry_for(self.registry_key());

        // If the current thread already holds the write lock, the exclusive
        // flock covers this read; hand back a no-op guard so a same-thread
        // read-under-write cannot deadlock against our own flock.
        {
            let state = lock_gate(&entry);
            if state.owner == Some(thread::current().id()) {
                return Ok(ReadLockGuard { _file: None });
            }
        }

        let file = self.open_lock_file()?;
        file.lock_shared().map_err(LockError::Acquire)?;
        Ok(ReadLockGuard { _file: Some(file) })
    }

    pub fn write(&self) -> Result<WriteLockGuard> {
        self.ensure_lock_dir()?;
        let entry = entry_for(self.registry_key());
        let tid = thread::current().id();
        let mut state = lock_gate(&entry);
        loop {
            match state.owner {
                Some(owner) if owner == tid => {
                    state.depth += 1;
                    return Ok(WriteLockGuard {
                        entry: Arc::clone(&entry),
                    });
                }
                None => {
                    // Acquire the cross-process flock once for the outermost
                    // holder. Holding the gate across this blocking call is
                    // intentional: other local threads must block here until we
                    // either win the flock or fail.
                    let file = self.open_lock_file()?;
                    file.lock_exclusive().map_err(LockError::Acquire)?;
                    state.owner = Some(tid);
                    state.depth = 1;
                    state.flock = Some(file);
                    return Ok(WriteLockGuard {
                        entry: Arc::clone(&entry),
                    });
                }
                Some(_) => {
                    state = entry.cv.wait(state).unwrap_or_else(|e| e.into_inner());
                }
            }
        }
    }

    pub fn try_read(&self) -> Result<Option<ReadLockGuard>> {
        self.ensure_lock_dir()?;
        let file = self.open_lock_file()?;

        match file.try_lock_shared() {
            Ok(()) => Ok(Some(ReadLockGuard { _file: Some(file) })),
            Err(_) => Ok(None),
        }
    }

    pub fn try_write(&self) -> Result<Option<WriteLockGuard>> {
        self.ensure_lock_dir()?;
        let entry = entry_for(self.registry_key());
        let mut state = lock_gate(&entry);
        // Non-blocking acquisition is NON-reentrant: a `try_write` while the lock
        // is held — by ANY thread, including this one — reports contention
        // (`None`). Reentrancy exists only to keep the blocking `write()` from
        // self-deadlocking on its own `flock`; a `try_*` can never deadlock, so a
        // caller that uses it to detect contention (e.g. the undo/redo
        // serialization lock, heddle#355) must see "held" regardless of holder.
        match state.owner {
            Some(_) => Ok(None),
            None => {
                let file = self.open_lock_file()?;
                match file.try_lock_exclusive() {
                    Ok(()) => {
                        state.owner = Some(thread::current().id());
                        state.depth = 1;
                        state.flock = Some(file);
                        Ok(Some(WriteLockGuard {
                            entry: Arc::clone(&entry),
                        }))
                    }
                    Err(_) => Ok(None),
                }
            }
        }
    }

    fn ensure_lock_dir(&self) -> Result<()> {
        if let Some(parent) = self.lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(LockError::Io)?;
        }
        Ok(())
    }

    /// Stable registry key for this lock path. The lock file itself may not exist
    /// yet, so canonicalize the (already-created) parent directory and re-join
    /// the filename rather than the whole path.
    fn registry_key(&self) -> PathBuf {
        match self.lock_path.parent() {
            Some(parent) => {
                let canon_parent = parent
                    .canonicalize()
                    .unwrap_or_else(|_| parent.to_path_buf());
                match self.lock_path.file_name() {
                    Some(name) => canon_parent.join(name),
                    None => canon_parent,
                }
            }
            None => self.lock_path.clone(),
        }
    }

    fn open_lock_file(&self) -> Result<File> {
        File::create(&self.lock_path).map_err(LockError::Io)
    }
}

pub trait RepositoryLockExt {
    fn locker(&self) -> RepoLock;
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            mpsc::{self},
        },
        thread,
    };

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_read_lock_acquired() {
        let temp = TempDir::new().unwrap();
        let lock = RepoLock::new(temp.path());

        let guard = lock.read().unwrap();
        assert!(std::mem::size_of_val(&guard) > 0);
    }

    #[test]
    fn test_write_lock_acquired() {
        let temp = TempDir::new().unwrap();
        let lock = RepoLock::new(temp.path());

        let guard = lock.write().unwrap();
        assert!(std::mem::size_of_val(&guard) > 0);
    }

    #[test]
    fn test_multiple_readers() {
        let temp = TempDir::new().unwrap();
        let lock = Arc::new(RepoLock::new(temp.path()));

        let mut handles = vec![];
        for _ in 0..10 {
            let lock = Arc::clone(&lock);
            let handle = thread::spawn(move || {
                let _guard = lock.read().unwrap();
                thread::sleep(std::time::Duration::from_millis(10));
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn test_writer_excludes_reader() {
        let temp = TempDir::new().unwrap();
        let lock = Arc::new(RepoLock::new(temp.path()));

        let _write_guard = lock.write().unwrap();
        let read_result = lock.try_read().unwrap();
        assert!(read_result.is_none(), "Reader should be blocked by writer");
    }

    #[test]
    fn test_reader_excludes_writer() {
        let temp = TempDir::new().unwrap();
        let lock = Arc::new(RepoLock::new(temp.path()));

        let _read_guard = lock.read().unwrap();
        let write_result = lock.try_write().unwrap();
        assert!(write_result.is_none(), "Writer should be blocked by reader");
    }

    #[test]
    fn test_lock_released_on_drop() {
        let temp = TempDir::new().unwrap();
        let lock = RepoLock::new(temp.path());

        {
            let _guard = lock.write().unwrap();
        }

        let _guard2 = lock.read().unwrap();
    }

    /// The owning thread may re-take the write lock without blocking on its own
    /// flock — the regression that self-deadlocked the canonical import lock.
    #[test]
    fn same_thread_write_is_reentrant() {
        let temp = TempDir::new().unwrap();
        let lock = RepoLock::new(temp.path());

        let _a = lock.write().unwrap();
        let _b = lock.write().unwrap();
        // Reaching here without hanging is the assertion (harness timeout is the
        // backstop on regression).
    }

    /// A read taken by the thread that already holds the write lock must not
    /// block against its own exclusive flock.
    #[test]
    fn same_thread_read_under_write_does_not_deadlock() {
        let temp = TempDir::new().unwrap();
        let lock = RepoLock::new(temp.path());

        let _w = lock.write().unwrap();
        let _r = lock.read().unwrap();
    }

    /// Reentrancy is strictly per-thread: while one thread holds the write lock,
    /// a different thread is excluded.
    #[test]
    fn distinct_threads_still_exclude() {
        let temp = TempDir::new().unwrap();
        let lock = Arc::new(RepoLock::new(temp.path()));

        let (acquired_tx, acquired_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let lock_a = Arc::clone(&lock);
        let handle = thread::spawn(move || {
            let _g = lock_a.write().unwrap();
            acquired_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });

        acquired_rx.recv().unwrap();
        assert!(
            lock.try_write().unwrap().is_none(),
            "a second thread must not acquire the write lock"
        );

        release_tx.send(()).unwrap();
        handle.join().unwrap();

        assert!(
            lock.try_write().unwrap().is_some(),
            "write lock is available once the owning thread releases"
        );
    }

    /// A reentrant (depth > 1) hold keeps the lock until the OUTERMOST guard
    /// drops; other threads stay excluded across the inner drops.
    #[test]
    fn reentrant_release_keeps_lock_until_outermost_drop() {
        let temp = TempDir::new().unwrap();
        let lock = Arc::new(RepoLock::new(temp.path()));

        let a1 = lock.write().unwrap();
        let a2 = lock.write().unwrap();

        let other = |lock: &Arc<RepoLock>| {
            let lock = Arc::clone(lock);
            thread::spawn(move || lock.try_write().unwrap().is_none())
                .join()
                .unwrap()
        };

        assert!(other(&lock), "excluded while held at depth 2");
        drop(a2);
        assert!(other(&lock), "still excluded while held at depth 1");
        drop(a1);

        let lock_b = Arc::clone(&lock);
        let now_available = thread::spawn(move || lock_b.try_write().unwrap().is_some())
            .join()
            .unwrap();
        assert!(now_available, "available after the outermost guard drops");
    }

    /// `try_write` is intentionally NON-reentrant: even the thread that already
    /// holds the write lock gets `None`, not a nested guard. Reentrancy exists
    /// only so the blocking `write()` can't self-deadlock on its own `flock`; a
    /// non-blocking `try_*` can never deadlock, and callers use it to DETECT
    /// contention (the undo/redo serialization lock, heddle#355), so it must
    /// report "held" regardless of holder. Do NOT "fix" this to mirror
    /// `write()`'s reentrancy.
    #[test]
    fn try_write_is_non_reentrant_even_for_owner() {
        let temp = TempDir::new().unwrap();
        let lock = RepoLock::new(temp.path());

        let _held = lock.write().unwrap();
        assert!(
            lock.try_write().unwrap().is_none(),
            "try_write must report contention even for the lock's own owner thread"
        );
    }
}
