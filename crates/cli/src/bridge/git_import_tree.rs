// SPDX-License-Identifier: Apache-2.0
//! Import Git trees as Heddle trees.

use std::collections::HashMap;

use objects::object::{Blob, ContentHash, FileMode, Tree, TreeEntry};
use objects::store::ObjectStore;
use objects::util::{GitTreeNameClassification, GitTreeNameLossyAction, classify_git_tree_name};
use repo::Repository as HeddleRepository;

use crate::bridge::git_core::{GitBridgeError, GitResult};
use crate::bridge::git_util::{GitImportOptions, LossyGitImportEntry};

const SUBMODULE_PREFIX: &str = "heddle-submodule:";

pub struct GitTreeImporter<'a> {
    heddle_repo: &'a HeddleRepository,
    repo: &'a gix::Repository,
    tree_cache: HashMap<gix::hash::ObjectId, ContentHash>,
    blob_cache: HashMap<gix::hash::ObjectId, ContentHash>,
    options: GitImportOptions,
    lossy_entries: Vec<LossyGitImportEntry>,
    lossy_by_tree: HashMap<gix::hash::ObjectId, Vec<LossyGitImportEntry>>,
}

impl<'a> GitTreeImporter<'a> {
    pub fn new(heddle_repo: &'a HeddleRepository, repo: &'a gix::Repository) -> Self {
        Self::with_options(heddle_repo, repo, GitImportOptions::default())
    }

    pub fn with_options(
        heddle_repo: &'a HeddleRepository,
        repo: &'a gix::Repository,
        options: GitImportOptions,
    ) -> Self {
        Self {
            heddle_repo,
            repo,
            tree_cache: HashMap::new(),
            blob_cache: HashMap::new(),
            options,
            lossy_entries: Vec::new(),
            lossy_by_tree: HashMap::new(),
        }
    }

    pub fn lossy_entries(&self) -> &[LossyGitImportEntry] {
        &self.lossy_entries
    }

    pub fn import_tree(&mut self, tree_oid: gix::hash::ObjectId) -> GitResult<ContentHash> {
        self.import_tree_at(tree_oid, "")
    }

    fn import_tree_at(
        &mut self,
        tree_oid: gix::hash::ObjectId,
        path_prefix: &str,
    ) -> GitResult<ContentHash> {
        if let Some(hash) = self.tree_cache.get(&tree_oid) {
            if let Some(entries) = self.lossy_by_tree.get(&tree_oid) {
                self.lossy_entries.extend(
                    entries
                        .iter()
                        .map(|entry| rebase_lossy_entry(path_prefix, entry)),
                );
            }
            return Ok(*hash);
        }

        let git_tree = self
            .repo
            .find_tree(tree_oid)
            .map_err(|err| GitBridgeError::Git(err.to_string()))?;

        let mut entries = Vec::new();
        let before_lossy = self.lossy_entries.len();

        for entry in git_tree.iter() {
            let entry = entry.map_err(|err| GitBridgeError::Git(err.to_string()))?;
            let Some(name) =
                self.import_entry_name(path_prefix, entry.filename().as_ref(), entry.object_id())?
            else {
                continue;
            };

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
                    let subtree_hash = self
                        .import_tree_at(entry.object_id(), &join_tree_path(path_prefix, &name))?;
                    entries.push(TreeEntry {
                        name,
                        mode: FileMode::Normal,
                        entry_type: objects::object::EntryType::Tree,
                        hash: subtree_hash,
                    });
                }
                gix::object::tree::EntryKind::Commit => {
                    let lossy = LossyGitImportEntry::converted(
                        join_tree_path(path_prefix, &name),
                        Some(entry.object_id().to_string()),
                        "gitlink/submodule entry converted to a heddle-submodule blob",
                    );
                    self.record_lossy(lossy)?;
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
        let tree_lossy_entries = self.lossy_entries[before_lossy..]
            .iter()
            .map(|entry| entry_relative_to_prefix(path_prefix, entry))
            .collect::<Vec<_>>();

        let tree = Tree::from_entries(entries);
        let hash = self.heddle_repo.store().put_tree(&tree)?;
        self.tree_cache.insert(tree_oid, hash);
        self.lossy_by_tree.insert(tree_oid, tree_lossy_entries);
        Ok(hash)
    }

    fn import_entry_name(
        &mut self,
        path_prefix: &str,
        raw_name: &[u8],
        object_id: gix::hash::ObjectId,
    ) -> GitResult<Option<String>> {
        match classify_git_tree_name(raw_name) {
            GitTreeNameClassification::Representable(name) => Ok(Some(name)),
            GitTreeNameClassification::NeedsLossy(lossy) => {
                let path = join_tree_path(path_prefix, &lossy.name);
                let entry = match lossy.action {
                    GitTreeNameLossyAction::Dropped => LossyGitImportEntry::dropped(
                        path,
                        Some(object_id.to_string()),
                        lossy.reason,
                    ),
                    GitTreeNameLossyAction::Converted => LossyGitImportEntry::converted(
                        path,
                        Some(object_id.to_string()),
                        lossy.reason,
                    ),
                };
                self.record_lossy(entry)?;
                if matches!(lossy.action, GitTreeNameLossyAction::Dropped) {
                    Ok(None)
                } else {
                    Ok(Some(lossy.name))
                }
            }
        }
    }

    fn record_lossy(&mut self, entry: LossyGitImportEntry) -> GitResult<()> {
        if !self.options.lossy {
            return Err(GitBridgeError::InvalidMapping(format!(
                "git import cannot represent tree entry losslessly: {}. Retry with --lossy to accept dropping or converting unrepresentable entries.",
                entry.summary_line()
            )));
        }
        tracing::warn!(entry = %entry.summary_line(), "lossy git import accepted");
        self.lossy_entries.push(entry);
        Ok(())
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

fn join_tree_path(prefix: &str, name: &str) -> String {
    let name = display_tree_name(name);
    if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    }
}

fn display_tree_name(name: &str) -> String {
    if name.bytes().any(|b| b < 0x20 || b == 0x7f) {
        name.escape_debug().to_string()
    } else {
        name.to_string()
    }
}

fn rebase_lossy_entry(prefix: &str, entry: &LossyGitImportEntry) -> LossyGitImportEntry {
    let mut rebased = entry.clone();
    if !prefix.is_empty() {
        rebased.path = format!("{prefix}/{}", entry.path);
    }
    rebased
}

fn entry_relative_to_prefix(prefix: &str, entry: &LossyGitImportEntry) -> LossyGitImportEntry {
    if prefix.is_empty() {
        return entry.clone();
    }

    let mut relative = entry.clone();
    if let Some(stripped) = entry.path.strip_prefix(prefix) {
        relative.path = stripped.trim_start_matches('/').to_string();
    }
    relative
}
