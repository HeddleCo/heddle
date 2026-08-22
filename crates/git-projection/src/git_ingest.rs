// SPDX-License-Identifier: Apache-2.0
//! Ingest-backed Git history import for bridge-facing commands.

use std::{collections::HashSet, path::Path};

use objects::store::ObjectStore;
use sley::ObjectId;

use super::{
    git_core::{
        collect_import_source_ref_updates, open_repo, GitProjection, GitProjectionError,
        GitProjectionResult, RefNamespace,
    },
    git_export::commit_requires_residual,
    git_residual::ResidualStore,
    git_util::ImportStats,
};

pub fn import_git_history(
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
    bridge.build_existing_mapping(Some(source))?;
    bridge.seed_ingest_identity_mappings_from_store()?;
    capture_import_residuals(bridge, source, refs, &stats)?;
    bridge.save_mapping_to_disk()?;
    Ok(import_stats_from_ingest(stats))
}

fn capture_import_residuals(
    bridge: &GitProjection<'_>,
    source: &Path,
    refs: &[String],
    stats: &ingest::ImportStats,
) -> GitProjectionResult<()> {
    let source_repo = open_repo(source)?;
    let updates = collect_import_source_ref_updates(&source_repo, refs)?;
    let residuals = ResidualStore::open(bridge.heddle_repo.heddle_dir());

    for git_sha in &stats.non_reconstructable_commits {
        let git_oid = git_sha
            .parse::<ObjectId>()
            .map_err(|error| GitProjectionError::InvalidMapping(error.to_string()))?;
        let Some(state_id) = bridge.mapping.get_heddle(git_oid) else {
            return Err(GitProjectionError::InvalidMapping(format!(
                "non-reconstructable imported commit {git_oid} has no Git Projection Mapping"
            )));
        };
        let Some(state) = bridge.heddle_repo.store().get_state(&state_id)? else {
            return Err(GitProjectionError::StateNotFound(state_id));
        };
        if !commit_requires_residual(&state) {
            return Err(GitProjectionError::InvalidMapping(format!(
                "ingest classified {git_oid} as non-reconstructable but its mapped state {state_id} is byte-faithful"
            )));
        }
        residuals.capture_commit_closure_from_git_repo(&source_repo, &git_oid)?;
    }

    for update in updates {
        match update.namespace {
            // The ingest pack writes annotated tags directly into the native
            // object store. No bridge-only residual/sidecar path is needed.
            RefNamespace::Tag | RefNamespace::Heddle => {}
            RefNamespace::Note => {
                residuals.capture_object_closure_from_git_repo(&source_repo, &update.target)?;
                residuals.record_note_ref(&update.name, update.target)?;
            }
            RefNamespace::Branch => {}
        }
    }
    Ok(())
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
        Some(_) => "heddle bridge git import --path <full-git-repo> --ref <ref>".to_string(),
        None => "heddle bridge git import --path <full-git-repo>".to_string(),
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
