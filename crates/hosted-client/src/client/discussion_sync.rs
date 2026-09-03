// SPDX-License-Identifier: Apache-2.0
//! Hosted discussion sync bridge.
//!
//! Local discussions live in the append-only [`CollaborationStore`] op-log
//! (`.heddle/collaboration/ops`). The hosted weft `CollaborationService` speaks
//! a different, per-state `DiscussionsBlob` model (id-keyed discussions with a
//! linear turn list). This module bridges the two:
//!
//! * **Push (write path):** after a successful `heddle push`, replay local
//!   symbol-anchored discussion turns *we authored* to the server via the
//!   caller-authenticated `OpenDiscussion` / `AppendTurn` RPCs (enforce-mode
//!   signed). #549 rejects attachments in the pack, so they cannot ride it.
//! * **Pull/clone (read path):** after a successful clone/pull, consume the
//!   pull bootstrap's discussions when present, falling back to `ListByState`
//!   for older servers and when the server advertised `discussions_from_pack`
//!   but this client cannot consume the attachment (missing / wrong kind /
//!   version skew). Live discussions are not a pack snapshot. Materialize
//!   unseen turns into the local op-log.
//!
//! ## Turn identity
//!
//! Local turn order is op-log materialization order; server turn order is
//! push/append order. They diverge the moment both sides append, so a single
//! "N turns synced" prefix count is a lie. Instead the per-repo mirror map
//! (`.heddle/collaboration/hosted-mirror.json`) records, per discussion, an
//! explicit set of **turn links**: a local turn id ↔ a server turn ordinal.
//!
//! A local turn id is `(CollabOpId, index-within-op)` — NOT the `CollabOpId`
//! alone, because a `LegacyImported` op (a migrated blob→op-log discussion)
//! materializes *all* its turns under one shared `CollabOpId`. Keying on the op
//! alone would give turns 2..N the same idempotency key with different bodies
//! (a weft `with_idempotency` conflict) and collapse them into one link, so the
//! rest would be silently dropped on exactly the migrated repos.
//!
//! Push sends only turns that are self-authored AND unlinked; pull materializes
//! only server ordinals not yet linked. A materialized turn op's idempotency
//! key is derived deterministically from `(hosted discussion id, server turn
//! ordinal)` (see `turn_op_key`) — never a random per-write nonce — so the same
//! server turn earns the same `CollabOpId` on every clone and a retry replays
//! instead of conflicting or duplicating.
//! Resolve-into-annotation operations use the same durable mirror: their
//! request identity is derived from `(hosted discussion id, server resolution
//! key)` (see `resolve_op_key`) and is recorded only after `ResolveDiscussion`
//! succeeds.
//!
//! ## Reconciliation is author-aware, never body-alone
//!
//! When the mirror map is lost/rebuilt, an unlinked server turn is reconciled
//! against an unlinked local turn only under an explicit **author** rule — never
//! body equality alone, which would cross-link two different authors' identical
//! bodies (`"lgtm"`, `"+1"`) and silently drop one:
//! * (i) a turn WE pushed — the local turn is self-authored AND the server
//!   turn's author is our own hosted username (weft stamps
//!   `Principal::new(username, "")`); or
//! * (ii) a turn we previously PULLED — the local op's author (written as
//!   `Principal::new(author_name, author_email)`) and `occurred_at_ms` exactly
//!   equal the server turn's author and `posted_at`.
//!
//! Anything matching neither rule materializes as a new, distinct turn.
//! Distinguishing "a turn I pushed" from "a turn another clone of the SAME user
//! pushed" is impossible client-side without server-minted turn ids (weft#640);
//! rule (i) is precise across distinct hosted principals, which is the real
//! multi-party case.
//!
//! The mirror is saved after **each** discussion and on the error path, with
//! collect-and-continue per discussion — one wedged discussion (e.g. weft#638's
//! no-HEAD `AppendTurn`) cannot abort the rest, and a mid-run failure never
//! leaves durable writes without their mapping.
//!
//! Scope: discussions only; `context`/`review` share the same seam (not built).
//! By-edit, dismiss, and reopen state are not yet mirrored.

#![cfg(feature = "client")]

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    fs::OpenOptions,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use objects::{
    fs_atomic::write_file_atomic,
    lock::RepoLock,
    object::{
        Attribution, CollabOpId, CollaborationAnchor, CollaborationIdempotencyKey,
        CollaborationOperationBodyV1, CollaborationOperationEnvelope, CollaborationResolution,
        Discussion, DiscussionRecordId, DiscussionTurnV1, MaterializedDiscussion, Principal,
        StateId, VisibilityTier,
    },
    store::ObjectStore,
};
use repo::{CollaborationStore, Repository, mark_legacy_discussions_migrated};
use serde::{Deserialize, Serialize};

use crate::{
    client::HostedClient,
    hosted_runtime::hosted::{HostedDiscussion, HostedDiscussionTurn, HostedResolution},
};

/// Deterministic namespace for the derived client-operation-ids so a retried
/// push replays (server-side idempotent) rather than duplicating a turn.
const OP_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x6865_6464_6c65_6469_7363_7573_7379_6e63);

#[derive(Debug, Default, Serialize, Deserialize)]
struct HostedMirror {
    /// Server repo path → mirror state for that hosted repo.
    #[serde(default)]
    repos: BTreeMap<String, RepoMirror>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RepoMirror {
    #[serde(default)]
    discussions: Vec<MirrorEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MirrorEntry {
    /// Local `DiscussionRecordId` (string form).
    local_id: String,
    /// Server-assigned discussion id.
    server_id: String,
    /// Turns known to exist on BOTH sides, each carrying its identity on both.
    #[serde(default)]
    links: Vec<TurnLink>,
    /// Client operation id of the resolve-into-annotation request known to
    /// exist on both sides.
    #[serde(default)]
    resolved_into_annotation_operation_id: Option<String>,
    /// Hosted resolution already imported (`dismissed:{reason}`,
    /// `by_edit:{hex}`, `annotation:{id}`). Only this exact hosted operation
    /// is treated as already applied; a distinct local resolution is a
    /// competing sibling.
    #[serde(default)]
    pulled_resolution_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TurnLink {
    /// Local turn id: `{CollabOpId}#{index-within-op}` — the stable turn
    /// identity (unique even when a `LegacyImported` op carries many turns).
    local_turn_id: String,
    /// Position of the turn in the server's linear turn list.
    server_ordinal: usize,
    /// Server-minted turn identity from the event stream / DiscussionTurn
    /// wire. Empty on older ListByState snapshots that only had ordinals.
    #[serde(default)]
    server_turn_id: Option<String>,
}

/// One local turn with the identity + attribution the sync bridge reasons over.
struct LocalTurn {
    turn_id: String,
    body: String,
    author_name: String,
    author_email: String,
    occurred_at_ms: i64,
    is_self: bool,
}

fn turn_identity(op_id: &CollabOpId, index_within_op: usize) -> String {
    format!("{}#{index_within_op}", op_id.to_string_full())
}

fn mirror_path(heddle_dir: &Path) -> PathBuf {
    heddle_dir.join("collaboration").join("hosted-mirror.json")
}

fn load_mirror(heddle_dir: &Path) -> Result<HostedMirror> {
    match fs::read(mirror_path(heddle_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("decode hosted discussion mirror map"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HostedMirror::default()),
        Err(error) => Err(error).context("read hosted discussion mirror map"),
    }
}

fn save_mirror(heddle_dir: &Path, mirror: &HostedMirror) -> Result<()> {
    let path = mirror_path(heddle_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("create collaboration dir")?;
    }
    let bytes = serde_json::to_vec_pretty(mirror).context("encode hosted discussion mirror map")?;
    write_file_atomic(&path, &bytes).context("write hosted discussion mirror map")?;
    Ok(())
}

fn mirror_lock(heddle_dir: &Path) -> Result<RepoLock> {
    let dir = heddle_dir.join("collaboration");
    fs::create_dir_all(&dir).context("create collaboration dir")?;
    let lock_path = dir.join("hosted-mirror.lock");
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&lock_path)
        .context("create hosted-mirror lock")?;
    Ok(RepoLock::at(lock_path))
}

fn lock_mirror_write(heddle_dir: &Path) -> Result<objects::lock::WriteLockGuard> {
    mirror_lock(heddle_dir)?
        .write()
        .map_err(|error| anyhow!("lock hosted discussion mirror: {error}"))
}

fn open_op_id(repo_path: &str, local_id: &str) -> String {
    uuid::Uuid::new_v5(
        &OP_NAMESPACE,
        format!("open:{repo_path}:{local_id}").as_bytes(),
    )
    .to_string()
}

fn append_op_id(repo_path: &str, server_id: &str, turn_id: &str) -> String {
    uuid::Uuid::new_v5(
        &OP_NAMESPACE,
        format!("append:{repo_path}:{server_id}:{turn_id}").as_bytes(),
    )
    .to_string()
}

fn resolve_into_annotation_op_id(
    repo_path: &str,
    server_id: &str,
    kind: objects::object::AnnotationKind,
    content: &str,
    tags: &[String],
) -> Result<String> {
    let identity = serde_json::to_vec(&(repo_path, server_id, kind.as_str(), content, tags))
        .context("encode resolve-into-annotation operation identity")?;
    Ok(uuid::Uuid::new_v5(&OP_NAMESPACE, &identity).to_string())
}

/// Enumerate a materialized discussion's turns with their per-op index (turn
/// identity), author, and whether the local principal authored them. Reads each
/// distinct op once for its author/timestamp.
fn collect_local_turns(
    store: &CollaborationStore,
    discussion: &MaterializedDiscussion,
    self_attr: Option<&Attribution>,
) -> Result<Vec<LocalTurn>> {
    let mut per_op: HashMap<CollabOpId, usize> = HashMap::new();
    let mut op_author: HashMap<CollabOpId, (Principal, i64)> = HashMap::new();
    let mut turns = Vec::with_capacity(discussion.turns.len());
    for (op_id, turn) in &discussion.turns {
        let index_within_op = {
            let slot = per_op.entry(*op_id).or_insert(0);
            let value = *slot;
            *slot += 1;
            value
        };
        let (principal, occurred_at_ms) = match op_author.get(op_id) {
            Some(cached) => cached.clone(),
            None => {
                let decoded = store
                    .read_operation(op_id)
                    .context("read collaboration operation")?
                    .ok_or_else(|| anyhow!("collaboration operation {op_id} missing"))?;
                let entry = (
                    decoded.operation.author.principal.clone(),
                    decoded.operation.occurred_at_ms,
                );
                op_author.insert(*op_id, entry.clone());
                entry
            }
        };
        // F3: fail closed — an op we cannot attribute to the local principal is
        // NOT treated as ours (no `self_attr` ⇒ never self).
        let is_self = self_attr.is_some_and(|attr| principals_match(&principal, &attr.principal));
        turns.push(LocalTurn {
            turn_id: turn_identity(op_id, index_within_op),
            body: turn.body.clone(),
            author_name: principal.name_lossy().into_owned(),
            author_email: principal.email_lossy().into_owned(),
            occurred_at_ms,
            is_self,
        });
    }
    Ok(turns)
}

/// Publish local symbol-anchored discussion turns we authored to the hosted
/// `CollaborationService`. Saves the mirror after each discussion and continues
/// past a per-discussion failure (warn-and-skip).
pub async fn push_discussions(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
) -> Result<usize> {
    let store = CollaborationStore::open(repo.heddle_dir()).context("open collaboration store")?;
    let materialized = store
        .materialize()
        .context("materialize local discussions")?;
    if materialized.discussions.is_empty() {
        return Ok(0);
    }
    let self_attr = repo.get_attribution().ok();

    let _guard = lock_mirror_write(repo.heddle_dir())?;
    let mut mirror = load_mirror(repo.heddle_dir())?;
    let mut synced = 0usize;

    for (discussion_id, discussion) in &materialized.discussions {
        let result = push_one(
            client,
            &store,
            repo,
            repo_path,
            &mut mirror,
            self_attr.as_ref(),
            &discussion_id.to_string(),
            discussion,
        )
        .await;
        // Persist links after every discussion — including the error path, where
        // some turns may already be on the server — so a retry resumes cleanly.
        save_mirror(repo.heddle_dir(), &mirror)?;
        match result {
            Ok(true) => synced += 1,
            Ok(false) => {}
            Err(error) => {
                eprintln!(
                    "{} hosted discussion {}: {error:#}",
                    heddle_cli_render::cli::style::warn_marker(),
                    discussion_id
                );
            }
        }
    }

    Ok(synced)
}

#[allow(clippy::too_many_arguments)]
async fn push_one(
    client: &mut HostedClient,
    store: &CollaborationStore,
    repo: &Repository,
    repo_path: &str,
    mirror: &mut HostedMirror,
    self_attr: Option<&Attribution>,
    local_id: &str,
    discussion: &MaterializedDiscussion,
) -> Result<bool> {
    let CollaborationAnchor::Symbol {
        state_id,
        path,
        symbol,
    } = &discussion.anchor
    else {
        // Only symbol-anchored discussions map to the hosted PathSymbolRef.
        return Ok(false);
    };
    let Some(state) = repo
        .store()
        .get_state(state_id)
        .context("load discussion anchor state")?
    else {
        return Ok(false);
    };
    let change_id = state.change_id;
    let visibility = discussion.visibility.as_str().to_string();

    let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
    let entry_index = repo_mirror
        .discussions
        .iter()
        .position(|entry| entry.local_id == local_id);
    let linked: HashSet<String> = match entry_index {
        Some(i) => repo_mirror.discussions[i]
            .links
            .iter()
            .map(|link| link.local_turn_id.clone())
            .collect(),
        None => HashSet::new(),
    };

    // Candidates: turns we authored that the server does not already hold.
    let local_turns = collect_local_turns(store, discussion, self_attr)?;
    let mut candidates: Vec<(String, String)> = Vec::new(); // (turn_id, body)
    let mut skipped_foreign = 0usize;
    for turn in &local_turns {
        if linked.contains(&turn.turn_id) {
            continue;
        }
        if !turn.is_self {
            // Never re-publish another author's turn under our identity.
            skipped_foreign += 1;
            continue;
        }
        candidates.push((turn.turn_id.clone(), turn.body.clone()));
    }
    if skipped_foreign > 0 {
        // F3: surface principal drift / foreign-authored unpushed turns instead
        // of silently producing an empty candidate set.
        eprintln!(
            "{} hosted discussion {local_id}: {skipped_foreign} unlinked turn(s) not attributed to the local principal were left unpublished",
            heddle_cli_render::cli::style::warn_marker(),
        );
    }
    let (index, server_id, mut changed) =
        match entry_index {
            None => {
                if candidates.is_empty() {
                    return Ok(false);
                }
                let (open_turn_id, open_body) = candidates[0].clone();
                let proposed_id = local_id.to_string();
                let hosted = client
                    .open_discussion(
                        repo_path,
                        change_id,
                        path,
                        symbol,
                        &open_body,
                        &visibility,
                        discussion.thread_ref.as_deref(),
                        open_op_id(repo_path, local_id),
                        &proposed_id,
                    )
                    .await
                    .with_context(|| format!("open hosted discussion for {local_id}"))?;
                let server_id = hosted.id.clone();
                let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
                repo_mirror.discussions.push(MirrorEntry {
                    local_id: local_id.to_string(),
                    server_id: server_id.clone(),
                    links: vec![TurnLink {
                        local_turn_id: open_turn_id,
                        server_ordinal: 0,
                        server_turn_id: None,
                    }],
                    resolved_into_annotation_operation_id: None,
                    pulled_resolution_key: None,
                });
                let index = repo_mirror.discussions.len() - 1;
                for (turn_id, body) in &candidates[1..] {
                    let hosted = client
                        .append_turn(
                            repo_path,
                            &server_id,
                            body,
                            append_op_id(repo_path, &server_id, turn_id),
                        )
                        .await
                        .with_context(|| format!("append hosted turn for {local_id}"))?;
                    push_link(
                        mirror,
                        repo_path,
                        index,
                        turn_id.clone(),
                        hosted.turns.len().saturating_sub(1),
                        hosted.turns.last().and_then(|turn| {
                            (!turn.turn_id.is_empty()).then(|| turn.turn_id.clone())
                        }),
                    );
                }
                (index, server_id, true)
            }
            Some(index) => {
                let server_id = mirror.repos[repo_path].discussions[index].server_id.clone();
                for (turn_id, body) in &candidates {
                    let hosted = client
                        .append_turn(
                            repo_path,
                            &server_id,
                            body,
                            append_op_id(repo_path, &server_id, turn_id),
                        )
                        .await
                        .with_context(|| format!("append hosted turn for {local_id}"))?;
                    push_link(
                        mirror,
                        repo_path,
                        index,
                        turn_id.clone(),
                        hosted.turns.len().saturating_sub(1),
                        hosted.turns.last().and_then(|turn| {
                            (!turn.turn_id.is_empty()).then(|| turn.turn_id.clone())
                        }),
                    );
                }
                (index, server_id, !candidates.is_empty())
            }
        };

    if push_into_annotation_resolution(client, repo_path, mirror, index, &server_id, discussion)
        .await?
    {
        changed = true;
    }
    Ok(changed)
}

async fn push_into_annotation_resolution(
    client: &mut HostedClient,
    repo_path: &str,
    mirror: &mut HostedMirror,
    index: usize,
    server_id: &str,
    discussion: &MaterializedDiscussion,
) -> Result<bool> {
    let Some(objects::object::CollaborationResolution::IntoAnnotation {
        annotation_kind,
        content,
        tags,
    }) = &discussion.resolution
    else {
        return Ok(false);
    };
    let operation_id =
        resolve_into_annotation_op_id(repo_path, server_id, *annotation_kind, content, tags)?;
    if mirror.repos[repo_path].discussions[index]
        .resolved_into_annotation_operation_id
        .as_deref()
        == Some(&operation_id)
    {
        return Ok(false);
    }
    let hosted = client
        .resolve_discussion_into_annotation(
            repo_path,
            server_id,
            *annotation_kind,
            content,
            tags.clone(),
            operation_id.clone(),
        )
        .await
        .with_context(|| format!("resolve hosted discussion {server_id} into annotation"))?;
    let entry = mirror
        .repos
        .get_mut(repo_path)
        .and_then(|repo_mirror| repo_mirror.discussions.get_mut(index))
        .ok_or_else(|| anyhow!("hosted discussion mirror entry disappeared during resolution"))?;
    entry.resolved_into_annotation_operation_id = Some(operation_id);
    if let Some(key) = hosted_resolution_key(&hosted.resolution) {
        entry.pulled_resolution_key = Some(key);
    }
    Ok(true)
}

/// Fetch hosted discussions for `against` (or repository HEAD) and materialize
/// any turns we do not already hold. Saves the mirror after each discussion
/// and continues past a per-discussion failure.
///
/// `against` is the pulled/cloned tip. Clone publishes HEAD only after this
/// call, and `heddle pull feature --local-thread feature` leaves HEAD on the
/// current checkout — ListByState must not re-read HEAD.
///
/// Repo-wide: clone/pull and unfiltered `discuss wait`. Hosted discussions
/// arrive as server-minted `Discussions` state-attachments; we claim the
/// one-shot legacy blob→op-log marker so those attachments are not also
/// converted (duplicates / multi-turn diverge).
pub async fn pull_discussions(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    bootstrap: Option<&[Discussion]>,
    against: Option<StateId>,
) -> Result<usize> {
    pull_discussions_filtered(repo, client, repo_path, bootstrap, against, None).await
}

/// Wait `--thread` bootstrap: same ListByState RPC, then keep only discussions
/// whose `thread_ref` matches the paired name or whose wire `thread_id`
/// matches the stable id.
///
/// `ListDiscussionsByStateRequest` has no thread fields (heddle-api 0.23.0).
/// Do not call `ListByThreadRef` — that would be a second list RPC.
/// Pull-fold `Some(discussions)` stays repo-wide: clone/pull must not shrink.
pub async fn pull_discussions_for_thread(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    bootstrap: Option<&[Discussion]>,
    thread: &str,
    thread_id: &str,
) -> Result<usize> {
    let filter = (!thread.is_empty() || !thread_id.is_empty()).then_some((thread, thread_id));
    pull_discussions_filtered(repo, client, repo_path, bootstrap, None, filter).await
}

async fn pull_discussions_filtered(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    bootstrap: Option<&[Discussion]>,
    against: Option<StateId>,
    thread_filter: Option<(&str, &str)>,
) -> Result<usize> {
    // Repo-wide import (clone/pull, unfiltered wait) claims the one-shot
    // marker so pulled hosted attachments are not also converted. A
    // `--thread` wait only imports that thread; claiming here would hide
    // other threads' local legacy discussions from later list/show.
    if thread_filter.is_none() {
        mark_legacy_discussions_migrated(repo)
            .context("claim legacy discussion migration marker")?;
    }

    let Some((head_state, hosted)) =
        listed_hosted_discussions(repo, client, repo_path, bootstrap, against, thread_filter)
            .await?
    else {
        // weft#638: a repo with no HEAD cannot resolve a state to list against
        // unless the caller passed the pulled/cloned tip.
        return Ok(0);
    };
    if hosted.is_empty() {
        return Ok(0);
    }
    // Our own hosted principal name, so reconciliation can recognize the turns
    // we pushed (weft stamps `Principal::new(username, "")`).
    let hosted_username = client.authenticated_username();

    let store = CollaborationStore::open(repo.heddle_dir()).context("open collaboration store")?;
    let self_attr = repo.get_attribution().ok();
    let _guard = lock_mirror_write(repo.heddle_dir())?;
    let mut mirror = load_mirror(repo.heddle_dir())?;
    let mut changed = 0usize;

    for discussion in hosted {
        let result = pull_one(
            &store,
            repo_path,
            &mut mirror,
            head_state,
            hosted_username.as_deref(),
            self_attr.as_ref(),
            &discussion,
        );
        save_mirror(repo.heddle_dir(), &mirror)?;
        match result {
            Ok(true) => changed += 1,
            Ok(false) => {}
            Err(error) => {
                eprintln!(
                    "{} hosted discussion {}: {error:#}",
                    heddle_cli_render::cli::style::warn_marker(),
                    discussion.id
                );
            }
        }
    }

    Ok(changed)
}

/// Prefer the pulled/cloned tip. HEAD is wrong on clone (not published yet)
/// and on `pull --local-thread` into a thread that is not checked out.
fn discussion_sync_state(repo: &Repository, against: Option<StateId>) -> Result<Option<StateId>> {
    match against {
        Some(state) => Ok(Some(state)),
        None => repo.head().context("resolve repository head"),
    }
}

async fn listed_hosted_discussions(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    bootstrap: Option<&[Discussion]>,
    against: Option<StateId>,
    thread_filter: Option<(&str, &str)>,
) -> Result<Option<(StateId, Vec<HostedDiscussion>)>> {
    let Some(state_id) = discussion_sync_state(repo, against)? else {
        return Ok(None);
    };
    let Some(state) = repo
        .store()
        .get_state(&state_id)
        .context("load discussion sync state")?
    else {
        return Ok(None);
    };

    // Pull-fold attachments are clone/pull's repo-wide snapshot. Never shrink
    // that set for wait `--thread` — wait's filtered path always passes None.
    let hosted = match bootstrap {
        Some(discussions) => discussions
            .iter()
            .cloned()
            .map(hosted_discussion_from_bootstrap)
            .collect(),
        None => {
            let listed = client
                .list_discussions_by_state(repo_path, state.change_id, "all")
                .await
                .context("list hosted discussions")?;
            match thread_filter {
                Some((thread, thread_id)) => listed
                    .into_iter()
                    .filter(|discussion| {
                        discussion_matches_wait_thread(discussion, thread, thread_id)
                    })
                    .collect(),
                None => listed,
            }
        }
    };
    Ok(Some((state_id, hosted)))
}

fn discussion_matches_wait_thread(
    discussion: &HostedDiscussion,
    thread: &str,
    thread_id: &str,
) -> bool {
    let name_hit = !thread.is_empty()
        && discussion
            .thread_ref
            .as_deref()
            .is_some_and(|thread_ref| thread_ref == thread);
    let id_hit = !thread_id.is_empty()
        && discussion
            .thread_id
            .as_deref()
            .is_some_and(|id| id == thread_id);
    name_hit || id_hit
}

/// Import one already-fetched hosted discussion into the local op-log.
/// Persists the mirror. Used by the live event consumer after GetDiscussion
/// (opened/resolved, or an unmirrored append) and the mirrored-append fast path.
pub fn apply_hosted_discussion(
    repo: &Repository,
    repo_path: &str,
    hosted_username: Option<&str>,
    discussion: &HostedDiscussion,
) -> Result<bool> {
    let Some(head_state) = repo.head().context("resolve repository head")? else {
        return Err(anyhow!(
            "cannot apply a hosted discussion without a repository HEAD"
        ));
    };
    let store = CollaborationStore::open(repo.heddle_dir()).context("open collaboration store")?;
    let self_attr = repo.get_attribution().ok();
    let _guard = lock_mirror_write(repo.heddle_dir())?;
    let mut mirror = load_mirror(repo.heddle_dir())?;
    let result = import_hosted_discussion(
        &store,
        repo_path,
        &mut mirror,
        head_state,
        hosted_username,
        self_attr.as_ref(),
        discussion,
    );
    save_mirror(repo.heddle_dir(), &mirror)?;
    result
}

fn hosted_discussion_from_bootstrap(discussion: Discussion) -> HostedDiscussion {
    HostedDiscussion {
        id: discussion.id,
        file: discussion.anchor.file,
        symbol: discussion.anchor.symbol,
        opened_against_state: Some(discussion.opened_against_state),
        visibility: discussion.visibility.as_str().to_string(),
        thread_ref: discussion.thread_ref,
        thread_id: None,
        turns: discussion
            .turns
            .into_iter()
            .map(|turn| HostedDiscussionTurn {
                author_name: turn.author.name_lossy().into_owned(),
                author_email: turn.author.email_lossy().into_owned(),
                body: turn.body,
                posted_at_secs: turn.posted_at,
                turn_id: String::new(),
                turn_seq: 0,
            })
            .collect(),
        resolution: match discussion.resolution {
            objects::object::DiscussionResolution::Open => HostedResolution::Open,
            objects::object::DiscussionResolution::ResolvedIntoAnnotation { annotation_id } => {
                HostedResolution::IntoAnnotation { annotation_id }
            }
            objects::object::DiscussionResolution::ResolvedByEdit { state_id } => {
                HostedResolution::ByEdit {
                    state_id: Some(state_id),
                }
            }
            objects::object::DiscussionResolution::Dismissed { reason } => {
                HostedResolution::Dismissed { reason }
            }
        },
        kind: 0,
    }
}

/// Materialize one hosted discussion (snapshot or live fetch) into the local
/// collab op-log. Idempotent via the hosted mirror: already-linked turns and
/// resolutions are left alone. Used by pull bootstrap and the event consumer.
#[allow(clippy::too_many_arguments)]
fn import_hosted_discussion(
    store: &CollaborationStore,
    repo_path: &str,
    mirror: &mut HostedMirror,
    head_state: StateId,
    hosted_username: Option<&str>,
    self_attr: Option<&Attribution>,
    discussion: &HostedDiscussion,
) -> Result<bool> {
    pull_one(
        store,
        repo_path,
        mirror,
        head_state,
        hosted_username,
        self_attr,
        discussion,
    )
}

/// Choose the local `DiscussionRecordId` for a hosted discussion: adopt the id
/// carried on the wire when it is a valid `disc-<UUIDv7>` (the client-sovereign
/// id the originator minted), otherwise derive a deterministic id from the
/// hosted id so every clone of a legacy `hc-` discussion still agrees.
fn adopt_or_derive_local_id(hosted_id: &str) -> DiscussionRecordId {
    hosted_id
        .parse::<DiscussionRecordId>()
        .unwrap_or_else(|_| DiscussionRecordId::for_hosted_source(hosted_id))
}

#[allow(clippy::too_many_arguments)]
fn pull_one(
    store: &CollaborationStore,
    repo_path: &str,
    mirror: &mut HostedMirror,
    head_state: StateId,
    hosted_username: Option<&str>,
    self_attr: Option<&Attribution>,
    discussion: &HostedDiscussion,
) -> Result<bool> {
    if discussion.turns.is_empty() && matches!(discussion.resolution, HostedResolution::Open) {
        return Ok(false);
    }
    let mut entry_index = mirror
        .repos
        .entry(repo_path.to_string())
        .or_default()
        .discussions
        .iter()
        .position(|entry| entry.server_id == discussion.id);

    // Client-sovereign id + resume guard: when there is no mirror entry yet but
    // the op-log already holds this discussion under its adopted (or derived)
    // local id — we pulled our own discussion back, or the mirror file was lost
    // — re-establish the mirror link and resume into the `Some` arm instead of
    // writing a duplicate `Open` root (materialize rejects a second root).
    if entry_index.is_none() && !discussion.turns.is_empty() {
        let candidate = adopt_or_derive_local_id(&discussion.id);
        if store
            .materialize_discussion(&candidate)
            .context("check op-log for an existing discussion before pull")?
            .is_some()
        {
            let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
            repo_mirror.discussions.push(MirrorEntry {
                local_id: candidate.to_string(),
                server_id: discussion.id.clone(),
                links: Vec::new(),
                resolved_into_annotation_operation_id: None,
                pulled_resolution_key: None,
            });
            entry_index = Some(repo_mirror.discussions.len() - 1);
        }
    }

    let mut changed = match entry_index {
        None => {
            if discussion.turns.is_empty() {
                return Ok(false);
            }
            // Adopt the id the originator minted (carried on the wire) so the
            // discussion is addressable by the same id on every clone; fall back
            // to a deterministic derivation for legacy `hc-` ids.
            let local_id = adopt_or_derive_local_id(&discussion.id);
            let anchor = hosted_open_anchor(discussion, head_state);
            let title = derive_title(&discussion.turns[0].body, &discussion.symbol);
            let visibility = parse_visibility_token(&discussion.visibility);

            let first = &discussion.turns[0];
            let open_op = write_local_operation(
                store,
                local_id,
                Vec::new(),
                turn_attribution(first),
                turn_ms(first),
                CollaborationOperationBodyV1::Open {
                    title,
                    anchor,
                    visibility,
                    turn: turn_body(first)?,
                    thread_ref: discussion.thread_ref.clone(),
                },
                turn_op_key(&discussion.id, server_ordinal(first, 0))?,
            )?;
            // Record the mapping immediately so a mid-materialization failure
            // resumes into the `Some` arm instead of orphaning the written ops.
            let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
            repo_mirror.discussions.push(MirrorEntry {
                local_id: local_id.to_string(),
                server_id: discussion.id.clone(),
                links: vec![TurnLink {
                    local_turn_id: turn_identity(&open_op, 0),
                    server_ordinal: server_ordinal(first, 0),
                    server_turn_id: server_turn_id(first),
                }],
                resolved_into_annotation_operation_id: None,
                pulled_resolution_key: None,
            });
            let index = repo_mirror.discussions.len() - 1;

            let mut heads = vec![open_op];
            for (list_index, turn) in discussion.turns.iter().enumerate().skip(1) {
                let ordinal = server_ordinal(turn, list_index);
                let op_id = write_local_operation(
                    store,
                    local_id,
                    heads.clone(),
                    turn_attribution(turn),
                    turn_ms(turn),
                    CollaborationOperationBodyV1::AppendTurn {
                        turn: turn_body(turn)?,
                    },
                    turn_op_key(&discussion.id, ordinal)?,
                )?;
                heads = vec![op_id];
                push_link(
                    mirror,
                    repo_path,
                    index,
                    turn_identity(&op_id, 0),
                    ordinal,
                    server_turn_id(turn),
                );
            }
            true
        }
        Some(index) => {
            let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
            let local_id: DiscussionRecordId = repo_mirror.discussions[index]
                .local_id
                .parse()
                .map_err(|e| anyhow!("mirror map has an invalid local discussion id: {e}"))?;
            let linked_ordinals: HashSet<usize> = repo_mirror.discussions[index]
                .links
                .iter()
                .map(|link| link.server_ordinal)
                .collect();
            let linked_server_turn_ids: HashSet<String> = repo_mirror.discussions[index]
                .links
                .iter()
                .filter_map(|link| link.server_turn_id.clone())
                .collect();
            let linked_turn_ids: HashSet<String> = repo_mirror.discussions[index]
                .links
                .iter()
                .map(|link| link.local_turn_id.clone())
                .collect();

            let existing = store
                .materialize_discussion(&local_id)
                .context("materialize mirrored discussion")?
                .ok_or_else(|| anyhow!("mirrored discussion {local_id} missing locally"))?;
            let mut heads: Vec<CollabOpId> = existing.heads.iter().copied().collect();
            // Unlinked local turns available to reconcile against server turns —
            // author-aware only (see the module note on why body alone is wrong).
            let mut available: Vec<LocalTurn> = collect_local_turns(store, &existing, self_attr)?
                .into_iter()
                .filter(|turn| !linked_turn_ids.contains(&turn.turn_id))
                .collect();

            let mut changed = false;
            for (list_index, server_turn) in discussion.turns.iter().enumerate() {
                let ordinal = server_ordinal(server_turn, list_index);
                if linked_ordinals.contains(&ordinal)
                    || server_turn_id(server_turn)
                        .is_some_and(|turn_id| linked_server_turn_ids.contains(&turn_id))
                {
                    continue;
                }
                if let Some(pos) = reconcile(&available, server_turn, hosted_username) {
                    let local = available.swap_remove(pos);
                    push_link(
                        mirror,
                        repo_path,
                        index,
                        local.turn_id,
                        ordinal,
                        server_turn_id(server_turn),
                    );
                    changed = true;
                    continue;
                }
                let op_id = write_local_operation(
                    store,
                    local_id,
                    heads.clone(),
                    turn_attribution(server_turn),
                    turn_ms(server_turn),
                    CollaborationOperationBodyV1::AppendTurn {
                        turn: turn_body(server_turn)?,
                    },
                    turn_op_key(&discussion.id, ordinal)?,
                )?;
                heads = vec![op_id];
                push_link(
                    mirror,
                    repo_path,
                    index,
                    turn_identity(&op_id, 0),
                    ordinal,
                    server_turn_id(server_turn),
                );
                changed = true;
            }
            changed
        }
    };

    if pull_resolution(store, repo_path, mirror, discussion)? {
        changed = true;
    }
    Ok(changed)
}

fn pull_resolution(
    store: &CollaborationStore,
    repo_path: &str,
    mirror: &mut HostedMirror,
    discussion: &HostedDiscussion,
) -> Result<bool> {
    let Some(resolution) = hosted_resolution_to_collab(&discussion.resolution) else {
        return Ok(false);
    };
    let Some(hosted_key) = hosted_resolution_key(&discussion.resolution) else {
        return Ok(false);
    };
    let Some(index) = mirror.repos.get(repo_path).and_then(|repo_mirror| {
        repo_mirror
            .discussions
            .iter()
            .position(|entry| entry.server_id == discussion.id)
    }) else {
        return Ok(false);
    };
    if mirror.repos[repo_path].discussions[index]
        .pulled_resolution_key
        .as_deref()
        == Some(hosted_key.as_str())
    {
        return Ok(false);
    }
    let local_id: DiscussionRecordId = mirror.repos[repo_path].discussions[index]
        .local_id
        .parse()
        .map_err(|e| anyhow!("mirror map has an invalid local discussion id: {e}"))?;
    let existing = store
        .materialize_discussion(&local_id)
        .context("materialize mirrored discussion")?
        .ok_or_else(|| anyhow!("mirrored discussion {local_id} missing locally"))?;
    let pushed_echo = is_pushed_annotation_echo(
        &mirror.repos[repo_path].discussions[index],
        &existing,
        &discussion.resolution,
    );
    let parents = if pushed_echo {
        // Finalize the server's annotation id as a descendant of the local
        // IntoAnnotation we already pushed — not a sibling that conflicts.
        existing.heads.iter().copied().collect()
    } else if existing.resolution.is_some() || !existing.conflict_operations.is_empty() {
        resolution_sibling_parents(store, &existing)?
    } else {
        existing.heads.iter().copied().collect()
    };
    if pushed_echo
        && matches!(
            existing.resolution,
            Some(CollaborationResolution::Annotation { ref annotation_id })
                if Some(annotation_id.as_str())
                    == hosted_annotation_id(&discussion.resolution)
        )
    {
        mirror
            .repos
            .get_mut(repo_path)
            .and_then(|repo_mirror| repo_mirror.discussions.get_mut(index))
            .ok_or_else(|| anyhow!("hosted discussion mirror entry disappeared during resolution"))?
            .pulled_resolution_key = Some(hosted_key);
        return Ok(false);
    }
    write_local_operation(
        store,
        local_id,
        parents,
        hosted_resolution_author(),
        resolution_ms(discussion),
        CollaborationOperationBodyV1::Resolve { resolution },
        resolve_op_key(&discussion.id, &hosted_key)?,
    )?;
    mirror
        .repos
        .get_mut(repo_path)
        .and_then(|repo_mirror| repo_mirror.discussions.get_mut(index))
        .ok_or_else(|| anyhow!("hosted discussion mirror entry disappeared during resolution"))?
        .pulled_resolution_key = Some(hosted_key);
    Ok(true)
}

fn is_pushed_annotation_echo(
    entry: &MirrorEntry,
    existing: &MaterializedDiscussion,
    hosted: &HostedResolution,
) -> bool {
    entry.resolved_into_annotation_operation_id.is_some()
        && hosted_annotation_id(hosted).is_some()
        && matches!(
            existing.resolution,
            Some(
                CollaborationResolution::IntoAnnotation { .. }
                    | CollaborationResolution::Annotation { .. }
            )
        )
}

fn hosted_open_anchor(discussion: &HostedDiscussion, head_state: StateId) -> CollaborationAnchor {
    use api::heddle::api::v1alpha1::DiscussionKind;

    let kind = DiscussionKind::try_from(discussion.kind).unwrap_or(DiscussionKind::Unspecified);
    let has_symbol = !discussion.file.is_empty() && !discussion.symbol.is_empty();
    // Coordination has no PathSymbolRef. An empty-anchor fetch of any kind
    // must still Open — failing validation here would not advance the
    // watermark and every restart would die on the same event.
    if kind == DiscussionKind::Coordination || !has_symbol {
        return CollaborationAnchor::Repository;
    }
    CollaborationAnchor::Symbol {
        state_id: discussion.opened_against_state.unwrap_or(head_state),
        path: discussion.file.clone(),
        symbol: discussion.symbol.clone(),
    }
}

fn hosted_annotation_id(resolution: &HostedResolution) -> Option<&str> {
    match resolution {
        HostedResolution::IntoAnnotation { annotation_id } if !annotation_id.is_empty() => {
            Some(annotation_id.as_str())
        }
        _ => None,
    }
}

fn hosted_resolution_key(resolution: &HostedResolution) -> Option<String> {
    match resolution {
        HostedResolution::Open => None,
        HostedResolution::Dismissed { reason } => Some(format!("dismissed:{reason}")),
        HostedResolution::ByEdit {
            state_id: Some(state_id),
        } => Some(format!("by_edit:{}", hex::encode(state_id.as_bytes()))),
        HostedResolution::ByEdit { state_id: None } => None,
        HostedResolution::IntoAnnotation { annotation_id } if !annotation_id.is_empty() => {
            Some(format!("annotation:{annotation_id}"))
        }
        HostedResolution::IntoAnnotation { .. } => None,
    }
}

fn resolution_sibling_parents(
    store: &CollaborationStore,
    existing: &MaterializedDiscussion,
) -> Result<Vec<CollabOpId>> {
    let candidates: Vec<CollabOpId> = if !existing.conflict_operations.is_empty() {
        existing.conflict_operations.iter().copied().collect()
    } else {
        existing.heads.iter().copied().collect()
    };
    for id in &candidates {
        let Some(decoded) = store
            .read_operation(id)
            .context("read head for hosted resolution parents")?
        else {
            continue;
        };
        match decoded.operation.body {
            CollaborationOperationBodyV1::Resolve { .. }
            | CollaborationOperationBodyV1::Reopen { .. }
            | CollaborationOperationBodyV1::ResolveConflict { .. } => {
                return Ok(decoded.operation.parents);
            }
            _ => {}
        }
    }
    Ok(existing.heads.iter().copied().collect())
}

fn hosted_resolution_author() -> Attribution {
    Attribution::human(Principal::new("hosted", ""))
}

fn hosted_resolution_to_collab(resolution: &HostedResolution) -> Option<CollaborationResolution> {
    match resolution {
        HostedResolution::Open => None,
        HostedResolution::IntoAnnotation { annotation_id } => {
            Some(CollaborationResolution::Annotation {
                annotation_id: annotation_id.clone(),
            })
        }
        HostedResolution::ByEdit { state_id } => {
            state_id.map(|state_id| CollaborationResolution::AddressedByState { state_id })
        }
        HostedResolution::Dismissed { reason } => Some(CollaborationResolution::Dismissed {
            reason: reason.clone(),
        }),
    }
}

fn server_ordinal(turn: &HostedDiscussionTurn, list_index: usize) -> usize {
    if turn.turn_seq > 0 {
        (turn.turn_seq as usize).saturating_sub(1)
    } else {
        list_index
    }
}

fn server_turn_id(turn: &HostedDiscussionTurn) -> Option<String> {
    (!turn.turn_id.is_empty()).then(|| turn.turn_id.clone())
}

/// Match an unlinked server turn against an unlinked local turn by AUTHOR, never
/// body alone. Returns the index into `available` when one of the two identity
/// rules holds.
fn reconcile(
    available: &[LocalTurn],
    server_turn: &HostedDiscussionTurn,
    hosted_username: Option<&str>,
) -> Option<usize> {
    let server_ms = server_turn.posted_at_secs.saturating_mul(1000);
    available.iter().position(|local| {
        if local.body != server_turn.body {
            return false;
        }
        // (i) A turn we pushed: locally self-authored AND the server stamped it
        // with our own hosted username.
        let pushed_by_us = local.is_self
            && hosted_username.is_some_and(|username| username == server_turn.author_name);
        // (ii) A turn we previously pulled: the local op copied the server
        // author + timestamp verbatim.
        let pulled_before = local.author_name == server_turn.author_name
            && local.author_email == server_turn.author_email
            && local.occurred_at_ms == server_ms;
        pushed_by_us || pulled_before
    })
}

fn push_link(
    mirror: &mut HostedMirror,
    repo_path: &str,
    index: usize,
    local_turn_id: String,
    server_ordinal: usize,
    server_turn_id: Option<String>,
) {
    if let Some(entry) = mirror
        .repos
        .get_mut(repo_path)
        .and_then(|repo_mirror| repo_mirror.discussions.get_mut(index))
    {
        entry.links.push(TurnLink {
            local_turn_id,
            server_ordinal,
            server_turn_id,
        });
    }
}

fn principals_match(a: &Principal, b: &Principal) -> bool {
    a.name == b.name && a.email == b.email
}

// --- deterministic idempotency keys for materialized hosted ops ---
//
// A `CollabOpId` is content-addressed OVER its idempotency key, so the key must
// be identical across clones for the same turn to earn the same op id (and for
// a retry to dedupe instead of duplicating). We derive it via UUIDv5 over the
// module `OP_NAMESPACE` (shared with the push-side op-id helpers; the `turn:` /
// `resolve:` descriptor prefixes keep the two spaces disjoint), purely from
// hosted, server-assigned inputs — the hosted discussion id and either the
// server turn ordinal or the server resolution key — never from local state.

/// Idempotency key for a turn op (open / append) materialized from a hosted
/// discussion. Derived from `(hosted discussion id, server turn ordinal)`, both
/// of which are identical on every clone, so the same server turn yields the
/// same `CollabOpId` everywhere.
fn turn_op_key(
    hosted_discussion_id: &str,
    server_ordinal: usize,
) -> Result<CollaborationIdempotencyKey> {
    let raw = uuid::Uuid::new_v5(
        &OP_NAMESPACE,
        format!("turn:{hosted_discussion_id}:{server_ordinal}").as_bytes(),
    )
    .to_string();
    CollaborationIdempotencyKey::new(raw)
        .map_err(|error| anyhow!("invalid idempotency key: {error}"))
}

/// Idempotency key for a resolve op materialized from a hosted discussion.
/// Derived from `(hosted discussion id, server resolution key)`, both hosted
/// values, so a resolution pulled on two clones earns the same `CollabOpId`.
fn resolve_op_key(
    hosted_discussion_id: &str,
    hosted_resolution_key: &str,
) -> Result<CollaborationIdempotencyKey> {
    let raw = uuid::Uuid::new_v5(
        &OP_NAMESPACE,
        format!("resolve:{hosted_discussion_id}:{hosted_resolution_key}").as_bytes(),
    )
    .to_string();
    CollaborationIdempotencyKey::new(raw)
        .map_err(|error| anyhow!("invalid idempotency key: {error}"))
}

fn write_local_operation(
    store: &CollaborationStore,
    discussion_id: DiscussionRecordId,
    parents: Vec<CollabOpId>,
    author: Attribution,
    occurred_at_ms: i64,
    body: CollaborationOperationBodyV1,
    key: CollaborationIdempotencyKey,
) -> Result<CollabOpId> {
    let operation = CollaborationOperationEnvelope::new(
        discussion_id,
        parents,
        key,
        author,
        occurred_at_ms,
        body,
    )
    .context("build collaboration operation")?;
    Ok(store
        .write_operation(&operation)
        .context("write collaboration operation")?
        .operation_id)
}

fn turn_body(turn: &HostedDiscussionTurn) -> Result<DiscussionTurnV1> {
    DiscussionTurnV1::new(turn.body.clone()).context("invalid discussion turn")
}

fn turn_attribution(turn: &HostedDiscussionTurn) -> Attribution {
    Attribution::human(Principal::new(
        turn.author_name.clone(),
        turn.author_email.clone(),
    ))
}

fn turn_ms(turn: &HostedDiscussionTurn) -> i64 {
    if turn.posted_at_secs > 0 {
        turn.posted_at_secs.saturating_mul(1000)
    } else {
        // heddle#1695: a materialized turn's `occurred_at_ms` is hashed into its
        // `CollabOpId`, so it MUST be identical on every clone. When the server
        // sends no `posted_at`, fall back to a deterministic 0 — never wall-clock
        // `now_ms()`, which would remint the op per clone.
        0
    }
}

/// Deterministic `occurred_at_ms` for a resolve op materialized from a hosted
/// discussion. The wire carries no dedicated resolution timestamp, and this
/// value is hashed into the resolve op's `CollabOpId`, so it MUST be identical
/// on every clone (heddle#1695). Derive it from the discussion's latest
/// server-stamped turn `posted_at` — the most recent server activity, identical
/// across clones — falling back to a deterministic 0, never wall-clock
/// `now_ms()`, which would remint the resolve op per clone.
fn resolution_ms(discussion: &HostedDiscussion) -> i64 {
    discussion
        .turns
        .iter()
        .map(|turn| turn.posted_at_secs)
        .filter(|secs| *secs > 0)
        .max()
        .map(|secs| secs.saturating_mul(1000))
        .unwrap_or(0)
}

fn derive_title(body: &str, symbol: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(symbol)
        .to_string()
}

fn parse_visibility_token(token: &str) -> VisibilityTier {
    match token {
        "public" => VisibilityTier::Public,
        "internal" => VisibilityTier::Internal,
        "team_scoped" => VisibilityTier::TeamScoped {
            team_id: String::new(),
        },
        "restricted" => VisibilityTier::Restricted {
            scope_label: String::new(),
        },
        "private" => VisibilityTier::Private {
            scope_label: String::new(),
        },
        _ => VisibilityTier::Internal,
    }
}

#[cfg(test)]
mod tests {
    use objects::object::{
        AnnotationKind, Attribution, CollaborationAnchor, CollaborationIdempotencyKey,
        CollaborationOperationBodyV1, CollaborationOperationEnvelope, CollaborationResolution,
        ContentHash, Discussion, DiscussionRecordId, DiscussionResolution, DiscussionTurn,
        DiscussionTurnV1, LegacyDiscussionId, LegacyDiscussionResolutionV1, LegacySourceLocator,
        Principal, State, StateAttachmentId, StateId, SymbolAnchor, Tree, VisibilityTier,
    };
    use tempfile::TempDir;

    use super::*;

    /// A stable per-turn idempotency key for tests that drive
    /// `write_local_operation` directly (production callers derive theirs from
    /// hosted ids via `turn_op_key`/`resolve_op_key`).
    fn test_key(seed: &str) -> CollaborationIdempotencyKey {
        CollaborationIdempotencyKey::new(format!("test-op:{seed}")).expect("valid test key")
    }

    fn local(
        body: &str,
        author_name: &str,
        author_email: &str,
        is_self: bool,
        ms: i64,
    ) -> LocalTurn {
        LocalTurn {
            turn_id: format!("co-{author_name}#0"),
            body: body.to_string(),
            author_name: author_name.to_string(),
            author_email: author_email.to_string(),
            occurred_at_ms: ms,
            is_self,
        }
    }

    fn server(
        body: &str,
        author_name: &str,
        author_email: &str,
        posted_at_secs: i64,
    ) -> HostedDiscussionTurn {
        HostedDiscussionTurn {
            author_name: author_name.to_string(),
            author_email: author_email.to_string(),
            body: body.to_string(),
            posted_at_secs,
            turn_id: String::new(),
            turn_seq: 0,
        }
    }

    // F1: identical bodies from DIFFERENT authors must NOT reconcile — the
    // server turn materializes as its own distinct turn; the local turn is left
    // unlinked (so push will still publish it). Body equality alone never links.
    #[test]
    fn reconcile_rejects_identical_body_across_authors() {
        // A's own unpushed "lgtm" (local principal "alice", not yet on server).
        let available = vec![local("lgtm", "alice", "alice@x", true, 111)];
        // B pushed "lgtm" (server stamped it "bob"); our hosted username is "alice".
        let st = server("lgtm", "bob", "", 5);
        assert_eq!(
            reconcile(&available, &st, Some("alice")),
            None,
            "a self turn must not link to a DIFFERENT author's identical body (rule i needs our username to be the server author)"
        );
    }

    // F1 rule (i): a turn WE pushed (self-authored locally, stamped with our
    // hosted username on the server) reconciles.
    #[test]
    fn reconcile_links_turn_we_pushed() {
        let available = vec![local("ship it", "alice-local", "alice@x", true, 111)];
        let st = server("ship it", "alice", "", 9); // server stamped our hosted username
        assert_eq!(reconcile(&available, &st, Some("alice")), Some(0));
    }

    // F1 rule (ii): a turn we previously PULLED (local op copied the server
    // author + posted_at verbatim) reconciles.
    #[test]
    fn reconcile_links_turn_we_pulled() {
        let available = vec![local("+1", "bob", "bob@x", false, 7000)]; // occurred = 7 * 1000
        let st = server("+1", "bob", "bob@x", 7);
        assert_eq!(reconcile(&available, &st, Some("alice")), Some(0));
        // Same body, wrong author → no match.
        let st_other = server("+1", "carol", "carol@x", 7);
        assert_eq!(reconcile(&available, &st_other, Some("alice")), None);
    }

    // F2: a LegacyImported op carries N turns under ONE CollabOpId. They must
    // yield N DISTINCT turn ids and thus N DISTINCT append idempotency keys —
    // otherwise weft dedup conflicts on turn 3 and turns 3..N are dropped.
    #[test]
    fn legacy_imported_multi_turn_op_has_distinct_identities_and_keys() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = CollaborationStore::open(temp.path()).unwrap();
        let discussion_id: DiscussionRecordId =
            "disc-018f47ea-4a54-7c89-b012-3456789abcde".parse().unwrap();
        let author = Attribution::human(Principal::new("Importer", "importer@x"));
        let anchor = CollaborationAnchor::Symbol {
            state_id: StateId::from_bytes([1; 32]),
            path: "src/lib.rs".to_string(),
            symbol: "run".to_string(),
        };
        let op = CollaborationOperationEnvelope::new(
            discussion_id,
            Vec::new(),
            CollaborationIdempotencyKey::new("legacy-1").unwrap(),
            author.clone(),
            1_000,
            CollaborationOperationBodyV1::LegacyImported {
                source: LegacySourceLocator::new(
                    StateId::from_bytes([1; 32]),
                    StateAttachmentId::from_hash(ContentHash::from_bytes([4; 32])),
                    ContentHash::from_bytes([5; 32]),
                ),
                legacy_discussion_id: LegacyDiscussionId::new("legacy-1".to_string()).unwrap(),
                aliases: Vec::new(),
                title: "run".to_string(),
                anchor,
                visibility: VisibilityTier::Internal,
                turns: vec![
                    DiscussionTurnV1::new("turn one").unwrap(),
                    DiscussionTurnV1::new("turn two").unwrap(),
                    DiscussionTurnV1::new("turn three").unwrap(),
                ],
                resolution: LegacyDiscussionResolutionV1::Open,
            },
        )
        .unwrap();
        store.write_operation(&op).unwrap();

        let materialized = store
            .materialize_discussion(&discussion_id)
            .unwrap()
            .unwrap();
        assert_eq!(materialized.turns.len(), 3);
        let self_attr = Attribution::human(Principal::new("Importer", "importer@x"));
        let turns = collect_local_turns(&store, &materialized, Some(&self_attr)).unwrap();

        // All three turns share ONE CollabOpId but MUST have distinct ids…
        let ids: HashSet<&String> = turns.iter().map(|t| &t.turn_id).collect();
        assert_eq!(ids.len(), 3, "multi-turn op must yield distinct turn ids");
        // …and distinct append idempotency keys.
        let keys: HashSet<String> = turns
            .iter()
            .map(|t| append_op_id("ns/repo", "server-1", &t.turn_id))
            .collect();
        assert_eq!(
            keys.len(),
            3,
            "each turn must get a distinct idempotency key"
        );
        // All authored by the importer (self) → all are push candidates.
        assert!(turns.iter().all(|t| t.is_self));
    }

    #[test]
    fn discussion_sync_state_prefers_the_pulled_tip_over_head() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("lib.rs"), "pub fn run() {}\n").unwrap();
        let first = repo.snapshot(Some("first".to_string()), None).unwrap().id();
        std::fs::write(temp.path().join("lib.rs"), "pub fn run() { 1 }\n").unwrap();
        let second = repo
            .snapshot(Some("second".to_string()), None)
            .unwrap()
            .id();
        assert_eq!(repo.head().unwrap(), Some(second));
        assert_eq!(
            discussion_sync_state(&repo, Some(first)).unwrap(),
            Some(first)
        );
        assert_eq!(discussion_sync_state(&repo, None).unwrap(), Some(second));
    }

    #[tokio::test]
    async fn clone_discussion_sync_uses_against_before_head_is_published() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let tree = Tree::new();
        let tree_id = repo.store().put_tree(&tree).unwrap();
        let state = State::new_snapshot(
            tree_id,
            Vec::new(),
            Attribution::human(Principal::new("Test", "test@example.com")),
        );
        repo.store().put_state(&state).unwrap();
        let against = state.id();
        assert_eq!(
            repo.head().unwrap(),
            None,
            "clone has not published HEAD yet"
        );

        let bootstrap = vec![Discussion {
            id: "server-discussion-before-head".to_string(),
            anchor: SymbolAnchor::new("lib.rs", "run"),
            opened_against_state: against,
            opened_at: 1_700_000_000,
            thread_ref: None,
            turns: vec![DiscussionTurn {
                author: Principal::new("Reviewer", "reviewer@example.com"),
                body: "keep this invariant".to_string(),
                posted_at: 1_700_000_001,
                references: Vec::new(),
            }],
            resolution: DiscussionResolution::Open,
            body_changed_since_open: false,
            anchor_ambiguous: false,
            orphaned: false,
            visibility: VisibilityTier::Internal,
            resolved_annotation_id: None,
        }];
        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;

        assert_eq!(
            pull_discussions(
                &repo,
                &mut client,
                "acme/widgets",
                Some(&bootstrap),
                Some(against),
            )
            .await
            .unwrap(),
            1,
            "discussion sync must not return Ok(0) just because HEAD is unpublished"
        );
        assert_eq!(
            repo.head().unwrap(),
            None,
            "sync must not publish clone HEAD"
        );

        let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
        assert_eq!(store.materialize().unwrap().discussions.len(), 1);

        client.close().await;
        server.await.unwrap();
    }

    // F3: no local principal ⇒ turns are NOT treated as ours (fail closed).
    #[test]
    fn collect_local_turns_fails_closed_without_self_principal() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = CollaborationStore::open(temp.path()).unwrap();
        let discussion_id = DiscussionRecordId::generate();
        let op = CollaborationOperationEnvelope::new(
            discussion_id,
            Vec::new(),
            CollaborationIdempotencyKey::new("k").unwrap(),
            Attribution::human(Principal::new("Ada", "ada@x")),
            1,
            CollaborationOperationBodyV1::Open {
                title: "t".to_string(),
                anchor: CollaborationAnchor::Symbol {
                    state_id: StateId::from_bytes([2; 32]),
                    path: "a.rs".to_string(),
                    symbol: "a".to_string(),
                },
                visibility: VisibilityTier::Internal,
                turn: DiscussionTurnV1::new("hi").unwrap(),
                thread_ref: None,
            },
        )
        .unwrap();
        store.write_operation(&op).unwrap();
        let materialized = store
            .materialize_discussion(&discussion_id)
            .unwrap()
            .unwrap();
        let turns = collect_local_turns(&store, &materialized, None).unwrap();
        assert!(
            turns.iter().all(|t| !t.is_self),
            "with no local principal, no turn may be classified as ours"
        );
    }

    #[tokio::test]
    async fn bootstrap_pull_materializes_discussion_once_and_persists_the_mirror() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("lib.rs"), "pub fn run() {}\n").unwrap();
        let state = repo
            .snapshot_with_attribution(
                Some("seed".to_string()),
                None,
                Attribution::human(Principal::new("Test", "test@example.com")),
            )
            .unwrap()
            .id();
        let bootstrap = vec![Discussion {
            id: "server-discussion-1".to_string(),
            anchor: SymbolAnchor::new("lib.rs", "run"),
            opened_against_state: state,
            opened_at: 1_700_000_000,
            thread_ref: None,
            turns: vec![DiscussionTurn {
                author: Principal::new("Reviewer", "reviewer@example.com"),
                body: "keep this invariant".to_string(),
                posted_at: 1_700_000_001,
                references: Vec::new(),
            }],
            resolution: DiscussionResolution::Open,
            body_changed_since_open: false,
            anchor_ambiguous: false,
            orphaned: false,
            visibility: VisibilityTier::Internal,
            resolved_annotation_id: None,
        }];
        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;

        assert_eq!(
            pull_discussions(
                &repo,
                &mut client,
                "acme/widgets",
                Some(&bootstrap),
                Some(state)
            )
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            pull_discussions(
                &repo,
                &mut client,
                "acme/widgets",
                Some(&bootstrap),
                Some(state)
            )
            .await
            .unwrap(),
            0
        );
        assert!(mirror_path(repo.heddle_dir()).is_file());

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn push_discussion_opens_appends_and_resolves_into_annotation() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("lib.rs"), "pub fn run() {}\n").unwrap();
        let state = repo
            .snapshot_with_attribution(
                Some("seed".to_string()),
                None,
                Attribution::human(Principal::new("Test", "test@example.com")),
            )
            .unwrap()
            .id();
        let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
        let discussion_id = DiscussionRecordId::generate();
        let author = repo.get_attribution().unwrap();
        let open = write_local_operation(
            &store,
            discussion_id,
            Vec::new(),
            author.clone(),
            1_700_000_000_000,
            CollaborationOperationBodyV1::Open {
                title: "run contract".to_string(),
                anchor: CollaborationAnchor::Symbol {
                    state_id: state,
                    path: "lib.rs".to_string(),
                    symbol: "run".to_string(),
                },
                visibility: VisibilityTier::Internal,
                turn: DiscussionTurnV1::new("first turn").unwrap(),
                thread_ref: Some("refs/heads/feature/run".to_string()),
            },
            test_key("open-run-contract"),
        )
        .unwrap();
        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
        assert_eq!(
            push_discussions(&repo, &mut client, "acme/widgets")
                .await
                .unwrap(),
            1
        );

        let append = write_local_operation(
            &store,
            discussion_id,
            vec![open],
            author.clone(),
            1_700_000_001_000,
            CollaborationOperationBodyV1::AppendTurn {
                turn: DiscussionTurnV1::new("second turn").unwrap(),
            },
            test_key("append-second-turn"),
        )
        .unwrap();
        assert_eq!(
            push_discussions(&repo, &mut client, "acme/widgets")
                .await
                .unwrap(),
            1
        );

        write_local_operation(
            &store,
            discussion_id,
            vec![append],
            author,
            1_700_000_002_000,
            CollaborationOperationBodyV1::Resolve {
                resolution: CollaborationResolution::IntoAnnotation {
                    annotation_kind: AnnotationKind::Invariant,
                    content: "the cache key includes visibility".to_string(),
                    tags: vec!["cache".to_string()],
                },
            },
            test_key("resolve-into-annotation"),
        )
        .unwrap();
        assert_eq!(
            push_discussions(&repo, &mut client, "acme/widgets")
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            push_discussions(&repo, &mut client, "acme/widgets")
                .await
                .unwrap(),
            0,
            "a mirrored resolution must not be sent again"
        );

        client.close().await;
        server.await.unwrap();
    }

    fn hosted(
        id: &str,
        body: &str,
        turn_id: &str,
        resolution: HostedResolution,
    ) -> HostedDiscussion {
        HostedDiscussion {
            id: id.to_string(),
            file: "lib.rs".to_string(),
            symbol: "run".to_string(),
            opened_against_state: None,
            visibility: "internal".to_string(),
            thread_ref: None,
            thread_id: None,
            kind: 0,
            turns: vec![HostedDiscussionTurn {
                author_name: "Ada".to_string(),
                author_email: "ada@example.com".to_string(),
                body: body.to_string(),
                posted_at_secs: 1_700_000_000,
                turn_id: turn_id.to_string(),
                turn_seq: 1,
            }],
            resolution,
        }
    }

    #[test]
    fn wait_thread_filter_matches_name_or_stamped_id() {
        let mut discussion = hosted("disc-1", "first", "turn-1", HostedResolution::Open);
        discussion.thread_ref = Some("foo".to_string());
        assert!(discussion_matches_wait_thread(
            &discussion,
            "foo",
            "thr-foo"
        ));
        assert!(!discussion_matches_wait_thread(
            &discussion,
            "bar",
            "thr-bar"
        ));
        discussion.thread_ref = Some("old-name".to_string());
        discussion.thread_id = Some("thr-foo".to_string());
        assert!(
            discussion_matches_wait_thread(&discussion, "foo", "thr-foo"),
            "a renamed thread still matches on the stable wire thread_id"
        );
        discussion.thread_id = None;
        assert!(
            !discussion_matches_wait_thread(&discussion, "foo", "thr-foo"),
            "weft stamps the UUID in thread_id, not thread_ref"
        );
        discussion.thread_ref = None;
        discussion.thread_id = Some("thr-foo".to_string());
        assert!(discussion_matches_wait_thread(
            &discussion,
            "foo",
            "thr-foo"
        ));
    }

    #[test]
    fn competing_hosted_resolution_is_recorded_not_unchanged() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("lib.rs"), "pub fn run() {}\n").unwrap();
        repo.snapshot_with_attribution(
            Some("seed".to_string()),
            None,
            Attribution::human(Principal::new("Test", "test@example.com")),
        )
        .unwrap();

        assert!(
            apply_hosted_discussion(
                &repo,
                "acme/widgets",
                None,
                &hosted("disc-1", "first", "turn-1", HostedResolution::Open),
            )
            .unwrap()
        );

        let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
        let existing = store
            .materialize()
            .unwrap()
            .discussions
            .into_values()
            .next()
            .unwrap();
        write_local_operation(
            &store,
            existing.discussion_id,
            existing.heads.iter().copied().collect(),
            Attribution::human(Principal::new("Local", "local@example.com")),
            1_700_000_100_000,
            CollaborationOperationBodyV1::Resolve {
                resolution: CollaborationResolution::Dismissed {
                    reason: "local-only".to_string(),
                },
            },
            test_key("resolve-dismissed-local"),
        )
        .unwrap();

        assert!(
            apply_hosted_discussion(
                &repo,
                "acme/widgets",
                None,
                &hosted(
                    "disc-1",
                    "first",
                    "turn-1",
                    HostedResolution::Dismissed {
                        reason: "hosted-only".to_string(),
                    },
                ),
            )
            .unwrap(),
            "a distinct hosted resolution must be recorded"
        );

        let conflicted = store
            .materialize()
            .unwrap()
            .discussions
            .into_values()
            .next()
            .unwrap();
        assert!(
            !conflicted.conflict_operations.is_empty(),
            "distinct hosted vs local resolutions must surface as competing collab state"
        );
        assert_eq!(conflicted.resolution, None);

        assert!(
            !apply_hosted_discussion(
                &repo,
                "acme/widgets",
                None,
                &hosted(
                    "disc-1",
                    "first",
                    "turn-1",
                    HostedResolution::Dismissed {
                        reason: "hosted-only".to_string(),
                    },
                ),
            )
            .unwrap(),
            "the same hosted resolution must not be imported twice"
        );
    }

    #[test]
    fn pushed_into_annotation_echo_does_not_conflict() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("lib.rs"), "pub fn run() {}\n").unwrap();
        repo.snapshot_with_attribution(
            Some("seed".to_string()),
            None,
            Attribution::human(Principal::new("Test", "test@example.com")),
        )
        .unwrap();

        assert!(
            apply_hosted_discussion(
                &repo,
                "acme/widgets",
                None,
                &hosted("disc-1", "first", "turn-1", HostedResolution::Open),
            )
            .unwrap()
        );

        let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
        let existing = store
            .materialize()
            .unwrap()
            .discussions
            .into_values()
            .next()
            .unwrap();
        write_local_operation(
            &store,
            existing.discussion_id,
            existing.heads.iter().copied().collect(),
            Attribution::human(Principal::new("Local", "local@example.com")),
            1_700_000_100_000,
            CollaborationOperationBodyV1::Resolve {
                resolution: CollaborationResolution::IntoAnnotation {
                    annotation_kind: AnnotationKind::Invariant,
                    content: "the cache key includes visibility".to_string(),
                    tags: vec!["cache".to_string()],
                },
            },
            test_key("resolve-into-annotation-local"),
        )
        .unwrap();

        let path = mirror_path(repo.heddle_dir());
        let mut mirror: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        mirror["repos"]["acme/widgets"]["discussions"][0]["resolved_into_annotation_operation_id"] =
            serde_json::json!("pushed-op");
        std::fs::write(&path, serde_json::to_vec_pretty(&mirror).unwrap()).unwrap();

        apply_hosted_discussion(
            &repo,
            "acme/widgets",
            None,
            &hosted(
                "disc-1",
                "first",
                "turn-1",
                HostedResolution::IntoAnnotation {
                    annotation_id: "ann-1".to_string(),
                },
            ),
        )
        .unwrap();

        let echoed = store
            .materialize()
            .unwrap()
            .discussions
            .into_values()
            .next()
            .unwrap();
        assert!(
            echoed.conflict_operations.is_empty(),
            "a pushed IntoAnnotation echo must not surface as competing collab state"
        );
        assert_eq!(
            echoed.resolution,
            Some(CollaborationResolution::Annotation {
                annotation_id: "ann-1".to_string(),
            })
        );
    }

    #[test]
    fn concurrent_apply_does_not_drop_turn_links() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("lib.rs"), "pub fn run() {}\n").unwrap();
        repo.snapshot_with_attribution(
            Some("seed".to_string()),
            None,
            Attribution::human(Principal::new("Test", "test@example.com")),
        )
        .unwrap();
        let path = temp.path().to_path_buf();
        drop(repo);

        let first = hosted("disc-a", "alpha", "turn-a", HostedResolution::Open);
        let second = hosted("disc-b", "beta", "turn-b", HostedResolution::Open);
        std::thread::scope(|scope| {
            let path_a = path.clone();
            let disc_a = first.clone();
            scope.spawn(move || {
                let repo = Repository::open(&path_a).unwrap();
                apply_hosted_discussion(&repo, "acme/widgets", None, &disc_a)
                    .expect("apply first discussion");
            });
            let path_b = path.clone();
            let disc_b = second.clone();
            scope.spawn(move || {
                let repo = Repository::open(&path_b).unwrap();
                apply_hosted_discussion(&repo, "acme/widgets", None, &disc_b)
                    .expect("apply second discussion");
            });
        });

        let repo = Repository::open(&path).unwrap();
        let mirror = load_mirror(repo.heddle_dir()).unwrap();
        let discussions = &mirror.repos["acme/widgets"].discussions;
        assert_eq!(
            discussions.len(),
            2,
            "both discussions must remain in the mirror"
        );
        assert!(
            discussions.iter().all(|entry| !entry.links.is_empty()),
            "concurrent apply must not drop TurnLinks"
        );
        let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
        assert_eq!(store.materialize().unwrap().discussions.len(), 2);
    }

    #[test]
    fn adopt_or_derive_local_id_adopts_valid_disc_and_derives_legacy() {
        // A valid `disc-<UUIDv7>` on the wire is adopted verbatim (client-sovereign).
        let minted = DiscussionRecordId::generate();
        assert_eq!(adopt_or_derive_local_id(&minted.to_string()), minted);
        // A legacy `hc-` id has no `disc-` form; it is derived deterministically.
        let a = adopt_or_derive_local_id("hc-legacy-xyz");
        assert_eq!(a, DiscussionRecordId::for_hosted_source("hc-legacy-xyz"));
        assert_ne!(a, adopt_or_derive_local_id("hc-other"));
    }

    fn seed_repo_with_state() -> (TempDir, Repository, StateId) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("lib.rs"), "pub fn run() {}\n").unwrap();
        let state = repo
            .snapshot_with_attribution(
                Some("seed".to_string()),
                None,
                Attribution::human(Principal::new("Test", "test@example.com")),
            )
            .unwrap()
            .id();
        (temp, repo, state)
    }

    fn bootstrap_discussion(id: &str, state: StateId) -> Discussion {
        Discussion {
            id: id.to_string(),
            anchor: SymbolAnchor::new("lib.rs", "run"),
            opened_against_state: state,
            opened_at: 1_700_000_000,
            thread_ref: None,
            turns: vec![DiscussionTurn {
                author: Principal::new("Reviewer", "reviewer@example.com"),
                body: "keep this invariant".to_string(),
                posted_at: 1_700_000_001,
                references: Vec::new(),
            }],
            resolution: DiscussionResolution::Open,
            body_changed_since_open: false,
            anchor_ambiguous: false,
            orphaned: false,
            visibility: VisibilityTier::Internal,
            resolved_annotation_id: None,
        }
    }

    // Falsifier: the originator's `disc-` id must be the id every clone can
    // address, and two clones of one source must agree. Pre-fix the pull path
    // minted a fresh UUIDv7 per clone, so neither held.
    #[tokio::test]
    async fn pull_adopts_the_originators_disc_id_so_clones_agree() {
        let originator = DiscussionRecordId::generate();
        let wire_id = originator.to_string();

        let mut materialized_ids = Vec::new();
        for _ in 0..2 {
            let (_temp, repo, state) = seed_repo_with_state();
            let bootstrap = vec![bootstrap_discussion(&wire_id, state)];
            let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
            assert_eq!(
                pull_discussions(&repo, &mut client, "acme/widgets", Some(&bootstrap), Some(state))
                    .await
                    .unwrap(),
                1
            );
            client.close().await;
            server.await.unwrap();

            let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
            // Addressable by the ORIGINATOR's id on this clone.
            assert!(
                store.materialize_discussion(&originator).unwrap().is_some(),
                "clone must be addressable by the originator's id {originator}"
            );
            let ids: Vec<DiscussionRecordId> =
                store.materialize().unwrap().discussions.keys().copied().collect();
            materialized_ids.push(ids);
        }
        assert_eq!(
            materialized_ids[0], materialized_ids[1],
            "two clones of one source must materialize the same discussion id set"
        );
    }

    // Falsifier for the resume guard: if the op-log already holds the discussion
    // (its own id came back over the wire) but the mirror has no entry, pull must
    // resume rather than write a second `Open` root (materialize rejects two roots).
    #[tokio::test]
    async fn pull_resumes_when_oplog_has_it_but_mirror_is_missing() {
        let (_temp, repo, state) = seed_repo_with_state();
        let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
        // This discussion already exists in the op-log under its own id, with the
        // open turn copied verbatim from the server (author + timestamp), so the
        // resumed pull reconciles the one turn rather than appending a copy.
        let local_id = DiscussionRecordId::generate();
        write_local_operation(
            &store,
            local_id,
            Vec::new(),
            Attribution::human(Principal::new("Reviewer", "reviewer@example.com")),
            1_700_000_001_000,
            CollaborationOperationBodyV1::Open {
                title: "run contract".to_string(),
                anchor: CollaborationAnchor::Symbol {
                    state_id: state,
                    path: "lib.rs".to_string(),
                    symbol: "run".to_string(),
                },
                visibility: VisibilityTier::Internal,
                turn: DiscussionTurnV1::new("keep this invariant").unwrap(),
                thread_ref: None,
            },
            test_key("open-keep-invariant"),
        )
        .unwrap();
        let ops_before = store.operation_ids().unwrap().len();

        // A pulls its own discussion back — same id on the wire, mirror empty.
        let bootstrap = vec![bootstrap_discussion(&local_id.to_string(), state)];
        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
        pull_discussions(&repo, &mut client, "acme/widgets", Some(&bootstrap), Some(state))
            .await
            .unwrap();
        client.close().await;
        server.await.unwrap();

        // No duplicate `Open` root: exactly one discussion, still addressable, and
        // the single seeded turn reconciled rather than duplicated.
        let materialized = store.materialize().unwrap();
        assert_eq!(materialized.discussions.len(), 1, "must not duplicate the discussion");
        assert!(materialized.discussions.contains_key(&local_id));
        assert_eq!(
            store.operation_ids().unwrap().len(),
            ops_before,
            "resume must not write a duplicate Open root for the reconciled turn"
        );
    }

    // Falsifier for heddle#1677: two clones must materialize the SAME
    // `CollabOpId`s for the same server turns. The idempotency key is now
    // derived from `(hosted discussion id, server turn ordinal)` — both
    // identical on every clone — instead of a random `Uuid::new_v4()` per
    // write. On the pre-change random key this assertion fails, the two clones
    // producing disjoint `co-…` id sets.
    #[tokio::test]
    async fn two_clones_agree_on_turn_op_ids() {
        // One server-side discussion, addressed by the originator's id and
        // pinned to a single server `opened_against_state` — identical for every
        // clone (the server sends the same value everywhere), so the Open op's
        // anchor does not depend on any clone's local HEAD.
        let wire_id = DiscussionRecordId::generate().to_string();
        let (_temp0, _repo0, pinned_state) = seed_repo_with_state();
        let mut discussion = bootstrap_discussion(&wire_id, pinned_state);
        discussion.turns.push(DiscussionTurn {
            author: Principal::new("Reviewer", "reviewer@example.com"),
            body: "and here is a follow-up".to_string(),
            posted_at: 1_700_000_002,
            references: Vec::new(),
        });

        let mut op_id_sets = Vec::new();
        for _ in 0..2 {
            let (_temp, repo, head_state) = seed_repo_with_state();
            let bootstrap = vec![discussion.clone()];
            let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
            // `against` is this clone's own resolvable tip; the shared anchor
            // rides on the discussion's pinned `opened_against_state`.
            pull_discussions(
                &repo,
                &mut client,
                "acme/widgets",
                Some(&bootstrap),
                Some(head_state),
            )
            .await
            .unwrap();
            client.close().await;
            server.await.unwrap();

            let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
            let mut ids: Vec<String> = store
                .operation_ids()
                .unwrap()
                .into_iter()
                .map(|id| id.to_string())
                .collect();
            ids.sort();
            assert_eq!(ids.len(), 2, "one Open + one AppendTurn materialized");
            op_id_sets.push(ids);
        }
        assert_eq!(
            op_id_sets[0], op_id_sets[1],
            "two clones of one source must materialize the same CollabOpId set"
        );
    }

    // heddle#1695 control: the clone/materialization path is deterministic
    // across clones for a REALISTIC hosted open turn — nonzero server posted_at,
    // server-echoed anchor state, server-stamped author. Two clones agree. (The
    // invariant #1677 established; the two tests below cover the posted_at=0 and
    // resolve-op cases that previously escaped it via wall-clock now_ms().)
    #[tokio::test]
    async fn two_clones_agree_on_open_op_for_realistic_hosted_turn() {
        let wire_id = DiscussionRecordId::generate().to_string();
        let (_temp0, _repo0, anchor_state) = seed_repo_with_state();
        let mut op_id_sets = Vec::new();
        for _ in 0..2 {
            let (_temp, repo, head_state) = seed_repo_with_state();
            // opened_against_state pinned to the server-echoed value (same for
            // every clone); posted_at nonzero (weft stamps now_secs()).
            let discussion = bootstrap_discussion(&wire_id, anchor_state);
            let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
            pull_discussions(
                &repo,
                &mut client,
                "acme/widgets",
                Some(&[discussion]),
                Some(head_state),
            )
            .await
            .unwrap();
            client.close().await;
            server.await.unwrap();

            let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
            let mut ids: Vec<String> = store
                .operation_ids()
                .unwrap()
                .into_iter()
                .map(|id| id.to_string())
                .collect();
            ids.sort();
            op_id_sets.push(ids);
        }
        assert_eq!(
            op_id_sets[0], op_id_sets[1],
            "two clones must agree on the open-op CollabOpId for a realistic hosted turn"
        );
    }

    // heddle#1695: a materialized turn whose server `posted_at` is 0 must still
    // earn the SAME open-op `CollabOpId` on every clone. `occurred_at_ms` is
    // hashed into the op id, and `turn_ms` previously fell back to wall-clock
    // `now_ms()` for a 0 `posted_at`, reminting the op per clone. Now it is a
    // deterministic 0. RED before the fix (disjoint co- ids), GREEN after.
    #[tokio::test]
    async fn two_clones_agree_on_open_op_when_posted_at_is_zero() {
        let wire_id = DiscussionRecordId::generate().to_string();
        let (_temp0, _repo0, anchor_state) = seed_repo_with_state();
        let mut op_id_sets = Vec::new();
        for _ in 0..2 {
            let (_temp, repo, head_state) = seed_repo_with_state();
            let mut discussion = bootstrap_discussion(&wire_id, anchor_state);
            discussion.turns[0].posted_at = 0; // server omitted the turn timestamp
            let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
            pull_discussions(
                &repo,
                &mut client,
                "acme/widgets",
                Some(&[discussion]),
                Some(head_state),
            )
            .await
            .unwrap();
            client.close().await;
            server.await.unwrap();

            let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
            let mut ids: Vec<String> = store
                .operation_ids()
                .unwrap()
                .into_iter()
                .map(|id| id.to_string())
                .collect();
            ids.sort();
            op_id_sets.push(ids);
        }
        assert_eq!(
            op_id_sets[0], op_id_sets[1],
            "two clones must agree on the open-op CollabOpId even when posted_at is 0"
        );
    }

    // heddle#1695: a resolution pulled onto two clones must earn the SAME
    // resolve-op `CollabOpId`. The resolve op's `occurred_at_ms` is hashed into
    // its id and previously came from wall-clock `now_ms()`, reminting the op per
    // clone. It is now derived deterministically from the discussion's latest
    // server turn `posted_at` (`resolution_ms`). RED before the fix, GREEN after.
    #[tokio::test]
    async fn two_clones_agree_on_resolve_op() {
        let wire_id = DiscussionRecordId::generate().to_string();
        let (_temp0, _repo0, anchor_state) = seed_repo_with_state();
        let mut op_id_sets = Vec::new();
        for _ in 0..2 {
            let (_temp, repo, head_state) = seed_repo_with_state();
            let mut discussion = bootstrap_discussion(&wire_id, anchor_state);
            // Server-side dismissal — carries a resolution the clone materializes
            // as a Resolve op alongside the open op.
            discussion.resolution = DiscussionResolution::Dismissed {
                reason: "handled offline".to_string(),
            };
            let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
            pull_discussions(
                &repo,
                &mut client,
                "acme/widgets",
                Some(&[discussion]),
                Some(head_state),
            )
            .await
            .unwrap();
            client.close().await;
            server.await.unwrap();

            let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
            let mut ids: Vec<String> = store
                .operation_ids()
                .unwrap()
                .into_iter()
                .map(|id| id.to_string())
                .collect();
            ids.sort();
            assert_eq!(ids.len(), 2, "one Open + one Resolve materialized");
            op_id_sets.push(ids);
        }
        assert_eq!(
            op_id_sets[0], op_id_sets[1],
            "two clones must agree on the resolve-op CollabOpId"
        );
    }

    // A retried materialization re-derives the SAME idempotency key from the
    // stable `(hosted discussion id, server turn ordinal)` tuple, so the op
    // dedupes at the store instead of duplicating. On the pre-change random key
    // the retry would mint a fresh `CollabOpId` and double the op count.
    #[test]
    fn rederived_turn_op_key_dedupes_on_retry() {
        let (_temp, repo, state) = seed_repo_with_state();
        let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
        let discussion_id = DiscussionRecordId::generate();
        let author = Attribution::human(Principal::new("Reviewer", "reviewer@example.com"));
        let body = || CollaborationOperationBodyV1::Open {
            title: "run contract".to_string(),
            anchor: CollaborationAnchor::Symbol {
                state_id: state,
                path: "lib.rs".to_string(),
                symbol: "run".to_string(),
            },
            visibility: VisibilityTier::Internal,
            turn: DiscussionTurnV1::new("keep this invariant").unwrap(),
            thread_ref: None,
        };

        let first = write_local_operation(
            &store,
            discussion_id,
            Vec::new(),
            author.clone(),
            1_700_000_001_000,
            body(),
            turn_op_key("disc-xyz", 0).unwrap(),
        )
        .unwrap();
        let after_first = store.operation_ids().unwrap().len();

        // Retry: same stable inputs → `turn_op_key` returns the same key.
        let second = write_local_operation(
            &store,
            discussion_id,
            Vec::new(),
            author,
            1_700_000_001_000,
            body(),
            turn_op_key("disc-xyz", 0).unwrap(),
        )
        .unwrap();

        assert_eq!(first, second, "a retry must earn the same CollabOpId");
        assert_eq!(
            store.operation_ids().unwrap().len(),
            after_first,
            "a retry must dedupe, not append a duplicate op"
        );
    }
}
