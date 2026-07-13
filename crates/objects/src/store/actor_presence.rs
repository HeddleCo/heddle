// SPDX-License-Identifier: Apache-2.0
//! Durable actor presence and work-context records.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    fs_atomic::write_file_atomic,
    lock::RepoLock,
    store::{HeddleError, Result},
};

const STALE_AGENT_TTL_DAYS: i64 = 7;

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
pub enum ActorPresenceStatus {
    /// Agent is actively working.
    Active,
    /// Agent work was abandoned or interrupted.
    Abandoned,
    /// Agent has finished work (snapshot taken) but not yet merged.
    Complete,
    /// Agent's thread has been merged into the base thread.
    Merged,
}

impl std::fmt::Display for ActorPresenceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActorPresenceStatus::Active => write!(f, "active"),
            ActorPresenceStatus::Abandoned => write!(f, "abandoned"),
            ActorPresenceStatus::Complete => write!(f, "complete"),
            ActorPresenceStatus::Merged => write!(f, "merged"),
        }
    }
}

/// A registry entry describing one active (or recently finished) agent session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorPresence {
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
    /// Full state id the session was anchored to.
    #[serde(default)]
    pub anchor_state: Option<String>,
    /// Root tree id the session was anchored to.
    #[serde(default)]
    pub anchor_root: Option<String>,
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
    pub status: ActorPresenceStatus,
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
    pub status: ActorPresenceStatus,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
}

impl From<&ActorPresence> for ActorChainNode {
    fn from(entry: &ActorPresence) -> Self {
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

/// Manages actor presence stored in `.heddle/actor-presence/`.
pub struct ActorPresenceStore {
    presence_dir: PathBuf,
}

impl ActorPresenceStore {
    /// Create a store backed by `<heddle_dir>/actor-presence/`.
    pub fn new(heddle_dir: &Path) -> Self {
        Self {
            presence_dir: heddle_dir.join("actor-presence"),
        }
    }

    fn entry_path(&self, session_id: &str) -> Result<PathBuf> {
        // Only allow characters produced by generate_actor_session_id: lowercase
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
        Ok(self.presence_dir.join(format!("{}.toml", session_id)))
    }

    fn lock_path(&self) -> PathBuf {
        self.presence_dir.join(".lock")
    }

    fn write_lock(&self) -> Result<crate::lock::WriteLockGuard> {
        RepoLock::at(self.lock_path()).write().map_err(|err| {
            HeddleError::Config(format!("failed to acquire agent registry lock: {err}"))
        })
    }

    fn write_entry_file(&self, entry: &ActorPresence) -> Result<()> {
        crate::fs_atomic::create_dir_all_durable(&self.presence_dir)?;
        let path = self.entry_path(&entry.session_id)?;
        let content =
            toml::to_string_pretty(entry).map_err(|e| HeddleError::Config(e.to_string()))?;
        Ok(write_file_atomic(&path, content.as_bytes())?)
    }

    fn load_entry_from_path(&self, path: &Path) -> Result<Option<ActorPresence>> {
        if !path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(path)?;
        let entry = toml::from_str(&content).map_err(|e| HeddleError::Config(e.to_string()))?;
        Ok(Some(entry))
    }

    fn is_stale_terminal_entry(&self, entry: &ActorPresence) -> bool {
        if matches!(entry.status, ActorPresenceStatus::Active) {
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

    pub fn current_entries(&self) -> Result<Vec<ActorPresence>> {
        self.list()
    }

    pub fn active_entries(&self) -> Result<Vec<ActorPresence>> {
        Ok(self
            .current_entries()?
            .into_iter()
            .filter(|entry| entry.status == ActorPresenceStatus::Active)
            .collect())
    }

    fn create_generated_entry_with<F, G>(
        &self,
        mut generate_id: G,
        mut build_entry: F,
    ) -> Result<ActorPresence>
    where
        F: FnMut(&str) -> Result<ActorPresence>,
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
    pub fn create_generated_entry<F>(&self, build_entry: F) -> Result<ActorPresence>
    where
        F: FnMut(&str) -> Result<ActorPresence>,
    {
        self.create_generated_entry_with(generate_actor_session_id, build_entry)
    }

    /// Persist an agent entry.
    ///
    /// Atomic write: uses write-to-temp-then-rename so a crash mid-write
    /// never leaves the TOML file truncated or partially written.
    pub fn save(&self, entry: &ActorPresence) -> Result<()> {
        let _lock = self.write_lock()?;
        self.write_entry_file(entry)
    }

    /// Load a single agent entry by session ID.
    pub fn load(&self, session_id: &str) -> Result<Option<ActorPresence>> {
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
    pub fn list(&self) -> Result<Vec<ActorPresence>> {
        if !self.presence_dir.exists() {
            return Ok(Vec::new());
        }

        let mut stale_paths = Vec::new();
        let mut entries = Vec::new();
        for dir_entry in std::fs::read_dir(&self.presence_dir)? {
            let dir_entry = dir_entry?;
            let path = dir_entry.path();
            if path.extension().map(|e| e == "toml").unwrap_or(false) {
                let content = std::fs::read_to_string(&path)?;
                let entry = toml::from_str::<ActorPresence>(&content).map_err(|err| {
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
    pub fn update_status(&self, session_id: &str, status: ActorPresenceStatus) -> Result<()> {
        let _lock = self.write_lock()?;
        let path = self.entry_path(session_id)?;
        if let Some(mut entry) = self.load_entry_from_path(&path)? {
            entry.status = status;
            entry.completed_at = match entry.status {
                ActorPresenceStatus::Active => None,
                ActorPresenceStatus::Abandoned
                | ActorPresenceStatus::Complete
                | ActorPresenceStatus::Merged => Some(Utc::now()),
            };
            self.write_entry_file(&entry)?;
        }
        Ok(())
    }

    /// Mutate an existing agent entry under the registry write lock.
    pub fn update_entry<F>(&self, session_id: &str, mut update: F) -> Result<Option<ActorPresence>>
    where
        F: FnMut(&mut ActorPresence),
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
    ) -> Result<(ActorPresence, bool)>
    where
        FMatch: FnMut(&ActorPresence) -> bool,
        FUpdate: FnMut(&mut ActorPresence),
        FBuild: FnMut(&str) -> Result<ActorPresence>,
    {
        let _lock = self.write_lock()?;
        crate::fs_atomic::create_dir_all_durable(&self.presence_dir)?;

        for dir_entry in std::fs::read_dir(&self.presence_dir)? {
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
            if entry.status == ActorPresenceStatus::Active && matches(&entry) {
                update_existing(&mut entry);
                self.write_entry_file(&entry)?;
                return Ok((entry, false));
            }
        }

        loop {
            let session_id = generate_actor_session_id();
            let path = self.entry_path(&session_id)?;
            if path.exists() {
                continue;
            }

            let entry = build_entry(&session_id)?;
            self.write_entry_file(&entry)?;
            return Ok((entry, true));
        }
    }

    /// Find the active session whose visible or private execution root matches
    /// the given worktree root.
    pub fn find_active_by_path(&self, worktree_root: &Path) -> Result<Option<ActorPresence>> {
        let canonical = worktree_root
            .canonicalize()
            .unwrap_or_else(|_| worktree_root.to_path_buf());
        let entries = self.active_entries()?;
        Ok(entries
            .into_iter()
            .find(|entry| entry_matches_root(entry, &canonical)))
    }

    /// Find the active registry entry associated with the given Heddle session ID.
    pub fn find_active_by_heddle_session_id(
        &self,
        heddle_session_id: &str,
    ) -> Result<Option<ActorPresence>> {
        let entries = self.active_entries()?;
        Ok(entries
            .into_iter()
            .find(|entry| entry.heddle_session_id.as_deref() == Some(heddle_session_id)))
    }

    /// Find the active registry entry associated with a stable harness-side
    /// client instance identifier.
    pub fn find_active_by_client_instance_id(
        &self,
        client_instance_id: &str,
    ) -> Result<Option<ActorPresence>> {
        let entries = self.active_entries()?;
        Ok(entries
            .into_iter()
            .find(|entry| entry.client_instance_id.as_deref() == Some(client_instance_id)))
    }

    /// Find the active registry entry associated with a harness-native actor key.
    pub fn find_active_by_native_actor_key(
        &self,
        native_actor_key: &str,
    ) -> Result<Option<ActorPresence>> {
        let entries = self.active_entries()?;
        Ok(entries
            .into_iter()
            .find(|entry| entry.native_actor_key.as_deref() == Some(native_actor_key)))
    }

    /// Return this actor's native parent chain, ordered root to leaf.
    ///
    /// The lookup intentionally follows harness-native actor keys rather than
    /// thread names: subagents may work in lightweight directories or forked
    /// threads, but the native parent key is the stable "who spawned whom"
    /// edge that preserves Human -> agent -> agent attribution.
    pub fn actor_chain_for_session(&self, session_id: &str) -> Result<Vec<ActorChainNode>> {
        let entries = self.current_entries()?;
        let by_session: HashMap<&str, &ActorPresence> = entries
            .iter()
            .map(|entry| (entry.session_id.as_str(), entry))
            .collect();
        let by_native_key: HashMap<&str, &ActorPresence> = entries
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
    ) -> Result<Option<ActorPresence>> {
        let canonical = worktree_root
            .canonicalize()
            .unwrap_or_else(|_| worktree_root.to_path_buf());
        let entries = self.active_entries()?;
        Ok(entries.into_iter().find(|entry| {
            entry.native_instance_key.as_deref() == Some(native_instance_key)
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
            && entry.status == ActorPresenceStatus::Active
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
pub fn generate_actor_session_id() -> String {
    let random_bytes: [u8; 12] = rand::random();
    format!(
        "agent-{}",
        base32::encode(base32::Alphabet::Rfc4648 { padding: false }, &random_bytes).to_lowercase()
    )
}

fn entry_matches_root(entry: &ActorPresence, canonical: &Path) -> bool {
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

    fn presence(session_id: &str, status: ActorPresenceStatus) -> ActorPresence {
        ActorPresence {
            session_id: session_id.to_string(),
            client_instance_id: None,
            native_actor_key: None,
            native_parent_actor_key: None,
            native_instance_key: None,
            heddle_session_id: None,
            thread_id: None,
            thread: "feature/test".to_string(),
            anchor_state: Some("hd-state".to_string()),
            anchor_root: Some("root".to_string()),
            path: None,
            base_state: "hd-state".to_string(),
            started_at: Utc::now(),
            provider: Some("openai".to_string()),
            model: Some("gpt-5".to_string()),
            harness: Some("codex".to_string()),
            thinking_level: None,
            usage_summary: AgentUsageSummary::default(),
            last_progress_at: None,
            report_flush_state: None,
            attach_reason: Some("test".to_string()),
            task_assignment_id: None,
            attach_precedence: vec!["explicit".to_string()],
            winning_attach_rule: Some("explicit".to_string()),
            probe_source: None,
            probe_confidence: None,
            status,
            completed_at: None,
            context_queries: vec![],
        }
    }

    #[test]
    fn active_presence_is_independent_of_writer_liveness() {
        let temp = TempDir::new().unwrap();
        let store = ActorPresenceStore::new(temp.path());
        store
            .save(&presence("agent-one", ActorPresenceStatus::Active))
            .unwrap();

        let active = store.active_entries().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].session_id, "agent-one");
    }

    #[test]
    fn active_presence_can_be_reused_by_native_identity() {
        let temp = TempDir::new().unwrap();
        let store = ActorPresenceStore::new(temp.path());
        let (first, created) = store
            .find_or_create_active_entry(
                |_| false,
                |_| {},
                |session_id| {
                    let mut entry = presence(session_id, ActorPresenceStatus::Active);
                    entry.native_actor_key = Some("codex:thread:one".to_string());
                    Ok(entry)
                },
            )
            .unwrap();
        assert!(created);

        let (second, created) = store
            .find_or_create_active_entry(
                |entry| entry.native_actor_key.as_deref() == Some("codex:thread:one"),
                |_| {},
                |_| panic!("matching presence should be reused"),
            )
            .unwrap();
        assert!(!created);
        assert_eq!(first.session_id, second.session_id);
    }

    #[test]
    fn terminal_presence_is_retained_for_recent_provenance() {
        let temp = TempDir::new().unwrap();
        let store = ActorPresenceStore::new(temp.path());
        let mut complete = presence("agent-done", ActorPresenceStatus::Complete);
        complete.completed_at = Some(Utc::now());
        store.save(&complete).unwrap();

        let loaded = store.load("agent-done").unwrap().unwrap();
        assert_eq!(loaded.status, ActorPresenceStatus::Complete);
    }
}
