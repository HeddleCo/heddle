// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashSet, fs, fs::OpenOptions};

use super::{
    super::{
        FsStore,
        fs_io::{list_hashes_from_dir, list_state_ids_from_dir},
        fs_paths::{blobs_dir, packs_dir, state_path, states_dir, trees_dir},
        npk1::{Npk1Build, Npk1BuildError, build_npk1_pack},
    },
    compact::add_compact_metadata,
    cutover::{
        acquire_repack_lock, cutover, file_len, hash_file, object_file_len,
        preserve_commit_markers, publish_npk1,
    },
    staging::{BuildError, RepackSnapshot, RepackStaging, verify_staged},
};
use crate::store::{
    HeddleError, ObjectStore, Result,
    pack::{
        ObjectType, PackObjectId, PackReader, RepackContext, RepackError, RepackInventory,
        RepackOperation, RepackOutcome, StreamingPackBuilder,
    },
};

type StagedBuild = (
    HashSet<PackObjectId>,
    HashSet<crate::object::ContentHash>,
    u64,
    Option<Npk1Build>,
);

/// Compact native encoder wired behind the generic background repack seam.
///
/// It loads trees one at a time while retaining pack-wide dictionaries and
/// sketches, verifies every typed id and the exact expected object set, then
/// installs the immutable replacement before retiring source packs under the
/// pack-manager write lock. The scheduler itself has no native-pack knowledge.
pub struct FsRepackOperation {
    store: FsStore,
    excluded_blobs: HashSet<crate::object::ContentHash>,
    #[cfg(test)]
    corrupt_first_object: bool,
}

impl FsRepackOperation {
    /// Create a native filesystem repack payload.
    pub fn new(store: FsStore) -> Self {
        Self {
            store,
            excluded_blobs: HashSet::new(),
            #[cfg(test)]
            corrupt_first_object: false,
        }
    }

    /// Exclude one blob from the replacement pack.
    ///
    /// This is the storage primitive used by purge: cutover retires every
    /// source pack only after the replacement has been verified without the
    /// excluded identity.
    pub fn excluding_blob(mut self, hash: crate::object::ContentHash) -> Self {
        self.excluded_blobs.insert(hash);
        self
    }

    #[cfg(test)]
    pub(super) fn with_corrupted_output(mut self) -> Self {
        self.corrupt_first_object = true;
        self
    }

    fn inventory(&self) -> Result<RepackInventory> {
        let loose_blobs = list_hashes_from_dir(&blobs_dir(self.store.root()))?;
        let loose_trees = list_hashes_from_dir(&trees_dir(self.store.root()))?;
        let loose_states = list_state_ids_from_dir(&states_dir(self.store.root()))?;
        let loose_bytes = loose_blobs
            .iter()
            .map(|hash| object_file_len(&blobs_dir(self.store.root()), hash))
            .chain(
                loose_trees
                    .iter()
                    .map(|hash| object_file_len(&trees_dir(self.store.root()), hash)),
            )
            .chain(
                loose_states
                    .iter()
                    .map(|id| file_len(&state_path(self.store.root(), id))),
            )
            .sum();
        let manager =
            self.store.pack_manager().read().map_err(|_| {
                HeddleError::Config("Failed to acquire pack manager lock".to_string())
            })?;
        let paths = manager.pack_file_paths();
        let generic_pack_count = paths.len();
        let mut pack_bytes: u64 = paths
            .iter()
            .flat_map(|(pack, index)| [*pack, *index])
            .map(file_len)
            .sum();
        let mut ids = manager.list_all_ids()?;
        drop(manager);
        let npk1 =
            self.store.npk1_manager().read().map_err(|_| {
                HeddleError::Config("Failed to acquire NPK1 manager lock".to_string())
            })?;
        let npk1_paths = npk1.file_paths();
        pack_bytes =
            pack_bytes.saturating_add(npk1_paths.iter().map(|path| file_len(path)).sum::<u64>());
        ids.extend(npk1.list_ids()?.into_iter().map(PackObjectId::Hash));
        let unique = ids.iter().copied().collect::<HashSet<_>>().len() as u64;
        Ok(RepackInventory {
            loose_objects: (loose_blobs.len() + loose_trees.len() + loose_states.len()) as u64,
            loose_bytes,
            pack_count: (generic_pack_count + npk1_paths.len()) as u64,
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
            && snapshot.loose_states.is_empty()
        {
            return Ok(RepackOutcome::default());
        }

        let staging = RepackStaging::new(&packs).map_err(RepackError::operation)?;
        let (expected_generic, expected_trees, logical_bytes, npk1_build) = self
            .build_staged(&snapshot, &staging, context)
            .map_err(|error| match error {
                BuildError::Cancelled(error) => error,
                BuildError::Store(error) => RepackError::operation(error),
            })?;
        verify_staged(&staging, &expected_generic, &expected_trees, context)?;
        context.checkpoint(0)?;

        // Cutover starts here and is intentionally non-cancellable. The new
        // immutable files are durable before any source path is retired.
        let (new_npk1_name, npk1_preexisting) = if npk1_build.is_some() {
            let (name, preexisting) =
                publish_npk1(&packs, &staging.npk1).map_err(RepackError::operation)?;
            (Some(name), preexisting)
        } else {
            (None, false)
        };
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
            new_npk1_name.as_deref(),
            replacement_preexisting,
            npk1_preexisting,
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
            objects_repacked: (expected_generic.len() + expected_trees.len()) as u64,
            bytes_repacked: logical_bytes,
            bytes_reclaimed: reclaimed,
        })
    }

    fn build_staged(
        &self,
        snapshot: &RepackSnapshot,
        staging: &RepackStaging,
        context: &RepackContext,
    ) -> std::result::Result<StagedBuild, BuildError> {
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
        let mut expected = HashSet::new();
        let mut expected_trees = HashSet::new();
        let mut state_ids = Vec::new();
        let mut tree_hashes = Vec::new();
        let mut blob_hashes = Vec::new();
        #[cfg(test)]
        let mut corrupt_first = self.corrupt_first_object;
        #[cfg(not(test))]
        let mut corrupt_first = false;
        let mut logical_bytes = 0u64;

        for (pack, index) in &snapshot.old_pack_files {
            let reader = PackReader::open(pack, index)?;
            let mut checkpoint_error = None;
            let visit = reader.visit_objects(|id, object_type, data| {
                if object_type == ObjectType::Blob
                    && matches!(id, PackObjectId::Hash(hash) if self.excluded_blobs.contains(&hash))
                {
                    if let Err(error) = context.checkpoint(data.len() as u64) {
                        checkpoint_error = Some(error);
                        return Err(HeddleError::InvalidObject(
                            "repack source walk interrupted".to_string(),
                        ));
                    }
                    return Ok(());
                }
                match (id, object_type) {
                    (PackObjectId::Hash(hash), ObjectType::Tree) => {
                        if expected_trees.insert(hash) {
                            tree_hashes.push(hash);
                        }
                    }
                    (PackObjectId::Hash(hash), ObjectType::Blob) => {
                        if !expected.insert(id) {
                            return Ok(());
                        }
                        blob_hashes.push(hash);
                    }
                    (PackObjectId::StateId(state_id), ObjectType::State) => {
                        if !expected.insert(id) {
                            return Ok(());
                        }
                        state_ids.push(state_id);
                    }
                    _ => {
                        if !expected.insert(id) {
                            return Ok(());
                        }
                        let mut data = data.to_vec();
                        if corrupt_first {
                            data.push(0xff);
                            corrupt_first = false;
                        }
                        logical_bytes = logical_bytes.saturating_add(data.len() as u64);
                        builder.add_id(id, object_type, data)?;
                    }
                }
                if let Err(error) = context.checkpoint(data.len() as u64) {
                    checkpoint_error = Some(error);
                    return Err(HeddleError::InvalidObject(
                        "repack source walk interrupted".to_string(),
                    ));
                }
                Ok(())
            });
            if let Some(error) = checkpoint_error {
                return Err(BuildError::Cancelled(error));
            }
            visit?;
        }
        for hash in &snapshot.npk1_trees {
            if expected_trees.insert(*hash) {
                tree_hashes.push(*hash);
            }
        }
        for hash in &snapshot.loose_blobs {
            let id = PackObjectId::Hash(*hash);
            if self.excluded_blobs.contains(hash) {
                continue;
            }
            if expected.insert(id) {
                blob_hashes.push(*hash);
            }
        }
        for hash in &snapshot.loose_trees {
            if expected_trees.insert(*hash) {
                tree_hashes.push(*hash);
            }
        }
        for id in &snapshot.loose_states {
            let object_id = PackObjectId::StateId(*id);
            if expected.insert(object_id) {
                state_ids.push(*id);
            }
        }
        let compact = add_compact_metadata(
            &self.store,
            &mut builder,
            &state_ids,
            &tree_hashes,
            &blob_hashes,
            context,
            &mut corrupt_first,
        )?;
        logical_bytes = logical_bytes.saturating_add(compact.logical_bytes);
        builder.finalize()?;
        let npk1_build = if compact.tree_order.is_empty() {
            None
        } else {
            let build = build_npk1_pack(
                &self.store,
                &compact.tree_order,
                &compact.tree_parents,
                &staging.npk1,
                context,
            )
            .map_err(|error| match error {
                Npk1BuildError::Store(error) => BuildError::Store(error),
                Npk1BuildError::Cancelled(error) => BuildError::Cancelled(error),
            })?;
            logical_bytes = logical_bytes.saturating_add(build.logical_bytes);
            Some(build)
        };
        Ok((expected, expected_trees, logical_bytes, npk1_build))
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
