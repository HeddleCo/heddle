// SPDX-License-Identifier: Apache-2.0
//! Export Heddle states to Git commits functionality.

use objects::store::ObjectStore;
use std::collections::HashSet;

use gix::bstr::ByteSlice;
use gix::refs::transaction::PreviousValue;
use objects::{
    error::HeddleError,
    object::{ChangeId, ContentHash, FileMode, MarkerName, ThreadName},
};
use repo::{AudienceTier, Repository as HeddleRepository, visible};

use crate::bridge::{
    git_core::{
        GitBridge, GitBridgeError, GitResult, LocalGitIdentity, SyncMapping,
        count_exported_commits, delete_reference_if_present,
        git_config_identity_with_global_fallback, git_err, principal_is_default_unknown,
        set_reference,
    },
    git_notes,
    git_sync::{sync_marker_to_tag, sync_track_to_branch},
    git_util::{ExportStats, ExportedRef},
};

const SUBMODULE_PREFIX: &str = "heddle-submodule:";

/// Export a single state to Git for `audience`.
///
/// Returns `Ok(None)` — **absence** — when the state's effective visibility
/// tier is not visible to `audience`: the public mirror never mints a Git
/// commit (no stub, no partial tree) for an embargoed state (spike §5.0/§5.3).
/// The caller realizes downward-closure by also withholding any state whose
/// parent was withheld, so an embargoed commit *and its descendants* stay
/// absent from the mirror.
pub(crate) fn export_state(
    mapping: &mut SyncMapping,
    heddle_repo: &HeddleRepository,
    repo: &gix::Repository,
    state_id: &ChangeId,
    identity: Option<&LocalGitIdentity>,
    message_override: Option<&str>,
    audience: &AudienceTier,
) -> GitResult<Option<gix::hash::ObjectId>> {
    let state = heddle_repo
        .store()
        .get_state(state_id)?
        .ok_or(GitBridgeError::StateNotFound(*state_id))?;

    // Audience-aware minting. The visibility decision lives here, at the state
    // walk where the `ChangeId` is in scope — never in the blob-keyed
    // `export_tree` (no `ChangeId`/audience).
    let tier = heddle_repo
        .effective_visibility_tier(state_id)
        .map_err(|e| GitBridgeError::Git(format!("resolve visibility for {state_id}: {e:#}")))?;
    if !visible(&tier, audience) {
        return Ok(None);
    }

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
    Ok(Some(commit.id))
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

    // The Git bridge publishes the PUBLIC mirror — the export audience is
    // always `Public`. Per-commit visibility is enforced here, in the OSS
    // bridge, by emitting absence (the authoritative wire serve gate is weft's
    // job, spike §10 #4).
    let audience = AudienceTier::Public;

    let sorted_states = bridge.sort_states_topologically(&states)?;
    // Reachable set, used to tell a withheld parent (absent from the mapping
    // but present in this export) apart from a genuinely-missing shallow
    // boundary (absent from both).
    let reachable: HashSet<ChangeId> = sorted_states.iter().copied().collect();
    let repo = bridge.open_git_repo()?;
    bridge.mapping.retain_git_objects(&repo);
    bridge.seed_git_checkpoint_mappings_from_checkout(&repo)?;

    // The desired/actual ref sets span the WHOLE mirror, not just this export's
    // scoped thread: a prior all-thread export can leave `refs/heads`/`refs/tags`
    // for OTHER threads/markers whose commits — or their ancestors — were later
    // marked Private. Reconciling only the scoped thread would keep serving those
    // now-embargoed commits via the other thread's branch (heddle#316 cross-thread
    // embargo leak). So purge + project + reconcile over every heddle-managed
    // thread/marker regardless of scope; the mint loop below stays scoped (only the
    // requested thread's new commits are minted), so widening changes WHICH refs
    // are reconciled, never what gets created.
    let remote_names = git_remote_names(bridge.heddle_repo);
    let threads: Vec<String> = {
        let mut all: Vec<String> = bridge
            .heddle_repo
            .refs()
            .list_threads()?
            .into_iter()
            .filter(|thread| !is_remote_tracking_thread_name(thread, &remote_names))
            .map(|t| t.to_string())
            .collect();
        // A scoped export's own thread may be a remote-tracking name the filter
        // drops; keep it so the requested thread is always reconciled.
        if let Some(t) = thread
            && !all.iter().any(|x| x == t)
        {
            all.push(t.to_string());
        }
        all
    };
    let markers: Vec<MarkerName> = bridge.heddle_repo.refs().list_markers()?;

    // Roots of the whole-mirror served frontier: every reconciled thread's tip and
    // every marker's state. Purging over their reachable closure (below) drops any
    // out-of-scope commit whose tier — or an ancestor's — is now unserved, so
    // `project_desired_refs` lags those branches/tags correctly even on a scoped
    // export (heddle#316).
    let mut frontier_roots: Vec<ChangeId> = Vec::new();
    for track_name in &threads {
        if let Some(tip) = bridge
            .heddle_repo
            .refs()
            .get_thread(&ThreadName::new(track_name))?
        {
            frontier_roots.push(tip);
        }
    }
    for marker_name in &markers {
        if let Some(state_id) = bridge.heddle_repo.refs().get_marker(marker_name)? {
            frontier_roots.push(state_id);
        }
    }
    let frontier_reachable = reachable_states(bridge.heddle_repo, &frontier_roots)?;

    // Re-validate the served set against CURRENT visibility before anything treats
    // a mapping as "already served". A state minted while public in a prior export
    // can be marked under-tier later; `build_existing_mapping` rebuilds its stale
    // ChangeId→OID mapping from the notes/sidecar every run, so without this purge
    // the frontier walk, the note re-write, and the tag sync would all keep serving
    // the now-embargoed commit. Purging is downward-closed: a still-visible state
    // whose ancestor is embargoed is withheld too (its Git commit chains to the
    // embargoed one). The purge spans the mint set UNION the whole-mirror frontier,
    // so a scoped export still drops an out-of-scope thread's now-embargoed tip; for
    // an all-thread export the frontier ⊆ the mint set and this reduces to the prior
    // behavior. After this, `mapping` == the served set across every reconciled ref,
    // exactly what `frontier_git_oid` assumes.
    // Snapshot EVERY mapped target before the purge mutates the mapping: these are
    // exactly the commits that may already carry a `refs/notes/*` entry in the
    // mirror, so the notes-ref retraction below must consider all of them —
    // including the states the purge is about to drop AND any orphaned mapping a
    // deleted thread left behind, which no current-ref frontier reaches (heddle#316).
    let pre_purge_targets: Vec<(ChangeId, gix::hash::ObjectId)> =
        bridge.mapping.iter().map(|(c, o)| (*c, *o)).collect();

    let purge_reachable: HashSet<ChangeId> = sorted_states
        .iter()
        .copied()
        .chain(frontier_reachable.iter().copied())
        .collect();
    let purge_sorted =
        bridge.sort_states_topologically(&purge_reachable.iter().copied().collect::<Vec<_>>())?;
    let embargoed_oids = purge_unserved_mappings(
        bridge.heddle_repo,
        &mut bridge.mapping,
        &purge_sorted,
        &purge_reachable,
        &audience,
    )?;

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

        // Downward-closure (spike §5.0): withhold a state whose parent was
        // itself withheld for this audience. Processed in topo order, so a
        // parent's mapped-ness is already decided. A parent absent from the
        // mapping but present in `reachable` was withheld → withhold this
        // child too (and, transitively, all its descendants). A parent absent
        // from both is a shallow boundary (public-by-absence) — let the mint
        // proceed exactly as before.
        let parent_withheld = bridge
            .heddle_repo
            .store()
            .get_state(&state_id)?
            .map(|state| {
                state
                    .parents
                    .iter()
                    .any(|p| reachable.contains(p) && bridge.mapping.get_git(p).is_none())
            })
            .unwrap_or(false);
        if parent_withheld {
            continue;
        }

        let message_override = bridge
            .commit_message_overrides
            .get(&state_id)
            .map(String::as_str);
        let Some(git_oid) = export_state(
            &mut bridge.mapping,
            bridge.heddle_repo,
            &repo,
            &state_id,
            identity.as_ref(),
            message_override,
            &audience,
        )?
        else {
            // Embargoed for this audience — emit absence (no commit minted).
            continue;
        };
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

    // The downward-closure served set across EVERY note target — the pre-purge
    // mapping (commits that may already carry a note in the mirror) UNION the
    // current post-mint mapping (served states + freshly minted commits),
    // computed over the FULL ancestry of all of them. The branch purge is
    // ref-rooted (it walks the whole-mirror frontier of current thread tips +
    // markers), so it never examines an ORPHANED mapping a deleted thread left
    // behind; without this closure such a commit's note — public-tier but with a
    // now-Private ancestor — would slip past both the backfill gate and the
    // retraction below. This is the SAME served rule the branch frontier uses,
    // applied to notes (heddle#316). For an all-states export it reduces to the
    // post-purge served set, so behavior there is unchanged.
    let note_target_roots: Vec<ChangeId> = pre_purge_targets
        .iter()
        .map(|(c, _)| *c)
        .chain(bridge.mapping.iter().map(|(c, _)| *c))
        .collect();
    let note_reachable_vec = reachable_states(bridge.heddle_repo, &note_target_roots)?;
    let note_reachable: HashSet<ChangeId> = note_reachable_vec.iter().copied().collect();
    let note_sorted = bridge.sort_states_topologically(&note_reachable_vec)?;
    let note_served = served_change_ids(
        bridge.heddle_repo,
        &note_sorted,
        &note_reachable,
        &audience,
    )?;

    // For states whose git_oid was already in the mapping (the SHA-stable
    // path above), make sure the note is present too. This covers two
    // cases: (a) the state was imported from a non-heddle git source and
    // never had a note, and (b) the note was deleted from the mirror.
    let note_targets: Vec<(ChangeId, gix::hash::ObjectId)> =
        bridge.mapping.iter().map(|(c, o)| (*c, *o)).collect();
    for (change_id, git_oid) in note_targets {
        // Gate the backfill on the downward-closure served set, not the commit's
        // DIRECT tier. The mapping can carry orphaned entries (a deleted thread's
        // commits) the ref-rooted purge never examined; gating on direct
        // visibility alone would re-publish a note for a public commit whose
        // ancestor became Private — a commit the branch downward-closure
        // withholds. `note_served` is the same served notion the branch frontier
        // uses, so no note-write site can emit metadata for an unserved commit
        // (heddle#316).
        if note_served.contains(&change_id)
            && git_notes::read_note(&repo, git_oid)?.is_none()
            && let Some(state) = bridge.heddle_repo.store().get_state(&change_id)?
        {
            let note = git_notes::HeddleNote::from_state(&state);
            git_notes::write_note(&repo, git_oid, &note)?;
        }
    }

    // Retract the notes for every mapped target that is NOT served under the
    // downward-closure rule. The mirror copies `refs/notes/*`
    // (`collect_ref_updates`) alongside branches and tags, so a note left for an
    // unserved commit keeps leaking its metadata even after its branch/tag were
    // retracted. This is the notes-ref sibling of the branch/tag retraction
    // above (heddle#316). Considering EVERY pre-purge target — not just the
    // `embargoed_oids` the ref-rooted purge dropped — catches an orphaned note an
    // ancestor embargo stranded on a deleted thread's commit. Guard the
    // degenerate case where a still-served state maps to the same git OID by
    // keeping any OID a served target maps to.
    let served_note_oids: HashSet<gix::hash::ObjectId> = pre_purge_targets
        .iter()
        .copied()
        .chain(bridge.mapping.iter().map(|(c, o)| (*c, *o)))
        .filter(|(c, _)| note_served.contains(c))
        .map(|(_, oid)| oid)
        .collect();
    let notes_to_retract: HashSet<gix::hash::ObjectId> = pre_purge_targets
        .iter()
        .filter(|(c, _)| !note_served.contains(c))
        .map(|(_, oid)| *oid)
        .filter(|oid| !served_note_oids.contains(oid))
        .collect();
    git_notes::remove_notes(&repo, &notes_to_retract)?;

    // THE PROJECTION (heddle#316 r13): the desired heddle-owned ref-set for this
    // audience — heads lagged to the served frontier, tags at served markers — as
    // a pure function of the post-purge served `mapping` + audience + ownership.
    // Every mirror ref op below (set / forced embargo retract / delete) is DERIVED
    // from this ONE map, so a ref surface can never drift out of one enforcement
    // pass while another keeps serving it. The mirror MATERIALIZES this desired
    // set; downstream `plan_destination_reconcile` then reconciles each
    // destination against it — one projection, one reconcile, all destinations.
    let desired = project_desired_refs(bridge.heddle_repo, &bridge.mapping, &threads, &markers)?;

    // Reconcile the mirror's heads to the desired set. The mirror is heddle-owned
    // (no out-of-band tips), so the apply keeps the FF guard on an ordinary
    // advance, FORCES only a deliberate embargo retract (the prior tip is now
    // embargoed), and DELETES a head the projection dropped (its line has no
    // served frontier — embargoed-to-root, or reset/rebased onto a Private root).
    //
    // A scoped export reconciles every CURRENT thread (heddle#316) so it RETRACTS
    // an out-of-scope thread's branch whose commit became embargoed — but it only
    // MATERIALIZES (creates) the branch it was scoped to. An out-of-scope thread
    // with no existing mirror branch is left absent: a scoped export must not
    // publish a brand-new branch for a thread the caller did not ask to export
    // (that would also strand a stale ref if the thread is later dropped — the
    // #289 dropped-thread contract). A branch can only "keep serving" an embargoed
    // commit if it already EXISTS, and existing branches are still rewound/deleted
    // below, so this skips nothing that could leak.
    for track_name in &threads {
        if bridge
            .heddle_repo
            .refs()
            .get_thread(&ThreadName::new(track_name))?
            .is_none()
        {
            // A listed thread name with no tip is neither synced nor pruned.
            continue;
        }
        let branch_ref = format!("refs/heads/{track_name}");
        let in_scope = thread.is_none() || thread == Some(track_name.as_str());
        match desired.get(&branch_ref).copied() {
            Some(git_oid) => {
                let existing = branch_tip_oid(&repo, &branch_ref);
                if existing.is_none() && !in_scope {
                    // Out-of-scope served thread with no mirror branch yet: a
                    // scoped export retracts, it does not materialize.
                    continue;
                }
                let retracting = existing.is_some_and(|oid| embargoed_oids.contains(&oid));
                if retracting {
                    set_reference(
                        &repo,
                        &branch_ref,
                        git_oid,
                        PreviousValue::Any,
                        "heddle: retract embargoed thread frontier",
                    )?;
                } else {
                    sync_track_to_branch(&repo, track_name, git_oid)?;
                }
                stats.threads_synced += 1;
                stats.branches.push(ExportedRef {
                    name: track_name.clone(),
                    tip: git_oid,
                });
            }
            None => {
                // Absent from the projection ⇒ no served ancestor-or-self ⇒ any
                // prior mirror branch is stale. `delete_reference_if_present` is a
                // no-op when absent, so this also covers genuine emit-absence.
                delete_reference_if_present(&repo, &branch_ref)?;
            }
        }
    }

    // The downward-closure served set over the whole-mirror frontier — the SAME
    // closure the purge ran over (every thread tip + every marker state). A marker
    // target is served iff it is visible to this audience AND every reachable
    // ancestor is served. The marker reconcile below uses this to classify a
    // marker that `desired` dropped, since `desired` collapses two distinct causes
    // to absent (heddle#316).
    let frontier_served = {
        let reachable_set: HashSet<ChangeId> = frontier_reachable.iter().copied().collect();
        let sorted = bridge.sort_states_topologically(&frontier_reachable)?;
        served_change_ids(bridge.heddle_repo, &sorted, &reachable_set, &audience)?
    };

    // Reconcile the mirror's tags. The tag's fate is a pure function of THREE
    // axes — the NEW marker target {served+minted, served-but-unminted, unserved,
    // no-target(deleted)} × the EXISTING mirror tag's target {served, embargoed,
    // absent} × scope {scoped, full} — never the existing tag's mere PRESENCE.
    // Inspecting the existing tag's TARGET (the axis the prior code never read) is
    // what distinguishes a served-but-unminted tag worth preserving (r18) from one
    // pointing at an embargoed commit that must be deleted (r19).
    //
    // Iterate the UNION of current markers AND existing mirror tag names: a DELETED
    // marker drops out of `markers`, so its stale mirror tag is only reachable via
    // the existing-tags side (heddle#316 S3 — a deleted marker must delete its tag).
    enum TagTarget {
        /// Marker target is served AND minted into the mirror this run (its OID).
        ServedMinted(gix::hash::ObjectId),
        /// Marker target is served (visible + every reachable ancestor served) but
        /// NOT minted into the mapping this run — a scoped export that didn't reach
        /// it. A later all-thread export re-mints + advances the tag.
        ServedUnminted,
        /// Marker target is unserved (embargoed, withheld for a withheld ancestor,
        /// or retargeted to a never-minted Private state).
        Unserved,
        /// No marker by this name exists any more (deleted) — only a stale mirror
        /// tag may remain.
        NoTarget,
    }

    let mut tag_names: std::collections::BTreeSet<String> =
        markers.iter().map(|m| m.to_string()).collect();
    for tag in repo.references().map_err(git_err)?.tags().map_err(git_err)? {
        let tag = tag.map_err(git_err)?;
        tag_names.insert(tag.name().shorten().to_string());
    }

    for name in &tag_names {
        let tag_ref = format!("refs/tags/{name}");
        let existing_oid = branch_tip_oid(&repo, &tag_ref);
        // The existing mirror tag is "served" iff its tip was NOT dropped by this
        // run's purge. An existing tip in `embargoed_oids` is serving a commit the
        // purge just withheld — preserving it would keep leaking it (the r19 root
        // cause: the prior code never read this axis).
        let existing_served = existing_oid
            .map(|oid| !embargoed_oids.contains(&oid))
            .unwrap_or(false);
        let in_scope_full = thread.is_none();

        let new_status = match bridge.heddle_repo.refs().get_marker(&MarkerName::new(name.as_str()))? {
            None => TagTarget::NoTarget,
            Some(state) => match bridge.mapping.get_git(&state) {
                Some(oid) => TagTarget::ServedMinted(oid),
                None if frontier_served.contains(&state) => TagTarget::ServedUnminted,
                None => TagTarget::Unserved,
            },
        };

        match (new_status, existing_oid, existing_served) {
            // Cell 5 — scoped export, served+minted target, no mirror tag yet:
            // retract-only, never materialize a brand-new tag the caller did not
            // ask to export (symmetric with the head reconcile's materialize gate).
            (TagTarget::ServedMinted(_), None, _) if !in_scope_full => {}
            // Cells 1-4 — served+minted target: FORCE-retarget the mirror tag to it.
            // `sync_marker_to_tag` force-sets an existing tag (heddle#316 S1) and
            // creates an absent one; tags are free-move by design.
            (TagTarget::ServedMinted(oid), _, _) => {
                sync_marker_to_tag(&repo, name, oid)?;
                stats.markers_synced += 1;
                stats.tags.push(ExportedRef {
                    name: name.clone(),
                    tip: oid,
                });
            }
            // Cell 6 (r18) — served-but-unminted target whose EXISTING tag is itself
            // served: PRESERVE. A scoped export merely didn't mint it; a later
            // all-thread export re-mints the target and advances the tag.
            (TagTarget::ServedUnminted, Some(_), true) => {}
            // Cell 7 (r19 FIX) — served-but-unminted target but the EXISTING tag
            // points at an embargoed commit: preserving would keep serving it.
            // DELETE. PRESERVE requires `existing_served`.
            (TagTarget::ServedUnminted, Some(_), false) => {
                delete_reference_if_present(&repo, &tag_ref)?;
            }
            // Cell 8 — served-but-unminted target, no existing tag: nothing to do
            // (a scoped export does not materialize an unminted target).
            (TagTarget::ServedUnminted, None, _) => {}
            // Cells 9-11 — target genuinely unserved (embargoed / withheld ancestor
            // / Private): the tag is stale. DELETE (a no-op when the ref is absent).
            (TagTarget::Unserved, _, _) => {
                delete_reference_if_present(&repo, &tag_ref)?;
            }
            // Cells 12-13 (S3 FIX) — the marker was deleted: its stale mirror tag
            // must go (DELETE is a no-op when the ref is already absent).
            (TagTarget::NoTarget, _, _) => {
                delete_reference_if_present(&repo, &tag_ref)?;
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

/// Purge from `mapping` every reachable state whose effective visibility is no
/// longer served by `audience`, and return the Git OIDs that were dropped so
/// the caller can retract any ref still pointing at them.
///
/// A state can be minted while public and only later marked under-tier; its
/// stale ChangeId→OID mapping is rebuilt from the notes/sidecar on every
/// export, so the served set must be re-derived against CURRENT visibility
/// here rather than trusted from the mapping. The purge is downward-closed: a
/// still-visible state is unserved if any reachable ancestor is unserved,
/// because its minted Git commit chains to the ancestor's (now-embargoed)
/// commit. `sorted_states` is topological (parents before children), so a
/// parent's served-ness is decided before its child is examined.
fn purge_unserved_mappings(
    heddle_repo: &HeddleRepository,
    mapping: &mut SyncMapping,
    sorted_states: &[ChangeId],
    reachable: &HashSet<ChangeId>,
    audience: &AudienceTier,
) -> GitResult<HashSet<gix::hash::ObjectId>> {
    let served = served_change_ids(heddle_repo, sorted_states, reachable, audience)?;
    let mut purged: HashSet<gix::hash::ObjectId> = HashSet::new();
    for state_id in sorted_states {
        if !served.contains(state_id)
            && let Some(oid) = mapping.remove(state_id)
        {
            purged.insert(oid);
        }
    }
    Ok(purged)
}

/// The downward-closure served set (spike §5.0): a state is served iff it is
/// visible to `audience` AND every *reachable* parent is itself served. The
/// topo order of `sorted_states` guarantees a parent's servedness is already
/// decided when its child is visited. A parent outside `reachable` is a shallow
/// boundary (public-by-absence, treated as served).
///
/// The single notion of "served" shared by the branch-frontier purge and the
/// notes-ref retraction — so a note can never be published for a commit whose
/// branch the same rule would withhold (heddle#316).
fn served_change_ids(
    heddle_repo: &HeddleRepository,
    sorted_states: &[ChangeId],
    reachable: &HashSet<ChangeId>,
    audience: &AudienceTier,
) -> GitResult<HashSet<ChangeId>> {
    let mut served: HashSet<ChangeId> = HashSet::new();
    for state_id in sorted_states {
        let tier = heddle_repo.effective_visibility_tier(state_id).map_err(|e| {
            GitBridgeError::Git(format!("resolve visibility for {state_id}: {e:#}"))
        })?;
        let parents_served = match heddle_repo.store().get_state(state_id)? {
            Some(state) => state
                .parents
                .iter()
                .all(|p| !reachable.contains(p) || served.contains(p)),
            None => true,
        };
        if visible(&tier, audience) && parents_served {
            served.insert(*state_id);
        }
    }
    Ok(served)
}

/// Resolve `ref_name` to its tip commit OID in the mirror, or `None` when the
/// ref is absent or unpeelable.
fn branch_tip_oid(repo: &gix::Repository, ref_name: &str) -> Option<gix::hash::ObjectId> {
    let mut reference = repo.find_reference(ref_name).ok()?;
    reference.peel_to_id().ok().map(|id| id.detach())
}

/// Project the DESIRED heddle-owned ref-set for an export: full ref name → its
/// served target OID. A ref appears iff heddle should publish it now; a ref the
/// projection omits is one the mirror reconcile must DELETE (its prior export is
/// stale). This is the single place that decides WHICH refs exist and at WHAT
/// target — the mirror reconcile, and downstream every destination reconcile,
/// derive their ops (create / fast-forward / forced rewind / delete / skip) from
/// this set, so a surface can never silently drop out of one enforcement pass
/// while another keeps serving it (heddle#316 r13).
///
/// * heads — `refs/heads/<thread>` at the maximal SERVED ancestor-or-self of the
///   thread tip ([`frontier_git_oid`]); a thread whose whole line is unserved is
///   ABSENT (downward-closed: an embargoed commit and its descendants stay off
///   the public branch).
/// * tags — `refs/tags/<marker>` at the marker's served state; a marker whose
///   state is not served (embargoed, withheld for a withheld ancestor, or
///   retargeted to a never-minted Private state) is ABSENT.
///
/// Notes (`refs/notes/heddle`) are the history-bearing member of the desired set
/// and are projected by content rebuild (backfill + [`git_notes::remove_notes`])
/// upstream rather than a target swap, so they are not enumerated here.
fn project_desired_refs(
    heddle_repo: &HeddleRepository,
    mapping: &SyncMapping,
    threads: &[String],
    markers: &[MarkerName],
) -> GitResult<std::collections::HashMap<String, gix::hash::ObjectId>> {
    let mut desired = std::collections::HashMap::new();
    for track_name in threads {
        let Some(tip) = heddle_repo.refs().get_thread(&ThreadName::new(track_name))? else {
            continue;
        };
        if let Some(git_oid) = frontier_git_oid(heddle_repo, mapping, tip)? {
            desired.insert(format!("refs/heads/{track_name}"), git_oid);
        }
    }
    for marker_name in markers {
        let Some(state_id) = heddle_repo.refs().get_marker(marker_name)? else {
            continue;
        };
        if let Some(git_oid) = mapping.get_git(&state_id) {
            desired.insert(format!("refs/tags/{marker_name}"), git_oid);
        }
    }
    Ok(desired)
}

/// The Git OID the public branch should lag to for a thread whose raw tip is
/// `tip`: the maximal **served** ancestor-or-self of `tip`. A state is served
/// iff it is present in the mapping — `purge_unserved_mappings` runs first to
/// drop any mapped-but-now-embargoed state (and its descendants), so the mapped
/// set is exactly the served set. Returns `None` when no ancestor of `tip` is
/// served (the whole line is embargoed to its root → absence).
fn frontier_git_oid(
    heddle_repo: &HeddleRepository,
    mapping: &SyncMapping,
    tip: ChangeId,
) -> GitResult<Option<gix::hash::ObjectId>> {
    let mut visited = HashSet::new();
    let mut stack = vec![tip];
    let mut frontier: Vec<ChangeId> = Vec::new();
    while let Some(id) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        // Stop at the first served (mapped) state on each downward path: that
        // is a maximal served ancestor — its own served ancestors are
        // dominated by it, so we do not descend past it.
        if mapping.get_git(&id).is_some() {
            frontier.push(id);
            continue;
        }
        if let Some(state) = heddle_repo.store().get_state(&id)? {
            stack.extend(state.parents.iter().copied());
        }
    }
    // A linear thread yields exactly one maximal served state. A merge whose
    // embargo splits the DAG can leave an antichain of ≥2 maximal served
    // states; advertising each sibling line under its own ref is the
    // multi-root work deferred to issues #4/#5. Until then the branch lags
    // deterministically (lowest ChangeId) — never published from a raw
    // embargoed tip — and the other lines are absent from this branch.
    let chosen = frontier.into_iter().min_by_key(|c| c.to_string_full());
    Ok(chosen.and_then(|c| mapping.get_git(&c)))
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
