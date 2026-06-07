// SPDX-License-Identifier: Apache-2.0
//! Import Git commits into Heddle states functionality.

use objects::store::ObjectStore;
use std::{collections::HashSet, path::Path};

use chrono::{TimeZone, Utc};
use objects::object::{
    Agent, Attribution, ChangeId, MarkerName, Principal, State, Status, ThreadName,
    parse_commit_extension_headers,
};
use refs::{Head, RefExpectation};
use repo::{Repository as HeddleRepository, ThreadId};
use tracing::warn;

pub use super::git_import_tree::{GitTreeImporter, import_git_tree};
use super::git_import_tree::{PackImportSink, fail_lossy_entry};
use crate::bridge::{
    git_core::{
        GitBridge, GitBridgeError, GitResult, RefNamespace, RefUpdate, SyncMapping,
        apply_ref_updates, copy_reachable_objects, git_err, open_repo, parse_git_ref,
        thread_is_unclaimed_bootstrap,
    },
    git_notes,
    git_util::{GitImportOptions, ImportStats, PartialMirrorRef, SkippedRef},
};

/// One source ref the import will consider, with both its immediate target
/// (the OID stored on disk for that ref — for annotated tags this is the
/// tag *object* OID) and the peeled commit OID we use to walk ancestry.
///
/// Keeping both is what lets the bridge round-trip annotated tags as actual
/// tag objects: we copy the tag object into the mirror and write the
/// mirror's ref pointing at it, and later `sync_marker_to_tag`'s
/// already-exists check sees the existing ref peel to the right commit and
/// preserves the annotated form unchanged.
struct RefPlan {
    short_name: String,
    namespace: RefNamespace,
    /// The OID the source ref points at directly. For lightweight tags
    /// and branches this is a commit; for annotated tags it's a tag
    /// object that wraps a commit.
    immediate_oid: gix::hash::ObjectId,
    /// The commit reachable by peeling `immediate_oid` through any tag
    /// chain. Used as a tip for ancestry walking.
    peeled_commit_oid: gix::hash::ObjectId,
}

/// Peel `reference` to its final OID and confirm the OID is a commit. If
/// it's a blob (e.g. `git/git`'s `refs/tags/junio-gpg-pub` pointing at a
/// GPG public key), a tree (e.g. `git-lfs`'s `refs/tags/core-gpg-keys`),
/// or anything else, return `Ok(None)`. The caller is expected to log
/// and record the skip via `SkippedRef`.
///
/// Heddle's marker model currently requires the target to be a commit;
/// the long-term fix is a `MarkerTarget::NonCommitRef { peeled_oid,
/// peeled_kind }` variant that round-trips losslessly. Until that lands,
/// this guard prevents the import from crashing on the very common
/// "tag-points-at-non-commit-blob" pattern in mature OSS repos.
fn peel_to_commit_oid(
    repo: &gix::Repository,
    reference: &mut gix::Reference,
) -> GitResult<Result<gix::hash::ObjectId, gix::objs::Kind>> {
    let oid = reference.peel_to_id().map_err(git_err)?.detach();
    let object = repo.find_object(oid).map_err(git_err)?;
    if object.kind == gix::objs::Kind::Commit {
        Ok(Ok(oid))
    } else {
        Ok(Err(object.kind))
    }
}

fn remote_tracking_ref_suggestions(
    repo: &gix::Repository,
    missing: &[String],
) -> GitResult<Vec<String>> {
    let missing = missing.iter().map(String::as_str).collect::<HashSet<_>>();
    let mut suggestions = Vec::new();

    for reference in repo
        .references()
        .map_err(git_err)?
        .prefixed("refs/remotes/")
        .map_err(git_err)?
    {
        let mut reference = reference.map_err(git_err)?;
        let Some(_) = reference.target().try_id() else {
            continue;
        };
        let full = reference.name().as_bstr().to_string();
        let short = reference.name().shorten().to_string();
        if short.ends_with("/HEAD") {
            continue;
        }
        if peel_to_commit_oid(repo, &mut reference)?.is_err() {
            continue;
        }
        let Some(parsed) = parse_git_ref(&full) else {
            continue;
        };
        if missing.contains(parsed.name) {
            suggestions.push(format!(
                "Remote-tracking branch '{short}' exists. Import it with `heddle bridge git import --ref {short}`. If you want a local branch with the shorter name later, create it in Heddle and sync it back through `heddle push`."
            ));
        }
    }

    suggestions.sort();
    suggestions.dedup();
    Ok(suggestions)
}

/// Resolve a heddle change_id for a git commit. Tried in order:
///   1. **Sidecar mapping** (already loaded into `mapping`): if the git_oid
///      is already known, reuse the change_id without scanning anything.
///   2. **`refs/notes/heddle`**: if a note attached to this commit carries
///      a change_id, adopt it. This is how identity survives a fresh
///      `git clone` of a heddle-exported repo.
///   3. **Legacy `Heddle-Change-Id:` trailer**: kept for backward
///      compatibility with commits exported by pre-Phase-B builds.
///   4. **Deterministic from git SHA**: the original heddle behavior —
///      take the first 16 bytes of the git SHA. Two heddle repos that
///      independently import the same git commit get the same change_id,
///      which is what we want.
fn resolve_identity(
    mapping: &SyncMapping,
    repo: &gix::Repository,
    git_oid: gix::hash::ObjectId,
    trailers: &std::collections::HashMap<String, String>,
) -> GitResult<(ChangeId, Option<git_notes::HeddleNote>)> {
    if let Some(existing) = mapping.get_heddle(git_oid) {
        return Ok((existing, None));
    }
    if let Some(note) = git_notes::read_note(repo, git_oid)? {
        let change_id = ChangeId::parse(&note.change_id)?;
        return Ok((change_id, Some(note)));
    }
    if let Some(id_str) = trailers.get(GitBridge::TRAILER_CHANGE_ID) {
        return Ok((ChangeId::parse(id_str)?, None));
    }
    let oid_hex = git_oid.to_hex_with_len(40).to_string();
    let bytes = hex::decode(&oid_hex[..32])
        .map_err(|err| GitBridgeError::InvalidMapping(err.to_string()))?;
    let mut change_id_bytes = [0u8; 16];
    change_id_bytes.copy_from_slice(&bytes);
    Ok((ChangeId::from_bytes(change_id_bytes), None))
}

/// Collect a commit's extension headers in their original on-the-wire order,
/// as raw bytes so non-UTF8 header values survive. ORDER IS LOAD-BEARING for
/// #566 byte-exactness.
///
/// Built straight from the raw commit object bytes (`commit.data`) via
/// [`parse_commit_extension_headers`] so `encoding` / `gpgsig` / `mergetag` /
/// any unknown header all land at their TRUE captured position through one code
/// path. We deliberately do NOT stitch the vec from gix's typed accessors
/// (`CommitRef::encoding`, …): gix surfaces some headers as typed fields outside
/// `extra_headers`, and re-inserting them by hand reorders them — the close-the-
/// class bug this replaces. The raw header block is the source of truth.
/// #564 de-lossy step 1.
fn collect_extra_headers(commit: &gix::Commit<'_>) -> GitResult<Vec<(Vec<u8>, Vec<u8>)>> {
    Ok(parse_commit_extension_headers(&commit.data))
}

/// Import a single Git commit as a Heddle state.
pub fn import_commit(
    mapping: &mut SyncMapping,
    repo: &gix::Repository,
    tree_importer: &mut GitTreeImporter<'_>,
    git_oid: gix::hash::ObjectId,
) -> GitResult<ChangeId> {
    let commit = repo.find_commit(git_oid).map_err(git_err)?;
    // Capture the raw message bytes verbatim for byte-exact reconstruction
    // (#566): a non-UTF8 message (latin-1, shift-jis, …) must survive intact,
    // so store bytes, not a String. A lossy String view is derived only for
    // trailer / intent parsing, which inspect the (ASCII) footer lines.
    let message_bytes = commit.message_raw_sloppy().to_vec();
    let message = String::from_utf8_lossy(&message_bytes).into_owned();
    let author = commit.author().map_err(git_err)?;
    let author_name = author.name.to_string();
    let author_email = author.email.to_string();
    let author_time = author.time().map_err(git_err)?;
    let authored_seconds = author_time.seconds;
    let authored_tz_offset = author_time.offset;
    // #565: also capture the committer identity + time. Git records both an
    // author (who wrote the change) and a committer (who created this commit
    // object); the two differ for rebased / cherry-picked / amended commits.
    let committer = commit.committer().map_err(git_err)?;
    let committer_name = committer.name.to_string();
    let committer_email = committer.email.to_string();
    let committer_time = committer.time().map_err(git_err)?;
    let committed_seconds = committer_time.seconds;
    let committer_tz_offset = committer_time.offset;
    // #565: capture all extension headers in true wire order (encoding /
    // gpgsig / mergetag / unknown all at their captured position) so the commit
    // is byte-reconstructable later (#566) without the git mirror (#568).
    let extra_headers = collect_extra_headers(&commit)?;
    let tree_id = commit.tree_id().map_err(git_err)?.detach();
    let parent_git_oids: Vec<gix::hash::ObjectId> =
        commit.parent_ids().map(|id| id.detach()).collect();

    let trailers = GitBridge::parse_trailers(&message);
    let (change_id, note) = resolve_identity(mapping, repo, git_oid, &trailers)?;

    let parent_oids: Vec<ChangeId> = parent_git_oids
        .iter()
        .map(|parent_oid| {
            mapping
                .get_heddle(*parent_oid)
                .ok_or_else(|| GitBridgeError::CommitNotFound(parent_oid.to_string()))
        })
        .collect::<GitResult<Vec<_>>>()?;

    // Canonical lossy marker (#567): if importing this commit's tree dropped or
    // converted any unrepresentable entry, the rebuilt tree no longer hashes to
    // the original, so record it on the State. `import_tree` appends to the
    // importer's running lossy-entry log (even for cached subtrees), so a growth
    // across this call means this commit's content is lossy.
    let lossy_before = tree_importer.lossy_entries().len();
    let tree_hash = tree_importer.import_tree(tree_id)?;
    let git_lossy = tree_importer.lossy_entries().len() > lossy_before;

    let principal = Principal::new(author_name, author_email);

    // Agent / confidence / status: prefer the note (Phase-B-and-later format)
    // and fall back to legacy trailers for pre-Phase-B history.
    let agent = note
        .as_ref()
        .and_then(|n| n.agent.as_ref())
        .map(|a| Agent::new(a.provider.clone(), a.model.clone()))
        .or_else(|| {
            trailers
                .get(GitBridge::TRAILER_AGENT)
                .and_then(|agent_str| {
                    let parts: Vec<&str> = agent_str.split('/').collect();
                    if parts.len() == 2 {
                        Some(Agent::new(parts[0], parts[1]))
                    } else {
                        None
                    }
                })
        });

    let attribution = if let Some(agent) = agent {
        Attribution::with_agent(principal, agent)
    } else {
        Attribution::human(principal)
    };

    let intent = GitBridge::extract_intent(&message);
    let confidence = note.as_ref().and_then(|n| n.confidence).or_else(|| {
        trailers
            .get(GitBridge::TRAILER_CONFIDENCE)
            .and_then(|c| c.parse::<f32>().ok())
            .map(|c| c.clamp(0.0, 1.0))
    });
    let status = note
        .as_ref()
        .map(|n| match n.status.as_str() {
            "published" => Status::Published,
            _ => Status::Draft,
        })
        .or_else(|| {
            trailers
                .get(GitBridge::TRAILER_STATUS)
                .map(|s| match s.as_str() {
                    "published" => Status::Published,
                    _ => Status::Draft,
                })
        })
        .unwrap_or(Status::Draft);

    // #565: `created_at` is the *committer* time (the commit object's
    // birth) and `authored_at` is the *author* time, matching the
    // `bridge git ingest` importer. Previously this path stored the author
    // time as `created_at` and dropped the committer entirely, so the
    // committer time wasn't recoverable; fidelity requires both.
    let created_at = Utc.timestamp_opt(committed_seconds, 0).single().ok_or_else(|| {
        GitBridgeError::InvalidMapping(format!("invalid Git timestamp: {}", committed_seconds))
    })?;
    let authored_at = Utc.timestamp_opt(authored_seconds, 0).single().ok_or_else(|| {
        GitBridgeError::InvalidMapping(format!("invalid Git timestamp: {}", authored_seconds))
    })?;

    let state = State::new(tree_hash, parent_oids, attribution)
        .with_change_id(change_id)
        .with_intent(intent.unwrap_or_else(|| "Imported from Git".to_string()))
        .with_timestamp(created_at)
        .with_authored_at(authored_at)
        .with_committer(Principal::new(committer_name, committer_email))
        .with_tz_offsets(authored_tz_offset, committer_tz_offset)
        .with_raw_message(message_bytes)
        .with_git_lossy(git_lossy)
        .with_extra_headers(extra_headers)
        .with_status(status);

    let state = if let Some(c) = confidence {
        state.with_confidence(c)
    } else {
        state
    };

    tree_importer.write_state(&state)?;

    Ok(change_id)
}

/// Import Git commits into Heddle states.
pub fn import_all(bridge: &mut GitBridge, git_path: Option<&Path>) -> GitResult<ImportStats> {
    import_with_ref_filter(bridge, git_path, None, GitImportOptions::default(), None)
}

pub fn import_all_with_options(
    bridge: &mut GitBridge,
    git_path: Option<&Path>,
    options: GitImportOptions,
) -> GitResult<ImportStats> {
    import_with_ref_filter(bridge, git_path, None, options, None)
}

/// Like [`import_all`], reporting the running commit count to `progress`
/// after each commit is walked (drives the adopt progress indicator).
pub fn import_all_with_progress(
    bridge: &mut GitBridge,
    git_path: Option<&Path>,
    progress: Option<&mut dyn FnMut(usize)>,
) -> GitResult<ImportStats> {
    import_with_ref_filter(bridge, git_path, None, GitImportOptions::default(), progress)
}

pub fn import_selected_refs(
    bridge: &mut GitBridge,
    git_path: Option<&Path>,
    refs: &[String],
) -> GitResult<ImportStats> {
    let wanted = refs.iter().cloned().collect::<HashSet<_>>();
    import_with_ref_filter(
        bridge,
        git_path,
        Some(&wanted),
        GitImportOptions::default(),
        None,
    )
}

pub fn import_selected_refs_with_options(
    bridge: &mut GitBridge,
    git_path: Option<&Path>,
    refs: &[String],
    options: GitImportOptions,
) -> GitResult<ImportStats> {
    let wanted = refs.iter().cloned().collect::<HashSet<_>>();
    import_with_ref_filter(bridge, git_path, Some(&wanted), options, None)
}

/// Like [`import_selected_refs`], reporting the running commit count to
/// `progress` after each commit is walked.
pub fn import_selected_refs_with_progress(
    bridge: &mut GitBridge,
    git_path: Option<&Path>,
    refs: &[String],
    progress: Option<&mut dyn FnMut(usize)>,
) -> GitResult<ImportStats> {
    let wanted = refs.iter().cloned().collect::<HashSet<_>>();
    import_with_ref_filter(
        bridge,
        git_path,
        Some(&wanted),
        GitImportOptions::default(),
        progress,
    )
}

fn import_with_ref_filter(
    bridge: &mut GitBridge,
    git_path: Option<&Path>,
    wanted_refs: Option<&HashSet<String>>,
    options: GitImportOptions,
    progress: Option<&mut dyn FnMut(usize)>,
) -> GitResult<ImportStats> {
    let repo = if let Some(path) = git_path {
        open_repo(path)?
    } else {
        bridge.open_git_repo()?
    };
    if repo.git_dir().join("shallow").is_file() {
        return Err(GitBridgeError::ShallowClone {
            repository: repo
                .workdir()
                .unwrap_or_else(|| repo.git_dir())
                .to_path_buf(),
            retry_command: shallow_import_retry_command(wanted_refs),
        });
    }

    let mut stats = ImportStats::default();
    let mut plans: Vec<RefPlan> = Vec::new();

    // Build per-ref plans for branches and tags. Each plan captures the
    // immediate target (annotated-tag-aware) and the peeled commit (for
    // ancestry walking). Non-commit-pointing refs are recorded in
    // `skipped_non_commit_refs` and excluded from the plan list.
    for reference in repo
        .references()
        .map_err(git_err)?
        .local_branches()
        .map_err(git_err)?
    {
        let mut reference = reference.map_err(git_err)?;
        let short = reference.name().shorten().to_string();
        if wanted_refs.is_some_and(|wanted| !wanted.contains(&short)) {
            continue;
        }
        let immediate = match reference.target().try_id() {
            Some(id) => id.to_owned(),
            None => continue, // symbolic ref (e.g. HEAD) — not a real ref to import
        };
        match peel_to_commit_oid(&repo, &mut reference)? {
            Ok(commit_oid) => plans.push(RefPlan {
                short_name: short,
                namespace: RefNamespace::Branch,
                immediate_oid: immediate,
                peeled_commit_oid: commit_oid,
            }),
            Err(kind) => {
                // A *branch* pointing at a non-commit is exceedingly rare
                // and strongly suggests upstream corruption. Record + skip.
                warn!(
                    "skipping local branch {} -> {} (not a commit, kind={:?})",
                    short, immediate, kind
                );
                stats.skipped_non_commit_refs.push(SkippedRef {
                    name: format!("refs/heads/{short}"),
                    peeled_oid: immediate.to_string(),
                    peeled_kind: format!("{kind:?}"),
                });
            }
        }
    }
    if wanted_refs.is_some() {
        for reference in repo
            .references()
            .map_err(git_err)?
            .prefixed("refs/remotes/")
            .map_err(git_err)?
        {
            let mut reference = reference.map_err(git_err)?;
            let short = reference.name().shorten().to_string();
            if short.ends_with("/HEAD") {
                continue;
            }
            if wanted_refs.is_some_and(|wanted| !wanted.contains(&short)) {
                continue;
            }
            let immediate = match reference.target().try_id() {
                Some(id) => id.to_owned(),
                None => continue,
            };
            match peel_to_commit_oid(&repo, &mut reference)? {
                Ok(commit_oid) => plans.push(RefPlan {
                    short_name: short,
                    namespace: RefNamespace::Branch,
                    immediate_oid: immediate,
                    peeled_commit_oid: commit_oid,
                }),
                Err(kind) => {
                    warn!(
                        "skipping remote-tracking branch {} -> {} (not a commit, kind={:?})",
                        short, immediate, kind
                    );
                    stats.skipped_non_commit_refs.push(SkippedRef {
                        name: format!("refs/remotes/{short}"),
                        peeled_oid: immediate.to_string(),
                        peeled_kind: format!("{kind:?}"),
                    });
                }
            }
        }
    }
    for reference in repo
        .references()
        .map_err(git_err)?
        .tags()
        .map_err(git_err)?
    {
        let mut reference = reference.map_err(git_err)?;
        let short = reference.name().shorten().to_string();
        if wanted_refs.is_some_and(|wanted| !wanted.contains(&short)) {
            continue;
        }
        let immediate = match reference.target().try_id() {
            Some(id) => id.to_owned(),
            None => continue,
        };
        match peel_to_commit_oid(&repo, &mut reference)? {
            Ok(commit_oid) => plans.push(RefPlan {
                short_name: short,
                namespace: RefNamespace::Tag,
                immediate_oid: immediate,
                peeled_commit_oid: commit_oid,
            }),
            Err(kind) => {
                // A tag pointing at a non-commit IS a real-world pattern
                // (junio-gpg-pub, core-gpg-keys, etc.). Skip with a
                // record so we don't lose track that this ref existed
                // upstream.
                warn!(
                    "skipping tag {} -> {} (not a commit, kind={:?}); \
                     non-commit-pointing tags are not yet representable in heddle's \
                     marker model",
                    short, immediate, kind
                );
                stats.skipped_non_commit_refs.push(SkippedRef {
                    name: format!("refs/tags/{short}"),
                    peeled_oid: immediate.to_string(),
                    peeled_kind: format!("{kind:?}"),
                });
            }
        }
    }

    if let Some(wanted_refs) = wanted_refs {
        let planned = plans
            .iter()
            .map(|plan| plan.short_name.clone())
            .collect::<HashSet<_>>();
        let mut missing = wanted_refs
            .iter()
            .filter(|name| !planned.contains(*name))
            .cloned()
            .collect::<Vec<_>>();
        missing.sort();
        if !missing.is_empty() {
            let mut message = format!(
                "requested ref(s) not found or not commit-pointing: {}",
                missing.join(", ")
            );
            let suggestions = remote_tracking_ref_suggestions(&repo, &missing)?;
            if !suggestions.is_empty() {
                message.push_str("\n\n");
                message.push_str(&suggestions.join("\n"));
            }
            return Err(GitBridgeError::CommitNotFound(message));
        }
    }

    // Populate the bridge mirror with the source's reachable objects AND
    // its refs verbatim (when we're importing from an external path
    // rather than the mirror itself).
    //
    // Mirror population enables two things downstream:
    //   1. **SHA-stable export**: `bridge export --destination Y`
    //      copies the original commit bytes verbatim from the mirror,
    //      so destination commits keep their original SHAs.
    //   2. **Annotated tag preservation**: writing the source ref into
    //      the mirror at its IMMEDIATE target (the tag object OID, not
    //      the peeled commit) makes the existing-ref check in
    //      `sync_marker_to_tag` skip the rewrite — leaving the
    //      annotated tag intact through to the destination push.
    //
    // We do this **per ref** rather than as a single bulk copy. A ref
    // whose ancestry references a missing object (a known failure mode
    // in real-world repos like git-lfs, where pack data carries dangling
    // references that `git fsck` doesn't catch) doesn't poison the rest
    // of the mirror — only that one ref loses SHA stability.
    if git_path.is_some() {
        bridge.init_mirror()?;
        let mirror_repo = bridge.open_git_repo()?;
        if mirror_repo.git_dir() != repo.git_dir() {
            let mut successful_updates: Vec<RefUpdate> = Vec::new();
            for plan in &plans {
                // Roots include both the immediate target (tag object for
                // annotated tags) and the peeled commit (so the walker
                // descends through commit→tree→blob even when the
                // immediate object is a tag).
                let roots = [plan.immediate_oid, plan.peeled_commit_oid];
                match copy_reachable_objects(&repo, &mirror_repo, roots) {
                    Ok(()) => successful_updates.push(RefUpdate {
                        name: plan.short_name.clone(),
                        target: plan.immediate_oid,
                        namespace: plan.namespace,
                    }),
                    Err(err) => {
                        let full = match plan.namespace {
                            RefNamespace::Branch => format!("refs/heads/{}", plan.short_name),
                            RefNamespace::Tag => format!("refs/tags/{}", plan.short_name),
                            RefNamespace::Note => format!("refs/notes/{}", plan.short_name),
                        };
                        warn!(
                            "partial mirror for {} (target {}): {}; \
                             SHA-stable export degraded for commits reachable only \
                             from this ref",
                            full, plan.immediate_oid, err
                        );
                        stats.partial_mirror_refs.push(PartialMirrorRef {
                            name: full,
                            error: err.to_string(),
                        });
                    }
                }
            }
            // Write source refs into the mirror. For annotated tags this
            // points refs/tags/<name> at the tag object (not the peeled
            // commit), which is what preserves the annotated form across
            // export.
            apply_ref_updates(
                &mirror_repo,
                &successful_updates,
                "heddle: import refs from source",
            )?;
            let note_updates = collect_note_ref_updates(&repo)?;
            if !note_updates.is_empty() {
                copy_reachable_objects(
                    &repo,
                    &mirror_repo,
                    note_updates.iter().map(|update| update.target),
                )?;
                apply_ref_updates(
                    &mirror_repo,
                    &note_updates,
                    "heddle: import Heddle notes from source",
                )?;
            }
        }
    }

    bridge.build_existing_mapping(Some(repo.path()))?;

    // heddle#555: route the bulk import through a single streaming pack (one
    // atomic install) instead of N loose objects + per-object fsync. Stage
    // under the Heddle store dir so the final install is a same-filesystem
    // rename(2). Only the write sink changes — every bridge import semantic
    // (identity recovery, annotated-tag mirror, lossy handling, divergence
    // checks, ref/tag/marker sync) is preserved by reusing the same walk.
    let staging_dir = bridge
        .heddle_repo
        .heddle_dir()
        .join("bridge-import")
        .join("staging");
    let pack_sink = PackImportSink::new(&staging_dir)?;
    let mut tree_importer = GitTreeImporter::with_options_packed(
        bridge.heddle_repo,
        &repo,
        options.clone(),
        pack_sink,
    );
    let mut noop_progress = |_: usize| {};
    let progress_cb: &mut dyn FnMut(usize) = match progress {
        Some(callback) => callback,
        None => &mut noop_progress,
    };
    let import_result =
        walk_plans_into_states(bridge, &repo, &mut tree_importer, &plans, &mut stats, progress_cb);
    match import_result {
        Ok(()) => {
            stats
                .lossy_entries
                .extend(tree_importer.lossy_entries().iter().cloned());
            // Crash-safe sequencing (risk #3): the pack must be durably
            // installed BEFORE the change_id↔git_oid mapping is committed or
            // any ref/tag/marker is synced below, so a crash can never leave
            // a ref or mapping entry pointing into a pack that didn't land.
            tree_importer.finalize_pack_install()?;
            bridge.write_mapping_tmp_to_disk()?;
            bridge.commit_mapping_tmp_to_disk()?;
        }
        Err(error) => {
            tree_importer.abort_pack();
            return Err(error);
        }
    }

    for plan in plans
        .iter()
        .filter(|plan| plan.namespace == RefNamespace::Branch)
    {
        let name = &plan.short_name;
        if wanted_refs.is_some_and(|wanted| !wanted.contains(name.as_str())) {
            continue;
        }
        if let Some(change_id) = bridge.mapping.get_heddle(plan.peeled_commit_oid) {
            // A git branch name becomes a Heddle thread id here. Reject one that
            // isn't a valid thread id (e.g. contains a shell metacharacter git
            // permits in a ref) rather than silently slugifying it — a silent
            // rewrite would violate the no-silent-default stance, and an unsafe
            // id breaks recommended-command breadcrumbs. Pre-1.0: the operator
            // renames the branch and re-imports. (heddle#464 close-the-class.)
            if let Err(err) = ThreadId::new(name.as_str()) {
                return Err(GitBridgeError::InvalidThreadName {
                    branch: name.to_string(),
                    message: err.to_string(),
                });
            }
            let existing = bridge
                .heddle_repo
                .refs()
                .get_thread(&ThreadName::new(name.as_str()))?;
            if let Some(existing_change) = existing
                && !thread_can_adopt_change(bridge.heddle_repo, &existing_change, &change_id)?
            {
                return Err(GitBridgeError::GitHeddleThreadDiverged {
                    thread: name.to_string(),
                    branch: name.to_string(),
                    thread_change: existing_change,
                    branch_change: change_id,
                });
            }

            if should_materialize_imported_current_thread(bridge.heddle_repo, name, existing)? {
                bridge
                    .heddle_repo
                    .fast_forward_attached_without_record(&change_id)
                    .map_err(|e| {
                        GitBridgeError::InvalidMapping(format!(
                            "materialize imported branch '{}' failed: {}",
                            name, e
                        ))
                    })?;
            } else {
                bridge
                    .heddle_repo
                    .refs()
                    .set_thread(&ThreadName::new(name.as_str()), &change_id)
                    .map_err(|e| {
                        GitBridgeError::InvalidMapping(format!(
                            "set_thread failed for '{}': {}",
                            name, e
                        ))
                    })?;
            }
            stats.branches_synced += 1;
        }
    }

    for tag in repo
        .references()
        .map_err(git_err)?
        .tags()
        .map_err(git_err)?
    {
        let mut tag = tag.map_err(git_err)?;
        let name = tag.name().shorten().to_string();
        if wanted_refs.is_some_and(|wanted| !wanted.contains(&name)) {
            continue;
        }
        // Skip non-commit-pointing tags here too; the tips loop already
        // recorded them in `skipped_non_commit_refs`.
        let oid = match peel_to_commit_oid(&repo, &mut tag)? {
            Ok(oid) => oid,
            Err(_) => continue,
        };
        if let Some(change_id) = bridge.mapping.get_heddle(oid) {
            // Markers become lightweight tags (just a ref → peeled commit);
            // that round-trips through the mirror unchanged and needs no object.
            sync_marker_from_git_tag(bridge, &name, &change_id)?;
            // annotated-tag-object fidelity: see #575 (first-class content-addressed storage)
            stats.tags_synced += 1;
        }
    }

    Ok(stats)
}

fn shallow_import_retry_command(wanted_refs: Option<&HashSet<String>>) -> String {
    match wanted_refs.and_then(|refs| refs.iter().next()) {
        Some(_) => "heddle bridge git import --path <full-git-repo> --ref <ref>".to_string(),
        None => "heddle bridge git import --path <full-git-repo>".to_string(),
    }
}

fn sync_marker_from_git_tag(
    bridge: &GitBridge<'_>,
    name: &str,
    change_id: &ChangeId,
) -> GitResult<()> {
    let mn = MarkerName::new(name);
    match bridge.heddle_repo.refs().get_marker(&mn) {
        Ok(Some(existing)) if existing == *change_id => Ok(()),
        Ok(Some(_)) => bridge
            .heddle_repo
            .refs()
            .set_marker_cas(&mn, RefExpectation::Any, change_id)
            .map_err(|error| {
                GitBridgeError::InvalidMapping(format!(
                    "failed to update marker '{}' during git import: {}",
                    name, error
                ))
            }),
        Ok(None) => bridge
            .heddle_repo
            .refs()
            .create_marker(&mn, change_id)
            .map_err(|error| {
                GitBridgeError::InvalidMapping(format!(
                    "failed to create marker '{}' during git import: {}",
                    name, error
                ))
            }),
        Err(error) => Err(error.into()),
    }
}

fn collect_note_ref_updates(repo: &gix::Repository) -> GitResult<Vec<RefUpdate>> {
    let mut updates = Vec::new();
    for reference in repo
        .references()
        .map_err(git_err)?
        .prefixed("refs/notes/")
        .map_err(git_err)?
    {
        let reference = reference.map_err(git_err)?;
        let Some(target) = reference.try_id() else {
            continue;
        };
        let full = reference.name().as_bstr().to_string();
        let short = full
            .strip_prefix("refs/notes/")
            .unwrap_or(&full)
            .to_string();
        updates.push(RefUpdate {
            name: short,
            target: target.detach(),
            namespace: RefNamespace::Note,
        });
    }
    Ok(updates)
}

fn should_materialize_imported_current_thread(
    heddle_repo: &HeddleRepository,
    name: &str,
    existing: Option<ChangeId>,
) -> GitResult<bool> {
    if heddle_repo.capability() != repo::RepositoryCapability::NativeHeddle {
        return Ok(false);
    }
    if !matches!(
        heddle_repo.refs().read_head()?,
        Head::Attached { ref thread } if thread == name
    ) {
        return Ok(false);
    }
    let Some(existing) = existing else {
        return Ok(false);
    };
    let Some(state) = heddle_repo.store().get_state(&existing)? else {
        return Ok(false);
    };
    let Some(tree) = heddle_repo.store().get_tree(&state.tree)? else {
        return Ok(false);
    };
    heddle_repo
        .worktree_is_clean_cached(&tree)
        .map_err(|err| GitBridgeError::InvalidMapping(err.to_string()))
}

pub(crate) fn thread_can_adopt_change(
    heddle_repo: &HeddleRepository,
    existing: &ChangeId,
    change_id: &ChangeId,
) -> GitResult<bool> {
    if existing == change_id {
        return Ok(true);
    }
    if thread_is_unclaimed_bootstrap(heddle_repo, existing)? {
        return Ok(true);
    }
    proto::is_ancestor(heddle_repo.store(), *existing, *change_id)
        .map_err(|err| GitBridgeError::InvalidMapping(err.to_string()))
}

/// Phase work for the iterative ancestry walker.
///
/// `Enter(oid)` schedules a commit for visit: discover its parents and
/// queue them. `Emit(oid)` finalizes a commit: import it as a heddle
/// state once all its parents have already been emitted.
///
/// We separate the phases because we need post-order traversal (parents
/// before children), and a single-marker stack can't express "I've
/// queued this commit's parents but haven't emitted the commit itself
/// yet" without keeping per-node state outside the stack.
enum WalkPhase {
    Enter(gix::hash::ObjectId),
    Emit(gix::hash::ObjectId),
}

/// Iterative ancestry walk — post-order DFS using an explicit stack
/// instead of recursion.
///
/// **Why this matters:** the previous version recursed once per parent
/// hop, so the call stack grew as deep as the longest chain in the
/// commit DAG. On `git/git` (84k commits) this overflowed the main
/// thread's 8MB stack after ~1 second and aborted with SIGABRT before
/// any state was written. With the explicit stack we're bounded only by
/// heap memory, which scales with the DAG's total node count rather
/// than its depth.
///
/// Behavior is otherwise unchanged: parents are processed before their
/// children, already-imported nodes are skipped, and re-entering a node
/// that's still in flight (a merge with two paths to the same ancestor)
/// is a no-op.
/// Walk each ref plan's ancestry into Heddle states, threading the optional
/// per-commit progress callback through. Extracted from the import flow so the
/// `progress` borrow is scoped to this call (an inline closure would force the
/// borrow to outlive the surrounding function).
#[allow(clippy::too_many_arguments)]
fn walk_plans_into_states(
    bridge: &mut GitBridge<'_>,
    repo: &gix::Repository,
    tree_importer: &mut GitTreeImporter<'_>,
    plans: &[RefPlan],
    stats: &mut ImportStats,
    progress: &mut dyn FnMut(usize),
) -> GitResult<()> {
    let mut visiting = HashSet::new();
    let mut imported = HashSet::new();
    for plan in plans {
        import_commit_ancestry(
            bridge,
            repo,
            tree_importer,
            plan.peeled_commit_oid,
            &mut visiting,
            &mut imported,
            stats,
            &mut *progress,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn import_commit_ancestry(
    bridge: &mut GitBridge<'_>,
    repo: &gix::Repository,
    tree_importer: &mut GitTreeImporter<'_>,
    git_oid: gix::hash::ObjectId,
    visiting: &mut HashSet<gix::hash::ObjectId>,
    imported: &mut HashSet<gix::hash::ObjectId>,
    stats: &mut ImportStats,
    progress: &mut dyn FnMut(usize),
) -> GitResult<()> {
    let mut stack: Vec<WalkPhase> = vec![WalkPhase::Enter(git_oid)];

    while let Some(phase) = stack.pop() {
        match phase {
            WalkPhase::Enter(oid) => {
                // Skip only if we've fully processed this OID earlier in
                // the same walk. We deliberately do NOT skip on
                // `mapping.has_git(oid)` here — even when the mapping
                // already knows the change_id (e.g. recovered from
                // refs/notes/heddle on a fresh re-import of an exported
                // repo), the heddle state for this commit may not yet
                // exist in the store. Letting the walk continue ensures
                // `import_commit` runs and writes the state.
                if imported.contains(&oid) {
                    continue;
                }
                if !visiting.insert(oid) {
                    // Already in flight via another merge path — its Emit
                    // is already scheduled, no need to re-queue.
                    continue;
                }

                let commit = repo.find_commit(oid).map_err(git_err)?;
                let parent_git_oids: Vec<gix::hash::ObjectId> =
                    commit.parent_ids().map(|id| id.detach()).collect();

                // Schedule emit AFTER all parents are processed. Stack is
                // LIFO so the Emit goes on first; then parents on top of
                // it pop first. Reverse so the original parent order is
                // preserved.
                stack.push(WalkPhase::Emit(oid));
                for parent_oid in parent_git_oids.into_iter().rev() {
                    stack.push(WalkPhase::Enter(parent_oid));
                }
            }
            WalkPhase::Emit(oid) => {
                // Decide whether to call import_commit by checking the
                // *store*, not the mapping: the mapping can carry an
                // entry recovered from a note that has no matching state
                // object yet. `import_commit` is idempotent — if the
                // change_id (from mapping or trailer or derived) already
                // has a state in the store, `put_state` overwrites it
                // with identical bytes.
                let existing_change_id = bridge.mapping.get_heddle(oid);
                let needs_state = match existing_change_id {
                    // heddle#555 risk #2: a state buffered in the un-finalized
                    // pack isn't readable via the store yet, so check the
                    // in-memory staged set first; fall back to the store for
                    // states a prior import already installed (keeps re-import
                    // idempotent — states_created stays 0 on a no-op re-adopt).
                    Some(cid) => {
                        !tree_importer.state_staged_in_pack(&cid)
                            && bridge.heddle_repo.store().get_state(&cid)?.is_none()
                    }
                    None => true,
                };
                if needs_state {
                    let before_lossy = tree_importer.lossy_entries().len();
                    let change_id =
                        import_commit(&mut bridge.mapping, repo, tree_importer, oid)?;
                    bridge.mapping.insert(change_id, oid);
                    let commit_lossy_entries =
                        tree_importer.lossy_entries()[before_lossy..].to_vec();
                    bridge
                        .mapping
                        .set_git_lossy_entries(oid, commit_lossy_entries);
                    stats.states_created += 1;
                } else if let Some(lossy_entries) = bridge.mapping.get_git_lossy_entries(oid) {
                    if !tree_importer.lossy_enabled() {
                        return Err(fail_lossy_entry(&lossy_entries[0]));
                    }
                    stats.lossy_entries.extend(lossy_entries.iter().cloned());
                }
                // Counted regardless of `needs_state`: `commits_imported`
                // reports commits **walked from the source**, mirroring
                // what `bridge git ingest` reports. Without this, an
                // already-imported ref read 0 in the JSON even though
                // every commit in the ancestry had been resolved —
                // which is what made heddle#147 look like a silent
                // failure next to `ingest`. `states_created` retains
                // the "new heddle states written" meaning.
                stats.commits_imported += 1;
                progress(stats.commits_imported);
                visiting.remove(&oid);
                imported.insert(oid);
            }
        }
    }

    Ok(())
}
