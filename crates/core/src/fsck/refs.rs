// SPDX-License-Identifier: Apache-2.0
use objects::{error::Result, object::ContentHash, store::ObjectStore};
use repo::Repository;

use super::{FsckError, make_error};

pub(crate) fn check_refs(
    repo: &Repository,
    errors: &mut Vec<FsckError>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let threads = repo.refs().list_threads()?;
    for thread in threads {
        if let Some(state_id) = repo.refs().get_thread(&thread)?
            && !repo.store().has_state(&state_id)?
        {
            errors.push(make_error(
                "dangling_ref",
                &format!("Thread '{}' points to non-existent state", thread),
                Some(state_id.short()),
            ));
        }
    }

    let markers = repo.refs().list_markers()?;
    for marker in markers {
        if let Some(state_id) = repo.refs().get_marker(&marker)?
            && !repo.store().has_state(&state_id)?
        {
            warnings.push(format!(
                "Marker '{}' points to non-existent state {}",
                marker,
                state_id.short()
            ));
        }
    }

    if let Some(state_id) = repo.head()?
        && !repo.store().has_state(&state_id)?
    {
        errors.push(make_error(
            "dangling_head",
            "HEAD points to non-existent state",
            Some(state_id.short()),
        ));
    }

    Ok(())
}

pub(crate) fn check_merge_state(repo: &Repository, warnings: &mut Vec<String>) -> Result<()> {
    let merge_manager = repo.merge_state_manager();
    if merge_manager.is_merge_in_progress() {
        warnings.push(
            "Merge in progress - resolve conflicts or use 'heddle resolve --abort'".to_string(),
        );
    }

    let stash_manager = repo.stash_manager();
    let stashes = stash_manager.list()?;
    for stash in &stashes {
        let tree_hash = ContentHash::from_hex(&stash.tree_hash);
        if tree_hash.is_err() || !repo.store().has_tree(&tree_hash.unwrap())? {
            warnings.push(format!("Stash {} has invalid tree reference", stash.index));
        }
    }

    Ok(())
}
