// SPDX-License-Identifier: Apache-2.0
//! Pack file manager for coordinating multiple pack files.

use std::{
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

pub struct PackManager {
    packs_dir: PathBuf,
    packs: Vec<CachedPack>,
}

struct CachedPack {
    pack_path: PathBuf,
    index_path: PathBuf,
    reader: PackReader,
}

impl PackManager {
    pub fn new(packs_dir: PathBuf) -> Self {
        let packs = Self::load_packs(&packs_dir).unwrap_or_default();
        Self { packs_dir, packs }
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
        self.packs = Self::load_packs(&self.packs_dir)?;
        Ok(())
    }

    /// Cheap check: does the packs directory hold more pack/index
    /// pairs than we have loaded? Reuses `discover_pack_paths` so
    /// half-installed packs (a `.pack` whose `.idx` sibling hasn't
    /// landed yet) are filtered out — otherwise we'd loop forever
    /// reloading a count we can never match.
    pub(crate) fn needs_reload(&self) -> Result<bool> {
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
    pub(crate) fn reload_if_disk_grew(&mut self) -> Result<bool> {
        if !self.needs_reload()? {
            return Ok(false);
        }
        debug!("PackManager: pack dir grew under us, reloading");
        self.reload()?;
        Ok(true)
    }

    pub fn get_object(&self, id: &PackObjectId) -> Result<Option<(ObjectType, Vec<u8>)>> {
        for pack in &self.packs {
            if let Some((obj_type, data)) = pack.reader.get_object(id)? {
                trace!("Found object in pack");
                return Ok(Some((obj_type, data)));
            }
        }

        trace!("Object not found in any pack");
        Ok(None)
    }

    #[instrument(skip(self), fields(hash = %hash.short()))]
    pub fn get_hashed_object(&self, hash: &ContentHash) -> Result<Option<(ObjectType, Vec<u8>)>> {
        self.get_object(&PackObjectId::Hash(*hash))
    }

    pub fn has_object(&self, hash: &ContentHash) -> bool {
        self.packs
            .iter()
            .any(|pack| pack.reader.has_object(&PackObjectId::Hash(*hash)))
    }

    /// Look up the uncompressed size of `hash` across all loaded
    /// packs without decompressing the payload. Returns `Ok(None)`
    /// when the object isn't in any loaded pack.
    pub fn get_hashed_object_size(&self, hash: &ContentHash) -> Result<Option<u64>> {
        for pack in &self.packs {
            if let Some(size) = pack.reader.get_hashed_object_size(hash)? {
                return Ok(Some(size));
            }
        }
        Ok(None)
    }

    pub fn has_object_id(&self, id: &PackObjectId) -> bool {
        self.packs.iter().any(|pack| pack.reader.has_object(id))
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