// SPDX-License-Identifier: Apache-2.0
//! Iterative validation of file-provenance trees.

use std::{collections::HashSet, path::PathBuf};

use objects::{
    error::Result,
    object::{ContentHash, EntryType, FileProvenance, Tree, TreeEntry},
    store::ObjectStore,
};
use repo::Repository;

use super::{FsckError, make_error};

pub(super) fn check_provenance_tree(
    repo: &Repository,
    data_tree: &Tree,
    provenance_root: ContentHash,
    errors: &mut Vec<FsckError>,
) -> Result<()> {
    let mut stack = vec![(data_tree.clone(), provenance_root, PathBuf::new())];
    let mut visited = HashSet::new();

    while let Some((data_tree, provenance_hash, path)) = stack.pop() {
        if !visited.insert((data_tree.hash(), provenance_hash)) {
            continue;
        }
        let Some(provenance_tree) = repo.store().get_tree(&provenance_hash)? else {
            errors.push(make_error(
                "invalid_provenance",
                &format!("Missing provenance tree for '{}'", path.display()),
                Some(provenance_hash.short()),
            ));
            continue;
        };

        for entry in provenance_tree.entries().iter().rev() {
            let entry_path = path.join(entry.name());
            let Some(data_entry) = data_tree.get(entry.name()) else {
                errors.push(make_error(
                    "invalid_provenance",
                    &format!(
                        "Provenance path '{}' does not exist in the data tree",
                        entry_path.display()
                    ),
                    None,
                ));
                continue;
            };
            match entry.entry_type() {
                EntryType::Tree => {
                    if !data_entry.is_tree() {
                        errors.push(make_error(
                            "invalid_provenance",
                            &format!(
                                "Provenance path '{}' points to a directory but data tree has a file",
                                entry_path.display()
                            ),
                            None,
                        ));
                        continue;
                    }
                    let (Some(data_hash), Some(child_provenance_hash)) =
                        (data_entry.tree_hash(), entry.tree_hash())
                    else {
                        continue;
                    };
                    if let Some(subtree) = repo.store().get_tree(&data_hash)? {
                        stack.push((subtree, child_provenance_hash, entry_path));
                    }
                }
                EntryType::Blob => {
                    check_provenance_blob(repo, data_entry, entry, &entry_path, errors)?
                }
                EntryType::Symlink | EntryType::Gitlink | EntryType::Spoollink => {}
            }
        }
    }
    Ok(())
}

fn check_provenance_blob(
    repo: &Repository,
    data_entry: &TreeEntry,
    provenance_entry: &TreeEntry,
    entry_path: &std::path::Path,
    errors: &mut Vec<FsckError>,
) -> Result<()> {
    if !data_entry.is_blob() {
        errors.push(make_error(
            "invalid_provenance",
            &format!(
                "Provenance path '{}' points to a file but data tree has a directory",
                entry_path.display()
            ),
            None,
        ));
        return Ok(());
    }
    let Some(provenance_hash) = provenance_entry.blob_hash() else {
        return Ok(());
    };
    let Some(provenance_blob) = repo.store().get_blob(&provenance_hash)? else {
        errors.push(make_error(
            "invalid_provenance",
            &format!("Missing provenance blob for '{}'", entry_path.display()),
            Some(provenance_hash.short()),
        ));
        return Ok(());
    };
    let provenance: FileProvenance = match rmp_serde::from_slice(provenance_blob.content()) {
        Ok(provenance) => provenance,
        Err(error) => {
            errors.push(make_error(
                "invalid_provenance",
                &format!(
                    "Invalid provenance blob for '{}': {}",
                    entry_path.display(),
                    error
                ),
                Some(provenance_hash.short()),
            ));
            return Ok(());
        }
    };
    if let Err(error) = provenance.validate() {
        errors.push(make_error(
            "invalid_provenance",
            &format!(
                "Invalid provenance spans for '{}': {}",
                entry_path.display(),
                error
            ),
            Some(provenance_hash.short()),
        ));
        return Ok(());
    }
    let Some(data_hash) = data_entry.blob_hash() else {
        return Ok(());
    };
    if provenance.file_blob != data_hash {
        errors.push(make_error(
            "invalid_provenance",
            &format!(
                "Provenance for '{}' points to blob {} but file uses {}",
                entry_path.display(),
                provenance.file_blob.short(),
                data_hash.short()
            ),
            Some(provenance_hash.short()),
        ));
        return Ok(());
    }
    if let Some(blob) = repo.store().get_blob(&data_hash)?
        && let Ok(text) = std::str::from_utf8(blob.content())
    {
        let line_count = text.lines().count() as u32;
        if provenance.line_count != line_count {
            errors.push(make_error(
                "invalid_provenance",
                &format!(
                    "Provenance for '{}' records {} lines but file has {}",
                    entry_path.display(),
                    provenance.line_count,
                    line_count
                ),
                Some(provenance_hash.short()),
            ));
        }
    }
    Ok(())
}
