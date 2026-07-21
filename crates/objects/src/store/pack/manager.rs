// SPDX-License-Identifier: Apache-2.0
//! Pack file manager for coordinating multiple pack files.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use tracing::{debug, instrument, trace};

use crate::{
    object::{ContentHash, StateId},
    store::{
        Result, SnapshotCommitArtifact, SnapshotCommitDescriptor,
        pack::{ObjectType, PackObjectId, PackReader},
        snapshot_commit::snapshot_commit_marker_path,
    },
};

pub struct PackManager {
    packs_dir: PathBuf,
    packs: Vec<CachedPack>,
    snapshot_commits: Vec<SnapshotCommitDescriptor>,
    snapshot_commits_by_state: HashMap<StateId, SnapshotCommitDescriptor>,
    snapshot_commit_index_error: Option<String>,
}

struct CachedPack {
    pack_path: PathBuf,
    index_path: PathBuf,
    reader: PackReader<'static>,
}

impl PackManager {
    pub(crate) fn snapshot_commit_descriptors(&self) -> Result<Vec<SnapshotCommitDescriptor>> {
        self.ensure_snapshot_commit_index_valid()?;
        Ok(self.snapshot_commits.clone())
    }

    pub(crate) fn snapshot_commit_descriptor_for_state(
        &self,
        state: &StateId,
    ) -> Result<Option<SnapshotCommitDescriptor>> {
        self.ensure_snapshot_commit_index_valid()?;
        Ok(self.snapshot_commits_by_state.get(state).cloned())
    }

    fn ensure_snapshot_commit_index_valid(&self) -> Result<()> {
        if let Some(error) = &self.snapshot_commit_index_error {
            return Err(crate::store::StoreError::InvalidObject(error.clone()));
        }
        Ok(())
    }

    fn snapshot_commit_descriptors_for_pack(
        cached: &CachedPack,
    ) -> Result<Vec<SnapshotCommitDescriptor>> {
        let mut descriptors = Vec::new();
        let object_ids = cached.reader.list_ids();
        for id in &object_ids {
            let Some((ObjectType::SnapshotCommit, bytes)) = cached.reader.get_object(id)? else {
                continue;
            };
            let PackObjectId::Hash(expected) = id else {
                continue;
            };
            let artifact: SnapshotCommitArtifact = rmp_serde::from_slice(&bytes)?;
            artifact.validate()?;
            if artifact.id() != *expected {
                return Err(crate::store::StoreError::InvalidObject(
                    "snapshot commit artifact address mismatch".to_string(),
                ));
            }
            if !snapshot_commit_marker_path(&cached.pack_path, expected).exists() {
                continue;
            }
            descriptors.push(SnapshotCommitDescriptor {
                artifact,
                pack_name: cached
                    .pack_path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_string(),
                pack_path: cached.pack_path.clone(),
                object_ids: object_ids.clone(),
            });
        }
        Ok(descriptors)
    }

    pub fn new(packs_dir: PathBuf) -> Self {
        let packs = Self::load_packs(&packs_dir).unwrap_or_default();
        let ((snapshot_commits, snapshot_commits_by_state), snapshot_commit_index_error) =
            match Self::index_snapshot_commits(&packs) {
                Ok(index) => (index, None),
                Err(error) => ((Vec::new(), HashMap::new()), Some(error.to_string())),
            };
        Self {
            packs_dir,
            packs,
            snapshot_commits,
            snapshot_commits_by_state,
            snapshot_commit_index_error,
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
        let (snapshot_commits, snapshot_commits_by_state) = Self::index_snapshot_commits(&packs)?;
        self.packs = packs;
        self.snapshot_commits = snapshot_commits;
        self.snapshot_commits_by_state = snapshot_commits_by_state;
        self.snapshot_commit_index_error = None;
        Ok(())
    }

    fn index_snapshot_commits(
        packs: &[CachedPack],
    ) -> Result<(
        Vec<SnapshotCommitDescriptor>,
        HashMap<StateId, SnapshotCommitDescriptor>,
    )> {
        let mut descriptors = Vec::new();
        let mut by_state = HashMap::new();
        for cached in packs {
            for descriptor in Self::snapshot_commit_descriptors_for_pack(cached)? {
                by_state.insert(descriptor.artifact.state, descriptor.clone());
                descriptors.push(descriptor);
            }
        }
        Ok((descriptors, by_state))
    }

    pub(crate) fn add_pack(&mut self, pack_path: PathBuf, index_path: PathBuf) -> Result<()> {
        if self.packs.iter().any(|pack| pack.pack_path == pack_path) {
            return Ok(());
        }
        let cached = CachedPack {
            reader: PackReader::open(&pack_path, &index_path)?,
            pack_path,
            index_path,
        };
        for descriptor in Self::snapshot_commit_descriptors_for_pack(&cached)? {
            self.snapshot_commits_by_state
                .insert(descriptor.artifact.state, descriptor.clone());
            self.snapshot_commits.push(descriptor);
        }
        self.packs.push(cached);
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

    /// Zero-copy variant of `get_hashed_object`. Returns
    /// [`bytes::Bytes`] views into the underlying pack mmap when
    /// the entry is non-delta and stored uncompressed; falls back
    /// to the standard decompress-into-Vec path otherwise.
    pub fn get_hashed_object_bytes(
        &self,
        hash: &ContentHash,
    ) -> Result<Option<(ObjectType, bytes::Bytes)>> {
        let id = PackObjectId::Hash(*hash);
        for pack in &self.packs {
            if let Some((obj_type, data)) = pack.reader.get_object_bytes(&id)? {
                return Ok(Some((obj_type, data)));
            }
        }
        Ok(None)
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

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use heddle_format::compression::CompressionConfig;
    use tempfile::TempDir;

    use super::PackManager;
    use crate::{
        object::StateId,
        store::{
            SNAPSHOT_COMMIT_ARTIFACT_SCHEMA, SnapshotCommitArtifact,
            pack::{ObjectType, PackBuilder},
            snapshot_commit::snapshot_commit_marker_path,
        },
    };

    fn write_snapshot_pack(
        root: &std::path::Path,
        ordinal: usize,
    ) -> (std::path::PathBuf, std::path::PathBuf, StateId) {
        let state = StateId::from_bytes([u8::try_from(ordinal + 1).unwrap(); 32]);
        let artifact = SnapshotCommitArtifact {
            schema: SNAPSHOT_COMMIT_ARTIFACT_SCHEMA,
            transaction_id: format!("tx-{ordinal}"),
            scope: "snapshot".to_string(),
            base_oplog_head_id: ordinal as u64,
            state,
            encoded_records: vec![vec![ordinal as u8]],
        };
        let artifact_id = artifact.id();
        let mut builder = PackBuilder::new(CompressionConfig {
            max_delta_size: 0,
            ..CompressionConfig::default()
        });
        builder.add(
            artifact_id,
            ObjectType::SnapshotCommit,
            rmp_serde::to_vec_named(&artifact).unwrap(),
        );
        let (pack_data, index_data, _) = builder.build().unwrap();
        let pack_path = root.join(format!("snapshot-{ordinal:03}.pack"));
        let index_path = root.join(format!("snapshot-{ordinal:03}.idx"));
        std::fs::write(&pack_path, pack_data).unwrap();
        std::fs::write(&index_path, index_data).unwrap();
        std::fs::write(snapshot_commit_marker_path(&pack_path, &artifact_id), []).unwrap();
        (pack_path, index_path, state)
    }

    #[test]
    fn repeated_state_descriptor_lookup_stays_on_incremental_index_after_many_snapshots() {
        let temp = TempDir::new().unwrap();
        let mut manager = PackManager::new(temp.path().to_path_buf());
        let mut states = Vec::new();
        for ordinal in 0..128 {
            let (pack_path, index_path, state) = write_snapshot_pack(temp.path(), ordinal);
            manager.add_pack(pack_path, index_path).unwrap();
            states.push(state);
        }
        assert_eq!(manager.pack_count(), 128);
        assert_eq!(manager.snapshot_commit_descriptors().unwrap().len(), 128);

        let started = Instant::now();
        for iteration in 0..100_000 {
            let state = states[iteration % states.len()];
            let descriptor = manager
                .snapshot_commit_descriptor_for_state(&state)
                .unwrap()
                .expect("every incrementally installed snapshot is indexed");
            assert_eq!(descriptor.artifact.state, state);
        }
        eprintln!(
            "100k cached snapshot descriptor lookups across 128 packs: {:?}",
            started.elapsed()
        );
        assert_eq!(
            manager.pack_count(),
            128,
            "lookup must not reload the pack set"
        );
    }
}
