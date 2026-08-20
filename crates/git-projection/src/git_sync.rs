// SPDX-License-Identifier: Apache-2.0
//! Sync threads/markers for Git Projection Mapping.

use objects::object::{MarkerName, StateId, ThreadName};
use refs::RefExpectation;
use sley::{
    ObjectId as SleyObjectId, RefPrecondition, ReferenceTarget, Repository as SleyRepository,
};

use crate::{
    git_core::{
        GitProjection, GitProjectionError, GitProjectionResult, git_err,
        thread_is_unclaimed_bootstrap,
    },
    git_util::FailedRefExportReason,
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
        if let Some(state_id) = bridge.mapping.get_heddle(target) {
            if crate::git_frontier::is_reserved_heddle_git_ref(&reference.name)
                || objects::object::is_reserved_heddle_namespace(name)
            {
                return Err(GitProjectionError::InvalidMapping(format!(
                    "refused to import reserved heddle/ Git branch '{name}'"
                )));
            }
            let tn = ThreadName::try_new(name)
                .map_err(|error| GitProjectionError::InvalidMapping(error.to_string()))?;
            if let Some(existing) = bridge.heddle_repo.refs().get_thread(&tn)?
                && !thread_can_adopt_change(bridge, &existing, &state_id)?
            {
                return Err(GitProjectionError::GitHeddleThreadDiverged {
                    thread: name.to_string(),
                    branch: name.to_string(),
                    thread_change: existing,
                    branch_change: state_id,
                });
            }

            bridge.heddle_repo.set_thread_recorded(&tn, &state_id)?;
            stats += 1;
        }
    }

    Ok(stats)
}

fn thread_can_adopt_change(
    bridge: &GitProjection<'_>,
    existing: &StateId,
    state_id: &StateId,
) -> GitProjectionResult<bool> {
    if existing == state_id {
        return Ok(true);
    }
    if thread_is_unclaimed_bootstrap(bridge.heddle_repo, existing)? {
        return Ok(true);
    }
    wire::is_ancestor(bridge.heddle_repo.store(), *existing, *state_id)
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

        if let Some(state_id) = bridge.mapping.get_heddle(oid) {
            if crate::git_frontier::is_reserved_heddle_git_ref(&reference.name)
                || objects::object::is_reserved_heddle_namespace(name)
            {
                return Err(GitProjectionError::InvalidMapping(format!(
                    "refused to import reserved heddle/ Git tag '{name}'"
                )));
            }
            let mn = MarkerName::try_new(name)
                .map_err(|error| GitProjectionError::InvalidMapping(error.to_string()))?;
            match bridge.heddle_repo.refs().get_marker(&mn) {
                Ok(Some(existing)) if existing != state_id => bridge
                    .heddle_repo
                    .set_marker_recorded_cas(&mn, RefExpectation::Any, &state_id)?,
                Ok(_) => {}
                Err(err) => return Err(err.into()),
            }

            if bridge.heddle_repo.refs().get_marker(&mn)?.is_none() {
                bridge.heddle_repo.create_marker_recorded(&mn, &state_id)?;
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

/// Force-rewind a Heddle-owned mirror branch to an embargo-lagged frontier.
///
/// The force bypasses the fast-forward guard, but never the ref transaction's
/// compare-and-swap: `expected_old` is the exact tip Heddle's managed-ref record
/// says it last published. A Git-side writer that moves the ref after reconcile
/// classification loses this CAS and is reported through [`set_ref`]'s
/// [`FailedRefExportReason`] classifier instead of being overwritten.
pub(crate) fn force_rewind_track_to_branch(
    repo: &SleyRepository,
    track_name: &str,
    expected_old: SleyObjectId,
    git_oid: SleyObjectId,
) -> GitProjectionResult<()> {
    let branch_ref = format!("refs/heads/{track_name}");
    set_ref(
        repo,
        &branch_ref,
        git_oid,
        RefPrecondition::MustExistAndMatch(ReferenceTarget::Direct(expected_old)),
        "heddle: retract embargoed thread frontier",
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

pub(crate) fn set_ref(
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
        precondition.clone(),
        Some(sley::plumbing::sley_refs::ReflogEntry {
            old_oid,
            new_oid: oid,
            committer: git_projection_identity(),
            message: message.as_bytes().to_vec(),
        }),
    );
    tx.commit()
        .map_err(|err| GitProjectionError::RefExportFailed {
            name: name.to_string(),
            reason: classify_failed_ref_write(repo, name, &precondition, oid, git_err(err)),
        })
}

/// Classify a failed ref write into the export's [`FailedRefExportReason`]
/// vocabulary by RE-READING the ref and testing what it holds against the
/// precondition heddle asserted.
///
/// Classification is driven by observed state, never by the error text, so
/// rewording sley's transaction message cannot silently reclassify a race. If
/// the ref now contradicts `precondition`, another writer moved it and the
/// compare-and-swap is exactly what stopped heddle from clobbering them. If it
/// does NOT contradict, the write failed for some unrelated reason and is
/// reported as [`FailedRefExportReason::FailedToSet`] rather than blamed on a
/// race we cannot demonstrate.
///
/// WINDOW COVERED. sley re-verifies the precondition *while holding the ref
/// lock* ("a true CAS, not a check-then-write" -- `RefPrecondition`), so any
/// Git-side write that lands before heddle's transaction commits loses the swap
/// and arrives here. That is the entire read-then-write window of this export,
/// including the gap between `sync_track_to_branch`'s `find_reference` and its
/// `set_ref`.
///
/// WINDOW NOT COVERED. A Git-side write that lands AFTER heddle's transaction
/// commits: heddle's value is already in place and the later writer overwrites
/// it. Nothing here detects that, and nothing could -- it is not a lost swap,
/// it is a subsequent one. Nor is a `RefPrecondition::Any` write covered at all:
/// it asserts nothing, so it cannot lose a race by construction.
///
/// The re-read is DIAGNOSTIC only. It reports what Git holds a moment after the
/// failure, which a fast third writer may already have changed again from the
/// value that actually cost heddle the swap. `found` is therefore a faithful
/// "what is there now", not a proof of "what beat us".
fn classify_failed_ref_write(
    repo: &SleyRepository,
    name: &str,
    precondition: &RefPrecondition,
    intended: SleyObjectId,
    err: GitProjectionError,
) -> FailedRefExportReason {
    // The ref's RAW target, not its peeled commit: the precondition heddle
    // asserted was against the raw value, so peeling here could call an
    // annotated tag "unchanged" when its wrapper object was in fact replaced.
    let current = match repo.find_reference(name) {
        Ok(reference) => reference.map(|reference| reference.target),
        // The ref store cannot be read back, so nothing can be asserted about
        // another writer. Report the original failure, not a guess.
        Err(_) => return FailedRefExportReason::FailedToSet(err.to_string()),
    };

    match (precondition, current) {
        // heddle required a specific existing value and the ref is now gone.
        // Only `MustExistAndMatch` can conclude this: `ExistingMustMatch` is
        // satisfied by absence, so a missing ref there is not a lost swap.
        (RefPrecondition::MustExistAndMatch(_), None) => {
            FailedRefExportReason::DeletedInGit { intended }
        }
        // heddle required a specific existing value and Git holds another one.
        (
            RefPrecondition::MustExistAndMatch(expected)
            | RefPrecondition::ExistingMustMatch(expected),
            Some(ReferenceTarget::Direct(found)),
        ) if *expected != ReferenceTarget::Direct(found) => {
            FailedRefExportReason::ModifiedConcurrentlyInGit { found, intended }
        }
        // heddle required the ref to be absent (a create) and Git has one now:
        // another writer created it under the same name.
        (RefPrecondition::MustNotExist, Some(ReferenceTarget::Direct(found))) => {
            FailedRefExportReason::ModifiedConcurrentlyInGit { found, intended }
        }
        // What is on disk is CONSISTENT with what heddle asserted (or is a
        // symref, which heddle never asserts against), so the failure is not
        // attributable to another writer -- a locked ref, an IO error, an
        // invalid name. Reporting it as a race would be a claim we cannot back.
        _ => FailedRefExportReason::FailedToSet(err.to_string()),
    }
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

#[cfg(test)]
mod tests {
    use sley::{
        CommitObject, GitObjectType, GitTime, Signature, TreeEditor,
        plumbing::{sley_core::ByteString, sley_object::EncodedObject},
    };
    use tempfile::TempDir;

    use super::*;

    fn test_signature() -> Signature {
        let time = GitTime::new(0, 0);
        let raw = format!("Heddle Test <heddle@test> 0 {}", time.offset_token()).into_bytes();
        Signature {
            name: ByteString::new(b"Heddle Test".to_vec()),
            email: ByteString::new(b"heddle@test".to_vec()),
            time,
            raw,
        }
    }

    /// A bare repo plus two unrelated commits, standing in for "heddle's tip"
    /// and "the tip some other writer pushed".
    fn repo_with_two_commits() -> (TempDir, SleyRepository, SleyObjectId, SleyObjectId) {
        let temp = TempDir::new().expect("temp dir");
        let repo = SleyRepository::init_bare(temp.path()).expect("init bare");
        let tree = repo.write_tree(TreeEditor::new()).expect("empty tree");
        let sig = test_signature();
        let commit = |message: &str| {
            let object = CommitObject {
                tree,
                parents: Vec::new(),
                author: sig.to_ident_bytes(),
                committer: sig.to_ident_bytes(),
                encoding: None,
                message: message.as_bytes().to_vec(),
            };
            repo.write_object(EncodedObject::new(GitObjectType::Commit, object.write()))
                .expect("write commit")
        };
        let ours = commit("ours");
        let theirs = commit("theirs");
        (temp, repo, ours, theirs)
    }

    /// A LOST compare-and-swap -- the on-disk ref holds something other than
    /// what heddle asserted -- must classify as `ModifiedConcurrentlyInGit` and
    /// NAME the value Git actually holds. A generic `FailedToSet` here would
    /// tell an operator nothing about what moved underneath them.
    #[test]
    fn lost_cas_against_a_moved_ref_reports_modified_concurrently_in_git() {
        let (_temp, repo, ours, theirs) = repo_with_two_commits();
        // The ref really holds `theirs`: exactly the state a concurrent
        // Git-side branch update leaves behind.
        set_ref(
            &repo,
            "refs/heads/raced",
            theirs,
            RefPrecondition::MustNotExist,
            "test: concurrent git-side writer",
        )
        .expect("seed ref");

        let reason = classify_failed_ref_write(
            &repo,
            "refs/heads/raced",
            // What heddle read before it decided to write.
            &RefPrecondition::MustExistAndMatch(ReferenceTarget::Direct(ours)),
            ours,
            GitProjectionError::Git("transaction failed: expected ref to match".into()),
        );

        assert_eq!(
            reason,
            FailedRefExportReason::ModifiedConcurrentlyInGit {
                found: theirs,
                intended: ours,
            },
            "a ref holding a value other than the asserted one is a lost race"
        );
    }

    /// The ref VANISHING under a `MustExistAndMatch` assertion is a delete on
    /// the Git side, not a modification. The operator needs the difference to
    /// know whether to re-import or re-create the branch.
    #[test]
    fn lost_cas_against_a_deleted_ref_reports_deleted_in_git() {
        let (_temp, repo, ours, _theirs) = repo_with_two_commits();

        let reason = classify_failed_ref_write(
            &repo,
            "refs/heads/gone",
            &RefPrecondition::MustExistAndMatch(ReferenceTarget::Direct(ours)),
            ours,
            GitProjectionError::Git("transaction failed: expected ref to match".into()),
        );

        assert_eq!(
            reason,
            FailedRefExportReason::DeletedInGit { intended: ours },
            "an absent ref under a must-exist assertion was deleted in git"
        );
    }

    /// A failure whose observed state is CONSISTENT with what heddle asserted
    /// is not attributable to another writer, so it must stay generic. Without
    /// this, every locked-ref or IO failure would be misreported as a race.
    #[test]
    fn failure_with_a_satisfied_precondition_stays_generic() {
        let (_temp, repo, ours, _theirs) = repo_with_two_commits();
        set_ref(
            &repo,
            "refs/heads/intact",
            ours,
            RefPrecondition::MustNotExist,
            "test: seed",
        )
        .expect("seed ref");

        let reason = classify_failed_ref_write(
            &repo,
            "refs/heads/intact",
            &RefPrecondition::MustExistAndMatch(ReferenceTarget::Direct(ours)),
            ours,
            GitProjectionError::Git("could not lock ref".into()),
        );

        assert!(
            matches!(reason, FailedRefExportReason::FailedToSet(_)),
            "an unmoved ref is not evidence of a race, got {reason:?}"
        );
    }
}
