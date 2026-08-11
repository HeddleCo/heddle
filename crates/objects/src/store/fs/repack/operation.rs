// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashSet, fs, fs::OpenOptions};

use super::{
    super::{
        FsStore,
        fs_io::list_hashes_from_dir,
        fs_paths::{blobs_dir, packs_dir, trees_dir},
    },
    cutover::{
        acquire_repack_lock, cutover, file_len, hash_file, object_file_len, preserve_commit_markers,
    },
    staging::{BuildError, RepackSnapshot, RepackStaging, verify_staged},
};
use crate::store::{
    HeddleError, ObjectStore, Result,
    pack::{
        ObjectType, PackObjectId, RepackContext, RepackError, RepackInventory, RepackOperation,
        RepackOutcome, StreamingPackBuilder,
    },
};

/// Existing LMPK encoder wired behind the generic background repack seam.
///
/// It streams one object at a time (bounded memory), verifies every typed id
/// and the exact expected object set, then installs the immutable replacement
/// before retiring source packs under the pack-manager write lock. S2/S4
/// replace this operation; the scheduler itself has no native-pack knowledge.
pub struct FsRepackOperation {
    store: FsStore,
    #[cfg(test)]
    corrupt_first_object: bool,
}

impl FsRepackOperation {
    /// Create a native filesystem repack payload.
    pub fn new(store: FsStore) -> Self {
        Self {
            store,
            #[cfg(test)]
            corrupt_first_object: false,
        }
    }

    #[cfg(test)]
    pub(super) fn with_corrupted_output(mut self) -> Self {
        self.corrupt_first_object = true;
        self
    }

    fn inventory(&self) -> Result<RepackInventory> {
        let loose_blobs = list_hashes_from_dir(&blobs_dir(self.store.root()))?;
        let loose_trees = list_hashes_from_dir(&trees_dir(self.store.root()))?;
        let loose_bytes = loose_blobs
            .iter()
            .map(|hash| object_file_len(&blobs_dir(self.store.root()), hash))
            .chain(
                loose_trees
                    .iter()
                    .map(|hash| object_file_len(&trees_dir(self.store.root()), hash)),
            )
            .sum();
        let manager =
            self.store.pack_manager().read().map_err(|_| {
                HeddleError::Config("Failed to acquire pack manager lock".to_string())
            })?;
        let paths = manager.pack_file_paths();
        let pack_bytes = paths
            .iter()
            .flat_map(|(pack, index)| [*pack, *index])
            .map(file_len)
            .sum();
        let ids = manager.list_all_ids()?;
        let unique = ids.iter().copied().collect::<HashSet<_>>().len() as u64;
        Ok(RepackInventory {
            loose_objects: (loose_blobs.len() + loose_trees.len()) as u64,
            loose_bytes,
            pack_count: paths.len() as u64,
            pack_bytes,
            duplicate_objects: ids.len() as u64 - unique,
            packed_objects: ids.len() as u64,
        })
    }

    fn execute(&self, context: &RepackContext) -> std::result::Result<RepackOutcome, RepackError> {
        let packs = packs_dir(self.store.root());
        fs::create_dir_all(&packs).map_err(RepackError::operation)?;
        let _operation_lock = acquire_repack_lock(&packs, context)?;
        context.checkpoint(0)?;

        let snapshot = RepackSnapshot::capture(&self.store).map_err(RepackError::operation)?;
        if snapshot.ids.is_empty()
            && snapshot.loose_blobs.is_empty()
            && snapshot.loose_trees.is_empty()
        {
            return Ok(RepackOutcome::default());
        }

        let staging = RepackStaging::new(&packs).map_err(RepackError::operation)?;
        let (expected, logical_bytes) =
            self.build_staged(&snapshot, &staging, context)
                .map_err(|error| match error {
                    BuildError::Cancelled(error) => error,
                    BuildError::Store(error) => RepackError::operation(error),
                })?;
        verify_staged(&staging, &expected, context)?;
        context.checkpoint(0)?;

        // Cutover starts here and is intentionally non-cancellable. The new
        // pair is durable before any source path is retired.
        let new_pack_name = hash_file(&staging.pack).map_err(RepackError::operation)?;
        let replacement_preexisting = packs.join(format!("{new_pack_name}.pack")).exists()
            && packs.join(format!("{new_pack_name}.idx")).exists();
        ObjectStore::install_pack_streaming(&self.store, &staging.pack, &staging.index)
            .map_err(RepackError::operation)?;
        preserve_commit_markers(&packs, &new_pack_name, &snapshot.commit_artifact_ids)
            .map_err(RepackError::operation)?;
        let cutover = cutover(
            &self.store,
            &snapshot,
            &new_pack_name,
            replacement_preexisting,
        )
        .map_err(RepackError::operation)?;
        // The replacement is authoritative before loose copies are pruned.
        // `prune_loose_objects_impl` rechecks exact packed content, so a
        // concurrent loose write is removed only when the durable pack has it.
        let (_, loose_bytes) = self
            .store
            .prune_loose_objects_impl()
            .map_err(RepackError::operation)?;
        let reclaimed = cutover
            .removed_pack_bytes
            .saturating_add(loose_bytes)
            .saturating_sub(cutover.replacement_bytes);

        Ok(RepackOutcome {
            objects_repacked: expected.len() as u64,
            bytes_repacked: logical_bytes,
            bytes_reclaimed: reclaimed,
        })
    }

    fn build_staged(
        &self,
        snapshot: &RepackSnapshot,
        staging: &RepackStaging,
        context: &RepackContext,
    ) -> std::result::Result<(HashSet<PackObjectId>, u64), BuildError> {
        let pack_file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&staging.pack)?;
        let mut compression = self.store.compression();
        compression.max_delta_size = 0;
        let mut builder = StreamingPackBuilder::new(
            pack_file,
            staging.index.clone(),
            compression,
            staging.buckets.clone(),
        )?;
        let loose_tree_set = snapshot.loose_trees.iter().copied().collect::<HashSet<_>>();
        let mut expected = HashSet::new();
        #[cfg(test)]
        let mut first = true;

        for id in &snapshot.ids {
            if !expected.insert(*id) {
                continue;
            }
            let Some((object_type, mut data)) = self.read_snapshot_object(id)? else {
                return Err(HeddleError::InvalidObject(format!(
                    "repack source object disappeared: {id:?}"
                ))
                .into());
            };
            if let PackObjectId::Hash(hash) = id
                && object_type == ObjectType::Tree
                && loose_tree_set.contains(hash)
                && let Some(loose) = ObjectStore::get_tree_serialized(&self.store, hash)?
            {
                data = loose;
            }
            #[cfg(test)]
            if self.corrupt_first_object && first {
                data.push(0xff);
            }
            #[cfg(test)]
            {
                first = false;
            }
            let bytes = data.len() as u64;
            builder.add_id(*id, object_type, data)?;
            context.checkpoint(bytes).map_err(BuildError::Cancelled)?;
        }
        for hash in &snapshot.loose_blobs {
            let id = PackObjectId::Hash(*hash);
            if expected.insert(id) {
                let blob = ObjectStore::get_blob(&self.store, hash)?.ok_or_else(|| {
                    HeddleError::InvalidObject(format!("loose blob disappeared: {hash}"))
                })?;
                let data = blob.content().to_vec();
                let bytes = data.len() as u64;
                builder.add(*hash, ObjectType::Blob, data)?;
                context.checkpoint(bytes).map_err(BuildError::Cancelled)?;
            }
        }
        for hash in &snapshot.loose_trees {
            let id = PackObjectId::Hash(*hash);
            if expected.insert(id) {
                let data =
                    ObjectStore::get_tree_serialized(&self.store, hash)?.ok_or_else(|| {
                        HeddleError::InvalidObject(format!("loose tree disappeared: {hash}"))
                    })?;
                let bytes = data.len() as u64;
                builder.add(*hash, ObjectType::Tree, data)?;
                context.checkpoint(bytes).map_err(BuildError::Cancelled)?;
            }
        }
        let (_, stats) = builder.finalize()?;
        Ok((expected, stats.total_uncompressed))
    }

    fn read_snapshot_object(&self, id: &PackObjectId) -> Result<Option<(ObjectType, Vec<u8>)>> {
        let manager =
            self.store.pack_manager().read().map_err(|_| {
                HeddleError::Config("Failed to acquire pack manager lock".to_string())
            })?;
        manager.get_object(id)
    }
}

impl RepackOperation for FsRepackOperation {
    fn key(&self) -> String {
        self.store.root().display().to_string()
    }

    fn inspect(&self) -> std::result::Result<RepackInventory, RepackError> {
        self.inventory().map_err(RepackError::operation)
    }

    fn run(&self, context: &RepackContext) -> std::result::Result<RepackOutcome, RepackError> {
        self.execute(context)
    }
}
