// SPDX-License-Identifier: Apache-2.0
//! Hosted replication for native discussion operations.
//!
//! Push signs the original local operation envelope, stores the exact signed
//! record in the durable mirror, and publishes those bytes unchanged. Pull
//! verifies the signed records returned by the server and writes the original
//! operation bytes into the local [`CollaborationStore`]. This preserves the
//! authored anchor, title, actor, scope, causal parents, and occurrence time.
//!
//! Hosted projections are never reconstructed as authored evidence. If a
//! projection exists without its native signed operation, replication reports
//! the repository as incomplete.

#![cfg(feature = "client")]

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    fs::OpenOptions,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use api::heddle::api::v1alpha2::SignedRecord;
use objects::{
    fs_atomic::write_file_atomic,
    lock::RepoLock,
    object::{
        Attribution, CollabOpId, CollaborationAnchor, CollaborationIdempotencyKey,
        CollaborationOperationBodyV1, CollaborationOperationEnvelope, CollaborationResolution,
        CollaborationRevision, DiscussionRecordId, DiscussionTurnV1, MaterializedDiscussion,
        Principal, StateId, VisibilityTier, thread_replication::ThreadOperationBody,
    },
    store::ObjectStore,
};
use prost::Message;
use repo::{CollaborationStore, Repository};
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
    /// Native signed originals prepared before delivery. These bytes are the
    /// retry source of truth and are retained as authored evidence.
    #[serde(default)]
    native_operations: Vec<PreparedNativeOperation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PreparedNativeOperation {
    local_operation_id: String,
    signed_record: Vec<u8>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone)]
struct LocalTurn {
    operation_id: CollabOpId,
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
            operation_id: *op_id,
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

fn validate_replicable_anchor(repo: &Repository, anchor: &CollaborationAnchor) -> Result<()> {
    let state = match anchor {
        CollaborationAnchor::State { state_id }
        | CollaborationAnchor::Path { state_id, .. }
        | CollaborationAnchor::Symbol { state_id, .. } => Some(*state_id),
        CollaborationAnchor::Source { source } => match source.revision {
            CollaborationRevision::State { state_id } => Some(state_id),
            CollaborationRevision::GitCommit { .. } => None,
        },
        CollaborationAnchor::Repository => None,
        CollaborationAnchor::Change { .. } => {
            return Err(anyhow!(
                "change-anchored discussion replication is incomplete: an exact source revision is required"
            ));
        }
    };
    if let Some(state) = state
        && repo
            .store()
            .get_state(&state)
            .context("load discussion anchor state")?
            .is_none()
    {
        return Err(anyhow!(
            "discussion anchor state {state} is unavailable; native replication is incomplete"
        ));
    }
    Ok(())
}

async fn publish_authored_append(
    client: &mut HostedClient,
    store: &CollaborationStore,
    repo: &Repository,
    repo_path: &str,
    mirror: &mut HostedMirror,
    server_id: &str,
    turn: &LocalTurn,
) -> Result<HostedDiscussion> {
    let signed = if let Some(prepared) = mirror.repos.get(repo_path).and_then(|repository| {
        repository
            .native_operations
            .iter()
            .find(|operation| operation.local_operation_id == turn.operation_id.to_string_full())
    }) {
        SignedRecord::decode(prepared.signed_record.as_slice())
            .context("decode prepared native discussion append")?
    } else {
        let authored = store
            .read_operation(&turn.operation_id)
            .context("read authored discussion turn")?
            .ok_or_else(|| anyhow!("authored discussion turn {} is missing", turn.operation_id))?
            .operation;
        let signed = client
            .prepare_append_discussion_operation(repo_path, server_id, &authored)
            .await?;
        mirror
            .repos
            .entry(repo_path.to_string())
            .or_default()
            .native_operations
            .push(PreparedNativeOperation {
                local_operation_id: turn.operation_id.to_string_full(),
                signed_record: signed.encode_to_vec(),
            });
        save_mirror(repo.heddle_dir(), mirror)?;
        signed
    };
    client
        .publish_append_discussion_operation(
            signed,
            append_op_id(repo_path, server_id, &turn.turn_id),
        )
        .await
        .map_err(Into::into)
}

/// Publish every supported locally authored discussion anchor through the
/// hosted `CollaborationService`. The durable signed record is saved before
/// delivery. Per-discussion failures are collected and returned as an explicit
/// incomplete replication result after other discussions have been attempted.
pub async fn push_discussions(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    default_thread_ref: &str,
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
    let mut incomplete = Vec::new();

    for (discussion_id, discussion) in &materialized.discussions {
        let result = push_one(
            client,
            &store,
            repo,
            repo_path,
            default_thread_ref,
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
                client.warn(
                    "hosted_discussion_sync_failed",
                    format!("hosted discussion {discussion_id}: {error:#}"),
                );
                incomplete.push(format!("{discussion_id}: {error:#}"));
            }
        }
    }

    if !incomplete.is_empty() {
        return Err(anyhow!(
            "hosted discussion replication incomplete: {}",
            incomplete.join("; ")
        ));
    }
    Ok(synced)
}

#[allow(clippy::too_many_arguments)]
async fn push_one(
    client: &mut HostedClient,
    store: &CollaborationStore,
    repo: &Repository,
    repo_path: &str,
    default_thread_ref: &str,
    mirror: &mut HostedMirror,
    self_attr: Option<&Attribution>,
    local_id: &str,
    discussion: &MaterializedDiscussion,
) -> Result<bool> {
    validate_replicable_anchor(repo, &discussion.anchor)?;

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
    let mut candidates: Vec<LocalTurn> = Vec::new();
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
        candidates.push(turn.clone());
    }
    if skipped_foreign > 0 {
        return Err(anyhow!(
            "discussion {local_id} has {skipped_foreign} unlinked turn(s) not attributed to the local principal and no retained native signed operation; replication is incomplete"
        ));
    }
    let (_index, _server_id, changed, _hosted) = match entry_index {
        None => {
            if candidates.is_empty() {
                return Ok(false);
            }
            let open_turn = candidates[0].clone();
            let open_turn_id = open_turn.turn_id.clone();
            let open_operation_id = discussion
                .turns
                .first()
                .map(|(operation, _)| *operation)
                .ok_or_else(|| anyhow!("discussion {local_id} has no opening operation"))?;
            let authored = store
                .read_operation(&open_operation_id)
                .context("read authored discussion open")?
                .ok_or_else(|| anyhow!("authored discussion open {open_operation_id} is missing"))?
                .operation;
            if !matches!(authored.body, CollaborationOperationBodyV1::Open { .. }) {
                return Err(anyhow!(
                    "discussion {local_id} uses an unsupported imported opening shape; native replication is incomplete"
                ));
            }
            let signed = if let Some(prepared) =
                mirror.repos.get(repo_path).and_then(|repository| {
                    repository.native_operations.iter().find(|operation| {
                        operation.local_operation_id == open_operation_id.to_string_full()
                    })
                }) {
                SignedRecord::decode(prepared.signed_record.as_slice())
                    .context("decode prepared native discussion operation")?
            } else {
                let signed = client
                    .prepare_open_discussion_operation(repo_path, default_thread_ref, &authored)
                    .await
                    .with_context(|| {
                        format!("prepare native discussion {local_id} in its requested scope")
                    })?;
                mirror
                    .repos
                    .entry(repo_path.to_string())
                    .or_default()
                    .native_operations
                    .push(PreparedNativeOperation {
                        local_operation_id: open_operation_id.to_string_full(),
                        signed_record: signed.encode_to_vec(),
                    });
                save_mirror(repo.heddle_dir(), mirror)?;
                signed
            };
            let mut hosted = client
                .publish_open_discussion_operation(signed, open_op_id(repo_path, local_id))
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
            for turn in &candidates[1..] {
                hosted = publish_authored_append(
                    client, store, repo, repo_path, mirror, &server_id, turn,
                )
                .await
                .with_context(|| format!("append hosted turn for {local_id}"))?;
                push_link(
                    mirror,
                    repo_path,
                    index,
                    turn.turn_id.clone(),
                    hosted.turns.len().saturating_sub(1),
                    hosted
                        .turns
                        .last()
                        .and_then(|turn| (!turn.turn_id.is_empty()).then(|| turn.turn_id.clone())),
                );
            }
            // OpenDiscussion / AppendTurn echo the full discussion (≥2 turns
            // after append). Adopt uses this last snapshot.
            (index, server_id, true, Some(hosted))
        }
        Some(index) => {
            let server_id = mirror.repos[repo_path].discussions[index].server_id.clone();
            let mut hosted = None;
            for turn in &candidates {
                let echoed = publish_authored_append(
                    client, store, repo, repo_path, mirror, &server_id, turn,
                )
                .await
                .with_context(|| format!("append hosted turn for {local_id}"))?;
                push_link(
                    mirror,
                    repo_path,
                    index,
                    turn.turn_id.clone(),
                    echoed.turns.len().saturating_sub(1),
                    echoed
                        .turns
                        .last()
                        .and_then(|turn| (!turn.turn_id.is_empty()).then(|| turn.turn_id.clone())),
                );
                // Weft AppendTurn/OpenDiscussion echoes the full discussion
                // (≥2 turns after append). Adopt uses this snapshot so a
                // last-turn-only echo cannot under-adopt.
                hosted = Some(echoed);
            }
            (index, server_id, !candidates.is_empty(), hosted)
        }
    };

    if discussion.resolution.is_some() {
        return Err(anyhow!(
            "discussion {local_id} has a legacy resolution without a persisted native signed operation; replication is incomplete"
        ));
    }
    Ok(changed)
}

/// Fetch native signed discussion operations for `against` (or repository
/// HEAD). A hosted projection without its signed source is incomplete.
///
/// `against` is the pulled/cloned tip. Clone publishes HEAD only after this
/// call, and `heddle pull feature --local-thread feature` leaves HEAD on the
/// current checkout — ListByState must not re-read HEAD.
///
/// Repo-wide: clone/pull and unfiltered `discuss wait`.
pub async fn pull_discussions(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    against: Option<StateId>,
) -> Result<usize> {
    pull_discussions_filtered(repo, client, repo_path, against, None).await
}

/// Fetch native signed operations for `discuss wait --thread`, keeping only
/// discussions whose thread name or stable id matches the requested thread.
pub async fn pull_discussions_for_thread(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    thread: &str,
    thread_id: &str,
) -> Result<usize> {
    let filter = (!thread.is_empty() || !thread_id.is_empty()).then_some((thread, thread_id));
    pull_discussions_filtered(repo, client, repo_path, None, filter).await
}

async fn pull_discussions_filtered(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    against: Option<StateId>,
    thread_filter: Option<(&str, &str)>,
) -> Result<usize> {
    let signed = client
        .list_discussion_operations(repo_path)
        .await
        .context("observe native discussion operations")?;
    if !signed.is_empty() {
        let store =
            CollaborationStore::open(repo.heddle_dir()).context("open collaboration store")?;
        let _guard = lock_mirror_write(repo.heddle_dir())?;
        let mut mirror = load_mirror(repo.heddle_dir())?;
        let mut changed = HashSet::new();
        for record in signed {
            let verified = thread_api::collaboration::verify(&record)
                .map_err(|error| anyhow!(error.to_string()))?;
            let ThreadOperationBody::Discussion(bytes) = verified.body else {
                continue;
            };
            let decoded = CollaborationOperationEnvelope::decode(&bytes)
                .map_err(|error| anyhow!(error.to_string()))?;
            if let Some((thread, thread_id)) = thread_filter {
                let by_name = matches!(
                    &decoded.operation.body,
                    CollaborationOperationBodyV1::Open { thread_ref, .. }
                        if thread_ref.as_deref() == Some(thread)
                );
                let by_id = decoded
                    .operation
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.scope.thread)
                    .is_some_and(|id| id.to_string() == thread_id);
                if !by_name && !by_id {
                    continue;
                }
            }
            store
                .write_operation_bytes(&bytes)
                .context("store native signed discussion operation")?;
            changed.insert(decoded.operation.discussion_id);
            let local_operation_id = decoded.operation_id.to_string_full();
            let repository = mirror.repos.entry(repo_path.to_string()).or_default();
            if !repository
                .native_operations
                .iter()
                .any(|operation| operation.local_operation_id == local_operation_id)
            {
                repository.native_operations.push(PreparedNativeOperation {
                    local_operation_id,
                    signed_record: record.encode_to_vec(),
                });
            }
        }
        save_mirror(repo.heddle_dir(), &mirror)?;
        return Ok(changed.len());
    }
    let Some((_head_state, hosted)) =
        listed_hosted_discussions(repo, client, repo_path, against, thread_filter).await?
    else {
        // weft#638: a repo with no HEAD cannot resolve a state to list against
        // unless the caller passed the pulled/cloned tip.
        return Ok(0);
    };
    if hosted.is_empty() {
        return Ok(0);
    }
    Err(anyhow!(
        "hosted discussions omitted their signed operations; replication is incomplete"
    ))
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

    let listed = client
        .list_discussions_by_state(repo_path, state.change_id, "all")
        .await
        .context("observe hosted discussions")?;
    let hosted = match thread_filter {
        Some((thread, thread_id)) => listed
            .into_iter()
            .filter(|discussion| discussion_matches_wait_thread(discussion, thread, thread_id))
            .collect(),
        None => listed,
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

#[cfg(test)]
fn hosted_discussion_fixture(discussion: objects::object::Discussion) -> HostedDiscussion {
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
                causal_id: Vec::new(),
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
        causal_heads: Vec::new(),
        version: Vec::new(),
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
            let first = &discussion.turns[0];
            let open_op = write_local_operation(
                store,
                local_id,
                Vec::new(),
                turn_attribution(first),
                turn_ms(first),
                hosted_turn_body(discussion, first, 0, head_state)?,
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
                    hosted_turn_body(discussion, turn, list_index, head_state)?,
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
    if let Some(index) = mirror.repos.get(repo_path).and_then(|repo_mirror| {
        repo_mirror
            .discussions
            .iter()
            .position(|entry| entry.server_id == discussion.id)
    }) {
        let local_id = mirror.repos[repo_path].discussions[index].local_id.clone();
        if let Some(entry) = mirror
            .repos
            .get_mut(repo_path)
            .and_then(|repo_mirror| repo_mirror.discussions.get_mut(index))
            && adopt_hosted_canonical_turns(store, head_state, &local_id, discussion, entry)?
        {
            changed = true;
        }
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
            Some(CollaborationResolution::Annotation { .. })
        )
    {
        // Local already bound a real context annotation id. Weft's
        // ResolveDiscussion.IntoAnnotation mints a distinct hc- that is
        // not a context row (heddle#1731). Keep the local id.
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
    let has_symbol = !discussion.file.is_empty() && !discussion.symbol.is_empty();
    // Coordination has no source anchor. An empty-anchor fetch
    // must still Open — failing validation here would not advance the
    // watermark and every restart would die on the same event.
    if !has_symbol {
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

fn hosted_turn_body(
    discussion: &HostedDiscussion,
    turn: &HostedDiscussionTurn,
    index: usize,
    head_state: StateId,
) -> Result<CollaborationOperationBodyV1> {
    if index == 0 {
        Ok(CollaborationOperationBodyV1::Open {
            blocking: false,
            title: derive_title(&turn.body, &discussion.symbol),
            anchor: hosted_open_anchor(discussion, head_state),
            visibility: parse_visibility_token(&discussion.visibility),
            turn: turn_body(turn)?,
            thread_ref: discussion.thread_ref.clone(),
        })
    } else {
        Ok(CollaborationOperationBodyV1::AppendTurn {
            turn: turn_body(turn)?,
        })
    }
}

fn parse_linked_op_id(local_turn_id: &str) -> Result<CollabOpId> {
    let op = local_turn_id
        .split_once('#')
        .map(|(op, _)| op)
        .unwrap_or(local_turn_id);
    op.parse()
        .map_err(|error| anyhow!("mirror turn link has an invalid CollabOpId {op:?}: {error}"))
}

/// True when every linked published turn already carries `turn_op_key` for
/// its server ordinal — clone/bootstrap/prior-adopt materialization, not an
/// originator-local v4 / `--op-id` key.
fn linked_turns_already_use_hosted_keys(
    store: &CollaborationStore,
    hosted: &HostedDiscussion,
    entry: &MirrorEntry,
) -> Result<bool> {
    if entry.links.is_empty() {
        return Ok(false);
    }
    for link in &entry.links {
        let op_id = parse_linked_op_id(&link.local_turn_id)?;
        let Some(decoded) = store
            .read_operation(&op_id)
            .context("read linked op before hosted CollabOpId adoption")?
        else {
            return Ok(false);
        };
        let hosted_key = turn_op_key(&hosted.id, link.server_ordinal)?;
        if decoded.operation.idempotency_key != hosted_key {
            return Ok(false);
        }
    }
    Ok(true)
}

fn fill_missing_server_turn_ids(entry: &mut MirrorEntry, hosted: &HostedDiscussion) {
    for link in &mut entry.links {
        if link.server_turn_id.is_some() {
            continue;
        }
        link.server_turn_id = hosted.turns.iter().enumerate().find_map(|(index, turn)| {
            (server_ordinal(turn, index) == link.server_ordinal)
                .then(|| server_turn_id(turn))
                .flatten()
        });
    }
}

/// The adopt `push_one` runs after keeping the last OpenDiscussion/AppendTurn
/// echo. Weft echoes the **full** discussion (≥2 turns after append). Empty
/// `turns` is a test-server / older-weft no-op. A short echo is refused so a
/// last-turn-only snapshot cannot retire local turns it cannot replace.
#[cfg(test)]
fn adopt_from_push_echo(
    store: &CollaborationStore,
    head_state: StateId,
    local_id: &str,
    hosted: &HostedDiscussion,
    entry: &mut MirrorEntry,
) -> Result<bool> {
    if hosted.turns.is_empty() {
        return Ok(false);
    }
    if hosted.turns.len() < entry.links.len() {
        return Err(anyhow!(
            "hosted echo for {local_id} has {} turn(s) but this discussion has {} linked op(s); refusing adopt so a partial echo cannot drop local turns",
            hosted.turns.len(),
            entry.links.len(),
        ));
    }
    adopt_hosted_canonical_turns(store, head_state, local_id, hosted, entry)
}

/// Replace local-only published turn ops with the envelopes a clone would
/// materialize from `hosted`. No-op when the local linked ops already use
/// hosted `turn_op_key`s (clone/bootstrap/prior adopt), or when unpublished
/// local turns still parent the chain. A later doorbell GetDiscussion is
/// often a thinner snapshot than the first hosted materialization; reminting
/// from it would disagree with the live replay path.
fn adopt_hosted_canonical_turns(
    store: &CollaborationStore,
    head_state: StateId,
    local_id: &str,
    hosted: &HostedDiscussion,
    entry: &mut MirrorEntry,
) -> Result<bool> {
    if hosted.turns.is_empty() || hosted.id.is_empty() {
        return Ok(false);
    }
    let local_record: DiscussionRecordId = local_id
        .parse()
        .map_err(|error| anyhow!("invalid local discussion id {local_id}: {error}"))?;
    let Some(existing) = store
        .materialize_discussion(&local_record)
        .context("materialize discussion before hosted CollabOpId adoption")?
    else {
        return Ok(false);
    };

    let linked_ops: HashSet<CollabOpId> = entry
        .links
        .iter()
        .map(|link| parse_linked_op_id(&link.local_turn_id))
        .collect::<Result<HashSet<_>>>()?;
    if existing
        .turns
        .iter()
        .any(|(op_id, _)| !linked_ops.contains(op_id))
    {
        // An unpushed local turn still parents the published prefix. Rewriting
        // the prefix would orphan that child; wait until it is published.
        return Ok(false);
    }

    if linked_turns_already_use_hosted_keys(store, hosted, entry)? {
        // Already the hosted key space. Keep those envelopes — do not rebuild
        // from this snapshot (GetDiscussion may omit opened_against_state).
        fill_missing_server_turn_ids(entry, hosted);
        return Ok(false);
    }

    let mut parents = Vec::new();
    let mut canonical = Vec::with_capacity(hosted.turns.len());
    let mut new_links = Vec::with_capacity(hosted.turns.len());
    for (index, turn) in hosted.turns.iter().enumerate() {
        let ordinal = server_ordinal(turn, index);
        let envelope = CollaborationOperationEnvelope::new(
            local_record,
            parents.clone(),
            turn_op_key(&hosted.id, ordinal)?,
            turn_attribution(turn),
            turn_ms(turn),
            hosted_turn_body(hosted, turn, index, head_state)?,
        )
        .context("build hosted-canonical collaboration operation")?;
        let bytes = envelope
            .encode()
            .map_err(|error| anyhow!("encode hosted-canonical operation: {error}"))?;
        let id = CollabOpId::for_bytes(&bytes);
        new_links.push(TurnLink {
            local_turn_id: turn_identity(&id, 0),
            server_ordinal: ordinal,
            server_turn_id: server_turn_id(turn),
        });
        canonical.push((id, envelope));
        parents = vec![id];
    }

    let already_canonical = linked_ops.len() == canonical.len()
        && canonical.iter().all(|(id, _)| linked_ops.contains(id));
    if already_canonical {
        // Link metadata only (e.g. filling server_turn_id). The op-log is
        // unchanged — doorbell replay must stay Unchanged.
        if entry.links != new_links {
            entry.links = new_links;
        }
        return Ok(false);
    }

    let retire: Vec<CollabOpId> = linked_ops
        .into_iter()
        .filter(|id| !canonical.iter().any(|(canonical_id, _)| canonical_id == id))
        .collect();
    let write: Vec<CollaborationOperationEnvelope> = canonical
        .into_iter()
        .map(|(_, envelope)| envelope)
        .collect();
    store
        .replace_operations(&retire, &write)
        .context("adopt hosted-canonical collaboration operations")?;
    entry.links = new_links;
    Ok(true)
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
    use api::heddle::api::common::{RepoEvent, StateId as ProtoStateId};
    use objects::object::{
        AnnotationKind, Attribution, CollaborationAnchor, CollaborationIdempotencyKey,
        CollaborationOperationBodyV1, CollaborationOperationEnvelope, CollaborationResolution,
        ContentHash, Discussion, DiscussionRecordId, DiscussionResolution, DiscussionTurn,
        DiscussionTurnV1, LegacyDiscussionId, LegacyDiscussionResolutionV1, LegacySourceLocator,
        Principal, State, StateAttachmentId, StateId, SymbolAnchor, Tree, VisibilityTier,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::{
        client::discussion_live::{DiscussionEventOutcome, consume_discussion_event},
        hosted_runtime::hosted::{
            HostedDiscussion as ProtoDiscussion, HostedDiscussionTurn as ProtoTurn,
            test_server::CollaborationFixture,
        },
    };

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
            operation_id: CollabOpId::from_bytes([1; 32]),
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
            causal_id: Vec::new(),
        }
    }

    // F1: identical bodies from DIFFERENT authors must NOT reconcile — the
    // server turn materializes as its own distinct turn; the local turn is left
    // unlinked (so push will still publish it). Body equality alone never links.
    #[test]
    fn reconcile_rejects_identical_body_across_authors() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
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
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let available = vec![local("ship it", "alice-local", "alice@x", true, 111)];
        let st = server("ship it", "alice", "", 9); // server stamped our hosted username
        assert_eq!(reconcile(&available, &st, Some("alice")), Some(0));
    }

    // F1 rule (ii): a turn we previously PULLED (local op copied the server
    // author + posted_at verbatim) reconciles.
    #[test]
    fn reconcile_links_turn_we_pulled() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
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
        let _process_env_guard = crate::test_process_env::shared_blocking();
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
        let _process_env_guard = crate::test_process_env::shared_blocking();
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
    async fn clone_native_pull_rejects_projection_without_signed_operation() {
        let _process_env_guard = crate::test_process_env::shared().await;
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

        let discussion_id = DiscussionRecordId::generate().to_string();
        let proto = ProtoDiscussion {
            id: discussion_id.clone(),
            file: "lib.rs".to_string(),
            symbol: "run".to_string(),
            visibility: "internal".to_string(),
            turns: vec![ProtoTurn {
                author_name: "Reviewer".to_string(),
                author_email: "reviewer@example.com".to_string(),
                body: "keep this invariant".to_string(),
                turn_id: "turn-open".to_string(),
                turn_seq: 1,
                posted_at_secs: 1_700_000_001,
                ..Default::default()
            }],
            ..ProtoDiscussion::default()
        };
        let mut fixture = CollaborationFixture::default();
        fixture.list.push(proto.clone());
        fixture.discussions.insert(discussion_id.clone(), proto);
        let (mut client, server, fixture) =
            crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;

        let error = pull_discussions(&repo, &mut client, "acme/widgets", Some(against))
            .await
            .unwrap_err();
        let observed = *fixture.list_requests.lock().unwrap();
        assert!(observed >= 1, "native pull must ObserveCollaboration");
        assert!(
            format!("{error:#}").contains("omitted their signed operations"),
            "projection-only discussion must be an explicit incomplete result: {error:#}"
        );

        client.close().await;
        server.await.unwrap();
    }

    // F3: no local principal ⇒ turns are NOT treated as ours (fail closed).
    #[test]
    fn collect_local_turns_fails_closed_without_self_principal() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
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
                blocking: false,
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

    #[test]
    fn unsupported_anchor_is_an_explicit_incomplete_result() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let error = validate_replicable_anchor(
            &repo,
            &CollaborationAnchor::Change {
                change_id: objects::object::ChangeId::from_bytes([7; 16]),
            },
        )
        .expect_err("change anchor needs an exact source revision");
        let message = error.to_string();
        assert!(message.contains("replication is incomplete"), "{message}");
        assert!(message.contains("exact source revision"), "{message}");
    }

    #[tokio::test]
    async fn push_discussion_opens_and_appends_native_operations() {
        let _process_env_guard = crate::test_process_env::shared().await;
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
                blocking: false,
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
        write_local_operation(
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
        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
        assert_eq!(
            push_discussions(&repo, &mut client, "acme/widgets", "main")
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            push_discussions(&repo, &mut client, "acme/widgets", "main")
                .await
                .unwrap(),
            0,
            "mirrored native operations must not be sent again"
        );

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_push_pull_preserves_path_line_symbol_and_repository_discussions() {
        let _process_env_guard = crate::test_process_env::shared().await;
        let source_dir = TempDir::new().unwrap();
        let source = Repository::init_default(source_dir.path()).unwrap();
        std::fs::write(source_dir.path().join("lib.rs"), "pub fn run() {}\n").unwrap();
        let state = source
            .snapshot_with_attribution(
                Some("seed".to_string()),
                None,
                Attribution::human(Principal::new("Original Author", "author@test")),
            )
            .unwrap()
            .id();
        let store = CollaborationStore::open(source.heddle_dir()).unwrap();
        let author = source.get_attribution().unwrap();
        let anchors = vec![
            CollaborationAnchor::Path {
                state_id: state,
                path: "lib.rs".into(),
            },
            CollaborationAnchor::Source {
                source: objects::object::CollaborationSourceAnchor {
                    revision: CollaborationRevision::State { state_id: state },
                    path: "lib.rs".into(),
                    symbol_id: String::new(),
                    start_line: Some(1),
                    end_line: Some(1),
                    target: None,
                },
            },
            CollaborationAnchor::Symbol {
                state_id: state,
                path: "lib.rs".into(),
                symbol: "run".into(),
            },
            CollaborationAnchor::Repository,
        ];
        let mut expected = Vec::new();
        for (index, anchor) in anchors.into_iter().enumerate() {
            let discussion = DiscussionRecordId::generate();
            let occurred_at_ms = 1_700_000_000_123 + index as i64;
            let title = format!("authored title {index}");
            let thread_ref = (index != 0).then(|| "feature/provenance".to_string());
            let operation = CollaborationOperationEnvelope::new(
                discussion,
                Vec::new(),
                test_key(&format!("native-roundtrip-{index}")),
                author.clone(),
                occurred_at_ms,
                CollaborationOperationBodyV1::Open {
                    blocking: index % 2 == 0,
                    title: title.clone(),
                    anchor: anchor.clone(),
                    visibility: VisibilityTier::Internal,
                    turn: DiscussionTurnV1::new(format!("body {index}")).unwrap(),
                    thread_ref: thread_ref.clone(),
                },
            )
            .unwrap();
            let open_id = store.write_operation(&operation).unwrap().operation_id;
            let append_time = if index == 1 {
                let occurred = occurred_at_ms + 10_000;
                write_local_operation(
                    &store,
                    discussion,
                    vec![open_id],
                    author.clone(),
                    occurred,
                    CollaborationOperationBodyV1::AppendTurn {
                        turn: DiscussionTurnV1::new("authored follow-up").unwrap(),
                    },
                    test_key("native-roundtrip-append"),
                )
                .unwrap();
                Some(occurred)
            } else {
                None
            };
            expected.push((
                discussion,
                title,
                occurred_at_ms,
                anchor,
                thread_ref,
                append_time,
            ));
        }

        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
        assert_eq!(
            push_discussions(&source, &mut client, "acme/widgets", "feature/provenance",)
                .await
                .unwrap(),
            4
        );
        let mut source_signed: Vec<Vec<u8>> =
            load_mirror(source.heddle_dir()).unwrap().repos["acme/widgets"]
                .native_operations
                .iter()
                .map(|operation| operation.signed_record.clone())
                .collect();
        source_signed.sort();

        let destination_dir = TempDir::new().unwrap();
        let destination = Repository::init_default(destination_dir.path()).unwrap();
        assert_eq!(
            pull_discussions(&destination, &mut client, "acme/widgets", None)
                .await
                .unwrap(),
            4
        );
        let pulled_store = CollaborationStore::open(destination.heddle_dir()).unwrap();
        let materialized = pulled_store.materialize().unwrap();
        for (discussion_id, title, occurred_at_ms, authored_anchor, thread_ref, append_time) in
            expected
        {
            let discussion = materialized.discussions.get(&discussion_id).unwrap();
            assert_eq!(discussion.title, title);
            assert_eq!(discussion.thread_ref, thread_ref);
            let open_id = discussion.turns.first().unwrap().0;
            let open = pulled_store
                .read_operation(&open_id)
                .unwrap()
                .unwrap()
                .operation;
            assert_eq!(open.author, author);
            assert_eq!(open.occurred_at_ms, occurred_at_ms);
            let metadata = open.metadata.unwrap();
            assert!(metadata.scope.thread.is_some());
            assert!(!metadata.actor.principal_id.is_nil());
            match append_time {
                Some(expected_time) => {
                    assert_eq!(discussion.turns.len(), 2);
                    let append_id = discussion.turns[1].0;
                    let append = pulled_store
                        .read_operation(&append_id)
                        .unwrap()
                        .unwrap()
                        .operation;
                    assert_eq!(append.parents, vec![open_id]);
                    assert_eq!(append.author, author);
                    assert_eq!(append.occurred_at_ms, expected_time);
                }
                None => assert_eq!(discussion.turns.len(), 1),
            }
            match (authored_anchor, &discussion.anchor) {
                (CollaborationAnchor::Repository, CollaborationAnchor::Repository) => {}
                (
                    CollaborationAnchor::Path { state_id, path },
                    CollaborationAnchor::Source { source },
                ) => {
                    assert_eq!(source.revision, CollaborationRevision::State { state_id });
                    assert_eq!(source.path, path);
                    assert!(source.symbol_id.is_empty());
                    assert_eq!((source.start_line, source.end_line), (None, None));
                }
                (
                    CollaborationAnchor::Symbol {
                        state_id,
                        path,
                        symbol,
                    },
                    CollaborationAnchor::Source { source },
                ) => {
                    assert_eq!(source.revision, CollaborationRevision::State { state_id });
                    assert_eq!(source.path, path);
                    assert_eq!(source.symbol_id, symbol);
                    assert_eq!((source.start_line, source.end_line), (None, None));
                }
                (
                    CollaborationAnchor::Source { source: authored },
                    CollaborationAnchor::Source { source: pulled },
                ) => assert_eq!(*pulled, authored),
                (authored, pulled) => {
                    panic!("hosted canonical anchor lost {authored:?}: {pulled:?}")
                }
            }
        }
        let mut destination_signed: Vec<Vec<u8>> =
            load_mirror(destination.heddle_dir()).unwrap().repos["acme/widgets"]
                .native_operations
                .iter()
                .map(|operation| operation.signed_record.clone())
                .collect();
        destination_signed.sort();
        assert_eq!(
            destination_signed, source_signed,
            "pull must retain the byte-identical signed originals"
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
            turns: vec![HostedDiscussionTurn {
                author_name: "Ada".to_string(),
                author_email: "ada@example.com".to_string(),
                body: body.to_string(),
                posted_at_secs: 1_700_000_000,
                turn_id: turn_id.to_string(),
                turn_seq: 1,
                causal_id: Vec::new(),
            }],
            resolution,
            causal_heads: Vec::new(),
            version: Vec::new(),
        }
    }

    #[test]
    fn wait_thread_filter_matches_name_or_stamped_id() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
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
        let _process_env_guard = crate::test_process_env::shared_blocking();
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
        let _process_env_guard = crate::test_process_env::shared_blocking();
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
    fn pushed_real_annotation_echo_keeps_local_context_id() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
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
                resolution: CollaborationResolution::Annotation {
                    annotation_id: "local-context-ann".to_string(),
                },
            },
            test_key("resolve-into-real-annotation"),
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
                    annotation_id: "hc-orphan-from-weft".to_string(),
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
            "a pushed Annotation echo must not surface as competing collab state"
        );
        assert_eq!(
            echoed.resolution,
            Some(CollaborationResolution::Annotation {
                annotation_id: "local-context-ann".to_string(),
            }),
            "weft's minted hc- must not replace the real local context id"
        );
    }

    #[test]
    fn concurrent_apply_does_not_drop_turn_links() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
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
        let _process_env_guard = crate::test_process_env::shared_blocking();
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

    fn discussion_fixture(id: &str, state: StateId) -> Discussion {
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
    #[test]
    fn rederived_turn_op_key_dedupes_on_retry() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let (_temp, repo, state) = seed_repo_with_state();
        let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
        let discussion_id = DiscussionRecordId::generate();
        let author = Attribution::human(Principal::new("Reviewer", "reviewer@example.com"));
        let body = || CollaborationOperationBodyV1::Open {
            blocking: false,
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

    // heddle#1695: originator-local CollabOpId (random key + local author +
    // wall-clock time) must become the hosted-canonical id a clone materializes
    // from the same weft snapshot. Two clones already agreed after #1677/#1697;
    // this is the hosted-clone path against an originator who opened locally.
    #[test]
    fn partial_push_echo_does_not_under_adopt() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let wire_id = DiscussionRecordId::generate();
        let (_originator_temp, originator_repo, originator_state) = seed_repo_with_state();
        let originator_store = CollaborationStore::open(originator_repo.heddle_dir()).unwrap();
        let originator_open = write_local_operation(
            &originator_store,
            wire_id,
            Vec::new(),
            Attribution::human(Principal::new("Local Author", "local@example.com")),
            1_111_111_111_111,
            CollaborationOperationBodyV1::Open {
                blocking: false,
                title: "local title".to_string(),
                anchor: CollaborationAnchor::Symbol {
                    state_id: originator_state,
                    path: "lib.rs".to_string(),
                    symbol: "run".to_string(),
                },
                visibility: VisibilityTier::Internal,
                turn: DiscussionTurnV1::new("keep this invariant").unwrap(),
                thread_ref: None,
            },
            test_key("originator-local-open"),
        )
        .unwrap();
        let originator_append = write_local_operation(
            &originator_store,
            wire_id,
            vec![originator_open],
            Attribution::human(Principal::new("Local Author", "local@example.com")),
            1_111_111_111_222,
            CollaborationOperationBodyV1::AppendTurn {
                turn: DiscussionTurnV1::new("and a follow-up").unwrap(),
            },
            test_key("originator-local-append"),
        )
        .unwrap();

        let mut partial =
            hosted_discussion_fixture(discussion_fixture(&wire_id.to_string(), originator_state));
        partial.turns.truncate(1);
        assert_eq!(partial.turns.len(), 1);

        let mut entry = MirrorEntry {
            local_id: wire_id.to_string(),
            server_id: wire_id.to_string(),
            links: vec![
                TurnLink {
                    local_turn_id: turn_identity(&originator_open, 0),
                    server_ordinal: 0,
                    server_turn_id: None,
                },
                TurnLink {
                    local_turn_id: turn_identity(&originator_append, 0),
                    server_ordinal: 1,
                    server_turn_id: None,
                },
            ],
            resolved_into_annotation_operation_id: None,
            pulled_resolution_key: None,
        };
        let before = originator_store.operation_ids().unwrap();
        let error = adopt_from_push_echo(
            &originator_store,
            originator_state,
            &wire_id.to_string(),
            &partial,
            &mut entry,
        )
        .expect_err("a one-turn echo must not adopt over two linked local turns");
        assert!(
            error.to_string().contains("partial echo"),
            "refuse must name the partial-echo hazard, got: {error}"
        );
        let after = originator_store.operation_ids().unwrap();
        assert_eq!(
            after, before,
            "partial echo must leave both local ops in place"
        );
        assert!(after.contains(&originator_open));
        assert!(after.contains(&originator_append));
    }

    // The remint site on the hosted-clone/pull path: originator already
    // published (mirror link exists) but still holds the local-only open op.
    // pull_one must adopt the hosted-canonical envelope so the originator
    // matches a fresh clone of the same snapshot.
    #[tokio::test]
    async fn doorbell_after_adopt_does_not_duplicate_the_first_turn() {
        let _process_env_guard = crate::test_process_env::shared().await;
        let wire_id = DiscussionRecordId::generate();
        let (_temp0, _repo0, anchor_state) = seed_repo_with_state();
        let mut hosted_snapshot = discussion_fixture(&wire_id.to_string(), anchor_state);
        hosted_snapshot.turns[0].author = Principal::new("preview-user", "");
        hosted_snapshot.turns[0].posted_at = 1_700_000_042;
        let hosted = hosted_discussion_fixture(hosted_snapshot.clone());

        let (_originator_temp, originator_repo, originator_state) = seed_repo_with_state();
        let originator_store = CollaborationStore::open(originator_repo.heddle_dir()).unwrap();
        let originator_open = write_local_operation(
            &originator_store,
            wire_id,
            Vec::new(),
            Attribution::human(Principal::new("Local Author", "local@example.com")),
            1_111_111_111_111,
            CollaborationOperationBodyV1::Open {
                blocking: false,
                title: "local title".to_string(),
                anchor: CollaborationAnchor::Symbol {
                    state_id: originator_state,
                    path: "lib.rs".to_string(),
                    symbol: "run".to_string(),
                },
                visibility: VisibilityTier::Internal,
                turn: DiscussionTurnV1::new("keep this invariant").unwrap(),
                thread_ref: None,
            },
            test_key("originator-local-open"),
        )
        .unwrap();
        let mut entry = MirrorEntry {
            local_id: wire_id.to_string(),
            server_id: wire_id.to_string(),
            links: vec![TurnLink {
                local_turn_id: turn_identity(&originator_open, 0),
                server_ordinal: 0,
                server_turn_id: None,
            }],
            resolved_into_annotation_operation_id: None,
            pulled_resolution_key: None,
        };
        assert!(
            adopt_hosted_canonical_turns(
                &originator_store,
                originator_state,
                &wire_id.to_string(),
                &hosted,
                &mut entry,
            )
            .unwrap(),
            "originator must adopt the hosted-canonical open before the doorbell"
        );
        let mut mirror = HostedMirror::default();
        mirror
            .repos
            .entry("acme/widgets".to_string())
            .or_default()
            .discussions
            .push(entry);
        save_mirror(originator_repo.heddle_dir(), &mirror).unwrap();

        let mut after_adopt: Vec<String> = originator_store
            .operation_ids()
            .unwrap()
            .into_iter()
            .map(|id| id.to_string())
            .collect();
        after_adopt.sort();
        assert_eq!(after_adopt.len(), 1, "adopt must leave a single Open");
        assert_ne!(
            after_adopt[0],
            originator_open.to_string(),
            "adopt must retire the local-only open"
        );

        let mut fixture = CollaborationFixture::default();
        fixture.discussions.insert(
            wire_id.to_string(),
            ProtoDiscussion {
                id: wire_id.to_string(),
                file: "lib.rs".to_string(),
                symbol: "run".to_string(),
                visibility: "internal".to_string(),
                turns: vec![ProtoTurn {
                    author_name: "preview-user".to_string(),
                    author_email: String::new(),
                    body: "keep this invariant".to_string(),
                    turn_id: "turn-open".to_string(),
                    turn_seq: 1,
                    posted_at_secs: 1_700_000_042,
                    ..Default::default()
                }],
                ..ProtoDiscussion::default()
            },
        );
        let (mut client, server, _fixture) =
            crate::hosted_runtime::hosted::test_server::start_with_collaboration(fixture).await;
        let replay = consume_discussion_event(
            &originator_repo,
            &mut client,
            "acme/widgets",
            &RepoEvent {
                event_id: 3,
                repo_id: "repo-1".to_string(),
                event_type: "discussion.opened".to_string(),
                payload_json: serde_json::json!({
                    "discussion_id": wire_id.to_string(),
                    "turn_id": "turn-open",
                    "turn_seq": 1,
                })
                .to_string(),
                new_state: Some(ProtoStateId {
                    value: vec![0x11; 32],
                }),
                ..RepoEvent::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            replay,
            DiscussionEventOutcome::Unchanged {
                discussion_id: wire_id.to_string(),
            },
            "doorbell after adopt must not remint"
        );

        let discussion = originator_store
            .materialize()
            .unwrap()
            .discussions
            .into_values()
            .next()
            .unwrap();
        assert_eq!(
            discussion.turns.len(),
            1,
            "doorbell must not duplicate the first turn"
        );
        let mut after_doorbell: Vec<String> = originator_store
            .operation_ids()
            .unwrap()
            .into_iter()
            .map(|id| id.to_string())
            .collect();
        after_doorbell.sort();
        assert_eq!(
            after_doorbell, after_adopt,
            "doorbell GetDiscussion must keep the adopted CollabOpId"
        );

        client.close().await;
        server.await.unwrap();
    }
}
