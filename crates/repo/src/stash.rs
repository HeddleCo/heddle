// SPDX-License-Identifier: Apache-2.0
//! Stash storage and operations.

use std::{fs, path::PathBuf};

use objects::{
    fs_atomic::{sync_directory, temp_path, write_file_atomic},
    fs_ops::remove_path_recursively,
    lock::RepoLock,
    object::ContentHash,
};
use serde::{Deserialize, Serialize};

use crate::{Repository, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StashId([u8; 16]);

impl StashId {
    fn generate() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(1);
        let mut hasher = blake3::Hasher::new();
        hasher.update(
            &chrono::Utc::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
                .to_le_bytes(),
        );
        hasher.update(&NEXT.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
        Self(bytes)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StashEntry {
    pub index: usize,
    pub stash_id: StashId,
    pub tree_hash: String,
    pub parent_tree_hash: String,
    pub message: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub struct StashManager {
    stash_dir: PathBuf,
    lock: RepoLock,
}

impl StashManager {
    pub fn new(heddle_dir: impl AsRef<std::path::Path>) -> Self {
        Self {
            stash_dir: heddle_dir.as_ref().join("stashes"),
            lock: RepoLock::at(heddle_dir.as_ref().join("locks/stash.lock")),
        }
    }

    pub fn init(&self) -> Result<()> {
        if !self.stash_dir.exists() {
            fs::create_dir_all(&self.stash_dir)?;
        }
        Ok(())
    }

    pub fn push(
        &self,
        tree_hash: ContentHash,
        parent_tree_hash: String,
        message: Option<String>,
    ) -> Result<StashEntry> {
        let _lock = self.write_lock()?;
        let stashes = self.list_unlocked()?;

        let stash_id = StashId::generate();

        let entry = StashEntry {
            index: stashes.len(),
            stash_id,
            tree_hash: tree_hash.to_string(),
            parent_tree_hash,
            message,
            created_at: chrono::Utc::now(),
        };

        let entry_path = self.stash_dir.join(format!("{}", entry.index));
        let content = serde_json::to_string(&entry)?;
        write_file_atomic(&entry_path, content.as_bytes())?;

        Ok(entry)
    }

    pub fn list(&self) -> Result<Vec<StashEntry>> {
        let _lock = self.read_lock()?;
        self.list_unlocked()
    }

    fn list_unlocked(&self) -> Result<Vec<StashEntry>> {
        if !self.stash_dir.exists() {
            return Ok(Vec::new());
        }

        let mut stashes = Vec::new();

        for entry in fs::read_dir(&self.stash_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().is_none()
                && let Ok(content) = fs::read_to_string(&path)
                && let Ok(stash) = serde_json::from_str::<StashEntry>(&content)
            {
                stashes.push(stash);
            }
        }

        stashes.sort_by_key(|s| s.index);
        Ok(stashes)
    }

    pub fn top(&self) -> Result<Option<StashEntry>> {
        let stashes = self.list()?;
        Ok(stashes.last().cloned())
    }

    pub fn drop(&self) -> Result<Option<StashEntry>> {
        let _lock = self.write_lock()?;
        let mut stashes = self.list_unlocked()?;

        if stashes.is_empty() {
            return Ok(None);
        }

        let removed = stashes.pop();
        self.rewrite_unlocked(&mut stashes)?;
        Ok(removed)
    }

    pub fn pop_with<F>(&self, apply: F) -> Result<Option<StashEntry>>
    where
        F: FnOnce(&StashEntry) -> Result<()>,
    {
        let _lock = self.write_lock()?;
        let mut stashes = self.list_unlocked()?;

        let Some(removed) = stashes.pop() else {
            return Ok(None);
        };

        apply(&removed)?;
        self.rewrite_unlocked(&mut stashes)?;
        Ok(Some(removed))
    }

    fn rewrite_unlocked(&self, stashes: &mut [StashEntry]) -> Result<()> {
        let parent = self
            .stash_dir
            .parent()
            .ok_or_else(|| std::io::Error::other("invalid stash directory"))?;
        fs::create_dir_all(parent)?;

        let replacement_dir = temp_path(&self.stash_dir);
        fs::create_dir_all(&replacement_dir)?;

        for (new_index, entry) in stashes.iter_mut().enumerate() {
            entry.index = new_index;
            let path = replacement_dir.join(format!("{}", new_index));
            let content = serde_json::to_string(entry)?;
            write_file_atomic(&path, content.as_bytes())?;
        }

        sync_directory(&replacement_dir)?;

        let backup_dir = self.stash_dir.with_extension("old");
        remove_stash_path(&backup_dir)?;
        fs::rename(&self.stash_dir, &backup_dir)?;
        sync_directory(parent)?;
        if let Err(error) = fs::rename(&replacement_dir, &self.stash_dir) {
            fs::rename(&backup_dir, &self.stash_dir)?;
            sync_directory(parent)?;
            return Err(error.into());
        }
        sync_directory(parent)?;
        remove_stash_path(&backup_dir)?;
        sync_directory(parent)?;

        Ok(())
    }

    pub fn clear(&self) -> Result<usize> {
        let _lock = self.write_lock()?;
        let stashes = self.list_unlocked()?;
        let count = stashes.len();

        if self.stash_dir.exists() {
            if self.stash_dir.is_symlink() {
                fs::remove_file(&self.stash_dir)?;
            } else {
                remove_path_recursively(&self.stash_dir)?;
            }
        }
        fs::create_dir_all(&self.stash_dir)?;

        Ok(count)
    }

    fn read_lock(&self) -> Result<objects::lock::ReadLockGuard> {
        self.lock.read().map_err(|err| {
            std::io::Error::other(format!("failed to acquire stash lock: {err}")).into()
        })
    }

    fn write_lock(&self) -> Result<objects::lock::WriteLockGuard> {
        self.lock.write().map_err(|err| {
            std::io::Error::other(format!("failed to acquire stash lock: {err}")).into()
        })
    }
}

fn remove_stash_path(path: &std::path::Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    if path.is_symlink() {
        fs::remove_file(path)?;
    } else {
        remove_path_recursively(path)?;
    }

    Ok(())
}

impl Repository {
    pub fn stash_manager(&self) -> StashManager {
        StashManager::new(self.heddle_dir())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Barrier},
        thread,
    };

    use tempfile::TempDir;

    use super::*;

    fn create_manager() -> (TempDir, StashManager) {
        let temp_dir = TempDir::new().unwrap();
        let heddle_dir = temp_dir.path().join(".heddle");
        let manager = StashManager::new(&heddle_dir);
        manager.init().unwrap();
        (temp_dir, manager)
    }

    #[test]
    fn test_drop_rewrites_remaining_entries() {
        let (_temp_dir, manager) = create_manager();
        let first = manager
            .push(ContentHash::compute(b"one"), "parent-1".to_string(), None)
            .unwrap();
        let second = manager
            .push(ContentHash::compute(b"two"), "parent-2".to_string(), None)
            .unwrap();
        let third = manager
            .push(ContentHash::compute(b"three"), "parent-3".to_string(), None)
            .unwrap();

        let removed = manager.drop().unwrap().unwrap();
        assert_eq!(removed.stash_id, third.stash_id);

        let remaining = manager.list().unwrap();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].index, 0);
        assert_eq!(remaining[0].stash_id, first.stash_id);
        assert_eq!(remaining[1].index, 1);
        assert_eq!(remaining[1].stash_id, second.stash_id);

        let temp_entries = fs::read_dir(&manager.stash_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(temp_entries, 0);
        assert!(!manager.stash_dir.with_extension("old").exists());
    }

    #[test]
    fn test_pop_with_drops_only_after_successful_apply() {
        let (_temp_dir, manager) = create_manager();
        let first = manager
            .push(ContentHash::compute(b"one"), "parent-1".to_string(), None)
            .unwrap();
        let second = manager
            .push(ContentHash::compute(b"two"), "parent-2".to_string(), None)
            .unwrap();

        let error = manager
            .pop_with(|_| Err(std::io::Error::other("apply failed").into()))
            .unwrap_err();
        assert!(error.to_string().contains("apply failed"));
        assert_eq!(manager.list().unwrap().len(), 2);

        let applied = manager
            .pop_with(|stash| {
                assert_eq!(stash.stash_id, second.stash_id);
                Ok(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(applied.stash_id, second.stash_id);

        let remaining = manager.list().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].index, 0);
        assert_eq!(remaining[0].stash_id, first.stash_id);
    }

    #[test]
    fn test_concurrent_pushes_preserve_all_entries() {
        let (_temp_dir, manager) = create_manager();
        let manager = Arc::new(manager);
        let barrier = Arc::new(Barrier::new(9));
        let mut handles = Vec::new();

        for i in 0..8 {
            let manager = Arc::clone(&manager);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                manager
                    .push(
                        ContentHash::compute(format!("tree-{i}").as_bytes()),
                        format!("parent-{i}"),
                        Some(format!("stash-{i}")),
                    )
                    .unwrap();
            }));
        }

        barrier.wait();

        for handle in handles {
            handle.join().unwrap();
        }

        let stashes = manager.list().unwrap();
        assert_eq!(stashes.len(), 8);

        let mut indices: Vec<_> = stashes.iter().map(|entry| entry.index).collect();
        indices.sort_unstable();
        assert_eq!(indices, (0..8).collect::<Vec<_>>());

        let change_ids: std::collections::HashSet<_> =
            stashes.iter().map(|entry| entry.stash_id).collect();
        assert_eq!(change_ids.len(), 8);
    }
}
