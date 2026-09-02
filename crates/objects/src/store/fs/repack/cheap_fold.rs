// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
};

use super::{
    cutover::{hash_file, try_acquire_automatic_repack_lock, try_acquire_repack_lock},
    staging::RepackStaging,
};
use crate::{
    fs_atomic::sync_directory,
    object::ContentHash,
    store::{
        HeddleError, Result,
        fs::{FsStore, fs_paths::packs_dir},
        pack::{PackObjectId, PackReader, StreamingPackBuilder},
        snapshot_commit::{snapshot_commit_marker_path, snapshot_commit_markers_by_pack},
    },
};

const MAX_PACKS_BEFORE_FOLD: usize = 8;
const MAX_SNAPSHOT_PACKS_PER_FOLD: usize = 8;

/// Cross-process ownership for one automatic-maintenance worker.
pub struct AutomaticRepackLock {
    _file: File,
}

/// Result of the bounded generic-pack fold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotPackFold {
    NotNeeded {
        pack_count: usize,
    },
    Busy,
    Folded {
        source_packs: usize,
        objects: usize,
        pack_count: usize,
    },
}

impl FsStore {
    /// Claim automatic maintenance without waiting behind another process.
    pub fn try_lock_automatic_repack(&self) -> Result<Option<AutomaticRepackLock>> {
        fs::create_dir_all(packs_dir(self.root()))?;
        Ok(try_acquire_automatic_repack_lock(&packs_dir(self.root()))?
            .map(|file| AutomaticRepackLock { _file: file }))
    }

    /// Fold a bounded number of recent, one-capture packs into one generic
    /// pack. Logical record bytes are carried forward unchanged: this does not
    /// build NPK1, shared blob frames, or new tree encodings.
    pub fn fold_snapshot_packs_if_needed(&self) -> Result<SnapshotPackFold> {
        let packs = packs_dir(self.root());
        fs::create_dir_all(&packs)?;
        let Some(_repack_lock) = try_acquire_repack_lock(&packs)? else {
            return Ok(SnapshotPackFold::Busy);
        };
        self.reload_packs()?;

        let markers = snapshot_commit_markers_by_pack(&packs)?;
        let (pack_count, candidates) = {
            let manager = self.pack_manager().read().map_err(|_| {
                HeddleError::Config("Failed to acquire pack manager lock".to_string())
            })?;
            let paths = manager.pack_file_paths();
            let pack_count = paths.len();
            let candidates = paths
                .into_iter()
                .filter_map(|(pack, index)| {
                    let name = pack.file_stem()?.to_str()?;
                    (markers.get(name).is_some_and(|values| values.len() == 1))
                        .then(|| (pack.to_path_buf(), index.to_path_buf(), name.to_string()))
                })
                .collect::<Vec<_>>();
            (pack_count, candidates)
        };
        if pack_count <= MAX_PACKS_BEFORE_FOLD {
            return Ok(SnapshotPackFold::NotNeeded { pack_count });
        }

        let desired_sources = pack_count
            .saturating_sub(MAX_PACKS_BEFORE_FOLD)
            .saturating_add(1)
            .clamp(2, MAX_SNAPSHOT_PACKS_PER_FOLD);
        let selected = candidates
            .into_iter()
            .rev()
            .take(desired_sources)
            .collect::<Vec<_>>();
        if selected.len() < 2 {
            return Ok(SnapshotPackFold::NotNeeded { pack_count });
        }

        let staging = RepackStaging::new(&packs)?;
        let pack_file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&staging.pack)?;
        let mut compression = self.compression();
        compression.max_delta_size = 0;
        let mut builder = StreamingPackBuilder::new(
            pack_file,
            staging.index.clone(),
            compression,
            staging.buckets.clone(),
        )?;
        let mut expected = HashSet::<PackObjectId>::new();

        // `selected` is newest-first. Keep the newest physical encoding when
        // immutable identities repeat across incremental snapshots.
        for (pack, index, _) in &selected {
            let reader = PackReader::open(pack, index)?;
            reader.visit_objects(|id, object_type, data| {
                if expected.insert(id) {
                    builder.add_id(id, object_type, data.to_vec())?;
                }
                Ok(())
            })?;
        }
        let (_pack_file, stats) = builder.finalize()?;
        let replacement = hash_file(&staging.pack)?;
        let reader = PackReader::open(&staging.pack, &staging.index)?;
        let actual = reader.list_ids()?.into_iter().collect::<HashSet<_>>();
        if actual != expected || stats.object_count as usize != expected.len() {
            return Err(HeddleError::InvalidObject(
                "snapshot pack fold changed the logical object set".to_string(),
            ));
        }

        self.install_pack_files_streaming(&staging.pack, &staging.index)?;
        let replacement_pack = packs.join(format!("{replacement}.pack"));
        for (_, _, name) in &selected {
            if let Some(pack_markers) = markers.get(name) {
                for (artifact_id, bytes) in pack_markers {
                    install_commit_marker(&replacement_pack, artifact_id, bytes)?;
                }
            }
        }
        sync_directory(&packs)?;

        for (pack, index, name) in &selected {
            if pack != &replacement_pack {
                remove_file_ignore_missing(pack)?;
                remove_file_ignore_missing(index)?;
                if let Some(pack_markers) = markers.get(name) {
                    for (artifact_id, _) in pack_markers {
                        remove_file_ignore_missing(&snapshot_commit_marker_path(
                            pack,
                            artifact_id,
                        ))?;
                    }
                }
            }
        }
        sync_directory(&packs)?;
        self.reload_packs()?;

        let resulting_count = self
            .pack_manager()
            .read()
            .map_err(|_| HeddleError::Config("Failed to acquire pack manager lock".to_string()))?
            .pack_count();
        Ok(SnapshotPackFold::Folded {
            source_packs: selected.len(),
            objects: expected.len(),
            pack_count: resulting_count,
        })
    }
}

fn install_commit_marker(pack_path: &Path, artifact_id: &ContentHash, bytes: &[u8]) -> Result<()> {
    let marker = snapshot_commit_marker_path(pack_path, artifact_id);
    match OpenOptions::new().write(true).create_new(true).open(marker) {
        Ok(mut file) => {
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_file_ignore_missing(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use heddle_format::compression::CompressionConfig;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        object::StateId,
        store::{
            SNAPSHOT_COMMIT_ARTIFACT_SCHEMA, SnapshotCommitArtifact,
            fs::pack_install_journal::install_committed_snapshot_pack_bytes,
            pack::{ObjectType, PackBuilder},
        },
    };

    #[test]
    fn fold_bounds_incremental_packs_and_preserves_commit_markers() {
        let temp = TempDir::new().unwrap();
        let store = FsStore::new(temp.path().join(".heddle"));
        store.init().unwrap();
        let packs = packs_dir(store.root());
        let mut artifact_ids = Vec::new();
        let mut tree_records = Vec::new();

        for ordinal in 0..9u8 {
            let artifact = SnapshotCommitArtifact {
                schema: SNAPSHOT_COMMIT_ARTIFACT_SCHEMA,
                transaction_id: format!("fold-{ordinal}"),
                scope: "snapshot".to_string(),
                base_oplog_head_id: ordinal as u64,
                state: StateId::from_bytes([ordinal.saturating_add(1); 32]),
                encoded_records: vec![vec![ordinal]],
            };
            let artifact_id = artifact.id();
            let bytes = rmp_serde::to_vec_named(&artifact).unwrap();
            let mut builder = PackBuilder::new(CompressionConfig::disabled());
            builder.add(artifact_id, ObjectType::SnapshotCommit, bytes.clone());
            let payload = format!("incremental payload {ordinal}").into_bytes();
            builder.add(ContentHash::compute(&payload), ObjectType::Blob, payload);
            let tree_bytes = format!("HDC1-preserved-tree-{ordinal}").into_bytes();
            let tree_id = ContentHash::compute(&tree_bytes);
            builder.add(tree_id, ObjectType::Tree, tree_bytes.clone());
            let (pack, index, _) = builder.build().unwrap();
            install_committed_snapshot_pack_bytes(&packs, pack, index, artifact_id, bytes).unwrap();
            artifact_ids.push(artifact_id);
            tree_records.push((tree_id, tree_bytes));
        }
        store.reload_packs().unwrap();

        assert_eq!(
            store.fold_snapshot_packs_if_needed().unwrap(),
            SnapshotPackFold::Folded {
                source_packs: 2,
                objects: 6,
                pack_count: 8,
            }
        );
        assert_eq!(
            store.fold_snapshot_packs_if_needed().unwrap(),
            SnapshotPackFold::NotNeeded { pack_count: 8 }
        );

        let manager = store.pack_manager().read().unwrap();
        let descriptors = manager.snapshot_commit_recovery_descriptors().unwrap();
        assert_eq!(descriptors.len(), artifact_ids.len());
        for artifact_id in artifact_ids {
            assert!(
                manager
                    .get_object(&PackObjectId::Hash(artifact_id))
                    .unwrap()
                    .is_some(),
                "fold must preserve every authoritative snapshot artifact"
            );
        }
        for (tree_id, expected_bytes) in tree_records {
            assert_eq!(
                manager.get_object(&PackObjectId::Hash(tree_id)).unwrap(),
                Some((ObjectType::Tree, expected_bytes)),
                "fold must carry HDC1/HLR1 tree record bytes forward unchanged"
            );
        }
        assert!(
            std::fs::read_dir(&packs).unwrap().all(|entry| entry
                .unwrap()
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                != Some("npk")),
            "cheap fold must not rebuild NPK1"
        );
    }
}
