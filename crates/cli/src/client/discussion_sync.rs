// SPDX-License-Identifier: Apache-2.0
//! Hosted discussion sync bridge.
//!
//! Local discussions live in the append-only [`CollaborationStore`] op-log
//! (`.heddle/collaboration/ops`). The hosted weft `CollaborationService` speaks
//! a different, per-state `DiscussionsBlob` model (id-keyed discussions with a
//! linear turn list). This module bridges the two:
//!
//! * **Push (write path):** after a successful `heddle push`, replay every
//!   symbol-anchored local discussion to the server via the caller-authenticated
//!   `OpenDiscussion` / `AppendTurn` RPCs (enforce-mode signed — the CLI's
//!   existing signed hosted client). New turns authored offline ride the next
//!   push; #549 rejects attachments in the pack, so they cannot ride the pack.
//! * **Pull/clone (read path):** after a successful clone/pull, `ListByState`
//!   the head state's discussions and materialize any new ones (and any new
//!   turns) into the local op-log so `discuss list` / `discuss show` see them.
//!
//! A per-repo mirror map (`.heddle/collaboration/hosted-mirror.json`) records
//! the local↔server discussion id pairing and how many turns are known-synced,
//! so re-push / re-pull only ship the delta and stay idempotent.
//!
//! Scope: discussions only. `context` (annotations) and `review` anchor to the
//! same state-attachment seam and can follow this exact pattern (a hosted
//! ContextService / StateReviewService sync using the same mirror-map + delta
//! design) — deliberately not built here.
//!
//! Known limitations (linear-model bridge):
//! * Only `Symbol`-anchored discussions map to the hosted `PathSymbolRef`
//!   model; repository/state/change/path-anchored discussions are skipped.
//! * The op-log is a DAG; the hosted blob is a linear turn list. Turn sync is
//!   ordinal (common-prefix by count), which is correct for the non-branching
//!   authoring path but does not reconcile concurrent op-log branches.
//! * `resolve` / `reopen` are not yet mirrored (turns only).

#![cfg(feature = "client")]

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use objects::{
    fs_atomic::write_file_atomic,
    object::{
        Attribution, CollabOpId, CollaborationAnchor, CollaborationIdempotencyKey,
        CollaborationOperationBodyV1, CollaborationOperationEnvelope, DiscussionRecordId,
        DiscussionTurnV1, Principal, VisibilityTier,
    },
};
use objects::store::ObjectStore;
use repo::{CollaborationStore, Repository};
use serde::{Deserialize, Serialize};

use crate::client::HostedGrpcClient;
use heddle_client::grpc_hosted::HostedDiscussionTurn;

/// Deterministic namespace for the derived client-operation-ids so a retried
/// push replays (server-side idempotent) rather than duplicating a discussion.
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
    /// Number of turns known to be present on BOTH sides (common prefix length).
    synced_turns: usize,
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

fn op_operation_id(key: &str) -> String {
    uuid::Uuid::new_v5(&OP_NAMESPACE, key.as_bytes()).to_string()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Publish local symbol-anchored discussions (and any new turns) to the hosted
/// `CollaborationService`. Returns the number of discussions created or extended.
///
/// Best-effort: a per-discussion RPC failure aborts with context, but the
/// mirror map is only advanced for discussions that fully synced, so a re-push
/// resumes cleanly.
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

    let mut mirror = load_mirror(repo.heddle_dir())?;
    let mut synced = 0usize;

    for (discussion_id, discussion) in &materialized.discussions {
        let CollaborationAnchor::Symbol {
            state_id,
            path,
            symbol,
        } = &discussion.anchor
        else {
            // Only symbol-anchored discussions map to the hosted PathSymbolRef.
            continue;
        };
        let Some(state) = repo
            .store()
            .get_state(state_id)
            .context("load discussion anchor state")?
        else {
            continue;
        };
        let change_id = state.change_id;
        let visibility = discussion.visibility.as_str().to_string();
        let bodies: Vec<String> = discussion.turns.iter().map(|(_, t)| t.body.clone()).collect();
        if bodies.is_empty() {
            continue;
        }
        let local_id = discussion_id.to_string();

        let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
        let existing = repo_mirror
            .discussions
            .iter()
            .position(|entry| entry.local_id == local_id);

        match existing {
            None => {
                let hosted = client
                    .open_discussion(
                        repo_path,
                        change_id,
                        path,
                        symbol,
                        &bodies[0],
                        &visibility,
                        op_operation_id(&format!("open:{repo_path}:{local_id}")),
                    )
                    .await
                    .with_context(|| format!("open hosted discussion for {local_id}"))?;
                let server_id = hosted.id.clone();
                for (index, body) in bodies.iter().enumerate().skip(1) {
                    client
                        .append_turn(
                            repo_path,
                            &server_id,
                            body,
                            op_operation_id(&format!("append:{repo_path}:{local_id}:{index}")),
                        )
                        .await
                        .with_context(|| format!("append hosted turn {index} for {local_id}"))?;
                }
                let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
                repo_mirror.discussions.push(MirrorEntry {
                    local_id,
                    server_id,
                    synced_turns: bodies.len(),
                });
                synced += 1;
            }
            Some(index) => {
                let already = repo_mirror.discussions[index].synced_turns;
                if bodies.len() <= already {
                    continue;
                }
                let server_id = repo_mirror.discussions[index].server_id.clone();
                for (turn_index, body) in bodies.iter().enumerate().skip(already) {
                    client
                        .append_turn(
                            repo_path,
                            &server_id,
                            body,
                            op_operation_id(&format!("append:{repo_path}:{local_id}:{turn_index}")),
                        )
                        .await
                        .with_context(|| format!("append hosted turn {turn_index} for {local_id}"))?;
                }
                let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
                repo_mirror.discussions[index].synced_turns = bodies.len();
                synced += 1;
            }
        }
    }

    save_mirror(repo.heddle_dir(), &mirror)?;
    Ok(synced)
}

/// Fetch hosted discussions for the repository head and materialize any new
/// ones (and any new turns) into the local op-log. Returns the number of
/// discussions created or extended locally.
pub async fn pull_discussions(
    repo: &Repository,
    client: &mut HostedGrpcClient,
    repo_path: &str,
) -> Result<usize> {
    let Some(head_state) = repo.head().context("resolve repository head")? else {
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
        if discussion.turns.is_empty() {
            continue;
        }
        let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
        let existing = repo_mirror
            .discussions
            .iter()
            .position(|entry| entry.server_id == discussion.id);

        match existing {
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
                let mut head = write_local_operation(
                    &store,
                    local_id,
                    Vec::new(),
                    turn_attribution(first),
                    turn_ms(first),
                    CollaborationOperationBodyV1::Open {
                        title,
                        anchor,
                        visibility,
                        turn: DiscussionTurnV1::new(first.body.clone())
                            .map_err(|e| anyhow!("invalid discussion turn: {e}"))?,
                    },
                )?;
                for turn in &discussion.turns[1..] {
                    head = write_local_operation(
                        &store,
                        local_id,
                        vec![head],
                        turn_attribution(turn),
                        turn_ms(turn),
                        CollaborationOperationBodyV1::AppendTurn {
                            turn: DiscussionTurnV1::new(turn.body.clone())
                                .map_err(|e| anyhow!("invalid discussion turn: {e}"))?,
                        },
                    )?;
                }

                let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
                repo_mirror.discussions.push(MirrorEntry {
                    local_id: local_id.to_string(),
                    server_id: discussion.id.clone(),
                    synced_turns: discussion.turns.len(),
                });
                changed += 1;
            }
            Some(index) => {
                let already = repo_mirror.discussions[index].synced_turns;
                if discussion.turns.len() <= already {
                    continue;
                }
                let local_id: DiscussionRecordId = repo_mirror.discussions[index]
                    .local_id
                    .parse()
                    .map_err(|e| anyhow!("mirror map has an invalid local discussion id: {e}"))?;
                let existing_discussion = store
                    .materialize_discussion(&local_id)
                    .context("materialize mirrored discussion")?
                    .ok_or_else(|| anyhow!("mirrored discussion {local_id} missing locally"))?;
                let mut heads: Vec<CollabOpId> =
                    existing_discussion.heads.iter().copied().collect();
                for turn in &discussion.turns[already..] {
                    let id = write_local_operation(
                        &store,
                        local_id,
                        heads.clone(),
                        turn_attribution(turn),
                        turn_ms(turn),
                        CollaborationOperationBodyV1::AppendTurn {
                            turn: DiscussionTurnV1::new(turn.body.clone())
                                .map_err(|e| anyhow!("invalid discussion turn: {e}"))?,
                        },
                    )?;
                    heads = vec![id];
                }
                let repo_mirror = mirror.repos.entry(repo_path.to_string()).or_default();
                repo_mirror.discussions[index].synced_turns = discussion.turns.len();
                changed += 1;
            }
        }
    }

    save_mirror(repo.heddle_dir(), &mirror)?;
    Ok(changed)
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
    let operation = CollaborationOperationEnvelope::new(
        discussion_id,
        parents,
        key,
        author,
        occurred_at_ms,
        body,
    )
    .map_err(|e| anyhow!("build collaboration operation: {e}"))?;
    Ok(store
        .write_operation(&operation)
        .context("write collaboration operation")?
        .operation_id)
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
