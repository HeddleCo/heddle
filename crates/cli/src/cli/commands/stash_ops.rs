// SPDX-License-Identifier: Apache-2.0
//! Stash worktree helper operations.

use std::fs;

use anyhow::{Result, anyhow};
use objects::{
    fs_ops::remove_path_recursively,
    object::{Blob, ContentHash, Tree, TreeEntry},
    worktree::WorktreeStatus,
};
use repo::{Repository, StashEntry};

pub(super) fn build_worktree_tree(repo: &Repository, status: &WorktreeStatus) -> Result<Tree> {
    let mut tree = Tree::new();

    for path in &status.modified {
        let full_path = repo.root().join(path);
        if full_path.exists() {
            let content = fs::read(&full_path)?;
            let blob = Blob::new(content);
            let hash = repo.store().put_blob(&blob)?;
            let entry = TreeEntry::file(path.to_string_lossy().to_string(), hash, false)?;
            tree.insert(entry);
        }
    }

    for path in &status.added {
        let full_path = repo.root().join(path);
        if full_path.exists() {
            let content = fs::read(&full_path)?;
            let blob = Blob::new(content);
            let hash = repo.store().put_blob(&blob)?;
            let entry = TreeEntry::file(path.to_string_lossy().to_string(), hash, false)?;
            tree.insert(entry);
        }
    }

    Ok(tree)
}

pub(super) fn restore_worktree(
    repo: &Repository,
    tree: &Tree,
    status: &WorktreeStatus,
) -> Result<()> {
    for path in &status.modified {
        let full_path = repo.root().join(path);
        if let Some(entry) = tree.get(&path.to_string_lossy()) {
            let blob = repo.require_blob(&entry.hash)?;
            fs::write(&full_path, blob.content())?;
        }
    }

    for path in &status.added {
        let full_path = repo.root().join(path);
        if full_path.is_symlink() {
            fs::remove_file(&full_path)?;
        } else if full_path.is_dir() {
            remove_path_recursively(&full_path)?;
        } else if full_path.exists() {
            fs::remove_file(&full_path)?;
        }
    }

    Ok(())
}

pub(super) fn apply_stash(repo: &Repository, stash: &StashEntry) -> Result<()> {
    let stash_tree_hash = ContentHash::from_hex(&stash.tree_hash)
        .map_err(|e| anyhow!("Invalid stash tree hash: {}", e))?;
    let stash_tree = repo.store().get_tree(&stash_tree_hash)?.unwrap_or_default();

    for entry in stash_tree.entries() {
        let full_path = repo.root().join(&entry.name);
        let blob = repo.require_blob(&entry.hash)?;
        if let Some(parent) = full_path.parent()
            && !parent.exists()
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(&full_path, blob.content())?;
    }

    Ok(())
}