// SPDX-License-Identifier: Apache-2.0
//! Sync threads/markers functionality for Git bridge.

use gix::refs::transaction::PreviousValue;

use crate::bridge::{
    git_core::{GitBridge, GitBridgeError, GitResult, git_err, set_reference},
    git_import::thread_can_adopt_change,
};

/// Sync Heddle threads to Git branches.
pub fn sync_threads(bridge: &mut GitBridge) -> GitResult<usize> {
    let repo = bridge.open_git_repo()?;
    let mut stats = 0;

    let threads = bridge.heddle_repo.refs().list_threads()?;
    for track_name in threads {
        if let Some(state_id) = bridge.heddle_repo.refs().get_thread(&track_name)?
            && let Some(git_oid) = bridge.mapping.get_git(&state_id)
        {
            sync_track_to_branch(&repo, &track_name, git_oid)?;
            stats += 1;
        }
    }

    Ok(stats)
}

/// Sync Heddle markers to Git tags.
pub fn sync_markers(bridge: &mut GitBridge) -> GitResult<usize> {
    let repo = bridge.open_git_repo()?;
    let mut stats = 0;

    let markers = bridge.heddle_repo.refs().list_markers()?;
    for marker_name in markers {
        if let Some(state_id) = bridge.heddle_repo.refs().get_marker(&marker_name)?
            && let Some(git_oid) = bridge.mapping.get_git(&state_id)
        {
            sync_marker_to_tag(&repo, &marker_name, git_oid)?;
            stats += 1;
        }
    }

    Ok(stats)
}

/// Sync Git branches to Heddle threads.
pub fn sync_branches(bridge: &mut GitBridge) -> GitResult<usize> {
    let repo = bridge.open_git_repo()?;
    let mut stats = 0;

    for branch in repo
        .references()
        .map_err(git_err)?
        .local_branches()
        .map_err(git_err)?
    {
        let mut branch = branch.map_err(git_err)?;
        let name = branch.name().shorten().to_string();
        let target = branch.peel_to_id().map_err(git_err)?.detach();
        if let Some(change_id) = bridge.mapping.get_heddle(target) {
            if let Some(existing) = bridge.heddle_repo.refs().get_thread(&name)?
                && !thread_can_adopt_change(bridge.heddle_repo, &existing, &change_id)?
            {
                return Err(GitBridgeError::Conflict(format!(
                    "thread {} at {} differs from branch {} at {}. \
                     To recover, switch to '{}' and run `heddle sync` after \
                     resolving the divergent history, or explicitly reset the \
                     Heddle thread if the Git branch should replace it.",
                    name, existing, name, change_id, name
                )));
            }

            bridge.heddle_repo.refs().set_thread(&name, &change_id)?;
            stats += 1;
        }
    }

    Ok(stats)
}

/// Sync Git tags to Heddle markers.
pub fn sync_tags(bridge: &mut GitBridge) -> GitResult<usize> {
    let repo = bridge.open_git_repo()?;
    let mut stats = 0;

    for tag in repo
        .references()
        .map_err(git_err)?
        .tags()
        .map_err(git_err)?
    {
        let mut tag = tag.map_err(git_err)?;
        let name = tag.name().shorten().to_string();
        let oid = tag.peel_to_id().map_err(git_err)?.detach();

        if let Some(change_id) = bridge.mapping.get_heddle(oid) {
            match bridge.heddle_repo.refs().get_marker(&name) {
                Ok(Some(existing)) if existing != change_id => {
                    return Err(GitBridgeError::Conflict(format!(
                        "marker {} at {} differs from tag {} at {}",
                        name, existing, name, change_id
                    )));
                }
                Ok(_) => {}
                Err(err) => return Err(err.into()),
            }

            bridge.heddle_repo.refs().create_marker(&name, &change_id)?;
            stats += 1;
        }
    }

    Ok(stats)
}

/// Sync a Heddle thread to a Git branch.
pub fn sync_track_to_branch(
    repo: &gix::Repository,
    track_name: &str,
    git_oid: gix::hash::ObjectId,
) -> GitResult<()> {
    let branch_ref = format!("refs/heads/{}", track_name);

    if let Ok(mut branch) = repo.find_reference(&branch_ref) {
        let existing = branch.peel_to_id().map_err(git_err)?.detach();
        if existing != git_oid {
            set_reference(
                repo,
                &branch_ref,
                git_oid,
                PreviousValue::Any,
                "heddle: sync thread",
            )?;
        }
        return Ok(());
    }

    repo.reference(
        branch_ref,
        git_oid,
        PreviousValue::MustNotExist,
        "heddle: sync thread",
    )
    .map_err(git_err)?;
    Ok(())
}

/// Sync a Heddle marker to a Git tag.
pub fn sync_marker_to_tag(
    repo: &gix::Repository,
    marker_name: &str,
    git_oid: gix::hash::ObjectId,
) -> GitResult<()> {
    let tag_ref = format!("refs/tags/{}", marker_name);
    if let Ok(mut reference) = repo.find_reference(&tag_ref) {
        let existing = reference.peel_to_id().map_err(git_err)?.detach();
        if existing != git_oid {
            return Err(GitBridgeError::Conflict(format!(
                "tag {} at {} differs from marker {} at {}",
                marker_name, existing, marker_name, git_oid
            )));
        }
        return Ok(());
    }

    repo.tag_reference(marker_name, git_oid, PreviousValue::MustNotExist)
        .map_err(git_err)?;
    Ok(())
}