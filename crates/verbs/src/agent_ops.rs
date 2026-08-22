// SPDX-License-Identifier: Apache-2.0
//! Pure agent reservation API helpers (non-fanout verbs).
//!
//! Owns:
//! - `agent capture` option/plan validation and thread-ownership checks
//! - `agent ready` plan assembly from a resolved active reservation
//! - `agent list` filter + writer lease report assembly
//! - attach/explain field assembly from registry facts
//! - pure heartbeat / release status transitions
//!
//! Registry I/O, recovery advice, harness probing, and human/JSON render stay
//! CLI-owned.

use objects::store::{ActorPresence, WriterLease, WriterLeaseStatus};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Capture plan / thread check
// ---------------------------------------------------------------------------

/// Caller-supplied `agent capture` options (CLI surface, no I/O).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCaptureOptions {
    pub lease: String,
    pub message: Option<String>,
    pub confidence: Option<f32>,
}

/// Validated capture plan after pure option preflight.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCapturePlan {
    pub lease: String,
    pub message: Option<String>,
    pub confidence: Option<f32>,
}

/// Failures from pure agent capture option validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentCapturePlanError {
    EmptyLease,
    /// Confidence was not a finite value in `0.0..=1.0`.
    InvalidConfidence {
        value: String,
    },
}

impl std::fmt::Display for AgentCapturePlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyLease => write!(f, "agent capture requires a non-empty --lease"),
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
    let lease = require_nonempty_lease(&options.lease)?;
    let confidence = normalize_confidence(options.confidence)?;
    Ok(AgentCapturePlan {
        lease,
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
    pub lease: String,
    pub message: Option<String>,
    pub confidence: Option<f32>,
}

/// Ready plan after pure option preflight + reservation facts.
///
/// The CLI still enforces active-session I/O; this only assembles the
/// session-scoped ready payload (thread comes from the reservation entry).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentReadyPlan {
    pub lease: String,
    pub thread: String,
    pub message: Option<String>,
    pub confidence: Option<f32>,
}

/// Failures from pure agent ready option validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentReadyPlanError {
    EmptyLease,
    InvalidConfidence { value: String },
}

impl std::fmt::Display for AgentReadyPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyLease => write!(f, "agent ready requires a non-empty --lease"),
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
pub fn plan_agent_ready(
    lease: &WriterLease,
    options: &AgentReadyOptions,
) -> Result<AgentReadyPlan, AgentReadyPlanError> {
    let lease_id = require_nonempty_lease(&options.lease).map_err(|err| match err {
        AgentCapturePlanError::EmptyLease => AgentReadyPlanError::EmptyLease,
        AgentCapturePlanError::InvalidConfidence { value } => {
            AgentReadyPlanError::InvalidConfidence { value }
        }
    })?;
    let confidence = normalize_confidence(options.confidence).map_err(|err| match err {
        AgentCapturePlanError::EmptyLease => AgentReadyPlanError::EmptyLease,
        AgentCapturePlanError::InvalidConfidence { value } => {
            AgentReadyPlanError::InvalidConfidence { value }
        }
    })?;
    Ok(AgentReadyPlan {
        lease: lease_id,
        thread: lease.thread.clone(),
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
    pub lease_id: String,
    pub actor_session_id: Option<String>,
    pub thread: String,
    pub anchor_state: Option<String>,
    pub anchor_root: Option<String>,
    pub task_assignment_id: Option<String>,
    pub status: String,
    pub path: Option<String>,
    pub heartbeat_at: String,
    pub lease_expires_at: String,
    pub liveness: String,
}

impl From<&WriterLease> for AgentReservationReport {
    fn from(lease: &WriterLease) -> Self {
        Self {
            lease_id: lease.lease_id.clone(),
            actor_session_id: lease.actor_session_id.clone(),
            thread: lease.thread.clone(),
            anchor_state: lease.anchor_state.clone(),
            anchor_root: lease.anchor_root.clone(),
            task_assignment_id: lease.task_assignment_id.clone(),
            status: lease.status.to_string(),
            path: lease.path.as_ref().map(|path| path.display().to_string()),
            heartbeat_at: lease.heartbeat_at.to_rfc3339(),
            lease_expires_at: lease.lease_expires_at().to_rfc3339(),
            liveness: lease.liveness_at(chrono::Utc::now()).to_string(),
        }
    }
}

/// Assemble one reservation report from registry facts (pure).
pub fn assemble_agent_reservation(lease: &WriterLease) -> AgentReservationReport {
    AgentReservationReport::from(lease)
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
    entries: impl IntoIterator<Item = WriterLease>,
    thread: Option<&str>,
    alive_only: bool,
) -> Vec<WriterLease> {
    entries
        .into_iter()
        .filter(|lease| thread.is_none_or(|thread| lease.thread == thread))
        .filter(|lease| !alive_only || lease.status == WriterLeaseStatus::Active)
        .collect()
}

/// Pure filter over a borrowed slice.
pub fn filter_agent_reservations_ref<'a>(
    entries: impl IntoIterator<Item = &'a WriterLease>,
    thread: Option<&str>,
    alive_only: bool,
) -> Vec<&'a WriterLease> {
    entries
        .into_iter()
        .filter(|lease| thread.is_none_or(|thread| lease.thread == thread))
        .filter(|lease| !alive_only || lease.status == WriterLeaseStatus::Active)
        .collect()
}

/// Filter entries and assemble the list report domain fields.
pub fn assemble_agent_reservation_list(
    entries: impl IntoIterator<Item = WriterLease>,
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
// Explain assembly from ActorPresence facts
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

/// Assemble explain fields from an [`ActorPresence`] (pure, no I/O).
pub fn assemble_agent_explain(entry: &ActorPresence) -> AgentExplainReport {
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

// ---------------------------------------------------------------------------
// Shared option helpers
// ---------------------------------------------------------------------------

fn require_nonempty_lease(lease: &str) -> Result<String, AgentCapturePlanError> {
    let trimmed = lease.trim();
    if trimmed.is_empty() {
        Err(AgentCapturePlanError::EmptyLease)
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
    use objects::store::WriterLeaseStatus;

    use super::*;

    fn lease() -> WriterLease {
        let now = Utc::now();
        WriterLease {
            lease_id: "lease-one".to_string(),
            thread: "feature/a".to_string(),
            actor_session_id: Some("agent-one".to_string()),
            task_assignment_id: None,
            anchor_state: Some("hd-state".to_string()),
            anchor_root: Some("root".to_string()),
            path: None,
            token_hash: "hash".to_string(),
            pid: None,
            boot_id: None,
            heartbeat_at: now,
            started_at: now,
            status: WriterLeaseStatus::Active,
            completed_at: None,
        }
    }

    #[test]
    fn capture_plan_requires_a_lease_id() {
        let error = plan_agent_capture(&AgentCaptureOptions {
            lease: " ".to_string(),
            message: None,
            confidence: None,
        })
        .unwrap_err();
        assert_eq!(error, AgentCapturePlanError::EmptyLease);
    }

    #[test]
    fn ready_plan_uses_the_leased_thread() {
        let plan = plan_agent_ready(
            &lease(),
            &AgentReadyOptions {
                lease: "lease-one".to_string(),
                message: Some("ready".to_string()),
                confidence: Some(0.9),
            },
        )
        .unwrap();
        assert_eq!(plan.thread, "feature/a");
        assert_eq!(plan.lease, "lease-one");
    }

    #[test]
    fn reservation_report_never_contains_token_material() {
        let value = serde_json::to_value(assemble_agent_reservation(&lease())).unwrap();
        assert!(value.get("token").is_none());
        assert!(value.get("token_hash").is_none());
        assert_eq!(value["lease_id"], "lease-one");
    }
}
