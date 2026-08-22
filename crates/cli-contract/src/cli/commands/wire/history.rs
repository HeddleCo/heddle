// SPDX-License-Identifier: Apache-2.0
//! Real wire payloads for the history-reading verbs (`show`, `log`,
//! reflog/timeline, `thread expand`, markers, blame, revert).

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
            agent: state.attribution.agent.as_ref().map(objects::object::Agent::to_string),
            confidence: state.confidence,
            created_at: state.created_at.format("%Y-%m-%d %H:%M:%S").to_string(),
            parents: state.parents.iter().map(objects::object::StateId::short).collect(),
        }
    }
}
