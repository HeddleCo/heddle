// SPDX-License-Identifier: Apache-2.0
//! Sync threads/markers for Git Projection Mapping.

use objects::object::{ChangeId, MarkerName, ThreadName};
use refs::RefExpectation;
use sley::{
    ObjectId as SleyObjectId, RefPrecondition, ReferenceTarget, Repository as SleyRepository,
};

use crate::git_projection_engine::git_core::{
    GitProjection, GitProjectionError, GitProjectionResult, git_err, thread_is_unclaimed_bootstrap,
};

/// Sync Heddle threads to Git branches.
pub fn sync_threads(bridge: &mut GitProjection) -> GitProjectionResult<usize> {
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
pub fn sync_markers(bridge: &mut GitProjection) -> GitProjectionResult<usize> {
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
pub fn sync_branches(bridge: &mut GitProjection) -> GitProjectionResult<usize> {
    let repo = bridge.open_git_repo()?;
    let mut stats = 0;

    for reference in repo.references().list_refs().map_err(git_err)? {
        let Some(name) = reference.name.strip_prefix("refs/heads/") else {
            continue;
        };
        let Some(target) = peeled_oid(&repo, &reference.name, &reference.target)? else {
            continue;
        };
        if let Some(change_id) = bridge.mapping.get_heddle(target) {
            let tn = ThreadName::new(name);
            if let Some(existing) = bridge.heddle_repo.refs().get_thread(&tn)?
                && !thread_can_adopt_change(bridge, &existing, &change_id)?
            {
                return Err(GitProjectionError::GitHeddleThreadDiverged {
                    thread: name.to_string(),
                    branch: name.to_string(),
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

fn thread_can_adopt_change(
    bridge: &GitProjection<'_>,
    existing: &ChangeId,
    change_id: &ChangeId,
) -> GitProjectionResult<bool> {
    if existing == change_id {
        return Ok(true);
    }
    if thread_is_unclaimed_bootstrap(bridge.heddle_repo, existing)? {
        return Ok(true);
    }
    wire::is_ancestor(bridge.heddle_repo.store(), *existing, *change_id)
        .map_err(|err| GitProjectionError::InvalidMapping(err.to_string()))
}

/// Sync Git tags to Heddle markers.
pub fn sync_tags(bridge: &mut GitProjection) -> GitProjectionResult<usize> {
    let repo = bridge.open_git_repo()?;
    let mut stats = 0;

    for reference in repo.references().list_refs().map_err(git_err)? {
        let Some(name) = reference.name.strip_prefix("refs/tags/") else {
            continue;
        };
        let Some(oid) = peeled_oid(&repo, &reference.name, &reference.target)? else {
            continue;
        };

        if let Some(change_id) = bridge.mapping.get_heddle(oid) {
            let mn = MarkerName::new(name);
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
    repo: &SleyRepository,
    track_name: &str,
    git_oid: SleyObjectId,
) -> GitProjectionResult<()> {
    let branch_ref = format!("refs/heads/{}", track_name);

    if let Some(branch) = repo.find_reference(&branch_ref).map_err(git_err)? {
        let existing = branch.peeled_oid(repo).map_err(git_err)?;
        let Some(existing) = existing else {
            return set_ref(
                repo,
                &branch_ref,
                git_oid,
                RefPrecondition::Any,
                "heddle: sync thread",
            );
        };
        if existing != git_oid {
            ensure_commit_update_fast_forward(repo, &branch_ref, existing, git_oid)?;
            set_ref(
                repo,
                &branch_ref,
                git_oid,
                RefPrecondition::MustExistAndMatch(ReferenceTarget::Direct(existing)),
                "heddle: sync thread",
            )?;
        }
        return Ok(());
    }

    set_ref(
        repo,
        &branch_ref,
        git_oid,
        RefPrecondition::MustNotExist,
        "heddle: sync thread",
    )
}

/// Sync a Heddle marker to a Git tag.
pub fn sync_marker_to_tag(
    repo: &SleyRepository,
    marker_name: &str,
    git_oid: SleyObjectId,
) -> GitProjectionResult<()> {
    let tag_ref = format!("refs/tags/{}", marker_name);
    if let Some(reference) = repo.find_reference(&tag_ref).map_err(git_err)? {
        let existing = peeled_oid(repo, &tag_ref, &reference.target)?;
        let Some(existing) = existing else {
            return set_ref(
                repo,
                &tag_ref,
                git_oid,
                RefPrecondition::Any,
                "heddle: sync marker",
            );
        };
        if existing != git_oid {
            // A marker is a free-move ref (`classify_tag_move`): a legitimate
            // RETARGET to a new served+minted OID must FORCE-set the mirror tag,
            // not abort the whole export with a conflict (heddle#316 S1). The
            // mirror is heddle-owned, so there is no out-of-band tip to spare
            // here; the destination-side ownership gate (`classify_tag_move`,
            // `recorded == old`) still spares an out-of-band DESTINATION tag.
            set_ref(
                repo,
                &tag_ref,
                git_oid,
                RefPrecondition::Any,
                "heddle: sync marker",
            )?;
        }
        return Ok(());
    }

    set_ref(
        repo,
        &tag_ref,
        git_oid,
        RefPrecondition::MustNotExist,
        "heddle: sync marker",
    )
}

fn set_ref(
    repo: &SleyRepository,
    name: &str,
    oid: SleyObjectId,
    precondition: RefPrecondition,
    message: &str,
) -> GitProjectionResult<()> {
    let old_oid = match &precondition {
        RefPrecondition::MustExistAndMatch(ReferenceTarget::Direct(oid))
        | RefPrecondition::ExistingMustMatch(ReferenceTarget::Direct(oid)) => *oid,
        _ => SleyObjectId::null(repo.object_format()),
    };
    let refs = repo.references();
    let mut tx = refs.transaction();
    tx.update_to(
        name,
        ReferenceTarget::Direct(oid),
        precondition,
        Some(sley::plumbing::sley_refs::ReflogEntry {
            old_oid,
            new_oid: oid,
            committer: git_projection_identity(),
            message: message.as_bytes().to_vec(),
        }),
    );
    tx.commit().map_err(git_err)
}

fn ensure_commit_update_fast_forward(
    repo: &SleyRepository,
    ref_name: &str,
    old: SleyObjectId,
    new: SleyObjectId,
) -> GitProjectionResult<()> {
    if sley::plumbing::sley_rev::is_ancestor(
        repo.git_dir(),
        repo.object_format(),
        repo.objects().as_ref(),
        &old,
        &new,
    )
    .map_err(git_err)?
    {
        Ok(())
    } else {
        Err(GitProjectionError::NonFastForwardRef {
            name: ref_name.to_string(),
            old,
            new,
            remote_destination: false,
        })
    }
}

fn peeled_oid(
    repo: &SleyRepository,
    name: &str,
    target: &ReferenceTarget,
) -> GitProjectionResult<Option<SleyObjectId>> {
    let Some(oid) = (match target {
        ReferenceTarget::Direct(oid) => Ok(Some(*oid)),
        ReferenceTarget::Symbolic(_) => {
            let Some(reference) = repo.find_reference(name).map_err(git_err)? else {
                return Ok(None);
            };
            reference.peeled_oid(repo).map_err(git_err)
        }
    })?
    else {
        return Ok(None);
    };
    match sley::plumbing::sley_rev::peel_to_commit(
        repo.objects().as_ref(),
        repo.object_format(),
        &oid,
    ) {
        Ok(commit_oid) => Ok(Some(commit_oid)),
        Err(_) => Ok(None),
    }
}

fn git_projection_identity() -> Vec<u8> {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format!("Heddle <heddle@local> {seconds} +0000").into_bytes()
}
