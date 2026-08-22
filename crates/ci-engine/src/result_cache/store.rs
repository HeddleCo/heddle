// SPDX-License-Identifier: Apache-2.0
//! In-memory and filesystem stores for portable result-cache entries.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use super::{
    entry::{ResultCacheEntry, ResultCacheError},
    key::{CacheKey, entry_id},
};

/// Lookup and seed a content-addressed result cache.
pub trait ResultCache {
    /// Load the entry for `key` + `check_name`, if a valid one is stored.
    fn get(
        &self,
        key: &CacheKey,
        check_name: &str,
    ) -> Result<Option<ResultCacheEntry>, ResultCacheError>;

    /// Persist `entry` under its recorded triple and check name.
    fn put(&self, entry: &ResultCacheEntry) -> Result<(), ResultCacheError>;
}

/// Process-local cache used by tests and as a seed/hit fixture.
#[derive(Debug, Default)]
pub struct MemoryResultCache {
    entries: Mutex<BTreeMap<String, ResultCacheEntry>>,
}

impl MemoryResultCache {
    /// Empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of every stored entry, for inspection and tampering tests.
    #[must_use]
    pub fn entries(&self) -> Vec<ResultCacheEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect()
    }
}

impl ResultCache for MemoryResultCache {
    fn get(
        &self,
        key: &CacheKey,
        check_name: &str,
    ) -> Result<Option<ResultCacheEntry>, ResultCacheError> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(entries
            .get(&entry_id(key, check_name))
            .filter(|entry| entry.is_valid_for(key, check_name))
            .cloned())
    }

    fn put(&self, entry: &ResultCacheEntry) -> Result<(), ResultCacheError> {
        let key = entry.cache_key();
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(entry_id(&key, &entry.check_name), entry.clone());
        Ok(())
    }
}

/// Directory-backed cache. Entries are JSON files named by the entry digest.
///
/// The layout is portable: copy the directory (or a single entry file) to
/// another machine and the same triple hits.
#[derive(Debug, Clone)]
pub struct FsResultCache {
    root: PathBuf,
}

impl FsResultCache {
    /// Use `root` as the cache directory. Created on first write.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Filesystem root this cache reads and writes.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, key: &CacheKey, check_name: &str) -> PathBuf {
        let id = entry_id(key, check_name);
        self.root.join(&id[..2]).join(format!("{id}.json"))
    }
}

impl ResultCache for FsResultCache {
    fn get(
        &self,
        key: &CacheKey,
        check_name: &str,
    ) -> Result<Option<ResultCacheEntry>, ResultCacheError> {
        let path = self.path_for(key, check_name);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let Ok(entry) = serde_json::from_slice::<ResultCacheEntry>(&bytes) else {
            return Ok(None);
        };
        if entry.is_valid_for(key, check_name) {
            Ok(Some(entry))
        } else {
            Ok(None)
        }
    }

    fn put(&self, entry: &ResultCacheEntry) -> Result<(), ResultCacheError> {
        let key = entry.cache_key();
        let path = self.path_for(&key, &entry.check_name);
        let bytes = serde_json::to_vec(entry).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
        })?;
        write_atomic(&path, &bytes)
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ResultCacheError> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "result cache entry path has no parent",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let unique = format!(
        ".{}.{}-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("entry"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );
    let tmp = parent.join(unique);
    let written = write_tmp_then_rename(&tmp, path, bytes);
    if written.is_err() {
        let _cleanup = std::fs::remove_file(&tmp);
    }
    written
}

fn write_tmp_then_rename(tmp: &Path, dest: &Path, bytes: &[u8]) -> Result<(), ResultCacheError> {
    let mut file = File::create(tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(tmp, dest)?;
    Ok(())
}
