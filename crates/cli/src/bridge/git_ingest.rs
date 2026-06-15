// SPDX-License-Identifier: Apache-2.0
//! Ingest-backed Git history import for bridge-facing commands.

use std::{collections::HashSet, path::Path};

use super::{
    git_core::{GitBridge, GitBridgeError, GitResult, open_repo},
    git_util::ImportStats,
};

pub(crate) fn import_git_history(
    bridge: &mut GitBridge<'_>,
    git_path: Option<&Path>,
    refs: &[String],
    options: ingest::ImportOptions,
    progress: Option<&mut dyn FnMut(ingest::ImportProgressEvent)>,
) -> GitResult<ImportStats> {
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
    Ok(import_stats_from_ingest(stats))
}

fn map_ingest_error(error: ingest::IngestError) -> GitBridgeError {
    match error {
        ingest::IngestError::ThreadDiverged {
            thread,
            branch,
            existing,
            incoming,
        } => GitBridgeError::GitHeddleThreadDiverged {
            thread,
            branch,
            thread_change: existing,
            branch_change: incoming,
        },
        other => GitBridgeError::Git(other.to_string()),
    }
}

fn reject_shallow_source(source: &Path, refs: &[String]) -> GitResult<()> {
    let repo = open_repo(source)?;
    if repo.git_dir().join("shallow").is_file() {
        let wanted = (!refs.is_empty()).then(|| refs.iter().cloned().collect::<HashSet<_>>());
        return Err(GitBridgeError::ShallowClone {
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
