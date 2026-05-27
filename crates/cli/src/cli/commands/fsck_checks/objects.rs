// SPDX-License-Identifier: Apache-2.0
use std::collections::HashSet;

use anyhow::Result;
use objects::object::{ContentHash, Tree};
use repo::Repository;

use super::{FsckError, make_error};

pub(crate) fn check_trees(
    repo: &Repository,
    errors: &mut Vec<FsckError>,
    warnings: &mut Vec<String>,
    objects_checked: &mut usize,
) -> Result<()> {
    let states = repo.store().list_states()?;
    let mut checked_trees: HashSet<ContentHash> = HashSet::new();

    for state_id in states {
        if let Some(state) = repo.store().get_state(&state_id)? {
            check_tree_recursive(
                repo,
                &state.tree,
                &mut checked_trees,
                errors,
                warnings,
                objects_checked,
            )?;
        }
    }

    Ok(())
}

fn check_tree_recursive(
    repo: &Repository,
    tree_hash: &ContentHash,
    checked: &mut HashSet<ContentHash>,
    errors: &mut Vec<FsckError>,
    warnings: &mut Vec<String>,
    objects_checked: &mut usize,
) -> Result<()> {
    if checked.contains(tree_hash) {
        return Ok(());
    }
    checked.insert(*tree_hash);

    let Some(tree) = repo.store().get_tree(tree_hash)? else {
        return Ok(());
    };

    *objects_checked += 1;

    for entry in tree.entries() {
        if entry.is_blob() {
            if !repo.store().has_blob(&entry.hash)? {
                if repo.is_missing_blob(&entry.hash)? {
                    warnings.push(format!(
                        "Tree entry '{}' references blob {} that is explicitly absent under partial fetch",
                        entry.name,
                        entry.hash.short()
                    ));
                } else {
                    errors.push(make_error(
                        "missing_blob",
                        &format!("Tree entry '{}' references missing blob", entry.name),
                        Some(entry.hash.short()),
                    ));
                }
            }
        } else if entry.is_tree() {
            check_tree_recursive(
                repo,
                &entry.hash,
                checked,
                errors,
                warnings,
                objects_checked,
            )?;
        }
    }

    Ok(())
}

pub(crate) fn check_blobs(
    repo: &Repository,
    errors: &mut Vec<FsckError>,
    _warnings: &mut Vec<String>,
    objects_checked: &mut usize,
) -> Result<()> {
    let states = repo.store().list_states()?;
    let mut checked_blobs: HashSet<ContentHash> = HashSet::new();

    for state_id in states {
        if let Some(state) = repo.store().get_state(&state_id)?
            && let Some(tree) = repo.store().get_tree(&state.tree)?
        {
            collect_blobs_from_tree(repo, &tree, &mut checked_blobs)?;
        }
    }

    for blob_hash in checked_blobs {
        *objects_checked += 1;
        if let Some(blob) = repo.store().get_blob(&blob_hash)? {
            let computed_hash = blob.hash();
            if computed_hash != blob_hash {
                errors.push(make_error(
                    "hash_mismatch",
                    "Blob content hash does not match stored hash",
                    Some(blob_hash.short()),
                ));
            }
        } else if repo.is_missing_blob(&blob_hash)? {
            continue;
        } else {
            errors.push(make_error(
                "missing_blob",
                "Tree references missing blob",
                Some(blob_hash.short()),
            ));
        }
    }

    Ok(())
}

fn collect_blobs_from_tree(
    repo: &Repository,
    tree: &Tree,
    blobs: &mut HashSet<ContentHash>,
) -> Result<()> {
    for entry in tree.entries() {
        if entry.is_blob() {
            blobs.insert(entry.hash);
        } else if entry.is_tree()
            && let Some(subtree) = repo.store().get_tree(&entry.hash)?
        {
            collect_blobs_from_tree(repo, &subtree, blobs)?;
        }
    }
    Ok(())
}
