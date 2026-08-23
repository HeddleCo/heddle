// SPDX-License-Identifier: Apache-2.0
//! Wire payloads for `clone`, `adopt`, `remote add/remove/set-default`,
//! `pull`, and `push`.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Serialize;
use verbs::{
    ActionTemplate, PullOutcome, PushOutcome, RepositoryVerificationState,
};

/// JSON payload for `heddle clone`. One struct for both transports: the
/// transport-specific facts are optional fields, omitted when absent.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "CloneSchema")]
pub struct CloneOutput {
    pub output_kind: &'static str,
    pub action: &'static str,
    pub status: &'static str,
    pub success: bool,
    pub cloned: bool,
    pub transport: &'static str,
    pub remote: String,
    pub local: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_capability: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commits_imported: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub states_created: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objects: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(rename = "verification")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust: Option<RepositoryVerificationState>,
}

/// JSON payload for `heddle adopt`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "AdoptSchema")]
pub struct AdoptOutput {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub action: &'static str,
    pub adopted: bool,
    pub initialized: bool,
    #[schemars(with = "String")]
    pub path: PathBuf,
    pub refs: Vec<String>,
    pub commits_imported: usize,
    pub states_created: usize,
    pub branches_synced: usize,
    pub tags_synced: usize,
    pub skipped_non_commit_refs: usize,
    pub already_in_sync: bool,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplate>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

/// JSON payload for `remote add` / `remote remove` / `remote set-default`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "RemoteMutationSchema")]
pub struct RemoteMutationOutput {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub action: &'static str,
    pub name: String,
    pub url: Option<String>,
    pub default: Option<String>,
    pub message: String,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

/// JSON payload for `heddle pull`: the verbs [`PullOutcome`] body beside
/// repository verification.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "PullOutput")]
pub struct PullOutput {
    #[serde(flatten)]
    pub outcome: PullOutcome,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

/// JSON payload for `heddle push`: the verbs [`PushOutcome`] body beside
/// verification-derived recovery actions.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "PushOutput")]
pub struct PushOutput {
    #[serde(flatten)]
    pub outcome: PushOutcome,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplate>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplate>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}
