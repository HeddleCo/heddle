// SPDX-License-Identifier: Apache-2.0
//! Pure agent reservation API helpers (non-fanout verbs).
//!
//! Owns:
//! - `agent capture` option/plan validation and thread-ownership checks
//! - `agent ready` plan assembly from a resolved active reservation
//! - `agent list` filter + reservation report assembly from [`AgentEntry`]
//! - attach/explain field assembly from registry facts
//! - pure heartbeat / release status transitions
//!
//! Registry I/O, recovery advice, harness probing, and human/JSON render stay
//! CLI-owned.

use chrono::{DateTime, Utc};
use objects::store::{AgentEntry, AgentRegistry, AgentStatus};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Session gate (shared by capture / ready)
// ---------------------------------------------------------------------------

/// Whether a loaded reservation may run a session-guarded mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSessionUse {
    /// Status is [`AgentStatus::Active`].
    Active,
    /// Terminal or otherwise non-active status.
    Inactive,
}

/// Pure status gate used after the caller loads a reservation by session id.
pub fn classify_agent_session_use(status: &AgentStatus) -> AgentSessionUse {
    if *status == AgentStatus::Active {
        AgentSessionUse::Active
    } else {
        AgentSessionUse::Inactive
    }
}

/// True when [`classify_agent_session_use`] is [`AgentSessionUse::Active`].
pub fn session_is_active(status: &AgentStatus) -> bool {
    classify_agent_session_use(status) == AgentSessionUse::Active
}

// ---------------------------------------------------------------------------
// Capture plan / thread check
// ---------------------------------------------------------------------------

/// Caller-supplied `agent capture` options (CLI surface, no I/O).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCaptureOptions {
    pub session: String,
    pub message: Option<String>,
    pub confidence: Option<f32>,
}

/// Validated capture plan after pure option preflight.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCapturePlan {
    pub session: String,
    pub message: Option<String>,
    pub confidence: Option<f32>,
}

/// Failures from pure agent capture option validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentCapturePlanError {
    /// `--session` was empty/whitespace.
    EmptySession,
    /// Confidence was not a finite value in `0.0..=1.0`.
    InvalidConfidence { value: String },
}

impl std::fmt::Display for AgentCapturePlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptySession => write!(f, "agent capture requires a non-empty --session"),
            Self::InvalidConfidence { value } => write!(
                f,
                "confidence must be a finite number from 0.0 to 1.0, got `{value}`"
            ),
        }
    }
}

impl std::error::Error for AgentCapturePlanError {}

/// Pure preflight for `heddle agent capture` options (no registry I/O).
pub fn plan_agent_capture(
    options: &AgentCaptureOptions,
) -> Result<AgentCapturePlan, AgentCapturePlanError> {
    let session = require_nonempty_session(&options.session)?;
    let confidence = normalize_confidence(options.confidence)?;
    Ok(AgentCapturePlan {
        session,
        message: nonempty_optional_string(options.message.clone()),
        confidence,
    })
}

/// Outcome of comparing the reservation's thread to the current checkout lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentCaptureThreadCheck {
    /// No current lane, or current lane matches the reserved thread.
    Ok,
    /// Checkout is attached to a different thread than the reservation owns.
    Mismatch {
        reserved_thread: String,
        current_thread: String,
    },
}

/// Pure thread-ownership check for session-guarded capture.
///
/// When `current_lane` is `None` (detached), capture is allowed — matching the
/// historical CLI: only an attached mismatched lane fails closed.
pub fn check_agent_capture_thread(
    reserved_thread: &str,
    current_lane: Option<&str>,
) -> AgentCaptureThreadCheck {
    match current_lane.map(str::trim).filter(|s| !s.is_empty()) {
        Some(current) if current != reserved_thread => AgentCaptureThreadCheck::Mismatch {
            reserved_thread: reserved_thread.to_string(),
            current_thread: current.to_string(),
        },
        _ => AgentCaptureThreadCheck::Ok,
    }
}

// ---------------------------------------------------------------------------
// Ready plan
// ---------------------------------------------------------------------------

/// Caller-supplied `agent ready` options (CLI surface, no I/O).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentReadyOptions {
    pub session: String,
    pub message: Option<String>,
    pub confidence: Option<f32>,
}

/// Ready plan after pure option preflight + reservation facts.
///
/// The CLI still enforces active-session I/O; this only assembles the
/// session-scoped ready payload (thread comes from the reservation entry).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentReadyPlan {
    pub session: String,
    pub thread: String,
    pub message: Option<String>,
    pub confidence: Option<f32>,
}

/// Failures from pure agent ready option validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentReadyPlanError {
    EmptySession,
    InvalidConfidence { value: String },
}

impl std::fmt::Display for AgentReadyPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptySession => write!(f, "agent ready requires a non-empty --session"),
            Self::InvalidConfidence { value } => write!(
                f,
                "confidence must be a finite number from 0.0 to 1.0, got `{value}`"
            ),
        }
    }
}

impl std::error::Error for AgentReadyPlanError {}

/// Pure preflight for `heddle agent ready` from options + resolved entry facts.
///
/// Does not check [`AgentStatus`]; call [`session_is_active`] after load.
pub fn plan_agent_ready(
    entry: &AgentEntry,
    options: &AgentReadyOptions,
) -> Result<AgentReadyPlan, AgentReadyPlanError> {
    let session = require_nonempty_session(&options.session).map_err(|err| match err {
        AgentCapturePlanError::EmptySession => AgentReadyPlanError::EmptySession,
        AgentCapturePlanError::InvalidConfidence { value } => {
            AgentReadyPlanError::InvalidConfidence { value }
        }
    })?;
    let confidence = normalize_confidence(options.confidence).map_err(|err| match err {
        AgentCapturePlanError::EmptySession => AgentReadyPlanError::EmptySession,
        AgentCapturePlanError::InvalidConfidence { value } => {
            AgentReadyPlanError::InvalidConfidence { value }
        }
    })?;
    Ok(AgentReadyPlan {
        session,
        thread: entry.thread.clone(),
        message: nonempty_optional_string(options.message.clone()),
        confidence,
    })
}

// ---------------------------------------------------------------------------
// List filter + reservation report assembly
// ---------------------------------------------------------------------------

/// Machine JSON domain fields for one reservation (stable field names).
///
/// Mirrors the public `agent list` / reservation envelope body without the
/// verification wrapper. `task` is the registry `attach_reason` (CLI contract).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AgentReservationReport {
    pub session_id: String,
    pub reservation_token: Option<String>,
    pub thread: String,
    pub anchor_state: Option<String>,
    pub anchor_root: Option<String>,
    pub task_assignment_id: Option<String>,
    /// Lifecycle status as a stable kebab-case string.
    pub status: String,
    pub path: Option<String>,
    pub task: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub harness: Option<String>,
    pub thinking_level: Option<String>,
    pub probe_source: Option<String>,
    pub probe_confidence: Option<f32>,
    pub heartbeat_at: Option<String>,
    pub lease_expires_at: Option<String>,
    pub last_progress_at: Option<String>,
    pub liveness: String,
}

impl From<&AgentEntry> for AgentReservationReport {
    fn from(entry: &AgentEntry) -> Self {
        Self {
            session_id: entry.session_id.clone(),
            reservation_token: entry.reservation_token.clone(),
            thread: entry.thread.clone(),
            anchor_state: entry.anchor_state.clone(),
            anchor_root: entry.anchor_root.clone(),
            task_assignment_id: entry.task_assignment_id.clone(),
            status: entry.status.to_string(),
            path: entry.path.as_ref().map(|path| path.display().to_string()),
            task: entry.attach_reason.clone(),
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            harness: entry.harness.clone(),
            thinking_level: entry.thinking_level.clone(),
            probe_source: entry.probe_source.clone(),
            probe_confidence: entry.probe_confidence,
            heartbeat_at: entry.heartbeat_at.map(|value| value.to_rfc3339()),
            lease_expires_at: entry.lease_expires_at().map(|value| value.to_rfc3339()),
            last_progress_at: entry.last_progress_at.map(|value| value.to_rfc3339()),
            liveness: AgentRegistry::liveness_for(entry).to_string(),
        }
    }
}

/// Assemble one reservation report from registry facts (pure).
pub fn assemble_agent_reservation(entry: &AgentEntry) -> AgentReservationReport {
    AgentReservationReport::from(entry)
}

/// Domain list payload for `agent list` (no verification envelope).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AgentReservationListReport {
    pub reservations: Vec<AgentReservationReport>,
    pub alive_only: bool,
    pub thread: Option<String>,
}

/// Pure filter for `agent list`: optional thread name + alive-only (Active).
pub fn filter_agent_reservations(
    entries: impl IntoIterator<Item = AgentEntry>,
    thread: Option<&str>,
    alive_only: bool,
) -> Vec<AgentEntry> {
    entries
        .into_iter()
        .filter(|entry| thread.is_none_or(|t| entry.thread == t))
        .filter(|entry| !alive_only || entry.status == AgentStatus::Active)
        .collect()
}

/// Pure filter over a borrowed slice.
pub fn filter_agent_reservations_ref<'a>(
    entries: impl IntoIterator<Item = &'a AgentEntry>,
    thread: Option<&str>,
    alive_only: bool,
) -> Vec<&'a AgentEntry> {
    entries
        .into_iter()
        .filter(|entry| thread.is_none_or(|t| entry.thread == t))
        .filter(|entry| !alive_only || entry.status == AgentStatus::Active)
        .collect()
}

/// Filter entries and assemble the list report domain fields.
pub fn assemble_agent_reservation_list(
    entries: impl IntoIterator<Item = AgentEntry>,
    thread: Option<String>,
    alive_only: bool,
) -> AgentReservationListReport {
    let filtered = filter_agent_reservations(entries, thread.as_deref(), alive_only);
    AgentReservationListReport {
        reservations: filtered.iter().map(assemble_agent_reservation).collect(),
        alive_only,
        thread,
    }
}

// ---------------------------------------------------------------------------
// Explain assembly from AgentEntry facts
// ---------------------------------------------------------------------------

/// Pure attach/explain report built from registry facts.
///
/// Field names align with actor-explain style presentation (`attach_reason`,
/// `winning_rule`, probe identity) without harness detection or verification.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AgentExplainReport {
    pub session_id: String,
    pub thread: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heddle_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_actor_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_parent_actor_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_instance_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_confidence: Option<f32>,
    pub attach_reason: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attach_precedence: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub winning_rule: Option<String>,
}

/// Default attach-reason when the registry entry has none persisted.
pub fn default_attach_reason_message() -> &'static str {
    "no persisted attach reason is available for this agent"
}

/// Assemble explain fields from an [`AgentEntry`] (pure, no I/O).
pub fn assemble_agent_explain(entry: &AgentEntry) -> AgentExplainReport {
    let attach_reason = entry
        .attach_reason
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| default_attach_reason_message().to_string());
    AgentExplainReport {
        session_id: entry.session_id.clone(),
        thread: entry.thread.clone(),
        status: entry.status.to_string(),
        heddle_session_id: entry.heddle_session_id.clone(),
        client_instance_id: entry.client_instance_id.clone(),
        native_actor_key: entry.native_actor_key.clone(),
        native_parent_actor_key: entry.native_parent_actor_key.clone(),
        native_instance_key: entry.native_instance_key.clone(),
        probe_source: entry.probe_source.clone(),
        probe_confidence: entry.probe_confidence,
        attach_reason,
        attach_precedence: entry.attach_precedence.clone(),
        winning_rule: entry.winning_attach_rule.clone(),
    }
}

/// Terminal status requested by `agent release`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentReleaseKind {
    Complete,
    Abandoned,
}

impl AgentReleaseKind {
    /// Map to the registry [`AgentStatus`] written on release.
    pub fn to_status(self) -> AgentStatus {
        match self {
            Self::Complete => AgentStatus::Complete,
            Self::Abandoned => AgentStatus::Abandoned,
        }
    }
}

/// Pure release transition: set status and terminal `completed_at` when needed.
///
/// Matches historical CLI: `Active` clears `completed_at`; terminal statuses
/// (`Abandoned` / `Complete` / `Merged`) stamp `completed_at`.
pub fn apply_agent_release(
    mut entry: AgentEntry,
    status: AgentStatus,
    now: DateTime<Utc>,
) -> AgentEntry {
    entry.status = status;
    entry.completed_at = match entry.status {
        AgentStatus::Active => None,
        AgentStatus::Abandoned | AgentStatus::Complete | AgentStatus::Merged => Some(now),
    };
    entry
}

/// Mutating form for use inside `registry.update_entry` closures.
pub fn touch_agent_release(entry: &mut AgentEntry, status: AgentStatus, now: DateTime<Utc>) {
    entry.status = status;
    entry.completed_at = match entry.status {
        AgentStatus::Active => None,
        AgentStatus::Abandoned | AgentStatus::Complete | AgentStatus::Merged => Some(now),
    };
}

// ---------------------------------------------------------------------------
// Shared option helpers
// ---------------------------------------------------------------------------

fn require_nonempty_session(session: &str) -> Result<String, AgentCapturePlanError> {
    let trimmed = session.trim();
    if trimmed.is_empty() {
        Err(AgentCapturePlanError::EmptySession)
    } else {
        Ok(trimmed.to_string())
    }
}

fn normalize_confidence(confidence: Option<f32>) -> Result<Option<f32>, AgentCapturePlanError> {
    match confidence {
        None => Ok(None),
        Some(value) if value.is_finite() && (0.0..=1.0).contains(&value) => Ok(Some(value)),
        Some(value) => Err(AgentCapturePlanError::InvalidConfidence {
            value: value.to_string(),
        }),
    }
}

fn nonempty_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use objects::store::{AgentEntry, AgentStatus, AgentUsageSummary};

    use super::*;

    fn sample_entry(session_id: &str, status: AgentStatus, thread: &str) -> AgentEntry {
        AgentEntry {
            session_id: session_id.to_string(),
            client_instance_id: Some("client-1".to_string()),
            native_actor_key: Some("native:1".to_string()),
            native_parent_actor_key: None,
            native_instance_key: Some("inst-1".to_string()),
            heddle_session_id: Some("hs-1".to_string()),
            thread_id: Some(thread.to_string()),
            thread: thread.to_string(),
            pid: Some(7),
            boot_id: None,
            heartbeat_at: None,
            anchor_state: Some("abcfull".to_string()),
            anchor_root: Some("rootshort".to_string()),
            reservation_token: Some("tok".to_string()),
            path: Some(std::path::PathBuf::from("/tmp/work")),
            base_state: "abc".to_string(),
            started_at: Utc::now(),
            provider: Some("anthropic".to_string()),
            model: Some("claude".to_string()),
            harness: Some("heddle-agent-api".to_string()),
            thinking_level: Some("high".to_string()),
            usage_summary: AgentUsageSummary::default(),
            last_progress_at: None,
            report_flush_state: None,
            attach_reason: Some("implement feature".to_string()),
            task_assignment_id: Some("task-1".to_string()),
            attach_precedence: vec!["agent-reserve".to_string()],
            winning_attach_rule: Some("agent-reserve".to_string()),
            probe_source: Some("agent_api".to_string()),
            probe_confidence: Some(1.0),
            status,
            completed_at: None,
            context_queries: vec![],
        }
    }

    #[test]
    fn session_gate_active_only() {
        assert!(session_is_active(&AgentStatus::Active));
        assert!(!session_is_active(&AgentStatus::Complete));
        assert_eq!(
            classify_agent_session_use(&AgentStatus::Abandoned),
            AgentSessionUse::Inactive
        );
        assert_eq!(
            classify_agent_session_use(&AgentStatus::Active),
            AgentSessionUse::Active
        );
    }

    #[test]
    fn plan_capture_rejects_empty_session_and_bad_confidence() {
        let err = plan_agent_capture(&AgentCaptureOptions {
            session: "   ".to_string(),
            message: None,
            confidence: None,
        })
        .unwrap_err();
        assert_eq!(err, AgentCapturePlanError::EmptySession);

        let err = plan_agent_capture(&AgentCaptureOptions {
            session: "agent-1".to_string(),
            message: Some("ok".to_string()),
            confidence: Some(1.5),
        })
        .unwrap_err();
        assert!(matches!(
            err,
            AgentCapturePlanError::InvalidConfidence { .. }
        ));

        let plan = plan_agent_capture(&AgentCaptureOptions {
            session: "  agent-1  ".to_string(),
            message: Some("  ship it  ".to_string()),
            confidence: Some(0.8),
        })
        .unwrap();
        assert_eq!(plan.session, "agent-1");
        assert_eq!(plan.message.as_deref(), Some("ship it"));
        assert_eq!(plan.confidence, Some(0.8));
    }

    #[test]
    fn capture_thread_check_only_mismatches_attached_lane() {
        assert_eq!(
            check_agent_capture_thread("feature/a", None),
            AgentCaptureThreadCheck::Ok
        );
        assert_eq!(
            check_agent_capture_thread("feature/a", Some("feature/a")),
            AgentCaptureThreadCheck::Ok
        );
        assert_eq!(
            check_agent_capture_thread("feature/a", Some("feature/b")),
            AgentCaptureThreadCheck::Mismatch {
                reserved_thread: "feature/a".to_string(),
                current_thread: "feature/b".to_string(),
            }
        );
    }

    #[test]
    fn plan_ready_uses_entry_thread() {
        let entry = sample_entry("agent-1", AgentStatus::Active, "feature/x");
        let plan = plan_agent_ready(
            &entry,
            &AgentReadyOptions {
                session: "agent-1".to_string(),
                message: Some("ready".to_string()),
                confidence: Some(0.9),
            },
        )
        .unwrap();
        assert_eq!(plan.thread, "feature/x");
        assert_eq!(plan.session, "agent-1");
        assert_eq!(plan.message.as_deref(), Some("ready"));
        assert_eq!(plan.confidence, Some(0.9));
    }

    #[test]
    fn filter_reservations_by_thread_and_alive() {
        let entries = vec![
            sample_entry("a1", AgentStatus::Active, "t1"),
            sample_entry("a2", AgentStatus::Complete, "t1"),
            sample_entry("a3", AgentStatus::Active, "t2"),
        ];
        let alive_t1 = filter_agent_reservations(entries.clone(), Some("t1"), true);
        assert_eq!(alive_t1.len(), 1);
        assert_eq!(alive_t1[0].session_id, "a1");

        let all_t1 = filter_agent_reservations(entries, Some("t1"), false);
        assert_eq!(all_t1.len(), 2);
    }

    #[test]
    fn reservation_report_stable_field_names() {
        let entry = sample_entry("agent-test", AgentStatus::Active, "feature/x");
        let report = assemble_agent_reservation(&entry);
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["session_id"], "agent-test");
        assert_eq!(value["thread"], "feature/x");
        assert_eq!(value["status"], "active");
        assert_eq!(value["task"], "implement feature");
        assert_eq!(value["reservation_token"], "tok");
        assert_eq!(value["task_assignment_id"], "task-1");
        assert_eq!(value["path"], "/tmp/work");
        assert_eq!(value["provider"], "anthropic");
        assert_eq!(value["probe_confidence"], 1.0);
    }

    #[test]
    fn list_report_filters_and_maps() {
        let entries = vec![
            sample_entry("a1", AgentStatus::Active, "t1"),
            sample_entry("a2", AgentStatus::Complete, "t1"),
        ];
        let report = assemble_agent_reservation_list(entries, Some("t1".to_string()), true);
        assert!(report.alive_only);
        assert_eq!(report.thread.as_deref(), Some("t1"));
        assert_eq!(report.reservations.len(), 1);
        assert_eq!(report.reservations[0].session_id, "a1");
    }

    #[test]
    fn explain_report_defaults_missing_attach_reason() {
        let mut entry = sample_entry("agent-1", AgentStatus::Active, "t1");
        entry.attach_reason = None;
        let report = assemble_agent_explain(&entry);
        assert_eq!(report.session_id, "agent-1");
        assert_eq!(report.thread, "t1");
        assert_eq!(report.attach_reason, default_attach_reason_message());
        assert_eq!(report.winning_rule.as_deref(), Some("agent-reserve"));
        assert_eq!(report.attach_precedence, vec!["agent-reserve".to_string()]);
        assert_eq!(report.native_actor_key.as_deref(), Some("native:1"));

        entry.attach_reason = Some("  do work  ".to_string());
        let report = assemble_agent_explain(&entry);
        // Non-empty reasons are kept as stored (CLI presents them verbatim).
        assert_eq!(report.attach_reason, "  do work  ");
    }

    #[test]
    fn release_transitions() {
        let entry = sample_entry("agent-1", AgentStatus::Active, "t1");
        let now = Utc::now();
        let released =
            apply_agent_release(entry.clone(), AgentReleaseKind::Complete.to_status(), now);
        assert_eq!(released.status, AgentStatus::Complete);
        assert_eq!(released.completed_at, Some(now));

        let abandoned = apply_agent_release(entry, AgentReleaseKind::Abandoned.to_status(), now);
        assert_eq!(abandoned.status, AgentStatus::Abandoned);
        assert_eq!(abandoned.completed_at, Some(now));
    }

    #[test]
    fn touch_release_mutates_in_place() {
        let mut entry = sample_entry("agent-1", AgentStatus::Active, "t1");
        let now = Utc::now();
        touch_agent_release(&mut entry, AgentStatus::Complete, now);
        assert_eq!(entry.status, AgentStatus::Complete);
        assert_eq!(entry.completed_at, Some(now));
    }
}
