// SPDX-License-Identifier: Apache-2.0
//! Repository locking for concurrent access.

use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
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

pub struct ReadLockGuard {
    _file: File,
}

impl Drop for ReadLockGuard {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

pub struct WriteLockGuard {
    _file: File,
}

impl Drop for WriteLockGuard {
    fn drop(&mut self) {
        let _ = self._file.unlock();
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
        let file = self.open_lock_file()?;
        file.lock_shared().map_err(LockError::Acquire)?;
        Ok(ReadLockGuard { _file: file })
    }

    pub fn write(&self) -> Result<WriteLockGuard> {
        self.ensure_lock_dir()?;
        let file = self.open_lock_file()?;
        file.lock_exclusive().map_err(LockError::Acquire)?;
        Ok(WriteLockGuard { _file: file })
    }

    pub fn try_read(&self) -> Result<Option<ReadLockGuard>> {
        self.ensure_lock_dir()?;
        let file = self.open_lock_file()?;

        match file.try_lock_shared() {
            Ok(()) => Ok(Some(ReadLockGuard { _file: file })),
            Err(_) => Ok(None),
        }
    }

    pub fn try_write(&self) -> Result<Option<WriteLockGuard>> {
        self.ensure_lock_dir()?;
        let file = self.open_lock_file()?;

        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(WriteLockGuard { _file: file })),
            Err(_) => Ok(None),
        }
    }

    fn ensure_lock_dir(&self) -> Result<()> {
        if let Some(parent) = self.lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(LockError::Io)?;
        }
        Ok(())
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
    use std::{sync::Arc, thread};

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
}
