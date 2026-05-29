// SPDX-License-Identifier: Apache-2.0
//! Export Heddle states to Git commits functionality.

use objects::store::ObjectStore;
use std::collections::HashSet;

use gix::bstr::ByteSlice;
use objects::{
    error::HeddleError,
    object::{ChangeId, ContentHash, FileMode, ThreadName},
};
use repo::Repository as HeddleRepository;

use crate::bridge::{
    git_core::{
        GitBridge, GitBridgeError, GitResult, LocalGitIdentity, SyncMapping,
        count_exported_commits, git_config_identity_with_global_fallback, git_err,
        principal_is_default_unknown,
    },
    git_notes,
    git_sync::{sync_marker_to_tag, sync_track_to_branch},
    git_util::{ExportStats, ExportedRef},
};

const SUBMODULE_PREFIX: &str = "heddle-submodule:";

/// Export a single state to Git.
pub(crate) fn export_state(
    mapping: &mut SyncMapping,
    heddle_repo: &HeddleRepository,
    repo: &gix::Repository,
    state_id: &ChangeId,
    identity: Option<&LocalGitIdentity>,
    message_override: Option<&str>,
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
    let message = match message_override {
        Some(message) => GitBridge::build_commit_message_with_footer_with_body(
            &state, message, hosted_url, /*omitted=*/ 0,
        ),
        None => {
            GitBridge::build_commit_message_with_footer(&state, hosted_url, /*omitted=*/ 0)
        }
    };
    let parent_oids: Vec<gix::hash::ObjectId> = state
        .parents
        .iter()
        .map(|parent_id| {
            mapping
                .get_git(parent_id)
                .ok_or(GitBridgeError::StateNotFound(*parent_id))
        })
        .collect::<GitResult<Vec<_>>>()?;

    let sig = if principal_is_default_unknown(&state.attribution.principal) {
        let Some(identity) = identity else {
            return Err(GitBridgeError::Git(
                "refusing to write a Git commit with Unknown <unknown@example.com>; configure user.name/user.email, HEDDLE_PRINCIPAL_NAME/HEDDLE_PRINCIPAL_EMAIL, or .heddle principal".to_string(),
            ));
        };
        identity.to_signature(state.created_at.timestamp())
    } else {
        state_to_signature(&state)
    };
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
    export_scoped(bridge, None)
}

/// Export one Heddle thread to its matching Git branch.
pub fn export_current_thread(bridge: &mut GitBridge, thread: &str) -> GitResult<ExportStats> {
    export_scoped(bridge, Some(thread))
}

fn export_scoped(bridge: &mut GitBridge, thread: Option<&str>) -> GitResult<ExportStats> {
    bridge.init_mirror()?;

    let states = match thread {
        Some(thread) => {
            let Some(state_id) = bridge.heddle_repo.refs().get_thread(&ThreadName::new(thread))? else {
                return Err(GitBridgeError::Git(format!(
                    "thread '{thread}' has no state to export"
                )));
            };
            reachable_states(bridge.heddle_repo, &[state_id])?
        }
        None => bridge.heddle_repo.store().list_states()?,
    };
    let mut stats = ExportStats::default();

    bridge.build_existing_mapping(None)?;
    let identity = git_config_identity_with_global_fallback(bridge.heddle_repo.root())?;

    let sorted_states = bridge.sort_states_topologically(&states)?;
    let repo = bridge.open_git_repo()?;
    bridge.mapping.retain_git_objects(&repo);
    bridge.seed_git_checkpoint_mappings_from_checkout(&repo)?;

    // Git OIDs minted during this run. Used below to partition the copied
    // ref set into newly-written vs already-mapped — so the "newly" count
    // is a subset of the same walk that produces the total, never a
    // parallel tally over `list_states()` that could include an orphan
    // state reachable from no copied ref.
    let mut newly_minted: HashSet<gix::hash::ObjectId> = HashSet::new();

    for state_id in sorted_states {
        // Skip states already mapped to a git object that exists in the
        // mirror — that's the common case for git-imported states whose
        // original commit bytes are already present (and whose SHAs we
        // want to preserve verbatim, which means NOT recreating them).
        if bridge.mapping.has_heddle(&state_id) {
            // Already mapped to an existing commit — nothing to mint.
            // Whether it counts toward the total is decided below by
            // ref-reachability, not by membership in the walked set.
            continue;
        }
        let message_override = bridge
            .commit_message_overrides
            .get(&state_id)
            .map(String::as_str);
        let git_oid = export_state(
            &mut bridge.mapping,
            bridge.heddle_repo,
            &repo,
            &state_id,
            identity.as_ref(),
            message_override,
        )?;
        bridge.mapping.insert(state_id, git_oid);
        newly_minted.insert(git_oid);

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

    let threads: Vec<String> = match thread {
        Some(thread) => vec![thread.to_string()],
        None => {
            let remote_names = git_remote_names(bridge.heddle_repo);
            bridge
                .heddle_repo
                .refs()
                .list_threads()?
                .into_iter()
                .filter(|thread| !is_remote_tracking_thread_name(thread, &remote_names))
                .map(|t| t.to_string())
                .collect()
        }
    };
    for track_name in threads {
        if let Some(state_id) = bridge.heddle_repo.refs().get_thread(&ThreadName::new(&track_name))?
            && let Some(git_oid) = bridge.mapping.get_git(&state_id)
        {
            sync_track_to_branch(&repo, &track_name, git_oid)?;
            stats.threads_synced += 1;
            stats.branches.push(ExportedRef {
                name: track_name.clone(),
                tip: git_oid,
            });
        }
    }

    if thread.is_none() {
        let markers = bridge.heddle_repo.refs().list_markers()?;
        for marker_name in markers {
            if let Some(state_id) = bridge.heddle_repo.refs().get_marker(&marker_name)?
                && let Some(git_oid) = bridge.mapping.get_git(&state_id)
            {
                sync_marker_to_tag(&repo, &marker_name, git_oid)?;
                stats.markers_synced += 1;
                stats.tags.push(ExportedRef {
                    name: marker_name.to_string(),
                    tip: git_oid,
                });
            }
        }
    }

    // Every count in the summary is a partition of the SINGLE copied ref
    // set: `total` is unique commits reachable from the mirror's branch/tag
    // tips (the exact ref set `copy_mirror_to_path` writes via
    // `collect_ref_updates`), and `states_exported` ("newly") is the subset
    // of THAT walk minted this run. Deriving both from one walk — rather
    // than tallying `states_exported` inline over `list_states()` — makes
    // `newly + already == total` hold by construction: a state minted into
    // the mirror but reachable from no copied ref (e.g. a dropped thread's
    // orphan history) is in neither count, so the impossible
    // "1 total (2 newly written)" summary cannot occur.
    let counts = count_exported_commits(&repo, &newly_minted)?;
    stats.commits_total = counts.total;
    stats.states_exported = counts.newly;

    bridge.save_mapping_to_disk()?;

    Ok(stats)
}

fn git_remote_names(heddle_repo: &HeddleRepository) -> HashSet<String> {
    let Ok(repo) = gix::discover(heddle_repo.root()) else {
        return HashSet::new();
    };
    repo.remote_names()
        .into_iter()
        .map(|name| name.to_str_lossy().into_owned())
        .filter(|name| !name.trim().is_empty())
        .collect()
}

fn is_remote_tracking_thread_name(thread: &str, remote_names: &HashSet<String>) -> bool {
    let Some((remote, branch)) = thread.split_once('/') else {
        return false;
    };
    !branch.is_empty() && remote_names.contains(remote)
}

fn reachable_states(
    heddle_repo: &HeddleRepository,
    roots: &[ChangeId],
) -> GitResult<Vec<ChangeId>> {
    let mut stack = roots.to_vec();
    let mut seen = HashSet::new();
    let mut states = Vec::new();
    while let Some(state_id) = stack.pop() {
        if !seen.insert(state_id) {
            continue;
        }
        states.push(state_id);
        if let Some(state) = heddle_repo.store().get_state(&state_id)? {
            stack.extend(state.parents.iter().copied());
        }
    }
    Ok(states)
}

fn state_to_signature(state: &objects::object::State) -> gix::actor::Signature {
    gix::actor::Signature {
        name: state.attribution.principal.name.as_str().into(),
        email: state.attribution.principal.email.as_str().into(),
        time: gix::date::Time {
            seconds: state.created_at.timestamp(),
            offset: 0,
        },
    }
}

fn submodule_oid_from_blob(content: &[u8]) -> Option<gix::hash::ObjectId> {
    let text = std::str::from_utf8(content).ok()?;
    let text = text.trim();
    let trimmed = text.strip_prefix(SUBMODULE_PREFIX)?.trim();

    trimmed.parse().ok()
}
