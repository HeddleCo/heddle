// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashSet, fs, path::PathBuf};

use super::super::{
    FsStore,
    fs_io::list_hashes_from_dir,
    fs_paths::{blobs_dir, trees_dir},
};
use crate::{
    object::ContentHash,
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
    pub(super) old_pack_files: Vec<(PathBuf, PathBuf)>,
    pub(super) commit_artifact_ids: Vec<ContentHash>,
}

impl RepackSnapshot {
    pub(super) fn capture(store: &FsStore) -> Result<Self> {
        let loose_blobs = list_hashes_from_dir(&blobs_dir(store.root()))?;
        let loose_trees = list_hashes_from_dir(&trees_dir(store.root()))?;
        let manager = store
            .pack_manager()
            .read()
            .map_err(|_| HeddleError::Config("Failed to acquire pack manager lock".to_string()))?;
        let ids = manager.list_all_ids()?;
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
        Ok(Self {
            ids,
            loose_blobs,
            loose_trees,
            old_pack_files,
            commit_artifact_ids,
        })
    }
}

pub(super) struct RepackStaging {
    pub(super) root: PathBuf,
    pub(super) pack: PathBuf,
    pub(super) index: PathBuf,
    pub(super) buckets: PathBuf,
}

impl RepackStaging {
    pub(super) fn new(packs: &std::path::Path) -> std::io::Result<Self> {
        let root = packs.join(format!(".repack-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root)?;
        Ok(Self {
            pack: root.join("replacement.pack"),
            index: root.join("replacement.idx"),
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
    expected: &HashSet<PackObjectId>,
    context: &RepackContext,
) -> std::result::Result<(), RepackError> {
    let reader = PackReader::open(&staging.pack, &staging.index).map_err(RepackError::operation)?;
    let ids = reader.list_ids().map_err(RepackError::operation)?;
    let actual = ids.iter().copied().collect::<HashSet<_>>();
    if actual != *expected || actual.len() != ids.len() {
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
