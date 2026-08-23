// SPDX-License-Identifier: Apache-2.0
//! Wire payloads for the thread family (`thread list/show/current/captures`,
//! `start`, thread refresh/drop/promote, cleanup, resolve, absorb, and the
//! approval verbs).

use schemars::JsonSchema;
use serde::Serialize;

use heddle_cli_render::cli::render::RepositoryContextInfo;

use super::operator::OperatorCommandOutput;
use verbs::{ActionTemplate, AvailableGitRef, RepositoryVerificationState, ThreadSummary};

/// FSKit readiness detail surfaced by `start --workspace virtualized` on
/// macOS when the CLI took an FSKit-specific decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(rename = "FsKitReadinessSchema")]
pub struct FskitReadinessReport {
    pub state: &'static str,
    pub backend: &'static str,
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings_url: Option<&'static str>,
}

/// JSON payload for `heddle thread list`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ThreadListSchema")]
pub struct ThreadListOutput {
    pub output_kind: &'static str,
    pub repository_capability: String,
    pub repository_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_context: Option<RepositoryContextInfo>,
    pub storage_model: String,
    pub hosted_enabled: bool,
    pub threads: Vec<ThreadSummary>,
    pub available_git_refs: Vec<AvailableGitRef>,
    pub current: Option<String>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    #[schemars(with = "Option<String>")]
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplate>,
    pub recovery_commands: Vec<String>,
    pub recovery_action_templates: Vec<ActionTemplate>,
    /// Carried for the human-readable renderer only. Not part of the
    /// JSON contract: import-hint information is exposed via
    /// `heddle status --output json` instead.
    #[serde(skip)]
    #[schemars(skip)]
    pub import_guidance: Option<ThreadListImportGuidanceOutput>,
}

#[derive(Serialize, JsonSchema)]
pub struct ThreadListImportGuidanceOutput {
    pub current_branch: String,
    pub missing_branch_count: usize,
    pub missing_branches: Vec<String>,
    pub recommended_command: String,
}

/// JSON payload for `heddle thread show`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ThreadShowSchema")]
pub struct ThreadShowOutput {
    pub output_kind: &'static str,
    pub repository_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_context: Option<RepositoryContextInfo>,
    #[serde(flatten)]
    pub summary: ThreadSummary,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    #[schemars(with = "Option<String>")]
    pub next_action: String,
    pub next_action_template: Option<ActionTemplate>,
    pub recommended_action_template: Option<ActionTemplate>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
    pub recovery_commands: Vec<String>,
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

/// JSON payload for `start`, `thread create`, `thread switch`,
/// `thread rename`, and thread refresh/drop/promote.
#[derive(Serialize, JsonSchema)]
pub struct ThreadOpOutput {
    pub output_kind: &'static str,
    pub status: &'static str,
    pub action: &'static str,
    pub name: String,
    pub message: String,
    pub next_action: Option<String>,
    pub next_action_template: Option<ActionTemplate>,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplate>,
    pub thread: Option<ThreadSummary>,
    pub path: Option<String>,
    pub execution_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fskit_readiness: Option<FskitReadinessReport>,
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    #[schemars(skip)]
    pub trust: Option<RepositoryVerificationState>,
}

/// JSON payload for `thread current`.
#[derive(Serialize, JsonSchema)]
pub struct ThreadCurrentOutput {
    pub thread: String,
}

/// One entry of the `thread captures` array.
#[derive(Clone, Serialize, JsonSchema)]
pub struct ThreadCaptureOutput {
    pub state_id: String,
    pub created_at: String,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub agent: Option<String>,
    pub message: String,
    /// Per-capture file count delta vs the parent state. `None` for
    /// captures with no parent (the bootstrap snapshot of a fresh
    /// repo) and when the diff cannot be computed (parent state
    /// missing from the local store).
    pub summary: Option<ThreadCaptureSummary>,
}

#[derive(Clone, Serialize, JsonSchema)]
pub struct ThreadCaptureSummary {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub total: usize,
}

/// JSON payload for `thread refresh` / `thread drop` / `thread promote`
/// where the whole refreshed record is echoed beside the operator envelope.
#[derive(Serialize, JsonSchema)]
pub struct ThreadRecordOutput {
    #[serde(flatten)]
    pub operator: OperatorCommandOutput,
    pub thread: repo::Thread,
    pub changed_path_count: usize,
}

/// JSON payload for `thread cleanup`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ThreadCleanupSchema")]
pub struct ThreadCleanupOutput {
    #[serde(flatten)]
    pub operator: OperatorCommandOutput,
    /// Whether the run was a dry run (no on-disk changes performed).
    pub dry_run: bool,
    /// Threads dropped (or that would be dropped, in dry-run) because
    /// their lifecycle state is `merged`.
    pub merged: Vec<DroppedThread>,
    /// Threads dropped (or that would be dropped, in dry-run) because
    /// they are auto-created and stale per `--older-than`.
    pub auto: Vec<DroppedThread>,
    /// Abandoned threads cleaned (or that would be cleaned, in dry-run)
    /// because operational residue remains.
    pub abandoned: Vec<DroppedThread>,
    /// Total bytes reclaimed from removing thread checkouts. Always
    /// `0` in dry-run mode — see `would_reclaim_bytes` for the
    /// estimate.
    pub reclaimed_bytes: u64,
    /// Estimated bytes that *would* be reclaimed if the run were applied.
    pub would_reclaim_bytes: u64,
    /// Threads that matched the cleanup criteria but were skipped
    /// (e.g. the active thread the user is currently inside). Empty
    /// in the common case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedThread>,
}

#[derive(Clone, Serialize, JsonSchema)]
#[schemars(rename = "ThreadDroppedSchema")]
pub struct DroppedThread {
    pub thread: String,
    pub id: String,
    pub reason: &'static str,
    pub age_seconds: i64,
    /// Bytes the thread checkout occupied on disk before removal.
    /// `0` when no execution path existed (e.g. lightweight thread
    /// with the checkout already pruned).
    pub bytes: u64,
    pub execution_path: Option<String>,
}

#[derive(Clone, Serialize, JsonSchema)]
#[schemars(rename = "ThreadCleanupSkippedSchema")]
pub struct SkippedThread {
    pub thread: String,
    pub id: String,
    /// Stable reason code so automation can branch on it. Currently
    /// only `active` is emitted.
    pub reason: &'static str,
    /// Human-readable note explaining the skip.
    pub note: String,
}

/// JSON payload for `thread resolve`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ThreadResolveSchema")]
pub struct ThreadResolveOutput {
    #[serde(flatten)]
    pub operator: OperatorCommandOutput,
    pub thread: String,
}

/// JSON payload for `thread absorb`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ThreadAbsorbSchema")]
pub struct ThreadAbsorbOutput {
    pub thread: String,
    pub into: String,
    pub preview_only: bool,
    pub conflicts: Vec<String>,
    pub merge_state: Option<String>,
    pub message: String,
}

/// One approval row (`thread approve`, `thread approvals`,
/// `thread check-merge`).
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ThreadApprovalSchema")]
pub struct ApprovalOutput {
    pub id: String,
    pub repo_path: String,
    pub source_thread: String,
    pub target_thread: String,
    pub source_state: String,
    pub approver_user_id: String,
    pub note: String,
    pub approved_at: u64,
    pub expires_at: u64,
}

/// One unmet merge-eligibility requirement.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ThreadMergeRequirementSchema")]
pub struct UnmetOutput {
    pub policy_id: String,
    pub kind: String,
    pub group_id: String,
    pub reason: String,
    pub needed: u32,
    pub have: u32,
}

/// JSON payload for `thread check-merge`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ThreadMergeEligibilitySchema")]
pub struct EligibilityOutput {
    pub allowed: bool,
    pub unmet: Vec<UnmetOutput>,
    pub valid_approvals: Vec<ApprovalOutput>,
}

/// JSON payload for `thread revoke-approval`.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "ThreadRevokeApprovalSchema")]
pub struct ApprovalRevokeOutput {
    pub output_kind: &'static str,
    pub id: String,
    pub deleted: bool,
}
