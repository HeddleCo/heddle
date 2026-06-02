// SPDX-License-Identifier: Apache-2.0
//! Import Git trees as Heddle trees.

use std::collections::HashMap;

use objects::{
    object::{Blob, ContentHash, FileMode, Tree, TreeEntry},
    store::ObjectStore,
};
use repo::Repository as HeddleRepository;

use crate::bridge::git_core::{GitBridgeError, GitResult};

const SUBMODULE_PREFIX: &str = "heddle-submodule:";

pub struct GitTreeImporter<'a> {
    heddle_repo: &'a HeddleRepository,
    repo: &'a gix::Repository,
    tree_cache: HashMap<gix::hash::ObjectId, ContentHash>,
    blob_cache: HashMap<gix::hash::ObjectId, ContentHash>,
}

impl<'a> GitTreeImporter<'a> {
    pub fn new(heddle_repo: &'a HeddleRepository, repo: &'a gix::Repository) -> Self {
        Self {
            heddle_repo,
            repo,
            tree_cache: HashMap::new(),
            blob_cache: HashMap::new(),
        }
    }

    pub fn import_tree(&mut self, tree_oid: gix::hash::ObjectId) -> GitResult<ContentHash> {
        if let Some(hash) = self.tree_cache.get(&tree_oid) {
            return Ok(*hash);
        }

        let git_tree = self
            .repo
            .find_tree(tree_oid)
            .map_err(|err| GitBridgeError::Git(err.to_string()))?;

        let mut entries = Vec::new();

        for entry in git_tree.iter() {
            let entry = entry.map_err(|err| GitBridgeError::Git(err.to_string()))?;
            let name = String::from_utf8_lossy(entry.filename().as_ref()).into_owned();

            match entry.kind() {
                gix::object::tree::EntryKind::Blob
                | gix::object::tree::EntryKind::BlobExecutable => {
                    let hash = self.import_blob(entry.object_id())?;

                    let mode =
                        if matches!(entry.kind(), gix::object::tree::EntryKind::BlobExecutable) {
                            FileMode::Executable
                        } else {
                            FileMode::Normal
                        };

                    entries.push(TreeEntry {
                        name,
                        mode,
                        entry_type: objects::object::EntryType::Blob,
                        hash,
                    });
                }
                gix::object::tree::EntryKind::Link => {
                    let hash = self.import_blob(entry.object_id())?;
                    // Phase E: must be `EntryType::Symlink` so the
                    // materialization planner reaches the symlink-write
                    // branch. Previously this said `EntryType::Blob`,
                    // which routed checkout through the regular file
                    // path and wrote the symlink target as file content
                    // (so e.g. ripgrep's `HomebrewFormula -> pkg/brew`
                    // appeared on disk as an 8-byte text file containing
                    // "pkg/brew" rather than a symlink).
                    entries.push(TreeEntry {
                        name,
                        mode: FileMode::Symlink,
                        entry_type: objects::object::EntryType::Symlink,
                        hash,
                    });
                }
                gix::object::tree::EntryKind::Tree => {
                    let subtree_hash = self.import_tree(entry.object_id())?;
                    entries.push(TreeEntry {
                        name,
                        mode: FileMode::Normal,
                        entry_type: objects::object::EntryType::Tree,
                        hash: subtree_hash,
                    });
                }
                gix::object::tree::EntryKind::Commit => {
                    let hash = self.import_gitlink(entry.object_id())?;
                    entries.push(TreeEntry {
                        name,
                        mode: FileMode::Normal,
                        entry_type: objects::object::EntryType::Blob,
                        hash,
                    });
                }
            }
        }

        let tree = Tree::from_entries(entries);
        let hash = self.heddle_repo.store().put_tree(&tree)?;
        self.tree_cache.insert(tree_oid, hash);
        Ok(hash)
    }

    fn import_blob(&mut self, blob_oid: gix::hash::ObjectId) -> GitResult<ContentHash> {
        if let Some(hash) = self.blob_cache.get(&blob_oid) {
            return Ok(*hash);
        }

        let mut blob = self
            .repo
            .find_blob(blob_oid)
            .map_err(|err| GitBridgeError::Git(err.to_string()))?;

        let heddle_blob = Blob::new(blob.take_data());
        let hash = self.heddle_repo.store().put_blob(&heddle_blob)?;
        self.blob_cache.insert(blob_oid, hash);
        Ok(hash)
    }

    fn import_gitlink(&mut self, oid: gix::hash::ObjectId) -> GitResult<ContentHash> {
        if let Some(hash) = self.blob_cache.get(&oid) {
            return Ok(*hash);
        }

        let blob = Blob::new(format!("{} {}", SUBMODULE_PREFIX, oid).into_bytes());
        let hash = self.heddle_repo.store().put_blob(&blob)?;
        self.blob_cache.insert(oid, hash);
        Ok(hash)
    }
}

/// Import a Git tree as a Heddle tree.
pub fn import_git_tree(
    heddle_repo: &HeddleRepository,
    repo: &gix::Repository,
    tree_oid: gix::hash::ObjectId,
) -> GitResult<ContentHash> {
    GitTreeImporter::new(heddle_repo, repo).import_tree(tree_oid)
}
