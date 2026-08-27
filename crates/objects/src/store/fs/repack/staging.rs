// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use super::super::{
    FsStore,
    fs_io::{list_hashes_from_dir, list_state_ids_from_dir},
    fs_paths::{blobs_dir, states_dir, trees_dir},
    npk1::Npk1Pack,
};
use crate::{
    object::{ContentHash, StateId},
    store::{
        HeddleError, Result,
        fs::fs_impl::validate_pack_entry,
        pack::{PackObjectId, PackReader, RepackContext, RepackError},
    },
};

pub(super) struct RepackSnapshot {
    pub(super) ids: Vec<PackObjectId>,
    pub(super) loose_blobs: Vec<ContentHash>,
    pub(super) loose_trees: Vec<ContentHash>,
    pub(super) loose_states: Vec<StateId>,
    pub(super) old_pack_files: Vec<(PathBuf, PathBuf)>,
    pub(super) old_npk1_files: Vec<PathBuf>,
    pub(super) npk1_trees: Vec<ContentHash>,
    pub(super) commit_artifact_ids: Vec<ContentHash>,
}

impl RepackSnapshot {
    pub(super) fn capture(store: &FsStore) -> Result<Self> {
        let loose_blobs = list_hashes_from_dir(&blobs_dir(store.root()))?;
        let loose_trees = list_hashes_from_dir(&trees_dir(store.root()))?;
        let loose_states = list_state_ids_from_dir(&states_dir(store.root()))?;
        let manager = store
            .pack_manager()
            .read()
            .map_err(|_| HeddleError::Config("Failed to acquire pack manager lock".to_string()))?;
        let mut ids = manager.list_all_ids()?;
        let old_pack_files = manager
            .pack_file_paths()
            .into_iter()
            .map(|(pack, index)| (pack.to_path_buf(), index.to_path_buf()))
            .collect();
        let commit_artifact_ids = manager
            .snapshot_commit_descriptors()?
            .into_iter()
            .map(|descriptor| descriptor.artifact.id())
            .collect();
        drop(manager);
        let npk1 = store
            .npk1_manager()
            .read()
            .map_err(|_| HeddleError::Config("Failed to acquire NPK1 manager lock".to_string()))?;
        let npk1_ids = npk1.list_ids()?;
        ids.extend(npk1_ids.iter().copied().map(PackObjectId::Hash));
        let old_npk1_files = npk1
            .file_paths()
            .into_iter()
            .map(Path::to_path_buf)
            .collect();
        Ok(Self {
            ids,
            loose_blobs,
            loose_trees,
            loose_states,
            old_pack_files,
            old_npk1_files,
            npk1_trees: npk1_ids,
            commit_artifact_ids,
        })
    }
}

pub(super) struct RepackStaging {
    pub(super) root: PathBuf,
    pub(super) pack: PathBuf,
    pub(super) index: PathBuf,
    pub(super) npk1: PathBuf,
    pub(super) buckets: PathBuf,
}

impl RepackStaging {
    pub(super) fn new(packs: &std::path::Path) -> std::io::Result<Self> {
        let root = packs.join(format!(".repack-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root)?;
        Ok(Self {
            pack: root.join("replacement.pack"),
            index: root.join("replacement.idx"),
            npk1: root.join("replacement.npk"),
            buckets: root.join("index-buckets"),
            root,
        })
    }
}

impl Drop for RepackStaging {
    fn drop(&mut self) {
        let _ = remove_tree(&self.root);
    }
}

#[derive(Debug)]
pub(super) enum BuildError {
    Store(HeddleError),
    Cancelled(RepackError),
}

impl From<HeddleError> for BuildError {
    fn from(error: HeddleError) -> Self {
        Self::Store(error)
    }
}

impl From<std::io::Error> for BuildError {
    fn from(error: std::io::Error) -> Self {
        Self::Store(HeddleError::Io(error))
    }
}

pub(super) fn verify_staged(
    staging: &RepackStaging,
    expected_generic: &HashSet<PackObjectId>,
    expected_trees: &HashSet<ContentHash>,
    context: &RepackContext,
) -> std::result::Result<(), RepackError> {
    let reader = PackReader::open(&staging.pack, &staging.index).map_err(RepackError::operation)?;
    let ids = reader.list_ids().map_err(RepackError::operation)?;
    let actual = ids.iter().copied().collect::<HashSet<_>>();
    if actual != *expected_generic || actual.len() != ids.len() {
        return Err(RepackError::message(
            "replacement pack object set differs from the source snapshot",
        ));
    }
    let mut checkpoint_error = None;
    let verification = reader.visit_objects(|id, object_type, data| {
        validate_pack_entry(&id, object_type, data)?;
        if let Err(error) = context.checkpoint(data.len() as u64) {
            checkpoint_error = Some(error);
            return Err(HeddleError::InvalidObject(
                "compact verification interrupted".to_string(),
            ));
        }
        Ok(())
    });
    if let Some(error) = checkpoint_error {
        return Err(error);
    }
    verification.map_err(RepackError::operation)?;
    if !expected_trees.is_empty() {
        let npk1 = Npk1Pack::open(&staging.npk1).map_err(RepackError::operation)?;
        let actual = npk1.ids().collect::<HashSet<_>>();
        if actual != *expected_trees || actual.len() != expected_trees.len() {
            return Err(RepackError::message(
                "replacement NPK1 object set differs from the source snapshot",
            ));
        }
        for hash in expected_trees {
            let tree = npk1.resolve(hash).map_err(RepackError::operation)?;
            context.checkpoint(tree.len() as u64)?;
        }
    }
    Ok(())
}

fn remove_tree(root: &std::path::Path) -> std::io::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let entries = fs::read_dir(root)?.collect::<std::io::Result<Vec<_>>>()?;
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || metadata.is_file() {
            fs::remove_file(path)?;
        } else if metadata.is_dir() {
            remove_tree(&path)?;
        }
    }
    fs::remove_dir(root)
}
