// SPDX-License-Identifier: Apache-2.0
//! Real `--output json` wire payloads for the CLI verbs.
//!
//! These are the serialization structs the commands emit, not schema
//! mirrors: `Serialize` and `JsonSchema` derive from the same definition,
//! so a skip-serialized field cannot reappear on the published schema.
//! `crates/cli` re-exports each payload at its historical path, and
//! [`super::schemas`] registers these types directly (InitOutput precedent).
//!
//! The `#[schemars(rename)]` attributes keep the published `$defs` titles
//! stable while the Rust types carry their natural names.

use schemars::JsonSchema;
use serde::Serialize;

use objects::object::{Agent, Principal};
use verbs::{ActionTemplate, RepositoryVerificationState, UndoBatchSummary};

// ---- capture ---------------------------------------------------------------

/// JSON payload for `heddle capture` / `heddle agent capture`.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(rename = "CaptureSchema")]
pub struct SnapshotOutput {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub action: &'static str,
    pub state_id: String,
    pub content_hash: String,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub task_assignment_id: Option<String>,
    pub principal: SnapshotPrincipalOutput,
    pub principal_source: String,
    pub agent: Option<SnapshotAgentOutput>,
    pub promotion_suggested: bool,
    pub heavy_impact_paths: Vec<String>,
    pub captured_path_count: usize,
    pub warnings: Vec<String>,
    /// Whether this state carries an ed25519 author signature (heddle#482).
    /// `false` means signing degraded (no key, or an unreadable key); the
    /// state is still captured, just unsigned — surfaced here so a degraded
    /// signing path is never silent.
    pub signed: bool,
    pub message: String,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplate>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplate>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(rename = "CommitPrincipalSchema")]
pub struct SnapshotPrincipalOutput {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(rename = "CommitAgentSchema")]
pub struct SnapshotAgentOutput {
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segment_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

impl From<&Principal> for SnapshotPrincipalOutput {
    fn from(principal: &Principal) -> Self {
        Self {
            name: principal.name_lossy().into_owned(),
            email: principal.email_lossy().into_owned(),
        }
    }
}

impl From<&Agent> for SnapshotAgentOutput {
    fn from(agent: &Agent) -> Self {
        Self {
            provider: agent.provider.clone(),
            model: agent.model.clone(),
            session_id: agent.session_id.clone(),
            segment_id: agent.segment_id.clone(),
            policy_id: agent.policy_id.clone(),
            thought_level: agent.thought_level.clone(),
            parent: agent.parent.clone(),
        }
    }
}

// ---- commit ----------------------------------------------------------------

/// JSON payload for `heddle commit`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "CommitSchema")]
pub struct CommitOutput {
    pub output_kind: &'static str,
    pub action: &'static str,
    pub status: &'static str,
    pub state_id: String,
    pub git_commit: String,
    pub summary: String,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplate>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

// ---- undo / redo / recover -------------------------------------------------

/// JSON payload for `heddle undo`, `undo --redo`, and `undo --recover`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "UndoSchema")]
pub struct UndoRedoOutput {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub action: String,
    pub message: String,
    pub batches: Vec<UndoBatchSummary>,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplate>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplate>,
    /// heddle#305: the pre-undo state preserved for recovery, and the marker
    /// pointing at it. Present only on a completed `undo`; omitted from the
    /// wire when absent (preview / redo).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_marker: Option<String>,
    #[serde(skip_serializing)]
    #[schemars(skip)]
    pub trust: Option<RepositoryVerificationState>,
}
