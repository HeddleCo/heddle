// SPDX-License-Identifier: Apache-2.0
//! Actor domain: list/show assembly plus pure spawn/done planning.
//!
//! Owns:
//! - listing registry entries into a typed report with stable JSON field names
//! - pure active-only (and related) filters over [`AgentEntry`] slices
//! - assembling a single actor entry plus ancestry chain for show/spawn JSON
//! - pure [`plan_actor_spawn`] / [`build_spawn_entry`] for `actor spawn`
//! - pure [`complete_actor_entry`] / [`plan_actor_done`] and thin
//!   [`mark_actor_done`] for `actor done`
//!
//! Human/JSON rendering and implicit session resolution (current-lane / path /
//! any-active fallbacks) stay CLI-owned because they couple to recovery advice
//! and checkout state. Thread-ref creation on mint also stays CLI-owned.

use anyhow::Result;
use chrono::{DateTime, Utc};
use objects::store::{ActorChainNode, AgentEntry, AgentRegistry, AgentStatus, AgentUsageSummary};
use repo::{Repository, ThreadId, ThreadIdError};
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
    pub heartbeat_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<String>,
    pub liveness: String,
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
            heartbeat_at: entry.heartbeat_at.map(|ts| ts.to_rfc3339()),
            lease_expires_at: entry.lease_expires_at().map(|ts| ts.to_rfc3339()),
            liveness: AgentRegistry::liveness_for(entry).to_string(),
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
    let entries = registry.current_entries()?;
    let entries = filter_actors(entries, active_only);
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

// ---------------------------------------------------------------------------
// Spawn planning
// ---------------------------------------------------------------------------

/// Caller-supplied spawn inputs for pure planning.
///
/// Field names mirror the CLI `actor spawn` surface plus resolved host facts
/// (probe, HEAD base state, current lane). Harness probing and HEAD resolution
/// stay caller-owned.
#[derive(Debug, Clone, PartialEq)]
pub struct ActorSpawnOptions {
    /// Explicit `--thread` name (CLI conflicts with `no_thread`).
    pub thread: Option<String>,
    /// Attach to the current lane without minting `actor/<session>`.
    pub no_thread: bool,
    /// Current checkout lane when known. Required when `no_thread` is true.
    pub current_lane: Option<String>,
    /// CLI `--provider` (empty/whitespace treated as unset).
    pub provider: Option<String>,
    /// CLI `--model` (empty/whitespace treated as unset).
    pub model: Option<String>,
    /// Harness probe fields (caller-resolved; unused when identity is explicit).
    pub probe_provider: Option<String>,
    pub probe_model: Option<String>,
    pub harness: Option<String>,
    pub thinking_level: Option<String>,
    pub probe_source: Option<String>,
    pub probe_confidence: Option<f32>,
    /// Full base state id string for `anchor_state`.
    pub base_state_full: String,
    /// Short display form for `base_state`.
    pub base_state_short: String,
}

/// How the spawn thread name is determined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorSpawnThreadSource {
    /// Caller supplied `--thread <name>`.
    Explicit(String),
    /// `--no-thread` attached to the current lane.
    CurrentLane(String),
    /// Default: mint `actor/<session_id>` once the registry allocates an id.
    GeneratedFromSession,
}

/// Attach rule used for registry attribution on spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorSpawnAttachMode {
    /// Fresh spawn (optional explicit thread or generated `actor/<session>`).
    ExplicitSpawn,
    /// `--no-thread` attach to the current lane.
    NoThreadAttach,
}

impl ActorSpawnAttachMode {
    /// Registry `winning_attach_rule` / precedence token.
    pub fn rule(self) -> &'static str {
        match self {
            Self::ExplicitSpawn => "explicit-actor-spawn",
            Self::NoThreadAttach => "no-thread-attach",
        }
    }

    /// Human attach-reason string persisted on the registry entry.
    pub fn reason(self, session_id: &str, thread: &str) -> String {
        match self {
            Self::ExplicitSpawn => {
                format!("actor {session_id} was spawned explicitly on thread {thread}")
            }
            Self::NoThreadAttach => format!(
                "actor {session_id} was attached to the current thread {thread} without minting a new thread"
            ),
        }
    }
}

/// Pure plan for `heddle actor spawn` after option preflight.
///
/// Thread-ref creation, registry writes, and harness probing remain with the
/// caller. Finalize the thread name with [`resolve_spawn_thread_name`] once a
/// session id is allocated, then [`build_spawn_entry`].
#[derive(Debug, Clone, PartialEq)]
pub struct ActorSpawnPlan {
    pub thread_source: ActorSpawnThreadSource,
    /// When true, caller should create a thread ref if it does not exist.
    pub mint_thread_if_missing: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub harness: Option<String>,
    pub thinking_level: Option<String>,
    pub probe_source: Option<String>,
    pub probe_confidence: Option<f32>,
    pub attach_mode: ActorSpawnAttachMode,
    pub base_state_full: String,
    pub base_state_short: String,
}

/// Failures from pure actor spawn planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorSpawnError {
    /// Explicit `--thread` failed the safe-slug / reserved-structure rule.
    InvalidThreadName(ThreadIdError),
    /// `--no-thread` with no current lane to attach to.
    NoCurrentLane,
}

impl std::fmt::Display for ActorSpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidThreadName(err) => write!(f, "{err}"),
            Self::NoCurrentLane => write!(
                f,
                "cannot attach with --no-thread: HEAD is not attached to a thread"
            ),
        }
    }
}

impl std::error::Error for ActorSpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidThreadName(err) => Some(err),
            Self::NoCurrentLane => None,
        }
    }
}

impl From<ThreadIdError> for ActorSpawnError {
    fn from(value: ThreadIdError) -> Self {
        Self::InvalidThreadName(value)
    }
}

/// Treat empty/whitespace CLI attribution strings as unset.
pub fn nonempty_attr(value: Option<String>) -> Option<String> {
    value.and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

/// True when the caller supplied a non-empty `--provider` and/or `--model`.
pub fn is_explicit_identity(provider: &Option<String>, model: &Option<String>) -> bool {
    provider
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty())
        || model.as_ref().is_some_and(|value| !value.trim().is_empty())
}

/// Default thread name minted for a generated actor session.
pub fn default_actor_thread_name(session_id: &str) -> String {
    format!("actor/{session_id}")
}

/// Pure preflight for `heddle actor spawn`.
///
/// Validates explicit thread names, enforces `--no-thread` lane requirements,
/// and resolves identity / attach fields that do not need I/O.
pub fn plan_actor_spawn(options: &ActorSpawnOptions) -> Result<ActorSpawnPlan, ActorSpawnError> {
    let (thread_source, mint_thread_if_missing, attach_mode) = if options.no_thread {
        let lane = options
            .current_lane
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or(ActorSpawnError::NoCurrentLane)?;
        (
            ActorSpawnThreadSource::CurrentLane(lane.to_string()),
            false,
            ActorSpawnAttachMode::NoThreadAttach,
        )
    } else if let Some(name) = options.thread.as_deref() {
        let validated = ThreadId::new(name).map_err(ActorSpawnError::from)?;
        (
            ActorSpawnThreadSource::Explicit(validated.as_str().to_string()),
            true,
            ActorSpawnAttachMode::ExplicitSpawn,
        )
    } else {
        (
            ActorSpawnThreadSource::GeneratedFromSession,
            true,
            ActorSpawnAttachMode::ExplicitSpawn,
        )
    };

    let explicit = is_explicit_identity(&options.provider, &options.model);
    let provider =
        nonempty_attr(options.provider.clone()).or_else(|| options.probe_provider.clone());
    let model = nonempty_attr(options.model.clone()).or_else(|| options.probe_model.clone());
    let probe_source = if explicit {
        Some("explicit_payload".to_string())
    } else {
        options.probe_source.clone()
    };
    let probe_confidence = if explicit {
        Some(1.0)
    } else {
        options.probe_confidence
    };

    Ok(ActorSpawnPlan {
        thread_source,
        mint_thread_if_missing,
        provider,
        model,
        harness: options.harness.clone(),
        thinking_level: options.thinking_level.clone(),
        probe_source,
        probe_confidence,
        attach_mode,
        base_state_full: options.base_state_full.clone(),
        base_state_short: options.base_state_short.clone(),
    })
}

/// Resolve the concrete thread name once a session id is allocated.
pub fn resolve_spawn_thread_name(plan: &ActorSpawnPlan, session_id: &str) -> String {
    match &plan.thread_source {
        ActorSpawnThreadSource::Explicit(name) | ActorSpawnThreadSource::CurrentLane(name) => {
            name.clone()
        }
        ActorSpawnThreadSource::GeneratedFromSession => default_actor_thread_name(session_id),
    }
}

/// Pure assembly of a registry entry from a spawn plan and session runtime fields.
///
/// Does not touch the filesystem. Caller supplies `pid`, `reservation_token`,
/// and `now` so tests can inject deterministic values.
pub fn build_spawn_entry(
    plan: &ActorSpawnPlan,
    session_id: &str,
    pid: Option<u32>,
    reservation_token: Option<String>,
    now: DateTime<Utc>,
) -> AgentEntry {
    let thread = resolve_spawn_thread_name(plan, session_id);
    let rule = plan.attach_mode.rule().to_string();
    AgentEntry {
        session_id: session_id.to_string(),
        client_instance_id: None,
        native_actor_key: None,
        native_parent_actor_key: None,
        native_instance_key: None,
        heddle_session_id: None,
        thread_id: None,
        thread: thread.clone(),
        pid,
        boot_id: None,
        heartbeat_at: Some(now),
        anchor_state: Some(plan.base_state_full.clone()),
        anchor_root: None,
        reservation_token,
        path: None,
        base_state: plan.base_state_short.clone(),
        started_at: now,
        provider: plan.provider.clone(),
        model: plan.model.clone(),
        harness: plan.harness.clone(),
        thinking_level: plan.thinking_level.clone(),
        usage_summary: AgentUsageSummary::default(),
        last_progress_at: None,
        report_flush_state: None,
        attach_reason: Some(plan.attach_mode.reason(session_id, &thread)),
        task_assignment_id: None,
        attach_precedence: vec![rule.clone()],
        winning_attach_rule: Some(rule),
        probe_source: plan.probe_source.clone(),
        probe_confidence: plan.probe_confidence,
        status: AgentStatus::Active,
        completed_at: None,
        context_queries: vec![],
    }
}

// ---------------------------------------------------------------------------
// Done planning
// ---------------------------------------------------------------------------

/// Caller inputs for `actor done` planning (identity already resolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorDoneOptions {
    pub session_id: String,
}

/// Pure plan describing the done transition for machine/human output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorDonePlan {
    pub session_id: String,
    pub thread: String,
    /// Always [`AgentStatus::Complete`] for a successful done plan.
    pub status: AgentStatus,
}

/// Build a done plan from a resolved registry entry (no I/O).
pub fn plan_actor_done(entry: &AgentEntry) -> ActorDonePlan {
    ActorDonePlan {
        session_id: entry.session_id.clone(),
        thread: entry.thread.clone(),
        status: AgentStatus::Complete,
    }
}

/// Pure status transition for `actor done`: mark complete with a timestamp.
pub fn complete_actor_entry(mut entry: AgentEntry, completed_at: DateTime<Utc>) -> AgentEntry {
    entry.status = AgentStatus::Complete;
    entry.completed_at = Some(completed_at);
    entry
}

/// Mark an actor complete in the registry (thin store mutation).
pub fn mark_actor_done(registry: &AgentRegistry, session_id: &str) -> Result<()> {
    registry.update_status(session_id, AgentStatus::Complete)?;
    Ok(())
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

    fn spawn_options() -> ActorSpawnOptions {
        ActorSpawnOptions {
            thread: None,
            no_thread: false,
            current_lane: None,
            provider: None,
            model: None,
            probe_provider: Some("probe-provider".to_string()),
            probe_model: Some("probe-model".to_string()),
            harness: Some("codex".to_string()),
            thinking_level: Some("high".to_string()),
            probe_source: Some("env".to_string()),
            probe_confidence: Some(0.7),
            base_state_full: "abcdef0123456789".to_string(),
            base_state_short: "abcdef0".to_string(),
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

    #[test]
    fn plan_spawn_defaults_to_generated_thread() {
        let plan = plan_actor_spawn(&spawn_options()).unwrap();
        assert_eq!(
            plan.thread_source,
            ActorSpawnThreadSource::GeneratedFromSession
        );
        assert!(plan.mint_thread_if_missing);
        assert_eq!(plan.attach_mode, ActorSpawnAttachMode::ExplicitSpawn);
        assert_eq!(plan.provider.as_deref(), Some("probe-provider"));
        assert_eq!(plan.model.as_deref(), Some("probe-model"));
        assert_eq!(plan.probe_source.as_deref(), Some("env"));
        assert_eq!(plan.probe_confidence, Some(0.7));
        assert_eq!(
            resolve_spawn_thread_name(&plan, "agent-abc"),
            "actor/agent-abc"
        );
    }

    #[test]
    fn plan_spawn_explicit_thread_validates_name() {
        let mut options = spawn_options();
        options.thread = Some("feature/ok".to_string());
        let plan = plan_actor_spawn(&options).unwrap();
        assert_eq!(
            plan.thread_source,
            ActorSpawnThreadSource::Explicit("feature/ok".to_string())
        );
        assert!(plan.mint_thread_if_missing);

        options.thread = Some("../escape".to_string());
        assert!(matches!(
            plan_actor_spawn(&options),
            Err(ActorSpawnError::InvalidThreadName(_))
        ));
    }

    #[test]
    fn plan_spawn_no_thread_requires_current_lane() {
        let mut options = spawn_options();
        options.no_thread = true;
        options.current_lane = None;
        assert_eq!(
            plan_actor_spawn(&options),
            Err(ActorSpawnError::NoCurrentLane)
        );

        options.current_lane = Some("main".to_string());
        let plan = plan_actor_spawn(&options).unwrap();
        assert_eq!(
            plan.thread_source,
            ActorSpawnThreadSource::CurrentLane("main".to_string())
        );
        assert!(!plan.mint_thread_if_missing);
        assert_eq!(plan.attach_mode, ActorSpawnAttachMode::NoThreadAttach);
        assert_eq!(plan.attach_mode.rule(), "no-thread-attach");
    }

    #[test]
    fn plan_spawn_explicit_identity_overrides_probe() {
        let mut options = spawn_options();
        options.provider = Some("  anthropic  ".to_string());
        options.model = Some("claude".to_string());
        let plan = plan_actor_spawn(&options).unwrap();
        assert_eq!(plan.provider.as_deref(), Some("anthropic"));
        assert_eq!(plan.model.as_deref(), Some("claude"));
        assert_eq!(plan.probe_source.as_deref(), Some("explicit_payload"));
        assert_eq!(plan.probe_confidence, Some(1.0));
    }

    #[test]
    fn plan_spawn_empty_cli_identity_falls_back_to_probe() {
        let mut options = spawn_options();
        options.provider = Some("   ".to_string());
        options.model = Some("".to_string());
        let plan = plan_actor_spawn(&options).unwrap();
        assert!(!is_explicit_identity(&options.provider, &options.model));
        assert_eq!(plan.provider.as_deref(), Some("probe-provider"));
        assert_eq!(plan.model.as_deref(), Some("probe-model"));
        assert_eq!(plan.probe_source.as_deref(), Some("env"));
        assert_eq!(plan.probe_confidence, Some(0.7));
    }

    #[test]
    fn build_spawn_entry_assembles_active_registry_fields() {
        let plan = plan_actor_spawn(&spawn_options()).unwrap();
        let now = Utc::now();
        let entry = build_spawn_entry(&plan, "agent-xyz", Some(42), Some("token".to_string()), now);
        assert_eq!(entry.session_id, "agent-xyz");
        assert_eq!(entry.thread, "actor/agent-xyz");
        assert_eq!(entry.pid, Some(42));
        assert_eq!(entry.reservation_token.as_deref(), Some("token"));
        assert_eq!(entry.base_state, "abcdef0");
        assert_eq!(entry.anchor_state.as_deref(), Some("abcdef0123456789"));
        assert_eq!(entry.status, AgentStatus::Active);
        assert_eq!(entry.started_at, now);
        assert_eq!(entry.heartbeat_at, Some(now));
        assert_eq!(
            entry.winning_attach_rule.as_deref(),
            Some("explicit-actor-spawn")
        );
        assert_eq!(
            entry.attach_precedence,
            vec!["explicit-actor-spawn".to_string()]
        );
        assert!(
            entry
                .attach_reason
                .as_deref()
                .unwrap()
                .contains("spawned explicitly")
        );
        assert!(entry.completed_at.is_none());
    }

    #[test]
    fn complete_actor_entry_sets_status_and_timestamp() {
        let entry = sample_entry("agent-1", AgentStatus::Active, "t1");
        let done_at = Utc::now();
        let completed = complete_actor_entry(entry, done_at);
        assert_eq!(completed.status, AgentStatus::Complete);
        assert_eq!(completed.completed_at, Some(done_at));
    }

    #[test]
    fn plan_actor_done_captures_session_and_thread() {
        let entry = sample_entry("agent-1", AgentStatus::Active, "feature/x");
        let plan = plan_actor_done(&entry);
        assert_eq!(plan.session_id, "agent-1");
        assert_eq!(plan.thread, "feature/x");
        assert_eq!(plan.status, AgentStatus::Complete);
    }

    #[test]
    fn mark_actor_done_updates_registry() {
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
        mark_actor_done(&registry, "agent-active").unwrap();
        let loaded = registry.load("agent-active").unwrap().unwrap();
        assert_eq!(loaded.status, AgentStatus::Complete);
        assert!(loaded.completed_at.is_some());
    }
}
