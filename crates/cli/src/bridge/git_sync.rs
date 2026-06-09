// SPDX-License-Identifier: Apache-2.0
//! Sync threads/markers functionality for Git bridge.

use git_substrate::{GitRepo, RefConstraint};
use objects::object::{MarkerName, ThreadName};
use refs::RefExpectation;

use crate::bridge::{
    git_core::{git_err, GitBridge, GitBridgeError, GitResult},
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
            sync_track_to_branch_repo(&repo, &track_name, git_oid)?;
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
            sync_marker_to_tag_repo(&repo, &marker_name, git_oid)?;
            stats += 1;
        }
    }

    Ok(stats)
}

/// Sync Git branches to Heddle threads.
pub fn sync_branches(bridge: &mut GitBridge) -> GitResult<usize> {
    let repo = bridge.open_git_repo()?;
    let mut stats = 0;

    for name in repo.local_branch_names().map_err(git_err)? {
        let branch_ref = format!("refs/heads/{name}");
        let target = match repo.peel_reference_to_commit(&branch_ref).map_err(git_err)? {
            Ok(oid) => oid,
            Err(_) => continue,
        };
        if let Some(change_id) = bridge.mapping.get_heddle(&target) {
            let tn = ThreadName::new(&name);
            if let Some(existing) = bridge.heddle_repo.refs().get_thread(&tn)?
                && !thread_can_adopt_change(bridge.heddle_repo, &existing, &change_id)?
            {
                return Err(GitBridgeError::GitHeddleThreadDiverged {
                    thread: name.clone(),
                    branch: name,
                    thread_change: existing,
                    branch_change: change_id,
                });
            }

            bridge.heddle_repo.refs().set_thread(&tn, &change_id)?;
            stats += 1;
        }
    }

    Ok(stats)
}

/// Sync Git tags to Heddle markers.
pub fn sync_tags(bridge: &mut GitBridge) -> GitResult<usize> {
    let repo = bridge.open_git_repo()?;
    let mut stats = 0;

    for name in repo.local_tag_names().map_err(git_err)? {
        let tag_ref = format!("refs/tags/{name}");
        let oid = match repo.peel_reference_to_commit(&tag_ref).map_err(git_err)? {
            Ok(oid) => oid,
            Err(_) => continue,
        };

        if let Some(change_id) = bridge.mapping.get_heddle(&oid) {
            let mn = MarkerName::new(&name);
            match bridge.heddle_repo.refs().get_marker(&mn) {
                Ok(Some(existing)) if existing != change_id => bridge
                    .heddle_repo
                    .refs()
                    .set_marker_cas(&mn, RefExpectation::Any, &change_id)?,
                Ok(_) => {}
                Err(err) => return Err(err.into()),
            }

            if bridge.heddle_repo.refs().get_marker(&mn)?.is_none() {
                bridge.heddle_repo.refs().create_marker(&mn, &change_id)?;
            }
            stats += 1;
        }
    }

    Ok(stats)
}

/// Sync a Heddle thread to a Git branch.
pub fn sync_track_to_branch(
    repo: &GitRepo,
    track_name: &str,
    git_oid: crate::bridge::git_core::ObjectId,
) -> GitResult<()> {
    sync_track_to_branch_repo(repo, track_name, git_oid)
}

pub(crate) fn sync_track_to_branch_repo(
    repo: &GitRepo,
    track_name: &str,
    git_oid: crate::bridge::git_core::ObjectId,
) -> GitResult<()> {
    let branch_ref = format!("refs/heads/{track_name}");

    if let Some(existing) = repo.read_ref_oid(&branch_ref).map_err(git_err)? {
        if existing != git_oid {
            super::git_core::ensure_commit_update_fast_forward(repo, &branch_ref, &existing, &git_oid)?;
            git_substrate::set_reference(
                repo.git_dir(),
                repo.object_format(),
                &branch_ref,
                &git_oid,
                RefConstraint::MustExistAndMatch(existing),
                "heddle: sync thread",
            )
            .map_err(git_err)?;
        }
        return Ok(());
    }

    git_substrate::set_reference(
        repo.git_dir(),
        repo.object_format(),
        &branch_ref,
        &git_oid,
        RefConstraint::MustNotExist,
        "heddle: sync thread",
    )
    .map_err(git_err)?;
    Ok(())
}

/// Sync a Heddle marker to a Git tag.
pub fn sync_marker_to_tag(
    repo: &GitRepo,
    marker_name: &str,
    git_oid: crate::bridge::git_core::ObjectId,
) -> GitResult<()> {
    sync_marker_to_tag_repo(repo, marker_name, git_oid)
}

pub(crate) fn sync_marker_to_tag_repo(
    repo: &GitRepo,
    marker_name: &str,
    git_oid: crate::bridge::git_core::ObjectId,
) -> GitResult<()> {
    let tag_ref = format!("refs/tags/{marker_name}");
    if let Some(existing) = repo.read_ref_oid(&tag_ref).map_err(git_err)? {
        // Markers map to commit OIDs, but an existing annotated tag ref points at
        // the tag object. Peel before comparing so import-preserved annotated
        // tags survive export sync unchanged.
        let needs_update = if existing == git_oid {
            false
        } else if let Ok(Ok(peeled_commit)) = repo.peel_reference_to_commit(&tag_ref).map_err(git_err)
        {
            peeled_commit != git_oid
        } else {
            true
        };
        if needs_update {
            // A marker is a free-move ref (`classify_tag_move`): a legitimate
            // RETARGET to a new served+minted OID must FORCE-set the mirror tag,
            // not abort the whole export with a conflict (heddle#316 S1). The
            // mirror is heddle-owned, so there is no out-of-band tip to spare
            // here; the destination-side ownership gate (`classify_tag_move`,
            // `recorded == old`) still spares an out-of-band DESTINATION tag.
            git_substrate::set_reference(
                repo.git_dir(),
                repo.object_format(),
                &tag_ref,
                &git_oid,
                RefConstraint::Any,
                "heddle: sync marker",
            )
            .map_err(git_err)?;
        }
        return Ok(());
    }

    git_substrate::set_reference(
        repo.git_dir(),
        repo.object_format(),
        &tag_ref,
        &git_oid,
        RefConstraint::MustNotExist,
        "heddle: sync marker",
    )
    .map_err(git_err)?;
    Ok(())
}

