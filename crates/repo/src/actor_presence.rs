// SPDX-License-Identifier: Apache-2.0
//! Durable actor presence and work-context records.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use objects::error::{HeddleError, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
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

mod sqlite;
pub(crate) use sqlite::initialize_schema;

/// Actor presence in the shared object-store metadata database.
/// Presence is independent of checkout writer leases.
pub struct ActorPresenceStore {
    heddle_dir: PathBuf,
}

impl ActorPresenceStore {
    pub fn new(heddle_dir: &Path) -> Self {
        Self {
            heddle_dir: heddle_dir.to_path_buf(),
        }
    }

    pub fn current_entries(&self) -> Result<Vec<ActorPresence>> {
        self.list()
    }
    pub fn active_entries(&self) -> Result<Vec<ActorPresence>> {
        self.query("status='active'", &[], false)
    }
    pub fn list(&self) -> Result<Vec<ActorPresence>> {
        self.query("1=1", &[], true)
    }
    /// Cleanup previews are read-only and include terminal records past TTL.
    pub fn list_without_pruning(&self) -> Result<Vec<ActorPresence>> {
        self.query("1=1", &[], false)
    }
    pub fn load(&self, session_id: &str) -> Result<Option<ActorPresence>> {
        sqlite::validate_id(session_id)?;
        self.first("session_id=?1", &[&session_id], true)
    }
    pub fn save(&self, entry: &ActorPresence) -> Result<()> {
        self.mutate(|db| sqlite::save(db, entry))
    }
    fn create_generated_entry_with<F, G>(
        &self,
        mut generate_id: G,
        mut build: F,
    ) -> Result<ActorPresence>
    where
        F: FnMut(&str) -> Result<ActorPresence>,
        G: FnMut() -> String,
    {
        self.mutate(|db| {
            loop {
                let id = generate_id();
                sqlite::validate_id(&id)?;
                if sqlite::load(db, &id)?.is_some() {
                    continue;
                }
                let entry = build(&id)?;
                if entry.session_id != id {
                    return Err(HeddleError::Config(
                        "generated actor changed its session identity".into(),
                    ));
                }
                sqlite::save(db, &entry)?;
                return Ok(entry);
            }
        })
    }
    pub fn create_generated_entry<F>(&self, build: F) -> Result<ActorPresence>
    where
        F: FnMut(&str) -> Result<ActorPresence>,
    {
        self.create_generated_entry_with(generate_actor_session_id, build)
    }
    pub fn update_entry<F>(&self, session_id: &str, mut update: F) -> Result<Option<ActorPresence>>
    where
        F: FnMut(&mut ActorPresence),
    {
        sqlite::validate_id(session_id)?;
        self.mutate(|db| {
            let Some(mut entry) = sqlite::load(db, session_id)? else {
                return Ok(None);
            };
            update(&mut entry);
            if entry.session_id != session_id {
                return Err(HeddleError::Config(
                    "actor update changed its session identity".into(),
                ));
            }
            sqlite::save(db, &entry)?;
            Ok(Some(entry))
        })
    }
    pub fn update_status(&self, session_id: &str, status: ActorPresenceStatus) -> Result<()> {
        self.update_entry(session_id, |entry| {
            entry.status = status.clone();
            entry.completed_at = if status == ActorPresenceStatus::Active {
                None
            } else {
                Some(Utc::now())
            };
        })?;
        Ok(())
    }
    /// The indexed native identity narrows candidates before the compatibility
    /// predicate. Selection, update and creation share one write transaction.
    pub fn find_or_create_active_entry<FMatch, FUpdate, FBuild>(
        &self,
        native_actor_key: &str,
        mut matches: FMatch,
        mut update: FUpdate,
        mut build: FBuild,
    ) -> Result<(ActorPresence, bool)>
    where
        FMatch: FnMut(&ActorPresence) -> bool,
        FUpdate: FnMut(&mut ActorPresence),
        FBuild: FnMut(&str) -> Result<ActorPresence>,
    {
        self.mutate(|db| {
            for mut entry in sqlite::query(
                db,
                "status='active' AND native_actor_key=?1",
                &[&native_actor_key],
                false,
            )? {
                if matches(&entry) {
                    let id = entry.session_id.clone();
                    update(&mut entry);
                    if entry.session_id != id
                        || entry.native_actor_key.as_deref() != Some(native_actor_key)
                    {
                        return Err(HeddleError::Config(
                            "actor update changed its indexed identity".into(),
                        ));
                    }
                    sqlite::save(db, &entry)?;
                    return Ok((entry, false));
                }
            }
            loop {
                let id = generate_actor_session_id();
                if sqlite::load(db, &id)?.is_some() {
                    continue;
                }
                let entry = build(&id)?;
                if entry.session_id != id
                    || entry.native_actor_key.as_deref() != Some(native_actor_key)
                {
                    return Err(HeddleError::Config(
                        "new actor differs from requested identity".into(),
                    ));
                }
                sqlite::save(db, &entry)?;
                return Ok((entry, true));
            }
        })
    }
    pub fn find_active_by_path(&self, path: &Path) -> Result<Option<ActorPresence>> {
        let path = sqlite::path_key(path);
        self.first("status='active' AND path=?1", &[&path], false)
    }
    pub fn find_active_by_heddle_session_id(&self, id: &str) -> Result<Option<ActorPresence>> {
        self.first("status='active' AND heddle_session_id=?1", &[&id], false)
    }
    pub fn find_active_by_client_instance_id(&self, id: &str) -> Result<Option<ActorPresence>> {
        self.first("status='active' AND client_instance_id=?1", &[&id], false)
    }
    pub fn find_active_by_native_actor_key(&self, id: &str) -> Result<Option<ActorPresence>> {
        self.first("status='active' AND native_actor_key=?1", &[&id], false)
    }
    pub fn find_active_by_native_instance_key_at_path(
        &self,
        id: &str,
        path: &Path,
    ) -> Result<Option<ActorPresence>> {
        let path = sqlite::path_key(path);
        self.first(
            "status='active' AND native_instance_key=?1 AND path=?2",
            &[&id, &path],
            false,
        )
    }
    pub fn actor_chain_for_session(&self, session_id: &str) -> Result<Vec<ActorChainNode>> {
        let Some(mut current) = self.load(session_id)? else {
            return Ok(Vec::new());
        };
        let mut chain = vec![ActorChainNode::from(&current)];
        let mut seen = HashSet::from([current.session_id.clone()]);
        while let Some(parent_key) = current.native_parent_actor_key.as_deref() {
            let Some(parent) = self.first("native_actor_key=?1", &[&parent_key], true)? else {
                break;
            };
            if !seen.insert(parent.session_id.clone()) {
                break;
            }
            chain.push(ActorChainNode::from(&parent));
            current = parent;
        }
        chain.reverse();
        Ok(chain)
    }
    pub fn log_context_query(&self, session_id: &str, query: ContextQueryEntry) -> Result<()> {
        self.update_entry(session_id, |entry| {
            if entry.status == ActorPresenceStatus::Active {
                entry.context_queries.push(query.clone());
            }
        })?;
        Ok(())
    }
    pub fn delete(&self, session_id: &str) -> Result<()> {
        sqlite::validate_id(session_id)?;
        self.mutate(|db| {
            db.execute(
                "DELETE FROM actor_presence WHERE session_id=?1",
                [session_id],
            )
            .map_err(sqlite::error)?;
            Ok(())
        })
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
                "codex:thread:one",
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
                "codex:thread:one",
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

    #[test]
    fn independent_connections_reuse_one_actor_and_preserve_updates() {
        let temp = TempDir::new().expect("temporary metadata store");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let root = temp.path().to_path_buf();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let store = ActorPresenceStore::new(&root);
                    barrier.wait();
                    let (entry, _) = store
                        .find_or_create_active_entry(
                            "codex:shared",
                            |_| true,
                            |_| {},
                            |id| {
                                let mut entry = presence(id, ActorPresenceStatus::Active);
                                entry.native_actor_key = Some("codex:shared".into());
                                Ok(entry)
                            },
                        )
                        .expect("atomic actor reuse");
                    for _ in 0..16 {
                        store
                            .update_entry(&entry.session_id, |entry| {
                                entry.usage_summary.tool_calls =
                                    Some(entry.usage_summary.tool_calls.unwrap_or(0) + 1);
                            })
                            .expect("atomic increment");
                    }
                    entry.session_id
                })
            })
            .collect();
        let ids: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("actor worker"))
            .collect();
        assert_eq!(ids[0], ids[1]);
        let entries = ActorPresenceStore::new(temp.path())
            .active_entries()
            .expect("active actors");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].usage_summary.tool_calls, Some(32));
    }

    #[test]
    fn indexed_lookup_chooses_latest_actor_and_follows_updated_fields() {
        let temp = TempDir::new().expect("temporary metadata store");
        let store = ActorPresenceStore::new(temp.path());
        let mut old = presence("agent-old", ActorPresenceStatus::Active);
        old.native_actor_key = Some("codex:same".into());
        old.started_at -= chrono::Duration::days(1);
        store.save(&old).expect("old actor");
        let mut newer = presence("agent-new", ActorPresenceStatus::Active);
        newer.native_actor_key = old.native_actor_key.clone();
        newer.client_instance_id = Some("client-one".into());
        newer.heddle_session_id = Some("session-one".into());
        newer.native_instance_key = Some("instance-one".into());
        newer.path = Some(temp.path().to_path_buf());
        store.save(&newer).expect("new actor");
        assert_eq!(
            store
                .find_active_by_native_actor_key("codex:same")
                .expect("lookup")
                .expect("actor")
                .session_id,
            newer.session_id
        );
        assert!(
            store
                .find_active_by_client_instance_id("client-one")
                .expect("client lookup")
                .is_some()
        );
        assert!(
            store
                .find_active_by_heddle_session_id("session-one")
                .expect("session lookup")
                .is_some()
        );
        assert!(
            store
                .find_active_by_native_instance_key_at_path("instance-one", temp.path())
                .expect("instance lookup")
                .is_some()
        );
        store
            .update_entry(&newer.session_id, |entry| {
                entry.client_instance_id = Some("client-two".into())
            })
            .expect("update index");
        assert!(
            store
                .find_active_by_client_instance_id("client-one")
                .expect("old client lookup")
                .is_none()
        );
        assert!(
            store
                .find_active_by_client_instance_id("client-two")
                .expect("new client lookup")
                .is_some()
        );
        let db = crate::local_metadata::open_existing(temp.path())
            .expect("metadata")
            .expect("existing");
        let plan: String = db.query_row("EXPLAIN QUERY PLAN SELECT payload FROM actor_presence WHERE status='active' AND native_actor_key='codex:same' ORDER BY started_at DESC,session_id LIMIT 1", [], |row| row.get(3)).expect("query plan");
        assert!(
            plan.contains("SEARCH actor_presence USING INDEX actor_presence_native"),
            "{plan}"
        );
    }

    #[test]
    fn terminal_cleanup_is_bounded_and_never_expires_active_actors() {
        let temp = TempDir::new().expect("temporary metadata store");
        let store = ActorPresenceStore::new(temp.path());
        let db = crate::local_metadata::open(temp.path()).expect("metadata");
        for number in 0..600 {
            let mut entry = presence(
                &format!("agent-done-{number}"),
                ActorPresenceStatus::Complete,
            );
            entry.started_at -= chrono::Duration::days(9);
            sqlite::save(&db, &entry).expect("seed terminal history");
        }
        assert_eq!(
            store
                .list_without_pruning()
                .expect("read-only preview")
                .len(),
            600
        );
        assert!(store.list().expect("current list").is_empty());
        assert_eq!(
            store
                .list_without_pruning()
                .expect("preview remains read-only")
                .len(),
            600
        );
        let mut active = presence("agent-live", ActorPresenceStatus::Active);
        active.started_at -= chrono::Duration::days(90);
        store.save(&active).expect("bounded cleanup");
        assert_eq!(
            store.list_without_pruning().expect("retained rows").len(),
            345
        );
        assert_eq!(store.active_entries().expect("active rows").len(), 1);
    }

    #[test]
    fn failed_actor_identity_update_preserves_record_and_change_cursor() {
        let temp = TempDir::new().expect("temporary metadata store");
        let store = ActorPresenceStore::new(temp.path());
        store
            .save(&presence("agent-one", ActorPresenceStatus::Active))
            .expect("actor");
        let mut db = crate::local_metadata::open_existing(temp.path())
            .expect("metadata")
            .expect("existing");
        let before = crate::local_metadata::changes(&mut db, 0, 1024)
            .expect("cursor")
            .cursor;
        assert!(
            store
                .update_entry("agent-one", |entry| entry.session_id = "agent-other".into())
                .is_err()
        );
        assert!(
            store
                .load("agent-other")
                .expect("absent renamed actor")
                .is_none()
        );
        assert_eq!(
            crate::local_metadata::changes(&mut db, before, 1024)
                .expect("unchanged cursor")
                .cursor,
            before
        );
        assert!(!temp.path().join("actor-presence").exists());
    }
}
