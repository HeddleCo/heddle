// SPDX-License-Identifier: Apache-2.0
//! Shared save primitive for `capture` / `commit` / `checkpoint` / ready auto-capture.
//!
//! CLI verbs become thin shells that build a [`SavePlan`] and call
//! [`execute_save`]. Repo keeps atomic tree/state mutation; this module owns
//! the composition of preflight-adjacent routing, Heddle snapshot, and
//! optional Git-overlay write-through.

use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use heddle_git_projection::{GitProjection, WriteThroughOutcome};
use objects::{
    HeddleError, RecoveryDetails,
    lock::RepositoryLockExt,
    object::{Agent, Attribution, ContentHash, Principal, State, StateId, ThreadName, Tree},
    store::ObjectStore,
    worktree::WorktreeStatus,
};
use oplog::{OpLogBackend, OpRecord};
use refs::Head;
use repo::{
    ActorPresenceStore, CommitGraphIndex, GitCheckpointRecord, Hook, HookContext, HookManager,
    OperationScope, Repository, RepositoryCapability, SnapshotProfile, Thread, ThreadFreshness,
    ThreadIntegrationPolicy, ThreadManager, ThreadMode, ThreadState, WorktreeStateLookupProfile,
    WorktreeStatusOptions, refresh_active_thread_metadata, update_thread_state_from_state,
};
use schemars::JsonSchema;
use serde::Serialize;
use sley::Repository as SleyRepository;

use crate::{
    ActionTemplate, ExecutionContext, HeddleReport, MachineContractInput, MachineOutputKind,
    OutputDiscriminator, ReportContract, RepositoryVerificationState,
    build_repository_verification_health_with_worktree_status, build_repository_verification_state,
    build_repository_verification_state_with_machine_contract,
    build_repository_verification_state_with_worktree_status,
    build_repository_verification_state_with_worktree_status_and_machine_contract,
    schema_for_report,
    status::next_action::{contextual_thread_action, import_guidance_includes_active_branch},
    verify::action_template,
};

const BULK_CAPTURE_WARNING_THRESHOLD: usize = 500;

/// Fully-resolved inputs for the normal local capture operation.
///
/// Attribution remains an embedding concern because the CLI combines explicit
/// flags, harness detection, sessions, and environment variables. [`capture`]
/// resolves it lazily after non-mutating safety checks, then owns the remaining
/// mutation ordering.
#[derive(Debug)]
pub struct CaptureOptions {
    pub intent: String,
    pub confidence: Option<f32>,
    pub force: bool,
    pub worktree_status_options: WorktreeStatusOptions,
    pub machine_contract_input: Option<MachineContractInput>,
}

/// Attribution resolved lazily after capture's non-mutating safety checks.
#[derive(Debug, Clone)]
pub struct CaptureAttribution {
    pub attribution: Attribution,
    pub principal_source: String,
    /// Native harness session from the workspace identity stamp. This remains
    /// distinct from Heddle `Session.id` and only advances the last-turn cursor.
    pub harness_session_id: Option<String>,
}

/// Capture-specific timings owned by the operation implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureProfile {
    pub worktree_status_ms: u128,
    pub preflight_ms: u128,
    pub attribution_ms: u128,
    pub execute_save_ms: u128,
}

/// Final semantic report returned by the capture seam.
///
/// The CLI may project this into its stable JSON wire type or render it for a
/// person, but it must not add recovery state or derive workflow actions.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CaptureReport {
    pub output_kind: &'static str,
    pub state_id: String,
    pub content_hash: String,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub task_assignment_id: Option<String>,
    pub principal: CapturePrincipalReport,
    pub principal_source: String,
    pub agent: Option<CaptureAgentReport>,
    pub promotion_suggested: bool,
    pub heavy_impact_paths: Vec<String>,
    pub captured_path_count: usize,
    pub warnings: Vec<String>,
    pub signed: bool,
    pub message: String,
    pub recommended_action: Option<String>,
    pub recommended_action_template: Option<ActionTemplate>,
    pub verification: RepositoryVerificationState,
    #[serde(skip)]
    #[schemars(skip)]
    pub captured_thread_targets_integration: bool,
    #[serde(skip)]
    #[schemars(skip)]
    pub diagnostics: CaptureDiagnostics,
}

impl CaptureReport {
    pub const CONTRACT: ReportContract = ReportContract {
        schema_name: "capture",
        machine_output_kind: MachineOutputKind::Json,
        output_discriminator: Some(OutputDiscriminator {
            field: "output_kind",
            value: "capture",
        }),
        schema: schema_for_report::<CaptureReport>,
    };
}

impl HeddleReport for CaptureReport {
    const CONTRACT: ReportContract = CaptureReport::CONTRACT;
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct CapturePrincipalReport {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct CaptureAgentReport {
    pub provider: String,
    pub model: String,
    pub session_id: Option<String>,
    pub segment_id: Option<String>,
    pub policy_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CaptureDiagnostics {
    pub save: SaveReport,
    pub profile: CaptureProfile,
}

/// How far a save should write through into Git (Git-overlay only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitScope {
    /// Heddle state only — no Git checkpoint (capture; native commit).
    None,
    /// Checkpoint the staged Git index boundary (caller supplies the tree).
    Staged,
    /// Capture/checkpoint the full worktree (or current clean state).
    WorktreeAll,
}

/// Public CLI / facade verb that requested the save.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveVerb {
    Capture,
    Commit,
    Checkpoint,
}

/// Inputs for [`execute_save`]. Attribution is resolved by the caller so CLI
/// env/harness/agent precedence stays at the embedding surface.
#[derive(Debug)]
pub struct SavePlan {
    pub verb: SaveVerb,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub attribution: Attribution,
    pub git_scope: GitScope,
    /// When set, snapshot this tree instead of walking the worktree
    /// (staged-index commits).
    pub supplied_tree: Option<Tree>,
    /// Prefer the current HEAD state when present (checkpoint bootstrap path).
    pub reuse_current_state: bool,
    /// After ensuring state, refuse dirty Heddle worktree before Git write-through.
    pub require_clean_worktree: bool,
    /// Refuse a worktree snapshot whose tree is identical to the current state.
    /// The comparison happens during the snapshot tree build, avoiding a
    /// separate preflight walk.
    pub require_worktree_change: bool,
    pub worktree_status_options: WorktreeStatusOptions,
    /// Authoritative parent-relative paths found by capture preflight. The
    /// snapshot builder consumes these before the monitor cursor advances so
    /// it can rewrite only the affected leaf-to-root chain.
    pub known_worktree_changes: Option<WorktreeStatus>,
    /// Run pre/post snapshot hooks when creating a new Heddle state.
    pub run_hooks: bool,
    /// Map post-verify "commit" next actions to `heddle status` (commit UX).
    pub commit_safe_post_verify: bool,
    /// Fold snapshot + GitCheckpoint oplog batches into one undo unit.
    pub coalesce_snapshot_and_checkpoint: bool,
    /// Export an unmapped checkpoint state on top of the checkout's current
    /// Git tip. Used only by sequential multi-peer land.
    pub linearize_git_parent: bool,
    /// Optional precomputed git-overlay worktree status for verification reuse
    /// on the no-new-state path. Post-mutation paths always recompute.
    pub precomputed_worktree_status:
        Option<repo::Result<Option<objects::worktree::WorktreeStatus>>>,
    /// Optional embedding-surface machine-contract inventory. Passing it into
    /// core verification lets callers reuse the post-save proof instead of
    /// rebuilding the entire repository health envelope for presentation.
    pub machine_contract_input: Option<MachineContractInput>,
}

impl SavePlan {
    pub fn capture(intent: impl Into<String>, attribution: Attribution) -> Self {
        Self {
            verb: SaveVerb::Capture,
            intent: Some(intent.into()),
            confidence: None,
            attribution,
            git_scope: GitScope::None,
            supplied_tree: None,
            reuse_current_state: false,
            require_clean_worktree: false,
            require_worktree_change: false,
            worktree_status_options: WorktreeStatusOptions::default(),
            known_worktree_changes: None,
            run_hooks: true,
            commit_safe_post_verify: false,
            coalesce_snapshot_and_checkpoint: false,
            linearize_git_parent: false,
            precomputed_worktree_status: None,
            machine_contract_input: None,
        }
    }

    pub fn commit(
        intent: impl Into<String>,
        attribution: Attribution,
        git_scope: GitScope,
    ) -> Self {
        Self {
            verb: SaveVerb::Commit,
            intent: Some(intent.into()),
            confidence: None,
            attribution,
            git_scope,
            supplied_tree: None,
            reuse_current_state: false,
            require_clean_worktree: matches!(git_scope, GitScope::WorktreeAll),
            require_worktree_change: false,
            worktree_status_options: WorktreeStatusOptions::default(),
            known_worktree_changes: None,
            run_hooks: true,
            commit_safe_post_verify: true,
            coalesce_snapshot_and_checkpoint: matches!(
                git_scope,
                GitScope::Staged | GitScope::WorktreeAll
            ),
            linearize_git_parent: false,
            precomputed_worktree_status: None,
            machine_contract_input: None,
        }
    }

    pub fn checkpoint(message: Option<String>, attribution: Attribution, staged: bool) -> Self {
        Self {
            verb: SaveVerb::Checkpoint,
            intent: message,
            confidence: None,
            attribution,
            git_scope: if staged {
                GitScope::Staged
            } else {
                GitScope::WorktreeAll
            },
            supplied_tree: None,
            reuse_current_state: true,
            require_clean_worktree: !staged,
            require_worktree_change: false,
            worktree_status_options: WorktreeStatusOptions::default(),
            known_worktree_changes: None,
            run_hooks: true,
            commit_safe_post_verify: false,
            coalesce_snapshot_and_checkpoint: false,
            linearize_git_parent: false,
            precomputed_worktree_status: None,
            machine_contract_input: None,
        }
    }

    pub fn with_confidence(mut self, confidence: Option<f32>) -> Self {
        self.confidence = confidence;
        self
    }

    pub fn with_supplied_tree(mut self, tree: Tree) -> Self {
        self.supplied_tree = Some(tree);
        self
    }

    pub fn with_worktree_status_options(mut self, options: WorktreeStatusOptions) -> Self {
        self.worktree_status_options = options;
        self
    }

    pub fn with_precomputed_worktree_status(
        mut self,
        status: repo::Result<Option<objects::worktree::WorktreeStatus>>,
    ) -> Self {
        self.precomputed_worktree_status = Some(status);
        self
    }
}

/// Result of a successful save.
#[derive(Debug, Clone)]
pub struct SaveReport {
    pub verb: SaveVerb,
    pub state_id: StateId,
    pub content_hash: ContentHash,
    pub intent: Option<String>,
    pub confidence: Option<f32>,
    pub signed: bool,
    pub git_commit: Option<String>,
    pub git_previous_commit: Option<String>,
    pub summary: String,
    pub principal: Principal,
    pub agent: Option<Agent>,
    pub promotion_suggested: bool,
    pub heavy_impact_paths: Vec<String>,
    /// Number of paths changed by this save relative to the state that was
    /// current when the operation began.
    pub captured_path_count: usize,
    pub verification: RepositoryVerificationState,
    pub created_new_state: bool,
    pub git_checkpoint: Option<GitCheckpointRecord>,
    pub snapshot_profile: SnapshotProfile,
    pub state_create_ms: u128,
    pub captured_path_count_ms: u128,
    pub post_verification_ms: u128,
    pub thread_metadata_ms: u128,
    pub previous_state_ms: u128,
    pub previous_state_profile: WorktreeStateLookupProfile,
    pub signature_lookup_ms: u128,
}

/// Pure routing helper: which Git write-through scope a verb should use.
///
/// Used by unit tests and by CLI shells that build a [`SavePlan`] before
/// calling [`execute_save`].
pub fn plan_git_scope(
    verb: SaveVerb,
    capability: RepositoryCapability,
    staged_index_paths: bool,
    include_all_worktree: bool,
) -> GitScope {
    match verb {
        SaveVerb::Capture => GitScope::None,
        SaveVerb::Checkpoint => {
            if staged_index_paths {
                GitScope::Staged
            } else {
                GitScope::WorktreeAll
            }
        }
        SaveVerb::Commit => {
            if capability != RepositoryCapability::GitOverlay {
                GitScope::None
            } else if staged_index_paths && !include_all_worktree {
                GitScope::Staged
            } else {
                GitScope::WorktreeAll
            }
        }
    }
}

/// Whether this plan should create a new Heddle state (vs reusing HEAD).
pub fn plan_creates_new_state(plan: &SavePlan, has_current_state: bool) -> bool {
    if plan.supplied_tree.is_some() {
        return true;
    }
    if plan.reuse_current_state && has_current_state {
        return false;
    }
    // Checkpoint without current state still bootstraps a capture.
    if plan.verb == SaveVerb::Checkpoint && has_current_state {
        return false;
    }
    true
}

/// Whether this plan should perform a Git-overlay write-through.
pub fn plan_writes_git_checkpoint(plan: &SavePlan, capability: RepositoryCapability) -> bool {
    plan.git_scope != GitScope::None && capability == RepositoryCapability::GitOverlay
}

/// Leaf path component for Git index → Heddle tree entry names.
pub fn tree_leaf_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Next-action after a git-projection commit from verification facts only.
///
/// Precedence: explicit trust recommendation → verify when untrusted → push
/// when a default remote is configured.
pub fn commit_next_action_from_trust(
    recommended_action: &str,
    verified: bool,
    has_default_remote: bool,
) -> Option<String> {
    if !recommended_action.trim().is_empty() {
        return Some(recommended_action.to_string());
    }
    if !verified {
        return Some("heddle verify".to_string());
    }
    has_default_remote.then(|| "heddle push".to_string())
}

// ---------------------------------------------------------------------------
// Git-projection commit index planning (pure)
// ---------------------------------------------------------------------------

/// Pure commit index plan for internal Git projection writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitGitIndexPlan {
    pub commit_mode: &'static str,
    pub has_staged_changes: bool,
    pub staged_paths: Vec<String>,
    pub unstaged_paths: Vec<String>,
    pub untracked_paths: Vec<String>,
    pub will_commit: Vec<String>,
    pub preserved_after_commit: Vec<String>,
}

/// Split `unstaged: ` / `untracked: ` prefixed extra paths from status rows.
pub fn split_git_extra_paths(extra_paths: &[String]) -> (Vec<String>, Vec<String>) {
    let mut unstaged_paths = Vec::new();
    let mut untracked_paths = Vec::new();
    for path in extra_paths {
        if let Some(path) = path.strip_prefix("unstaged: ") {
            unstaged_paths.push(path.to_string());
        } else if let Some(path) = path.strip_prefix("untracked: ") {
            untracked_paths.push(path.to_string());
        }
    }
    (unstaged_paths, untracked_paths)
}

/// Plan commit scope from staged + extra worktree paths and `--all`.
pub fn plan_commit_git_index(
    staged_paths: &[String],
    extra_paths: &[String],
    include_all: bool,
) -> CommitGitIndexPlan {
    let (unstaged_paths, untracked_paths) = split_git_extra_paths(extra_paths);
    let has_staged_changes = !staged_paths.is_empty();
    let mut will_commit = Vec::new();
    if has_staged_changes {
        will_commit.extend(staged_paths.iter().cloned());
    }
    if include_all || !has_staged_changes {
        will_commit.extend(unstaged_paths.iter().cloned());
        will_commit.extend(untracked_paths.iter().cloned());
    }
    let commit_mode = if has_staged_changes && include_all {
        "worktree_all_explicit"
    } else if has_staged_changes {
        "staged_index"
    } else if will_commit.is_empty() {
        "none"
    } else {
        "worktree_all"
    };
    let preserved_after_commit = if has_staged_changes && !include_all {
        extra_paths.to_vec()
    } else {
        Vec::new()
    };
    CommitGitIndexPlan {
        commit_mode,
        has_staged_changes,
        staged_paths: staged_paths.to_vec(),
        unstaged_paths,
        untracked_paths,
        will_commit,
        preserved_after_commit,
    }
}

/// Index-only plan: commit staged paths, preserve all extras.
pub fn plan_commit_git_index_only(
    staged_paths: &[String],
    extra_paths: &[String],
) -> CommitGitIndexPlan {
    let (unstaged_paths, untracked_paths) = split_git_extra_paths(extra_paths);
    CommitGitIndexPlan {
        commit_mode: "staged_index",
        has_staged_changes: !staged_paths.is_empty(),
        staged_paths: staged_paths.to_vec(),
        unstaged_paths,
        untracked_paths,
        will_commit: staged_paths.to_vec(),
        preserved_after_commit: extra_paths.to_vec(),
    }
}

/// Human scope line for git-projection commit text mode.
pub fn commit_scope_text(commit_mode: &str) -> &'static str {
    match commit_mode {
        "staged_index" => {
            "staged Git index only; unstaged and untracked paths stay in the worktree"
        }
        "worktree_all_explicit" => "all staged, unstaged, and untracked worktree changes (--all)",
        "worktree_all" => "all unstaged and untracked worktree changes",
        "none" => "no Git paths",
        _ => "Git worktree changes",
    }
}

/// Annotate a commit summary when staged-only commit leaves extras behind.
pub fn staged_commit_summary(
    summary: &str,
    staged_path_count: usize,
    extra_path_count: usize,
) -> String {
    if extra_path_count == 0 {
        return summary.to_string();
    }
    format!(
        "{summary} (committed {staged_path_count} staged path(s); left {extra_path_count} unstaged/untracked path(s) in the worktree)"
    )
}

/// Capture the current worktree as one synchronous local operation.
///
/// The implementation owns the complete semantic sequence: authority-aware
/// worktree checks, safety preflight, Heddle mutation, manual-resolution
/// completion, best-effort intent-to-add maintenance, and final report
/// assembly. There are no genuine suspension points on this path.
pub fn capture(
    ctx: &ExecutionContext,
    options: CaptureOptions,
    resolve_attribution: impl FnOnce(&Repository) -> Result<CaptureAttribution>,
) -> Result<CaptureReport> {
    let repo = ctx.require_repo()?;
    if options.intent.trim().is_empty() {
        return Err(capture_refusal(
            "missing_capture_intent",
            "refusing to capture without an intent",
            "Provide a short intent with `heddle capture -m \"...\"`.",
            "no capture intent was supplied with -m/--message/--intent",
            "capturing without intent would create a weak provenance record",
            "repository state, refs, metadata, and worktree files were left unchanged",
            vec!["heddle capture -m \"...\"".to_string()],
        ));
    }

    preflight_unimported_git_history(repo, "capture")?;
    let complete_thread_resolution = merge_resolution_is_complete(repo)?;
    let worktree_status_started = Instant::now();
    let known_worktree_changes = if complete_thread_resolution {
        None
    } else {
        let status = capture_worktree_status(repo, &options.worktree_status_options)?;
        if repo.capability() == RepositoryCapability::GitOverlay && status.is_clean() {
            return Err(map_capture_error(anyhow!(HeddleError::NoChanges)));
        }
        Some(status)
    };

    let worktree_status = repo.git_overlay_worktree_status();
    let worktree_status_ms = worktree_status_started.elapsed().as_millis();

    let preflight_started = Instant::now();
    preflight_large_capture(options.force, &worktree_status)?;
    preflight_capture_mutation(
        repo,
        &worktree_status,
        options.machine_contract_input.as_ref(),
    )?;
    let preflight_ms = preflight_started.elapsed().as_millis();
    let attribution_started = Instant::now();
    let resolved_attribution = resolve_attribution(repo)?;
    let harness_session_id = resolved_attribution
        .attribution
        .agent
        .is_some()
        .then(|| resolved_attribution.harness_session_id.clone())
        .flatten();
    let attribution_ms = attribution_started.elapsed().as_millis();

    let plan = SavePlan {
        verb: SaveVerb::Capture,
        intent: Some(options.intent),
        confidence: options.confidence,
        attribution: resolved_attribution.attribution,
        git_scope: GitScope::None,
        supplied_tree: None,
        reuse_current_state: false,
        require_clean_worktree: false,
        // Native repositories compare the built tree with their Heddle HEAD.
        // Git-overlay performs its distinct authority-aware comparison above:
        // an overlay can legitimately have no Heddle HEAD yet and still need
        // to capture an empty tree that represents deletion from Git's base.
        require_worktree_change: repo.capability() == RepositoryCapability::NativeHeddle
            && !complete_thread_resolution,
        worktree_status_options: options.worktree_status_options,
        known_worktree_changes,
        run_hooks: true,
        commit_safe_post_verify: false,
        coalesce_snapshot_and_checkpoint: false,
        linearize_git_parent: false,
        precomputed_worktree_status: Some(worktree_status),
        machine_contract_input: options.machine_contract_input,
    };
    let execute_save_started = Instant::now();
    let save = execute_save(repo, plan).map_err(map_capture_error)?;
    let execute_save_ms = execute_save_started.elapsed().as_millis();
    if let Some(session_id) = harness_session_id
        && let Err(error) = crate::record_last_turn_capture(repo, &session_id, save.state_id)
    {
        tracing::warn!(%error, "could not update reconstructible last-turn anchor");
    }

    let manual_resolution_action = if complete_thread_resolution {
        complete_current_thread_manual_resolution(repo)?
    } else {
        None
    };
    update_capture_intent_to_add(repo, &save.state_id);

    let current_thread = current_thread(repo)?;
    let captured_thread_targets_integration = current_thread
        .as_ref()
        .and_then(|thread| thread.target_thread.as_ref())
        .is_some();
    let task_assignment_id = active_task_assignment_id(repo, current_thread.as_ref())?;
    let principal_source = resolved_attribution.principal_source;
    let warnings = bulk_capture_warning(save.captured_path_count)
        .into_iter()
        .collect();

    let mut recommended_action = non_empty_action(&save.verification.recommended_action);
    let mut recommended_action_template = recommended_action
        .as_deref()
        .and_then(action_template)
        .or_else(|| save.verification.recommended_action_template.clone());
    if let Some(action) = manual_resolution_action {
        recommended_action_template = action_template(&action);
        recommended_action = Some(action);
    }

    let principal = CapturePrincipalReport {
        name: save.principal.name_lossy().into_owned(),
        email: save.principal.email_lossy().into_owned(),
    };
    let agent = save.agent.as_ref().map(|agent| CaptureAgentReport {
        provider: agent.provider.clone(),
        model: agent.model.clone(),
        session_id: agent.session_id.clone(),
        segment_id: agent.segment_id.clone(),
        policy_id: agent.policy_id.clone(),
        thought_level: agent.thought_level.clone(),
        parent: agent.parent.clone(),
    });
    Ok(CaptureReport {
        output_kind: "capture",
        state_id: save.state_id.short(),
        content_hash: save.content_hash.short(),
        intent: save.intent.clone(),
        confidence: save.confidence,
        task_assignment_id,
        principal,
        principal_source,
        agent,
        promotion_suggested: save.promotion_suggested,
        heavy_impact_paths: save.heavy_impact_paths.clone(),
        captured_path_count: save.captured_path_count,
        warnings,
        signed: save.signed,
        message: save.summary.clone(),
        recommended_action,
        recommended_action_template,
        verification: save.verification.clone(),
        captured_thread_targets_integration,
        diagnostics: CaptureDiagnostics {
            save,
            profile: CaptureProfile {
                worktree_status_ms,
                preflight_ms,
                attribution_ms,
                execute_save_ms,
            },
        },
    })
}

fn preflight_unimported_git_history(repo: &Repository, action: &str) -> Result<()> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Ok(());
    }
    let Some(guidance) = repo.git_import_guidance()? else {
        return Ok(());
    };
    if !import_guidance_includes_active_branch(&guidance) {
        return Ok(());
    }
    let branches = preview_paths(&guidance.missing_branches);
    let command = guidance.recommended_command;
    Err(capture_refusal(
        "git_history_needs_import",
        format!("Refusing to {action}: Git history has not been imported into Heddle"),
        format!("Run `{command}` before retrying `heddle {action}`."),
        format!("Git branch(es) waiting for Heddle import: {branches}"),
        format!(
            "{action} would write new Heddle state before Heddle has adopted the existing Git history"
        ),
        "Git refs, Heddle refs, and worktree files were left unchanged",
        vec![command],
    ))
}

fn preflight_capture_mutation(
    repo: &Repository,
    worktree_status: &repo::Result<Option<WorktreeStatus>>,
    machine_contract_input: Option<&MachineContractInput>,
) -> Result<()> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Ok(());
    }
    if let Some(operation) = repo.operation_status()?
        && matches!(operation.scope, OperationScope::Git)
    {
        return Err(capture_refusal(
            "raw_git_operation_in_progress",
            format!(
                "Refusing to capture: an externally-started Git {} is in progress",
                operation.kind
            ),
            format!(
                "Inspect with `heddle verify`. Heddle did not start this raw Git {}, so finish or abort it with the Git-compatible tool that started it, then run `heddle verify` for the exact adoption command before retrying `heddle capture`.",
                operation.kind
            ),
            format!(
                "Git {} is {}; Heddle cannot safely turn sequencer state into a saved change inside the no-git runtime",
                operation.kind, operation.state
            ),
            "capture would capture worktree/index contents while Git still has unresolved sequencer metadata",
            "Git refs, Git sequencer files, Heddle refs, and worktree files were left unchanged",
            vec!["heddle verify".to_string()],
        ));
    }

    let health = build_repository_verification_health_with_worktree_status(repo, worktree_status);
    let trust = if let Some(input) = machine_contract_input {
        build_repository_verification_state_with_worktree_status_and_machine_contract(
            repo,
            health,
            worktree_status,
            input,
        )
    } else {
        build_repository_verification_state_with_worktree_status(repo, health, worktree_status)
    };
    if trust.status != "needs_reconcile" || uncheckpointed_state_is_ahead_of_git(repo)? {
        return Ok(());
    }
    let primary = if trust.recommended_action.trim().is_empty() {
        "heddle verify".to_string()
    } else {
        trust.recommended_action.clone()
    };
    let recovery_commands = if trust.recovery_commands.is_empty() {
        vec![primary.clone()]
    } else {
        trust.recovery_commands.clone()
    };
    Err(capture_refusal(
        "repository_verification_blocked",
        format!(
            "Refusing to capture: repository verification is blocked ({})",
            trust.status
        ),
        format!("Run `{primary}` before retrying `heddle capture`."),
        format!(
            "repository verification status is {}: {}",
            trust.status, trust.summary
        ),
        "capture would write new Heddle or Git state while Git and Heddle disagree",
        "Git refs, Heddle refs, Git checkpoint metadata, and worktree files were left unchanged",
        recovery_commands,
    ))
}

fn uncheckpointed_state_is_ahead_of_git(repo: &Repository) -> Result<bool> {
    let Some(branch) = repo.git_overlay_current_branch()? else {
        return Ok(false);
    };
    let Some(tip) = repo.git_overlay_branch_tip(&branch)? else {
        return Ok(false);
    };
    let Some(mapped) = tip.mapped_state else {
        return Ok(false);
    };
    let Some(current) = repo.current_state()? else {
        return Ok(false);
    };
    if mapped == current.state_id {
        return Ok(false);
    }
    let mut graph = CommitGraphIndex::new(repo);
    if !graph
        .is_ancestor(&mapped, &current.state_id)
        .unwrap_or(false)
        || graph
            .is_ancestor(&current.state_id, &mapped)
            .unwrap_or(false)
    {
        return Ok(false);
    }
    Ok(repo
        .latest_git_checkpoint_for_state(&current.state_id)?
        .is_none())
}

fn preflight_large_capture(
    force: bool,
    worktree_status: &repo::Result<Option<WorktreeStatus>>,
) -> Result<()> {
    if force {
        return Ok(());
    }
    let Ok(Some(status)) = worktree_status else {
        return Ok(());
    };
    let total = status.change_count();
    let delete_count = status.deleted.len();
    let add_count = status.added.len();
    if !crate::large_capture_requires_force(total, delete_count, add_count) {
        return Ok(());
    }
    let sample = status
        .deleted
        .iter()
        .chain(status.added.iter())
        .chain(status.modified.iter())
        .take(5)
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let sample = if sample.is_empty() {
        "no sample paths available".to_string()
    } else {
        sample
    };
    Err(capture_refusal(
        "large_capture_requires_force",
        format!(
            "Large capture safety check: this would capture {total} changed paths ({delete_count} deletions, {add_count} additions)"
        ),
        "If this is intentional, rerun with `heddle capture --force -m \"...\"`.",
        format!("sample changed paths: {sample}"),
        "capture would preserve an unusually large Git-overlay worktree change without an explicit confirmation",
        "repository state, refs, metadata, and worktree files were left unchanged",
        vec!["heddle capture --force -m \"...\"".to_string()],
    ))
}

fn merge_resolution_is_complete(repo: &Repository) -> Result<bool> {
    Ok(repo
        .merge_state_manager()
        .load()?
        .is_some_and(|merge_state| {
            merge_state
                .conflicts
                .iter()
                .all(|path| merge_state.resolved.contains(path))
        }))
}

/// Compare against Heddle's current tree before asking Git for its index-based
/// status. These are distinct authorities: Sley's Git status cannot prime
/// Heddle's persisted worktree index, which the following snapshot consumes.
/// In particular, retaining this check prevents a fast forced retry after a
/// refused directory deletion from being misclassified as an unchanged tree.
fn capture_worktree_status(
    repo: &Repository,
    options: &WorktreeStatusOptions,
) -> Result<WorktreeStatus> {
    if repo.current_state_for_worktree_status()?.is_none()
        && let Some(status) = repo.git_overlay_worktree_status()?
    {
        return Ok(status);
    }
    let tree = match repo.current_state_for_worktree_status()? {
        Some(state) => repo.require_tree_for_worktree_status(&state.tree)?,
        None => Tree::new(),
    };
    Ok(repo.compare_worktree_cached_with_options(&tree, options)?)
}

/// Complete a captured manual resolution and return its contextual land action.
///
/// This is shared by capture and the operator continuation path so the thread
/// metadata/ref/oplog transaction has one implementation.
pub fn complete_current_thread_manual_resolution(repo: &Repository) -> Result<Option<String>> {
    let Some(current_thread) = repo.current_lane()? else {
        return Ok(None);
    };
    let Some(current_state) = repo.head()? else {
        return Ok(None);
    };
    let Some(current_state_object) = repo.store().get_state(&current_state)? else {
        return Ok(None);
    };
    let manager = ThreadManager::new(repo.heddle_dir());
    let Some(mut thread) = manager.find_by_thread(&current_thread)? else {
        return Ok(None);
    };
    let Some(target_thread) = thread.target_thread.clone() else {
        return Ok(None);
    };
    let Some(target_state) = repo.refs().get_thread(&ThreadName::new(&target_thread))? else {
        return Ok(None);
    };
    let Some(target_state_object) = repo.store().get_state(&target_state)? else {
        return Ok(None);
    };
    let before = crate::capture_thread_update_before(repo, &manager, &thread)?;

    thread.base_state = target_state.short();
    thread.base_root = target_state_object.tree.short();
    update_thread_state_from_state(&mut thread, &current_state_object);
    thread.state = ThreadState::Ready;
    thread.freshness = ThreadFreshness::Current;
    thread.integration_policy_result = ThreadIntegrationPolicy {
        status: Some("manual_resolved".to_string()),
        reason: Some("manual conflict resolution captured".to_string()),
        manual_resolution_state: Some(current_state.short()),
        conflicts_resolved_manually: true,
    };
    thread.updated_at = Utc::now();
    let thread_id = thread.id.clone();
    let target = thread.target_thread.clone();
    crate::save_thread_update(repo, &manager, &thread, before, current_state)?;

    Ok(Some(manual_resolution_land_action(
        repo,
        &thread_id,
        target.as_deref(),
    )))
}

fn manual_resolution_land_action(
    repo: &Repository,
    thread_id: &str,
    target_thread: Option<&str>,
) -> String {
    let action = crate::status::next_action::land_local_command(thread_id);
    contextual_thread_action(repo, thread_id, target_thread, &action)
}

fn update_capture_intent_to_add(repo: &Repository, state_id: &StateId) {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return;
    }
    let projection = GitProjection::new(repo);
    if let Err(error) = projection.update_intent_to_add(state_id) {
        tracing::debug!(%error, "intent-to-add index update skipped");
    }
}

fn current_thread(repo: &Repository) -> Result<Option<Thread>> {
    let manager = ThreadManager::new(repo.heddle_dir());
    if let Some(thread) = manager.find_by_execution_root(repo.root())? {
        return Ok(Some(thread));
    }
    let Head::Attached { thread } = repo.head_ref()? else {
        return Ok(None);
    };
    let current_state_id = repo.refs().get_thread(&thread)?;
    let current_state = current_state_id.map(|state| state.short());
    let base_root = current_state_id
        .and_then(|state| repo.store().get_state(&state).ok().flatten())
        .map(|state| state.tree.short())
        .unwrap_or_default();
    let thread = thread.to_string();
    Ok(Some(Thread {
        id: thread.clone(),
        thread,
        target_thread: None,
        parent_thread: None,
        mode: ThreadMode::Materialized,
        state: ThreadState::Active,
        base_state: current_state.clone().unwrap_or_default(),
        base_root,
        current_state,
        merged_state: None,
        task: None,
        execution_path: repo.root().to_path_buf(),
        materialized_path: None,
        changed_paths: Vec::new(),
        impact_categories: Vec::new(),
        heavy_impact_paths: Vec::new(),
        promotion_suggested: false,
        freshness: ThreadFreshness::Unknown,
        verification_summary: Default::default(),
        confidence_summary: Default::default(),
        integration_policy_result: Default::default(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        ephemeral: None,
        auto: false,
        shared_target_dir: None,
    }))
}

fn active_task_assignment_id(repo: &Repository, thread: Option<&Thread>) -> Result<Option<String>> {
    let Some(thread) = thread else {
        return Ok(None);
    };
    let store = ActorPresenceStore::new(repo.heddle_dir());
    Ok(store
        .active_entries()?
        .into_iter()
        .filter(|entry| entry.thread == thread.id)
        .max_by_key(|entry| entry.started_at)
        .and_then(|entry| entry.task_assignment_id))
}

fn bulk_capture_warning(captured_path_count: usize) -> Option<String> {
    (captured_path_count >= BULK_CAPTURE_WARNING_THRESHOLD).then(|| {
        format!(
            "captured {captured_path_count} paths in one operation; check root .gitignore and .heddleignore rules if build artifacts or tool state were included"
        )
    })
}

fn non_empty_action(action: &str) -> Option<String> {
    (!action.trim().is_empty()).then(|| action.to_string())
}

fn map_capture_error(error: anyhow::Error) -> anyhow::Error {
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<HeddleError>()
            .is_some_and(|error| matches!(error, HeddleError::NoChanges))
    }) {
        return capture_refusal(
            "nothing_to_capture",
            "nothing to capture: worktree has no changes eligible for Heddle capture",
            "Inspect the worktree with `heddle status`; make changes before running `heddle capture -m \"...\"`.",
            "the worktree has no modified, deleted, or untracked paths relative to the current Heddle state",
            "capture would not create a meaningful Heddle state",
            "repository state was left unchanged",
            vec!["heddle status".to_string()],
        );
    }
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(objects::fs_atomic::is_out_of_space)
    }) {
        return capture_refusal(
            "capture_out_of_space",
            format!("Capture aborted because the filesystem is out of space: {error:#}"),
            "Free disk space and re-run `heddle capture`. Your working tree changes are intact.",
            "the filesystem reported no remaining space while Heddle was writing captured objects",
            "retrying before freeing space may fail again or leave another incomplete object write",
            "the working tree was not modified; already-committed repository data remains behind atomic write boundaries",
            vec!["heddle capture -m \"...\"".to_string()],
        );
    }
    error
}

#[allow(clippy::too_many_arguments)]
fn capture_refusal(
    kind: &'static str,
    error: impl Into<String>,
    hint: impl Into<String>,
    unsafe_condition: impl Into<String>,
    would_change: impl Into<String>,
    preserved: impl Into<String>,
    recovery_commands: Vec<String>,
) -> anyhow::Error {
    anyhow!(HeddleError::recovery(
        RecoveryDetails::safety_refusal(
            kind,
            error,
            hint,
            unsafe_condition,
            would_change,
            preserved,
        )
        .with_recovery_commands(recovery_commands),
    ))
}

fn preview_paths(paths: &[String]) -> String {
    let shown = paths
        .iter()
        .take(12)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let hidden = paths.len().saturating_sub(12);
    if hidden == 0 {
        shown
    } else {
        format!("{shown}, and {hidden} more")
    }
}

/// Execute a save: optional Heddle snapshot + optional Git checkpoint write-through.
///
/// Callers own clap validation (missing message/intent) and plain-Git refusal.
/// Mutation composition, hooks, thread metadata, Git write-through, and post
/// verification live here.
pub fn execute_save(repo: &Repository, plan: SavePlan) -> Result<SaveReport> {
    // A plan that asks for a Git checkpoint on a non-overlay repo is a hard
    // error: `plan_writes_git_checkpoint` silently returns false for native
    // repos, so guard on the raw `git_scope` intent instead (the previous
    // `plan_writes_git_checkpoint(..) && capability != GitOverlay` was
    // self-contradictory and never fired).
    if plan.git_scope != GitScope::None && repo.capability() != RepositoryCapability::GitOverlay {
        return Err(anyhow!(HeddleError::recovery(
            RecoveryDetails::safety_refusal(
                "native_checkpoint_unavailable",
                "Git checkpointing is only available in Git-overlay repositories",
                "Use `heddle capture -m \"...\"` to save Heddle state in a native checkout.",
                "this checkout is not a Git-overlay repository",
                "checkpoint would try to write a Git commit where no active Git store is bound",
                "repository state, refs, and worktree files were left unchanged",
            ),
        )));
    }

    let previous_state_started = Instant::now();
    let (previous_state, previous_state_profile) =
        repo.current_state_for_worktree_status_profiled()?;
    let previous_state_ms = previous_state_started.elapsed().as_millis();
    let has_current = previous_state.is_some();
    let mut created_new_state = false;
    let mut snapshot_profile = SnapshotProfile::default();
    let mut thread_metadata_ms = 0u128;
    let mut promotion_suggested = false;
    let mut heavy_impact_paths = Vec::new();
    let mut snapshot_state_id: Option<StateId> = None;
    let mut captured_path_count = 0usize;
    let mut state_create_ms = 0u128;
    let mut captured_path_count_ms = 0u128;

    let mut state = if plan_creates_new_state(&plan, has_current) {
        created_new_state = true;
        let state_create_started = Instant::now();
        let execution = create_heddle_state(repo, &plan)?;
        state_create_ms = state_create_started.elapsed().as_millis();
        snapshot_profile = execution.profile;
        thread_metadata_ms = execution.thread_metadata_ms;
        promotion_suggested = execution.promotion_suggested;
        heavy_impact_paths = execution.heavy_impact_paths;
        snapshot_state_id = Some(execution.state.state_id);
        let previous_tree = match previous_state.as_ref() {
            Some(state) => state.tree,
            None => repo.store().put_tree(&Tree::new())?,
        };
        let captured_path_count_started = Instant::now();
        captured_path_count = repo
            .diff_trees(&previous_tree, &execution.state.tree)?
            .len();
        captured_path_count_ms = captured_path_count_started.elapsed().as_millis();
        execution.state
    } else {
        repo.current_state()?
            .ok_or_else(|| anyhow!("no captured state found for save"))?
    };

    let mut git_commit = None;
    let mut git_previous_commit = None;
    let mut git_checkpoint = None;

    if plan_writes_git_checkpoint(&plan, repo.capability()) {
        if plan.require_clean_worktree {
            let tree = repo.require_tree(&state.tree)?;
            let status = repo.compare_worktree_cached_detailed_with_options(
                &tree,
                &plan.worktree_status_options,
            )?;
            if !status.is_clean() {
                return Err(anyhow!(HeddleError::recovery(
                    RecoveryDetails::safety_refusal(
                        "dirty_worktree",
                        "Save worktree changes before committing",
                        "Save the work with `heddle capture -m \"...\"`, then retry the commit.",
                        "the current Heddle state was left unchanged; these paths have not been captured",
                        "commit would write Git history that does not include dirty worktree paths",
                        "the current Heddle state was left unchanged; these paths have not been captured",
                    ),
                )));
            }
        }

        if let Some(existing) = repo.latest_git_checkpoint_for_state(&state.state_id)?
            && repo.pending_git_checkpoint_intent()?.is_none()
        {
            git_commit = Some(existing.git_commit.clone());
            git_checkpoint = Some(existing);
        } else {
            let previous = repo
                .pending_git_checkpoint_intent()?
                .and_then(|intent| intent.previous_git_oid)
                .or_else(|| git_rev_parse_head(repo.root()));
            git_previous_commit = previous.clone();
            let summary = checkpoint_summary(&plan, &state);
            let record = write_git_checkpoint(repo, &state, summary, plan.linearize_git_parent)?;
            if plan.coalesce_snapshot_and_checkpoint
                && let Some(state_id) = snapshot_state_id.as_ref()
            {
                coalesce_snapshot_and_checkpoint(repo, state_id, &record.git_commit)?;
            }
            git_commit = Some(record.git_commit.clone());
            git_checkpoint = Some(record);
        }
    }

    // Post-mutation verification is always fresh when we created state or wrote
    // a Git checkpoint (those mutations flip health classification). Otherwise
    // reuse a caller-supplied worktree status to avoid a redundant walk.
    let captured_native_worktree = created_new_state
        && plan.supplied_tree.is_none()
        && repo.capability() == RepositoryCapability::NativeHeddle;
    let captured_worktree_status = Ok(Some(objects::worktree::WorktreeStatus::default()));
    let verification_started = Instant::now();
    let mut verification = if captured_native_worktree && git_checkpoint.is_none() {
        let health = build_repository_verification_health_with_worktree_status(
            repo,
            &captured_worktree_status,
        );
        if let Some(input) = &plan.machine_contract_input {
            build_repository_verification_state_with_worktree_status_and_machine_contract(
                repo,
                health,
                &captured_worktree_status,
                input,
            )
        } else {
            build_repository_verification_state_with_worktree_status(
                repo,
                health,
                &captured_worktree_status,
            )
        }
    } else if created_new_state || git_checkpoint.is_some() {
        if let Some(input) = &plan.machine_contract_input {
            build_repository_verification_state_with_machine_contract(repo, input)?
        } else {
            build_repository_verification_state(repo)?
        }
    } else if let Some(status) = &plan.precomputed_worktree_status {
        let health = build_repository_verification_health_with_worktree_status(repo, status);
        if let Some(input) = &plan.machine_contract_input {
            build_repository_verification_state_with_worktree_status_and_machine_contract(
                repo, health, status, input,
            )
        } else {
            build_repository_verification_state_with_worktree_status(repo, health, status)
        }
    } else {
        if let Some(input) = &plan.machine_contract_input {
            build_repository_verification_state_with_machine_contract(repo, input)?
        } else {
            build_repository_verification_state(repo)?
        }
    };
    if plan.commit_safe_post_verify {
        soften_commit_next_action(&mut verification);
    }
    let post_verification_ms = verification_started.elapsed().as_millis();

    let summary = match plan.verb {
        SaveVerb::Capture => format!(
            "Captured state {} ({})",
            state.state_id.short(),
            state.hash().short()
        ),
        SaveVerb::Commit => plan
            .intent
            .clone()
            .unwrap_or_else(|| format!("Commit {}", state.state_id.short())),
        SaveVerb::Checkpoint => git_checkpoint
            .as_ref()
            .map(|r| r.summary.clone())
            .unwrap_or_else(|| format!("Checkpoint {}", state.state_id.short())),
    };

    let signature_lookup_started = Instant::now();
    let signed = repo.get_state_signature(&state.id())?.is_some();
    let signature_lookup_ms = signature_lookup_started.elapsed().as_millis();
    Ok(SaveReport {
        verb: plan.verb,
        state_id: state.state_id,
        content_hash: state.hash(),
        intent: state.intent.clone(),
        confidence: state.confidence,
        signed,
        git_commit,
        git_previous_commit,
        summary,
        principal: state.attribution.principal.clone(),
        agent: state.attribution.agent.clone(),
        promotion_suggested,
        heavy_impact_paths,
        captured_path_count,
        verification,
        created_new_state,
        git_checkpoint,
        snapshot_profile,
        state_create_ms,
        captured_path_count_ms,
        post_verification_ms,
        thread_metadata_ms,
        previous_state_ms,
        previous_state_profile,
        signature_lookup_ms,
    })
}

struct CreatedState {
    state: State,
    profile: SnapshotProfile,
    thread_metadata_ms: u128,
    promotion_suggested: bool,
    heavy_impact_paths: Vec<String>,
}

fn create_heddle_state(repo: &Repository, plan: &SavePlan) -> Result<CreatedState> {
    let hook_manager = HookManager::new(repo);
    let hook_ctx = HookContext::new(repo);

    if plan.run_hooks {
        hook_manager.run(Hook::PreSnapshot, &hook_ctx)?;
        let pre_capture_payload = serde_json::json!({
            "thread": current_thread_name(repo),
            "intent": plan.intent.clone().unwrap_or_default(),
        });
        let pre_capture_response = hook_manager.run_with_payload(
            Hook::PreSnapshot,
            &hook_ctx,
            &pre_capture_payload,
            std::time::Duration::from_secs(5),
        )?;
        if let Some(resp) = pre_capture_response
            && !resp.abort.is_empty()
        {
            return Err(anyhow!(HeddleError::recovery(
                RecoveryDetails::safety_refusal(
                    "hook_veto",
                    format!("pre_capture hook vetoed: {}", resp.abort),
                    "Inspect `pre_capture` with `heddle hook list`, update the hook policy or inputs, then retry.",
                    format!("pre_capture hook vetoed capture: {}", resp.abort),
                    "capture would continue after repository policy explicitly aborted the operation",
                    "the operation stopped at the hook boundary before the protected action ran",
                )
                .with_recovery_commands(vec!["heddle hook list".to_string()]),
            )));
        }
    }

    let mut execution = if let Some(tree) = plan.supplied_tree.clone() {
        repo.snapshot_tree_with_attribution_profiled(
            tree,
            plan.intent.clone(),
            plan.confidence,
            plan.attribution.clone(),
        )?
    } else if let Some(status) = plan.known_worktree_changes.clone() {
        repo.snapshot_with_attribution_profiled_from_status(
            plan.intent.clone(),
            plan.confidence,
            plan.attribution.clone(),
            status,
            plan.require_worktree_change,
        )?
    } else if plan.require_worktree_change {
        repo.snapshot_with_attribution_profiled_if_changed(
            plan.intent.clone(),
            plan.confidence,
            plan.attribution.clone(),
        )?
    } else {
        repo.snapshot_with_attribution_profiled(
            plan.intent.clone(),
            plan.confidence,
            plan.attribution.clone(),
        )?
    };

    let thread_metadata_start = Instant::now();
    let refresh = refresh_active_thread_metadata(repo, &execution.state, &execution.tree)?;
    let thread_metadata_ms = thread_metadata_start.elapsed().as_millis();

    if plan.run_hooks {
        hook_manager.run(Hook::PostSnapshot, &hook_ctx)?;
        let post_capture_payload = serde_json::json!({
            "state_id": execution.state.state_id.to_string_full(),
        });
        if let Err(err) = hook_manager.run_with_payload(
            Hook::PostSnapshot,
            &hook_ctx,
            &post_capture_payload,
            std::time::Duration::from_secs(5),
        ) {
            tracing::warn!(error = %err, "post_capture hook error swallowed");
        }
    }

    Ok(CreatedState {
        state: execution.state,
        profile: std::mem::take(&mut execution.profile),
        thread_metadata_ms,
        promotion_suggested: refresh.promotion_suggested,
        heavy_impact_paths: refresh.heavy_impact_paths,
    })
}

fn write_git_checkpoint(
    repo: &Repository,
    state: &State,
    summary: String,
    linearize_git_parent: bool,
) -> Result<GitCheckpointRecord> {
    let _lock = repo.locker().write()?;
    objects::fault_inject::maybe_fail_at("git_checkpoint_before_write_through")?;
    let mut bridge = GitProjection::new(repo);
    if linearize_git_parent {
        bridge.linearize_unmapped_tip_to_checkout();
    }
    let git_commit = match bridge
        .write_through_current_checkout_with_message(state.state_id, summary.clone())?
    {
        WriteThroughOutcome::Wrote(git_commit) => git_commit.to_string(),
        WriteThroughOutcome::Skipped(reason) => {
            return Err(anyhow!(HeddleError::recovery(
                RecoveryDetails::safety_refusal(
                    "checkpoint_git_write_skipped",
                    format!("Git checkpoint write-through was skipped: {reason}"),
                    "Inspect `heddle verify`, resolve the skip reason, then retry `heddle land`.",
                    format!("write-through skipped: {reason}"),
                    "checkpoint would need to write the current Heddle state into the Git branch and index",
                    "the current Heddle state was preserved; no Git checkpoint record was written",
                ),
            )));
        }
    };
    let intent = repo.pending_git_checkpoint_intent()?.ok_or_else(|| {
        anyhow!("Git checkpoint published without its durable finalization intent")
    })?;
    if intent.phase != repo::GitCheckpointIntentPhase::Published
        || intent.state_id != state.state_id.to_string_full()
        || intent.new_git_oid != git_commit
    {
        return Err(anyhow!(
            "published Git checkpoint does not match its durable finalization intent"
        ));
    }
    finalize_published_git_checkpoint(repo, &state.state_id, git_commit, summary, intent)
}

/// Finish the metadata/oplog half of a checkpoint whose Git ref was already
/// published before a crash. Returns `None` when no matching published intent
/// exists, so callers can continue with their own recovery policy.
pub fn recover_published_git_checkpoint(
    repo: &Repository,
    state_id: &StateId,
) -> Result<Option<GitCheckpointRecord>> {
    let _lock = repo.locker().write()?;
    let Some(mut intent) = repo.pending_git_checkpoint_intent()? else {
        return Ok(None);
    };
    if intent.state_id != state_id.to_string_full() {
        return Ok(None);
    }
    let current_branch = repo.git_overlay_current_branch()?;
    if current_branch.as_deref() != Some(intent.branch.as_str()) {
        return Err(anyhow!(
            "pending Git checkpoint targets branch '{}' but the checkout is on '{}'",
            intent.branch,
            current_branch.as_deref().unwrap_or("detached HEAD")
        ));
    }
    let current_oid = git_rev_parse_head(repo.root());
    if intent.phase == repo::GitCheckpointIntentPhase::Prepared {
        if current_oid == intent.previous_git_oid {
            return Ok(None);
        }
        if current_oid.as_deref() != Some(intent.new_git_oid.as_str()) {
            return Err(anyhow!(
                "prepared Git checkpoint expected HEAD at {} or {}, found {}",
                intent.previous_git_oid.as_deref().unwrap_or("<unborn>"),
                intent.new_git_oid,
                current_oid.as_deref().unwrap_or("<unborn>")
            ));
        }
        let git_oid = intent.new_git_oid.clone();
        intent = repo.mark_git_checkpoint_published(state_id, &git_oid)?;
    }
    if intent.phase != repo::GitCheckpointIntentPhase::Published {
        return Ok(None);
    }
    if current_oid.as_deref() != Some(intent.new_git_oid.as_str()) {
        return Err(anyhow!(
            "published Git checkpoint expected HEAD at {}, found {}",
            intent.new_git_oid,
            current_oid.as_deref().unwrap_or("<unborn>")
        ));
    }
    let git_commit = intent.new_git_oid.clone();
    let summary = intent.summary.clone();
    finalize_published_git_checkpoint(repo, state_id, git_commit, summary, intent).map(Some)
}

fn finalize_published_git_checkpoint(
    repo: &Repository,
    state_id: &StateId,
    git_commit: String,
    summary: String,
    intent: repo::GitCheckpointIntent,
) -> Result<GitCheckpointRecord> {
    let record = repo.record_git_checkpoint(state_id, git_commit.clone(), summary)?;
    objects::fault_inject::maybe_panic_at("git_checkpoint_after_metadata_before_oplog");
    let transaction_id = format!(
        "git-checkpoint:v1:{}:{}",
        state_id.to_string_full(),
        git_commit
    );
    repo.oplog().record_batch_exactly_once(
        vec![
            OpRecord::GitCheckpoint {
                branch: intent.branch,
                state: *state_id,
                previous_git_oid: intent.previous_git_oid,
                new_git_oid: git_commit.clone(),
            },
            OpRecord::TransactionCommit {
                transaction_id: transaction_id.clone(),
                op_count: 1,
            },
        ],
        Some(&repo.op_scope()),
        &transaction_id,
    )?;
    objects::fault_inject::maybe_panic_at("git_checkpoint_after_oplog_before_finalize");
    repo.finish_git_checkpoint_intent(state_id, &git_commit)?;
    Ok(record)
}

fn coalesce_snapshot_and_checkpoint(
    repo: &Repository,
    state_id: &StateId,
    git_commit: &str,
) -> Result<()> {
    let snapshot_batch = repo
        .oplog()
        .recent_batches_scoped(8, Some(&repo.op_scope()))?
        .into_iter()
        .find(|batch| {
            batch.entries.iter().any(|entry| {
                matches!(
                    &entry.operation,
                    OpRecord::Snapshot { new_state, .. } if new_state == state_id
                )
            })
        })
        .ok_or_else(|| anyhow!("capture succeeded but its oplog batch was not found"))?;
    let checkpoint_batch = repo
        .oplog()
        .recent_batches_scoped(8, Some(&repo.op_scope()))?
        .into_iter()
        .find(|batch| {
            batch.entries.iter().any(|entry| {
                matches!(
                    &entry.operation,
                    OpRecord::GitCheckpoint { new_git_oid, .. } if new_git_oid == git_commit
                )
            })
        })
        .ok_or_else(|| anyhow!("Git checkpoint succeeded but its oplog batch was not found"))?;
    repo.oplog()
        .coalesce_batches(snapshot_batch.id, checkpoint_batch.id)
        .context(
            "commit completed but failed to record capture and Git checkpoint as one undo batch",
        )?;
    Ok(())
}

fn checkpoint_summary(plan: &SavePlan, state: &State) -> String {
    plan.intent
        .clone()
        .or_else(|| state.intent.clone())
        .unwrap_or_else(|| format!("Checkpoint {}", state.state_id.short()))
}

fn current_thread_name(repo: &Repository) -> String {
    match repo.head_ref() {
        Ok(Head::Attached { thread }) => thread.to_string(),
        _ => String::new(),
    }
}

fn git_rev_parse_head(root: &std::path::Path) -> Option<String> {
    let git = SleyRepository::discover(root).ok()?;
    git.head().ok()?.oid.map(|id| id.to_string())
}

fn soften_commit_next_action(trust: &mut RepositoryVerificationState) {
    if is_commit_action(&trust.recommended_action) {
        trust.recommended_action = "heddle status".to_string();
        trust.recommended_action_template = None;
    }
    for check in &mut trust.checks {
        if check
            .recommended_action
            .as_deref()
            .is_some_and(is_commit_action)
        {
            check.recommended_action = Some("heddle status".to_string());
            check.recommended_action_template = None;
        }
    }
}

fn is_commit_action(action: &str) -> bool {
    let trimmed = action.trim();
    trimmed == "heddle capture" || trimmed.starts_with("heddle capture ")
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use repo::RepositoryCapability;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn capture_interface_owns_mutation_and_returns_the_report_contract() {
        let temp = TempDir::new().expect("create temp repository");
        let repo = Repository::init_default(temp.path()).expect("initialize repository");
        std::fs::write(temp.path().join("tracked.txt"), "captured\n")
            .expect("write worktree change");
        let ctx = ExecutionContext::builder()
            .repo(repo)
            .principal_fallback(Some(("Ada".into(), "ada@example.com".into())))
            .build();

        let report = capture(
            &ctx,
            CaptureOptions {
                intent: "exercise the deep capture interface".into(),
                confidence: Some(0.9),
                force: false,
                worktree_status_options: WorktreeStatusOptions::default(),
                machine_contract_input: None,
            },
            |_| {
                Ok(CaptureAttribution {
                    attribution: Attribution::human(Principal::new("Ada", "ada@example.com")),
                    principal_source: "embedder".into(),
                    harness_session_id: None,
                })
            },
        )
        .expect("capture succeeds");

        assert_eq!(report.output_kind, "capture");
        assert_eq!(report.captured_path_count, 1);
        assert_eq!(report.principal.name, "Ada");
        assert_eq!(report.principal.email, "ada@example.com");
        assert_eq!(report.principal_source, "embedder");
        assert_eq!(CaptureReport::CONTRACT.schema_name, "capture");
        assert_eq!(
            ctx.require_repo()
                .expect("repository")
                .head()
                .expect("read head")
                .expect("captured head")
                .short(),
            report.state_id
        );

        let wire = serde_json::to_value(&report).expect("serialize report");
        assert_eq!(wire["output_kind"], "capture");
        assert!(wire.get("diagnostics").is_none());
        assert!(wire.get("captured_thread_targets_integration").is_none());
    }

    #[test]
    fn manual_resolution_land_action_quotes_untrusted_thread_ids() {
        let temp = TempDir::new().expect("create temp repository");
        let repo = Repository::init_default(temp.path()).expect("initialize repository");

        assert_eq!(
            manual_resolution_land_action(&repo, "bad;echo pwn", None),
            "heddle land --thread 'bad;echo pwn'"
        );
        assert_eq!(
            manual_resolution_land_action(&repo, "-danger", None),
            "heddle land --thread=-danger"
        );
    }

    #[test]
    fn clean_overlay_refuses_before_resolving_attribution() {
        let temp = TempDir::new().expect("create temp repository");
        SleyRepository::init(temp.path()).expect("initialize Git repository");
        let repo = Repository::init_git_overlay_sidecar(temp.path())
            .expect("initialize Git-overlay sidecar");
        let ctx = ExecutionContext::builder().repo(repo).build();
        let resolver_called = Cell::new(false);

        let error = capture(
            &ctx,
            CaptureOptions {
                intent: "nothing changed".into(),
                confidence: None,
                force: false,
                worktree_status_options: WorktreeStatusOptions::default(),
                machine_contract_input: None,
            },
            |_| {
                resolver_called.set(true);
                Ok(CaptureAttribution {
                    attribution: Attribution::human(Principal::new("Ada", "ada@example.com")),
                    principal_source: "embedder".into(),
                    harness_session_id: None,
                })
            },
        )
        .expect_err("clean overlay must refuse capture");

        assert!(!resolver_called.get());
        assert!(error.to_string().contains("nothing to capture"));
    }

    #[test]
    fn capture_always_uses_git_scope_none() {
        assert_eq!(
            plan_git_scope(
                SaveVerb::Capture,
                RepositoryCapability::GitOverlay,
                true,
                true
            ),
            GitScope::None
        );
        assert_eq!(
            plan_git_scope(
                SaveVerb::Capture,
                RepositoryCapability::NativeHeddle,
                false,
                false
            ),
            GitScope::None
        );
    }

    #[test]
    fn commit_native_never_writes_git() {
        assert_eq!(
            plan_git_scope(
                SaveVerb::Commit,
                RepositoryCapability::NativeHeddle,
                true,
                true
            ),
            GitScope::None
        );
    }

    #[test]
    fn commit_git_overlay_routes_staged_vs_worktree() {
        assert_eq!(
            plan_git_scope(
                SaveVerb::Commit,
                RepositoryCapability::GitOverlay,
                true,
                false
            ),
            GitScope::Staged
        );
        assert_eq!(
            plan_git_scope(
                SaveVerb::Commit,
                RepositoryCapability::GitOverlay,
                true,
                true
            ),
            GitScope::WorktreeAll
        );
        assert_eq!(
            plan_git_scope(
                SaveVerb::Commit,
                RepositoryCapability::GitOverlay,
                false,
                false
            ),
            GitScope::WorktreeAll
        );
    }

    #[test]
    fn checkpoint_routes_staged_flag() {
        assert_eq!(
            plan_git_scope(
                SaveVerb::Checkpoint,
                RepositoryCapability::GitOverlay,
                true,
                false
            ),
            GitScope::Staged
        );
        assert_eq!(
            plan_git_scope(
                SaveVerb::Checkpoint,
                RepositoryCapability::GitOverlay,
                false,
                false
            ),
            GitScope::WorktreeAll
        );
    }

    #[test]
    fn plan_creates_new_state_routing() {
        let attr = Attribution::human(Principal::new("Ada", "ada@example.com"));
        let capture = SavePlan::capture("wip", attr.clone());
        assert!(plan_creates_new_state(&capture, true));
        assert!(plan_creates_new_state(&capture, false));

        let checkpoint = SavePlan::checkpoint(Some("cp".into()), attr.clone(), false);
        assert!(!plan_creates_new_state(&checkpoint, true));
        assert!(plan_creates_new_state(&checkpoint, false));

        let staged =
            SavePlan::commit("msg", attr, GitScope::Staged).with_supplied_tree(Tree::new());
        assert!(plan_creates_new_state(&staged, true));
    }

    #[test]
    fn plan_writes_git_checkpoint_respects_scope_and_capability() {
        let attr = Attribution::human(Principal::new("Ada", "ada@example.com"));
        let capture = SavePlan::capture("wip", attr.clone());
        assert!(!plan_writes_git_checkpoint(
            &capture,
            RepositoryCapability::GitOverlay
        ));

        let commit = SavePlan::commit("msg", attr.clone(), GitScope::WorktreeAll);
        assert!(plan_writes_git_checkpoint(
            &commit,
            RepositoryCapability::GitOverlay
        ));
        assert!(!plan_writes_git_checkpoint(
            &commit,
            RepositoryCapability::NativeHeddle
        ));

        let none = SavePlan::commit("msg", attr, GitScope::None);
        assert!(!plan_writes_git_checkpoint(
            &none,
            RepositoryCapability::GitOverlay
        ));
    }

    #[test]
    fn save_plan_builders_set_expected_defaults() {
        let attr = Attribution::human(Principal::new("Ada", "ada@example.com"));
        let capture = SavePlan::capture("intent", attr.clone());
        assert_eq!(capture.verb, SaveVerb::Capture);
        assert_eq!(capture.git_scope, GitScope::None);
        assert!(!capture.coalesce_snapshot_and_checkpoint);

        let commit = SavePlan::commit("msg", attr.clone(), GitScope::WorktreeAll);
        assert_eq!(commit.verb, SaveVerb::Commit);
        assert!(commit.coalesce_snapshot_and_checkpoint);
        assert!(commit.commit_safe_post_verify);

        let staged = SavePlan::checkpoint(None, attr, true);
        assert_eq!(staged.git_scope, GitScope::Staged);
        assert!(!staged.require_clean_worktree);
        assert!(staged.reuse_current_state);
    }

    #[test]
    fn tree_leaf_name_and_commit_next_action() {
        assert_eq!(tree_leaf_name("a/b/c.rs"), "c.rs");
        assert_eq!(tree_leaf_name("solo"), "solo");
        assert_eq!(
            commit_next_action_from_trust("heddle push", false, false).as_deref(),
            Some("heddle push")
        );
        assert_eq!(
            commit_next_action_from_trust("", false, true).as_deref(),
            Some("heddle verify")
        );
        assert_eq!(
            commit_next_action_from_trust("", true, true).as_deref(),
            Some("heddle push")
        );
        assert_eq!(commit_next_action_from_trust("", true, false), None);
    }

    #[test]
    fn commit_git_index_plan_modes() {
        let staged = vec!["a.rs".into()];
        let extra = vec!["unstaged: b.rs".into(), "untracked: c.rs".into()];
        let staged_only = plan_commit_git_index(&staged, &extra, false);
        assert_eq!(staged_only.commit_mode, "staged_index");
        assert_eq!(staged_only.will_commit, vec!["a.rs"]);
        assert_eq!(staged_only.preserved_after_commit.len(), 2);

        let all = plan_commit_git_index(&staged, &extra, true);
        assert_eq!(all.commit_mode, "worktree_all_explicit");
        assert_eq!(all.will_commit.len(), 3);

        let index_only = plan_commit_git_index_only(&staged, &extra);
        assert_eq!(index_only.commit_mode, "staged_index");
        assert_eq!(index_only.will_commit, vec!["a.rs"]);

        assert!(commit_scope_text("staged_index").contains("staged Git index"));
        assert!(staged_commit_summary("ok", 1, 2).contains("left 2 unstaged/untracked"));
        assert_eq!(staged_commit_summary("ok", 1, 0), "ok");
    }
}
