// SPDX-License-Identifier: Apache-2.0
//! Pack file manager for coordinating multiple pack files.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use tracing::{debug, instrument, trace};

use crate::{
    object::ContentHash,
    store::{
        Result,
        pack::{ObjectType, PackObjectId, PackReader},
    },
};

/// Format-only coordinator for loaded pack and index files.
///
/// Object-domain indexes belong in a wrapper owned by the consuming crate.
pub struct PackManager {
    packs_dir: PathBuf,
    packs: Vec<CachedPack>,
    object_locations: HashMap<PackObjectId, usize>,
}

struct CachedPack {
    pack_path: PathBuf,
    index_path: PathBuf,
    reader: PackReader<'static>,
}

impl PackManager {
    pub fn new(packs_dir: PathBuf) -> Self {
        let packs = Self::load_packs(&packs_dir).unwrap_or_default();
        let object_locations = Self::index_object_locations(&packs);
        Self {
            packs_dir,
            packs,
            object_locations,
        }
    }

    fn discover_pack_paths(packs_dir: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
        let mut packs = Vec::new();

        if !packs_dir.exists() {
            return Ok(packs);
        }

        for entry in fs::read_dir(packs_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().map(|e| e == "pack").unwrap_or(false) {
                let index_path = path.with_extension("idx");
                if index_path.exists() {
                    packs.push((path, index_path));
                }
            }
        }

        packs.sort_by(|left, right| left.0.cmp(&right.0));

        debug!(count = packs.len(), "Discovered pack files");
        Ok(packs)
    }

    fn load_packs(packs_dir: &Path) -> Result<Vec<CachedPack>> {
        let mut cached_packs = Vec::new();

        for (pack_path, index_path) in Self::discover_pack_paths(packs_dir)? {
            match PackReader::open(&pack_path, &index_path) {
                Ok(reader) => cached_packs.push(CachedPack {
                    pack_path,
                    index_path,
                    reader,
                }),
                Err(error) => {
                    debug!("Failed to open pack {:?}: {}", pack_path, error);
                }
            }
        }

        Ok(cached_packs)
    }

    pub fn reload(&mut self) -> Result<()> {
        let packs = Self::load_packs(&self.packs_dir)?;
        let object_locations = Self::index_object_locations(&packs);
        self.packs = packs;
        self.object_locations = object_locations;
        Ok(())
    }

    fn index_object_locations(packs: &[CachedPack]) -> HashMap<PackObjectId, usize> {
        let mut locations = HashMap::new();
        for (pack_index, pack) in packs.iter().enumerate() {
            for id in pack.reader.list_ids() {
                // Preserve the historical first-pack-wins lookup behavior for
                // duplicate immutable objects.
                locations.entry(id).or_insert(pack_index);
            }
        }
        locations
    }

    /// Add a complete pack/index pair to the in-memory format index.
    pub fn add_pack(&mut self, pack_path: PathBuf, index_path: PathBuf) -> Result<()> {
        if self.packs.iter().any(|pack| pack.pack_path == pack_path) {
            return Ok(());
        }
        let cached = CachedPack {
            reader: PackReader::open(&pack_path, &index_path)?,
            pack_path,
            index_path,
        };
        let pack_index = self.packs.len();
        for id in cached.reader.list_ids() {
            self.object_locations.entry(id).or_insert(pack_index);
        }
        self.packs.push(cached);
        Ok(())
    }

    /// Cheap check: does the packs directory hold more pack/index
    /// pairs than we have loaded? Reuses `discover_pack_paths` so
    /// half-installed packs (a `.pack` whose `.idx` sibling hasn't
    /// landed yet) are filtered out — otherwise we'd loop forever
    /// reloading a count we can never match.
    pub fn needs_reload(&self) -> Result<bool> {
        Ok(Self::discover_pack_paths(&self.packs_dir)?.len() > self.packs.len())
    }

    /// Reload the pack list only if the packs directory has more
    /// pack/index pairs on disk than we know about in memory.
    ///
    /// Catches the multi-instance case: two `FsStore`s back the same
    /// shared object dir (typical for lightweight thread worktrees,
    /// where the worktree's repo opens its own store but points at
    /// the main repo's `.heddle/`). When the worktree's store installs
    /// a new pack, the main repo's already-open `pack_manager`
    /// doesn't know about it; without this `get_blob`/`has_blob`
    /// from the main repo would surface "object not found".
    pub fn reload_if_disk_grew(&mut self) -> Result<bool> {
        if !self.needs_reload()? {
            return Ok(false);
        }
        debug!("PackManager: pack dir grew under us, reloading");
        self.reload()?;
        Ok(true)
    }

    pub fn get_object(&self, id: &PackObjectId) -> Result<Option<(ObjectType, Vec<u8>)>> {
        let Some(pack_index) = self.object_locations.get(id).copied() else {
            trace!("Object not found in any pack");
            return Ok(None);
        };
        let object = self.packs[pack_index].reader.get_object(id)?;
        if object.is_some() {
            trace!("Found object in pack");
        }
        Ok(object)
    }

    #[instrument(skip(self), fields(hash = %hash.short()))]
    pub fn get_hashed_object(&self, hash: &ContentHash) -> Result<Option<(ObjectType, Vec<u8>)>> {
        self.get_object(&PackObjectId::Hash(*hash))
    }

    /// Zero-copy variant of `get_hashed_object`. Returns
    /// [`bytes::Bytes`] views into the underlying pack mmap when
    /// the entry is non-delta and stored uncompressed; falls back
    /// to the standard decompress-into-Vec path otherwise.
    pub fn get_hashed_object_bytes(
        &self,
        hash: &ContentHash,
    ) -> Result<Option<(ObjectType, bytes::Bytes)>> {
        let id = PackObjectId::Hash(*hash);
        let Some(pack_index) = self.object_locations.get(&id).copied() else {
            return Ok(None);
        };
        self.packs[pack_index].reader.get_object_bytes(&id)
    }

    pub fn has_object(&self, hash: &ContentHash) -> bool {
        self.object_locations
            .contains_key(&PackObjectId::Hash(*hash))
    }

    /// Look up the uncompressed size of `hash` across all loaded
    /// packs without decompressing the payload. Returns `Ok(None)`
    /// when the object isn't in any loaded pack.
    pub fn get_hashed_object_size(&self, hash: &ContentHash) -> Result<Option<u64>> {
        let id = PackObjectId::Hash(*hash);
        let Some(pack_index) = self.object_locations.get(&id).copied() else {
            return Ok(None);
        };
        self.packs[pack_index].reader.get_hashed_object_size(hash)
    }

    pub fn has_object_id(&self, id: &PackObjectId) -> bool {
        self.object_locations.contains_key(id)
    }

    /// List all object hashes across all packs.
    pub fn list_all_hashes(&self) -> Result<Vec<ContentHash>> {
        let mut hashes = Vec::new();
        for pack in &self.packs {
            hashes.extend(pack.reader.list_hashes());
        }
        Ok(hashes)
    }

    pub fn list_all_ids(&self) -> Result<Vec<PackObjectId>> {
        let mut ids = Vec::new();
        for pack in &self.packs {
            ids.extend(pack.reader.list_ids());
        }
        Ok(ids)
    }

    /// Return paths of all pack files (for deletion during aggressive repack).
    pub fn pack_file_paths(&self) -> Vec<(&Path, &Path)> {
        self.packs
            .iter()
            .map(|pack| (pack.pack_path.as_path(), pack.index_path.as_path()))
            .collect()
    }

    pub fn pack_count(&self) -> usize {
        self.packs.len()
    }

    pub fn packs_dir(&self) -> &Path {
        &self.packs_dir
    }
}

#[cfg(test)]
mod tests {
    use heddle_format::compression::CompressionConfig;
    use tempfile::TempDir;

    use super::PackManager;
    use crate::{
        object::ContentHash,
        store::pack::{ObjectType, PackBuilder},
    };

    fn write_pack(
        root: &std::path::Path,
        ordinal: usize,
    ) -> (std::path::PathBuf, std::path::PathBuf, ContentHash) {
        let payload = format!("pack-object-{ordinal}").into_bytes();
        let object_id = ContentHash::compute(&payload);
        let mut builder = PackBuilder::new(CompressionConfig {
            max_delta_size: 0,
            ..CompressionConfig::default()
        });
        builder.add(object_id, ObjectType::Blob, payload);
        let (pack_data, index_data, _) = builder.build().unwrap();
        let pack_path = root.join(format!("format-{ordinal:03}.pack"));
        let index_path = root.join(format!("format-{ordinal:03}.idx"));
        std::fs::write(&pack_path, pack_data).unwrap();
        std::fs::write(&index_path, index_data).unwrap();
        (pack_path, index_path, object_id)
    }

    #[test]
    fn object_locator_tracks_incremental_and_reloaded_packs() {
        let temp = TempDir::new().unwrap();
        let mut manager = PackManager::new(temp.path().to_path_buf());
        for ordinal in 0..8 {
            let (pack_path, index_path, _) = write_pack(temp.path(), ordinal);
            manager.add_pack(pack_path, index_path).unwrap();
        }

        let ids = manager.list_all_ids().unwrap();
        assert_eq!(ids.len(), 8);
        for id in &ids {
            assert!(manager.has_object_id(id));
            assert!(manager.get_object(id).unwrap().is_some());
        }

        let reloaded = PackManager::new(temp.path().to_path_buf());
        assert_eq!(reloaded.pack_count(), 8);
        for id in &ids {
            assert!(reloaded.has_object_id(id));
            assert!(reloaded.get_object(id).unwrap().is_some());
        }
    }
}
