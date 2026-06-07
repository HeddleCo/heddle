// SPDX-License-Identifier: Apache-2.0
//! Export Heddle states to Git commits functionality.

use objects::store::ObjectStore;
use std::collections::HashSet;

use gix::bstr::ByteSlice;
use gix::refs::transaction::PreviousValue;
use objects::{
    error::HeddleError,
    object::{ChangeId, ContentHash, FileMode, MarkerName, Principal, State, ThreadName},
};
use repo::{AudienceTier, Repository as HeddleRepository, visible};

use crate::bridge::{
    git_core::{
        GitBridge, GitBridgeError, GitResult, LocalGitIdentity, SyncMapping,
        count_exported_commits, delete_reference_if_present,
        git_config_identity_with_global_fallback, git_err, principal_is_default_unknown,
        read_or_seed_mirror_managed_refs, set_reference, write_mirror_managed_refs,
    },
    git_notes,
    git_reconstruct::{commit_object_id, reconstruct_commit_bytes, write_commit_object},
    git_sync::{sync_marker_to_tag, sync_track_to_branch},
    git_util::{ExportStats, ExportedRef},
};

const SUBMODULE_PREFIX: &str = "heddle-submodule:";

/// Whether `state` carries a captured original git commit to reconstruct
/// byte-exactly (the #565 de-lossy fidelity fields). When true, export
/// regenerates the commit object from state via [`reconstruct_commit_bytes`]
/// with NO W2 footer and NO `"No intent specified"` placeholder — any injected
/// byte would push the minted object off the original SHA (#567). When false
/// (a native heddle commit, no original to preserve), export mints with the
/// footer/placeholder as before.
///
/// `raw_message` is the load-bearing signal: the git importer always records it
/// (even as an empty body for an empty-message commit) for an imported commit,
/// and never for a native one.
fn has_git_fidelity(state: &State) -> bool {
    state.raw_message.is_some()
}

/// Whether `who`'s name/email round-trip byte-exactly through reconstruction.
/// `Principal.name/email` are `String`, so the git importer replaced any non-UTF8
/// identity byte with U+FFFD when it called `to_string()` on the raw actor bytes
/// (the #565-deferred gap; `Principal` is still `String`, see #564). Those
/// replaced bytes can't be regenerated, so reconstruction would hash off the
/// original SHA. A literal U+FFFD that was itself valid UTF-8 in the original
/// survives fine — so this can only FALSE-POSITIVE into the safe verbatim
/// fallback, never a wrong-SHA mint.
fn identity_is_byte_faithful(who: &Principal) -> bool {
    !who.name.contains('\u{FFFD}') && !who.email.contains('\u{FFFD}')
}

/// Whether reconstructing `state`'s commit object from Heddle state alone is
/// guaranteed byte-exact to the original commit — the precondition for the #567
/// reconstruct-from-state path. False for the two #564 lossy gaps:
///   1. non-UTF8 author/committer identity bytes (see [`identity_is_byte_faithful`]);
///   2. lossy imports, where unrepresentable tree entries were dropped/converted
///      so the rebuilt tree — hence commit — OID diverges.
///
/// (2) is read off ONE canonical signal — [`State::git_lossy`] — that BOTH lossy
/// population paths (`bridge git import --lossy` AND `bridge git ingest --lossy`)
/// set, rather than enumerating import surfaces or keying off the mirror's
/// per-OID lossy log. That log is unreachable for an UNMAPPED state (an ingest
/// import never populates the bridge mapping), which is exactly the gap that let
/// an ingest-lossy commit reconstruct to a wrong SHA (#567 round 2); the state
/// flag closes the whole class, including any future lossy entry point.
///
/// When false the caller MUST keep the verbatim mirror bytes / preserved mapped
/// OID (or fall through to the native mint) rather than mint a wrong-SHA
/// reconstructed object.
fn commit_is_byte_faithful(state: &State) -> bool {
    has_git_fidelity(state)
        && !state.git_lossy
        && identity_is_byte_faithful(&state.attribution.principal)
        && state
            .committer
            .as_ref()
            .map(identity_is_byte_faithful)
            .unwrap_or(true)
}

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

    // Fidelity mint (#567): the state carries a captured original git commit
    // (#565 fields — `raw_message` is the load-bearing signal). MINT the commit
    // object from that raw metadata via `reconstruct_commit_bytes` — NO footer,
    // NO placeholder, NO message override — so the minted bytes preserve the
    // original message/identities/headers rather than the native intent+footer.
    // This is the path that lets the git mirror be dropped (#568): a correct
    // export no longer depends on the mirror holding the verbatim imported bytes.
    //
    // Routing (#567 round 3): export keys off (is byte-faithful?) AND (does a
    // bridge mapping exist?). The verbatim / mapped-OID fallback for a lossy
    // commit applies ONLY when a bridge mapping holds a TRACKED original OID to
    // preserve — and that branch lives in `export_scoped`'s already-mapped path.
    // `export_state` is only ever reached for an UNMAPPED state (the caller's
    // `has_heddle` guard), so there is NO original OID to match and NO verbatim
    // mirror bytes to fall back to. Every unmapped fidelity state therefore MINTS
    // from its own raw metadata — a `--lossy` one is NOT rejected into a
    // nonexistent verbatim source (the r2 over-correction, #567 round 3):
    //   * byte-faithful (a clean `bridge git ingest`, native heddle commit with
    //     fidelity, ...) → the derived OID coincides with the original commit SHA;
    //   * lossy / non-UTF8 (`bridge git ingest --lossy`) → a DERIVED OID that
    //     still preserves raw_message/identities/headers. With no original to
    //     match this is correct, not the wrong-SHA bug the r2 `git_lossy` guard
    //     (rightly) blocks ONLY for a MAPPED commit.
    if has_git_fidelity(&state) {
        let content = reconstruct_commit_bytes(heddle_repo, repo, mapping, &state)?;
        return Ok(Some(write_commit_object(repo, &content)?));
    }

    // Native heddle commit: no original to preserve. Mint via `new_commit_as`
    // and inject the durable W2 footer (and the "No intent specified"
    // placeholder for an empty intent) — these ride ONLY native commits.
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
    // The purge MUTATES the mapping down to the served set. Its returned drop-set
    // (the OIDs THIS run withheld) is deliberately NOT used to classify EXISTING
    // mirror tips: a scoped run's purge omits a tip embargoed in a PRIOR run, or
    // out of this run's purge reach, so classifying by it misreads such a tip as
    // served and keeps serving it. Existing-tip served classification (heads + tags
    // below) uses the whole-mirror served-OID set (`served_oids`) instead
    // (heddle#316).
    purge_unserved_mappings(
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
        // Already mapped to a git object — the common case for git-imported
        // states (the import populated the ChangeId→OID mapping) and for
        // native commits a prior export already minted. Not re-counted as
        // "newly minted" (the total is decided below by ref-reachability).
        if bridge.mapping.has_heddle(&state_id) {
            // For an IMPORTED commit (#565 fidelity fields present),
            // REGENERATE the object from state into the mirror rather than
            // leaning on the verbatim imported bytes still being there (#567).
            // Byte-identical, so the OID is unchanged and the write is
            // idempotent today; what changes is that a correct export no
            // longer DEPENDS on the mirror's verbatim copy — the step that
            // lets the mirror be dropped (#568). Native already-mapped commits
            // have no original to reconstruct (raw_message is None), so they
            // are left to their prior mint; re-minting those is out of scope.
            if let Some(state) = bridge.heddle_repo.store().get_state(&state_id)?
                && has_git_fidelity(&state)
            {
                let mapped = bridge.mapping.get_git(&state_id);
                // mirror still required for non-byte-faithful commits (non-UTF8
                // identities, --lossy); #568 mirror elimination must account for
                // these, and full de-lossy needs byte-preserving identities (#564
                // follow-up).
                // Fidelity guard (#567): regenerate from state ONLY when the
                // state is fully byte-faithful to the original import. A
                // non-byte-faithful commit (non-UTF8 identity, or a `--lossy`
                // import — both import-lossy and ingest-lossy carry the canonical
                // `git_lossy` flag) would reconstruct to a WRONG SHA, so leave it
                // on the preserved mapped OID — the verbatim mirror bytes stay the
                // served object (the pre-#567 behavior for that commit).
                if commit_is_byte_faithful(&state) {
                    let content = reconstruct_commit_bytes(
                        bridge.heddle_repo,
                        &repo,
                        &bridge.mapping,
                        &state,
                    )?;
                    // Safety net: the regenerated object MUST hash to the mapped
                    // OID. A mismatch means reconstruction diverged from the
                    // imported bytes (an undetected fidelity gap), so fall back to
                    // the verbatim mirror / mapped OID rather than write a
                    // wrong-SHA object.
                    let reconstructed = commit_object_id(&content);
                    if mapped.map(|m| m == reconstructed).unwrap_or(true) {
                        write_commit_object(&repo, &content)?;
                    }
                }
            }
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

    // The downward-closure served set over the WHOLE-MIRROR frontier — the SAME
    // closure the purge ran over (every thread tip + every marker state). A state is
    // served iff visible to this audience AND every reachable ancestor is served.
    // Drives BOTH the served-OID set just below AND (further down) the tag
    // classifier's served-but-unminted axis.
    let frontier_served = {
        let reachable_set: HashSet<ChangeId> = frontier_reachable.iter().copied().collect();
        let sorted = bridge.sort_states_topologically(&frontier_reachable)?;
        served_change_ids(bridge.heddle_repo, &sorted, &reachable_set, &audience)?
    };

    // The whole-mirror SERVED-OID set: the git OID of every served frontier state.
    // An EXISTING mirror tip (head or tag) is "served" iff it is one of these — an
    // actually-served commit RIGHT NOW — independent of whether THIS run's purge
    // happened to drop it. `frontier_served` is downward-closed at the ChangeId
    // level (served ⟹ every reachable ancestor served) and every minted commit's
    // parents are themselves mapped, so the mapped OIDs of `frontier_served` already
    // form the downward-closed git-ancestry set — no separate git walk is needed
    // (heddle#316). Replaces the prior `embargoed_oids` (this-run-only purge
    // drop-set) classification that leaked a prior-run / out-of-scope embargo.
    let served_oids: HashSet<gix::hash::ObjectId> = frontier_served
        .iter()
        .filter_map(|state| bridge.mapping.get_git(state))
        .collect();

    // The mirror's NAME-KEYED ownership record (heddle#316): a mirror ref is
    // MANAGED iff heddle recorded WRITING it under that full name — NEVER by OID
    // membership (the r20c bug that classified a foreign ref at a heddle OID as
    // heddle's). The mirror analog of the destination's `heddle-exported-refs`
    // record. Read BEFORE the head/tag loops mutate any ref so a genuine first run
    // (absent record) seeds from the prior-run ref set rather than misreading every
    // pre-existing ref as foreign — which would silently stop embargo retraction.
    let mut managed_record = read_or_seed_mirror_managed_refs(&repo)?;

    // Reconcile the mirror's HEADS via the shared `reconcile_ref` decision. Iterate
    // the CURRENT threads: a dropped thread's stale branch is intentionally NOT
    // pruned (the #289 dropped-thread contract) — it is never iterated, survives in
    // the mirror, and stays in the managed record so the push still copies it. The
    // desired head target is the maximal served ancestor-or-self of the thread tip
    // (`frontier_git_oid`, via `project_desired_refs`). The existing tip is
    // classified against the whole-mirror served-OID set, so a still-served tip
    // fast-forwards, an embargoed tip force-rewinds to its served ancestor, and a
    // whole-line-embargoed head is deleted. A scoped export reconciles every current
    // thread but MATERIALIZES (creates) only the one it was scoped to.
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
        let desired_oid = desired.get(&branch_ref).copied();
        let existing_oid = branch_tip_oid(&repo, &branch_ref);
        match reconcile_ref(
            ReconcileNs::Head,
            desired_oid,
            existing_oid,
            in_scope,
            /* marker_served_unminted */ false,
            &served_oids,
        ) {
            ReconcileOp::Write => {
                let git_oid = desired_oid.expect("Write implies a desired target");
                sync_track_to_branch(&repo, track_name, git_oid)?;
                managed_record.insert(branch_ref.clone(), git_oid);
                stats.threads_synced += 1;
                stats.branches.push(ExportedRef {
                    name: track_name.clone(),
                    tip: git_oid,
                });
            }
            ReconcileOp::ForceRewind => {
                let git_oid = desired_oid.expect("ForceRewind implies a desired target");
                set_reference(
                    &repo,
                    &branch_ref,
                    git_oid,
                    PreviousValue::Any,
                    "heddle: retract embargoed thread frontier",
                )?;
                managed_record.insert(branch_ref.clone(), git_oid);
                stats.threads_synced += 1;
                stats.branches.push(ExportedRef {
                    name: track_name.clone(),
                    tip: git_oid,
                });
            }
            ReconcileOp::Delete => {
                delete_reference_if_present(&repo, &branch_ref)?;
                managed_record.remove(&branch_ref);
            }
            // A head has no preserve path — `frontier_git_oid` recomputes the
            // target every run, so a head is always rewound/deleted, never kept at
            // a stale tip (Preserve is unreachable for `ReconcileNs::Head`).
            ReconcileOp::Skip | ReconcileOp::Preserve => {}
        }
    }

    // Reconcile the mirror's TAGS via the SAME `reconcile_ref` decision as heads.
    // Iterate the UNION of current markers AND the managed-record tag names: a
    // DELETED marker drops out of `markers`, so its stale managed mirror tag is
    // reachable only via the managed-record side (heddle#316 S3 — a deleted marker
    // must delete its tag). A FOREIGN tag heddle never wrote is in NEITHER set, so
    // it is never visited: it survives untouched and stays out of the push frontier
    // (`collect_managed_ref_updates`). The desired tag target comes from the
    // projection (a marker minted this run); the served-but-unminted vs embargoed
    // split (r18 PRESERVE vs r19 DELETE) is the existing tag's served-ness combined
    // with `marker_served_unminted`.
    let mut tag_names: std::collections::BTreeSet<String> =
        markers.iter().map(|m| m.to_string()).collect();
    for full_name in managed_record.keys() {
        if let Some(tag) = full_name.strip_prefix("refs/tags/") {
            tag_names.insert(tag.to_string());
        }
    }

    for name in &tag_names {
        let tag_ref = format!("refs/tags/{name}");
        let existing_oid = branch_tip_oid(&repo, &tag_ref);
        let desired_oid = desired.get(&tag_ref).copied();
        let in_scope = thread.is_none();
        // A live marker whose served target was NOT minted into the mapping this
        // run (a scoped export that didn't reach it). The desired projection omits
        // such a tag (it only publishes minted markers), so the reconcile sees
        // `desired_oid == None`; this flag plus the existing tag's served-ness is
        // the sole axis splitting r18-PRESERVE from r19-DELETE.
        let marker_served_unminted = match bridge
            .heddle_repo
            .refs()
            .get_marker(&MarkerName::new(name.as_str()))?
        {
            Some(state) => {
                bridge.mapping.get_git(&state).is_none() && frontier_served.contains(&state)
            }
            None => false,
        };
        match reconcile_ref(
            ReconcileNs::Tag,
            desired_oid,
            existing_oid,
            in_scope,
            marker_served_unminted,
            &served_oids,
        ) {
            ReconcileOp::Write => {
                let git_oid = desired_oid.expect("Write implies a desired target");
                sync_marker_to_tag(&repo, name, git_oid)?;
                managed_record.insert(tag_ref.clone(), git_oid);
                stats.markers_synced += 1;
                stats.tags.push(ExportedRef {
                    name: name.clone(),
                    tip: git_oid,
                });
            }
            ReconcileOp::Delete => {
                delete_reference_if_present(&repo, &tag_ref)?;
                managed_record.remove(&tag_ref);
            }
            // PRESERVE keeps the existing served tag (still managed → stays in the
            // record); SKIP is a no-op. A tag is free-move and never force-rewinds
            // (ForceRewind is unreachable for `ReconcileNs::Tag`).
            ReconcileOp::Preserve | ReconcileOp::Skip | ReconcileOp::ForceRewind => {}
        }
    }

    // Persist the updated ownership record so the next reconcile — and the push
    // frontier (`collect_managed_ref_updates`) — read heddle's managed set by name.
    write_mirror_managed_refs(&repo, &managed_record)?;

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

/// Which namespace a reconciled mirror ref lives in. The reconcile DECISION is
/// one shape for both; the only namespace-specific axis is how "write the desired
/// target" lands — a head is fast-forward-guarded (and force-rewound for an
/// embargo retract), a tag is free-move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileNs {
    Head,
    Tag,
}

/// The op the mirror reconcile applies to a single ref. The SINGLE decision the
/// head and tag reconciles share (heddle#316): a foreign ref never reaches here
/// (the iteration set is current threads/markers ∪ heddle-managed names), so every
/// arm acts on a ref heddle owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileOp {
    /// Nothing to do — a scoped export declining to materialize an out-of-scope
    /// ref, or a genuine no-op (no desired target and nothing to retract).
    Skip,
    /// Write the desired target through the namespace's guarded path: a head
    /// fast-forwards (or creates); a tag force-retargets (or creates).
    Write,
    /// Force-set a head to the desired target past the fast-forward guard — the
    /// embargo retract that rewinds an embargoed tip to its served ancestor.
    ForceRewind,
    /// Keep an existing served tag whose marker target is served-but-unminted this
    /// run (r18). A later all-thread export re-mints and advances it.
    Preserve,
    /// Delete the ref — its line/marker has no served frontier (whole-line embargo,
    /// r19 embargoed-existing tag, or a deleted marker's stale tag).
    Delete,
}

/// The mirror reconcile decision — IDENTICAL in shape for heads and tags
/// (heddle#316). `desired_oid` is the served target the projection wants published
/// (`None` ⇒ nothing served for this ref this run); `existing_oid` is the mirror
/// ref's CURRENT tip, already PEELED to a commit by [`branch_tip_oid`] (so an
/// annotated foreign tag colliding with a marker name is tested by its commit, not
/// its tag-object OID — heddle#316 risk #2). `in_scope` gates only
/// MATERIALIZATION: a scoped export reconciles existing refs but never CREATES a
/// brand-new one the caller did not ask for. `marker_served_unminted` is set only
/// for a tag whose live marker target is served but was not minted this run — the
/// sole axis that, combined with `existing_served`, splits r18-PRESERVE from
/// r19-DELETE. `served_oids` is the whole-mirror served-OID set classifying the
/// existing tip (NOT this run's purge drop-set, which omits a prior-run /
/// out-of-scope embargo).
fn reconcile_ref(
    ns: ReconcileNs,
    desired_oid: Option<gix::hash::ObjectId>,
    existing_oid: Option<gix::hash::ObjectId>,
    in_scope: bool,
    marker_served_unminted: bool,
    served_oids: &HashSet<gix::hash::ObjectId>,
) -> ReconcileOp {
    // `existing_oid` is already the peeled commit OID (`branch_tip_oid`), so this
    // membership test compares commit-against-commit (risk #2).
    let existing_served = existing_oid
        .map(|oid| served_oids.contains(&oid))
        .unwrap_or(false);
    match (desired_oid, existing_oid) {
        // Scoped export, would-create: never materialize a ref the caller did not
        // ask to export.
        (Some(_), None) if !in_scope => ReconcileOp::Skip,
        // Create a fresh ref at the served target.
        (Some(_), None) => ReconcileOp::Write,
        // Head with an existing tip: a still-served tip fast-forwards (r17 FF guard
        // applies); an embargoed tip is force-rewound to its served ancestor.
        (Some(_), Some(_)) if ns == ReconcileNs::Head => {
            if existing_served {
                ReconcileOp::Write
            } else {
                ReconcileOp::ForceRewind
            }
        }
        // Tag with an existing tip: free-move force-retarget to the served target.
        (Some(_), Some(_)) => ReconcileOp::Write,
        // Nothing served, nothing present.
        (None, None) => ReconcileOp::Skip,
        // Nothing served, but a tag exists whose marker target is served-but-
        // unminted AND the existing tag is itself served: PRESERVE (r18).
        (None, Some(_)) if marker_served_unminted && existing_served => ReconcileOp::Preserve,
        // Nothing served, an existing ref remains: DELETE (whole-line embargo, r19
        // embargoed existing tag, or a deleted marker's stale tag).
        (None, Some(_)) => ReconcileOp::Delete,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use objects::object::{Attribution, ContentHash, Principal, State};

    fn fidelity_state() -> State {
        State::new(
            ContentHash::from_bytes([7u8; 32]),
            vec![],
            Attribution::human(Principal::new("Alice", "alice@example.com")),
        )
        .with_raw_message("an imported commit\n")
    }

    /// The fidelity guard reconstructs a byte-faithful imported commit.
    #[test]
    fn byte_faithful_when_fidelity_present_and_not_lossy() {
        assert!(commit_is_byte_faithful(&fidelity_state()));
    }

    /// The canonical `git_lossy` marker — set by BOTH `import --lossy` and
    /// `ingest --lossy` — routes the commit OFF the reconstruct path regardless
    /// of which import surface produced it. A lossy import drops/converts tree
    /// entries, so reconstructing from state would mint a wrong SHA.
    #[test]
    fn lossy_marker_blocks_reconstruction() {
        let lossy = fidelity_state().with_git_lossy(true);
        assert!(
            !commit_is_byte_faithful(&lossy),
            "a state carrying the canonical git_lossy marker must NOT be \
             reconstructed from state, regardless of import surface"
        );
    }
}
