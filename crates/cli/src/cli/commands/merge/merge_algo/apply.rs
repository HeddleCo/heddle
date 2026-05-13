// SPDX-License-Identifier: Apache-2.0
use std::{collections::HashMap, fs, path::Path};

use anyhow::Result;
use objects::object::{Tree, TreeEntry};
use repo::Repository;

use super::super::prepare_dir_for_file_replacement;

pub(crate) fn apply_merged_tree(repo: &Repository, tree: &Tree) -> Result<()> {
    let current_tree = repo
        .current_state()?
        .and_then(|s| repo.store().get_tree(&s.tree).transpose())
        .transpose()?
        .unwrap_or_default();

    let current_entries: HashMap<&str, &TreeEntry> = current_tree
        .entries()
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();
    let merged_entries: HashMap<&str, &TreeEntry> = tree
        .entries()
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();

    // Drop tree-entries that don't survive into the merged tree.
    for (name, current) in &current_entries {
        if !merged_entries.contains_key(name) {
            let path = repo.root().join(name);
            remove_path_for_drop(repo, &path, current, &current_tree)?;
        }
    }

    // Type-change entries (file ↔ dir at the same name): clear the
    // existing thing first so `materialize_tree` can write the new
    // node type. Without explicit handling here, a dir → file change
    // explodes inside `materialize_blob` ("Is a directory" from
    // `fs::write` after `remove_file` no-ops on the dir).
    for (name, merged) in &merged_entries {
        if let Some(current) = current_entries.get(name)
            && current.entry_type != merged.entry_type
        {
            let path = repo.root().join(name);
            remove_path_for_type_change(repo, &path, current, merged, &current_tree)?;
        }
    }

    repo.materialize_tree(tree, repo.root())?;

    Ok(())
}

/// Remove a tree-entry that's gone in the merged tree. We don't care
/// what the merged side wanted here — the entry is gone, period.
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
    // Strip only heddle-tracked descendants. Heddle-ignored siblings
    // (`.git/`, `target/`, `node_modules/`, …) must survive a merge
    // that drops a tracked top-level directory; a recursive nuke
    // would otherwise destroy the user's local build/dependency
    // state alongside the tracked content.
    let source_subtree = source_subtree_for(repo, current, current_tree, &current.name)?;
    repo.remove_tracked_descendants_with_source(path, &source_subtree)?;
    Ok(())
}

/// Type-change at `path`: prepare disk for the new entry type. The
/// new entry's type is in `merged`; we need to clear what's there now
/// (described by `current`) so the materializer can write the
/// replacement.
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
        // file/symlink → dir: removing the file is enough; the
        // materializer will mkdir.
        fs::remove_file(path)?;
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }

    // dir → (file or symlink): drain the tracked content with a
    // tree-driven walk, then explicitly drop the directory. If
    // heddle-ignored content is keeping the directory occupied,
    // `prepare_dir_for_file_replacement` errors with a clear message
    // — the alternative is `materialize_blob` blowing up with a bare
    // "Is a directory" deep in the materializer.
    let _ = merged; // current type vs. merged type — currently both branches treat this the same.
    let source_subtree = source_subtree_for(repo, current, current_tree, &current.name)?;
    repo.remove_tracked_descendants_with_source(path, &source_subtree)?;
    if path.exists() {
        prepare_dir_for_file_replacement(path)?;
    }
    Ok(())
}

/// Resolve the subtree under `entry` (a top-level entry of `current_tree`).
/// Falls back to an empty tree if the entry isn't a Tree-typed entry or
/// the subtree object is missing — both cases mean "no tracked
/// descendants" and the caller will skip removal.
fn source_subtree_for(
    repo: &Repository,
    entry: &TreeEntry,
    current_tree: &Tree,
    name: &str,
) -> Result<Tree> {
    if entry.entry_type == objects::object::EntryType::Tree {
        Ok(repo
            .resolve_subtree(current_tree, Path::new(name))?
            .unwrap_or_default())
    } else {
        Ok(Tree::default())
    }
}