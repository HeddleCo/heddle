// SPDX-License-Identifier: Apache-2.0
use std::collections::HashSet;

use objects::{
    error::Result,
    object::tree_walk::{TreeIntegrityEvent, walk_tree_integrity},
    store::ObjectStore,
};
use repo::Repository;

use super::{FsckError, make_error};

pub(crate) fn check_tree_objects(
    repo: &Repository,
    errors: &mut Vec<FsckError>,
    warnings: &mut Vec<String>,
    objects_checked: &mut usize,
) -> Result<()> {
    let states = repo.store().list_states()?;
    let mut roots = Vec::with_capacity(states.len());
    for state_id in states {
        if let Some(state) = repo.store().get_state(&state_id)? {
            roots.push(state.tree);
        }
    }

    let mut blob_hashes = HashSet::new();

    walk_tree_integrity(repo.store(), roots, &mut |event| match event {
        TreeIntegrityEvent::EnterTree { .. } => {
            *objects_checked += 1;
            Ok(())
        }
        TreeIntegrityEvent::BlobLeaf { entry, .. } => {
            if let Some(hash) = entry.blob_hash() {
                if !repo.store().has_blob(&hash)? {
                    if repo.is_missing_blob(&hash)? {
                        warnings.push(format!(
                                "Tree entry '{}' references blob {} that is explicitly absent under partial fetch",
                                entry.name(),
                                hash.short()
                            ));
                    } else {
                        errors.push(make_error(
                            "missing_blob",
                            &format!("Tree entry '{}' references missing blob", entry.name()),
                            Some(hash.short()),
                        ));
                    }
                }
                blob_hashes.insert(hash);
            }
            Ok(())
        }
        TreeIntegrityEvent::TreeRef { .. } => Ok(()),
    })?;

    for blob_hash in blob_hashes {
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
