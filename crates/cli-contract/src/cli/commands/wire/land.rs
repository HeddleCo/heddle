// SPDX-License-Identifier: Apache-2.0
//! Wire payloads for `heddle sync`, `heddle land`, and `land --threads`.

use schemars::JsonSchema;
use serde::Serialize;

use super::operator::OperatorCommandOutput;
use verbs::RepositoryVerificationState;

/// JSON payload for `heddle sync`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "SyncSchema")]
pub struct SyncOutput {
    #[serde(flatten)]
    pub operator: OperatorCommandOutput,
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
    pub thread: String,
    pub current_state: Option<String>,
    pub chosen_path: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(rename = "SiblingRestackFailureSchema")]
pub struct SiblingRestackFailure {
    pub thread: String,
    pub message: String,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "LandSchema")]
pub struct LandOutput {
    #[serde(flatten)]
    pub operator: OperatorCommandOutput,
    pub thread: String,
    pub captured: bool,
    pub checkpointed: bool,
    pub git_commit: Option<String>,
    pub synced: bool,
    pub integrated: bool,
    pub performed_steps: Vec<String>,
    pub skipped_steps: Vec<String>,
    pub merge_state: Option<String>,
    pub blocker_details: Vec<LandBlockerDetail>,
    #[serde(default)]
    pub siblings_restacked: Vec<String>,
    #[serde(default)]
    pub siblings_restack_failed: Vec<SiblingRestackFailure>,
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
    pub chosen_path: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(rename = "LandBlockerDetailSchema")]
pub struct LandBlockerDetail {
    pub code: LandBlockerCode,
    pub check: LandBlockerCheck,
    pub message: String,
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_context: Option<LandBlockerStateContext>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "LandBlockerCodeSchema")]
pub enum LandBlockerCode {
    ThreadStateBlocked,
    MergeConflicts,
    AutoLandConfidenceBelowThreshold,
    VerificationTestsFailed,
    ThreadStale,
    IntegrationPreviewBlocked,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "LandBlockerCheckSchema")]
pub enum LandBlockerCheck {
    ThreadState,
    MergePreview,
    AutoLandConfidence,
    VerificationSummary,
    Freshness,
    IntegrationPreview,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(rename = "LandBlockerStateContextSchema")]
pub struct LandBlockerStateContext {
    pub recorded_thread_state: String,
    pub recorded_state_id: Option<String>,
    pub thread_tip_state_id: Option<String>,
    pub integration_policy_status: Option<String>,
    pub integration_policy_reason: Option<String>,
    pub merge_relation: String,
    pub conflict_count: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(rename = "LandBatchPeerSchema")]
pub struct MultiLandPeerResult {
    pub thread: String,
    pub status: String,
    pub message: String,
    pub captured: bool,
    pub checkpointed: bool,
    pub git_commit: Option<String>,
    pub integrated: bool,
    pub synced: bool,
    #[serde(default)]
    pub siblings_restacked: Vec<String>,
    #[serde(default)]
    pub siblings_restack_failed: Vec<SiblingRestackFailure>,
    #[serde(default)]
    pub blockers: Vec<String>,
    pub blocker_details: Vec<LandBlockerDetail>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_command: Option<String>,
    #[serde(default)]
    pub recovery_commands: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
#[schemars(rename = "LandBatchSchema")]
pub struct MultiLandOutput {
    pub output_kind: &'static str,
    pub status: String,
    pub action: &'static str,
    pub message: String,
    pub threads: Vec<String>,
    pub landed: Vec<String>,
    pub stopped_at: Option<String>,
    pub peers: Vec<MultiLandPeerResult>,
    pub git_head: Option<String>,
    pub recommended_action: Option<String>,
    #[serde(rename = "verification")]
    pub trust: Option<RepositoryVerificationState>,
}
