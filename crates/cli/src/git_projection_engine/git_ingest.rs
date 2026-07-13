// SPDX-License-Identifier: Apache-2.0
//! Ingest-backed Git history import for bridge-facing commands.

use std::{collections::HashSet, path::Path};

use objects::{object::StateId, store::ObjectStore};
use sley::{GitObjectType, ObjectId, Repository as SleyRepository};

use super::{
    git_core::{
        GitProjection, GitProjectionError, GitProjectionResult, collect_import_source_ref_updates,
        open_repo,
    },
    git_notes,
    git_util::ImportStats,
};

pub(crate) fn import_git_history(
    bridge: &mut GitProjection<'_>,
    git_path: Option<&Path>,
    refs: &[String],
    options: ingest::ImportOptions,
    progress: Option<&mut dyn FnMut(ingest::ImportProgressEvent)>,
) -> GitProjectionResult<ImportStats> {
    let source = git_path.unwrap_or_else(|| bridge.heddle_repo.root());
    reject_shallow_source(source, refs)?;
    let scope = if refs.is_empty() {
        ingest::ImportScope::all()
    } else {
        ingest::ImportScope::refs(refs.to_vec())
    };
    let (stats, _map) = ingest::import_git_into_scoped_with_options_and_progress(
        source,
        bridge.heddle_repo.root(),
        options,
        scope,
        progress,
    )
    .map_err(map_ingest_error)?;
    bridge.stage_ingest_source_in_mirror(source, refs)?;
    if refs.is_empty() {
        bridge.build_existing_mapping(Some(source))?;
    } else {
        bridge.build_existing_mapping(None)?;
    }
    let mirror_repo = bridge.open_git_repo()?;
    bridge.seed_ingest_identity_mappings_from_mirror(&mirror_repo)?;
    backfill_ingest_identity_notes_in_mirror(bridge, &mirror_repo, refs)?;
    Ok(import_stats_from_ingest(stats))
}

fn map_ingest_error(error: ingest::IngestError) -> GitProjectionError {
    match error {
        ingest::IngestError::ThreadDiverged {
            thread,
            branch,
            existing,
            incoming,
        } => GitProjectionError::GitHeddleThreadDiverged {
            thread,
            branch,
            thread_change: existing,
            branch_change: incoming,
        },
        other => GitProjectionError::Git(other.to_string()),
    }
}

fn reject_shallow_source(source: &Path, refs: &[String]) -> GitProjectionResult<()> {
    let repo = open_repo(source)?;
    if repo.git_dir().join("shallow").is_file() {
        let wanted = (!refs.is_empty()).then(|| refs.iter().cloned().collect::<HashSet<_>>());
        return Err(GitProjectionError::ShallowClone {
            repository: repo
                .workdir()
                .unwrap_or_else(|| repo.git_dir().to_path_buf()),
            retry_command: shallow_import_retry_command(wanted.as_ref()),
        });
    }
    Ok(())
}

fn shallow_import_retry_command(wanted_refs: Option<&HashSet<String>>) -> String {
    match wanted_refs.and_then(|refs| refs.iter().next()) {
        Some(_) => "heddle import git --path <full-git-repo> --ref <ref>".to_string(),
        None => "heddle import git --path <full-git-repo>".to_string(),
    }
}

fn import_stats_from_ingest(stats: ingest::ImportStats) -> ImportStats {
    ImportStats {
        commits_imported: stats.commits_imported,
        states_created: stats.states_created,
        branches_synced: stats.refs.threads_written,
        tags_synced: stats.refs.markers_written,
        skipped_non_commit_refs: stats.refs_seen.non_commit_skipped,
        lossy_entries: stats.lossy_entries,
    }
}

fn backfill_ingest_identity_notes_in_mirror(
    bridge: &GitProjection<'_>,
    mirror_repo: &SleyRepository,
    refs: &[String],
) -> GitProjectionResult<()> {
    let scoped_commits = if refs.is_empty() {
        None
    } else {
        let updates = collect_import_source_ref_updates(mirror_repo, refs)?;
        Some(reachable_commits_from_updates(mirror_repo, updates)?)
    };

    for (git_sha, state_id) in bridge.heddle_repo.git_overlay_ingest_commit_mapping()? {
        let state_id = StateId::parse(&state_id)?;
        let git_oid = git_sha
            .parse::<ObjectId>()
            .map_err(|err| GitProjectionError::InvalidMapping(err.to_string()))?;
        if scoped_commits
            .as_ref()
            .is_some_and(|commits| !commits.contains(&git_oid))
        {
            continue;
        }
        if mirror_repo.read_object(&git_oid).is_err() {
            continue;
        }
        if git_notes::read_note(mirror_repo, git_oid)?.is_some() {
            continue;
        }
        let tier = bridge
            .heddle_repo
            .effective_visibility_tier(&state_id)
            .map_err(|error| {
                GitProjectionError::Git(format!("resolve visibility for {state_id}: {error:#}"))
            })?;
        if !repo::visible(&tier, &repo::AudienceTier::Public) {
            continue;
        }
        let Some(state) = bridge.heddle_repo.store().get_state(&state_id)? else {
            continue;
        };
        git_notes::write_note(
            mirror_repo,
            git_oid,
            &git_notes::HeddleNote::from_state(&state),
        )?;
    }
    Ok(())
}

fn reachable_commits_from_updates(
    repo: &SleyRepository,
    updates: Vec<super::git_core::RefUpdate>,
) -> GitProjectionResult<HashSet<ObjectId>> {
    let mut stack = updates
        .into_iter()
        .map(|update| update.target)
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    let mut commits = HashSet::new();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let object = repo.read_object(&oid).map_err(super::git_core::git_err)?;
        match object.object_type {
            GitObjectType::Commit => {
                commits.insert(oid);
                let commit = repo.read_commit(&oid).map_err(super::git_core::git_err)?;
                stack.extend(commit.parents);
            }
            GitObjectType::Tag => {
                let tag = repo.read_tag(&oid).map_err(super::git_core::git_err)?;
                stack.push(tag.object);
            }
            GitObjectType::Tree | GitObjectType::Blob => {}
        }
    }
    Ok(commits)
}
