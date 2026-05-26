// SPDX-License-Identifier: Apache-2.0
//! Import Git trees as Heddle trees.

use std::collections::HashMap;

use objects::object::{Blob, ContentHash, FileMode, Tree, TreeEntry};
use repo::Repository as HeddleRepository;

use crate::bridge::git_core::{GitBridgeError, GitResult};

const SUBMODULE_PREFIX: &str = "heddle-submodule:";

pub struct GitTreeImporter<'a> {
    heddle_repo: &'a HeddleRepository,
    repo: &'a gix::Repository,
    tree_cache: HashMap<gix::hash::ObjectId, ContentHash>,
    blob_cache: HashMap<gix::hash::ObjectId, ContentHash>,
    /// Blobs and trees to write at flush time. Buffering them turns
    /// N `put_blob` / `put_tree` syscalls (one fsync each, ~3.6ms on
    /// Linux ext4) into one `put_*_packed` pack file each (one fsync
    /// per pack). Empty unless `flush_pending_writes` has yet to run.
    pending_blobs: Vec<(ContentHash, Vec<u8>)>,
    pending_trees: Vec<(ContentHash, Vec<u8>)>,
}

impl<'a> GitTreeImporter<'a> {
    pub fn new(heddle_repo: &'a HeddleRepository, repo: &'a gix::Repository) -> Self {
        Self {
            heddle_repo,
            repo,
            tree_cache: HashMap::new(),
            blob_cache: HashMap::new(),
            pending_blobs: Vec::new(),
            pending_trees: Vec::new(),
        }
    }

    /// Persist any buffered blob/tree writes as packs. MUST be called
    /// before the importer is dropped, otherwise objects referenced
    /// by imported states won't actually exist in the store.
    pub fn flush_pending_writes(&mut self) -> GitResult<()> {
        if !self.pending_blobs.is_empty() {
            let batch = std::mem::take(&mut self.pending_blobs);
            self.heddle_repo
                .store()
                .put_blobs_packed(batch)
                .map_err(|e| GitBridgeError::Git(format!("flush packed blobs: {e}")))?;
        }
        if !self.pending_trees.is_empty() {
            let batch = std::mem::take(&mut self.pending_trees);
            self.heddle_repo
                .store()
                .put_trees_packed(batch)
                .map_err(|e| GitBridgeError::Git(format!("flush packed trees: {e}")))?;
        }
        Ok(())
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
        let hash = tree.hash();
        // Defer the disk write. `Tree::hash` is a pure function of
        // entries, so the hash is stable now; callers needing the
        // tree to actually be on disk must invoke
        // `flush_pending_writes`.
        let serialized = rmp_serde::to_vec(&tree)
            .map_err(|e| GitBridgeError::Git(format!("rmp encode tree: {e}")))?;
        self.pending_trees.push((hash, serialized));
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

        let data = blob.take_data();
        let heddle_blob = Blob::from_slice(&data);
        let hash = heddle_blob.hash();
        // Defer the actual disk write — caller MUST call
        // `flush_pending_writes` before relying on the blob being
        // present. The hash is stable now because Blob's hash is a
        // pure function of bytes.
        self.pending_blobs.push((hash, data));
        self.blob_cache.insert(blob_oid, hash);
        Ok(hash)
    }

    fn import_gitlink(&mut self, oid: gix::hash::ObjectId) -> GitResult<ContentHash> {
        if let Some(hash) = self.blob_cache.get(&oid) {
            return Ok(*hash);
        }

        let data = format!("{} {}", SUBMODULE_PREFIX, oid).into_bytes();
        let blob = Blob::from_slice(&data);
        let hash = blob.hash();
        self.pending_blobs.push((hash, data));
        self.blob_cache.insert(oid, hash);
        Ok(hash)
    }
}

/// Import a Git tree as a Heddle tree. Convenience wrapper: persists
/// blobs and trees before returning. Long-running callers should
/// instead instantiate [`GitTreeImporter`] directly and call
/// [`GitTreeImporter::flush_pending_writes`] once at the end of the
/// import to batch the writes into a single packfile.
pub fn import_git_tree(
    heddle_repo: &HeddleRepository,
    repo: &gix::Repository,
    tree_oid: gix::hash::ObjectId,
) -> GitResult<ContentHash> {
    let mut importer = GitTreeImporter::new(heddle_repo, repo);
    let hash = importer.import_tree(tree_oid)?;
    importer.flush_pending_writes()?;
    Ok(hash)
}
