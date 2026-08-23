// SPDX-License-Identifier: Apache-2.0
//! Wire payloads for `agent presence`, reservations, tasks, fan-out, and
//! provenance sessions.

use schemars::JsonSchema;
use serde::Serialize;
use verbs::{ActionTemplate, ActorEntryReport, RepositoryVerificationState};

// ---- agent presence --------------------------------------------------------

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AgentPresenceSingleSchema")]
pub struct ActorSingleOutput {
    pub output_kind: &'static str,
    pub presence: ActorEntryReport,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AgentPresenceListSchema")]
pub struct ActorListOutput {
    pub output_kind: &'static str,
    pub presence: Vec<ActorEntryReport>,
    pub active_only: bool,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AgentPresenceCompleteSchema")]
pub struct ActorDoneOutput {
    pub output_kind: &'static str,
    pub session_id: String,
    pub status: &'static str,
    pub thread: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordination_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended_action_template: Option<ActionTemplate>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

/// `agent presence explain` — attach/detach diagnosis beside verification.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AgentPresenceExplainSchema")]
pub struct ActorExplainDetectedOutput {
    pub output_kind: &'static str,
    pub attached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_presence: Option<serde_json::Value>,
    pub reason: &'static str,
    pub repository: String,
    pub detected: DetectedActorOutput,
    pub environment: ActorEnvironmentOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended_action_template: Option<ActionTemplate>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[derive(Serialize, JsonSchema)]
pub struct DetectedActorOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
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
}

#[derive(Serialize, JsonSchema)]
pub struct ActorEnvironmentOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal_email: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub signals: Vec<String>,
}

// ---- agent reservations ----------------------------------------------------

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AgentReservationEnvelopeSchema")]
pub struct AgentReservationEnvelope {
    pub reservation: AgentReservationOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AgentReservationListSchema")]
pub struct AgentReservationListOutput {
    pub reservations: Vec<AgentReservationOutput>,
    pub alive_only: bool,
    pub thread: Option<String>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AgentReservationSchema")]
pub struct AgentReservationOutput {
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

// ---- agent tasks & fan-out -------------------------------------------------

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AgentTaskEnvelopeSchema")]
pub struct AgentTaskEnvelope {
    pub output_kind: &'static str,
    pub task: AgentTaskOutput,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AgentTaskListSchema")]
pub struct AgentTaskListOutput {
    pub output_kind: &'static str,
    pub tasks: Vec<AgentTaskOutput>,
    pub thread: Option<String>,
    pub status: Option<String>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AgentTaskSchema")]
pub struct AgentTaskOutput {
    pub schema_version: u32,
    pub task_id: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub target_thread: String,
    pub base_state: Option<String>,
    pub base_root: Option<String>,
    pub parent_task_id: Option<String>,
    pub coordination_discussion_id: Option<String>,
    pub allow_offline: bool,
    pub delegated_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AgentFanoutSchema")]
pub struct AgentFanoutOutput {
    pub output_kind: &'static str,
    pub title: String,
    pub parent_thread: String,
    pub base_state: String,
    pub base_root: String,
    pub coordination_discussion_id: Option<String>,
    pub parent_task: Option<AgentTaskOutput>,
    pub lanes: Vec<AgentFanoutLaneOutput>,
    pub commands: Vec<AgentFanoutCommandOutput>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[derive(Serialize, JsonSchema)]
pub struct AgentFanoutLaneOutput {
    pub thread: String,
    pub path: String,
    pub title: String,
    pub task: Option<AgentTaskOutput>,
    pub session_id: Option<String>,
    pub lease_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub status: String,
}

#[derive(Serialize, JsonSchema)]
pub struct AgentFanoutCommandOutput {
    pub lane_thread: String,
    pub command: String,
    pub argv: Vec<String>,
}

// ---- agent provenance ------------------------------------------------------

/// `agent provenance begin` / `end` / `show` envelope.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AgentProvenanceEnvelopeSchema")]
pub struct SessionEnvelope {
    pub session: SessionOutput,
    #[serde(skip)]
    #[schemars(skip)]
    pub trust: RepositoryVerificationState,
}

/// `agent provenance segment` envelope.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AgentProvenanceSegmentEnvelopeSchema")]
pub struct SegmentEnvelope {
    pub segment: SegmentOutput,
    #[serde(skip)]
    #[schemars(skip)]
    pub trust: RepositoryVerificationState,
}

/// `agent provenance list`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AgentProvenanceListSchema")]
pub struct SessionListOutput {
    pub sessions: Vec<SessionOutput>,
    pub active_only: bool,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "SessionEntrySchema")]
pub struct SessionOutput {
    pub id: String,
    pub principal: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    pub active: bool,
    pub segments: Vec<SegmentOutput>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "SessionSegmentSchema")]
pub struct SegmentOutput {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
}

impl From<&objects::store::WriterLease> for AgentReservationOutput {
    fn from(lease: &objects::store::WriterLease) -> Self {
        AgentReservationOutput::from(verbs::assemble_agent_reservation(lease))
    }
}

impl From<verbs::AgentReservationReport> for AgentReservationOutput {
    fn from(report: verbs::AgentReservationReport) -> Self {
        Self {
            lease_id: report.lease_id,
            actor_session_id: report.actor_session_id,
            thread: report.thread,
            anchor_state: report.anchor_state,
            anchor_root: report.anchor_root,
            task_assignment_id: report.task_assignment_id,
            status: report.status,
            path: report.path,
            heartbeat_at: report.heartbeat_at,
            lease_expires_at: report.lease_expires_at,
            liveness: report.liveness,
        }
    }
}

impl From<&repo::AgentTaskRecord> for AgentTaskOutput {
    fn from(task: &repo::AgentTaskRecord) -> Self {
        Self {
            schema_version: task.schema_version,
            task_id: task.task_id.clone(),
            title: task.title.clone(),
            body: task.body.clone(),
            status: task.status.to_string(),
            target_thread: task.target_thread.clone(),
            base_state: task.base_state.clone(),
            base_root: task.base_root.clone(),
            parent_task_id: task.parent_task_id.clone(),
            coordination_discussion_id: task.coordination_discussion_id.clone(),
            allow_offline: task.allow_offline,
            delegated_by: task.delegated_by.clone(),
            created_at: task.created_at.to_rfc3339(),
            updated_at: task.updated_at.to_rfc3339(),
            completed_at: task.completed_at.map(|time| time.to_rfc3339()),
        }
    }
}

impl From<&objects::object::Session> for SessionOutput {
    fn from(session: &objects::object::Session) -> Self {
        Self {
            id: session.id.clone(),
            principal: session.principal.to_string(),
            created_at: session.created_at.to_rfc3339(),
            ended_at: session.ended_at.map(|t| t.to_rfc3339()),
            active: session.is_active(),
            segments: session.segments.iter().map(SegmentOutput::from).collect(),
        }
    }
}

impl From<&objects::object::SessionSegment> for SegmentOutput {
    fn from(segment: &objects::object::SessionSegment) -> Self {
        Self {
            id: segment.id.clone(),
            provider: segment.provider.clone(),
            model: segment.model.clone(),
            started_at: segment.started_at.to_rfc3339(),
            policy_id: segment.policy_id.clone(),
        }
    }
}
