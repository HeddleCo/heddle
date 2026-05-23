// SPDX-License-Identifier: Apache-2.0
//! Export Heddle states to Git commits functionality.

use objects::{
    error::HeddleError,
    object::{ChangeId, ContentHash, FileMode},
};
use repo::Repository as HeddleRepository;

use crate::bridge::{
    git_core::{GitBridge, GitBridgeError, GitResult, SyncMapping, git_err},
    git_notes,
    git_sync::{sync_marker_to_tag, sync_track_to_branch},
    git_util::ExportStats,
};

const SUBMODULE_PREFIX: &str = "heddle-submodule:";

/// Export a single state to Git.
pub fn export_state(
    mapping: &mut SyncMapping,
    heddle_repo: &HeddleRepository,
    repo: &gix::Repository,
    state_id: &ChangeId,
) -> GitResult<gix::hash::ObjectId> {
    let state = heddle_repo
        .store()
        .get_state(state_id)?
        .ok_or(GitBridgeError::StateNotFound(*state_id))?;

    let git_tree_oid = export_tree(heddle_repo, repo, &state.tree)?;
    // R6: emit the W2 footer on every exported commit. The footer is
    // durable across remotes; per-scope breakdowns ride on the opt-in
    // git note. For first-pass we audit nothing about the state's
    // annotation set (the audience defaults to "public"); a follow-up
    // landed with `bridge git export --audience` threads the count
    // through here. See `git_util::build_commit_message_with_footer`.
    let hosted_url = heddle_repo
        .config()
        .hosted
        .upstream_url
        .as_deref()
        .filter(|s| !s.is_empty());
    let message =
        GitBridge::build_commit_message_with_footer(&state, hosted_url, /*omitted=*/ 0);
    let parent_oids: Vec<gix::hash::ObjectId> = state
        .parents
        .iter()
        .map(|parent_id| {
            mapping
                .get_git(parent_id)
                .ok_or(GitBridgeError::StateNotFound(*parent_id))
        })
        .collect::<GitResult<Vec<_>>>()?;

    let sig = GitBridge::state_to_signature(&state);
    let mut committer_buf = gix::date::parse::TimeBuf::default();
    let mut author_buf = gix::date::parse::TimeBuf::default();
    let commit = repo
        .new_commit_as(
            sig.to_ref(&mut committer_buf),
            sig.to_ref(&mut author_buf),
            &message,
            git_tree_oid,
            parent_oids,
        )
        .map_err(git_err)?;
    Ok(commit.id)
}

/// Export a Heddle tree to Git.
pub fn export_tree(
    heddle_repo: &HeddleRepository,
    repo: &gix::Repository,
    tree_hash: &ContentHash,
) -> GitResult<gix::hash::ObjectId> {
    let tree = heddle_repo
        .store()
        .get_tree(tree_hash)?
        .ok_or_else(|| HeddleError::NotFound(format!("tree {}", tree_hash)))?;

    let empty_tree = gix::hash::ObjectId::empty_tree(repo.object_hash());
    let mut editor = repo.edit_tree(empty_tree).map_err(git_err)?;

    for entry in tree.entries() {
        let (kind, id) = if entry.is_tree() {
            (
                gix::object::tree::EntryKind::Tree,
                export_tree(heddle_repo, repo, &entry.hash)?,
            )
        } else {
            // Redaction safety: if the blob carries an active redaction
            // record, export the stub instead of the bytes. This is the
            // single chokepoint between Heddle-side redactions and any
            // downstream Git remote (GitHub, internal mirrors, ...).
            // Bytes that escape via the bridge are bytes that escape,
            // full stop — we cannot retroactively scrub them from
            // outside repos. The check sits *here*, not in
            // `materialize_blob`, because export reads `blob.content()`
            // directly (we never touch the materialize path) and writes
            // the raw bytes through `repo.write_blob`.
            let stub = heddle_repo
                .redaction_stub_for_blob(&entry.hash)
                .map_err(|err| HeddleError::Config(format!("redaction lookup failed: {err}")))?;

            if let Some(stub_text) = stub {
                // Stubs are text-only; ASCII safe across newline/BOM
                // quirks and submodule-pointer detection.
                let kind = match entry.mode {
                    FileMode::Symlink => gix::object::tree::EntryKind::Link,
                    FileMode::Executable => gix::object::tree::EntryKind::BlobExecutable,
                    _ => gix::object::tree::EntryKind::Blob,
                };
                let oid = repo
                    .write_blob(stub_text.as_bytes())
                    .map_err(git_err)?
                    .detach();
                (kind, oid)
            } else {
                let blob = heddle_repo
                    .store()
                    .get_blob(&entry.hash)?
                    .ok_or_else(|| HeddleError::NotFound(format!("blob {}", entry.hash)))?;

                if entry.mode == FileMode::Normal
                    && let Some(oid) = submodule_oid_from_blob(blob.content())
                {
                    (gix::object::tree::EntryKind::Commit, oid)
                } else {
                    let kind = match entry.mode {
                        FileMode::Normal => gix::object::tree::EntryKind::Blob,
                        FileMode::Executable => gix::object::tree::EntryKind::BlobExecutable,
                        FileMode::Symlink => gix::object::tree::EntryKind::Link,
                    };
                    let oid = repo.write_blob(blob.content()).map_err(git_err)?.detach();
                    (kind, oid)
                }
            }
        };

        editor.upsert(&entry.name, kind, id).map_err(git_err)?;
    }

    Ok(editor.write().map_err(git_err)?.detach())
}

/// Export all Heddle states to Git commits.
pub fn export_all(bridge: &mut GitBridge) -> GitResult<ExportStats> {
    bridge.init_mirror()?;

    let states = bridge.heddle_repo.store().list_states()?;
    let mut stats = ExportStats::default();

    bridge.build_existing_mapping(None)?;

    let sorted_states = bridge.sort_states_topologically(&states)?;
    let repo = bridge.open_git_repo()?;
    bridge.mapping.retain_git_objects(&repo);

    for state_id in sorted_states {
        // Skip states already mapped to a git object that exists in the
        // mirror — that's the common case for git-imported states whose
        // original commit bytes are already present (and whose SHAs we
        // want to preserve verbatim, which means NOT recreating them).
        if bridge.mapping.has_heddle(&state_id) {
            continue;
        }
        let git_oid = export_state(&mut bridge.mapping, bridge.heddle_repo, &repo, &state_id)?;
        bridge.mapping.insert(state_id, git_oid);
        stats.states_exported += 1;

        // Attach a heddle note to the freshly-created commit so the
        // change_id survives a fresh `git clone` of the destination
        // (when only the git side travels, without our sidecar).
        if let Some(state) = bridge.heddle_repo.store().get_state(&state_id)? {
            let note = git_notes::HeddleNote::from_state(&state);
            git_notes::write_note(&repo, git_oid, &note)?;
        }
    }

    // For states whose git_oid was already in the mapping (the SHA-stable
    // path above), make sure the note is present too. This covers two
    // cases: (a) the state was imported from a non-heddle git source and
    // never had a note, and (b) the note was deleted from the mirror.
    let note_targets: Vec<(ChangeId, gix::hash::ObjectId)> =
        bridge.mapping.iter().map(|(c, o)| (*c, *o)).collect();
    for (change_id, git_oid) in note_targets {
        if git_notes::read_note(&repo, git_oid)?.is_none()
            && let Some(state) = bridge.heddle_repo.store().get_state(&change_id)?
        {
            let note = git_notes::HeddleNote::from_state(&state);
            git_notes::write_note(&repo, git_oid, &note)?;
        }
    }

    let threads = bridge.heddle_repo.refs().list_threads()?;
    for track_name in threads {
        if let Some(state_id) = bridge.heddle_repo.refs().get_thread(&track_name)?
            && let Some(git_oid) = bridge.mapping.get_git(&state_id)
        {
            sync_track_to_branch(&repo, &track_name, git_oid)?;
            stats.threads_synced += 1;
        }
    }

    let markers = bridge.heddle_repo.refs().list_markers()?;
    for marker_name in markers {
        if let Some(state_id) = bridge.heddle_repo.refs().get_marker(&marker_name)?
            && let Some(git_oid) = bridge.mapping.get_git(&state_id)
        {
            sync_marker_to_tag(&repo, &marker_name, git_oid)?;
            stats.markers_synced += 1;
        }
    }

    bridge.save_mapping_to_disk()?;

    Ok(stats)
}

fn submodule_oid_from_blob(content: &[u8]) -> Option<gix::hash::ObjectId> {
    let text = std::str::from_utf8(content).ok()?;
    let text = text.trim();
    let trimmed = text.strip_prefix(SUBMODULE_PREFIX)?.trim();

    trimmed.parse().ok()
}
