// SPDX-License-Identifier: Apache-2.0
//! Agent registry: lightweight discovery index for parallel agent sessions.
//!
//! Stores one TOML file per active agent in `.heddle/agents/<session-id>.toml`.
//! The registry does NOT manage worktrees or refs — those remain independent.
//! It exists purely so an orchestrator can query which agents are in flight.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    fs_atomic::write_file_atomic,
    lock::RepoLock,
    store::{
        HeddleError, Result,
        liveness::{Liveness, is_owner_alive},
    },
};

const STALE_AGENT_TTL_DAYS: i64 = 7;

/// Outcome of an attempt to reserve an active session on a thread.
///
/// `LiveOwner` carries the existing reservation so callers can compare
/// anchors and surface either an "already reserved by another live
/// process" or an "anchor drift" error. The registry reaps dead owners
/// in-place before producing this outcome — so an `Active` entry seen
/// here is guaranteed to have been alive at the moment of the check.
#[derive(Debug)]
pub enum ReserveOutcome {
    /// Reservation succeeded; the new entry is included.
    Reserved(AgentEntry),
    /// Another live agent already holds this thread.
    LiveOwner(AgentEntry),
}

/// A record of one `heddle context get` call made during an agent session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextQueryEntry {
    /// The file path that was queried.
    pub path: String,
    /// The scope filter used, if any (e.g. `symbol:parse_manifest`).
    pub scope: Option<String>,
    /// When the query was made.
    pub queried_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentUsageSummary {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
    #[serde(default)]
    pub tool_calls: Option<u32>,
    #[serde(default)]
    pub cost_micros_usd: Option<u64>,
}

/// Status of an agent session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Agent is actively working.
    Active,
    /// Agent's reservation was left behind and has been reaped.
    Abandoned,
    /// Agent has finished work (snapshot taken) but not yet merged.
    Complete,
    /// Agent's thread has been merged into the base thread.
    Merged,
}

impl std::fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentStatus::Active => write!(f, "active"),
            AgentStatus::Abandoned => write!(f, "abandoned"),
            AgentStatus::Complete => write!(f, "complete"),
            AgentStatus::Merged => write!(f, "merged"),
        }
    }
}

/// A registry entry describing one active (or recently finished) agent session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    /// Unique session identifier (e.g. `agent-xxxxxxxxxxxx`).
    pub session_id: String,
    /// Stable harness-side instance identifier used to reconnect the same
    /// local client process to its registry entry across bridge restarts.
    #[serde(default)]
    pub client_instance_id: Option<String>,
    /// Harness-native actor identity such as `codex:thread:thr_123`.
    #[serde(default)]
    pub native_actor_key: Option<String>,
    /// Harness-native parent actor identity for child/subagent sessions.
    #[serde(default)]
    pub native_parent_actor_key: Option<String>,
    /// Harness-native reconnect key such as a transcript path or client name.
    #[serde(default)]
    pub native_instance_key: Option<String>,
    /// Heddle session identifier when this registry entry is attached to a
    /// first-class Heddle multi-segment session.
    #[serde(default)]
    pub heddle_session_id: Option<String>,
    /// Thread identifier when the session is attached to a Heddle thread record.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// The Heddle thread the agent writes to.
    pub thread: String,
    /// Process id that created or last renewed the reservation.
    #[serde(default)]
    pub pid: Option<u32>,
    /// Host boot identifier when available; differentiates reused PIDs.
    #[serde(default)]
    pub boot_id: Option<String>,
    /// Advisory liveness lock file path for future long-lived runners.
    #[serde(default)]
    pub liveness_path: Option<PathBuf>,
    /// Most recent reservation heartbeat.
    #[serde(default)]
    pub heartbeat_at: Option<DateTime<Utc>>,
    /// Full state id the session was anchored to.
    #[serde(default)]
    pub anchor_state: Option<String>,
    /// Root tree id the session was anchored to.
    #[serde(default)]
    pub anchor_root: Option<String>,
    /// Opaque token returned to programmatic agent clients.
    #[serde(default)]
    pub reservation_token: Option<String>,
    /// Absolute path to the agent's checkout directory, if filesystem-based.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Short display form of the base state the agent started from.
    pub base_state: String,
    /// When the agent session was created.
    pub started_at: DateTime<Utc>,
    /// AI provider (e.g. `anthropic`).
    pub provider: Option<String>,
    /// AI model (e.g. `claude-sonnet-4-6`).
    pub model: Option<String>,
    /// Harness or client name (e.g. `claude-code`, `codex`).
    #[serde(default)]
    pub harness: Option<String>,
    /// Harness-specific reasoning/thinking level when available.
    #[serde(default)]
    pub thinking_level: Option<String>,
    /// Aggregated usage counters captured for the active session.
    #[serde(default)]
    pub usage_summary: AgentUsageSummary,
    /// Most recent progress heartbeat timestamp.
    #[serde(default)]
    pub last_progress_at: Option<DateTime<Utc>>,
    /// Summary flush state for the local session reporter.
    #[serde(default)]
    pub report_flush_state: Option<String>,
    /// Most recent explanation of why Heddle attached this actor to its current
    /// thread/session context.
    #[serde(default)]
    pub attach_reason: Option<String>,
    /// Local agent task assignment id this session is executing, if any.
    #[serde(default)]
    pub task_assignment_id: Option<String>,
    /// Ordered explanation of attach rules Heddle evaluated.
    #[serde(default)]
    pub attach_precedence: Vec<String>,
    /// The attach rule that won for this actor.
    #[serde(default)]
    pub winning_attach_rule: Option<String>,
    /// Where Heddle learned the harness identity from.
    #[serde(default)]
    pub probe_source: Option<String>,
    /// How confident Heddle was in the probe result.
    #[serde(default)]
    pub probe_confidence: Option<f32>,
    /// Current status.
    pub status: AgentStatus,
    /// When the agent was marked complete or merged.
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    /// Log of `heddle context get` calls made during this session.
    /// Appended by the CLI each time an agent queries context from its worktree.
    #[serde(default)]
    pub context_queries: Vec<ContextQueryEntry>,
}

/// One hop in an actor ancestry chain, ordered root to leaf.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActorChainNode {
    pub session_id: String,
    #[serde(default)]
    pub native_actor_key: Option<String>,
    #[serde(default)]
    pub native_parent_actor_key: Option<String>,
    pub thread: String,
    pub status: AgentStatus,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
}

impl From<&AgentEntry> for ActorChainNode {
    fn from(entry: &AgentEntry) -> Self {
        Self {
            session_id: entry.session_id.clone(),
            native_actor_key: entry.native_actor_key.clone(),
            native_parent_actor_key: entry.native_parent_actor_key.clone(),
            thread: entry.thread.clone(),
            status: entry.status.clone(),
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            harness: entry.harness.clone(),
        }
    }
}

/// Manages agent registry entries stored in `.heddle/agents/`.
pub struct AgentRegistry {
    agents_dir: PathBuf,
}

impl AgentRegistry {
    /// Create a new registry backed by `<heddle_dir>/agents/`.
    pub fn new(heddle_dir: &Path) -> Self {
        Self {
            agents_dir: heddle_dir.join("agents"),
        }
    }

    fn entry_path(&self, session_id: &str) -> Result<PathBuf> {
        // Only allow characters produced by generate_agent_id: lowercase
        // alphanumeric and hyphens.  This makes path traversal structurally
        // impossible: none of [a-z0-9-] can form ".." or "/".
        if session_id.is_empty()
            || !session_id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(HeddleError::Config(format!(
                "invalid session ID '{}': only lowercase alphanumeric and hyphens allowed",
                session_id
            )));
        }
        Ok(self.agents_dir.join(format!("{}.toml", session_id)))
    }

    fn lock_path(&self) -> PathBuf {
        self.agents_dir.join(".lock")
    }

    fn write_lock(&self) -> Result<crate::lock::WriteLockGuard> {
        RepoLock::at(self.lock_path()).write().map_err(|err| {
            HeddleError::Config(format!("failed to acquire agent registry lock: {err}"))
        })
    }

    fn write_entry_file(&self, entry: &AgentEntry) -> Result<()> {
        crate::fs_atomic::create_dir_all_durable(&self.agents_dir)?;
        let path = self.entry_path(&entry.session_id)?;
        let content =
            toml::to_string_pretty(entry).map_err(|e| HeddleError::Config(e.to_string()))?;
        Ok(write_file_atomic(&path, content.as_bytes())?)
    }

    fn load_entry_from_path(&self, path: &Path) -> Result<Option<AgentEntry>> {
        if !path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(path)?;
        let entry = toml::from_str(&content).map_err(|e| HeddleError::Config(e.to_string()))?;
        Ok(Some(entry))
    }

    fn is_stale_terminal_entry(&self, entry: &AgentEntry) -> bool {
        if matches!(entry.status, AgentStatus::Active) {
            return false;
        }

        let terminal_at = entry.completed_at.unwrap_or(entry.started_at);
        terminal_at <= Utc::now() - chrono::Duration::days(STALE_AGENT_TTL_DAYS)
    }

    fn prune_stale_entry_path(&self, path: &Path) -> Result<()> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Liveness verdict for an active entry. Terminal entries (anything
    /// other than `Active`) are reported as `Dead` so reaping logic can
    /// skip them in one pass alongside crashed actives.
    pub fn liveness_for(entry: &AgentEntry) -> Liveness {
        if entry.status != AgentStatus::Active {
            return Liveness::Dead;
        }
        is_owner_alive(entry.pid, entry.boot_id.as_deref())
    }

    /// Mark an active entry as `Abandoned`, recording the moment the
    /// reservation was reaped.
    fn abandon_active_entry(&self, mut entry: AgentEntry) -> Result<AgentEntry> {
        entry.status = AgentStatus::Abandoned;
        entry.completed_at = Some(Utc::now());
        self.write_entry_file(&entry)?;
        Ok(entry)
    }

    /// Sweep entries on a single thread and reap any whose recorded
    /// owner is demonstrably dead. Returns the number of entries
    /// transitioned to `Abandoned`. Caller must already hold the
    /// registry write lock.
    fn reap_dead_locked(&self, thread_filter: Option<&str>) -> Result<usize> {
        if !self.agents_dir.exists() {
            return Ok(0);
        }
        let mut reaped = 0usize;
        for dir_entry in std::fs::read_dir(&self.agents_dir)? {
            let dir_entry = dir_entry?;
            let path = dir_entry.path();
            if !path.extension().map(|e| e == "toml").unwrap_or(false) {
                continue;
            }
            let Some(entry) = self.load_entry_from_path(&path)? else {
                continue;
            };
            if entry.status != AgentStatus::Active {
                continue;
            }
            if let Some(name) = thread_filter
                && entry.thread != name
            {
                continue;
            }
            if Self::liveness_for(&entry) == Liveness::Dead {
                self.abandon_active_entry(entry)?;
                reaped += 1;
            }
        }
        Ok(reaped)
    }

    /// Reap any active reservation on `thread` whose owning process is
    /// no longer alive. Returns the number of entries abandoned.
    ///
    /// This is the read-side complement of [`try_reserve_thread`]:
    /// callers that only need a fresh view of which sessions are alive
    /// (e.g. `heddle agent list --alive-only`) can run it without
    /// reserving.
    pub fn reap_dead_for_thread(&self, thread: &str) -> Result<usize> {
        let _lock = self.write_lock()?;
        self.reap_dead_locked(Some(thread))
    }

    /// Reap dead reservations across every thread.
    pub fn reap_dead(&self) -> Result<usize> {
        let _lock = self.write_lock()?;
        self.reap_dead_locked(None)
    }

    fn create_generated_entry_with<F, G>(
        &self,
        mut generate_id: G,
        mut build_entry: F,
    ) -> Result<AgentEntry>
    where
        F: FnMut(&str) -> Result<AgentEntry>,
        G: FnMut() -> String,
    {
        let _lock = self.write_lock()?;

        loop {
            let session_id = generate_id();
            let path = self.entry_path(&session_id)?;
            if path.exists() {
                continue;
            }

            let entry = build_entry(&session_id)?;
            self.write_entry_file(&entry)?;
            return Ok(entry);
        }
    }

    /// Create and persist a new agent entry with a unique generated session ID.
    pub fn create_generated_entry<F>(&self, build_entry: F) -> Result<AgentEntry>
    where
        F: FnMut(&str) -> Result<AgentEntry>,
    {
        self.create_generated_entry_with(generate_agent_id, build_entry)
    }

    /// Persist an agent entry.
    ///
    /// Atomic write: uses write-to-temp-then-rename so a crash mid-write
    /// never leaves the TOML file truncated or partially written.
    pub fn save(&self, entry: &AgentEntry) -> Result<()> {
        let _lock = self.write_lock()?;
        self.write_entry_file(entry)
    }

    /// Load a single agent entry by session ID.
    pub fn load(&self, session_id: &str) -> Result<Option<AgentEntry>> {
        let path = self.entry_path(session_id)?;
        let Some(entry) = self.load_entry_from_path(&path)? else {
            return Ok(None);
        };

        if self.is_stale_terminal_entry(&entry) {
            let _lock = self.write_lock()?;
            if let Some(latest) = self.load_entry_from_path(&path)?
                && self.is_stale_terminal_entry(&latest)
            {
                self.prune_stale_entry_path(&path)?;
                return Ok(None);
            }
        }

        Ok(Some(entry))
    }

    /// List all agent entries, most-recently-started first.
    pub fn list(&self) -> Result<Vec<AgentEntry>> {
        if !self.agents_dir.exists() {
            return Ok(Vec::new());
        }

        let mut stale_paths = Vec::new();
        let mut entries = Vec::new();
        for dir_entry in std::fs::read_dir(&self.agents_dir)? {
            let dir_entry = dir_entry?;
            let path = dir_entry.path();
            if path.extension().map(|e| e == "toml").unwrap_or(false) {
                let content = std::fs::read_to_string(&path)?;
                let entry = toml::from_str::<AgentEntry>(&content).map_err(|err| {
                    HeddleError::Config(format!(
                        "failed to parse agent registry entry '{}': {err}",
                        path.display()
                    ))
                })?;
                if self.is_stale_terminal_entry(&entry) {
                    stale_paths.push(path);
                } else {
                    entries.push(entry);
                }
            }
        }

        if !stale_paths.is_empty() {
            let _lock = self.write_lock()?;
            for path in stale_paths {
                if let Some(entry) = self.load_entry_from_path(&path)?
                    && self.is_stale_terminal_entry(&entry)
                {
                    self.prune_stale_entry_path(&path)?;
                }
            }
        }

        entries.sort_by_key(|a| std::cmp::Reverse(a.started_at));
        Ok(entries)
    }

    /// Update the status of an agent entry in place.
    pub fn update_status(&self, session_id: &str, status: AgentStatus) -> Result<()> {
        let _lock = self.write_lock()?;
        let path = self.entry_path(session_id)?;
        if let Some(mut entry) = self.load_entry_from_path(&path)? {
            entry.status = status;
            entry.completed_at = match entry.status {
                AgentStatus::Active => None,
                AgentStatus::Abandoned | AgentStatus::Complete | AgentStatus::Merged => {
                    Some(Utc::now())
                }
            };
            self.write_entry_file(&entry)?;
        }
        Ok(())
    }

    /// Mutate an existing agent entry under the registry write lock.
    pub fn update_entry<F>(&self, session_id: &str, mut update: F) -> Result<Option<AgentEntry>>
    where
        F: FnMut(&mut AgentEntry),
    {
        let _lock = self.write_lock()?;
        let path = self.entry_path(session_id)?;
        let Some(mut entry) = self.load_entry_from_path(&path)? else {
            return Ok(None);
        };
        update(&mut entry);
        self.write_entry_file(&entry)?;
        Ok(Some(entry))
    }

    /// Under one registry write lock, reuse a matching active entry if one
    /// exists; otherwise create a new generated entry.
    pub fn find_or_create_active_entry<FMatch, FUpdate, FBuild>(
        &self,
        mut matches: FMatch,
        mut update_existing: FUpdate,
        mut build_entry: FBuild,
    ) -> Result<(AgentEntry, bool)>
    where
        FMatch: FnMut(&AgentEntry) -> bool,
        FUpdate: FnMut(&mut AgentEntry),
        FBuild: FnMut(&str) -> Result<AgentEntry>,
    {
        let _lock = self.write_lock()?;
        crate::fs_atomic::create_dir_all_durable(&self.agents_dir)?;

        for dir_entry in std::fs::read_dir(&self.agents_dir)? {
            let dir_entry = dir_entry?;
            let path = dir_entry.path();
            if !path.extension().map(|e| e == "toml").unwrap_or(false) {
                continue;
            }
            let Some(mut entry) = self.load_entry_from_path(&path)? else {
                continue;
            };
            if self.is_stale_terminal_entry(&entry) {
                self.prune_stale_entry_path(&path)?;
                continue;
            }
            if entry.status == AgentStatus::Active && matches(&entry) {
                update_existing(&mut entry);
                self.write_entry_file(&entry)?;
                return Ok((entry, false));
            }
        }

        loop {
            let session_id = generate_agent_id();
            let path = self.entry_path(&session_id)?;
            if path.exists() {
                continue;
            }

            let entry = build_entry(&session_id)?;
            self.write_entry_file(&entry)?;
            return Ok((entry, true));
        }
    }

    /// Atomic "reap dead, then reserve" for a single thread.
    ///
    /// Under one registry write lock this:
    /// 1. Prunes terminal-status entries past their TTL.
    /// 2. Transitions any *active* entry whose recorded owner has died
    ///    (per [`liveness_for`](Self::liveness_for)) to `Abandoned`.
    /// 3. If a still-living active entry on `thread` remains, returns
    ///    [`ReserveOutcome::LiveOwner`] with that entry.
    /// 4. Otherwise builds and persists a new entry, returning
    ///    [`ReserveOutcome::Reserved`].
    ///
    /// Both `cmd_agent_reserve` (the JSON API) and `start_thread`
    /// route through here, so reaping and conflict detection share
    /// exactly one code path.
    pub fn try_reserve_thread<F>(&self, thread: &str, build_entry: F) -> Result<ReserveOutcome>
    where
        F: FnMut(&str) -> Result<AgentEntry>,
    {
        let _lock = self.write_lock()?;
        crate::fs_atomic::create_dir_all_durable(&self.agents_dir)?;

        let mut live_owner: Option<AgentEntry> = None;
        for dir_entry in std::fs::read_dir(&self.agents_dir)? {
            let dir_entry = dir_entry?;
            let path = dir_entry.path();
            if !path.extension().map(|e| e == "toml").unwrap_or(false) {
                continue;
            }
            let Some(entry) = self.load_entry_from_path(&path)? else {
                continue;
            };
            if self.is_stale_terminal_entry(&entry) {
                self.prune_stale_entry_path(&path)?;
                continue;
            }
            if entry.status != AgentStatus::Active || entry.thread != thread {
                continue;
            }
            match Self::liveness_for(&entry) {
                Liveness::Dead => {
                    self.abandon_active_entry(entry)?;
                }
                Liveness::Alive | Liveness::Unknown => {
                    // Unknown collapses to Alive here so we never
                    // double-allocate a thread on insufficient evidence.
                    live_owner = Some(entry);
                    break;
                }
            }
        }

        if let Some(existing) = live_owner {
            return Ok(ReserveOutcome::LiveOwner(existing));
        }

        let mut build_entry = build_entry;
        loop {
            let session_id = generate_agent_id();
            let path = self.entry_path(&session_id)?;
            if path.exists() {
                continue;
            }

            let entry = build_entry(&session_id)?;
            self.write_entry_file(&entry)?;
            return Ok(ReserveOutcome::Reserved(entry));
        }
    }

    /// Backwards-compatible thin wrapper around [`try_reserve_thread`]
    /// that returns the same flat `Result<AgentEntry>` shape callers
    /// historically expected. Live-owner conflicts surface as a
    /// `HeddleError::Config(..)` with the same human-readable message
    /// as before.
    pub fn create_generated_entry_for_thread<F>(
        &self,
        thread: &str,
        build_entry: F,
    ) -> Result<AgentEntry>
    where
        F: FnMut(&str) -> Result<AgentEntry>,
    {
        match self.try_reserve_thread(thread, build_entry)? {
            ReserveOutcome::Reserved(entry) => Ok(entry),
            ReserveOutcome::LiveOwner(existing) => Err(HeddleError::Config(format!(
                "thread '{}' already has active reservation {}. Use `heddle thread show {}` to inspect it, or release the session before starting another writer.",
                thread, existing.session_id, thread
            ))),
        }
    }

    /// Find the active session whose visible or private execution root matches
    /// the given worktree root.
    pub fn find_active_by_path(&self, worktree_root: &Path) -> Result<Option<AgentEntry>> {
        let canonical = worktree_root
            .canonicalize()
            .unwrap_or_else(|_| worktree_root.to_path_buf());
        let entries = self.list()?;
        Ok(entries
            .into_iter()
            .find(|e| e.status == AgentStatus::Active && entry_matches_root(e, &canonical)))
    }

    /// Find the active registry entry associated with the given Heddle session ID.
    pub fn find_active_by_heddle_session_id(
        &self,
        heddle_session_id: &str,
    ) -> Result<Option<AgentEntry>> {
        let entries = self.list()?;
        Ok(entries.into_iter().find(|entry| {
            entry.status == AgentStatus::Active
                && entry.heddle_session_id.as_deref() == Some(heddle_session_id)
        }))
    }

    /// Find the active registry entry associated with a stable harness-side
    /// client instance identifier.
    pub fn find_active_by_client_instance_id(
        &self,
        client_instance_id: &str,
    ) -> Result<Option<AgentEntry>> {
        let entries = self.list()?;
        Ok(entries.into_iter().find(|entry| {
            entry.status == AgentStatus::Active
                && entry.client_instance_id.as_deref() == Some(client_instance_id)
        }))
    }

    /// Find the active registry entry associated with a harness-native actor key.
    pub fn find_active_by_native_actor_key(
        &self,
        native_actor_key: &str,
    ) -> Result<Option<AgentEntry>> {
        let entries = self.list()?;
        Ok(entries.into_iter().find(|entry| {
            entry.status == AgentStatus::Active
                && entry.native_actor_key.as_deref() == Some(native_actor_key)
        }))
    }

    /// Return this actor's native parent chain, ordered root to leaf.
    ///
    /// The lookup intentionally follows harness-native actor keys rather than
    /// thread names: subagents may work in lightweight directories or forked
    /// threads, but the native parent key is the stable "who spawned whom"
    /// edge that preserves Human -> agent -> agent attribution.
    pub fn actor_chain_for_session(&self, session_id: &str) -> Result<Vec<ActorChainNode>> {
        let entries = self.list()?;
        let by_session: HashMap<&str, &AgentEntry> = entries
            .iter()
            .map(|entry| (entry.session_id.as_str(), entry))
            .collect();
        let by_native_key: HashMap<&str, &AgentEntry> = entries
            .iter()
            .filter_map(|entry| entry.native_actor_key.as_deref().map(|key| (key, entry)))
            .collect();

        let Some(mut current) = by_session.get(session_id).copied() else {
            return Ok(Vec::new());
        };
        let mut leaf_to_root = vec![ActorChainNode::from(current)];
        let mut seen = HashSet::from([current.session_id.as_str()]);

        while let Some(parent_key) = current.native_parent_actor_key.as_deref() {
            let Some(parent) = by_native_key.get(parent_key).copied() else {
                break;
            };
            if !seen.insert(parent.session_id.as_str()) {
                break;
            }
            leaf_to_root.push(ActorChainNode::from(parent));
            current = parent;
        }

        leaf_to_root.reverse();
        Ok(leaf_to_root)
    }

    /// Find the active registry entry associated with a harness-native instance
    /// key inside the given worktree root.
    pub fn find_active_by_native_instance_key_at_path(
        &self,
        native_instance_key: &str,
        worktree_root: &Path,
    ) -> Result<Option<AgentEntry>> {
        let canonical = worktree_root
            .canonicalize()
            .unwrap_or_else(|_| worktree_root.to_path_buf());
        let entries = self.list()?;
        Ok(entries.into_iter().find(|entry| {
            entry.status == AgentStatus::Active
                && entry.native_instance_key.as_deref() == Some(native_instance_key)
                && entry_matches_root(entry, &canonical)
        }))
    }

    /// Append a context query to an active session's log.
    ///
    /// Best-effort: silently ignored if the session no longer exists or has completed.
    pub fn log_context_query(&self, session_id: &str, query: ContextQueryEntry) -> Result<()> {
        let _lock = self.write_lock()?;
        let path = self.entry_path(session_id)?;
        if let Some(mut entry) = self.load_entry_from_path(&path)?
            && entry.status == AgentStatus::Active
        {
            entry.context_queries.push(query);
            self.write_entry_file(&entry)?;
        }
        Ok(())
    }

    /// Delete an agent entry.
    pub fn delete(&self, session_id: &str) -> Result<()> {
        let path = self.entry_path(session_id)?;
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}

/// Generate a unique agent session identifier.
///
/// Uses 12 random bytes (96 bits) encoded as lowercase base32, giving
/// a birthday-paradox collision probability of < 10⁻²⁰ at a million sessions.
pub fn generate_agent_id() -> String {
    let random_bytes: [u8; 12] = rand::random();
    format!(
        "agent-{}",
        base32::encode(base32::Alphabet::Rfc4648 { padding: false }, &random_bytes).to_lowercase()
    )
}

fn entry_matches_root(entry: &AgentEntry, canonical: &Path) -> bool {
    entry
        .path
        .as_ref()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()) == canonical)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn create_registry() -> (TempDir, AgentRegistry) {
        let temp_dir = TempDir::new().unwrap();
        let registry = AgentRegistry::new(&temp_dir.path().join(".heddle"));
        (temp_dir, registry)
    }

    fn entry(session_id: &str, status: AgentStatus) -> AgentEntry {
        AgentEntry {
            session_id: session_id.to_string(),
            client_instance_id: None,
            native_actor_key: None,
            native_parent_actor_key: None,
            native_instance_key: None,
            heddle_session_id: None,
            thread_id: None,
            thread: format!("agent/{session_id}"),
            pid: None,
            boot_id: None,
            liveness_path: None,
            heartbeat_at: None,
            anchor_state: None,
            anchor_root: None,
            reservation_token: None,
            path: None,
            base_state: "hs-base".to_string(),
            started_at: Utc::now(),
            provider: None,
            model: None,
            harness: None,
            thinking_level: None,
            usage_summary: AgentUsageSummary::default(),
            last_progress_at: None,
            report_flush_state: None,
            attach_reason: None,
            task_assignment_id: None,
            attach_precedence: vec![],
            winning_attach_rule: None,
            probe_source: None,
            probe_confidence: None,
            status,
            completed_at: None,
            context_queries: vec![],
        }
    }

    #[test]
    fn list_prunes_stale_completed_entries() {
        let (_temp, registry) = create_registry();
        let mut stale = entry("agent-stale", AgentStatus::Complete);
        stale.completed_at = Some(Utc::now() - chrono::Duration::days(8));
        let active = entry("agent-active", AgentStatus::Active);

        registry.save(&stale).unwrap();
        registry.save(&active).unwrap();

        let entries = registry.list().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "agent-active");
        assert!(registry.load("agent-stale").unwrap().is_none());
    }

    #[test]
    fn load_returns_none_for_stale_completed_entry() {
        let (_temp, registry) = create_registry();
        let mut stale = entry("agent-stale", AgentStatus::Merged);
        stale.completed_at = Some(Utc::now() - chrono::Duration::days(8));
        registry.save(&stale).unwrap();

        assert!(registry.load("agent-stale").unwrap().is_none());
    }

    #[test]
    fn log_context_query_appends_to_active_session() {
        let (_temp, registry) = create_registry();
        let active = entry("agent-active", AgentStatus::Active);
        registry.save(&active).unwrap();

        let query = ContextQueryEntry {
            path: "src/auth/session.rs".to_string(),
            scope: Some("symbol:validate_token".to_string()),
            queried_at: Utc::now(),
        };
        registry.log_context_query("agent-active", query).unwrap();

        let loaded = registry.load("agent-active").unwrap().unwrap();
        assert_eq!(loaded.context_queries.len(), 1);
        assert_eq!(loaded.context_queries[0].path, "src/auth/session.rs");
        assert_eq!(
            loaded.context_queries[0].scope.as_deref(),
            Some("symbol:validate_token")
        );
    }

    #[test]
    fn log_context_query_no_op_for_complete_session() {
        let (_temp, registry) = create_registry();
        let mut complete = entry("agent-done", AgentStatus::Complete);
        complete.completed_at = Some(Utc::now());
        registry.save(&complete).unwrap();

        let query = ContextQueryEntry {
            path: "src/lib.rs".to_string(),
            scope: None,
            queried_at: Utc::now(),
        };
        registry.log_context_query("agent-done", query).unwrap();

        let loaded = registry.load("agent-done").unwrap().unwrap();
        assert_eq!(loaded.context_queries.len(), 0);
    }

    #[test]
    fn find_active_by_path_returns_matching_session() {
        let (temp, registry) = create_registry();
        let worktree = temp.path().join("checkout");
        std::fs::create_dir_all(&worktree).unwrap();

        let mut active = entry("agent-match", AgentStatus::Active);
        active.path = Some(worktree.clone());
        registry.save(&active).unwrap();

        let mut other = entry("agent-other", AgentStatus::Active);
        other.path = Some(temp.path().join("other-checkout"));
        registry.save(&other).unwrap();

        let found = registry.find_active_by_path(&worktree).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().session_id, "agent-match");
    }

    #[test]
    fn create_generated_entry_retries_collisions_under_lock() {
        let (_temp, registry) = create_registry();
        registry
            .save(&entry("agent-existing", AgentStatus::Active))
            .unwrap();

        let mut ids = vec!["agent-existing".to_string(), "agent-new".to_string()].into_iter();
        let created = registry
            .create_generated_entry_with(
                move || ids.next().unwrap(),
                |session_id| {
                    let mut entry = entry(session_id, AgentStatus::Active);
                    entry.thread = format!("agent/{session_id}");
                    Ok(entry)
                },
            )
            .unwrap();

        assert_eq!(created.session_id, "agent-new");
        assert!(registry.load("agent-existing").unwrap().is_some());
        assert!(registry.load("agent-new").unwrap().is_some());
    }

    /// Regression: `cmd_agent_reserve` reserves an Active entry via
    /// `try_reserve_thread` *before* it advances the thread ref via
    /// `set_thread_cas`. If the CAS races (another writer advanced the
    /// thread between the pre-check and the CAS) and the caller doesn't
    /// abandon the orphaned reservation, subsequent reservers see a
    /// ghost live owner that can't be cleared until pid-liveness reaping
    /// kicks in.
    ///
    /// The fix wraps the post-reserve work in a fallible block and
    /// transitions the entry to `Abandoned` via `update_entry` if the
    /// block returns Err. This test exercises that primitive: an
    /// Abandoned entry must not block a fresh reservation, and the
    /// next reserve must succeed cleanly.
    #[test]
    fn abandoning_active_entry_unblocks_subsequent_reserve_on_same_thread() {
        let (_temp, registry) = create_registry();

        // First reserve: Active entry is created. Stand in for what
        // `cmd_agent_reserve`'s `try_reserve_thread` call writes.
        let outcome = registry
            .try_reserve_thread("feature/leak-repro", |session_id| {
                let mut e = entry(session_id, AgentStatus::Active);
                e.thread = "feature/leak-repro".to_string();
                e.thread_id = Some("feature/leak-repro".to_string());
                // pid 1 is always alive — keeps liveness honest if the
                // test environment exposes a boot_id; we'll abandon
                // explicitly below regardless.
                e.pid = Some(1);
                e.boot_id = crate::store::liveness::current_boot_id();
                Ok(e)
            })
            .unwrap();
        let session_id = match outcome {
            ReserveOutcome::Reserved(entry) => entry.session_id,
            ReserveOutcome::LiveOwner(_) => panic!("first reserve must succeed"),
        };

        // Simulate the cleanup path: post-reserve work failed (e.g.,
        // `set_thread_cas` lost a race), so we transition the entry to
        // Abandoned and bubble the original error to the caller. The
        // caller never saw this session_id in JSON, so no orchestrator
        // is holding it.
        registry
            .update_entry(&session_id, |entry| {
                entry.status = AgentStatus::Abandoned;
                entry.completed_at = Some(Utc::now());
            })
            .unwrap();

        // Second reserve on the same thread must succeed: `try_reserve_thread`
        // skips Abandoned entries when deciding live-owner vs reuse, so
        // no ghost row blocks us.
        let next = registry
            .try_reserve_thread("feature/leak-repro", |session_id| {
                let mut e = entry(session_id, AgentStatus::Active);
                e.thread = "feature/leak-repro".to_string();
                e.thread_id = Some("feature/leak-repro".to_string());
                e.pid = Some(1);
                e.boot_id = crate::store::liveness::current_boot_id();
                Ok(e)
            })
            .unwrap();
        assert!(
            matches!(next, ReserveOutcome::Reserved(_)),
            "after the orphaned reservation is abandoned, the next reserve must succeed: {next:?}"
        );

        // The registry now holds exactly one Active entry on this
        // thread — the new one — plus the Abandoned husk. Counting
        // Active by thread should be 1, not 2.
        let active_count = registry
            .list()
            .unwrap()
            .into_iter()
            .filter(|e| e.thread == "feature/leak-repro" && e.status == AgentStatus::Active)
            .count();
        assert_eq!(
            active_count, 1,
            "exactly one Active reservation must own the thread after rollback + retry"
        );
    }

    #[test]
    fn update_entry_persists_harness_metadata() {
        let (_temp, registry) = create_registry();
        let active = entry("agent-active", AgentStatus::Active);
        registry.save(&active).unwrap();

        registry
            .update_entry("agent-active", |entry| {
                entry.heddle_session_id = Some("sess-123".to_string());
                entry.harness = Some("claude-code".to_string());
                entry.thinking_level = Some("deep".to_string());
                entry.report_flush_state = Some("pending-local".to_string());
                entry.attach_reason = Some("attached from test metadata update".to_string());
                entry.attach_precedence = vec!["matched-current-session".to_string()];
                entry.winning_attach_rule = Some("matched-current-session".to_string());
                entry.probe_source = Some("argv_env".to_string());
                entry.probe_confidence = Some(0.75);
                entry.last_progress_at = Some(Utc::now());
                entry.usage_summary.input_tokens = Some(42);
            })
            .unwrap();

        let loaded = registry.load("agent-active").unwrap().unwrap();
        assert_eq!(loaded.heddle_session_id.as_deref(), Some("sess-123"));
        assert_eq!(loaded.harness.as_deref(), Some("claude-code"));
        assert_eq!(loaded.thinking_level.as_deref(), Some("deep"));
        assert_eq!(loaded.report_flush_state.as_deref(), Some("pending-local"));
        assert_eq!(
            loaded.attach_reason.as_deref(),
            Some("attached from test metadata update")
        );
        assert_eq!(loaded.attach_precedence, vec!["matched-current-session"]);
        assert_eq!(
            loaded.winning_attach_rule.as_deref(),
            Some("matched-current-session")
        );
        assert_eq!(loaded.probe_source.as_deref(), Some("argv_env"));
        assert_eq!(loaded.probe_confidence, Some(0.75));
        assert_eq!(loaded.usage_summary.input_tokens, Some(42));
        assert!(loaded.last_progress_at.is_some());
    }

    #[test]
    fn find_active_by_client_instance_id_returns_matching_session() {
        let (_temp, registry) = create_registry();
        let mut active = entry("agent-client", AgentStatus::Active);
        active.client_instance_id = Some("client-a".to_string());
        registry.save(&active).unwrap();

        let mut other = entry("agent-other", AgentStatus::Active);
        other.client_instance_id = Some("client-b".to_string());
        registry.save(&other).unwrap();

        let found = registry
            .find_active_by_client_instance_id("client-a")
            .unwrap()
            .unwrap();
        assert_eq!(found.session_id, "agent-client");
    }

    #[test]
    fn find_active_by_native_actor_key_returns_matching_session() {
        let (_temp, registry) = create_registry();
        let mut active = entry("agent-native", AgentStatus::Active);
        active.native_actor_key = Some("codex:thread:thr_123".to_string());
        registry.save(&active).unwrap();

        let found = registry
            .find_active_by_native_actor_key("codex:thread:thr_123")
            .unwrap()
            .unwrap();
        assert_eq!(found.session_id, "agent-native");
    }

    #[test]
    fn actor_chain_follows_native_parent_keys_root_to_leaf() {
        let (_temp, registry) = create_registry();
        let mut root = entry("agent-root", AgentStatus::Active);
        root.native_actor_key = Some("human:foo".to_string());
        root.provider = Some("human".to_string());

        let mut parent = entry("agent-parent", AgentStatus::Active);
        parent.native_actor_key = Some("codex:thread:parent".to_string());
        parent.native_parent_actor_key = Some("human:foo".to_string());
        parent.provider = Some("openai".to_string());
        parent.model = Some("gpt-5".to_string());

        let mut child = entry("agent-child", AgentStatus::Active);
        child.native_actor_key = Some("codex:thread:child".to_string());
        child.native_parent_actor_key = Some("codex:thread:parent".to_string());
        child.provider = Some("openai".to_string());
        child.model = Some("gpt-5-mini".to_string());

        registry.save(&child).unwrap();
        registry.save(&root).unwrap();
        registry.save(&parent).unwrap();

        let chain = registry.actor_chain_for_session("agent-child").unwrap();
        let ids: Vec<_> = chain.iter().map(|node| node.session_id.as_str()).collect();
        assert_eq!(ids, vec!["agent-root", "agent-parent", "agent-child"]);
        assert_eq!(
            chain[2].native_parent_actor_key.as_deref(),
            Some("codex:thread:parent")
        );
    }

    #[test]
    fn try_reserve_thread_reaps_dead_active_entry_and_succeeds() {
        let (_temp, registry) = create_registry();
        let mut dead = entry("agent-dead", AgentStatus::Active);
        dead.thread = "feature/race".to_string();
        // PID 0x7fff_ffff is unassignable on Linux/macOS. Combined with
        // a stale boot id this is unambiguously dead.
        dead.pid = Some(0x7fff_ffff);
        dead.boot_id = Some("not-the-current-boot".to_string());
        registry.save(&dead).unwrap();

        let outcome = registry
            .try_reserve_thread("feature/race", |session_id| {
                let mut new = entry(session_id, AgentStatus::Active);
                new.thread = "feature/race".to_string();
                new.pid = Some(std::process::id());
                new.boot_id = crate::store::liveness::current_boot_id();
                Ok(new)
            })
            .unwrap();

        match outcome {
            ReserveOutcome::Reserved(entry) => assert_ne!(entry.session_id, "agent-dead"),
            ReserveOutcome::LiveOwner(_) => panic!("dead owner should have been reaped"),
        }
        let abandoned = registry.load("agent-dead").unwrap().unwrap();
        assert_eq!(abandoned.status, AgentStatus::Abandoned);
        assert!(abandoned.completed_at.is_some());
    }

    #[test]
    fn try_reserve_thread_reports_live_owner_when_pid_is_alive() {
        let (_temp, registry) = create_registry();
        let mut alive = entry("agent-alive", AgentStatus::Active);
        alive.thread = "feature/busy".to_string();
        alive.pid = Some(std::process::id());
        alive.boot_id = crate::store::liveness::current_boot_id();
        registry.save(&alive).unwrap();

        let outcome = registry
            .try_reserve_thread("feature/busy", |session_id| {
                let mut new = entry(session_id, AgentStatus::Active);
                new.thread = "feature/busy".to_string();
                Ok(new)
            })
            .unwrap();

        match outcome {
            ReserveOutcome::Reserved(_) => panic!("live owner should have blocked reservation"),
            ReserveOutcome::LiveOwner(existing) => assert_eq!(existing.session_id, "agent-alive"),
        }
        let still_alive = registry.load("agent-alive").unwrap().unwrap();
        assert_eq!(still_alive.status, AgentStatus::Active);
    }

    #[test]
    fn reap_dead_for_thread_only_touches_named_thread() {
        let (_temp, registry) = create_registry();
        let mut dead_a = entry("agent-dead-a", AgentStatus::Active);
        dead_a.thread = "feature/a".to_string();
        dead_a.pid = Some(0x7fff_ffff);
        dead_a.boot_id = Some("stale".to_string());
        let mut dead_b = entry("agent-dead-b", AgentStatus::Active);
        dead_b.thread = "feature/b".to_string();
        dead_b.pid = Some(0x7fff_ffff);
        dead_b.boot_id = Some("stale".to_string());
        registry.save(&dead_a).unwrap();
        registry.save(&dead_b).unwrap();

        let reaped = registry.reap_dead_for_thread("feature/a").unwrap();
        assert_eq!(reaped, 1);
        assert_eq!(
            registry.load("agent-dead-a").unwrap().unwrap().status,
            AgentStatus::Abandoned
        );
        assert_eq!(
            registry.load("agent-dead-b").unwrap().unwrap().status,
            AgentStatus::Active,
            "untargeted thread should not be reaped"
        );

        let reaped_all = registry.reap_dead().unwrap();
        assert_eq!(reaped_all, 1);
        assert_eq!(
            registry.load("agent-dead-b").unwrap().unwrap().status,
            AgentStatus::Abandoned
        );
    }

    #[test]
    fn actor_chain_breaks_cycles_without_looping() {
        let (_temp, registry) = create_registry();
        let mut a = entry("agent-a", AgentStatus::Active);
        a.native_actor_key = Some("actor:a".to_string());
        a.native_parent_actor_key = Some("actor:b".to_string());
        let mut b = entry("agent-b", AgentStatus::Active);
        b.native_actor_key = Some("actor:b".to_string());
        b.native_parent_actor_key = Some("actor:a".to_string());
        registry.save(&a).unwrap();
        registry.save(&b).unwrap();

        let chain = registry.actor_chain_for_session("agent-a").unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain.last().unwrap().session_id, "agent-a");
    }

    /// Concurrent-reservation soak. The W5b reservation contract:
    /// when N threads race to reserve the same thread name from the
    /// same anchor, exactly one wins (`ReserveOutcome::Reserved`) and
    /// the rest get `ReserveOutcome::LiveOwner` carrying the winner's
    /// session id. Tested 100× with 8 racers per round to catch any
    /// rare lock-acquisition flake.
    ///
    /// `#[ignore]` because this iterates 800 reservations and takes
    /// a few seconds; runs in `--include-ignored` sweeps and the
    /// nightly real-world matrix.
    #[test]
    #[ignore = "soak: 100× concurrent reservation race"]
    fn try_reserve_thread_under_concurrent_load_is_race_free() {
        use std::sync::{Arc, Barrier};

        const ROUNDS: usize = 100;
        const RACERS: usize = 8;

        for round in 0..ROUNDS {
            // Fresh registry each round so the previous round's
            // winner doesn't bias the next round (also models the
            // real "release between batches" pattern).
            let (_temp, registry) = create_registry();
            let registry = Arc::new(registry);
            let barrier = Arc::new(Barrier::new(RACERS));
            let thread_name = format!("feature/race-{round}");

            let handles: Vec<_> = (0..RACERS)
                .map(|racer_idx| {
                    let registry = Arc::clone(&registry);
                    let barrier = Arc::clone(&barrier);
                    let thread_name = thread_name.clone();
                    std::thread::spawn(move || {
                        // All racers wait on the barrier so the
                        // contention is maximised — without this the
                        // OS scheduler can serialize them and the
                        // test passes trivially.
                        barrier.wait();
                        let outcome = registry.try_reserve_thread(&thread_name, |session_id| {
                            let mut entry = entry(
                                &format!("agent-{racer_idx}-{session_id}"),
                                AgentStatus::Active,
                            );
                            entry.thread = thread_name.clone();
                            entry.pid = Some(std::process::id());
                            entry.boot_id = crate::store::liveness::current_boot_id();
                            Ok(entry)
                        });
                        outcome.expect("reservation call must not error")
                    })
                })
                .collect();

            let outcomes: Vec<ReserveOutcome> = handles
                .into_iter()
                .map(|h| h.join().expect("racer panic"))
                .collect();

            let reserved_count = outcomes
                .iter()
                .filter(|o| matches!(o, ReserveOutcome::Reserved(_)))
                .count();
            let live_owner_count = outcomes
                .iter()
                .filter(|o| matches!(o, ReserveOutcome::LiveOwner(_)))
                .count();

            assert_eq!(
                reserved_count, 1,
                "round {round}: exactly one racer must win the reservation; got {reserved_count}"
            );
            assert_eq!(
                live_owner_count,
                RACERS - 1,
                "round {round}: every loser must get a LiveOwner outcome; \
                 reserved={reserved_count} live_owner={live_owner_count}"
            );

            // Strong invariant: every LiveOwner outcome's `existing`
            // entry must be the winner's session — not some stale
            // dead entry, not a different racer's losing-and-then-
            // dead entry.
            let winner_session = outcomes
                .iter()
                .find_map(|o| match o {
                    ReserveOutcome::Reserved(entry) => Some(entry.session_id.clone()),
                    _ => None,
                })
                .expect("a winner must exist");
            for outcome in &outcomes {
                if let ReserveOutcome::LiveOwner(existing) = outcome {
                    assert_eq!(
                        existing.session_id, winner_session,
                        "round {round}: LiveOwner conflicts must point at the actual winner; \
                         got {} expected {winner_session}",
                        existing.session_id
                    );
                }
            }
        }
    }
}
