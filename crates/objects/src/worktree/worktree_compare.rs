// SPDX-License-Identifier: Apache-2.0
//! Worktree comparison logic.

use std::{collections::HashMap, fs, path::Path};

use super::{worktree_ignore::should_ignore, worktree_types::WorktreeStatus};
use crate::{
    error::Result,
    object::{Blob, EntryType, Tree, TreeEntry},
    store::ObjectStore,
};

/// Compare worktree against a tree.
pub fn compare_worktree<S: ObjectStore + ?Sized>(
    store: &S,
    root: &Path,
    tree: &Tree,
    ignore_patterns: &[String],
) -> Result<WorktreeStatus> {
    let mut status = WorktreeStatus::default();
    compare_worktree_recursive(store, root, root, Some(tree), ignore_patterns, &mut status)?;

    status.modified.sort();
    status.added.sort();
    status.deleted.sort();

    Ok(status)
}

fn compare_worktree_recursive<S: ObjectStore + ?Sized>(
    store: &S,
    base: &Path,
    dir: &Path,
    tree: Option<&Tree>,
    ignore_patterns: &[String],
    status: &mut WorktreeStatus,
) -> Result<()> {
    let tree_entries: HashMap<&str, &TreeEntry> = tree
        .map(|t| t.entries().iter().map(|e| (e.name.as_str(), e)).collect())
        .unwrap_or_default();

    let mut seen_entries: std::collections::HashSet<&str> = std::collections::HashSet::new();

    if dir.exists() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };

            let rel_path = path.strip_prefix(base).unwrap_or(&path);

            if should_ignore(rel_path, ignore_patterns) {
                continue;
            }

            if let Some((tree_name, _)) = tree_entries.get_key_value(name) {
                seen_entries.insert(*tree_name);
            }

            if path.is_symlink() {
                let target = fs::read_link(&path)?;
                let blob = Blob::new(crate::util::symlink_target_bytes(&target));
                let hash = blob.hash();
                match tree_entries.get(&name) {
                    Some(tree_entry) if tree_entry.entry_type == EntryType::Symlink => {
                        if hash != tree_entry.hash {
                            status.modified.push(rel_path.to_path_buf());
                        }
                    }
                    Some(_) => {
                        status.modified.push(rel_path.to_path_buf());
                    }
                    None => {
                        status.added.push(rel_path.to_path_buf());
                    }
                }
            } else {
                let metadata = path.metadata()?;

                if metadata.is_file() {
                    match tree_entries.get(&name) {
                        Some(tree_entry) if tree_entry.is_blob() => {
                            let content = fs::read(&path)?;
                            let blob = Blob::new(content);
                            let hash = blob.hash();

                            if hash != tree_entry.hash {
                                status.modified.push(rel_path.to_path_buf());
                            }
                        }
                        _ => {
                            status.added.push(rel_path.to_path_buf());
                        }
                    }
                } else if metadata.is_dir() {
                    let subtree = match tree_entries.get(&name) {
                        Some(tree_entry) if tree_entry.is_tree() => {
                            store.get_tree(&tree_entry.hash)?
                        }
                        _ => None,
                    };

                    compare_worktree_recursive(
                        store,
                        base,
                        &path,
                        subtree.as_ref(),
                        ignore_patterns,
                        status,
                    )?;
                }
            }
        }
    }

    for (name, entry) in &tree_entries {
        if !seen_entries.contains(name) {
            let rel_path = dir.strip_prefix(base).unwrap_or(dir).join(name);

            if entry.entry_type == EntryType::Blob {
                status.deleted.push(rel_path);
            } else if entry.entry_type == EntryType::Tree
                && let Some(subtree) = store.get_tree(&entry.hash)?
            {
                mark_all_deleted(store, &rel_path, &subtree, status)?;
            }
        }
    }

    Ok(())
}

fn mark_all_deleted<S: ObjectStore + ?Sized>(
    store: &S,
    prefix: &Path,
    tree: &Tree,
    status: &mut WorktreeStatus,
) -> Result<()> {
    for entry in tree.entries() {
        let path = prefix.join(&entry.name);

        match entry.entry_type {
            EntryType::Blob => {
                status.deleted.push(path);
            }
            EntryType::Tree => {
                if let Some(subtree) = store.get_tree(&entry.hash)? {
                    mark_all_deleted(store, &path, &subtree, status)?;
                }
            }
            EntryType::Symlink => {
                status.deleted.push(path);
            }
        }
    }
    Ok(())
}
