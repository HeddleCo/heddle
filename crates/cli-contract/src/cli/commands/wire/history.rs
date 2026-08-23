// SPDX-License-Identifier: Apache-2.0
//! Real wire payloads for the history-reading verbs (`show`, `log`,
//! reflog/timeline, `thread expand`, markers, blame, revert).

use repo::Repository;
use repo::{TimelineNavigationSnapshot, TimelineNavigationStep};
use schemars::JsonSchema;
use serde::Serialize;

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ThreadMarkerListSchema")]
pub struct MarkerListOutput {
    pub output_kind: &'static str,
    pub markers: Vec<MarkerEntry>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ThreadMarkerEntrySchema")]
pub struct MarkerEntry {
    pub name: String,
    /// Short change-id of the state the marker points at.
    pub state_id: String,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ThreadMarkerOpSchema")]
pub struct MarkerOpOutput {
    pub output_kind: &'static str,
    pub name: String,
    /// Short change-id of the state the marker pointed at after the op.
    /// `None` for ops that delete the marker.
    pub state_id: Option<String>,
    pub message: String,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "MarkerBulkDeleteSchema")]
pub struct MarkerBulkDeleteOutput {
    pub output_kind: &'static str,
    pub deleted: Vec<MarkerEntry>,
    pub count: usize,
    pub message: String,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ExpandSchema")]
pub struct ExpandOutput {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub requested: String,
    pub collapsed: CollapsedLandOutput,
    pub captures: Vec<ExpandedCaptureOutput>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ExpandedCollapseSchema")]
pub struct CollapsedLandOutput {
    pub state_id: String,
    pub state_id_full: String,
    pub git_commit: Option<String>,
    pub thread: Option<String>,
    pub source_count: usize,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ExpandedCaptureSchema")]
pub struct ExpandedCaptureOutput {
    pub state_id: String,
    pub state_id_full: String,
    pub content_hash: String,
    pub intent: Option<String>,
    pub principal: String,
    pub agent: Option<String>,
    pub confidence: Option<f32>,
    pub created_at: String,
    pub parents: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "RevertSchema")]
pub struct RevertOutput {
    pub output_kind: &'static str,
    pub state_id: Option<String>,
    pub reverted_state: String,
    pub files_affected: Vec<String>,
    pub message: String,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "BlameSchema")]
pub struct BlameOutput {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub file: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<ContextSnippet>,
    pub lines: Vec<BlameLine>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "BlameLineSchema")]
pub struct BlameLine {
    pub line_number: usize,
    pub content: String,
    pub state_id: String,
    pub principal: PrincipalInfo,
    pub agent: Option<AgentInfo>,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origins: Option<Vec<BlameOrigin>>,
}

#[derive(Clone, Serialize, JsonSchema)]
#[schemars(rename = "BlameOriginSchema")]
pub struct BlameOrigin {
    pub state_id: String,
    pub principal: PrincipalInfo,
    pub agent: Option<AgentInfo>,
    pub timestamp: String,
}

#[derive(Clone, Serialize, JsonSchema)]
#[schemars(rename = "BlamePrincipalSchema")]
pub struct PrincipalInfo {
    pub name: String,
    pub email: String,
}

#[derive(Clone, Serialize, JsonSchema)]
#[schemars(rename = "BlameAgentSchema")]
pub struct AgentInfo {
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
}

#[derive(Clone, Serialize, JsonSchema)]
#[schemars(rename = "BlameContextSnippetSchema")]
pub struct ContextSnippet {
    pub annotation_id: String,
    pub kind: String,
    pub content: String,
    pub revision_count: usize,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ShowSchema")]
pub struct ShowOutput {
    pub output_kind: &'static str,
    pub repository_capability: String,
    pub storage_model: String,
    pub state_id: String,
    pub state_id_full: String,
    pub content_hash: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub principal: ShowPrincipalInfo,
    pub agent: Option<ShowAgentInfo>,
    pub created_at: String,
    pub status: String,
    pub verification: Option<ShowVerificationInfo>,
    pub git_checkpoint: Option<String>,
    /// Carried for the human-readable renderer only. Not part of the
    /// JSON contract.
    #[serde(skip)]
    #[schemars(skip)]
    pub import_guidance: Option<ShowImportGuidanceOutput>,
}

#[derive(Serialize, JsonSchema)]
pub struct ShowImportGuidanceOutput {
    pub current_branch: String,
    pub missing_branch_count: usize,
    pub missing_branches: Vec<String>,
    pub recommended_command: String,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ShowPrincipalSchema")]
pub struct ShowPrincipalInfo {
    pub name: String,
    pub email: String,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ShowAgentSchema")]
pub struct ShowAgentInfo {
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ShowVerificationInfo")]
pub struct ShowVerificationInfo {
    pub tests_passed: Option<bool>,
    pub tests_failed: Option<u32>,
    pub coverage_pct: Option<f32>,
    pub coverage_delta: Option<f32>,
    pub lint_warnings: Option<u32>,
}

impl From<objects::object::State> for ExpandedCaptureOutput {
    fn from(state: objects::object::State) -> Self {
        Self {
            state_id: state.state_id.short(),
            state_id_full: state.state_id.to_string_full(),
            content_hash: state.compute_hash().short(),
            intent: state.intent,
            principal: state.attribution.principal.to_string(),
            agent: state
                .attribution
                .agent
                .as_ref()
                .map(objects::object::Agent::to_string),
            confidence: state.confidence,
            created_at: state.created_at.format("%Y-%m-%d %H:%M:%S").to_string(),
            parents: state
                .parents
                .iter()
                .map(objects::object::StateId::short)
                .collect(),
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "LogSchema")]
pub struct LogOutput {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub repository_capability: String,
    pub storage_model: String,
    pub states: Vec<StateEntry>,
    /// Carried for the human-readable renderer only. Not part of the
    /// JSON contract: import-hint information is exposed via
    /// `heddle status --output json` instead.
    #[serde(skip)]
    #[schemars(skip)]
    pub import_guidance: Option<LogImportGuidanceOutput>,
    /// Init seed id for the text renderer only. Not a JSON key: the
    /// walk still omits genesis from `states` and names the id in text.
    #[serde(skip)]
    #[schemars(skip)]
    pub omitted_genesis: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct LogImportGuidanceOutput {
    pub current_branch: String,
    pub missing_branch_count: usize,
    pub missing_branches: Vec<String>,
    pub recommended_command: String,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "StateEntrySchema")]
pub struct StateEntry {
    pub state_id: String,
    pub content_hash: String,
    pub intent: Option<String>,
    pub principal: String,
    /// Raw principal name + email so we can render a styled
    /// `name <email>` pair (bold/dim) without re-parsing the
    /// pre-formatted `principal` string. Skipped from JSON
    /// serialization to keep the wire format unchanged — only the
    /// human-readable renderer reads them.
    #[serde(skip)]
    #[schemars(skip)]
    pub principal_name: String,
    #[serde(skip)]
    #[schemars(skip)]
    pub principal_email: String,
    pub agent: Option<String>,
    pub confidence: Option<f32>,
    pub created_at: String,
    pub parents: Vec<String>,
    pub git_checkpoint: Option<String>,
    pub collapsed: Option<CollapsedEntry>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "CollapsedEntrySchema")]
pub struct CollapsedEntry {
    pub expandable: bool,
    pub source_count: usize,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "LogReflogSchema")]
pub struct ReflogOutput {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub repository_capability: String,
    pub storage_model: String,
    pub entries: Vec<ReflogEntry>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[schemars(rename = "ReflogEntrySchema")]
pub struct ReflogEntry {
    pub source: String,
    pub reference: String,
    pub old_oid: String,
    pub new_oid: String,
    pub actor: String,
    pub timestamp: Option<String>,
    pub message: String,
}

impl From<verbs::ReflogLine> for ReflogEntry {
    fn from(line: verbs::ReflogLine) -> Self {
        Self {
            source: line.source,
            reference: line.reference,
            old_oid: line.old_oid,
            new_oid: line.new_oid,
            actor: line.actor,
            timestamp: line.timestamp,
            message: line.message,
        }
    }
}

impl From<&objects::object::State> for StateEntry {
    fn from(state: &objects::object::State) -> Self {
        Self {
            state_id: state.state_id.short(),
            content_hash: state.compute_hash().short(),
            intent: state.intent.clone(),
            principal: state.attribution.principal.to_string(),
            principal_name: state.attribution.principal.name_lossy().into_owned(),
            principal_email: state.attribution.principal.email_lossy().into_owned(),
            agent: state
                .attribution
                .agent
                .as_ref()
                .map(objects::object::Agent::to_string),
            confidence: state.confidence,
            created_at: state.created_at.format("%Y-%m-%d %H:%M:%S").to_string(),
            parents: state
                .parents
                .iter()
                .map(objects::object::StateId::short)
                .collect(),
            git_checkpoint: None,
            collapsed: None,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "TimelineStatusSchema")]
pub struct TimelineStatusOutput {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub thread: String,
    pub cursor_branch_id: Option<String>,
    pub cursor_step_id: Option<String>,
    pub cursor_state: Option<String>,
    pub current_step: Option<TimelineStatusStepOutput>,
    pub active_branch_path: Vec<String>,
    pub can_undo: bool,
    pub can_redo: bool,
    pub branch_count: usize,
    pub step_count: usize,
    pub recovery: Option<TimelineStatusRecoveryOutput>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "TimelineStatusStepSchema")]
pub struct TimelineStatusStepOutput {
    pub step_id: String,
    pub branch_id: String,
    pub parent_step_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_status: Option<&'static str>,
    pub changed: Option<bool>,
    pub payload_summary: Option<String>,
    pub payload_hash: Option<String>,
    pub labels: Vec<&'static str>,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub can_seek: bool,
    pub can_fork: bool,
    pub can_reset: bool,
    pub can_materialize: bool,
    pub has_boundary_warning: bool,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "TimelineStatusRecoverySchema")]
pub struct TimelineStatusRecoveryOutput {
    pub status: &'static str,
    pub branch_id: String,
    pub from_step_id: Option<String>,
    pub to_step_id: Option<String>,
    pub from_state: String,
    pub to_state: String,
    pub reason: String,
    pub moved_at_ms: i64,
    pub checkout_state: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "TimelineRecordingSchema")]
pub struct TimelineRecordingOutput {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub action: &'static str,
    pub thread: String,
    pub step_id: String,
    pub branch_id: String,
    pub parent_step_id: Option<String>,
    pub operation_id: String,
    pub before_state: Option<String>,
    pub after_state: Option<String>,
    pub changed: Option<bool>,
    pub tool_status: Option<&'static str>,
    pub payload_summary: Option<String>,
    pub payload_hash: Option<String>,
    pub branch_count: usize,
    pub step_count: usize,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "TimelineActionSchema")]
pub struct TimelineActionOutput {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub action: &'static str,
    pub thread: String,
    pub branch_id: Option<String>,
    pub parent_branch_id: Option<String>,
    pub from_step_id: Option<String>,
    pub cursor_branch_id: Option<String>,
    pub cursor_step_id: Option<String>,
    pub operation_id: Option<String>,
    pub recovered_operation_id: Option<String>,
    pub materialized: Option<bool>,
    pub materialization_status: Option<String>,
    pub recovery_status: Option<String>,
    pub blocker_count: usize,
    pub branch_count: usize,
    pub step_count: usize,
}

impl TimelineLogOutput {
    pub fn from_snapshot(repo: &Repository, snapshot: TimelineNavigationSnapshot) -> Self {
        Self {
            output_kind: "timeline_log",
            status: "completed",
            repository_capability: repo.capability_label().to_string(),
            storage_model: repo.storage_model_label().to_string(),
            thread: snapshot.thread,
            cursor: TimelineCursorOutput {
                branch_id: snapshot.cursor.branch_id.map(|id| id.to_string()),
                step_id: snapshot.cursor.step_id.map(|id| id.to_string()),
                state: snapshot.cursor.state.map(|state| state.short()),
                state_full: snapshot.cursor.state.map(|state| state.to_string_full()),
            },
            branches: snapshot
                .branches
                .into_iter()
                .map(|branch| TimelineBranchOutput {
                    branch_id: branch.branch_id.to_string(),
                    parent_branch_id: branch.parent_branch_id.map(|id| id.to_string()),
                    forked_from_step_id: branch.forked_from_step_id.map(|id| id.to_string()),
                    forked_from_state: branch.forked_from_state.map(|state| state.short()),
                    reason: branch
                        .reason
                        .as_ref()
                        .map(|r| verbs::timeline_branch_reason(r).to_string()),
                    created_at_ms: branch.created_at_ms,
                    step_ids: branch.step_ids.iter().map(ToString::to_string).collect(),
                    is_active: branch.is_active,
                    is_on_active_path: branch.is_on_active_path,
                })
                .collect(),
            steps: snapshot
                .steps
                .into_iter()
                .map(TimelineStepOutput::from_step)
                .collect(),
            active_branch_path: snapshot
                .active_branch_path
                .iter()
                .map(ToString::to_string)
                .collect(),
            actions: TimelineActionsOutput {
                can_undo: snapshot.actions.can_undo,
                can_redo: snapshot.actions.can_redo,
            },
            recovery: snapshot.recovery.map(|recovery| TimelineRecoveryOutput {
                status: verbs::timeline_recovery_status(recovery.status).to_string(),
                branch_id: recovery.branch_id.to_string(),
                from_step_id: recovery.from_step_id.map(|id| id.to_string()),
                to_step_id: recovery.to_step_id.map(|id| id.to_string()),
                from_state: recovery.from_state.short(),
                to_state: recovery.to_state.short(),
                reason: verbs::timeline_cursor_reason(&recovery.reason).to_string(),
                moved_at_ms: recovery.moved_at_ms,
                checkout_state: recovery.checkout_state.map(|state| state.short()),
            }),
        }
    }
}

impl TimelineStepOutput {
    pub fn from_step(step: TimelineNavigationStep) -> Self {
        Self {
            step_id: step.step_id.to_string(),
            branch_id: step.branch_id.to_string(),
            parent_step_id: step.parent_step_id.map(|id| id.to_string()),
            native: step.native.map(|native| TimelineNativeOutput {
                harness: native.harness,
                session_id: native.session_id,
                message_id: native.message_id,
                tool_call_id: native.tool_call_id,
            }),
            tool_name: step.tool_name,
            status: step
                .status
                .as_ref()
                .map(|st| verbs::timeline_tool_status(st).to_string()),
            changed: step.changed,
            touched_paths: step.touched_paths,
            labels: step
                .labels
                .iter()
                .map(|l| verbs::timeline_label(l).to_string())
                .collect(),
            before_state: step.before_state.map(|state| state.short()),
            after_state: step.after_state.map(|state| state.short()),
            capture_state: step.capture_state.map(|state| state.short()),
            cursor_state: step.cursor_state.map(|state| state.short()),
            cursor_state_full: step.cursor_state.map(|state| state.to_string_full()),
            payload_summary: step.payload_summary,
            payload_hash: step.payload_hash.map(|hash| hash.short()),
            capture_oplog_batch_id: step.capture_oplog_batch_id,
            started_at_ms: step.started_at_ms,
            finished_at_ms: step.finished_at_ms,
            operation_ids: step
                .operation_ids
                .iter()
                .map(|id| id.to_string_full())
                .collect(),
            is_current: step.is_current,
            is_on_active_branch_path: step.is_on_active_branch_path,
            can_seek: step.can_seek,
            can_fork: step.can_fork,
            can_reset: step.can_reset,
            can_materialize: step.can_materialize,
            has_boundary_warning: step.has_boundary_warning,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "TimelineLogSchema")]
pub struct TimelineLogOutput {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub repository_capability: String,
    pub storage_model: String,
    pub thread: String,
    pub cursor: TimelineCursorOutput,
    pub branches: Vec<TimelineBranchOutput>,
    pub steps: Vec<TimelineStepOutput>,
    pub active_branch_path: Vec<String>,
    pub actions: TimelineActionsOutput,
    pub recovery: Option<TimelineRecoveryOutput>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "TimelineCursorSchema")]
pub struct TimelineCursorOutput {
    pub branch_id: Option<String>,
    pub step_id: Option<String>,
    pub state: Option<String>,
    pub state_full: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "TimelineBranchSchema")]
pub struct TimelineBranchOutput {
    pub branch_id: String,
    pub parent_branch_id: Option<String>,
    pub forked_from_step_id: Option<String>,
    pub forked_from_state: Option<String>,
    pub reason: Option<String>,
    pub created_at_ms: Option<i64>,
    pub step_ids: Vec<String>,
    pub is_active: bool,
    pub is_on_active_path: bool,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "TimelineStepSchema")]
pub struct TimelineStepOutput {
    pub step_id: String,
    pub branch_id: String,
    pub parent_step_id: Option<String>,
    pub native: Option<TimelineNativeOutput>,
    pub tool_name: Option<String>,
    pub status: Option<String>,
    pub changed: Option<bool>,
    pub touched_paths: Vec<String>,
    pub labels: Vec<String>,
    pub before_state: Option<String>,
    pub after_state: Option<String>,
    pub capture_state: Option<String>,
    pub cursor_state: Option<String>,
    pub cursor_state_full: Option<String>,
    pub payload_summary: Option<String>,
    pub payload_hash: Option<String>,
    pub capture_oplog_batch_id: Option<u64>,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub operation_ids: Vec<String>,
    pub is_current: bool,
    pub is_on_active_branch_path: bool,
    pub can_seek: bool,
    pub can_fork: bool,
    pub can_reset: bool,
    pub can_materialize: bool,
    pub has_boundary_warning: bool,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "TimelineNativeSchema")]
pub struct TimelineNativeOutput {
    pub harness: String,
    pub session_id: Option<String>,
    pub message_id: Option<String>,
    pub tool_call_id: String,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "TimelineActionsSchema")]
pub struct TimelineActionsOutput {
    pub can_undo: bool,
    pub can_redo: bool,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "TimelineRecoverySchema")]
pub struct TimelineRecoveryOutput {
    pub status: String,
    pub branch_id: String,
    pub from_step_id: Option<String>,
    pub to_step_id: Option<String>,
    pub from_state: String,
    pub to_state: String,
    pub reason: String,
    pub moved_at_ms: i64,
    pub checkout_state: Option<String>,
}
