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
//!   signed — the CLI's existing signed hosted client). #549 rejects attachments
//!   in the pack, so they cannot ride the pack.
//! * **Pull/clone (read path):** after a successful clone/pull, `ListByState`
//!   the head state's discussions and materialize any turns we do not already
//!   hold into the local op-log so `discuss list` / `discuss show` see them.
//!
//! ## Turn identity (why not an ordinal count)
//!
//! Local turn order is op-log materialization order `(occurred_at_ms, op_id)`;
//! server turn order is push/append order. The moment both sides append these
//! diverge, so a single "N turns synced" prefix count is a lie — it would
//! re-publish another author's pulled turn under our identity and drop our own.
//!
//! Instead the per-repo mirror map (`.heddle/collaboration/hosted-mirror.json`)
//! records, per discussion, an explicit set of **turn links**: local
//! `CollabOpId` ↔ server turn ordinal. A turn is on the server iff its local
//! op-id is linked. Push sends only turns that are (a) authored by us and (b)
//! not yet linked; pull materializes only server ordinals not yet linked. Client
//! operation ids for the RPCs are derived deterministically from that stable
//! turn identity (the local `CollabOpId`), so a retry replays server-side
//! instead of duplicating or hitting a `with_idempotency` body-conflict.
//!
//! The mirror is saved after **each** discussion completes and on the error
//! path, and per-discussion failures are collected-and-continued — one wedged
//! discussion (e.g. weft#638's no-HEAD `AppendTurn`) cannot abort the rest, and
//! a mid-run failure never leaves durable writes without their mapping.
//!
//! Scope: discussions only. `context` (annotations) and `review` anchor to the
//! same state-attachment seam and can follow this exact pattern (a hosted
//! ContextService / StateReviewService sync using the same mirror-map + link
//! design) — deliberately not built here.
//!
//! Known limitations (first cut):
//! * Only `Symbol`-anchored discussions map to the hosted `PathSymbolRef` model;
//!   other anchors are skipped.
//! * Content reconciliation (matching an unlinked turn when the mirror map was
//!   lost/rebuilt) keys on the turn **body** only — author and `posted_at`
//!   diverge across the push boundary (local authoring principal vs the server
//!   principal; local `occurred_at_ms` vs server `posted_at`).
//! * `resolve` / `reopen` are not yet mirrored (turns only).
//! * weft#638: weft resolves `AppendTurn` / `ListByState` against a single state
//!   with no head carry-forward, so after a head-advancing push, sync of an
//!   already-opened discussion degrades (warn-and-skip). We only degrade
//!   gracefully here; the durable fix is server-side.

#![cfg(feature = "client")]

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use objects::fs_atomic::write_file_atomic;
use objects::store::ObjectStore;
use objects::object::{
    Attribution, CollabOpId, CollaborationAnchor, CollaborationIdempotencyKey,
    CollaborationOperationBodyV1, CollaborationOperationEnvelope, DiscussionRecordId,
    DiscussionTurnV1, MaterializedDiscussion, Principal, StateId, VisibilityTier,
};
use repo::{CollaborationStore, Repository, mark_legacy_discussions_migrated};
use serde::{Deserialize, Serialize};

use crate::client::HostedGrpcClient;
use heddle_client::grpc_hosted::{HostedDiscussion, HostedDiscussionTurn};

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
    /// Turns known to exist on BOTH sides, each carrying its identity on both:
    /// the local op-id and the server turn ordinal.
    #[serde(default)]
    links: Vec<TurnLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TurnLink {
    /// Local `CollabOpId` (`to_string_full`) — the stable turn identity.
    local_op_id: String,
    /// Position of the turn in the server's linear turn list.
    server_ordinal: usize,
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

fn open_op_id(repo_path: &str, local_id: &str) -> String {
    uuid::Uuid::new_v5(&OP_NAMESPACE, format!("open:{repo_path}:{local_id}").as_bytes()).to_string()
}

fn append_op_id(repo_path: &str, server_id: &str, local_op_id: &str) -> String {
    uuid::Uuid::new_v5(
        &OP_NAMESPACE,
        format!("append:{repo_path}:{server_id}:{local_op_id}").as_bytes(),
    )
    .to_string()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Publish local symbol-anchored discussion turns we authored to the hosted
/// `CollaborationService`. Returns the number of discussions created or
/// extended. Saves the mirror after each discussion and continues past a
/// per-discussion failure (warn-and-skip).
pub async fn push_discussions(
    repo: &Repository,
    client: &mut HostedGrpcClient,
    repo_path: &str,
) -> Result<usize> {
    let store = CollaborationStore::open(repo.heddle_dir()).context("open collaboration store")?;
    let materialized = store.materialize().context("materialize local discussions")?;
    if materialized.discussions.is_empty() {
        return Ok(0);
    }
    let self_attr = repo.get_attribution().ok();

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
                    crate::cli::style::warn_marker(),
                    discussion_id
                );
            }
        }
    }

    Ok(synced)
}

#[allow(clippy::too_many_arguments)]
async fn push_one(
    client: &mut HostedGrpcClient,
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
            .map(|link| link.local_op_id.clone())
            .collect(),
        None => HashSet::new(),
    };

    // Candidates: turns we authored that the server does not already hold.
    // Filtering to *self-authored* is the load-bearing guard against
    // re-publishing a pulled turn (another author's) under our identity even if
    // the mirror map was lost; the link check skips turns already synced.
    let mut candidates: Vec<(CollabOpId, String)> = Vec::new();
    for (op_id, turn) in &discussion.turns {
        if linked.contains(&op_id.to_string_full()) {
            continue;
        }
        if !author_is_self(store, op_id, self_attr) {
            continue;
        }
        candidates.push((*op_id, turn.body.clone()));
    }
    if candidates.is_empty() {
        return Ok(false);
    }

    match entry_index {
        None => {
            // A discussion with no mapping is one we opened locally (pulled
            // discussions always carry an entry), so its first turn is ours.
            let (open_op, open_body) = candidates[0].clone();
            let hosted = client
                .open_discussion(
                    repo_path,
                    change_id,
                    path,
                    symbol,
                    &open_body,
                    &visibility,
                    open_op_id(repo_path, local_id),
                )
                .await
                .with_context(|| format!("open hosted discussion for {local_id}"))?;
            let server_id = hosted.id.clone();
            let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
            repo_mirror.discussions.push(MirrorEntry {
                local_id: local_id.to_string(),
                server_id: server_id.clone(),
                links: vec![TurnLink {
                    local_op_id: open_op.to_string_full(),
                    server_ordinal: 0,
                }],
            });
            let index = repo_mirror.discussions.len() - 1;
            for (op_id, body) in &candidates[1..] {
                let hosted = client
                    .append_turn(
                        repo_path,
                        &server_id,
                        body,
                        append_op_id(repo_path, &server_id, &op_id.to_string_full()),
                    )
                    .await
                    .with_context(|| format!("append hosted turn for {local_id}"))?;
                let ordinal = hosted.turns.len().saturating_sub(1);
                push_link(mirror, repo_path, index, op_id.to_string_full(), ordinal);
            }
            Ok(true)
        }
        Some(index) => {
            let server_id = mirror.repos[repo_path].discussions[index].server_id.clone();
            for (op_id, body) in &candidates {
                let hosted = client
                    .append_turn(
                        repo_path,
                        &server_id,
                        body,
                        append_op_id(repo_path, &server_id, &op_id.to_string_full()),
                    )
                    .await
                    .with_context(|| format!("append hosted turn for {local_id}"))?;
                let ordinal = hosted.turns.len().saturating_sub(1);
                push_link(mirror, repo_path, index, op_id.to_string_full(), ordinal);
            }
            Ok(true)
        }
    }
}

/// Fetch hosted discussions for the repository head and materialize any turns we
/// do not already hold into the local op-log. Saves the mirror after each
/// discussion and continues past a per-discussion failure.
pub async fn pull_discussions(
    repo: &Repository,
    client: &mut HostedGrpcClient,
    repo_path: &str,
) -> Result<usize> {
    // Hosted discussions arrive as server-minted `Discussions` state-attachments
    // on the pulled objects. Those are the transport form of what we
    // authoritatively re-materialize below via the CollaborationService RPCs —
    // so claim the one-shot legacy blob->op-log migration marker to keep it from
    // also converting them (which would duplicate every discussion and diverge
    // on multi-turn supersede history). Fresh clones have no genuine local
    // legacy discussions, and existing repos already hold the marker.
    mark_legacy_discussions_migrated(repo).context("claim legacy discussion migration marker")?;

    let Some(head_state) = repo.head().context("resolve repository head")? else {
        // weft#638: a repo with no HEAD cannot resolve a state to list against.
        // Degrade gracefully — nothing to materialize.
        return Ok(0);
    };
    let Some(state) = repo
        .store()
        .get_state(&head_state)
        .context("load head state")?
    else {
        return Ok(0);
    };
    let change_id = state.change_id;

    let hosted = client
        .list_discussions_by_state(repo_path, change_id, "all")
        .await
        .context("list hosted discussions")?;
    if hosted.is_empty() {
        return Ok(0);
    }

    let store = CollaborationStore::open(repo.heddle_dir()).context("open collaboration store")?;
    let mut mirror = load_mirror(repo.heddle_dir())?;
    let mut changed = 0usize;

    for discussion in hosted {
        let result = pull_one(&store, repo_path, &mut mirror, head_state, &discussion);
        save_mirror(repo.heddle_dir(), &mirror)?;
        match result {
            Ok(true) => changed += 1,
            Ok(false) => {}
            Err(error) => {
                eprintln!(
                    "{} hosted discussion {}: {error:#}",
                    crate::cli::style::warn_marker(),
                    discussion.id
                );
            }
        }
    }

    Ok(changed)
}

fn pull_one(
    store: &CollaborationStore,
    repo_path: &str,
    mirror: &mut HostedMirror,
    head_state: StateId,
    discussion: &HostedDiscussion,
) -> Result<bool> {
    if discussion.turns.is_empty() {
        return Ok(false);
    }
    let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
    let entry_index = repo_mirror
        .discussions
        .iter()
        .position(|entry| entry.server_id == discussion.id);

    match entry_index {
        None => {
            let local_id = DiscussionRecordId::generate();
            let anchor = CollaborationAnchor::Symbol {
                state_id: discussion.opened_against_state.unwrap_or(head_state),
                path: discussion.file.clone(),
                symbol: discussion.symbol.clone(),
            };
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
                },
            )?;
            // Record the mapping immediately so a mid-materialization failure
            // resumes into the `Some` arm instead of orphaning the written ops.
            let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
            repo_mirror.discussions.push(MirrorEntry {
                local_id: local_id.to_string(),
                server_id: discussion.id.clone(),
                links: vec![TurnLink {
                    local_op_id: open_op.to_string_full(),
                    server_ordinal: 0,
                }],
            });
            let index = repo_mirror.discussions.len() - 1;

            let mut heads = vec![open_op];
            for (ordinal, turn) in discussion.turns.iter().enumerate().skip(1) {
                let op_id = write_local_operation(
                    store,
                    local_id,
                    heads.clone(),
                    turn_attribution(turn),
                    turn_ms(turn),
                    CollaborationOperationBodyV1::AppendTurn { turn: turn_body(turn)? },
                )?;
                heads = vec![op_id];
                push_link(mirror, repo_path, index, op_id.to_string_full(), ordinal);
            }
            Ok(true)
        }
        Some(index) => {
            let local_id: DiscussionRecordId = repo_mirror.discussions[index]
                .local_id
                .parse()
                .map_err(|e| anyhow!("mirror map has an invalid local discussion id: {e}"))?;
            let linked_ordinals: HashSet<usize> = repo_mirror.discussions[index]
                .links
                .iter()
                .map(|link| link.server_ordinal)
                .collect();
            let linked_local: HashSet<String> = repo_mirror.discussions[index]
                .links
                .iter()
                .map(|link| link.local_op_id.clone())
                .collect();

            let existing = store
                .materialize_discussion(&local_id)
                .context("materialize mirrored discussion")?
                .ok_or_else(|| anyhow!("mirrored discussion {local_id} missing locally"))?;
            let mut heads: Vec<CollabOpId> = existing.heads.iter().copied().collect();
            // Unlinked local turns available to reconcile against server turns by
            // body (mirror-loss / re-sync guard — see the module note on why body
            // is the identity here).
            let mut available: Vec<(CollabOpId, String)> = existing
                .turns
                .iter()
                .filter(|(op, _)| !linked_local.contains(&op.to_string_full()))
                .map(|(op, turn)| (*op, turn.body.clone()))
                .collect();

            let mut changed = false;
            for (ordinal, turn) in discussion.turns.iter().enumerate() {
                if linked_ordinals.contains(&ordinal) {
                    continue;
                }
                if let Some(pos) = available.iter().position(|(_, body)| body == &turn.body) {
                    // Already present locally (we authored+pushed it, or a prior
                    // partial sync) — link, don't duplicate.
                    let (op_id, _) = available.swap_remove(pos);
                    push_link(mirror, repo_path, index, op_id.to_string_full(), ordinal);
                    changed = true;
                    continue;
                }
                let op_id = write_local_operation(
                    store,
                    local_id,
                    heads.clone(),
                    turn_attribution(turn),
                    turn_ms(turn),
                    CollaborationOperationBodyV1::AppendTurn { turn: turn_body(turn)? },
                )?;
                heads = vec![op_id];
                push_link(mirror, repo_path, index, op_id.to_string_full(), ordinal);
                changed = true;
            }
            Ok(changed)
        }
    }
}

fn push_link(
    mirror: &mut HostedMirror,
    repo_path: &str,
    index: usize,
    local_op_id: String,
    server_ordinal: usize,
) {
    if let Some(entry) = mirror
        .repos
        .get_mut(repo_path)
        .and_then(|repo_mirror| repo_mirror.discussions.get_mut(index))
    {
        entry.links.push(TurnLink {
            local_op_id,
            server_ordinal,
        });
    }
}

/// Whether the op at `op_id` was authored by the local principal. On any
/// uncertainty (no local principal, unreadable op) we do not filter — the link
/// check remains the primary "already on the server" guard.
fn author_is_self(
    store: &CollaborationStore,
    op_id: &CollabOpId,
    self_attr: Option<&Attribution>,
) -> bool {
    let Some(self_attr) = self_attr else {
        return true;
    };
    match store.read_operation(op_id) {
        Ok(Some(decoded)) => principals_match(&decoded.operation.author, self_attr),
        _ => true,
    }
}

fn principals_match(a: &Attribution, b: &Attribution) -> bool {
    a.principal.name == b.principal.name && a.principal.email == b.principal.email
}

fn write_local_operation(
    store: &CollaborationStore,
    discussion_id: DiscussionRecordId,
    parents: Vec<CollabOpId>,
    author: Attribution,
    occurred_at_ms: i64,
    body: CollaborationOperationBodyV1,
) -> Result<CollabOpId> {
    let key = CollaborationIdempotencyKey::new(uuid::Uuid::new_v4().to_string())
        .map_err(|e| anyhow!("invalid idempotency key: {e}"))?;
    let operation =
        CollaborationOperationEnvelope::new(discussion_id, parents, key, author, occurred_at_ms, body)
            .map_err(|e| anyhow!("build collaboration operation: {e}"))?;
    Ok(store
        .write_operation(&operation)
        .context("write collaboration operation")?
        .operation_id)
}

fn turn_body(turn: &HostedDiscussionTurn) -> Result<DiscussionTurnV1> {
    DiscussionTurnV1::new(turn.body.clone()).map_err(|e| anyhow!("invalid discussion turn: {e}"))
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
        now_ms()
    }
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
