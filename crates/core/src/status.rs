// SPDX-License-Identifier: Apache-2.0
//! Status facade and report contract.

pub mod next_action;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Instant,
};

use objects::{
    HeddleError,
    error::Result,
    object::{State, ThreadName},
    worktree::WorktreeStatus,
};
use repo::{
    AgentUsageSummary, GitOverlayImportHint, GitRemoteTrackingStatus, RepoConfig, Repository,
    RepositoryCapability, RepositoryOperationStatus, Thread, ThreadFreshness, ThreadImpactCategory,
    ThreadMode, ThreadState, WorktreeCompareProfile, describe_thread_advice_with_initial,
    is_synthetic_root,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use sley::{
    Repository as SleyRepository, ShortStatusOptions, ShortStatusRow, StatusUntrackedMode,
    StreamControl,
};

use crate::{
    ActionTemplate, ExecutionContext, HeddleReport, MachineOutputKind, OutputDiscriminator,
    ReportContract, RepositoryContextInfo, RepositoryVerificationState, schema_for_report,
    verify::serialize_empty_action_as_null,
};

use self::next_action::{
    NextActionInput, effective_next_action, non_empty_action, remote_tracking_status,
    thread_recovery_action_is_primary,
};

pub type GitOverlayHealthFn = fn(&Repository, &Result<Option<WorktreeStatus>>) -> GitOverlayHealth;
pub type RepositoryTrustWithWorktreeFn = fn(
    &Repository,
    GitOverlayHealth,
    &Result<Option<WorktreeStatus>>,
) -> RepositoryVerificationState;
pub type GitIndexForRepoFn = fn(&Repository) -> Result<Option<GitIndexPlan>>;
pub type IdentityNoticeFn = fn(&Repository, Option<&State>) -> Result<Option<String>>;
pub type ThreadSummariesFn = fn(&Repository) -> Result<Vec<StatusThreadSummary>>;
pub type ThreadSummaryFn = fn(&Repository, &str) -> Result<Option<StatusThreadSummary>>;
pub type ContextualThreadActionFn = fn(&Repository, &str, Option<&str>, &str) -> String;
pub type ActionTemplateFn = fn(&str) -> Option<ActionTemplate>;

#[derive(Clone, Copy)]
pub struct StatusAdapters {
    pub git_overlay_health: GitOverlayHealthFn,
    pub repository_trust_with_worktree: RepositoryTrustWithWorktreeFn,
    pub git_index_for_repo: GitIndexForRepoFn,
    pub identity_notice: IdentityNoticeFn,
    pub collect_thread_summaries: ThreadSummariesFn,
    pub find_thread_summary: ThreadSummaryFn,
    pub contextual_thread_action: ContextualThreadActionFn,
    pub action_template: ActionTemplateFn,
}

#[derive(Clone)]
pub struct StatusOptions {
    pub start_path: Option<PathBuf>,
    pub detail: StatusDetail,
    pub worktree_status_options: repo::WorktreeStatusOptions,
    pub adapters: StatusAdapters,
}

impl StatusOptions {
    pub fn new(
        detail: StatusDetail,
        worktree_status_options: repo::WorktreeStatusOptions,
        adapters: StatusAdapters,
    ) -> Self {
        Self {
            start_path: None,
            detail,
            worktree_status_options,
            adapters,
        }
    }

    pub fn with_start_path(mut self, start_path: impl Into<PathBuf>) -> Self {
        self.start_path = Some(start_path.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusDetail {
    ShortText,
    CompactMachine,
    DefaultText,
    Full,
}

impl StatusDetail {
    fn short_path(self) -> bool {
        matches!(self, Self::ShortText | Self::CompactMachine)
    }

    fn needs_full_walk(self) -> bool {
        matches!(self, Self::Full)
    }

    fn needs_remote_tracking(self) -> bool {
        matches!(self, Self::ShortText | Self::Full)
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(rename = "StatusSchema")]
pub struct StatusReport {
    pub output_kind: &'static str,
    pub repository_capability: String,
    pub repository_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_context: Option<RepositoryContextInfo>,
    pub storage_model: String,
    pub hosted_enabled: bool,
    #[serde(skip)]
    #[schemars(skip)]
    pub validation_capability: RepositoryCapability,
    #[schemars(with = "Option<serde_json::Value>")]
    pub operation: Option<RepositoryOperationStatus>,
    #[schemars(with = "Option<serde_json::Value>")]
    pub remote_tracking: Option<GitRemoteTrackingStatus>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
    pub git_index: Option<GitIndexPlan>,
    #[serde(skip)]
    #[schemars(skip)]
    pub git_overlay_import_hint: Option<GitOverlayImportHintReport>,
    pub git_overlay_health: GitOverlayHealth,
    pub thread: Option<String>,
    pub base_state: Option<String>,
    pub base_root: Option<String>,
    pub current_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heddle_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<ActorInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub usage_summary: Option<AgentUsageSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_progress_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_flush_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attach_reason: Option<String>,
    #[schemars(with = "Option<String>")]
    pub thread_mode: Option<ThreadMode>,
    #[schemars(with = "Option<String>")]
    pub thread_state: Option<ThreadState>,
    #[schemars(with = "Option<String>")]
    pub freshness: Option<ThreadFreshness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_thread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thread: Option<String>,
    pub child_threads: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub promotion_suggested: bool,
    #[schemars(with = "Vec<String>")]
    pub impact_categories: Vec<ThreadImpactCategory>,
    pub heavy_impact_paths: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub changed_paths: Vec<String>,
    pub changed_path_count: usize,
    pub worktree_changed_path_count: usize,
    pub thread_changed_path_count: usize,
    pub blockers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_notice: Option<String>,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    #[schemars(with = "Option<String>")]
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplate>,
    pub recovery_commands: Vec<String>,
    pub recovery_action_templates: Vec<ActionTemplate>,
    pub thread_health: String,
    pub coordination_status: CoordinationStatus,
    #[serde(skip)]
    #[schemars(skip)]
    pub coordination_blocked_by_trust: bool,
    pub is_isolated: bool,
    pub parallel_threads: Vec<ParallelThreadInfo>,
    pub state: Option<StateInfo>,
    pub git_checkpoint: Option<GitCheckpointInfo>,
    pub changes: ChangesInfo,
    #[serde(default)]
    pub materialized_threads: Vec<MaterializedThreadInfo>,
    #[serde(skip)]
    #[schemars(skip)]
    pub profile: StatusProfile,
}

impl StatusReport {
    pub const CONTRACT: ReportContract = ReportContract {
        schema_name: "status",
        machine_output_kind: MachineOutputKind::JsonOrJsonLines,
        output_discriminator: Some(OutputDiscriminator {
            field: "output_kind",
            value: "status",
        }),
        schema: status_report_schema,
    };
}

impl HeddleReport for StatusReport {
    const CONTRACT: ReportContract = StatusReport::CONTRACT;
}

fn status_report_schema() -> Value {
    let mut schema = schema_for_report::<StatusReport>();
    require_schema_field(&mut schema, "recommended_action");
    replace_property_schema(
        &mut schema,
        "thread_mode",
        serde_json::json!({
            "anyOf": [
                {
                    "type": "string",
                    "enum": ["materialized", "virtualized", "solid"]
                },
                { "type": "null" }
            ]
        }),
    );
    schema
}

fn require_schema_field(schema: &mut Value, field: &str) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let required = object
        .entry("required".to_string())
        .or_insert_with(|| serde_json::json!([]));
    let Some(required) = required.as_array_mut() else {
        return;
    };
    if !required
        .iter()
        .any(|candidate| candidate.as_str() == Some(field))
    {
        required.push(Value::String(field.to_string()));
    }
}

fn replace_property_schema(schema: &mut Value, field: &str, replacement: Value) {
    let Some(properties) = schema
        .get_mut("properties")
        .and_then(|properties| properties.as_object_mut())
    else {
        return;
    };
    properties.insert(field.to_string(), replacement);
}

#[derive(Debug, Clone, Default)]
pub struct StatusProfile {
    pub repo_open_ms: u128,
    pub current_state_ms: u128,
    pub operation_ms: u128,
    pub remote_tracking_ms: u128,
    pub import_hint_ms: u128,
    pub git_overlay_status_ms: u128,
    pub git_overlay_health_ms: u128,
    pub verification_ms: u128,
    pub git_index_ms: u128,
    pub worktree_status_ms: u128,
    pub thread_summary_ms: u128,
    pub parallel_threads_ms: u128,
    pub late_state_ms: u128,
    pub materialized_threads_ms: u128,
    pub advice_ms: u128,
    pub build_total_ms: u128,
    pub worktree_profile: Option<WorktreeCompareProfile>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GitOverlayHealth {
    pub status: String,
    pub clean: bool,
    pub summary: String,
    pub recovery_commands: Vec<String>,
    pub checks: Vec<GitOverlayHealthCheck>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GitOverlayHealthCheck {
    pub name: String,
    pub status: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub details: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GitOverlayImportHintReport {
    pub current_branch: String,
    pub missing_branch_count: usize,
    pub missing_branches: Vec<String>,
    pub recommended_command: String,
}

impl From<GitOverlayImportHint> for GitOverlayImportHintReport {
    fn from(hint: GitOverlayImportHint) -> Self {
        Self {
            current_branch: hint.current_branch,
            missing_branch_count: hint.missing_branch_count,
            missing_branches: hint.missing_branches,
            recommended_command: hint.recommended_command,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GitIndexPlan {
    pub commit_mode: &'static str,
    pub has_staged_changes: bool,
    pub staged_paths: Vec<String>,
    pub unstaged_paths: Vec<String>,
    pub untracked_paths: Vec<String>,
    pub will_commit: Vec<String>,
    pub preserved_after_commit: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MaterializedThreadInfo {
    pub name: String,
    pub state_id: String,
    pub tree_hash_short: String,
    pub file_count: usize,
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ActorInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ParallelThreadInfo {
    pub name: String,
    pub coordination_status: CoordinationStatus,
    pub current_state: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StateInfo {
    pub change_id: String,
    pub content_hash: String,
    pub intent: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GitCheckpointInfo {
    pub git_commit: String,
    pub committed_at: String,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ChangesInfo {
    pub modified: Vec<String>,
    pub added: Vec<String>,
    pub deleted: Vec<String>,
}

impl ChangesInfo {
    pub fn is_empty(&self) -> bool {
        self.modified.is_empty() && self.added.is_empty() && self.deleted.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CoordinationStatus {
    Clean,
    Ahead,
    Diverged,
    Blocked,
    MergeReady,
}

impl std::fmt::Display for CoordinationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clean => write!(f, "clean"),
            Self::Ahead => write!(f, "ahead"),
            Self::Diverged => write!(f, "diverged"),
            Self::Blocked => write!(f, "blocked"),
            Self::MergeReady => write!(f, "merge-ready"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatusThreadSummary {
    pub name: String,
    pub base_state: Option<String>,
    pub base_root: Option<String>,
    pub current_state: Option<String>,
    pub path: Option<String>,
    pub execution_path: Option<String>,
    pub session_id: Option<String>,
    pub heddle_session_id: Option<String>,
    pub actor: Option<ActorInfo>,
    pub harness: Option<String>,
    pub thinking_level: Option<String>,
    pub usage_summary: Option<AgentUsageSummary>,
    pub last_progress_at: Option<String>,
    pub report_flush_state: Option<String>,
    pub attach_reason: Option<String>,
    pub thread_mode: Option<ThreadMode>,
    pub thread_state: Option<ThreadState>,
    pub freshness: Option<ThreadFreshness>,
    pub target_thread: Option<String>,
    pub parent_thread: Option<String>,
    pub child_threads: Vec<String>,
    pub task: Option<String>,
    pub promotion_suggested: bool,
    pub impact_categories: Vec<ThreadImpactCategory>,
    pub heavy_impact_paths: Vec<String>,
    pub changed_paths: Vec<String>,
    pub verification_summary: repo::ThreadVerificationSummary,
    pub confidence_summary: repo::ThreadConfidenceSummary,
    pub integration_policy_result: repo::ThreadIntegrationPolicy,
    pub coordination_status: CoordinationStatus,
    pub is_current: bool,
    pub is_isolated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FastShortStatusReport {
    pub subject: String,
    pub health: String,
    pub changes: ChangesInfo,
    #[serde(skip)]
    #[schemars(skip)]
    pub profile: FastShortStatusProfile,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FastShortStatusProfile {
    pub git_discover_ms: u128,
    pub config_ms: u128,
    pub sley_status_ms: u128,
    pub branch_ms: u128,
    pub remote_ms: u128,
    pub total_ms: u128,
}

pub fn status(ctx: &ExecutionContext, opts: StatusOptions) -> Result<StatusReport> {
    let fallback;
    let start = if let Some(start) = opts.start_path.as_deref() {
        start
    } else if let Some(start) = ctx.start_path() {
        start
    } else {
        fallback = std::env::current_dir().map_err(HeddleError::Io)?;
        fallback.as_path()
    };

    let repo_open_start = Instant::now();
    let opened;
    let repo = if let Some(repo) = ctx.repo() {
        repo
    } else {
        opened = Repository::open(start)?;
        &opened
    };
    let repo_open_ms = repo_open_start.elapsed().as_millis();
    let body_start = Instant::now();

    let current_state_start = Instant::now();
    let current_state = repo.current_state()?;
    let current_state_ms = current_state_start.elapsed().as_millis();

    let operation_start = Instant::now();
    let operation = repo.operation_status()?;
    let operation_ms = operation_start.elapsed().as_millis();

    let remote_tracking_start = Instant::now();
    let remote_tracking = if opts.detail.needs_remote_tracking() {
        repo.git_remote_tracking_status().unwrap_or(None)
    } else {
        None
    };
    let remote_tracking_ms = remote_tracking_start.elapsed().as_millis();

    let import_hint_start = Instant::now();
    let import_hint = if opts.detail.short_path() {
        None
    } else {
        repo.git_overlay_import_hint().unwrap_or(None)
    };
    let import_hint_ms = import_hint_start.elapsed().as_millis();

    let git_overlay_status_start = Instant::now();
    let git_worktree_status_result = repo.git_overlay_worktree_status();
    let git_overlay_status_ms = git_overlay_status_start.elapsed().as_millis();

    let git_overlay_health_start = Instant::now();
    let git_overlay_health = (opts.adapters.git_overlay_health)(repo, &git_worktree_status_result);
    let git_overlay_health_ms = git_overlay_health_start.elapsed().as_millis();

    let verification_start = Instant::now();
    let trust = (opts.adapters.repository_trust_with_worktree)(
        repo,
        git_overlay_health.clone(),
        &git_worktree_status_result,
    );
    let verification_ms = verification_start.elapsed().as_millis();
    let remote_tracking =
        remote_tracking.map(|remote| remote_tracking_with_verification_action(remote, &trust));

    let git_worktree_status = git_worktree_status_result.unwrap_or(None);

    let git_index_start = Instant::now();
    let git_index = (opts.adapters.git_index_for_repo)(repo)?;
    let git_index_ms = git_index_start.elapsed().as_millis();

    let identity_notice = (opts.adapters.identity_notice)(repo, current_state.as_ref())?;
    let git_clean_mapping_blocker = matches!(
        trust.status.as_str(),
        "needs_import" | "needs_reconcile" | "git_branch_advanced"
    ) && git_worktree_status
        .as_ref()
        .is_some_and(WorktreeStatus::is_clean);
    let git_backed_mapping = trust.mapping_state == "git_backed";

    let worktree_status_start = Instant::now();
    let (changes, worktree_profile) = if git_clean_mapping_blocker {
        (ChangesInfo::default(), None)
    } else if let Some(status) = git_worktree_status.as_ref()
        && !status.is_clean()
        && trust.status != "needs_checkpoint"
    {
        (changes_from_worktree_status(status), None)
    } else if git_backed_mapping {
        (
            git_worktree_status
                .as_ref()
                .map(changes_from_worktree_status)
                .unwrap_or_default(),
            None,
        )
    } else if let Some(ref state) = current_state {
        let tree = repo.require_tree(&state.tree)?;
        let (status, profile) = repo
            .compare_worktree_cached_profiled_with_options(&tree, &opts.worktree_status_options)?;
        (changes_from_worktree_status(&status), Some(profile))
    } else if let Some(status) = git_worktree_status {
        (changes_from_worktree_status(&status), None)
    } else {
        let tree = objects::object::Tree::new();
        let (status, profile) = repo
            .compare_worktree_cached_profiled_with_options(&tree, &opts.worktree_status_options)?;
        let mut changes = changes_from_worktree_status(&status);
        changes.modified.clear();
        changes.deleted.clear();
        (changes, Some(profile))
    };
    let worktree_status_ms = worktree_status_start.elapsed().as_millis();

    if opts.detail.short_path() {
        return Ok(build_short_path_report(ShortPathInputs {
            repo,
            opts: &opts,
            current_state: current_state.as_ref(),
            operation,
            remote_tracking,
            git_overlay_health,
            trust,
            import_hint,
            git_index,
            identity_notice,
            changes,
            profile: StatusProfile {
                repo_open_ms,
                current_state_ms,
                operation_ms,
                remote_tracking_ms,
                import_hint_ms,
                git_overlay_status_ms,
                git_overlay_health_ms,
                verification_ms,
                git_index_ms,
                worktree_status_ms,
                build_total_ms: body_start.elapsed().as_millis(),
                worktree_profile,
                ..StatusProfile::default()
            },
        }));
    }

    let thread_summary_start = Instant::now();
    let track_name = repo.current_lane()?;
    let full_thread_summaries = if opts.detail.needs_full_walk() {
        Some((opts.adapters.collect_thread_summaries)(repo)?)
    } else {
        None
    };
    let thread_summary = match (track_name.as_deref(), full_thread_summaries.as_ref()) {
        (Some(thread), Some(summaries)) => summaries
            .iter()
            .find(|summary| summary.name == thread)
            .cloned(),
        (Some(thread), None) => (opts.adapters.find_thread_summary)(repo, thread)?,
        (None, _) => None,
    };
    let thread_summary_ms = thread_summary_start.elapsed().as_millis();

    let parallel_threads_start = Instant::now();
    let parallel_threads = if let Some(summaries) = full_thread_summaries {
        summaries
            .into_iter()
            .filter(|thread| !thread.is_current)
            .filter(|thread| {
                matches!(
                    thread.coordination_status,
                    CoordinationStatus::Ahead
                        | CoordinationStatus::Blocked
                        | CoordinationStatus::Diverged
                        | CoordinationStatus::MergeReady
                )
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let parallel_threads_ms = parallel_threads_start.elapsed().as_millis();

    let late_state_start = Instant::now();
    let state_info = current_state.as_ref().map(|s| StateInfo {
        change_id: s.change_id.short(),
        content_hash: s.compute_hash().short(),
        intent: s.intent.clone(),
    });
    let current_state_short = current_state.as_ref().map(|state| state.change_id.short());
    let git_checkpoint = if trust.status == "needs_checkpoint" {
        None
    } else {
        current_state
            .as_ref()
            .and_then(|state| {
                repo.latest_git_checkpoint_for_change(&state.change_id)
                    .ok()
                    .flatten()
            })
            .map(|record| GitCheckpointInfo {
                git_commit: record.git_commit,
                committed_at: record.committed_at,
            })
    };

    let materialized_start = Instant::now();
    let materialized_threads = assess_materialized_threads(repo);
    let materialized_ms = materialized_start.elapsed().as_millis();
    let target_thread = thread_summary
        .as_ref()
        .and_then(|thread| thread.target_thread.clone());
    let parent_thread = thread_summary
        .as_ref()
        .and_then(|thread| thread.parent_thread.clone());
    let presentation =
        crate::repository_presentation(repo, target_thread.as_deref(), parent_thread.as_deref());

    let output = StatusReport {
        output_kind: "status",
        repository_capability: repo.capability_label().to_string(),
        repository_label: presentation.label,
        repository_context: presentation.context,
        storage_model: repo.storage_model_label().to_string(),
        hosted_enabled: repo.hosted_enabled(),
        validation_capability: repo.capability(),
        git_overlay_import_hint: import_hint.clone().map(Into::into),
        git_overlay_health: git_overlay_health.clone(),
        trust: trust.clone(),
        operation,
        remote_tracking,
        git_index,
        thread: track_name.clone(),
        base_state: thread_summary
            .as_ref()
            .and_then(|thread| thread.base_state.clone())
            .or_else(|| current_state_short.clone()),
        base_root: thread_summary
            .as_ref()
            .and_then(|thread| thread.base_root.clone()),
        current_state: thread_summary
            .as_ref()
            .and_then(|thread| thread.current_state.clone())
            .or_else(|| current_state_short.clone()),
        path: thread_summary
            .as_ref()
            .and_then(|thread| thread.path.clone()),
        execution_path: thread_summary
            .as_ref()
            .and_then(|thread| thread.execution_path.clone()),
        session_id: thread_summary
            .as_ref()
            .and_then(|thread| thread.session_id.clone()),
        heddle_session_id: thread_summary
            .as_ref()
            .and_then(|thread| thread.heddle_session_id.clone()),
        actor: thread_summary
            .as_ref()
            .and_then(|thread| thread.actor.clone()),
        harness: thread_summary
            .as_ref()
            .and_then(|thread| thread.harness.clone()),
        thinking_level: thread_summary
            .as_ref()
            .and_then(|thread| thread.thinking_level.clone()),
        usage_summary: thread_summary
            .as_ref()
            .and_then(|thread| thread.usage_summary.clone()),
        last_progress_at: thread_summary
            .as_ref()
            .and_then(|thread| thread.last_progress_at.clone()),
        report_flush_state: thread_summary
            .as_ref()
            .and_then(|thread| thread.report_flush_state.clone()),
        attach_reason: thread_summary
            .as_ref()
            .and_then(|thread| thread.attach_reason.clone()),
        thread_mode: thread_summary
            .as_ref()
            .and_then(|thread| thread.thread_mode.clone()),
        thread_state: thread_summary
            .as_ref()
            .and_then(|thread| thread.thread_state.clone()),
        freshness: thread_summary
            .as_ref()
            .and_then(|thread| thread.freshness.clone()),
        target_thread,
        parent_thread,
        child_threads: thread_summary
            .as_ref()
            .map(|thread| thread.child_threads.clone())
            .unwrap_or_default(),
        task: thread_summary
            .as_ref()
            .and_then(|thread| thread.task.clone()),
        promotion_suggested: thread_summary
            .as_ref()
            .map(|thread| thread.promotion_suggested)
            .unwrap_or(false),
        impact_categories: thread_summary
            .as_ref()
            .map(|thread| thread.impact_categories.clone())
            .unwrap_or_default(),
        heavy_impact_paths: thread_summary
            .as_ref()
            .map(|thread| thread.heavy_impact_paths.clone())
            .unwrap_or_default(),
        changed_paths: Vec::new(),
        changed_path_count: thread_summary
            .as_ref()
            .map(|thread| thread.changed_paths.len())
            .unwrap_or_default(),
        worktree_changed_path_count: changes_path_count(&changes),
        thread_changed_path_count: captured_thread_path_count(thread_summary.as_ref(), &changes),
        blockers: Vec::new(),
        identity_notice,
        recommended_action: String::new(),
        recommended_action_template: None,
        recovery_commands: trust.recovery_commands.clone(),
        recovery_action_templates: trust.recovery_action_templates.clone(),
        thread_health: "clean".to_string(),
        coordination_status: thread_summary
            .as_ref()
            .map(|thread| thread.coordination_status)
            .unwrap_or(CoordinationStatus::Clean),
        coordination_blocked_by_trust: false,
        is_isolated: thread_summary
            .as_ref()
            .map(|thread| thread.is_isolated)
            .unwrap_or(false),
        parallel_threads: parallel_threads
            .into_iter()
            .map(|thread| ParallelThreadInfo {
                name: thread.name,
                coordination_status: thread.coordination_status,
                current_state: thread.current_state,
            })
            .collect(),
        state: state_info,
        git_checkpoint,
        changes,
        materialized_threads,
        profile: StatusProfile::default(),
    };
    let late_state_ms = late_state_start.elapsed().as_millis();
    let advice_start = Instant::now();
    let mut output = apply_status_advice(
        repo,
        &opts,
        output,
        current_state.as_ref(),
        &thread_summary,
        import_hint,
        git_backed_mapping,
    );
    output.profile = StatusProfile {
        repo_open_ms,
        current_state_ms,
        operation_ms,
        remote_tracking_ms,
        import_hint_ms,
        git_overlay_status_ms,
        git_overlay_health_ms,
        verification_ms,
        git_index_ms,
        worktree_status_ms,
        thread_summary_ms,
        parallel_threads_ms,
        late_state_ms,
        materialized_threads_ms: materialized_ms,
        advice_ms: advice_start.elapsed().as_millis(),
        build_total_ms: body_start.elapsed().as_millis(),
        worktree_profile,
    };
    Ok(output)
}

struct ShortPathInputs<'a> {
    repo: &'a Repository,
    opts: &'a StatusOptions,
    current_state: Option<&'a State>,
    operation: Option<RepositoryOperationStatus>,
    remote_tracking: Option<GitRemoteTrackingStatus>,
    git_overlay_health: GitOverlayHealth,
    trust: RepositoryVerificationState,
    import_hint: Option<GitOverlayImportHint>,
    git_index: Option<GitIndexPlan>,
    identity_notice: Option<String>,
    changes: ChangesInfo,
    profile: StatusProfile,
}

fn build_short_path_report(input: ShortPathInputs<'_>) -> StatusReport {
    let recommended_action = effective_next_action(
        NextActionInput::default(input.operation.as_ref(), input.remote_tracking.as_ref(), None, None)
            .with_verification(&input.trust),
    );
    let worktree_clean = input.changes.is_empty();
    let recommended_action =
        first_save_recommendation(input.repo, input.current_state, worktree_clean)
            .unwrap_or(recommended_action);
    let presentation = crate::repository_presentation(input.repo, None, None);
    let recommended_action_template = (input.opts.adapters.action_template)(&recommended_action);
    StatusReport {
        output_kind: "status",
        repository_capability: input.repo.capability_label().to_string(),
        repository_label: presentation.label,
        repository_context: presentation.context,
        storage_model: input.repo.storage_model_label().to_string(),
        hosted_enabled: input.repo.hosted_enabled(),
        validation_capability: input.repo.capability(),
        git_overlay_import_hint: input.import_hint.map(Into::into),
        git_overlay_health: input.git_overlay_health,
        trust: input.trust.clone(),
        operation: input.operation,
        remote_tracking: input.remote_tracking,
        git_index: input.git_index,
        thread: None,
        base_state: None,
        base_root: None,
        current_state: None,
        path: None,
        execution_path: None,
        session_id: None,
        heddle_session_id: None,
        actor: None,
        harness: None,
        thinking_level: None,
        usage_summary: None,
        last_progress_at: None,
        report_flush_state: None,
        attach_reason: None,
        thread_mode: None,
        thread_state: None,
        freshness: None,
        target_thread: None,
        parent_thread: None,
        child_threads: Vec::new(),
        task: None,
        promotion_suggested: false,
        impact_categories: Vec::new(),
        heavy_impact_paths: Vec::new(),
        changed_paths: changes_paths(&input.changes).into_iter().collect(),
        changed_path_count: changes_path_count(&input.changes),
        worktree_changed_path_count: changes_path_count(&input.changes),
        thread_changed_path_count: 0,
        blockers: if input.trust.verified {
            Vec::new()
        } else {
            input
                .trust
                .checks
                .iter()
                .filter(|check| {
                    !check.clean
                        && check.status != "not_checked"
                        && !check
                            .summary
                            .contains("checked after the primary verification blocker")
                })
                .map(|check| format!("{}: {}", check.name, check.summary))
                .collect()
        },
        identity_notice: input.identity_notice,
        recommended_action_template,
        recommended_action,
        recovery_commands: input.trust.recovery_commands.clone(),
        recovery_action_templates: input.trust.recovery_action_templates.clone(),
        thread_health: input.trust.status.clone(),
        coordination_status: if input.trust.verified {
            CoordinationStatus::Clean
        } else {
            CoordinationStatus::Blocked
        },
        coordination_blocked_by_trust: !input.trust.verified,
        is_isolated: false,
        parallel_threads: Vec::new(),
        state: None,
        git_checkpoint: None,
        changes: input.changes,
        materialized_threads: assess_materialized_threads(input.repo),
        profile: input.profile,
    }
}

fn apply_status_advice(
    repo: &Repository,
    opts: &StatusOptions,
    output: StatusReport,
    current_state: Option<&State>,
    thread_summary: &Option<StatusThreadSummary>,
    import_hint: Option<GitOverlayImportHint>,
    git_backed_mapping: bool,
) -> StatusReport {
    let has_changes = !output.changes.is_empty();
    let checkpointed_clean = output.git_checkpoint.is_some() && !has_changes;
    let thread_stub = output.thread.as_ref().map(|thread| Thread {
        id: thread.clone(),
        thread: thread.clone(),
        target_thread: output.target_thread.clone(),
        parent_thread: thread_summary
            .as_ref()
            .and_then(|thread| thread.parent_thread.clone()),
        mode: output
            .thread_mode
            .clone()
            .unwrap_or(ThreadMode::Materialized),
        state: output.thread_state.clone().unwrap_or(ThreadState::Active),
        base_state: output.base_state.clone().unwrap_or_default(),
        base_root: output.base_root.clone().unwrap_or_default(),
        current_state: output.current_state.clone(),
        merged_state: None,
        task: output.task.clone(),
        execution_path: output
            .execution_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| repo.root().to_path_buf()),
        materialized_path: output.path.as_ref().map(PathBuf::from),
        changed_paths: thread_summary
            .as_ref()
            .map(|thread| thread.changed_paths.clone())
            .unwrap_or_default(),
        impact_categories: output.impact_categories.clone(),
        heavy_impact_paths: output.heavy_impact_paths.clone(),
        promotion_suggested: output.promotion_suggested && !checkpointed_clean,
        freshness: match output.freshness.clone().unwrap_or(ThreadFreshness::Unknown) {
            ThreadFreshness::Unknown if checkpointed_clean => ThreadFreshness::Current,
            freshness => freshness,
        },
        verification_summary: thread_summary
            .as_ref()
            .map(|thread| thread.verification_summary.clone())
            .unwrap_or_default(),
        confidence_summary: thread_summary
            .as_ref()
            .map(|thread| thread.confidence_summary.clone())
            .unwrap_or_default(),
        integration_policy_result: thread_summary
            .as_ref()
            .map(|thread| thread.integration_policy_result.clone())
            .unwrap_or_default(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        ephemeral: None,
        auto: false,
        shared_target_dir: None,
    });
    let initial_state = current_state.map(is_synthetic_root).unwrap_or(true);
    let advice = thread_stub.as_ref().map(|thread| {
        describe_thread_advice_with_initial(thread, has_changes, 0, false, initial_state)
    });
    let mut trust = output.trust.clone();
    if let Some(thread) = output.thread.as_deref()
        && !trust.recommended_action.is_empty()
    {
        let contextual = (opts.adapters.contextual_thread_action)(
            repo,
            thread,
            output.target_thread.as_deref(),
            &trust.recommended_action,
        );
        if contextual != trust.recommended_action {
            override_trust_recommended_action(&mut trust, contextual, &opts.adapters);
        }
    }
    let thread_health = advice.as_ref().map(|advice| advice.thread_health.as_str());
    let thread_action = advice
        .as_ref()
        .map(|advice| advice.recommended_action.as_str());
    let fallback = non_empty_action(thread_action)
        .or_else(|| non_empty_action(Some(trust.recommended_action.as_str())));
    let recommended_action = effective_next_action(
        NextActionInput::default(
            output.operation.as_ref(),
            output.remote_tracking.as_ref(),
            import_hint.as_ref(),
            fallback,
        )
        .current_thread(thread_health)
        .with_verification(&trust),
    );
    let recommended_action = if let Some(thread) = output.thread.as_deref() {
        (opts.adapters.contextual_thread_action)(
            repo,
            thread,
            output.target_thread.as_deref(),
            &recommended_action,
        )
    } else {
        recommended_action
    };
    if trust.verified
        && !recommended_action.is_empty()
        && trust.recommended_action != recommended_action
        && thread_recovery_action_is_primary(thread_health, &recommended_action)
    {
        override_trust_recommended_action(&mut trust, recommended_action.clone(), &opts.adapters);
    }
    let recommended_action = if git_backed_mapping {
        if has_changes {
            "heddle commit -m \"...\"".to_string()
        } else {
            String::new()
        }
    } else {
        first_save_recommendation(repo, current_state, !has_changes).unwrap_or(recommended_action)
    };
    let thread_health = if trust.verified {
        if git_backed_mapping {
            if has_changes {
                "dirty_worktree".to_string()
            } else {
                "clean".to_string()
            }
        } else {
            advice
                .as_ref()
                .map(|advice| advice.thread_health.clone())
                .unwrap_or_else(|| "clean".to_string())
        }
    } else {
        trust.status.clone()
    };
    let needs_checkpoint = trust.status == "needs_checkpoint";
    let mut trust_blockers = trust
        .checks
        .iter()
        .filter(|check| {
            !check.clean
                && check.status != "not_checked"
                && (check.name != "Clone" || check.status != "blocked")
                && !check
                    .summary
                    .contains("checked after the primary verification blocker")
        })
        .map(|check| format!("{}: {}", check.name, check.summary))
        .collect::<Vec<_>>();
    let blocked_by_trust = !trust.verified;
    if blocked_by_trust && trust_blockers.is_empty() && !trust.summary.trim().is_empty() {
        trust_blockers.push(format!("Verification: {}", trust.summary));
    }
    let display_thread_summary = (!git_backed_mapping)
        .then_some(thread_summary.as_ref())
        .flatten();
    let worktree_changed_path_count = changes_path_count(&output.changes);
    let thread_changed_path_count =
        captured_thread_path_count(display_thread_summary, &output.changes);
    let (coordination_status, coordination_blocked_by_trust) = resolve_coordination_with_trust(
        output.coordination_status,
        blocked_by_trust,
        needs_checkpoint,
    );
    let recommended_action_template = (opts.adapters.action_template)(&recommended_action);
    StatusReport {
        blockers: if blocked_by_trust {
            trust_blockers
        } else {
            advice
                .as_ref()
                .map(|advice| advice.blockers.clone())
                .unwrap_or_default()
        },
        identity_notice: output.identity_notice,
        recommended_action: recommended_action.clone(),
        recommended_action_template,
        recovery_commands: trust.recovery_commands.clone(),
        recovery_action_templates: trust.recovery_action_templates.clone(),
        thread_health,
        coordination_status,
        coordination_blocked_by_trust,
        thread_state: output.thread_state,
        changed_paths: changed_paths(display_thread_summary, &output.changes),
        changed_path_count: if trust.verified {
            changed_path_count(display_thread_summary, &output.changes)
        } else {
            changes_path_count(&output.changes)
        },
        worktree_changed_path_count,
        thread_changed_path_count,
        trust,
        ..output
    }
}

fn override_trust_recommended_action(
    trust: &mut RepositoryVerificationState,
    action: String,
    adapters: &StatusAdapters,
) {
    let template = (adapters.action_template)(&action);
    trust.recommended_action = action.clone();
    trust.recommended_action_template = template.clone();
    if let Some(check) = trust
        .checks
        .iter_mut()
        .find(|check| check.name == "Workflow")
    {
        check.recommended_action = Some(action);
        check.recommended_action_template = template;
    }
}

pub fn fast_short_status_report(start: &Path) -> Result<Option<FastShortStatusReport>> {
    let total_start = Instant::now();
    let discover_start = Instant::now();
    let git = match SleyRepository::discover(start) {
        Ok(git) => git,
        Err(_) => return Ok(None),
    };
    let Some(workdir) = git.workdir() else {
        return Ok(None);
    };
    let git_discover_ms = discover_start.elapsed().as_millis();

    let config_start = Instant::now();
    let repo_kind = fast_short_repo_kind(&workdir)?;
    if matches!(repo_kind, FastShortRepoKind::Fallback) {
        return Ok(None);
    }
    let config_ms = config_start.elapsed().as_millis();

    let status_start = Instant::now();
    let changes = fast_sley_changes(&git)?;
    let sley_status_ms = status_start.elapsed().as_millis();

    let branch_start = Instant::now();
    let branch = fast_git_branch(&git)?;
    let subject = branch.as_deref().unwrap_or("detached").to_string();
    let branch_ms = branch_start.elapsed().as_millis();

    let remote_start = Instant::now();
    let remote_health = match repo_kind {
        FastShortRepoKind::PlainGit | FastShortRepoKind::Fallback => None,
        FastShortRepoKind::GitOverlay => branch
            .as_deref()
            .map(|branch| fast_remote_health(&git, branch))
            .transpose()?
            .flatten(),
    };
    let remote_ms = remote_start.elapsed().as_millis();
    let health = if changes.is_empty() {
        match repo_kind {
            FastShortRepoKind::PlainGit => "setup needed".to_string(),
            FastShortRepoKind::GitOverlay | FastShortRepoKind::Fallback => {
                remote_health.unwrap_or("clean").to_string()
            }
        }
    } else {
        String::new()
    };
    Ok(Some(FastShortStatusReport {
        subject,
        health,
        changes,
        profile: FastShortStatusProfile {
            git_discover_ms,
            config_ms,
            sley_status_ms,
            branch_ms,
            remote_ms,
            total_ms: total_start.elapsed().as_millis(),
        },
    }))
}

enum FastShortRepoKind {
    PlainGit,
    GitOverlay,
    Fallback,
}

fn fast_short_repo_kind(workdir: &Path) -> Result<FastShortRepoKind> {
    let heddle_dir = workdir.join(".heddle");
    if !heddle_dir.exists() {
        return Ok(FastShortRepoKind::PlainGit);
    }
    if heddle_dir.join("objectstore").is_file() {
        return Ok(FastShortRepoKind::Fallback);
    }
    let config_path = heddle_dir.join("config.toml");
    if !config_path.is_file() {
        return Ok(FastShortRepoKind::Fallback);
    }
    RepoConfig::load(&config_path)?;
    Ok(FastShortRepoKind::GitOverlay)
}

fn fast_sley_changes(git: &SleyRepository) -> Result<ChangesInfo> {
    let mut changes = ChangesInfo::default();
    git.stream_short_status_with_options(
        ShortStatusOptions {
            untracked_mode: StatusUntrackedMode::All,
            ..ShortStatusOptions::default()
        },
        |entry| {
            append_fast_status_row(&mut changes, entry);
            Ok(StreamControl::Continue)
        },
    )
    .map_err(sley_error)?;
    Ok(changes)
}

fn append_fast_status_row(changes: &mut ChangesInfo, entry: ShortStatusRow<'_>) {
    let path = String::from_utf8_lossy(entry.path).into_owned();
    if path.is_empty() || ignored_git_overlay_status_path(&path) {
        return;
    }
    if entry.index == b'?' && entry.worktree == b'?' {
        changes.added.push(path);
    } else if entry.index == b'D' || entry.worktree == b'D' {
        changes.deleted.push(path);
    } else if entry.index == b'A'
        || entry.index == b'R'
        || entry.index == b'C'
        || entry.head_oid.is_none()
    {
        changes.added.push(path);
    } else {
        changes.modified.push(path);
    }
}

fn ignored_git_overlay_status_path(path: &str) -> bool {
    path == ".heddle" || path.starts_with(".heddle/")
}

fn fast_git_branch(git: &SleyRepository) -> Result<Option<String>> {
    Ok(git
        .head()
        .ok()
        .and_then(|head| head.branch_name().map(str::to_string)))
}

fn fast_remote_health(git: &SleyRepository, branch: &str) -> Result<Option<&'static str>> {
    let Some(head) = git.head().ok().and_then(|head| head.oid) else {
        return Ok(None);
    };
    if git
        .find_reference(&format!("refs/heads/{branch}"))
        .map_err(sley_error)?
        .is_some()
        && let Some(tracking_ref) = fast_configured_tracking_ref(git, branch)?
        && let Some(upstream) = fast_rev_parse(git, &tracking_ref)
    {
        return fast_remote_health_for_pair(git, head, upstream);
    }

    let remotes = git.remote_names().map_err(sley_error)?;
    for remote in &remotes {
        if remote.trim().is_empty() {
            continue;
        }
        let remote_ref = format!("refs/remotes/{remote}/{branch}");
        let Some(upstream) = fast_rev_parse(git, &remote_ref) else {
            continue;
        };
        if upstream == head {
            return Ok(None);
        }
        return fast_remote_health_for_pair(git, head, upstream);
    }

    if remotes.is_empty() {
        Ok(None)
    } else {
        Ok(Some("ready to push"))
    }
}

fn fast_configured_tracking_ref(git: &SleyRepository, branch: &str) -> Result<Option<String>> {
    let config = git.config_snapshot().map_err(sley_error)?;
    let Some(remote) = config.get("branch", Some(branch), "remote") else {
        return Ok(None);
    };
    let Some(merge) = config.get("branch", Some(branch), "merge") else {
        return Ok(None);
    };
    if remote == "." {
        return Ok(Some(merge.to_string()));
    }
    let Some(short) = merge.strip_prefix("refs/heads/") else {
        return Ok(None);
    };
    Ok(Some(format!("refs/remotes/{remote}/{short}")))
}

fn fast_rev_parse(git: &SleyRepository, rev: &str) -> Option<sley::ObjectId> {
    git.rev_parse(rev).ok()
}

fn fast_remote_health_for_pair(
    git: &SleyRepository,
    head: sley::ObjectId,
    upstream: sley::ObjectId,
) -> Result<Option<&'static str>> {
    if head == upstream {
        return Ok(None);
    }
    let db = sley::ObjectDatabase::from_git_dir(git.common_dir(), git.object_format());
    let (ahead, behind) = sley::plumbing::sley_rev::ahead_behind_counts(
        git.git_dir(),
        git.object_format(),
        &db,
        &head,
        &upstream,
    )
    .map_err(sley_error)?;
    Ok(match (ahead, behind) {
        (0, 0) => None,
        (_, 0) => Some("ready to push"),
        (0, _) => Some("behind upstream"),
        _ => Some("remote_diverged"),
    })
}

fn sley_error(err: sley::GitError) -> HeddleError {
    HeddleError::Config(err.to_string())
}

pub fn assess_materialized_threads(repo: &Repository) -> Vec<MaterializedThreadInfo> {
    let summaries = match repo::thread_manifest::list_thread_manifests(repo.heddle_dir()) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    summaries
        .into_iter()
        .map(|summary| {
            let stale = match repo.refs().get_thread(&ThreadName::new(&summary.thread)) {
                Ok(Some(head)) => head != summary.state_id,
                _ => false,
            };
            let tree_hash = summary.tree_hash.to_string();
            MaterializedThreadInfo {
                name: summary.thread,
                state_id: summary.state_id.short(),
                tree_hash_short: tree_hash[..std::cmp::min(12, tree_hash.len())].to_string(),
                file_count: summary.file_count,
                stale,
            }
        })
        .collect()
}

pub fn changes_from_worktree_status(status: &WorktreeStatus) -> ChangesInfo {
    ChangesInfo {
        modified: status
            .modified
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        added: status
            .added
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        deleted: status
            .deleted
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
    }
}

pub fn changes_path_count(changes: &ChangesInfo) -> usize {
    changes_paths(changes).len()
}

pub fn changes_paths(changes: &ChangesInfo) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    paths.extend(changes.modified.iter().cloned());
    paths.extend(changes.added.iter().cloned());
    paths.extend(changes.deleted.iter().cloned());
    paths
}

fn changed_path_count(thread: Option<&StatusThreadSummary>, changes: &ChangesInfo) -> usize {
    let mut paths = BTreeSet::new();
    if let Some(thread) = thread {
        paths.extend(thread.changed_paths.iter().cloned());
    }
    paths.extend(changes.modified.iter().cloned());
    paths.extend(changes.added.iter().cloned());
    paths.extend(changes.deleted.iter().cloned());
    paths.len()
}

fn changed_paths(thread: Option<&StatusThreadSummary>, changes: &ChangesInfo) -> Vec<String> {
    let mut paths = BTreeSet::new();
    if let Some(thread) = thread {
        paths.extend(thread.changed_paths.iter().cloned());
    }
    paths.extend(changes.modified.iter().cloned());
    paths.extend(changes.added.iter().cloned());
    paths.extend(changes.deleted.iter().cloned());
    paths.into_iter().collect()
}

fn captured_thread_path_count(
    thread: Option<&StatusThreadSummary>,
    changes: &ChangesInfo,
) -> usize {
    let Some(thread) = thread else {
        return 0;
    };
    let dirty_paths = changes_paths(changes);
    thread
        .changed_paths
        .iter()
        .filter(|path| !dirty_paths.contains(*path))
        .count()
}

fn first_save_recommendation(
    repo: &Repository,
    current_state: Option<&State>,
    worktree_clean: bool,
) -> Option<String> {
    if !worktree_clean || repo.capability() != RepositoryCapability::NativeHeddle {
        return None;
    }
    let empty_log = current_state.map(is_synthetic_root).unwrap_or(true);
    empty_log.then(|| "heddle commit -m \"...\"".to_string())
}

fn remote_tracking_with_verification_action(
    mut remote: GitRemoteTrackingStatus,
    trust: &RepositoryVerificationState,
) -> GitRemoteTrackingStatus {
    let remote_status = remote_tracking_status(&remote);
    if trust.status == remote_status && !trust.recommended_action.trim().is_empty() {
        remote.next_action = trust.recommended_action.clone();
    }
    remote
}

fn resolve_coordination_with_trust(
    pre_override: CoordinationStatus,
    blocked_by_trust: bool,
    needs_checkpoint: bool,
) -> (CoordinationStatus, bool) {
    let pre_override_clean = coordination_axis_clean(&pre_override, false);
    let trust_override = blocked_by_trust && !needs_checkpoint;
    let mask_as_trust = trust_override && pre_override_clean;
    let coordination_status = if mask_as_trust {
        CoordinationStatus::Blocked
    } else {
        pre_override
    };
    (coordination_status, mask_as_trust)
}

fn coordination_axis_clean(coordination: &CoordinationStatus, blocked_by_trust: bool) -> bool {
    match coordination {
        CoordinationStatus::Clean => true,
        CoordinationStatus::Blocked => blocked_by_trust,
        CoordinationStatus::Ahead
        | CoordinationStatus::Diverged
        | CoordinationStatus::MergeReady => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slow_path_bucket(row: &ShortStatusRow<'_>) -> &'static str {
        if row.index == b'?' && row.worktree == b'?' {
            "added"
        } else if row.index == b'D' || row.worktree == b'D' {
            "deleted"
        } else if row.index == b'A'
            || row.index == b'R'
            || row.index == b'C'
            || row.head_oid.is_none()
        {
            "added"
        } else {
            "modified"
        }
    }

    fn fast_path_bucket(row: ShortStatusRow<'_>) -> &'static str {
        let mut changes = ChangesInfo::default();
        append_fast_status_row(&mut changes, row);
        match (
            changes.added.len(),
            changes.deleted.len(),
            changes.modified.len(),
        ) {
            (1, 0, 0) => "added",
            (0, 1, 0) => "deleted",
            (0, 0, 1) => "modified",
            other => panic!("fast path produced unexpected bucket counts: {other:?}"),
        }
    }

    fn status_row<'a>(
        index: u8,
        worktree: u8,
        path: &'a [u8],
        in_head: bool,
    ) -> ShortStatusRow<'a> {
        ShortStatusRow {
            index,
            worktree,
            path,
            head_mode: None,
            index_mode: None,
            worktree_mode: None,
            head_oid: in_head.then(|| sley::ObjectId::null(sley::ObjectFormat::Sha1)),
            index_oid: None,
            submodule: None,
        }
    }

    #[test]
    fn fast_short_status_agrees_with_slow_path_on_ad_rename_copy() {
        let cases: &[(u8, u8, bool, &str)] = &[
            (b'A', b'D', false, "AD: staged-add then worktree-deleted"),
            (b'R', b' ', true, "R: renamed"),
            (b'C', b' ', true, "C: copied"),
            (b'A', b' ', false, "A: staged add"),
            (b'M', b' ', true, "M: modified"),
            (b' ', b'M', true, "worktree-modified"),
            (b'D', b' ', true, "D: staged delete"),
            (b' ', b'D', true, "worktree delete"),
            (b'?', b'?', false, "untracked"),
        ];
        for &(index, worktree, in_head, label) in cases {
            let path = label.as_bytes();
            let fast = fast_path_bucket(status_row(index, worktree, path, in_head));
            let slow = slow_path_bucket(&status_row(index, worktree, path, in_head));
            assert_eq!(
                fast, slow,
                "fast and slow short-status classification disagree for {label}",
            );
        }
    }
}
