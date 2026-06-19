// SPDX-License-Identifier: Apache-2.0
//! Hosted namespace and repository admin message payloads.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateNamespace {
    pub kind: String,
    pub slug: String,
    #[serde(default)]
    pub parent_path: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListHostedNamespaces {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostedNamespaceInfo {
    pub namespace_id: String,
    pub kind: String,
    pub slug: String,
    pub parent_id: Option<String>,
    pub display_name: Option<String>,
    pub full_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceCreated {
    pub namespace: HostedNamespaceInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateNamespace {
    pub full_path: String,
    #[serde(default)]
    pub new_slug: Option<String>,
    #[serde(default)]
    pub display_name: Option<Option<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteNamespace {
    pub full_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceUpdated {
    pub namespace: HostedNamespaceInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceDeleted {
    pub full_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespacesList {
    pub namespaces: Vec<HostedNamespaceInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateHostedRepository {
    pub namespace_path: String,
    pub slug: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListHostedRepositories {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostedRepositoryInfo {
    pub repo_id: String,
    pub namespace_id: String,
    pub slug: String,
    pub path: PathBuf,
    pub full_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryCreated {
    pub repository: HostedRepositoryInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateHostedRepository {
    pub full_path: String,
    pub new_slug: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteHostedRepository {
    pub full_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryUpdated {
    pub repository: HostedRepositoryInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryDeleted {
    pub full_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoriesList {
    pub repositories: Vec<HostedRepositoryInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateHostedGrant {
    pub subject: String,
    pub role: String,
    #[serde(default)]
    pub namespace_path: Option<String>,
    #[serde(default)]
    pub repo_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteHostedGrant {
    pub subject: String,
    #[serde(default)]
    pub namespace_path: Option<String>,
    #[serde(default)]
    pub repo_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateHostedGrant {
    pub subject: String,
    pub role: String,
    #[serde(default)]
    pub namespace_path: Option<String>,
    #[serde(default)]
    pub repo_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListHostedGrants {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostedGrantInfo {
    pub subject: String,
    pub role: String,
    #[serde(default)]
    pub namespace_path: Option<String>,
    #[serde(default)]
    pub repo_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostedGrantCreated {
    pub grant: HostedGrantInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostedGrantDeleted {
    pub subject: String,
    #[serde(default)]
    pub namespace_path: Option<String>,
    #[serde(default)]
    pub repo_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostedGrantUpdated {
    pub grant: HostedGrantInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostedGrantsList {
    pub grants: Vec<HostedGrantInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HarnessIdentity {
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub thinking_level: Option<String>,
    #[serde(default)]
    pub policy: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub tool_calls: Option<u32>,
    #[serde(default)]
    pub cost_micros_usd: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProgressCheckpoint {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub completed_steps: Option<u32>,
    #[serde(default)]
    pub total_steps: Option<u32>,
    #[serde(default)]
    pub touched_paths: Vec<String>,
    pub recorded_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscriptAttachmentRef {
    pub attachment_id: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionDiffSummary {
    #[serde(default)]
    pub changed_file_count: u32,
    #[serde(default)]
    pub added_files: u32,
    #[serde(default)]
    pub modified_files: u32,
    #[serde(default)]
    pub deleted_files: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeChangeBaseline {
    pub path: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReportEnvelope {
    pub version: u32,
    pub heddle_session_id: String,
    #[serde(default)]
    pub heddle_segment_id: Option<String>,
    #[serde(default)]
    pub agent_session_id: Option<String>,
    #[serde(default)]
    pub client_instance_id: Option<String>,
    #[serde(default)]
    pub native_actor_key: Option<String>,
    #[serde(default)]
    pub native_parent_actor_key: Option<String>,
    #[serde(default)]
    pub native_instance_key: Option<String>,
    pub repo_root: String,
    #[serde(default)]
    pub thread: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    pub opened_at: String,
    #[serde(default)]
    pub closed_at: Option<String>,
    #[serde(default)]
    pub base_state_at_open: Option<String>,
    #[serde(default)]
    pub worktree_changes_at_open: Vec<WorktreeChangeBaseline>,
    #[serde(default)]
    pub head_state_at_close: Option<String>,
    pub transport_mode: String,
    pub transcript_mode: String,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub owns_session: bool,
    pub harness: HarnessIdentity,
    #[serde(default)]
    pub progress: Vec<ProgressCheckpoint>,
    #[serde(default)]
    pub usage: UsageTotals,
    #[serde(default)]
    pub touched_paths: Vec<String>,
    #[serde(default)]
    pub changed_paths: Vec<String>,
    #[serde(default)]
    pub diff_summary: Option<SessionDiffSummary>,
    #[serde(default)]
    pub transcript_refs: Vec<TranscriptAttachmentRef>,
    #[serde(default)]
    pub last_progress_at: Option<String>,
    #[serde(default)]
    pub report_flush_state: Option<String>,
    #[serde(default)]
    pub attach_reason: Option<String>,
    #[serde(default)]
    pub attach_precedence: Vec<String>,
    #[serde(default)]
    pub winning_attach_rule: Option<String>,
    #[serde(default)]
    pub probe_source: Option<String>,
    #[serde(default)]
    pub probe_confidence: Option<f32>,
    #[serde(default)]
    pub pending_flush: bool,
    #[serde(default)]
    pub last_flushed_at: Option<String>,
}
