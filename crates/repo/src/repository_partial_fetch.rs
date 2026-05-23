// SPDX-License-Identifier: Apache-2.0
//! Repository-local metadata for explicit partial-fetch tracking.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use objects::{
    error::{HeddleError, Result},
    fs_atomic::write_file_atomic,
    lock::RepoLock,
    object::ContentHash,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MissingBlob {
    hash: ContentHash,
}

impl MissingBlob {
    pub fn new(hash: ContentHash) -> Self {
        Self { hash }
    }

    pub fn hash(&self) -> ContentHash {
        self.hash
    }

    fn parse(line: &str) -> Option<Self> {
        let hash = line.strip_prefix("blob ")?;
        let hash = ContentHash::from_hex(hash).ok()?;
        Some(Self { hash })
    }

    fn encode(&self) -> String {
        format!("blob {}", self.hash.to_hex())
    }
}

#[derive(Clone, Debug)]
pub struct PartialFetchMetadata {
    path: PathBuf,
    missing_blobs: HashSet<MissingBlob>,
}

impl PartialFetchMetadata {
    pub fn load(heddle_dir: &Path) -> Result<Self> {
        let path = heddle_dir.join("partial-fetch");
        let missing_blobs = if path.exists() {
            let contents = fs::read_to_string(&path)?;
            contents
                .lines()
                .filter_map(|line| {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        None
                    } else {
                        MissingBlob::parse(line)
                    }
                })
                .collect()
        } else {
            HashSet::new()
        };

        Ok(Self {
            path,
            missing_blobs,
        })
    }

    pub fn missing_blobs(&self) -> Vec<ContentHash> {
        let mut blobs: Vec<_> = self.missing_blobs.iter().map(MissingBlob::hash).collect();
        blobs.sort_by_key(|hash| hash.to_hex());
        blobs
    }

    pub fn is_missing_blob(&self, hash: &ContentHash) -> bool {
        self.missing_blobs.contains(&MissingBlob::new(*hash))
    }

    pub fn record_missing_blob(&mut self, hash: ContentHash) -> Result<bool> {
        let inserted = self.missing_blobs.insert(MissingBlob::new(hash));
        if inserted {
            self.save()?;
        }
        Ok(inserted)
    }

    pub fn clear_missing_blob(&mut self, hash: &ContentHash) -> Result<bool> {
        let removed = self.missing_blobs.remove(&MissingBlob::new(*hash));
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn clear_all_missing_blobs(&mut self) -> Result<bool> {
        if self.missing_blobs.is_empty() {
            return Ok(false);
        }
        self.missing_blobs.clear();
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> Result<()> {
        let mut contents: Vec<_> = self.missing_blobs.iter().map(MissingBlob::encode).collect();
        contents.sort();
        let contents = if contents.is_empty() {
            String::new()
        } else {
            format!("{}\n", contents.join("\n"))
        };

        write_file_atomic(&self.path, contents.as_bytes())?;
        Ok(())
    }
}

pub struct PartialFetchMetadataManager {
    heddle_dir: PathBuf,
    lock: RepoLock,
}

impl PartialFetchMetadataManager {
    pub fn new(heddle_dir: impl AsRef<Path>) -> Self {
        let heddle_dir = heddle_dir.as_ref();
        Self {
            heddle_dir: heddle_dir.to_path_buf(),
            lock: RepoLock::at(heddle_dir.join("locks/partial_fetch.lock")),
        }
    }

    pub fn record_missing_blob(&self, hash: ContentHash) -> Result<bool> {
        let _lock = self.write_lock()?;
        let mut partial = PartialFetchMetadata::load(&self.heddle_dir)?;
        partial.record_missing_blob(hash)
    }

    pub fn clear_missing_blob(&self, hash: &ContentHash) -> Result<bool> {
        if !self.metadata_path().exists() {
            return Ok(false);
        }
        let _lock = self.write_lock()?;
        if !self.metadata_path().exists() {
            return Ok(false);
        }
        let mut partial = PartialFetchMetadata::load(&self.heddle_dir)?;
        partial.clear_missing_blob(hash)
    }

    pub fn missing_blobs(&self) -> Result<Vec<ContentHash>> {
        if !self.metadata_path().exists() {
            return Ok(Vec::new());
        }
        let _lock = self.read_lock()?;
        Ok(PartialFetchMetadata::load(&self.heddle_dir)?.missing_blobs())
    }

    pub fn clear_all_missing_blobs(&self) -> Result<bool> {
        if !self.metadata_path().exists() {
            return Ok(false);
        }
        let _lock = self.write_lock()?;
        if !self.metadata_path().exists() {
            return Ok(false);
        }
        let mut partial = PartialFetchMetadata::load(&self.heddle_dir)?;
        partial.clear_all_missing_blobs()
    }

    pub fn is_missing_blob(&self, hash: &ContentHash) -> Result<bool> {
        if !self.metadata_path().exists() {
            return Ok(false);
        }
        let _lock = self.read_lock()?;
        Ok(PartialFetchMetadata::load(&self.heddle_dir)?.is_missing_blob(hash))
    }

    fn metadata_path(&self) -> PathBuf {
        self.heddle_dir.join("partial-fetch")
    }

    fn read_lock(&self) -> Result<objects::lock::ReadLockGuard> {
        self.lock
            .read()
            .map_err(|err| HeddleError::Io(std::io::Error::other(err.to_string())))
    }

    fn write_lock(&self) -> Result<objects::lock::WriteLockGuard> {
        self.lock
            .write()
            .map_err(|err| HeddleError::Io(std::io::Error::other(err.to_string())))
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, thread};

    use tempfile::TempDir;

    use super::*;

    fn setup_manager() -> (TempDir, Arc<PartialFetchMetadataManager>) {
        let temp = TempDir::new().unwrap();
        let heddle_dir = temp.path().join(".heddle");
        std::fs::create_dir_all(&heddle_dir).unwrap();
        (
            temp,
            Arc::new(PartialFetchMetadataManager::new(&heddle_dir)),
        )
    }

    #[test]
    fn concurrent_updates_do_not_drop_missing_blob_markers() {
        let (_temp, manager) = setup_manager();
        let first = ContentHash::compute(b"first");
        let second = ContentHash::compute(b"second");

        let mut threads = Vec::new();
        for hash in [first, second] {
            let manager = Arc::clone(&manager);
            threads.push(thread::spawn(move || {
                manager.record_missing_blob(hash).unwrap();
            }));
        }

        for handle in threads {
            handle.join().unwrap();
        }

        let missing = manager.missing_blobs().unwrap();
        assert_eq!(missing, vec![first, second]);
    }

    #[test]
    fn clear_missing_blob_is_noop_without_metadata_file() {
        let (_temp, manager) = setup_manager();
        let hash = ContentHash::compute(b"blob");

        assert!(!manager.clear_missing_blob(&hash).unwrap());
        assert!(!manager.metadata_path().exists());
        assert!(!manager.is_missing_blob(&hash).unwrap());
        assert!(manager.missing_blobs().unwrap().is_empty());
    }

    #[test]
    fn clear_all_missing_blobs_removes_metadata_once() {
        let (_temp, manager) = setup_manager();
        let first = ContentHash::compute(b"first");
        let second = ContentHash::compute(b"second");

        manager.record_missing_blob(first).unwrap();
        manager.record_missing_blob(second).unwrap();

        assert!(manager.clear_all_missing_blobs().unwrap());
        assert!(manager.missing_blobs().unwrap().is_empty());
        assert!(!manager.clear_all_missing_blobs().unwrap());
    }
}
