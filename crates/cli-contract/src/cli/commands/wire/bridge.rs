// SPDX-License-Identifier: Apache-2.0
//! Wire payloads for the Git bridge (`bridge git export/import`,
//! `sync git`), `integration list`, and `maintenance repack`.

use schemars::JsonSchema;
use serde::Serialize;
use verbs::{ActionTemplate, RepositoryVerificationState};

/// JSON payload for `bridge git export`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ExportGitSchema")]
pub struct ExportGitOutput {
    pub output_kind: &'static str,
    pub states_exported: u64,
    pub commits_total: u64,
    pub threads_synced: u64,
    pub markers_synced: u64,
    pub branches: Vec<ExportedRefOutput>,
    pub tags: Vec<ExportedRefOutput>,
    pub destination: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ExportedRefOutput {
    pub name: String,
    pub tip: String,
}

/// JSON payload for `bridge git import`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ImportGitSchema")]
pub struct ImportGitOutput {
    pub output_kind: &'static str,
    pub status: String,
    pub action: &'static str,
    pub summary: String,
    pub commits_imported: usize,
    pub states_created: usize,
    pub branches_synced: usize,
    pub tags_synced: usize,
    pub skipped_non_commit_refs: usize,
    pub lossy_entries: Vec<LossyImportEntryOutput>,
    pub already_in_sync: bool,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    #[schemars(with = "Option<String>")]
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplate>,
    pub recovery_commands: Vec<String>,
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    #[schemars(skip)]
    pub trust: RepositoryVerificationState,
}

#[derive(Serialize, JsonSchema)]
pub struct LossyImportEntryOutput {
    pub path: String,
    pub action: String,
    pub reason: String,
    pub git_object: Option<String>,
}

/// JSON payload for `sync git`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "SyncGitSchema")]
pub struct SyncGitOutput {
    pub output_kind: &'static str,
    pub status: String,
    pub action: &'static str,
    pub summary: String,
    pub states_exported: usize,
    pub commits_exported_total: usize,
    pub commits_imported: usize,
    pub threads_synced: usize,
    pub markers_synced: usize,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    #[schemars(with = "Option<String>")]
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplate>,
    pub recovery_commands: Vec<String>,
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    #[schemars(skip)]
    pub trust: RepositoryVerificationState,
}

/// One row of `integration list`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[schemars(rename = "IntegrationStatusListSchema")]
pub struct IntegrationStatusOutput {
    pub harness: String,
    pub scope: String,
    pub method: String,
    pub status: String,
    pub healthy: bool,
    pub paths: Vec<String>,
    pub capabilities: Vec<String>,
    pub capability_paths: Vec<String>,
    pub path_mode: String,
}

/// JSON payload for `maintenance repack`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "MaintenanceRepackSchema")]
pub struct RepackOutput {
    pub output_kind: &'static str,
    pub objects_repacked: u64,
    pub bytes_repacked: u64,
    pub duration_ms: u128,
    pub bytes_reclaimed: u64,
}

fn serialize_empty_action_as_null<S>(
    action: &String,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    // "" means "no action selected"; the wire contract is null
    // (HeddleCo/heddle#645), matching `verbs::serialize_empty_action_as_null`.
    if action.is_empty() {
        serializer.serialize_none()
    } else {
        serializer.serialize_some(action)
    }
}
