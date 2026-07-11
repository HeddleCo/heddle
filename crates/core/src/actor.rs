// SPDX-License-Identifier: Apache-2.0
//! Actor list/show domain: registry listing, pure filters, entry assembly.
//!
//! Owns the **read-side** of `heddle actor list` / `heddle actor show`:
//! - listing registry entries into a typed report with stable JSON field names
//! - pure active-only (and related) filters over [`AgentEntry`] slices
//! - assembling a single actor entry plus ancestry chain for show/spawn JSON
//!
//! Mutation (`actor spawn` / `actor done`) and human/JSON rendering stay CLI-owned.
//! Implicit session resolution (current-lane / path / any-active fallbacks) also
//! stays CLI-owned because it couples to recovery advice and checkout state.

use anyhow::Result;
use objects::store::{ActorChainNode, AgentEntry, AgentRegistry, AgentStatus, AgentUsageSummary};
use repo::Repository;
use serde::Serialize;

/// Machine JSON for `heddle actor list` domain fields (stable field names).
///
/// CLI may wrap this with a `verification` envelope; domain fields here match
/// the public `actor_list` contract (`output_kind`, `actors`, `active_only`).
#[derive(Debug, Clone, Serialize)]
pub struct ActorListReport {
    pub output_kind: &'static str,
    pub actors: Vec<ActorEntryReport>,
    pub active_only: bool,
}

/// Machine JSON for a single-actor payload (`actor_show` / `actor_spawn` body).
///
/// Domain portion only — CLI supplies `output_kind` and `verification` when
/// rendering machine output.
#[derive(Debug, Clone, Serialize)]
pub struct ActorShowReport {
    pub actor: ActorEntryReport,
}

/// One actor registry entry as surfaced by list/show machine JSON.
///
/// Field names match the existing CLI contract (`session_id`, `thread`,
/// `status`, `actor_chain`, …).
#[derive(Debug, Clone, Serialize)]
pub struct ActorEntryReport {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_actor_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_parent_actor_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_instance_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heddle_session_id: Option<String>,
    pub thread: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub base_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    pub usage_summary: AgentUsageSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_progress_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_flush_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_reason: Option<String>,
    pub attach_precedence: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub winning_attach_rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_confidence: Option<f32>,
    pub status: String,
    pub started_at: String,
    pub actor_chain: Vec<ActorChainEntry>,
}

/// One hop in an actor ancestry chain for machine JSON.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ActorChainEntry {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_actor_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_parent_actor_key: Option<String>,
    pub thread: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
}

impl From<ActorChainNode> for ActorChainEntry {
    fn from(node: ActorChainNode) -> Self {
        Self {
            session_id: node.session_id,
            native_actor_key: node.native_actor_key,
            native_parent_actor_key: node.native_parent_actor_key,
            thread: node.thread,
            status: node.status.to_string(),
            provider: node.provider,
            model: node.model,
            harness: node.harness,
        }
    }
}

impl From<&AgentEntry> for ActorEntryReport {
    fn from(entry: &AgentEntry) -> Self {
        Self {
            session_id: entry.session_id.clone(),
            client_instance_id: entry.client_instance_id.clone(),
            native_actor_key: entry.native_actor_key.clone(),
            native_parent_actor_key: entry.native_parent_actor_key.clone(),
            native_instance_key: entry.native_instance_key.clone(),
            heddle_session_id: entry.heddle_session_id.clone(),
            thread: entry.thread.clone(),
            thread_id: entry.thread_id.clone(),
            base_state: entry.base_state.clone(),
            path: entry.path.as_ref().map(|path| path.display().to_string()),
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            harness: entry.harness.clone(),
            thinking_level: entry.thinking_level.clone(),
            usage_summary: entry.usage_summary.clone(),
            last_progress_at: entry.last_progress_at.map(|ts| ts.to_rfc3339()),
            report_flush_state: entry.report_flush_state.clone(),
            attach_reason: entry.attach_reason.clone(),
            attach_precedence: entry.attach_precedence.clone(),
            winning_attach_rule: entry.winning_attach_rule.clone(),
            probe_source: entry.probe_source.clone(),
            probe_confidence: entry.probe_confidence,
            status: entry.status.to_string(),
            started_at: entry.started_at.to_rfc3339(),
            actor_chain: vec![],
        }
    }
}

impl ActorEntryReport {
    /// Attach an ancestry chain (root → leaf) to this entry report.
    pub fn with_chain(mut self, chain: Vec<ActorChainNode>) -> Self {
        self.actor_chain = chain.into_iter().map(ActorChainEntry::from).collect();
        self
    }
}

/// Pure filter: when `active_only`, retain only [`AgentStatus::Active`] entries.
pub fn filter_actors(entries: Vec<AgentEntry>, active_only: bool) -> Vec<AgentEntry> {
    if !active_only {
        return entries;
    }
    entries
        .into_iter()
        .filter(|entry| entry.status == AgentStatus::Active)
        .collect()
}

/// Pure filter over a borrowed slice (for callers that already hold entries).
pub fn filter_actors_ref<'a>(
    entries: impl IntoIterator<Item = &'a AgentEntry>,
    active_only: bool,
) -> Vec<&'a AgentEntry> {
    entries
        .into_iter()
        .filter(|entry| !active_only || entry.status == AgentStatus::Active)
        .collect()
}

/// List actors from the repository agent registry into a typed report.
///
/// Applies the active-only filter when requested. Does not attach verification
/// or render human text — CLI owns the envelope and presentation.
pub fn list_actors(repo: &Repository, active_only: bool) -> Result<ActorListReport> {
    let registry = AgentRegistry::new(repo.heddle_dir());
    list_actors_from_registry(&registry, active_only)
}

/// List actors from an already-opened [`AgentRegistry`].
pub fn list_actors_from_registry(
    registry: &AgentRegistry,
    active_only: bool,
) -> Result<ActorListReport> {
    let entries = filter_actors(registry.list()?, active_only);
    Ok(ActorListReport {
        output_kind: "actor_list",
        actors: entries.iter().map(ActorEntryReport::from).collect(),
        active_only,
    })
}

/// Assemble a single actor entry plus ancestry chain (pure relative to I/O).
///
/// Used by `actor show` / `actor spawn` machine JSON after the caller has
/// resolved which registry entry to surface.
pub fn assemble_actor_entry(
    registry: &AgentRegistry,
    entry: &AgentEntry,
) -> Result<ActorEntryReport> {
    let chain = registry.actor_chain_for_session(&entry.session_id)?;
    Ok(ActorEntryReport::from(entry).with_chain(chain))
}

/// Load and assemble one actor by explicit session id.
///
/// Returns `Ok(None)` when the session is not in the registry. Does **not**
/// perform implicit current-lane / path / any-active resolution — that stays
/// CLI-owned (recovery advice + checkout predicates).
pub fn show_actor_by_session(
    repo: &Repository,
    session_id: &str,
) -> Result<Option<ActorShowReport>> {
    let registry = AgentRegistry::new(repo.heddle_dir());
    let Some(entry) = registry.load(session_id)? else {
        return Ok(None);
    };
    Ok(Some(ActorShowReport {
        actor: assemble_actor_entry(&registry, &entry)?,
    }))
}

/// Assemble show payload from a resolved entry (after CLI implicit resolve).
pub fn show_actor_from_entry(
    registry: &AgentRegistry,
    entry: &AgentEntry,
) -> Result<ActorShowReport> {
    Ok(ActorShowReport {
        actor: assemble_actor_entry(registry, entry)?,
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use objects::store::{AgentEntry, AgentStatus, AgentUsageSummary};
    use tempfile::TempDir;

    use super::*;

    fn sample_entry(session_id: &str, status: AgentStatus, thread: &str) -> AgentEntry {
        AgentEntry {
            session_id: session_id.to_string(),
            client_instance_id: None,
            native_actor_key: None,
            native_parent_actor_key: None,
            native_instance_key: None,
            heddle_session_id: None,
            thread_id: None,
            thread: thread.to_string(),
            pid: None,
            boot_id: None,
            liveness_path: None,
            heartbeat_at: None,
            anchor_state: None,
            anchor_root: None,
            reservation_token: None,
            path: None,
            base_state: "abc123".to_string(),
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
            attach_precedence: vec!["explicit-actor-spawn".to_string()],
            winning_attach_rule: Some("explicit-actor-spawn".to_string()),
            probe_source: None,
            probe_confidence: None,
            status,
            completed_at: None,
            context_queries: vec![],
        }
    }

    #[test]
    fn filter_actors_active_only_keeps_active() {
        let entries = vec![
            sample_entry("a1", AgentStatus::Active, "t1"),
            sample_entry("a2", AgentStatus::Complete, "t2"),
            sample_entry("a3", AgentStatus::Active, "t3"),
            sample_entry("a4", AgentStatus::Merged, "t4"),
        ];
        let filtered = filter_actors(entries, true);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|e| e.status == AgentStatus::Active));
    }

    #[test]
    fn filter_actors_all_when_not_active_only() {
        let entries = vec![
            sample_entry("a1", AgentStatus::Active, "t1"),
            sample_entry("a2", AgentStatus::Complete, "t2"),
        ];
        let filtered = filter_actors(entries, false);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn entry_report_stable_json_field_names() {
        let entry = sample_entry("agent-test", AgentStatus::Active, "actor/agent-test");
        let report = ActorEntryReport::from(&entry);
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["session_id"], "agent-test");
        assert_eq!(value["thread"], "actor/agent-test");
        assert_eq!(value["status"], "active");
        assert_eq!(value["base_state"], "abc123");
        assert_eq!(value["provider"], "openai");
        assert_eq!(value["model"], "gpt-5");
        assert_eq!(value["harness"], "codex");
        assert!(value["started_at"].is_string());
        assert!(value["usage_summary"].is_object());
        assert!(value["attach_precedence"].is_array());
        assert!(value["actor_chain"].is_array());
        assert_eq!(value["actor_chain"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn list_report_stable_json_field_names() {
        let report = ActorListReport {
            output_kind: "actor_list",
            actors: vec![ActorEntryReport::from(&sample_entry(
                "agent-test",
                AgentStatus::Active,
                "main",
            ))],
            active_only: true,
        };
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["output_kind"], "actor_list");
        assert_eq!(value["active_only"], true);
        assert!(value["actors"].is_array());
        assert_eq!(value["actors"][0]["session_id"], "agent-test");
    }

    #[test]
    fn list_actors_from_empty_registry() {
        let temp = TempDir::new().unwrap();
        let heddle_dir = temp.path().join(".heddle");
        std::fs::create_dir_all(&heddle_dir).unwrap();
        let registry = AgentRegistry::new(&heddle_dir);
        let report = list_actors_from_registry(&registry, false).unwrap();
        assert_eq!(report.output_kind, "actor_list");
        assert!(report.actors.is_empty());
        assert!(!report.active_only);
    }

    #[test]
    fn list_actors_active_only_filters_registry() {
        let temp = TempDir::new().unwrap();
        let heddle_dir = temp.path().join(".heddle");
        std::fs::create_dir_all(&heddle_dir).unwrap();
        let registry = AgentRegistry::new(&heddle_dir);

        registry
            .save(&sample_entry(
                "agent-active",
                AgentStatus::Active,
                "t-active",
            ))
            .unwrap();
        registry
            .save(&sample_entry(
                "agent-complete",
                AgentStatus::Complete,
                "t-complete",
            ))
            .unwrap();

        let all = list_actors_from_registry(&registry, false).unwrap();
        assert_eq!(all.actors.len(), 2);

        let active_only = list_actors_from_registry(&registry, true).unwrap();
        assert_eq!(active_only.actors.len(), 1);
        assert_eq!(active_only.actors[0].session_id, "agent-active");
        assert_eq!(active_only.actors[0].status, "active");
        assert!(active_only.active_only);
    }

    #[test]
    fn with_chain_maps_nodes() {
        let entry = sample_entry("leaf", AgentStatus::Active, "t-leaf");
        let chain = vec![
            ActorChainNode {
                session_id: "root".to_string(),
                native_actor_key: Some("root-key".to_string()),
                native_parent_actor_key: None,
                thread: "t-root".to_string(),
                status: AgentStatus::Complete,
                provider: None,
                model: None,
                harness: None,
            },
            ActorChainNode {
                session_id: "leaf".to_string(),
                native_actor_key: Some("leaf-key".to_string()),
                native_parent_actor_key: Some("root-key".to_string()),
                thread: "t-leaf".to_string(),
                status: AgentStatus::Active,
                provider: Some("openai".to_string()),
                model: Some("gpt-5".to_string()),
                harness: Some("codex".to_string()),
            },
        ];
        let report = ActorEntryReport::from(&entry).with_chain(chain);
        assert_eq!(report.actor_chain.len(), 2);
        assert_eq!(report.actor_chain[0].session_id, "root");
        assert_eq!(report.actor_chain[0].status, "complete");
        assert_eq!(
            report.actor_chain[1].native_parent_actor_key.as_deref(),
            Some("root-key")
        );
    }
}
