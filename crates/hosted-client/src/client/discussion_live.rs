// SPDX-License-Identifier: Apache-2.0
//! Live discussion delivery over [`RepoEventClient`].
//!
//! Pack attachments are a snapshot, not a stream. Live turns belong on the
//! #1585 event-cursor channel. weft already emits `discussion.opened` /
//! `turn.appended` / `discussion.resolved`; this module is the consumer:
//!
//! 1. Bootstrap a fresh clone via [`super::discussion_sync::pull_discussions`]
//!    (pull-bootstrap discussions when present, otherwise `ListByState` — the
//!    same non-fatal path #1642 falls back to).
//! 2. Subscribe from the persisted client watermark (`after_event_id`).
//! 3. Apply each event into the local collab op-log, preserving server turn
//!    identity (`turn_id` / `turn_seq`) so the linear blob cannot erase it.
//! 4. Persist the watermark after each event so a reconnect resumes.
//!
//! Fail-closed on visibility: the server already filters emission by audience.
//! This consumer uses the caller's credentials, treats a refused subscription
//! as fatal, and does not invent a discussion from an event that lacks a
//! server id. `discussions_from_pack` is not read or written here.

#![cfg(feature = "client")]

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use objects::{
    fs_atomic::write_file_atomic,
    object::{Discussion, StateId},
};
use repo::Repository;
use serde::{Deserialize, Serialize};

use super::{
    discussion_sync::{apply_hosted_discussion, discussion_is_mirrored, pull_discussions},
    repo_events::{
        RepoEvent, RepoEventClient, RepoEventError, RepoEventSubscription, SubscribeRepoEventsRequest,
    },
};
use crate::{
    client::HostedClient,
    hosted_runtime::hosted::{HostedDiscussion, HostedDiscussionTurn, HostedResolution},
};

/// Event types this consumer subscribes to. Unknown types are ignored.
pub const DISCUSSION_EVENT_TYPES: &[&str] = &[
    "discussion.opened",
    "turn.appended",
    "discussion.resolved",
];

const CURSOR_FILE: &str = "event-cursor.json";

/// Per-repo durable resume cursor for discussion events.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscussionEventCursor {
    /// Last successfully consumed `RepoEvent.event_id`. Placed in
    /// `after_event_id` on the next subscribe.
    #[serde(default)]
    pub after_event_id: i64,
    /// Hosted repo id last seen on the wire. Empty until the first event.
    #[serde(default)]
    pub repo_id: String,
    /// True after a ListByState / bootstrap snapshot has been applied.
    #[serde(default)]
    pub bootstrapped: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CursorFile {
    #[serde(default)]
    repos: std::collections::BTreeMap<String, DiscussionEventCursor>,
}

/// What happened when one event was consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscussionEventOutcome {
    /// Not a discussion event; watermark still advances.
    Ignored,
    /// Authorized event that could not be materialized (missing id, or
    /// GetDiscussion hid it). Watermark advances so we do not retry forever.
    Skipped { reason: String },
    /// Local op-log changed.
    Applied { discussion_id: String },
    /// Event was already represented locally.
    Unchanged { discussion_id: String },
}

impl DiscussionEventOutcome {
    pub fn discussion_id(&self) -> Option<&str> {
        match self {
            Self::Applied { discussion_id } | Self::Unchanged { discussion_id } => {
                Some(discussion_id.as_str())
            }
            Self::Ignored | Self::Skipped { .. } => None,
        }
    }

    pub fn applied(&self) -> bool {
        matches!(self, Self::Applied { .. })
    }
}

fn cursor_path(heddle_dir: &Path) -> PathBuf {
    heddle_dir.join("collaboration").join(CURSOR_FILE)
}

fn load_cursors(heddle_dir: &Path) -> Result<CursorFile> {
    match fs::read(cursor_path(heddle_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("decode discussion event cursor"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CursorFile::default()),
        Err(error) => Err(error).context("read discussion event cursor"),
    }
}

fn save_cursors(heddle_dir: &Path, cursors: &CursorFile) -> Result<()> {
    let path = cursor_path(heddle_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("create collaboration dir")?;
    }
    let bytes = serde_json::to_vec_pretty(cursors).context("encode discussion event cursor")?;
    write_file_atomic(&path, &bytes).context("write discussion event cursor")?;
    Ok(())
}

/// Subscription identity for the durable event cursor.
///
/// Unfiltered waits (no thread filter, no authority) keep the legacy
/// `repo_path` slot so existing files keep working. A filtered wait must
/// never share that slot: skipping bar-thread events and then advancing a
/// repo-wide watermark would leave an unfiltered `discuss wait` permanently
/// past them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscussionCursorScope {
    /// Hosted authority (`host:port`). Empty for tests that only have a path.
    pub authority: String,
    pub repo_path: String,
    pub thread: String,
    pub thread_id: String,
}

impl DiscussionCursorScope {
    pub fn unfiltered(repo_path: impl Into<String>) -> Self {
        Self {
            repo_path: repo_path.into(),
            ..Self::default()
        }
    }

    pub fn is_filtered(&self) -> bool {
        !self.thread.is_empty() || !self.thread_id.is_empty()
    }

    /// Stable map key. Filtered keys never equal the unfiltered `repo_path`.
    pub fn slot(&self) -> String {
        cursor_slot(
            &self.authority,
            &self.repo_path,
            &self.thread,
            &self.thread_id,
        )
    }
}

fn cursor_slot(authority: &str, repo_path: &str, thread: &str, thread_id: &str) -> String {
    let mut key = String::new();
    if !authority.is_empty() {
        key.push_str(authority);
        key.push('\n');
    }
    key.push_str(repo_path);
    if !thread.is_empty() || !thread_id.is_empty() {
        key.push_str("\nthread=");
        key.push_str(thread);
        key.push_str("\nthread_id=");
        key.push_str(thread_id);
    }
    key
}

/// Load the persisted watermark for the unfiltered `repo_path` slot.
pub fn load_cursor(heddle_dir: &Path, repo_path: &str) -> Result<DiscussionEventCursor> {
    load_scoped_cursor(heddle_dir, &DiscussionCursorScope::unfiltered(repo_path))
}

/// Persist the watermark for the unfiltered `repo_path` slot.
pub fn save_cursor(
    heddle_dir: &Path,
    repo_path: &str,
    cursor: &DiscussionEventCursor,
) -> Result<()> {
    save_scoped_cursor(
        heddle_dir,
        &DiscussionCursorScope::unfiltered(repo_path),
        cursor,
    )
}

/// Load the watermark for `scope`.
///
/// Filtered scopes never fall back to the unfiltered watermark. An
/// authority-scoped unfiltered wait also does not inherit the bare
/// `repo_path` slot — that file has no hosted authority, so applying it
/// would skip events on a different remote that shares owner/name.
pub fn load_scoped_cursor(
    heddle_dir: &Path,
    scope: &DiscussionCursorScope,
) -> Result<DiscussionEventCursor> {
    Ok(load_cursors(heddle_dir)?
        .repos
        .get(&scope.slot())
        .cloned()
        .unwrap_or_default())
}

/// Persist the watermark for `scope`. Always writes the scoped slot.
pub fn save_scoped_cursor(
    heddle_dir: &Path,
    scope: &DiscussionCursorScope,
    cursor: &DiscussionEventCursor,
) -> Result<()> {
    let mut cursors = load_cursors(heddle_dir)?;
    cursors.repos.insert(scope.slot(), cursor.clone());
    save_cursors(heddle_dir, &cursors)
}

/// True when this event is a discussion doorbell or payload.
pub fn is_discussion_event(event: &RepoEvent) -> bool {
    DISCUSSION_EVENT_TYPES.contains(&event.event_type.as_str())
        || event.kind
            == api::heddle::api::v1alpha1::RepoEventKind::DiscussionTurn as i32
}

/// Subscribe request for the discussion live tail.
/// Pair a thread name with its stable record id for a scoped subscribe.
///
/// heddle-api 0.23.0 `SubscribeRepoEventsRequest` requires both when the
/// subscription is thread-scoped. An empty pair is the unfiltered wait.
pub fn paired_thread_scope(thread: &str, thread_id: &str) -> Result<(String, String)> {
    if thread.is_empty() && thread_id.is_empty() {
        return Ok((String::new(), String::new()));
    }
    if thread.is_empty() || thread_id.is_empty() {
        return Err(anyhow!(
            "thread-scoped discuss wait requires both the thread name and its stable id"
        ));
    }
    Ok((thread.to_string(), thread_id.to_string()))
}

pub fn subscribe_request(
    repo_id: &str,
    after_event_id: i64,
    thread: &str,
    thread_id: &str,
) -> SubscribeRepoEventsRequest {
    SubscribeRepoEventsRequest {
        repo_id: repo_id.to_string(),
        thread: thread.to_string(),
        after_event_id,
        event_types: DISCUSSION_EVENT_TYPES
            .iter()
            .map(|event_type| (*event_type).to_string())
            .collect(),
        thread_id: thread_id.to_string(),
    }
}

/// Snapshot bootstrap for a fresh clone, then mark the cursor bootstrapped.
///
/// Does not start the live tail. `bootstrap` is the pull-fold discussions
/// when the clone/pull already decoded them; `None` falls back to ListByState.
pub async fn bootstrap_discussions(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    bootstrap: Option<&[Discussion]>,
) -> Result<DiscussionEventCursor> {
    bootstrap_discussions_scoped(
        repo,
        client,
        repo_path,
        &DiscussionCursorScope::unfiltered(repo_path),
        bootstrap,
    )
    .await
}

/// Snapshot bootstrap, writing the bootstrapped flag on `scope`'s cursor.
pub async fn bootstrap_discussions_scoped(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    scope: &DiscussionCursorScope,
    bootstrap: Option<&[Discussion]>,
) -> Result<DiscussionEventCursor> {
    if repo
        .head()
        .context("resolve repository head")?
        .is_none()
    {
        return Err(anyhow!(
            "cannot bootstrap hosted discussions without a repository HEAD"
        ));
    }
    pull_discussions(repo, client, repo_path, bootstrap)
        .await
        .context("bootstrap hosted discussions")?;
    let mut cursor = load_scoped_cursor(repo.heddle_dir(), scope)?;
    cursor.bootstrapped = true;
    // SubscribeRepoEvents keys on the hosted repo id (`RepoEvent.repo_id`, a
    // weft UUID). owner/name is a RepositoryRef path used by GetDiscussion /
    // ListByState. Do not persist the path into this slot.
    save_scoped_cursor(repo.heddle_dir(), scope, &cursor)?;
    Ok(cursor)
}

/// Apply one already-received repo event into the local op-log and advance
/// the unfiltered watermark. Fetches via `GetDiscussion` when the payload
/// is a doorbell.
pub async fn consume_discussion_event(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    event: &RepoEvent,
) -> Result<DiscussionEventOutcome> {
    consume_discussion_event_scoped(
        repo,
        client,
        repo_path,
        &DiscussionCursorScope::unfiltered(repo_path),
        event,
    )
    .await
}

/// Apply one event and advance only `scope`'s watermark.
pub async fn consume_discussion_event_scoped(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    scope: &DiscussionCursorScope,
    event: &RepoEvent,
) -> Result<DiscussionEventOutcome> {
    let mut cursor = load_scoped_cursor(repo.heddle_dir(), scope)?;
    let outcome = apply_discussion_event(repo, client, repo_path, event).await?;
    cursor.after_event_id = cursor.after_event_id.max(event.event_id);
    if !event.repo_id.is_empty() {
        cursor.repo_id = event.repo_id.clone();
    }
    save_scoped_cursor(repo.heddle_dir(), scope, &cursor)?;
    Ok(outcome)
}

async fn apply_discussion_event(
    repo: &Repository,
    client: &mut HostedClient,
    repo_path: &str,
    event: &RepoEvent,
) -> Result<DiscussionEventOutcome> {
    if !is_discussion_event(event) {
        return Ok(DiscussionEventOutcome::Ignored);
    }
    let payload = parse_event_payload(event);
    let Some(discussion_id) = payload.discussion_id.clone() else {
        return Ok(DiscussionEventOutcome::Skipped {
            reason: "discussion event is missing a server discussion id".to_string(),
        });
    };

    let already_mirrored =
        discussion_is_mirrored(repo.heddle_dir(), repo_path, &discussion_id)?;
    let hosted = match discussion_from_payload(
        event.event_type.as_str(),
        &payload,
        already_mirrored,
    ) {
        Some(discussion) => discussion,
        None => match client
            .get_discussion(repo_path, &discussion_id, payload.opened_against_state)
            .await
        {
            Ok(discussion) => discussion,
            Err(error) if is_hidden_discussion(&error) => {
                return Ok(DiscussionEventOutcome::Skipped {
                    reason: format!("discussion {discussion_id} is not visible to this caller"),
                });
            }
            Err(error) => {
                return Err(anyhow!(error).context(format!(
                    "fetch hosted discussion {discussion_id} after {}",
                    event.event_type
                )));
            }
        },
    };

    let changed = apply_hosted_discussion(
        repo,
        repo_path,
        client.authenticated_username().as_deref(),
        &hosted,
    )?;
    Ok(if changed {
        DiscussionEventOutcome::Applied { discussion_id }
    } else {
        DiscussionEventOutcome::Unchanged { discussion_id }
    })
}

fn is_hidden_discussion(error: &wire::ProtocolError) -> bool {
    // Skip only a real visibility denial or a missing object. Unauthenticated
    // / expired credentials are fatal: skipping would advance the watermark
    // and permanently miss the discussion after re-auth.
    match error {
        wire::ProtocolError::AuthorizationFailed(_) | wire::ProtocolError::ObjectNotFound(_) => {
            true
        }
        wire::ProtocolError::RemoteFailure { code, .. } => matches!(
            code,
            wire::RemoteFailureCode::PermissionDenied | wire::RemoteFailureCode::NotFound
        ),
        _ => false,
    }
}

#[derive(Debug, Default)]
struct EventPayload {
    discussion_id: Option<String>,
    turn_id: Option<String>,
    turn_seq: u64,
    body: Option<String>,
    author_name: Option<String>,
    author_email: Option<String>,
    posted_at_secs: i64,
    file: Option<String>,
    symbol: Option<String>,
    visibility: Option<String>,
    thread_ref: Option<String>,
    opened_against_state: Option<StateId>,
    resolution: HostedResolution,
}

fn parse_event_payload(event: &RepoEvent) -> EventPayload {
    // weft emit (scope.rs transactional sidecar): a flat object keyed with the
    // CollaborationService field names. Not a nested `{discussion, turn}`
    // envelope and not an alias map. Doorbells carry discussion_id + turn
    // identity; opened/resolved may also carry the DiscussionTurn / resolution
    // fields from state_review.proto.
    let value = if event.payload_json.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(&event.payload_json).unwrap_or(serde_json::Value::Null)
    };
    EventPayload {
        discussion_id: string_field(&value, "discussion_id"),
        turn_id: string_field(&value, "turn_id"),
        turn_seq: u64_field(&value, "turn_seq").unwrap_or(0),
        body: string_field(&value, "body"),
        author_name: string_field(&value, "author_name"),
        author_email: string_field(&value, "author_email"),
        posted_at_secs: i64_field(&value, "posted_at").unwrap_or(0),
        file: string_field(&value, "file"),
        symbol: string_field(&value, "symbol"),
        visibility: string_field(&value, "visibility"),
        thread_ref: string_field(&value, "thread_ref")
            .or_else(|| (!event.thread.is_empty()).then(|| event.thread.clone())),
        opened_against_state: value
            .get("opened_against_state")
            .and_then(parse_state_id)
            .or_else(|| event.new_state.as_ref().and_then(proto_state_id)),
        resolution: parse_resolution(&value),
    }
}

fn discussion_from_payload(
    event_type: &str,
    payload: &EventPayload,
    already_mirrored: bool,
) -> Option<HostedDiscussion> {
    let discussion_id = payload.discussion_id.clone()?;
    // Fat append/resolve on an unknown server id must not mint Open: pull_one's
    // None arm treats turns[0] as the first turn. Fetch (or skip) until the
    // mirror already has this discussion. Only discussion.opened may Open.
    if event_type != "discussion.opened" && !already_mirrored {
        return None;
    }
    match event_type {
        "discussion.opened" => {
            if payload.visibility.is_none() || !payload_has_anchor(payload) {
                return None;
            }
            let body = payload.body.as_deref()?;
            if body.trim().is_empty() {
                return None;
            }
            Some(hosted_from_payload(discussion_id, payload))
        }
        "discussion.resolved" => {
            if !payload_has_resolution(&payload.resolution) {
                return None;
            }
            Some(hosted_from_payload(discussion_id, payload))
        }
        "turn.appended" => {
            let body = payload.body.as_deref()?;
            if body.trim().is_empty() {
                return None;
            }
            // turn_seq 0 is "not minted". A one-turn HostedDiscussion then
            // lands at list-index/ordinal 0 and pull_one drops it as the
            // already-linked Open turn. Fetch unless identity is complete.
            if payload.turn_id.is_none() || payload.turn_seq == 0 {
                return None;
            }
            Some(hosted_from_payload(discussion_id, payload))
        }
        _ => None,
    }
}

fn payload_has_anchor(payload: &EventPayload) -> bool {
    payload
        .file
        .as_deref()
        .is_some_and(|file| !file.is_empty())
        && payload
            .symbol
            .as_deref()
            .is_some_and(|symbol| !symbol.is_empty())
}

fn payload_has_resolution(resolution: &HostedResolution) -> bool {
    match resolution {
        HostedResolution::Open => false,
        HostedResolution::ByEdit { state_id: None } => false,
        HostedResolution::ByEdit { state_id: Some(_) } => true,
        HostedResolution::Dismissed { .. } => true,
        HostedResolution::IntoAnnotation { annotation_id } => !annotation_id.is_empty(),
    }
}

fn hosted_from_payload(discussion_id: String, payload: &EventPayload) -> HostedDiscussion {
    let turns = payload
        .body
        .as_deref()
        .filter(|body| !body.trim().is_empty())
        .map(|body| {
            vec![HostedDiscussionTurn {
                author_name: payload.author_name.clone().unwrap_or_default(),
                author_email: payload.author_email.clone().unwrap_or_default(),
                body: body.to_string(),
                posted_at_secs: payload.posted_at_secs,
                turn_id: payload.turn_id.clone().unwrap_or_default(),
                turn_seq: payload.turn_seq,
            }]
        })
        .unwrap_or_default();
    HostedDiscussion {
        id: discussion_id,
        file: payload.file.clone().unwrap_or_default(),
        symbol: payload.symbol.clone().unwrap_or_default(),
        opened_against_state: payload.opened_against_state,
        visibility: payload.visibility.clone().unwrap_or_default(),
        thread_ref: payload.thread_ref.clone(),
        turns,
        resolution: payload.resolution.clone(),
    }
}

fn parse_resolution(value: &serde_json::Value) -> HostedResolution {
    let Some(resolution) = value.get("resolution") else {
        return HostedResolution::Open;
    };
    match string_field(resolution, "kind").unwrap_or_default().as_str() {
        "dismissed" => HostedResolution::Dismissed {
            reason: string_field(resolution, "reason").unwrap_or_default(),
        },
        "by_edit" => HostedResolution::ByEdit {
            state_id: resolution.get("state_id").and_then(parse_state_id),
        },
        "into_annotation" => HostedResolution::IntoAnnotation {
            annotation_id: string_field(resolution, "annotation_id").unwrap_or_default(),
        },
        _ => HostedResolution::Open,
    }
}

fn string_field(value: &serde_json::Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(|field| field.as_str())
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .map(ToString::to_string)
}

fn u64_field(value: &serde_json::Value, name: &str) -> Option<u64> {
    value.get(name).and_then(|field| {
        field
            .as_u64()
            .or_else(|| field.as_i64().and_then(|n| u64::try_from(n).ok()))
            .or_else(|| field.as_str()?.parse().ok())
    })
}

fn i64_field(value: &serde_json::Value, name: &str) -> Option<i64> {
    value.get(name).and_then(|field| {
        field
            .as_i64()
            .or_else(|| field.as_u64().and_then(|n| i64::try_from(n).ok()))
            .or_else(|| field.as_str()?.parse().ok())
    })
}

fn parse_state_id(value: &serde_json::Value) -> Option<StateId> {
    if let Some(hex) = value.as_str() {
        let bytes = hex::decode(hex).ok()?;
        return StateId::try_from_slice(&bytes).ok();
    }
    if let Some(bytes) = value.get("value").and_then(|field| field.as_array()) {
        let bytes: Option<Vec<u8>> = bytes
            .iter()
            .map(|n| n.as_u64().and_then(|n| u8::try_from(n).ok()))
            .collect();
        return StateId::try_from_slice(&bytes?).ok();
    }
    None
}

fn proto_state_id(state: &api::heddle::api::v1alpha1::StateId) -> Option<StateId> {
    StateId::try_from_slice(&state.value).ok()
}

/// One live-tail session: bootstrap if needed, subscribe, apply, persist.
pub struct DiscussionEventConsumer<'a> {
    repo: &'a Repository,
    client: &'a mut HostedClient,
    events: RepoEventClient,
    repo_path: String,
    authority: String,
    thread: String,
    thread_id: String,
}

impl<'a> DiscussionEventConsumer<'a> {
    pub fn new(
        repo: &'a Repository,
        client: &'a mut HostedClient,
        repo_path: impl Into<String>,
    ) -> Self {
        let events = RepoEventClient::from_hosted_client(client.clone());
        Self {
            repo,
            client,
            events,
            repo_path: repo_path.into(),
            authority: String::new(),
            thread: String::new(),
            thread_id: String::new(),
        }
    }

    pub fn with_authority(mut self, authority: impl Into<String>) -> Self {
        self.authority = authority.into();
        self
    }

    pub fn with_thread(mut self, thread: impl Into<String>, thread_id: impl Into<String>) -> Self {
        self.thread = thread.into();
        self.thread_id = thread_id.into();
        self
    }

    fn cursor_scope(&self) -> DiscussionCursorScope {
        DiscussionCursorScope {
            authority: self.authority.clone(),
            repo_path: self.repo_path.clone(),
            thread: self.thread.clone(),
            thread_id: self.thread_id.clone(),
        }
    }

    /// Snapshot bootstrap (if this clone has no cursor yet) then open the
    /// replay-then-live subscription.
    pub async fn start(
        &mut self,
        bootstrap: Option<&[Discussion]>,
    ) -> Result<DiscussionEventSubscription, DiscussionLiveError> {
        let scope = self.cursor_scope();
        let mut cursor = load_scoped_cursor(self.repo.heddle_dir(), &scope)
            .map_err(DiscussionLiveError::cursor)?;
        if !cursor.bootstrapped {
            cursor = bootstrap_discussions_scoped(
                self.repo,
                self.client,
                &self.repo_path,
                &scope,
                bootstrap,
            )
            .await
            .map_err(DiscussionLiveError::bootstrap)?;
        }
        self.subscribe_from_cursor(&cursor).await
    }

    /// Subscribe from the persisted watermark without repeating bootstrap.
    pub async fn resume(
        &mut self,
    ) -> Result<DiscussionEventSubscription, DiscussionLiveError> {
        let cursor = load_scoped_cursor(self.repo.heddle_dir(), &self.cursor_scope())
            .map_err(DiscussionLiveError::cursor)?;
        self.subscribe_from_cursor(&cursor).await
    }

    async fn subscribe_from_cursor(
        &mut self,
        cursor: &DiscussionEventCursor,
    ) -> Result<DiscussionEventSubscription, DiscussionLiveError> {
        // weft keys SubscribeRepoEvents on the hosted repo UUID. After the
        // first event we have that id; before it, owner/name is the path
        // weft resolves the same way RepositoryRef.canonical_path does.
        let repo_id = if cursor.repo_id.is_empty() {
            self.repo_path.as_str()
        } else {
            cursor.repo_id.as_str()
        };
        let subscription = self
            .events
            .subscribe(subscribe_request(
                repo_id,
                cursor.after_event_id,
                &self.thread,
                &self.thread_id,
            ))
            .await
            .map_err(DiscussionLiveError::Subscribe)?;
        Ok(DiscussionEventSubscription {
            inner: subscription,
        })
    }

    pub async fn consume_next(
        &mut self,
        subscription: &mut DiscussionEventSubscription,
    ) -> Result<(RepoEvent, DiscussionEventOutcome), DiscussionLiveError> {
        let event = match subscription.inner.next().await {
            Ok(event) => event,
            Err(error) if error.resume_after_event_id().is_some() => {
                *subscription = self.resume().await?;
                match subscription.inner.next().await {
                    Ok(event) => event,
                    Err(error) => return Err(DiscussionLiveError::Subscribe(error)),
                }
            }
            Err(error) => return Err(DiscussionLiveError::Subscribe(error)),
        };
        let outcome = consume_discussion_event_scoped(
            self.repo,
            self.client,
            &self.repo_path,
            &self.cursor_scope(),
            &event,
        )
        .await
        .map_err(DiscussionLiveError::apply)?;
        Ok((event, outcome))
    }
}

/// Wrapper so callers do not have to name [`RepoEventSubscription`].
pub struct DiscussionEventSubscription {
    inner: RepoEventSubscription,
}

impl DiscussionEventSubscription {
    pub fn last_event_id(&self) -> i64 {
        self.inner.last_event_id()
    }

    pub fn resume_request(&self) -> SubscribeRepoEventsRequest {
        self.inner.resume_request()
    }
}

/// Failure to bootstrap, subscribe, or apply a live discussion event.
#[derive(Debug, thiserror::Error)]
pub enum DiscussionLiveError {
    #[error("failed to persist the discussion event cursor: {0}")]
    Cursor(String),
    #[error("failed to bootstrap hosted discussions: {0}")]
    Bootstrap(String),
    #[error(transparent)]
    Subscribe(#[from] RepoEventError),
    #[error("failed to apply a discussion event: {0}")]
    Apply(String),
}

impl DiscussionLiveError {
    fn cursor(error: anyhow::Error) -> Self {
        Self::Cursor(error.to_string())
    }

    fn bootstrap(error: anyhow::Error) -> Self {
        Self::Bootstrap(error.to_string())
    }

    fn apply(error: anyhow::Error) -> Self {
        Self::Apply(error.to_string())
    }

    pub fn resume_after_event_id(&self) -> Option<i64> {
        match self {
            Self::Subscribe(error) => error.resume_after_event_id(),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "discussion_live_tests.rs"]
mod tests;
