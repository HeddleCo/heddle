// SPDX-License-Identifier: Apache-2.0
//! Pack file manager for coordinating multiple pack files.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{OnceLock, RwLock},
};

use tracing::{debug, instrument, trace};

use crate::{
    object::ContentHash,
    store::{
        Result,
        pack::{ObjectType, PackObjectId, PackReadTier, PackReader},
    },
};

/// Format-only coordinator for loaded pack and index files.
///
/// Object-domain indexes belong in a wrapper owned by the consuming crate.
pub struct PackManager {
    packs_dir: PathBuf,
    packs: Vec<CachedPack>,
    object_locations: RwLock<ObjectLocationIndex>,
    eager_object_locations: bool,
}

#[derive(Default)]
struct ObjectLocationIndex {
    locations: HashMap<PackObjectId, ObjectLocation>,
    complete: bool,
}

#[derive(Clone, Copy)]
struct ObjectLocation {
    pack_index: usize,
    tier: PackReadTier,
}

struct CachedPack {
    pack_path: PathBuf,
    index_path: PathBuf,
    reader: OnceLock<Option<PackReader<'static>>>,
}

impl CachedPack {
    fn discovered(pack_path: PathBuf, index_path: PathBuf) -> Self {
        Self {
            pack_path,
            index_path,
            reader: OnceLock::new(),
        }
    }

    fn validated(pack_path: PathBuf, index_path: PathBuf, reader: PackReader<'static>) -> Self {
        Self {
            pack_path,
            index_path,
            reader: OnceLock::from(Some(reader)),
        }
    }

    fn reader(&self) -> Option<&PackReader<'static>> {
        self.reader
            .get_or_init(
                || match PackReader::open_lazy(&self.pack_path, &self.index_path) {
                    Ok(reader) => Some(reader),
                    Err(error) => {
                        debug!(pack = ?self.pack_path, %error, "Failed to open pack");
                        None
                    }
                },
            )
            .as_ref()
    }

    fn verified_reader(&self) -> Option<&PackReader<'static>> {
        self.reader
            .get_or_init(
                || match PackReader::open(&self.pack_path, &self.index_path) {
                    Ok(reader) => Some(reader),
                    Err(error) => {
                        debug!(pack = ?self.pack_path, %error, "Failed to open pack");
                        None
                    }
                },
            )
            .as_ref()
    }
}

impl PackManager {
    pub fn new(packs_dir: PathBuf) -> Self {
        Self::new_with_index_mode(packs_dir, force_eager_pack_index())
    }

    fn new_with_index_mode(packs_dir: PathBuf, eager_object_locations: bool) -> Self {
        let packs = Self::load_packs(&packs_dir).unwrap_or_default();
        let object_locations = Self::initial_object_locations(&packs, eager_object_locations);
        Self {
            packs_dir,
            packs,
            object_locations: RwLock::new(object_locations),
            eager_object_locations,
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
        Ok(Self::discover_pack_paths(packs_dir)?
            .into_iter()
            .map(|(pack_path, index_path)| CachedPack::discovered(pack_path, index_path))
            .collect())
    }

    pub fn reload(&mut self) -> Result<()> {
        self.packs = Self::load_packs(&self.packs_dir)?;
        self.reset_object_locations();
        Ok(())
    }

    fn initial_object_locations(packs: &[CachedPack], eager: bool) -> ObjectLocationIndex {
        if !eager {
            return ObjectLocationIndex::default();
        }
        let mut locations = HashMap::new();
        for (pack_index, pack) in packs.iter().enumerate() {
            let Some(reader) = pack.verified_reader() else {
                continue;
            };
            let Ok(objects) = reader.indexed_read_tiers() else {
                continue;
            };
            for (id, tier) in objects {
                remember_location(&mut locations, id, pack_index, tier);
            }
        }
        ObjectLocationIndex {
            locations,
            complete: true,
        }
    }

    fn reset_object_locations(&mut self) {
        self.object_locations = RwLock::new(Self::initial_object_locations(
            &self.packs,
            self.eager_object_locations,
        ));
    }

    fn object_location(&self, id: &PackObjectId) -> Result<Option<usize>> {
        {
            let index = self
                .object_locations
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(location) = index.locations.get(id) {
                return Ok(Some(location.pack_index));
            }
            if index.complete {
                return Ok(None);
            }
        }

        let mut index = self
            .object_locations
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !index.complete {
            for (pack_index, pack) in self.packs.iter().enumerate() {
                let Some(reader) = pack.reader() else {
                    continue;
                };
                let Ok(objects) = reader.indexed_read_tiers() else {
                    continue;
                };
                for (object_id, tier) in objects {
                    remember_location(&mut index.locations, object_id, pack_index, tier);
                }
            }
            index.complete = true;
        }
        Ok(index.locations.get(id).map(|location| location.pack_index))
    }

    /// Add a complete pack/index pair to the in-memory format index.
    pub fn add_pack(&mut self, pack_path: PathBuf, index_path: PathBuf) -> Result<()> {
        if self.packs.iter().any(|pack| pack.pack_path == pack_path) {
            return Ok(());
        }
        let reader = PackReader::open(&pack_path, &index_path)?;
        let objects = reader.indexed_read_tiers()?;
        let pack_index = self.packs.len();
        let cached = CachedPack::validated(pack_path, index_path, reader);
        self.packs.push(cached);
        let mut index = self
            .object_locations
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if index.complete {
            for (id, tier) in objects {
                remember_location(&mut index.locations, id, pack_index, tier);
            }
        }
        Ok(())
    }

    /// Check whether the immutable pack set on disk differs from this snapshot.
    ///
    /// Comparing only counts misses the decisive repack transition (`one old`
    /// → `one replacement`). Exact path comparison lets another `FsStore`
    /// recover after an atomic cutover even when cardinality is unchanged.
    /// Half-installed packs remain filtered by `discover_pack_paths`.
    pub fn needs_reload(&self) -> Result<bool> {
        let discovered = Self::discover_pack_paths(&self.packs_dir)?;
        Ok(discovered.len() != self.packs.len()
            || discovered
                .iter()
                .zip(&self.packs)
                .any(|((pack, index), cached)| {
                    *pack != cached.pack_path || *index != cached.index_path
                }))
    }

    /// Reload the pack list when the immutable pack set changed on disk.
    ///
    /// Catches the multi-instance case: two `FsStore`s back the same
    /// shared object dir (typical for lightweight thread worktrees,
    /// where the worktree's repo opens its own store but points at
    /// the main repo's `.heddle/`). When the worktree's store installs
    /// a new pack, the main repo's already-open `pack_manager`
    /// doesn't know about it; without this `get_blob`/`has_blob`
    /// from the main repo would surface "object not found".
    pub fn reload_if_stale(&mut self) -> Result<bool> {
        if !self.needs_reload()? {
            return Ok(false);
        }
        debug!("PackManager: pack set changed under us, reloading");
        self.reload()?;
        Ok(true)
    }

    pub fn get_object(&self, id: &PackObjectId) -> Result<Option<(ObjectType, Vec<u8>)>> {
        let Some(pack_index) = self.object_location(id)? else {
            trace!("Object not found in any pack");
            return Ok(None);
        };
        let Some(reader) = self.packs[pack_index].reader() else {
            return Ok(None);
        };
        let object = reader.get_object(id)?;
        if object.is_some() {
            trace!("Found object in pack");
        }
        Ok(object)
    }

    /// Return the physical tier that will serve `id`.
    ///
    /// When both layouts contain the same immutable object, lookup always
    /// selects the hot random-access record before a solid frame.
    pub fn object_read_tier(&self, id: &PackObjectId) -> Result<Option<PackReadTier>> {
        let _ = self.object_location(id)?;
        let index = self
            .object_locations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(index.locations.get(id).map(|location| location.tier))
    }

    /// Read `id` from one specific discovered pack without building the
    /// cross-pack location index. The record identity remains validated by
    /// [`PackReader`]; object-domain consumers must validate decoded content.
    pub fn get_object_from_pack(
        &self,
        pack_path: &Path,
        id: &PackObjectId,
    ) -> Result<Option<(ObjectType, Vec<u8>)>> {
        let Some(pack) = self.packs.iter().find(|pack| pack.pack_path == pack_path) else {
            return Ok(None);
        };
        let Some(reader) = pack.reader() else {
            return Ok(None);
        };
        reader.get_object(id)
    }

    /// List the identities in one specific pack without building the
    /// cross-pack location index.
    pub fn list_ids_from_pack(&self, pack_path: &Path) -> Result<Vec<PackObjectId>> {
        let Some(pack) = self.packs.iter().find(|pack| pack.pack_path == pack_path) else {
            return Ok(Vec::new());
        };
        let Some(reader) = pack.reader() else {
            return Ok(Vec::new());
        };
        reader.list_ids()
    }

    #[instrument(skip(self), fields(hash = %hash.short()))]
    pub fn get_hashed_object(&self, hash: &ContentHash) -> Result<Option<(ObjectType, Vec<u8>)>> {
        self.get_object(&PackObjectId::Hash(*hash))
    }

    /// Look up the logical object type without decoding the object payload.
    pub fn get_hashed_object_type(&self, hash: &ContentHash) -> Result<Option<ObjectType>> {
        let id = PackObjectId::Hash(*hash);
        let Some(pack_index) = self.object_location(&id)? else {
            return Ok(None);
        };
        let Some(reader) = self.packs[pack_index].reader() else {
            return Ok(None);
        };
        reader.get_hashed_object_type(hash)
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
        let Some(pack_index) = self.object_location(&id)? else {
            return Ok(None);
        };
        let Some(reader) = self.packs[pack_index].reader() else {
            return Ok(None);
        };
        reader.get_object_bytes(&id)
    }

    pub fn has_object(&self, hash: &ContentHash) -> bool {
        self.object_location(&PackObjectId::Hash(*hash))
            .is_ok_and(|location| location.is_some())
    }

    /// Look up the uncompressed size of `hash` across all loaded
    /// packs without decompressing the payload. Returns `Ok(None)`
    /// when the object isn't in any loaded pack.
    pub fn get_hashed_object_size(&self, hash: &ContentHash) -> Result<Option<u64>> {
        let id = PackObjectId::Hash(*hash);
        let Some(pack_index) = self.object_location(&id)? else {
            return Ok(None);
        };
        let Some(reader) = self.packs[pack_index].reader() else {
            return Ok(None);
        };
        reader.get_hashed_object_size(hash)
    }

    pub fn has_object_id(&self, id: &PackObjectId) -> bool {
        self.object_location(id)
            .is_ok_and(|location| location.is_some())
    }

    /// List all object hashes across all packs.
    pub fn list_all_hashes(&self) -> Result<Vec<ContentHash>> {
        let mut hashes = Vec::new();
        for pack in &self.packs {
            if let Some(reader) = pack.reader() {
                hashes.extend(reader.list_hashes()?);
            }
        }
        Ok(hashes)
    }

    pub fn list_all_ids(&self) -> Result<Vec<PackObjectId>> {
        let mut ids = Vec::new();
        for pack in &self.packs {
            if let Some(reader) = pack.reader() {
                ids.extend(reader.list_ids()?);
            }
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

fn remember_location(
    locations: &mut HashMap<PackObjectId, ObjectLocation>,
    id: PackObjectId,
    pack_index: usize,
    tier: PackReadTier,
) {
    let candidate = ObjectLocation { pack_index, tier };
    match locations.get_mut(&id) {
        Some(existing)
            if existing.tier == PackReadTier::SolidFrame && tier == PackReadTier::Hot =>
        {
            *existing = candidate;
        }
        Some(_) => {}
        None => {
            locations.insert(id, candidate);
        }
    }
}

fn force_eager_pack_index() -> bool {
    std::env::var("HEDDLE_PERF_FORCE_EAGER_PACK_INDEX")
        .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "yes"))
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;
