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
//! * **Pull/clone (read path):** after a successful clone/pull, `ListByState`
//!   the head state's discussions and materialize any turns we do not already
//!   hold into the local op-log so `discuss list` / `discuss show` see them.
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
//! only server ordinals not yet linked. Client operation ids are derived from
//! the stable turn id, so a retry replays instead of conflicting.
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
//! `resolve`/`reopen` are not yet mirrored (turns only).

#![cfg(feature = "client")]

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use heddle_client::hosted::{HostedDiscussion, HostedDiscussionTurn};
use objects::{
    fs_atomic::write_file_atomic,
    object::{
        Attribution, CollabOpId, CollaborationAnchor, CollaborationIdempotencyKey,
        CollaborationOperationBodyV1, CollaborationOperationEnvelope, DiscussionRecordId,
        DiscussionTurnV1, MaterializedDiscussion, Principal, StateId, VisibilityTier,
    },
    store::ObjectStore,
};
use repo::{CollaborationStore, Repository, mark_legacy_discussions_migrated};
use serde::{Deserialize, Serialize};

use crate::client::HostedClient;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TurnLink {
    /// Local turn id: `{CollabOpId}#{index-within-op}` — the stable turn
    /// identity (unique even when a `LegacyImported` op carries many turns).
    local_turn_id: String,
    /// Position of the turn in the server's linear turn list.
    server_ordinal: usize,
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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
            author_name: principal.name.clone(),
            author_email: principal.email.clone(),
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
            crate::cli::style::warn_marker(),
        );
    }
    if candidates.is_empty() {
        return Ok(false);
    }

    match entry_index {
        None => {
            let (open_turn_id, open_body) = candidates[0].clone();
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
                    local_turn_id: open_turn_id,
                    server_ordinal: 0,
                }],
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
                );
            }
            Ok(true)
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
                );
            }
            Ok(true)
        }
    }
}

/// Fetch hosted discussions for the repository head and materialize any turns we
/// do not already hold. Saves the mirror after each discussion and continues
/// past a per-discussion failure.
pub async fn pull_discussions(
    repo: &Repository,
    client: &mut HostedClient,
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
    // Our own hosted principal name, so reconciliation can recognize the turns
    // we pushed (weft stamps `Principal::new(username, "")`).
    let hosted_username = client.authenticated_username();

    let store = CollaborationStore::open(repo.heddle_dir()).context("open collaboration store")?;
    let self_attr = repo.get_attribution().ok();
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
                    crate::cli::style::warn_marker(),
                    discussion.id
                );
            }
        }
    }

    Ok(changed)
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
                    local_turn_id: turn_identity(&open_op, 0),
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
                    CollaborationOperationBodyV1::AppendTurn {
                        turn: turn_body(turn)?,
                    },
                )?;
                heads = vec![op_id];
                push_link(mirror, repo_path, index, turn_identity(&op_id, 0), ordinal);
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
            for (ordinal, server_turn) in discussion.turns.iter().enumerate() {
                if linked_ordinals.contains(&ordinal) {
                    continue;
                }
                if let Some(pos) = reconcile(&available, server_turn, hosted_username) {
                    let local = available.swap_remove(pos);
                    push_link(mirror, repo_path, index, local.turn_id, ordinal);
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
                )?;
                heads = vec![op_id];
                push_link(mirror, repo_path, index, turn_identity(&op_id, 0), ordinal);
                changed = true;
            }
            Ok(changed)
        }
    }
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
) {
    if let Some(entry) = mirror
        .repos
        .get_mut(repo_path)
        .and_then(|repo_mirror| repo_mirror.discussions.get_mut(index))
    {
        entry.links.push(TurnLink {
            local_turn_id,
            server_ordinal,
        });
    }
}

fn principals_match(a: &Principal, b: &Principal) -> bool {
    a.name == b.name && a.email == b.email
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

#[cfg(test)]
mod tests {
    use objects::object::{
        Attribution, CollaborationAnchor, CollaborationIdempotencyKey,
        CollaborationOperationBodyV1, CollaborationOperationEnvelope, ContentHash,
        DiscussionRecordId, DiscussionTurnV1, LegacyDiscussionId, LegacyDiscussionResolutionV1,
        LegacySourceLocator, Principal, StateAttachmentId, StateId, VisibilityTier,
    };

    use super::*;

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
}
