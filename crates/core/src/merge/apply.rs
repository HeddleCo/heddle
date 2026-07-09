// SPDX-License-Identifier: Apache-2.0
//! Apply a merged tree onto the repository worktree.

use std::{collections::HashMap, fs, path::Path};

use anyhow::{Result, anyhow};
use objects::{
    object::{Tree, TreeEntry},
    store::ObjectStore,
};
use repo::Repository;

use super::{advice, prepare_dir_for_file_replacement};

/// Materialize a computed merge tree into the repository worktree.
pub fn apply_merged_tree(repo: &Repository, tree: &Tree) -> Result<()> {
    // Two distinct "no tree" cases collapse here. Keep them apart:
    //
    // * No current state — fresh repo / first capture / pre-init goto.
    //   `Tree::default()` is the right baseline: there's nothing tracked
    //   to diff against, so the merged tree wins outright.
    // * Current state exists but its `state.tree` hash isn't in the
    //   store — corruption. Surface this as a hard error.
    let current_tree = match repo.current_state()? {
        Some(state) => repo.store().get_tree(&state.tree)?.ok_or_else(|| {
            anyhow!(advice::merge_integrity_refusal(
                format!(
                    "current state {} references tree {} but the object store has no such tree; \
                     aborting merge application to avoid silently replacing tracked content with \
                     an empty baseline",
                    state.change_id.short(),
                    state.tree,
                ),
                format!(
                    "current state {} references missing tree {} in the object store",
                    state.change_id.short(),
                    state.tree,
                ),
                "merge application would compare against an empty baseline and could silently replace tracked content with the merged tree, losing tracked paths outside it",
                "merge application stopped before materializing the merged tree; current HEAD, refs, object store, and worktree were left unchanged",
            ))
        })?,
        None => Tree::default(),
    };

    let current_entries: HashMap<&str, &TreeEntry> = current_tree
        .entries()
        .iter()
        .map(|e| (e.name(), e))
        .collect();
    let merged_entries: HashMap<&str, &TreeEntry> =
        tree.entries().iter().map(|e| (e.name(), e)).collect();

    // Drop tree-entries that don't survive into the merged tree.
    for (name, current) in &current_entries {
        if !merged_entries.contains_key(name) {
            let path = repo.root().join(name);
            remove_path_for_drop(repo, &path, current, &current_tree)?;
        }
    }

    // Type-change entries (file ↔ dir at the same name): clear the
    // existing thing first so `materialize_tree` can write the new
    // node type.
    for (name, merged) in &merged_entries {
        if let Some(current) = current_entries.get(name)
            && current.entry_type() != merged.entry_type()
        {
            let path = repo.root().join(name);
            remove_path_for_type_change(repo, &path, current, merged, &current_tree)?;
        }
    }

    repo.materialize_computed_tree(tree, repo.root())?;

    Ok(())
}

fn remove_path_for_drop(
    repo: &Repository,
    path: &Path,
    current: &TreeEntry,
    current_tree: &Tree,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(anyhow::Error::from(error)),
    };
    if metadata.is_symlink() || metadata.is_file() {
        fs::remove_file(path)?;
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    let source_subtree = source_subtree_for(repo, current, current_tree, current.name())?;
    repo.remove_tracked_descendants_with_source(path, &source_subtree)?;
    Ok(())
}

fn remove_path_for_type_change(
    repo: &Repository,
    path: &Path,
    current: &TreeEntry,
    merged: &TreeEntry,
    current_tree: &Tree,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(anyhow::Error::from(error)),
    };

    if metadata.is_symlink() || metadata.is_file() {
        fs::remove_file(path)?;
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }

    let _ = merged;
    let source_subtree = source_subtree_for(repo, current, current_tree, current.name())?;
    repo.remove_tracked_descendants_with_source(path, &source_subtree)?;
    if path.exists() {
        prepare_dir_for_file_replacement(path)?;
    }
    Ok(())
}

fn source_subtree_for(
    repo: &Repository,
    entry: &TreeEntry,
    current_tree: &Tree,
    name: &str,
) -> Result<Tree> {
    if entry.entry_type() != objects::object::EntryType::Tree {
        return Ok(Tree::default());
    }
    let Some(hash) = entry.tree_hash() else {
        return Ok(Tree::default());
    };
    repo.resolve_subtree(current_tree, Path::new(name))?
        .ok_or_else(|| {
            anyhow!(advice::merge_integrity_refusal(
                format!(
                    "current tree records subtree {:?} (hash {}) but the object store cannot \
                     resolve it; aborting merge application to avoid leaving the subtree's tracked \
                     descendants orphaned on disk",
                    name,
                    hash,
                ),
                format!(
                    "current tree records subtree {:?} with missing hash {} in the object store",
                    name,
                    hash,
                ),
                "merge application would drop the directory entry without a source subtree and could leave its tracked descendants orphaned on disk as untracked additions",
                "repository HEAD, refs, and object store were left unchanged; merge application stopped before removing this subtree's tracked descendants or writing the final merged tree",
            ))
        })
}
