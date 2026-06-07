// SPDX-License-Identifier: Apache-2.0
//! Thread commands.

use objects::store::ObjectStore;
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    process,
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use gix::bstr::ByteSlice;
use objects::{
    object::{ChangeId, State, ThreadName, Tree},
    store::{AgentEntry, AgentRegistry, AgentStatus, current_boot_id},
};
use oplog::OpLogBackend;
use refs::{Head, RefExpectation, RefUpdate};
use repo::{
    AgentUsageSummary, GitOverlayBranchTip, GitOverlayImportHint, GitRemoteTrackingStatus,
    Repository, RepositoryOperationStatus, Thread, ThreadCaptureOutcome, ThreadConfidenceSummary,
    ThreadFreshness, ThreadId, ThreadIdError, ThreadImpactCategory, ThreadIntegrationPolicy,
    ThreadManager, ThreadMode, ThreadRuntimeOverlay, ThreadState, ThreadVerificationSummary,
    ThreadView, describe_thread_advice,
};
use serde::Serialize;

use super::{
    action_line::{print_nested_next_step, print_nested_optional, print_next_step, print_optional},
    advice::RecoveryAdvice,
    command_catalog::{ActionTemplate, recommended_action_template},
    git_overlay_health::{
        RepositoryVerificationState, build_repository_verification_state,
        canonical_adopt_ref_command, canonical_bridge_reconcile_ref_preview_command,
        override_trust_recommended_action, serialize_empty_action_as_null,
    },
    mount_lifecycle,
    next_action::{
        NextActionInput, NextActionValidationContext, effective_next_action,
        thread_recovery_action_is_primary as shared_thread_recovery_action_is_primary,
        write_full_command_json,
    },
    operator_loop::{primary_next_action, primary_next_action_with_verification},
    snapshot::{ensure_current_state, summarize_confidence, summarize_verification},
    start_atomic,
    thread_cmd::{refresh_thread_freshness, thread_not_found_advice},
    worktree_cmd::{
        helpers::{plan_worktree_target, write_isolated_checkout},
        shared_target,
    },
    worktree_safety::ensure_worktree_clean,
};
use crate::{
    cli::{
        Cli, ThreadListArgs, ThreadStartArgs, WorkspaceModeArg, should_output_json, style,
        worktree_status_options,
    },
    config::{UserConfig, UserThreadWorkspaceMode},
};

pub(crate) const DEFAULT_AVAILABLE_GIT_REF_LIMIT: usize = 5;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Serialize)]
pub struct ThreadSummary {
    pub name: String,
    pub operation: Option<RepositoryOperationStatus>,
    pub remote_tracking: Option<GitRemoteTrackingStatus>,
    pub base_state: Option<String>,
    pub base_root: Option<String>,
    pub current_state: Option<String>,
    pub path: Option<String>,
    pub execution_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heddle_session_id: Option<String>,
    pub actor: Option<ThreadActorInfo>,
    pub harness: Option<String>,
    pub thinking_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_actor_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_parent_actor_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_confidence: Option<f32>,
    pub usage_summary: Option<AgentUsageSummary>,
    pub last_progress_at: Option<String>,
    pub last_activity_at: Option<String>,
    pub report_flush_state: Option<String>,
    pub attach_reason: Option<String>,
    pub thread_mode: Option<ThreadMode>,
    pub thread_state: Option<ThreadState>,
    pub freshness: Option<ThreadFreshness>,
    pub visibility: String,
    pub target_thread: Option<String>,
    pub parent_thread: Option<String>,
    pub child_threads: Vec<String>,
    pub sibling_threads: Vec<String>,
    pub stack_depth: usize,
    pub stale_from_parent: bool,
    pub task: Option<String>,
    pub changed_paths: Vec<String>,
    pub promotion_suggested: bool,
    pub impact_categories: Vec<ThreadImpactCategory>,
    pub heavy_impact_paths: Vec<String>,
    pub verification_summary: ThreadVerificationSummary,
    pub confidence_summary: ThreadConfidenceSummary,
    pub integration_policy_result: ThreadIntegrationPolicy,
    pub coordination_status: CoordinationStatus,
    pub is_current: bool,
    pub is_isolated: bool,
    pub thread_health: String,
    pub blockers: Vec<String>,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplate>,
    pub git_branch_tip: Option<String>,
    pub history_imported: bool,
    /// Mirror of [`repo::ThreadRecord::auto`]. `true` when the thread
    /// was created by a harness integration rather than an explicit
    /// user verb. Used by `heddle thread list` (default-hides) and
    /// `heddle thread cleanup --auto`.
    pub auto: bool,
    /// Mirror of [`repo::ThreadRecord::shared_target_dir`]. When
    /// present, the thread's checkout has its cargo `target/`
    /// redirected to this absolute path via a `.cargo/config.toml`
    /// committed inside the checkout. `None` for threads using
    /// cargo's default per-checkout `target/`.
    pub shared_target_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AvailableGitRef {
    pub name: String,
    pub git_commit: String,
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplate>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadActorInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl ThreadSummary {
    fn from_view(view: ThreadView, coordination_status: CoordinationStatus) -> Self {
        let mode = view.record.mode.clone();
        ThreadSummary {
            name: view.record.thread,
            operation: None,
            remote_tracking: None,
            base_state: Some(view.record.base_state),
            base_root: Some(view.record.base_root),
            current_state: view.record.current_state,
            path: view
                .runtime
                .materialized_path
                .as_ref()
                .or(view.runtime.path.as_ref())
                .and_then(|p| display_path_string(p)),
            execution_path: view
                .runtime
                .execution_path
                .as_ref()
                .and_then(|p| display_path_string(p)),
            session_id: view.runtime.session_id,
            heddle_session_id: view.runtime.heddle_session_id,
            actor: match (view.runtime.provider, view.runtime.model) {
                (None, None) => None,
                (provider, model) => Some(ThreadActorInfo { provider, model }),
            },
            harness: view.runtime.harness,
            thinking_level: view.runtime.thinking_level,
            native_actor_key: view.runtime.native_actor_key,
            native_parent_actor_key: view.runtime.native_parent_actor_key,
            probe_source: view.runtime.probe_source,
            probe_confidence: view.runtime.probe_confidence,
            usage_summary: view.runtime.usage_summary,
            last_progress_at: view.runtime.last_progress_at.map(|ts| ts.to_rfc3339()),
            last_activity_at: Some(view.record.updated_at.to_rfc3339()),
            report_flush_state: view.runtime.report_flush_state,
            attach_reason: view.runtime.attach_reason,
            thread_mode: Some(mode.clone()),
            thread_state: Some(view.record.state),
            freshness: Some(view.record.freshness),
            visibility: if view.is_isolated {
                visibility_label(&mode).to_string()
            } else {
                "ref_only".to_string()
            },
            target_thread: view.record.target_thread,
            parent_thread: view.record.parent_thread,
            child_threads: Vec::new(),
            sibling_threads: Vec::new(),
            stack_depth: 0,
            stale_from_parent: false,
            task: view.record.task,
            changed_paths: view.record.changed_paths,
            promotion_suggested: view.record.promotion_suggested,
            impact_categories: view.record.impact_categories,
            heavy_impact_paths: view.record.heavy_impact_paths,
            verification_summary: view.record.verification_summary,
            confidence_summary: view.record.confidence_summary,
            integration_policy_result: view.record.integration_policy_result,
            coordination_status,
            is_current: view.is_current,
            is_isolated: view.is_isolated,
            thread_health: "clean".to_string(),
            blockers: Vec::new(),
            recommended_action: String::new(),
            recommended_action_template: None,
            git_branch_tip: None,
            history_imported: true,
            auto: view.record.auto,
            shared_target_dir: view
                .record
                .shared_target_dir
                .as_ref()
                .map(|p| p.display().to_string()),
        }
    }
}

fn display_path_string(path: &Path) -> Option<String> {
    let rendered = path.display().to_string();
    if rendered.trim().is_empty() {
        None
    } else {
        Some(rendered)
    }
}

#[derive(Serialize)]
struct ThreadListOutput {
    output_kind: &'static str,
    repository_capability: String,
    repository_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_context: Option<crate::cli::render::RepositoryContextInfo>,
    storage_model: String,
    hosted_enabled: bool,
    threads: Vec<ThreadSummary>,
    available_git_refs: Vec<AvailableGitRef>,
    current: Option<String>,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    recommended_action: String,
    recommended_action_template: Option<ActionTemplate>,
    recovery_commands: Vec<String>,
    recovery_action_templates: Vec<ActionTemplate>,
    /// Carried for the human-readable renderer only. Not part of the
    /// JSON contract: import-hint information is exposed via
    /// `heddle bridge git status --output json` instead.
    #[serde(skip)]
    git_overlay_import_hint: Option<ThreadListGitOverlayImportHintOutput>,
}

#[derive(Serialize)]
struct ThreadListGitOverlayImportHintOutput {
    current_branch: String,
    missing_branch_count: usize,
    missing_branches: Vec<String>,
    recommended_command: String,
}

#[derive(Serialize)]
struct ThreadShowOutput {
    output_kind: &'static str,
    repository_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_context: Option<crate::cli::render::RepositoryContextInfo>,
    #[serde(flatten)]
    summary: ThreadSummary,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    next_action: String,
    next_action_template: Option<ActionTemplate>,
    recommended_action_template: Option<ActionTemplate>,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
    recovery_commands: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct ThreadOpOutput {
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
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "verification")]
    pub trust: Option<RepositoryVerificationState>,
}

#[derive(Serialize)]
pub(crate) struct ThreadCaptureOutput {
    pub change_id: String,
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

#[derive(Serialize)]
pub(crate) struct ThreadCaptureSummary {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub total: usize,
}

pub fn cmd_start(cli: &Cli, args: ThreadStartArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    if args.path.is_some() {
        ensure_worktree_clean(&repo, "start thread")?;
    }
    let print_cd = args.print_cd_path;
    let output = start_thread(&repo, args)?;
    if print_cd {
        return render_cd_path(&output);
    }
    render_thread_op(cli, output)
}

/// Print only the new thread's checkout path on stdout, then exit. Used by
/// shell wrappers (`dir=$(heddle start foo --print-cd-path) && cd "$dir"`).
/// Returns an error when the operation didn't produce a checkout path —
/// callers that pass `--print-cd-path` and get an error should fall back to
/// `heddle start foo` for the full report.
fn render_cd_path(output: &ThreadOpOutput) -> Result<()> {
    let thread_name = output
        .thread
        .as_ref()
        .map(|t| t.name.clone())
        .unwrap_or_else(|| output.name.clone());
    let path = output
        .thread
        .as_ref()
        .and_then(|t| t.path.as_deref())
        .ok_or_else(|| {
            anyhow!(RecoveryAdvice::thread_checkout_unavailable(
                &thread_name,
                "--print-cd-path",
            ))
        })?;
    println!("{path}");
    Ok(())
}

pub(crate) fn cmd_thread_captures(
    cli: &Cli,
    repo: &Repository,
    thread: &str,
    limit: usize,
) -> Result<()> {
    let captures = collect_thread_captures(repo, thread, limit)?;
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&captures)?);
        return Ok(());
    }

    println!("{}", style::section(&format!("Saved states on {thread}")));
    if captures.is_empty() {
        println!("{}", style::dim("  No saved states on this thread yet."));
        return Ok(());
    }
    for capture in captures {
        let confidence = capture
            .confidence
            .map(|value| format!("{value:.2}"))
            .unwrap_or_else(|| "None".to_string());
        println!(
            "  {} {} {}",
            style::accent(&capture.change_id),
            capture.message,
            style::dim(&format!("confidence {confidence}"))
        );
        println!("    {}", style::dim(&capture.created_at));
        if let Some(agent) = capture.agent {
            println!("    {}", style::field("Agent", &agent));
        }
    }
    Ok(())
}

fn collect_thread_captures(
    repo: &Repository,
    thread: &str,
    limit: usize,
) -> Result<Vec<ThreadCaptureOutput>> {
    let current = repo
        .refs()
        .get_thread(&ThreadName::new(thread))?
        .ok_or_else(|| anyhow!(thread_not_found_advice(thread, "list thread captures")))?;
    let base = ThreadManager::new(repo.heddle_dir())
        .load(thread)?
        .map(|thread| thread.base_state);
    let mut out = Vec::new();
    let mut cursor = Some(current);
    while let Some(change_id) = cursor {
        if base.as_deref() == Some(change_id.short().as_str())
            || base.as_deref().and_then(|base| ChangeId::parse(base).ok()) == Some(change_id)
        {
            break;
        }
        let Some(state) = repo.store().get_state(&change_id)? else {
            break;
        };
        if state
            .intent
            .as_deref()
            .is_some_and(|intent| !intent.starts_with("Bootstrap "))
        {
            let summary = capture_diff_summary(repo, &state);
            out.push(thread_capture_output(&state, summary));
        }
        if out.len() >= limit {
            break;
        }
        cursor = state.parents.first().copied();
    }
    Ok(out)
}

/// Summarize the file-count delta between a state and its first
/// parent. Best-effort: returns `None` when there is no parent (root
/// capture) or when the parent state isn't materialized in the local
/// store (e.g. shallow imports).
fn capture_diff_summary(repo: &Repository, state: &State) -> Option<ThreadCaptureSummary> {
    let parent_id = state.parents.first().copied()?;
    let parent = repo.store().get_state(&parent_id).ok().flatten()?;
    let changes = repo.diff_trees(&parent.tree, &state.tree).ok()?;
    Some(ThreadCaptureSummary {
        added: changes.added_count(),
        modified: changes.modified_count(),
        deleted: changes.deleted_count(),
        total: changes.len(),
    })
}

fn thread_capture_output(
    state: &State,
    summary: Option<ThreadCaptureSummary>,
) -> ThreadCaptureOutput {
    let agent = state
        .attribution
        .agent
        .as_ref()
        .map(|agent| format!("{}/{}", agent.provider, agent.model));
    let message = state
        .intent
        .clone()
        .unwrap_or_else(|| format!("Capture {}", state.change_id.short()));
    ThreadCaptureOutput {
        change_id: state.change_id.short(),
        created_at: state.created_at.to_rfc3339(),
        intent: state.intent.clone(),
        confidence: state.confidence,
        agent,
        message,
        summary,
    }
}

pub fn collect_thread_summaries(repo: &Repository) -> Result<Vec<ThreadSummary>> {
    let thread_refs = repo.refs().list_threads()?;
    let current = repo.current_lane()?;
    let operation = repo.operation_status()?;
    let remote_tracking = repo.git_remote_tracking_status().unwrap_or(None);
    let import_hint = repo.git_overlay_import_hint().unwrap_or(None);
    let branch_tips = repo
        .git_overlay_branch_tips()
        .unwrap_or_default()
        .into_iter()
        .map(|tip| (tip.branch.clone(), tip))
        .collect::<HashMap<_, _>>();
    let registry = AgentRegistry::new(repo.heddle_dir());
    let thread_manager = ThreadManager::new(repo.heddle_dir());
    let mut entries_by_thread: HashMap<String, Vec<AgentEntry>> = HashMap::new();
    let mut threads_by_name: HashMap<String, Thread> = HashMap::new();
    for entry in registry.list()? {
        entries_by_thread
            .entry(entry.thread.clone())
            .or_default()
            .push(entry);
    }
    for mut thread in thread_manager.list()? {
        if thread.state == ThreadState::Abandoned
            && repo.refs().get_thread(&ThreadName::new(&thread.thread))?.is_none()
        {
            continue;
        }
        refresh_thread_freshness(repo, &mut thread)?;
        threads_by_name.insert(thread.thread.clone(), thread);
    }

    let mut names: BTreeSet<String> = thread_refs.iter().map(|t| t.to_string()).collect();
    names.extend(current.iter().cloned());
    names.extend(entries_by_thread.keys().cloned());
    names.extend(threads_by_name.keys().cloned());
    names.extend(branch_tips.keys().cloned());

    let mut summaries = Vec::new();
    for name in names {
        let (view, coordination_status) = build_thread_view(
            repo,
            current.as_ref() == Some(&name),
            name.clone(),
            entries_by_thread.remove(&name).unwrap_or_default(),
            threads_by_name.remove(&name),
            branch_tips.get(&name).cloned(),
        )?;
        let mut summary = ThreadSummary::from_view(view, coordination_status);
        if let Some(branch_tip) = branch_tips.get(&summary.name) {
            summary.git_branch_tip = Some(branch_tip.git_commit.clone());
            summary.history_imported = branch_tip.history_imported;
        }
        let has_heddle_tip = thread_refs.iter().any(|thread| thread == &summary.name);
        let thread = Thread {
            id: summary.name.clone(),
            thread: summary.name.clone(),
            target_thread: summary.target_thread.clone(),
            parent_thread: summary.parent_thread.clone(),
            mode: summary
                .thread_mode
                .clone()
                .unwrap_or(ThreadMode::Materialized),
            state: summary.thread_state.clone().unwrap_or(ThreadState::Active),
            base_state: summary.base_state.clone().unwrap_or_default(),
            base_root: summary.base_root.clone().unwrap_or_default(),
            current_state: summary.current_state.clone(),
            merged_state: None,
            task: summary.task.clone(),
            execution_path: summary
                .execution_path
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| repo.root().to_path_buf()),
            materialized_path: summary.path.as_ref().map(PathBuf::from),
            changed_paths: summary.changed_paths.clone(),
            impact_categories: summary.impact_categories.clone(),
            heavy_impact_paths: summary.heavy_impact_paths.clone(),
            promotion_suggested: summary.promotion_suggested,
            freshness: summary
                .freshness
                .clone()
                .unwrap_or(ThreadFreshness::Unknown),
            verification_summary: summary.verification_summary.clone(),
            confidence_summary: summary.confidence_summary.clone(),
            integration_policy_result: summary.integration_policy_result.clone(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            ephemeral: None,
            auto: summary.auto,
            shared_target_dir: summary.shared_target_dir.as_ref().map(PathBuf::from),
        };
        let advice = describe_thread_advice(&thread, false, 0, false);
        summary.thread_health = advice.thread_health;
        summary.blockers = advice.blockers;
        summary.recommended_action = advice.recommended_action;
        apply_terminal_thread_advice(&mut summary);
        if let Some(branch_tip) = branch_tips.get(&summary.name)
            && !has_heddle_tip
        {
            if branch_tip.history_imported {
                summary.blockers.clear();
                if !summary.is_current {
                    summary.recommended_action = canonical_adopt_ref_command(&branch_tip.branch);
                }
            } else {
                summary.thread_health = "tip_only".to_string();
                summary.recommended_action = canonical_adopt_ref_command(&branch_tip.branch);
                if summary.is_current {
                    summary.blockers = vec![
                        "Heddle has not imported this Git branch history yet; import before using history-oriented commands".to_string(),
                    ];
                } else {
                    summary.blockers.clear();
                }
            }
        }
        if summary.history_imported
            && summary.current_state.is_some()
            && remote_tracking_local_ref(repo, &summary.name).is_some()
        {
            summary.thread_health = "remote_tracking".to_string();
            summary.coordination_status = CoordinationStatus::Clean;
            summary.blockers.clear();
            summary.recommended_action =
                canonical_bridge_reconcile_ref_preview_command(None, &summary.name);
        }
        if summary.is_current {
            enrich_current_summary_with_dirty_paths(repo, &mut summary)?;
            summary.operation = operation.clone();
            summary.remote_tracking = remote_tracking.clone();
            summary.recommended_action = current_thread_next_action(
                operation.as_ref(),
                remote_tracking.as_ref(),
                import_hint.as_ref(),
                Some(&summary.thread_health),
                Some(&summary.recommended_action),
            );
            summary.recommended_action = contextual_thread_action(
                repo,
                &summary.name,
                summary.target_thread.as_deref(),
                &summary.recommended_action,
            );
        }
        summaries.push(summary);
    }

    let mut children_by_parent: HashMap<String, Vec<String>> = HashMap::new();
    for summary in &summaries {
        if let Some(parent) = &summary.parent_thread {
            children_by_parent
                .entry(parent.clone())
                .or_default()
                .push(summary.name.clone());
        }
    }
    for summary in &mut summaries {
        if let Some(children) = children_by_parent.remove(&summary.name) {
            let mut children = children;
            children.sort();
            summary.child_threads = children;
        }
    }

    let summaries_by_name = summaries
        .iter()
        .map(|summary| (summary.name.clone(), summary.clone()))
        .collect::<HashMap<_, _>>();
    let mut siblings_by_thread: HashMap<String, Vec<String>> = HashMap::new();
    for summary in &summaries {
        if let Some(parent) = &summary.parent_thread {
            let siblings = summaries_by_name
                .values()
                .filter(|candidate| candidate.parent_thread.as_deref() == Some(parent.as_str()))
                .filter(|candidate| candidate.name != summary.name)
                .map(|candidate| candidate.name.clone())
                .collect::<Vec<_>>();
            siblings_by_thread.insert(summary.name.clone(), siblings);
        }
    }
    for summary in &mut summaries {
        summary.sibling_threads = siblings_by_thread.remove(&summary.name).unwrap_or_default();
        summary.stack_depth = stack_depth(&summaries_by_name, &summary.name);
        summary.stale_from_parent =
            summary.parent_thread.is_some() && summary.freshness == Some(ThreadFreshness::Stale);
        if summary.last_progress_at.is_some() {
            summary.last_activity_at = summary.last_progress_at.clone();
        }
        summary.recommended_action_template =
            recommended_action_template(&summary.recommended_action);
    }

    summaries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(summaries)
}

fn enrich_current_summary_with_dirty_paths(
    repo: &Repository,
    summary: &mut ThreadSummary,
) -> Result<()> {
    let baseline = match repo.current_state()? {
        Some(state) => repo.require_tree(&state.tree)?,
        None => Tree::new(),
    };
    let status = repo.compare_worktree_cached_with_options(
        &baseline,
        &worktree_status_options(Some(repo.config())),
    )?;
    let mut paths = summary
        .changed_paths
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    paths.extend(
        status
            .modified
            .iter()
            .chain(status.added.iter())
            .chain(status.deleted.iter())
            .map(|path| path.to_string_lossy().to_string()),
    );
    summary.changed_paths = paths.into_iter().collect();
    Ok(())
}

pub(crate) fn suppress_thread_actions_while_trust_blocked(
    summaries: &mut [ThreadSummary],
    trust: &RepositoryVerificationState,
) {
    if trust.verified {
        return;
    }
    let blocker = if trust.summary.trim().is_empty() {
        format!("Repository verification is {}", trust.status)
    } else {
        trust.summary.clone()
    };
    for summary in summaries {
        if summary.thread_health == "remote_tracking" {
            summary.recommended_action_template =
                recommended_action_template(&summary.recommended_action);
            continue;
        }
        summary.thread_health = trust.status.clone();
        summary.coordination_status = CoordinationStatus::Blocked;
        if !summary.blockers.iter().any(|existing| existing == &blocker) {
            summary.blockers.insert(0, blocker.clone());
        }
        if trust.status == "needs_import"
            && summary
                .recommended_action
                .starts_with("heddle adopt --ref ")
        {
            summary.recommended_action_template =
                recommended_action_template(&summary.recommended_action);
            continue;
        }
        summary.recommended_action.clear();
        summary.recommended_action_template = None;
    }
}

fn stack_depth(summaries_by_name: &HashMap<String, ThreadSummary>, thread: &str) -> usize {
    let mut depth = 0usize;
    let mut cursor = summaries_by_name
        .get(thread)
        .and_then(|summary| summary.parent_thread.clone());
    while let Some(parent) = cursor {
        depth += 1;
        cursor = summaries_by_name
            .get(&parent)
            .and_then(|summary| summary.parent_thread.clone());
    }
    depth
}

fn build_thread_view(
    repo: &Repository,
    is_current: bool,
    name: String,
    entries: Vec<AgentEntry>,
    thread: Option<Thread>,
    branch_tip: Option<GitOverlayBranchTip>,
) -> Result<(ThreadView, CoordinationStatus)> {
    let ref_state = repo.refs().get_thread(&ThreadName::new(&name))?;
    let current_state = ref_state
        .or_else(|| {
            (is_current && repo.capability() == repo::RepositoryCapability::GitOverlay)
                .then(|| {
                    branch_tip
                        .as_ref()
                        .and_then(|tip| tip.mapped_change)
                        .or_else(|| {
                            repo.git_overlay_mapped_change_for_branch(&name)
                                .ok()
                                .flatten()
                        })
                })
                .flatten()
        })
        .map(|id| id.short());
    let has_heddle_tip = current_state.is_some();
    let active: Vec<&AgentEntry> = entries
        .iter()
        .filter(|entry| entry.status == AgentStatus::Active)
        .collect();
    let complete: Vec<&AgentEntry> = entries
        .iter()
        .filter(|entry| entry.status == AgentStatus::Complete)
        .collect();

    let primary = active
        .iter()
        .max_by_key(|entry| entry.started_at)
        .copied()
        .or_else(|| entries.iter().max_by_key(|entry| entry.started_at));
    let base_state = thread
        .as_ref()
        .map(|thread| thread.base_state.clone())
        .or_else(|| primary.map(|entry| entry.base_state.clone()))
        .or(current_state.clone());
    let base_root = thread.as_ref().map(|thread| thread.base_root.clone());
    let runtime = ThreadRuntimeOverlay {
        path: thread
            .as_ref()
            .and_then(|thread| thread.materialized_path.clone())
            .or_else(|| primary.and_then(|entry| entry.path.clone())),
        execution_path: thread.as_ref().map(|thread| thread.execution_path.clone()),
        materialized_path: thread
            .as_ref()
            .and_then(|thread| thread.materialized_path.clone()),
        session_id: primary.map(|entry| entry.session_id.clone()),
        heddle_session_id: primary.and_then(|entry| entry.heddle_session_id.clone()),
        harness: primary.and_then(|entry| entry.harness.clone()),
        thinking_level: primary.and_then(|entry| entry.thinking_level.clone()),
        native_actor_key: primary.and_then(|entry| entry.native_actor_key.clone()),
        native_parent_actor_key: primary.and_then(|entry| entry.native_parent_actor_key.clone()),
        probe_source: primary.and_then(|entry| entry.probe_source.clone()),
        probe_confidence: primary.and_then(|entry| entry.probe_confidence),
        usage_summary: primary.map(|entry| entry.usage_summary.clone()),
        last_progress_at: primary.and_then(|entry| entry.last_progress_at),
        report_flush_state: primary.and_then(|entry| entry.report_flush_state.clone()),
        attach_reason: primary.and_then(|entry| entry.attach_reason.clone()),
        provider: primary.and_then(|entry| entry.provider.clone()),
        model: primary.and_then(|entry| entry.model.clone()),
        thread_mode: thread.as_ref().map(|thread| thread.mode.clone()),
        thread_state: thread.as_ref().map(|thread| thread.state.clone()),
    };
    let thread_record = thread.as_ref().map(|thread| thread.to_record());
    let thread_state_for_status = thread_record.as_ref().map(|thread| thread.state.clone());
    let coordination_status = if matches!(
        thread_state_for_status,
        Some(ThreadState::Merged | ThreadState::Abandoned)
    ) {
        CoordinationStatus::Clean
    } else if thread_state_for_status == Some(ThreadState::Blocked) {
        CoordinationStatus::Blocked
    } else if thread_state_for_status == Some(ThreadState::Ready) {
        CoordinationStatus::MergeReady
    } else if active.len() > 1 {
        CoordinationStatus::Blocked
    } else if !active.is_empty()
        && complete
            .iter()
            .any(|entry| entry.base_state != active[0].base_state)
    {
        CoordinationStatus::Diverged
    } else if !complete.is_empty() {
        CoordinationStatus::MergeReady
    } else if base_state.is_some() && current_state.is_some() && base_state != current_state {
        CoordinationStatus::Ahead
    } else {
        CoordinationStatus::Clean
    };

    let view = match thread {
        Some(mut thread) => {
            thread.current_state = current_state;
            thread.to_view(runtime, is_current)
        }
        None => ThreadView::from_record(
            repo::ThreadRecord {
                id: name.clone(),
                thread: name.clone(),
                target_thread: None,
                parent_thread: None,
                mode: ThreadMode::Materialized,
                state: ThreadState::Active,
                base_state: base_state.unwrap_or_default(),
                base_root: base_root.unwrap_or_default(),
                current_state,
                merged_state: None,
                task: None,
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
            },
            runtime,
            is_current,
        ),
    };

    if let Some(branch_tip) = branch_tip
        && !has_heddle_tip
        && view.record.current_state.is_none()
    {
        let mut record = view.record.clone();
        record.current_state = None;
        let mut runtime = view.runtime.clone();
        if runtime.attach_reason.is_none() {
            runtime.attach_reason = Some(format!(
                "auto-adopted Git branch tip {}",
                branch_tip.git_commit
            ));
        }
        return Ok((
            ThreadView::from_record(record, runtime, is_current),
            coordination_status,
        ));
    }

    Ok((view, coordination_status))
}

pub fn find_thread_summary(repo: &Repository, name: &str) -> Result<Option<ThreadSummary>> {
    Ok(collect_thread_summaries(repo)?
        .into_iter()
        .find(|summary| summary.name == name))
}

/// Fast single-thread summary. Skips the full `collect_thread_summaries`
/// walk (which reads every thread record, every agent entry, every git
/// branch tip — 45ms on a 69-thread repo) in favor of reading just the
/// one thread we care about.
///
/// Trade-offs vs. the full path:
/// - `child_threads` / `sibling_threads` are always empty. Computing
///   them needs a global parent-thread scan; callers that display these
///   relations should route through `find_thread_summary` instead.
/// - `git_branch_tip` and `history_imported` are not populated for
///   the same reason — discovering them needs the full gix branch walk.
///   `tip_only` thread_health is therefore not surfaced; the import-
///   hint line on the surrounding render already nudges the user.
///
/// Used by `heddle status` on the default text path where none of the
/// above fields are rendered. JSON and `-v` still go through the
/// full walk because those surfaces actually display the relations.
pub fn find_thread_summary_single(repo: &Repository, name: &str) -> Result<Option<ThreadSummary>> {
    let current = repo.current_lane()?;
    let is_current = current.as_deref() == Some(name);
    // Just this thread's record.
    let thread_manager = ThreadManager::new(repo.heddle_dir());
    let mut thread_record = thread_manager.find_by_thread(name)?;
    if let Some(thread) = thread_record.as_mut() {
        refresh_thread_freshness(repo, thread)?;
    }
    // Just this thread's agent entries.
    let registry = AgentRegistry::new(repo.heddle_dir());
    let entries: Vec<AgentEntry> = registry
        .list()?
        .into_iter()
        .filter(|entry| entry.thread == name)
        .collect();

    let (view, coordination_status) = build_thread_view(
        repo,
        is_current,
        name.to_string(),
        entries,
        thread_record,
        None, // skip branch_tip lookup (would require full gix walk)
    )?;
    let mut summary = ThreadSummary::from_view(view, coordination_status);

    // Re-run the per-thread fixups that `collect_thread_summaries` applies.
    let thread_for_advice = Thread {
        id: summary.name.clone(),
        thread: summary.name.clone(),
        target_thread: summary.target_thread.clone(),
        parent_thread: summary.parent_thread.clone(),
        mode: summary
            .thread_mode
            .clone()
            .unwrap_or(ThreadMode::Materialized),
        state: summary.thread_state.clone().unwrap_or(ThreadState::Active),
        base_state: summary.base_state.clone().unwrap_or_default(),
        base_root: summary.base_root.clone().unwrap_or_default(),
        current_state: summary.current_state.clone(),
        merged_state: None,
        task: summary.task.clone(),
        execution_path: summary
            .execution_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| repo.root().to_path_buf()),
        materialized_path: summary.path.as_ref().map(PathBuf::from),
        changed_paths: summary.changed_paths.clone(),
        impact_categories: summary.impact_categories.clone(),
        heavy_impact_paths: summary.heavy_impact_paths.clone(),
        promotion_suggested: summary.promotion_suggested,
        freshness: summary
            .freshness
            .clone()
            .unwrap_or(ThreadFreshness::Unknown),
        verification_summary: summary.verification_summary.clone(),
        confidence_summary: summary.confidence_summary.clone(),
        integration_policy_result: summary.integration_policy_result.clone(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        ephemeral: None,
        auto: summary.auto,
        shared_target_dir: summary.shared_target_dir.as_ref().map(PathBuf::from),
    };
    let advice = describe_thread_advice(&thread_for_advice, false, 0, false);
    summary.thread_health = advice.thread_health;
    summary.blockers = advice.blockers;
    summary.recommended_action = advice.recommended_action;
    apply_terminal_thread_advice(&mut summary);
    if is_current {
        // Current-thread next-action enrichment. Same as the full path,
        // but we skip the operation/remote_tracking/import_hint reads
        // because the caller (status) already has those and threads
        // through different fields anyway.
        summary.recommended_action =
            primary_next_action(None, None, None, Some(&summary.recommended_action));
        summary.recommended_action = contextual_thread_action(
            repo,
            &summary.name,
            summary.target_thread.as_deref(),
            &summary.recommended_action,
        );
    }
    summary.recommended_action_template = recommended_action_template(&summary.recommended_action);
    Ok(Some(summary))
}

pub(crate) fn contextual_thread_action(
    repo: &Repository,
    thread_id: &str,
    target_thread: Option<&str>,
    action: &str,
) -> String {
    super::thread_landing::contextual_thread_action(repo, thread_id, target_thread, action)
}

pub(crate) fn current_thread_next_action_with_verification(
    operation: Option<&RepositoryOperationStatus>,
    remote_tracking: Option<&GitRemoteTrackingStatus>,
    import_hint: Option<&GitOverlayImportHint>,
    thread_health: Option<&str>,
    thread_action: Option<&str>,
    trust: &RepositoryVerificationState,
) -> String {
    let fallback = non_empty_action_ref(thread_action)
        .or_else(|| non_empty_action_ref(Some(trust.recommended_action.as_str())));
    effective_next_action(
        NextActionInput::default(operation, remote_tracking, import_hint, fallback)
            .current_thread(thread_health)
            .with_verification(trust),
    )
}

pub(crate) fn current_thread_next_action(
    operation: Option<&RepositoryOperationStatus>,
    remote_tracking: Option<&GitRemoteTrackingStatus>,
    import_hint: Option<&GitOverlayImportHint>,
    thread_health: Option<&str>,
    thread_action: Option<&str>,
) -> String {
    effective_next_action(
        NextActionInput::default(operation, remote_tracking, import_hint, thread_action)
            .current_thread(thread_health),
    )
}

pub(crate) fn thread_recovery_action_is_primary(
    thread_health: Option<&str>,
    thread_action: &str,
) -> bool {
    shared_thread_recovery_action_is_primary(thread_health, thread_action)
}

fn non_empty_action_ref(action: Option<&str>) -> Option<&str> {
    action.filter(|action| !action.trim().is_empty())
}

fn apply_terminal_thread_advice(summary: &mut ThreadSummary) {
    match summary.thread_state {
        Some(ThreadState::Merged) => {
            summary.thread_health = "clean".to_string();
            summary.blockers.clear();
            summary.recommended_action = "heddle thread cleanup --merged --dry-run".to_string();
            summary.coordination_status = CoordinationStatus::Clean;
        }
        Some(ThreadState::Abandoned) => {
            summary.thread_health = "clean".to_string();
            summary.blockers.clear();
            summary.recommended_action.clear();
            summary.coordination_status = CoordinationStatus::Clean;
        }
        _ => {}
    }
}

pub(crate) fn visibility_label(mode: &ThreadMode) -> &'static str {
    match mode {
        ThreadMode::Materialized => "materialized",
        ThreadMode::Virtualized => "virtualized",
        ThreadMode::Solid => "solid",
    }
}

pub(crate) fn thread_workspace_label(mode: &ThreadMode) -> &'static str {
    match mode {
        ThreadMode::Materialized => "main checkout",
        ThreadMode::Virtualized => "virtual checkout",
        ThreadMode::Solid => "isolated checkout",
    }
}

pub(crate) fn thread_human_visibility(summary: &ThreadSummary) -> &str {
    if thread_is_imported_git_ref(summary) {
        return "imported Git branch";
    }
    if !summary.is_isolated && summary.path.is_none() && summary.execution_path.is_none() {
        return "no dedicated checkout";
    }
    summary
        .thread_mode
        .as_ref()
        .map(thread_workspace_label)
        .unwrap_or(&summary.visibility)
}

/// Compact glyph appended to a thread-list row to indicate whether
/// the thread's worktree is live on disk *right now*. Three states:
///
/// * `" ●"` (accent-coloured) — a path is recorded and exists. The
///   common case for a thread the user is actively working in.
/// * `" ✗"` (warn-coloured)   — a path is recorded but the directory
///   is gone (user did `rm -rf` or the disk was reset). Surfacing
///   this is the whole point of the glyph — silent "ghost" threads
///   are the friction we're trying to remove.
/// * `""` (empty)             — no path recorded (pure-ref thread,
///   never started; or `thread create` without `thread start`).
///   Glyphless because there's nothing meaningful to indicate.
///
/// Virtualised threads with a recorded mount point go through the
/// same `Path::exists()` check — if the FUSE/FSKit/ProjFS daemon is
/// dead the mount directory is still a real (empty) dir, so this
/// is honest. A `(stale)` advisory would be misleading.
fn thread_liveness_glyph(entry: &ThreadSummary) -> String {
    let path = entry.path.as_deref().or(entry.execution_path.as_deref());
    let Some(path) = path else {
        return String::new();
    };
    if std::path::Path::new(path).exists() {
        format!(" {}", style::accent("●"))
    } else {
        format!(" {}", style::warn("✗"))
    }
}

pub(crate) fn git_history_label(history_imported: bool) -> &'static str {
    if history_imported {
        "full history available"
    } else {
        "tip available"
    }
}

fn render_repository_context_lines(context: Option<&crate::cli::render::RepositoryContextInfo>) {
    let Some(context) = context else {
        return;
    };
    if let Some(parent_repository) = &context.parent_repository {
        println!("Parent repo: {}", parent_repository);
    }
    if let Some(target_thread) = &context.target_thread {
        println!("Target thread: {}", target_thread);
    }
    if let Some(parent_thread) = &context.parent_thread {
        println!("Parent thread: {}", parent_thread);
    }
}

pub(crate) fn split_available_git_refs(summaries: &mut Vec<ThreadSummary>) -> Vec<AvailableGitRef> {
    let mut available = Vec::new();
    summaries.retain(|summary| {
        if thread_is_available_git_ref(summary) {
            available.push(available_git_ref_from_summary(summary));
            false
        } else {
            true
        }
    });
    available
}

fn available_git_ref_from_summary(summary: &ThreadSummary) -> AvailableGitRef {
    AvailableGitRef {
        name: summary.name.clone(),
        git_commit: summary.git_branch_tip.clone().unwrap_or_default(),
        recommended_action: summary.recommended_action.clone(),
        recommended_action_template: summary
            .recommended_action_template
            .clone()
            .or_else(|| recommended_action_template(&summary.recommended_action)),
    }
}

pub(crate) fn cmd_thread_list(cli: &Cli, repo: &Repository, args: ThreadListArgs) -> Result<()> {
    let as_json = should_output_json(cli, Some(repo.config()));
    let current = repo.current_lane()?;
    let mut summaries = collect_thread_summaries(repo)?;
    let mut trust = build_repository_verification_state(repo);
    if !args.include_auto {
        // Always keep the current thread visible even if it's auto:
        // hiding it from the user who is *standing in it* would be
        // worse than the noise it adds.
        summaries.retain(|summary| summary.is_current || !summary.auto);
    }
    let available_git_refs = split_available_git_refs(&mut summaries);
    suppress_thread_actions_while_trust_blocked(&mut summaries, &trust);
    let current_summary = summaries.iter().find(|summary| summary.is_current);
    if let Some(current) = current_summary
        && !trust.recommended_action.is_empty()
    {
        let contextual = contextual_thread_action(
            repo,
            &current.name,
            current.target_thread.as_deref(),
            &trust.recommended_action,
        );
        if contextual != trust.recommended_action {
            override_trust_recommended_action(&mut trust, contextual);
        }
    }
    let current_action = summaries
        .iter()
        .find(|summary| summary.is_current)
        .map(|summary| summary.recommended_action.as_str());
    let recommended_action =
        primary_next_action_with_verification(None, None, None, current_action, &trust);
    if let Some(current) = current_summary
        && trust.verified
        && !recommended_action.is_empty()
        && trust.recommended_action != recommended_action
        && thread_recovery_action_is_primary(Some(&current.thread_health), &recommended_action)
    {
        override_trust_recommended_action(&mut trust, recommended_action.clone());
    }
    let presentation = crate::cli::render::repository_presentation(
        repo,
        current_summary.and_then(|summary| summary.target_thread.as_deref()),
        current_summary.and_then(|summary| summary.parent_thread.as_deref()),
    );
    let current = current_summary
        .map(|summary| summary.name.clone())
        .or(current);
    let output = ThreadListOutput {
        output_kind: "thread_list",
        repository_capability: repo.capability_label().to_string(),
        repository_label: presentation.label,
        repository_context: presentation.context,
        storage_model: repo.storage_model_label().to_string(),
        hosted_enabled: repo.hosted_enabled(),
        recommended_action: recommended_action.clone(),
        recommended_action_template: recommended_action_template(&recommended_action),
        recovery_commands: trust.recovery_commands.clone(),
        recovery_action_templates: trust.recovery_action_templates.clone(),
        trust,
        git_overlay_import_hint: if as_json {
            None
        } else {
            repo.git_overlay_import_hint()?
                .map(|hint| ThreadListGitOverlayImportHintOutput {
                    current_branch: hint.current_branch,
                    missing_branch_count: hint.missing_branch_count,
                    missing_branches: hint.missing_branches,
                    recommended_command: hint.recommended_command,
                })
        },
        threads: summaries,
        available_git_refs,
        current,
    };

    if as_json {
        write_full_command_json(
            &output,
            NextActionValidationContext::new(&["thread", "list"], repo.capability()),
        )?;
    } else if output.threads.is_empty() && output.available_git_refs.is_empty() {
        println!("No threads");
    } else {
        println!(
            "{} {} {}",
            style::bold("Threads"),
            style::dim("in"),
            output.repository_label
        );
        println!("Repository: {}", output.repository_label);
        render_repository_context_lines(output.repository_context.as_ref());
        if output.hosted_enabled {
            println!("Hosted: {}", style::accent("enabled"));
        }
        let trust_only_blocks_on_this_ready_thread = output.trust.workflow_status == "ready"
            && output.trust.recommended_action == output.recommended_action;
        if !output.trust.verified
            && !trust_only_blocks_on_this_ready_thread
            && !output.recommended_action.is_empty()
        {
            println!("Verification: {}", style::warn(&output.trust.summary));
            print_next_step(&output.recommended_action);
        }
        if output.trust.verified
            && let Some(hint) = &output.git_overlay_import_hint
        {
            println!(
                "{}",
                crate::cli::render::git_only_branch_summary(
                    &hint.missing_branches,
                    hint.missing_branch_count,
                )
            );
            if output.available_git_refs.is_empty() {
                print_optional(&hint.recommended_command);
            }
        }
        render_thread_sections(&output.threads, cli.verbose > 0);
        render_available_git_refs(&output.available_git_refs, cli.verbose > 0);
    }

    Ok(())
}

type ThreadSectionPredicate = fn(&ThreadSummary) -> bool;
type ThreadSection = (&'static str, ThreadSectionPredicate);

fn render_thread_sections(threads: &[ThreadSummary], verbose: bool) {
    let sections: [ThreadSection; 5] = [
        ("Current", |entry| entry.is_current),
        ("Needs attention", thread_needs_attention),
        ("Ready to merge", thread_ready_to_merge),
        ("Imported Git refs", thread_is_imported_git_ref),
        ("Other threads", |_| true),
    ];

    let mut printed = vec![false; threads.len()];
    for (label, predicate) in sections {
        let indexes = threads
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| (!printed[index] && predicate(entry)).then_some(index))
            .collect::<Vec<_>>();
        if indexes.is_empty() {
            continue;
        }
        println!();
        println!("{}", style::bold(label));
        for index in indexes {
            printed[index] = true;
            render_thread_entry(&threads[index], verbose);
        }
    }
}

fn thread_needs_attention(entry: &ThreadSummary) -> bool {
    !entry.blockers.is_empty()
        || entry.operation.is_some()
        || entry.coordination_status == CoordinationStatus::Blocked
        || entry.coordination_status == CoordinationStatus::Diverged
}

fn thread_ready_to_merge(entry: &ThreadSummary) -> bool {
    entry.coordination_status == CoordinationStatus::MergeReady
        || (entry.coordination_status == CoordinationStatus::Ahead
            && entry.thread_state != Some(ThreadState::Merged)
            && entry.target_thread.is_some())
}

pub(crate) fn thread_is_imported_git_ref(entry: &ThreadSummary) -> bool {
    !entry.is_current
        && entry.path.is_none()
        && entry.execution_path.is_none()
        && entry.target_thread.is_none()
        && entry.current_state.is_some()
        && entry.history_imported
        && (entry.git_branch_tip.is_some() || entry.name.starts_with("origin/"))
}

pub(crate) fn thread_is_available_git_ref(entry: &ThreadSummary) -> bool {
    !entry.is_current
        && entry.path.is_none()
        && entry.execution_path.is_none()
        && entry.target_thread.is_none()
        && entry.current_state.is_none()
        && entry.git_branch_tip.is_some()
}

fn remote_tracking_local_ref(repo: &Repository, thread_name: &str) -> Option<String> {
    let git = gix::discover(repo.root()).ok()?;
    let remotes = git
        .remote_names()
        .into_iter()
        .map(|name| name.to_str_lossy().into_owned())
        .collect::<Vec<_>>();
    remotes
        .iter()
        .find_map(|remote| thread_name.strip_prefix(&format!("{remote}/")))
        .filter(|branch| !branch.is_empty())
        .map(str::to_string)
}

fn render_available_git_refs(refs: &[AvailableGitRef], verbose: bool) {
    if refs.is_empty() {
        return;
    }
    println!();
    println!("{}", style::bold("Optional Git-only branches"));
    let visible_count = if verbose {
        refs.len()
    } else {
        refs.len().min(DEFAULT_AVAILABLE_GIT_REF_LIMIT)
    };
    for entry in refs.iter().take(visible_count) {
        println!(
            "{} {} {}",
            style::dim("-"),
            style::bold(&entry.name),
            style::dim("(available)")
        );
        if verbose {
            println!("    git tip: {}", style::dim(&entry.git_commit));
        }
        if !entry.recommended_action.is_empty() {
            print_nested_optional(&entry.recommended_action);
        }
    }
    println!(
        "  {}",
        style::dim("adopt when you want to work on this branch in Heddle")
    );
    if !verbose && refs.len() > visible_count {
        let remaining = refs.len() - visible_count;
        println!(
            "  {}",
            style::dim(&format!(
                "... {remaining} more Git-only branch(es); use --output json or -v to inspect all"
            ))
        );
    }
}

fn render_thread_entry(entry: &ThreadSummary, verbose: bool) {
    let prefix = if entry.is_current {
        style::accent("*")
    } else {
        style::dim("-")
    };
    // Worktree-liveness glyph after the mode label. Lets the user
    // tell at a glance which threads have an actual on-disk worktree
    // they can `cd` to vs. which are pure refs (or virtual mounts
    // whose daemon may or may not be up). One stat per row — cheap
    // for any sane thread count, no I/O at all on ref-only threads.
    let liveness = thread_liveness_glyph(entry);
    if verbose {
        let state = entry.current_state.as_deref().unwrap_or("(no state)");
        println!(
            "{} {} {} {} {}{}",
            prefix,
            style::bold(&entry.name),
            style::dim(state),
            style::thread_state(&entry.coordination_status.to_string()),
            style::dim(thread_human_visibility(entry)),
            liveness,
        );
    } else {
        println!(
            "{} {} {} {}{}",
            prefix,
            style::bold(&entry.name),
            style::thread_state(&entry.coordination_status.to_string()),
            style::dim(thread_human_visibility(entry)),
            liveness,
        );
    }
    if let Some(path) = &entry.path {
        println!("    path: {}", path);
    } else if let Some(path) = &entry.execution_path {
        println!("    execution root: {}", path);
    }
    if verbose && let Some(git_branch_tip) = &entry.git_branch_tip {
        println!(
            "    git tip: {} {}",
            style::dim(git_branch_tip),
            style::dim(&format!("({})", git_history_label(entry.history_imported)))
        );
    }
    if let Some(state) = &entry.thread_state
        && (verbose || matches!(state, ThreadState::Merged | ThreadState::Abandoned))
    {
        println!("    lifecycle: {}", style::thread_state(&state.to_string()));
    }
    if let Some(freshness) = &entry.freshness
        && *freshness != ThreadFreshness::Unknown
        && !matches!(
            entry.thread_state,
            Some(ThreadState::Merged | ThreadState::Abandoned)
        )
    {
        println!("    sync: {}", style::thread_state(&freshness.to_string()));
    }
    if let Some(operation) = &entry.operation {
        println!(
            "    in progress: {} {} ({})",
            style::warn(&operation.scope.to_string()),
            style::warn(&operation.kind.to_string()),
            style::dim(&operation.state)
        );
    }
    if let Some(remote_tracking) = &entry.remote_tracking {
        if remote_tracking.behind == 0 && remote_tracking.ahead > 0 {
            println!("    sync: {}", style::accent(&remote_tracking.message));
        } else {
            println!("    sync: {}", style::warn(&remote_tracking.message));
        }
    }
    if verbose
        && let Some(actor) = &entry.actor
        && let Some(text) =
            crate::cli::render::actor_display(actor.provider.as_deref(), actor.model.as_deref())
    {
        println!("    actor: {text}");
    }
    if let Some(task) = &entry.task {
        println!("    task: {}", task);
    }
    if verbose && let Some(parent) = &entry.parent_thread {
        println!("    parent: {}", parent);
    }
    if verbose && !entry.child_threads.is_empty() {
        println!("    children: {}", entry.child_threads.join(", "));
    }
    if verbose && entry.promotion_suggested && !entry.heavy_impact_paths.is_empty() {
        println!(
            "    promotion: suggested ({})",
            crate::cli::render::preview_list(
                &entry.heavy_impact_paths,
                entry.heavy_impact_paths.len(),
            )
        );
    }
    if verbose && !entry.impact_categories.is_empty() {
        println!(
            "    impacts: {}",
            entry
                .impact_categories
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !entry.blockers.is_empty() {
        println!(
            "    blocked by: {}",
            style::warn(&entry.blockers.join(" | "))
        );
    }
    if !entry.recommended_action.is_empty() && thread_is_available_git_ref(entry) {
        print_nested_optional(&entry.recommended_action);
    } else if !entry.recommended_action.is_empty() {
        print_nested_next_step(&entry.recommended_action);
    }
}

/// Retry-stable idempotency key for `thread start`.
///
/// Folds the op scope, the thread name, the resolved base state, AND a per-start
/// epoch ([`resolve_start_epoch`] — the in-flight thread record's creation
/// instant). Two properties have to hold at once:
///
///   * **Stable across a crash-retry of the *same* start.** It deliberately does
///     NOT fold in the live oplog head: that advances when the
///     `TransactionCommit` marker appends, so a post-commit crash-retry would
///     read the advanced head, derive a *fresh* key, miss the executor's
///     `transaction_id` dedup, and re-apply an already-committed start
///     (heddle#356 cid 3333881568). A crash-retry finds the still-Active thread
///     record and reuses its creation instant, so it re-derives this exact key
///     and dedups exactly-once.
///   * **Fresh for a genuinely-new start after a prior one was dropped.** A
///     silent drop (no `--delete-thread`) leaves the ref at `base_state`, so a
///     key folding only scope+name+base would collide with the dropped start's
///     committed marker and the new start would be wrongly deduped into a no-op
///     (heddle#356 cid 3335052848). The epoch breaks the collision: a post-drop
///     restart finds an Abandoned (non-Active) record and mints a fresh epoch,
///     so it derives a DISTINCT key and actually starts.
pub(crate) fn start_transaction_id(
    scope: &str,
    name: &str,
    base_state: &ChangeId,
    start_epoch: DateTime<Utc>,
) -> String {
    format!(
        "thread-start:{scope}:{name}:{}:{}",
        base_state.to_string_full(),
        start_epoch.timestamp_nanos_opt().unwrap_or_default(),
    )
}

/// The per-start epoch folded into the idempotency key ([`start_transaction_id`]).
///
/// The thread record is the durable "start reservation": it is written inside
/// the transaction (before the commit point) and survives a crash, so it is the
/// one artifact a crash-retry can read back. A crash-retry of an in-flight start
/// finds the still-[`ThreadState::Active`] record and reuses its creation
/// instant → identical key → the executor dedups it exactly-once. A
/// genuinely-new start — no record, or a record a prior silent drop left
/// [`ThreadState::Abandoned`] (the drop keeps the ref at the same base, so a
/// base-only key would otherwise collide — cid 3335052848) — mints a FRESH
/// instant → distinct key → it actually starts.
pub(crate) fn resolve_start_epoch(repo: &Repository, name: &str) -> Result<DateTime<Utc>> {
    let prior_active = ThreadManager::new(repo.heddle_dir())
        .load(name)?
        .filter(|thread| thread.state == ThreadState::Active);
    Ok(prior_active.map_or_else(Utc::now, |thread| thread.created_at))
}

/// Centralized "invalid thread name" advice (text + JSON error envelope) built
/// from a [`ThreadIdError`]. The error's `Display` already names the offending
/// input and suggests a valid rename; this wraps it as a usage refusal.
pub(crate) fn thread_name_invalid_advice(err: &ThreadIdError) -> RecoveryAdvice {
    RecoveryAdvice::invalid_usage(
        "thread_name_invalid",
        err.to_string(),
        "Choose a thread name using only letters, digits, and _ - . / @ : + = \
         (no spaces or shell metacharacters).",
        "heddle start <name>",
    )
}

pub(crate) fn start_thread(repo: &Repository, args: ThreadStartArgs) -> Result<ThreadOpOutput> {
    // The single user/external creation boundary for every thread start
    // (`heddle start`, `heddle thread start`, `try`, `attempt`, workflow). Reject
    // a name that isn't a safe single shell token here so a thread id with a
    // space or shell metacharacter can never be persisted — and so every
    // downstream breadcrumb can interpolate it bare. (heddle#464 close-the-class.)
    ThreadId::new(args.name.as_str()).map_err(|err| anyhow!(thread_name_invalid_advice(&err)))?;

    let existing = find_active_thread_entry(repo, &args.name)?;
    if let Some(entry) = existing {
        if let Some(ref requested_path) = args.path {
            let requested = normalize_path_for_containment(&absolute_path(requested_path)?)?;
            let existing_path = entry
                .path
                .as_ref()
                .ok_or_else(|| anyhow!(active_reservation_advice(&args.name, None)))?;
            let existing_path_normalized = normalize_path_for_containment(existing_path)
                .unwrap_or_else(|_| existing_path.clone());
            if existing_path_normalized != requested {
                return Err(anyhow!(active_reservation_advice(
                    &args.name,
                    Some(existing_path.display().to_string())
                )));
            }
        }

        let path = entry.path.map(|path| path.display().to_string());
        return Err(anyhow!(active_reservation_advice(&args.name, path)));
    }

    let existing_thread_state = repo.refs().get_thread(&ThreadName::new(&args.name))?;
    let base_state = match (&args.from, existing_thread_state) {
        (Some(spec), Some(existing)) => {
            let requested = repo.resolve_state(spec)?.ok_or_else(|| {
                anyhow!(RecoveryAdvice::thread_referenced_state_missing(spec, "State"))
            })?;
            if requested != existing {
                return Err(anyhow!(thread_anchor_mismatch_advice(
                    &args.name, &existing, &requested
                )));
            }
            existing
        }
        (None, Some(existing)) => existing,
        (Some(spec), None) => repo.resolve_state(spec)?.ok_or_else(|| {
            anyhow!(RecoveryAdvice::thread_referenced_state_missing(spec, "State"))
        })?,
        (None, None) => ensure_current_state(
            repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some(format!(
                "Bootstrap git-overlay before starting {}",
                args.name
            )),
        )?,
    };

    let actor_identity = resolve_start_actor_identity(repo, &args)?;

    let thread_mode = resolve_thread_mode(repo, &args);
    // Honesty pass for explicit `--workspace materialized` on a
    // filesystem that doesn't support reflinks. `resolve_thread_mode`
    // already downgrades Auto silently to `solid` in this case (the
    // mode label should match disk truth), but an explicit user
    // choice we respect — and then we owe the user a stderr note so
    // they know why their disk usage will be higher than the design-
    // doc promises. Fires once per `thread start` invocation, goes
    // to stderr so JSON consumers on stdout are unaffected.
    if args.workspace == WorkspaceModeArg::Materialized
        && !objects::fs_clone::filesystem_supports_reflink(repo.root())
    {
        eprintln!(
            "{}: this filesystem doesn't support reflinks/clonefile, so \
             `--workspace materialized` will fall back to per-file copies — \
             disk usage will match `--workspace solid`. \
             Use `--workspace solid` to make this explicit, or `--workspace auto` \
             to let heddle pick the right mode for this host.",
            style::warn("note"),
        );
    }
    let path = match thread_mode {
        // Bytes-on-disk modes honour an explicit `--path`. Clonefile
        // (`Materialized`) doesn't care where the destination lives;
        // full-copy (`Solid`) doesn't either. Default to a managed
        // path when `--path` is absent — heddle-internal layout for
        // Materialized so we can sweep it cleanly; the top-level
        // `default_thread_path` for Solid because users typically
        // want a navigable sibling directory.
        ThreadMode::Materialized => args
            .path
            .clone()
            .unwrap_or_else(|| default_lightweight_thread_path(repo, &args.name)),
        ThreadMode::Solid => args
            .path
            .clone()
            .unwrap_or_else(|| default_thread_path(repo, &args.name)),
        // Virtualised mounts must live at a Heddle-managed path so a
        // user-named directory never gets shadowed by a kernel mount.
        ThreadMode::Virtualized => default_virtualized_thread_path(repo, &args.name),
    };
    if args.path.is_some() {
        ensure_explicit_start_path_outside_tracked_tree(repo, &args.name, &path)?;
    }
    // The retry-stable idempotency key, resolved BEFORE the fresh-start preflight
    // so a committed retry is recognized before any precondition can reject it
    // (heddle#356 cid 3335586969). The per-start epoch comes from the durable
    // committed thread record (`resolve_start_epoch` reads its `created_at`), so a
    // start that committed its write-path but crashed before the post-commit
    // bookkeeping re-derives the SAME key here — not a fresh one.
    let scope = repo.op_scope();
    let start_epoch = resolve_start_epoch(repo, &args.name)?;
    let transaction_id = start_transaction_id(&scope, &args.name, &base_state, start_epoch);

    // Commit-detection GATES the fresh-start preflight — ONE decision point, not
    // two disconnected guards. If this start's transaction already committed
    // (checkout + ref + record + `TransactionCommit` marker landed, then a crash
    // interrupted the post-commit bookkeeping), recognize it as a committed retry
    // and complete the interrupted bookkeeping from the durable record. Do NOT run
    // `plan_worktree_target` (which would reject the now-non-empty checkout with
    // `worktree_target_not_empty`) or re-run `execute` (whose `apply` would fail on
    // the existing `.heddle`, never reaching the commit-point dedup). A
    // genuinely-new start — including a post-silent-drop restart at the same base,
    // whose Abandoned record yields a FRESH epoch — derives a distinct key that is
    // absent here and runs the preflight + execute below (cid 3335586969 /
    // 3335052848).
    if !repo
        .oplog()
        .committed_batch_records(&transaction_id)?
        .is_empty()
    {
        return finalize_committed_start(repo, &args, base_state, &actor_identity);
    }

    // Resolve + validate the target but DO NOT create it here: the directory
    // creation is the transaction's first step, so a failure in the remaining
    // pre-transaction work below can't orphan a dir we made before `execute`
    // had a rewind ledger (heddle#356 cid 3333881552).
    let prepared_target = plan_worktree_target(repo, &path)?;
    let target_dir_created = prepared_target.target_dir_created;
    let abs_path = normalize_path_for_containment(&prepared_target.path)?;

    // Item 2.1 of the heddle 6→8 plan: when starting a heavy
    // (materialized/lightweight) thread in a Rust workspace, redirect
    // the new checkout's `target/` to a workspace-shared dir so
    // parallel threads don't multiply cargo target trees on disk.
    //
    // We compute this *before* materialization so the heads-up
    // advisory below can reflect what would have happened, and we
    // apply the redirect *after* materialization (only the `cargo
    // config.toml` writer touches the checkout; the materializer
    // populates the rest).
    //
    // `--shared-target` in a non-Rust repo is a harmless no-op rather
    // than an error: automation that passes the flag unconditionally
    // across mixed-language repos shouldn't have to special-case
    // every non-cargo project. We log a debug-level note so a curious
    // operator can still see it landed silently.
    let shared_target_dir_path: Option<PathBuf> = if args.shared_target
        && matches!(thread_mode, ThreadMode::Solid | ThreadMode::Materialized)
    {
        if shared_target::workspace_root_is_rust(repo) {
            Some(shared_target::shared_target_dir(repo)?)
        } else {
            tracing::debug!(
                repo = %repo.root().display(),
                "--shared-target requested in a non-Rust repo (no top-level Cargo.toml); skipping"
            );
            None
        }
    } else {
        None
    };

    // Heads-up advisory: when starting a second-or-later materialized
    // thread in a Rust workspace without `--shared-target`, nudge the
    // user toward the flag. Doesn't fail the start; just stderr.
    if !args.shared_target
        && matches!(thread_mode, ThreadMode::Solid | ThreadMode::Materialized)
        && shared_target::should_advise_shared_target(repo)
    {
        shared_target::print_advisory(&args.name);
    }

    // Reads the thread record + the post-commit agent entry both need;
    // computed before the transaction (the record carries them in).
    let current_target_thread = match repo.head_ref()? {
        Head::Attached { thread } => Some(thread.to_string()),
        Head::Detached { .. } => None,
    };
    let base_short = base_state.short();
    let base_state_summary = repo
        .store()
        .get_state(&base_state)?
        .map(|state| {
            (
                state.tree.short(),
                summarize_verification(state.verification.as_ref()),
                summarize_confidence(state.confidence),
            )
        })
        .ok_or_else(|| {
            anyhow!(RecoveryAdvice::thread_referenced_state_missing(
                &base_state.short(),
                "Base state",
            ))
        })?;
    let (base_root, verification_summary, confidence_summary) = base_state_summary;
    // `start_epoch` was resolved above (before the commit-detection gate) and is
    // the record's creation instant: the idempotency key folds it, and a
    // crash-retry reuses it from this still-Active record (heddle#356 cid
    // 3335052848 / 3335586969).
    let thread_state = Thread {
        id: args.name.clone(),
        thread: args.name.clone(),
        target_thread: current_target_thread.clone(),
        parent_thread: args.parent_thread.clone(),
        mode: thread_mode.clone(),
        state: ThreadState::Active,
        base_state: base_short.clone(),
        base_root: base_root.clone(),
        current_state: Some(base_short.clone()),
        merged_state: None,
        task: args.task.clone(),
        execution_path: abs_path.clone(),
        materialized_path: match thread_mode {
            ThreadMode::Solid | ThreadMode::Materialized => Some(abs_path.clone()),
            // Virtualized records the mount point as the materialized
            // path so `heddle thread show` reports it; it's not a
            // checkout, but it is the path the user `cd`s into.
            ThreadMode::Virtualized => Some(abs_path.clone()),
        },
        changed_paths: vec![],
        impact_categories: vec![],
        heavy_impact_paths: vec![],
        promotion_suggested: false,
        freshness: ThreadFreshness::Current,
        verification_summary,
        confidence_summary,
        integration_policy_result: ThreadIntegrationPolicy::default(),
        // The start epoch IS the record's creation instant: the idempotency key
        // folds it, and a crash-retry reuses it from this still-Active record.
        created_at: start_epoch,
        updated_at: Utc::now(),
        ephemeral: None,
        auto: false,
        // The candidate redirect; the transaction clears it if the cargo
        // config writer no-ops on a pre-staged `.cargo/config.toml` (so
        // `thread show` never advertises a redirect that isn't in effect).
        shared_target_dir: shared_target_dir_path.clone(),
    };

    // The ENTIRE start write-path — thread ref, checkout materialize, manifest,
    // cargo-config redirect, `--hydrate` symlinks, and the ThreadManager record
    // — runs as ONE atomic transaction on the heddle#330 primitive (impl-c). A
    // failure anywhere mid-materialize rewinds every applied effect, with
    // precise directory + symlink rewind, back to the exact pre-start state;
    // the oplog `ThreadCreateV2` is the staged commit record appended once at
    // the single commit point. See `start_atomic::StartThread`.
    let mount_ownership =
        mount_lifecycle::MountOwnership::from_flags(args.daemon, args.no_daemon);
    // `scope` + `transaction_id` were resolved above the commit-detection gate.
    let hydrate_requested =
        args.hydrate && matches!(thread_mode, ThreadMode::Solid | ThreadMode::Materialized);
    let linked = repo::atomic::execute(
        repo,
        start_atomic::StartThread {
            transaction_id,
            name: args.name.clone(),
            base_state,
            existing_thread_state,
            thread_mode: thread_mode.clone(),
            abs_path: abs_path.clone(),
            target_dir_created,
            shared_target_dir: shared_target_dir_path,
            hydrate: hydrate_requested,
            mount_ownership,
            record: thread_state,
        },
    )
    .map_err(|e| anyhow!(e))?;

    finalize_thread_start(
        repo,
        &args,
        &thread_mode,
        &abs_path,
        base_state,
        &base_short,
        &base_root,
        &actor_identity,
        hydrate_requested,
        linked,
    )
}

/// Complete a `thread start` that was found ALREADY committed at the
/// commit-detection gate (heddle#356 cid 3335586969): the write-path landed
/// exactly-once, then a crash interrupted the post-commit bookkeeping. The
/// durable thread record is the source of truth for where the checkout lives and
/// what it anchors on, so reconstruct the finalize inputs from it rather than
/// re-running the fresh-start preflight (which would reject the now-non-empty
/// checkout) or `execute` (whose `apply` would fail on the existing `.heddle`).
/// `base_state` is the full `ChangeId` already resolved by `start_thread` (the
/// record only persists the short form). No hydrate is re-run, so no hydrate
/// note is emitted (the original start already linked the deps).
fn finalize_committed_start(
    repo: &Repository,
    args: &ThreadStartArgs,
    base_state: ChangeId,
    actor_identity: &StartActorIdentity,
) -> Result<ThreadOpOutput> {
    let committed = ThreadManager::new(repo.heddle_dir())
        .load(&args.name)?
        .ok_or_else(|| {
            anyhow!(
                "thread '{}' has a committed start transaction but no durable record to \
                 complete the interrupted start from",
                args.name
            )
        })?;
    let abs_path = committed.execution_path.clone();
    let base_short = base_state.short();
    finalize_thread_start(
        repo,
        args,
        &committed.mode,
        &abs_path,
        base_state,
        &base_short,
        &committed.base_root,
        actor_identity,
        // The original start owns the hydrate note; the retry only completes the
        // post-commit bookkeeping.
        false,
        Vec::new(),
    )
}

/// The post-commit bookkeeping shared by a fresh `thread start` and a committed
/// retry ([`finalize_committed_start`]): emit the hydrate note, create the
/// `AgentRegistry` reservation entry (the "active reservation" a crash before
/// this step leaves missing), and build the command output. Idempotent w.r.t. the
/// reservation: it is only reached when no live owner exists (a live owner is
/// caught earlier by `find_active_thread_entry`), so the retry completes the
/// interrupted reservation exactly-once.
#[allow(clippy::too_many_arguments)]
fn finalize_thread_start(
    repo: &Repository,
    args: &ThreadStartArgs,
    thread_mode: &ThreadMode,
    abs_path: &Path,
    base_state: ChangeId,
    base_short: &str,
    base_root: &str,
    actor_identity: &StartActorIdentity,
    hydrate_requested: bool,
    linked: Vec<String>,
) -> Result<ThreadOpOutput> {
    // Post-commit hydrate note (stderr; JSON consumers on stdout unaffected).
    if hydrate_requested {
        if linked.is_empty() {
            eprintln!(
                "{}: --hydrate found no ignored dependency directories at the origin \
                 checkout root to link.",
                style::warn("note"),
            );
        } else {
            eprintln!(
                "{}: hydrated {} ignored dependency dir(s) into '{}' via symlink: {} \
                 (shared with the origin checkout; they stay ignored and are not captured).",
                style::warn("note"),
                linked.len(),
                args.name,
                linked.join(", "),
            );
        }
    }

    let registry = AgentRegistry::new(repo.heddle_dir());
    let path_for_entry = abs_path.to_path_buf();
    let thread_name = args.name.clone();
    let entry = registry.create_generated_entry_for_thread(&thread_name, |session_id| {
        Ok(AgentEntry {
            session_id: session_id.to_string(),
            client_instance_id: None,
            native_actor_key: actor_identity.native_actor_key.clone(),
            native_parent_actor_key: actor_identity.native_parent_actor_key.clone(),
            native_instance_key: actor_identity.native_instance_key.clone(),
            heddle_session_id: None,
            thread_id: Some(thread_name.clone()),
            thread: thread_name.clone(),
            pid: Some(process::id()),
            boot_id: current_boot_id(),
            liveness_path: Some(
                repo.heddle_dir()
                    .join("agents")
                    .join(format!("{session_id}.live")),
            ),
            heartbeat_at: Some(Utc::now()),
            anchor_state: Some(base_state.to_string_full()),
            anchor_root: Some(base_root.to_string()),
            reservation_token: Some(objects::store::generate_agent_id()),
            path: match thread_mode {
                ThreadMode::Solid | ThreadMode::Materialized | ThreadMode::Virtualized => {
                    Some(path_for_entry.clone())
                }
            },
            base_state: base_short.to_string(),
            started_at: Utc::now(),
            provider: actor_identity.provider.clone(),
            model: actor_identity.model.clone(),
            harness: actor_identity.harness.clone(),
            thinking_level: actor_identity.thinking_level.clone(),
            usage_summary: AgentUsageSummary::default(),
            last_progress_at: None,
            report_flush_state: None,
            attach_reason: Some(format!(
                "actor {session_id} was created when thread {} started",
                thread_name
            )),
            attach_precedence: vec!["thread-start".to_string()],
            winning_attach_rule: Some("thread-start".to_string()),
            probe_source: actor_identity.probe_source.clone(),
            probe_confidence: actor_identity.probe_confidence,
            status: AgentStatus::Active,
            completed_at: None,
            context_queries: vec![],
        })
    })?;

    let summary = find_thread_summary(repo, &args.name)?;
    let message = match thread_mode {
        ThreadMode::Materialized | ThreadMode::Solid => {
            format!(
                "Started isolated thread '{}' at '{}' (Heddle-managed checkout, no .git directory)",
                args.name,
                abs_path.display()
            )
        }
        ThreadMode::Virtualized => {
            // Print the mount path so the user knows where to `cd`.
            // The trailing newline before the next status line keeps
            // the path easy to copy-paste from terminal output.
            format!(
                "Started virtualized thread '{}' mounted at '{}'",
                args.name,
                abs_path.display()
            )
        }
    };

    Ok(thread_op_output(
        "thread_start",
        "start",
        args.name.clone(),
        message,
        summary.as_ref().and_then(|thread| thread.path.clone()),
        Some(abs_path.display().to_string()),
        Some(build_repository_verification_state(repo)),
        summary.map(|mut thread| {
            thread.session_id = Some(entry.session_id.clone());
            thread
        }),
    ))
}

#[derive(Debug, Clone)]
struct StartActorIdentity {
    provider: Option<String>,
    model: Option<String>,
    harness: Option<String>,
    thinking_level: Option<String>,
    native_actor_key: Option<String>,
    native_parent_actor_key: Option<String>,
    native_instance_key: Option<String>,
    probe_source: Option<String>,
    probe_confidence: Option<f32>,
}

fn resolve_start_actor_identity(
    repo: &Repository,
    args: &ThreadStartArgs,
) -> Result<StartActorIdentity> {
    let explicit_provider = non_empty_identity_value(args.agent_provider.clone());
    let explicit_model = non_empty_identity_value(args.agent_model.clone());
    let probe = crate::harness::probe_current_process_harness(
        repo,
        explicit_provider.clone(),
        explicit_model.clone(),
        None,
    )?;
    let explicit_identity = explicit_provider.is_some() || explicit_model.is_some();
    let provider = explicit_provider.or_else(|| non_empty_identity_value(probe.provider.clone()));
    let model = explicit_model.or_else(|| non_empty_identity_value(probe.model.clone()));
    let harness = non_empty_identity_value(probe.harness.clone());
    let thinking_level = non_empty_identity_value(probe.thinking_level.clone());
    let native_actor_key = non_empty_identity_value(probe.native_actor_key.clone());
    let native_parent_actor_key = non_empty_identity_value(probe.native_parent_actor_key.clone());
    let native_instance_key = non_empty_identity_value(probe.native_instance_key.clone());
    let detected_identity = provider.is_some()
        || model.is_some()
        || harness.is_some()
        || thinking_level.is_some()
        || native_actor_key.is_some()
        || native_parent_actor_key.is_some()
        || native_instance_key.is_some();

    Ok(StartActorIdentity {
        provider,
        model,
        harness,
        thinking_level,
        native_actor_key,
        native_parent_actor_key,
        native_instance_key,
        probe_source: if explicit_identity {
            Some("explicit_payload".to_string())
        } else if detected_identity {
            probe.probe_source
        } else {
            Some("explicit_payload".to_string())
        },
        probe_confidence: if explicit_identity {
            Some(1.0)
        } else if detected_identity {
            probe.confidence
        } else {
            Some(1.0)
        },
    })
}

fn non_empty_identity_value(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    })
}

fn active_reservation_advice(thread: &str, existing_path: Option<String>) -> RecoveryAdvice {
    let location = existing_path
        .as_ref()
        .map(|path| format!(" at '{path}'"))
        .unwrap_or_default();
    let primary_command = format!("heddle thread show {thread}");
    RecoveryAdvice::safety_refusal(
        "active_thread_reservation",
        format!("Thread '{thread}' already has an active reservation{location}"),
        format!(
            "Inspect it with `{primary_command}`, or release that session before starting another writer."
        ),
        format!("thread '{thread}' already has an active writer reservation{location}"),
        "starting another writer could create competing worktree materializations for the same thread",
        "no worktree, refs, or reservation records were changed",
        primary_command.clone(),
        vec![primary_command],
    )
}

fn thread_anchor_mismatch_advice(
    thread: &str,
    existing: &ChangeId,
    requested: &ChangeId,
) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "thread_anchor_mismatch",
        format!(
            "Thread '{thread}' is anchored at {}, but --from resolved to {}",
            existing.short(),
            requested.short()
        ),
        format!(
            "Start a new thread name, or inspect this thread with `heddle thread show {thread}` before refreshing or rebasing it."
        ),
        format!(
            "thread '{thread}' already points at {}, while --from resolved to {}",
            existing.short(),
            requested.short()
        ),
        "attaching another workspace from a different base could fork the same thread name into competing histories",
        "no worktree, refs, or reservation records were changed",
        format!("heddle thread show {thread}"),
        vec![format!("heddle thread show {thread}")],
    )
}

fn resolve_thread_mode(repo: &Repository, args: &ThreadStartArgs) -> ThreadMode {
    // Explicit `--workspace` wins. `--path` only changes *where* the
    // worktree lives; the mode dispatch decides *how* the bytes get
    // there (clonefile vs mount vs full copy). We respect explicit
    // modes even on filesystems that won't deliver their full
    // performance promise (e.g. `--workspace materialized` on ext4
    // silently falls through to per-blob `fs::copy` inside the
    // materializer); the auto path probes the FS instead so the
    // mode label in `heddle status` stays accurate.
    match args.workspace {
        WorkspaceModeArg::Materialized => ThreadMode::Materialized,
        WorkspaceModeArg::Virtualized => ThreadMode::Virtualized,
        WorkspaceModeArg::Solid => ThreadMode::Solid,
        WorkspaceModeArg::Auto => {
            // Explicit `--path` with no explicit mode reads as "I want
            // a checkout I can navigate to" — point Auto away from
            // `virtualized` (which is always managed) toward the
            // bytes-on-disk modes.
            let candidate = if args.path.is_some() {
                ThreadMode::Materialized
            } else {
                match resolve_auto_workspace_default(repo, args) {
                    UserThreadWorkspaceMode::Materialized => ThreadMode::Materialized,
                    UserThreadWorkspaceMode::Virtualized => ThreadMode::Virtualized,
                    UserThreadWorkspaceMode::Solid => ThreadMode::Solid,
                    UserThreadWorkspaceMode::Auto => ThreadMode::Materialized,
                }
            };
            // Auto-only reflink-capability probe. `materialized`
            // implies "clonefile/reflink the captured tree into a
            // thread dir"; on ext4 / HFS+ / NTFS the clonefile call
            // returns `EOPNOTSUPP` and `materialize_blob` falls back
            // to per-blob `fs::copy`. That works but uses N× the disk
            // a CoW share would, and a user-facing `Workspace:
            // materialized` line would be misleading. Downgrade to
            // `solid` so the mode label matches what's actually on
            // disk. One-shot syscall pair (write + clonefile on a
            // tiny probe file under repo root), measured at <1 ms.
            if candidate == ThreadMode::Materialized
                && !objects::fs_clone::filesystem_supports_reflink(repo.root())
            {
                tracing::debug!(
                    root = %repo.root().display(),
                    "Auto workspace: filesystem does not support reflinks; \
                     falling back to `solid` so the mode label reflects disk truth"
                );
                return ThreadMode::Solid;
            }
            candidate
        }
    }
}

fn resolve_auto_workspace_default(
    _repo: &Repository,
    args: &ThreadStartArgs,
) -> UserThreadWorkspaceMode {
    let user_config = UserConfig::load_default().unwrap_or_default();
    if args.parent_thread.is_some() || args.automated {
        user_config
            .worktree
            .thread_workspace
            .delegated_default
            .unwrap_or(UserThreadWorkspaceMode::Materialized)
    } else {
        user_config.worktree.thread_workspace.top_level_default
    }
}

pub(crate) fn cmd_thread_create(
    cli: &Cli,
    repo: &Repository,
    name: String,
    ephemeral: bool,
    ttl_secs: Option<u32>,
) -> Result<()> {
    // Same user/external creation boundary guard as `start_thread`: reject a
    // name that isn't a safe single shell token before any ref/record is
    // persisted, so `heddle thread create` can't slip an unsafe id past the
    // early-reject layer. (heddle#464 close-the-class.)
    ThreadId::new(name.as_str()).map_err(|err| anyhow!(thread_name_invalid_advice(&err)))?;

    // `ephemeral` / `ttl_secs` are part of main's evolved
    // ephemeral-threads API; not yet plumbed into the Thread record
    // here. TODO: thread these through to a ThreadLifecycle field
    // when the ephemeral-threads work lands.
    let _ = (ephemeral, ttl_secs);
    // Codex's body: auto-bootstrap a current state when there isn't
    // one — needed in fresh git-overlay repos where `heddle init`
    // hasn't produced a snapshot yet.
    let current = ensure_current_state(
        repo,
        &UserConfig::load_default().unwrap_or_default(),
        Some(format!(
            "Bootstrap git-overlay before creating thread {}",
            name
        )),
    )?;

    repo.refs()
        .set_thread_cas(&ThreadName::new(&name), RefExpectation::Missing, &current)?;

    // Persist a Thread record so subsequent commands that go through
    // `ThreadManager::load` (delegate, land, integration policy,
    // `thread show`'s record path) can find it. Without this the ref
    // exists but the record file is missing and any `manager.load(name)`
    // returns `None`, surfacing as `Thread '<name>' not found` even
    // though `thread switch` (which only consults refs) works.
    //
    // `create` differs from `start` in that no worktree is materialized:
    // we record a `Lightweight` thread with an empty `execution_path`
    // (and `materialized_path: None`) — the same shape `Thread::from_record`
    // hydrates when no workspace overlay exists. Consumers that key off
    // `materialized_path`/`execution_path` to drive an actual checkout
    // already treat this as "no dedicated worktree".
    let base_short = current.short();
    let (base_root, verification_summary, confidence_summary) = repo
        .store()
        .get_state(&current)?
        .map(|state| {
            (
                state.tree.short(),
                summarize_verification(state.verification.as_ref()),
                summarize_confidence(state.confidence),
            )
        })
        .ok_or_else(|| {
            anyhow!(RecoveryAdvice::thread_referenced_state_missing(
                &base_short,
                "Base state",
            ))
        })?;
    let target_thread = match repo.head_ref()? {
        Head::Attached { thread } => Some(thread.to_string()),
        Head::Detached { .. } => None,
    };
    let thread_manager = ThreadManager::new(repo.heddle_dir());
    let now = Utc::now();
    let thread_state = Thread {
        id: name.clone(),
        thread: name.clone(),
        target_thread,
        parent_thread: None,
        mode: ThreadMode::Materialized,
        state: ThreadState::Active,
        base_state: base_short.clone(),
        base_root,
        current_state: Some(base_short.clone()),
        merged_state: None,
        task: None,
        execution_path: PathBuf::new(),
        materialized_path: None,
        changed_paths: vec![],
        impact_categories: vec![],
        heavy_impact_paths: vec![],
        promotion_suggested: false,
        freshness: ThreadFreshness::Current,
        verification_summary,
        confidence_summary,
        integration_policy_result: ThreadIntegrationPolicy::default(),
        created_at: now,
        updated_at: now,
        ephemeral: if ephemeral {
            Some(repo::EphemeralMarker::new(ttl_secs.unwrap_or(24 * 3600)))
        } else {
            None
        },
        // `heddle thread create` is the explicit user verb — never
        // an auto-thread, even when run inside a harness session.
        auto: false,
        // `heddle thread create` doesn't materialize a checkout, so
        // there's nowhere to redirect cargo's `target/` to. Threads
        // promoted to materialized later can opt in then.
        shared_target_dir: None,
    };
    thread_manager.save(&thread_state)?;

    // Snapshot the just-saved record into the OpRecord so `heddle redo`
    // can recreate it after `heddle undo` destroys it. Without this,
    // redo restores only the ref and record-backed commands (`thread
    // cd`, delegate, integration policy) silently degrade. heddle#23 r2
    // Codex P1 (mirrors the heddle#99 r2 FastForwardV2 pattern — record
    // what redo needs).
    //
    // Snapshot failure is fatal here: we just wrote the record, so a
    // round-trip-encode that can't read its own write is a serde
    // contract bug, not a runtime condition.
    let manager_snapshot = thread_manager.snapshot_thread_record(&name)?;
    repo.oplog()
        .record_thread_create(&ThreadName::new(&name), &current, manager_snapshot, Some(&repo.op_scope()))?;

    let output = thread_op_output(
        "thread_create",
        "thread create",
        name.clone(),
        format!("Created thread '{}' at {}", name, current.short()),
        None,
        None,
        Some(build_repository_verification_state(repo)),
        find_thread_summary(repo, &name)?,
    );

    render_thread_op(cli, output)
}

/// If `repo` was opened against a dedicated thread worktree, open
/// and return a fresh `Repository` handle rooted at the **main**
/// repo. Returns `Ok(None)` when `repo` IS the main repo.
///
/// Detection signal: in `Repository::open`, when a worktree pointer
/// (`.heddle/objectstore`) is followed, the resulting Repository
/// has `root = <worktree_root>` but `heddle_dir = <shared_main_heddle>`.
/// For the main repo, `heddle_dir == root.join(".heddle")`. So a
/// mismatch between `repo.root().join(".heddle")` and
/// `repo.heddle_dir()` is the worktree fingerprint.
///
/// Used by `cmd_thread_switch` to route HEAD writes at the main
/// repo when the user runs the command from inside a worktree —
/// otherwise `repo.refs().write_head()` lands on the worktree's
/// local HEAD file (see `RefManager::with_local_head` in
/// `crates/repo/src/repository.rs::open`) and clobbers the
/// source worktree's identity.
fn open_main_repo_from_worktree_if_needed(repo: &Repository) -> Result<Option<Repository>> {
    let expected_for_main = repo.root().join(".heddle");
    if expected_for_main == repo.heddle_dir() {
        return Ok(None);
    }
    let main_root = repo.heddle_dir().parent().ok_or_else(|| {
        anyhow!(
            "heddle dir {} has no parent (the main repo root); cannot route HEAD write",
            repo.heddle_dir().display()
        )
    })?;
    Ok(Some(Repository::open(main_root)?))
}

/// Look up the on-disk path for `name` and print it on stdout. Read-only;
/// no auto-capture, no state change. Powers the shell hook's
/// `heddle thread cd` (the function `cd`s into the printed path).
/// Print the name of the current thread — the thread the working
/// checkout is attached to. Single-line plain output keeps the verb
/// composable in shell pipelines (e.g. paired with `thread cd`). The
/// JSON form wraps the name in a `{"thread": "..."}` object for
/// scripted callers.
pub(crate) fn cmd_thread_current(cli: &Cli, repo: &Repository) -> Result<()> {
    let name = if let Some(lane) = repo.current_lane()? {
        lane
    } else if let Some(thread) = super::thread_cmd::current_thread(repo)? {
        thread.thread
    } else {
        return Err(anyhow!(RecoveryAdvice::no_current_thread(
            "thread current",
            None,
            "heddle thread list",
        )));
    };

    // `thread current` is a single-token printer designed for shell
    // composition (e.g. `heddle thread cd "$(heddle thread current)"`),
    // so the default `Auto` output mode — which flips to JSON whenever
    // stdout is piped — would be actively counterproductive here. Match
    // `thread cd` and emit plain text by default; only honor an
    // *explicit* request for JSON.
    let explicit_json = matches!(
        cli.output,
        Some(crate::cli::OutputMode::Json | crate::cli::OutputMode::JsonCompact)
    );
    if explicit_json {
        #[derive(Serialize)]
        struct CurrentOutput<'a> {
            thread: &'a str,
        }
        println!(
            "{}",
            serde_json::to_string(&CurrentOutput { thread: &name })?
        );
    } else {
        println!("{name}");
    }
    Ok(())
}

pub(crate) fn cmd_thread_cd(repo: &Repository, name: String) -> Result<()> {
    let manager = ThreadManager::new(repo.heddle_dir());
    let thread = manager
        .find_by_thread(&name)?
        .ok_or_else(|| anyhow!(thread_not_found_advice(&name, "locate thread worktree")))?;
    let path = thread.execution_path;
    if path.as_os_str().is_empty() {
        return Err(anyhow!(RecoveryAdvice::thread_worktree_unavailable(
            &name,
            "thread cd",
            format!(
                "thread `{name}` has no recorded on-disk worktree; it may be virtualized or metadata-only"
            ),
            format!("heddle thread show {name}"),
        )));
    }
    if !path.exists() {
        return Err(anyhow!(RecoveryAdvice::thread_worktree_unavailable(
            &name,
            "thread cd",
            format!(
                "thread `{name}` is registered at `{}` but that path no longer exists",
                path.display()
            ),
            format!("heddle start {name} --path <dir>"),
        )));
    }
    println!("{}", path.display());
    Ok(())
}

pub(crate) fn cmd_thread_switch(
    cli: &Cli,
    repo: &Repository,
    name: String,
    print_cd_path: bool,
    force: bool,
) -> Result<()> {
    // Resolve the *target* before touching the source. A typo'd
    // thread name otherwise produces (1) a new state on the source
    // thread, (2) the source's head advancing, then (3) a
    // `Thread not found` error — the bad side-effects already
    // landed by the time the user sees the failure. Resolve first;
    // bail before mutating anything if the target doesn't exist.
    let state = repo
        .refs()
        .get_thread(&ThreadName::new(&name))?
        .ok_or_else(|| anyhow!(thread_not_found_advice(&name, "switch thread")))?;

    if !force {
        ensure_worktree_clean(repo, "switch threads")?;
    }

    // Auto-capture-on-switch (jj-style): before flipping HEAD,
    // capture any uncommitted edits in the *source* thread so the
    // user never has the "you have uncommitted changes" experience
    // git inflicts. Errors here surface loudly — silently losing
    // the agent's work is the failure mode we are most allergic to.
    //
    // Fires when ALL of the following hold:
    //   * HEAD is currently attached to a thread (so there's a
    //     source thread to capture).
    //   * Target differs from source (no-op self-switch).
    //   * The source thread is `Materialized` or `Solid`. We
    //     deliberately skip `Virtualized`: a dead FUSE/FSKit/ProjFS
    //     mount leaves an empty real directory at the mount point,
    //     `path.exists()` is true, `capture_thread_from_disk` walks
    //     the empty dir, and the slow-path captures an *empty tree*
    //     as the thread's new state — silent destruction of the
    //     work the agent did inside the mount. Virtualised threads
    //     are kept in sync by the daemon's write notifications; the
    //     CLI doesn't second-guess that on switch.
    //   * The source thread has a non-empty recorded execution
    //     path that exists on disk. We capture there regardless of
    //     whether it equals `repo.root()` — when the user runs
    //     `heddle thread switch` from inside the source's own
    //     worktree, `repo.root()` IS the execution path, and that's
    //     exactly when we MOST want to capture (the user has been
    //     editing here). Found during dogfood.
    let auto_capture_outcome =
        if force {
            None
        } else {
            match repo.head_ref()? {
                Head::Attached {
                    thread: source_thread,
                } if source_thread != name => {
                    let manager = ThreadManager::new(repo.heddle_dir());
                    let source_record = manager.find_by_thread(&source_thread)?;
                    let source_mode = source_record.as_ref().map(|t| t.mode.clone());
                    let source_path = source_record
                        .map(|t| t.execution_path)
                        .filter(|p| !p.as_os_str().is_empty());
                    let mode_safe_to_capture = matches!(
                        source_mode,
                        Some(ThreadMode::Materialized) | Some(ThreadMode::Solid)
                    );
                    match (mode_safe_to_capture, source_path) {
                        (true, Some(path)) if path.exists() => {
                            let outcome = repo
                        .capture_thread_from_disk(&source_thread, &path)
                        .with_context(|| {
                            format!("auto-capture of '{source_thread}' before switch to '{name}'")
                        })?;
                            Some((source_thread, outcome))
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
        };

    // "Invisible thread directories" rule: switching to a thread that has
    // its *own* dedicated worktree (the one `heddle start --workspace
    // private|virtualized` recorded under `.heddle/threads/<name>/root/`)
    // is a metadata-only operation. The on-disk worktree at the
    // recorded path is already X's worktree — it was set up by `start`
    // and is kept in sync by the metadata-driven merge/rebase/goto/land
    // dispatcher (see `Repository::active_worktree_path`). The operator's
    // CWD must stay untouched so `thread switch X` from `$ROOT` does NOT
    // overwrite `$ROOT`'s files with X's tree.
    //
    // For threads without a dedicated worktree (created via
    // `thread create`, or threads whose recorded path collapses onto the
    // repo root), fall back to the legacy `goto`-based switch so the
    // traditional create-then-switch-then-snapshot workflow keeps working
    // — those threads share the repo root and the user expects the
    // worktree to flip to the target tree.
    let manager = ThreadManager::new(repo.heddle_dir());
    let dedicated_worktree = manager
        .find_by_thread(&name)?
        .map(|thread| thread.execution_path)
        .filter(|path| !path.as_os_str().is_empty() && path != repo.root());

    if let Some(path) = dedicated_worktree {
        if !path.exists() {
            // Forgiving recovery: the recorded worktree was deleted out
            // of band (manual `rm -rf`, partial cleanup, etc). Rebuild
            // it from `current_state` rather than erroring — `current_state`
            // is the canonical source of truth for the thread's content,
            // and there's no obvious recovery command to point the user
            // at. Anything the worktree held that wasn't snapshotted is
            // already gone, so re-materializing just restores the
            // last-known good state.
            write_isolated_checkout(repo, &path, &state, Some(&name))?;
        }
        // Metadata-only: the dedicated worktree is already correct, so
        // we only need to flip HEAD. Importantly: this does NOT touch
        // CWD. Intentional raw `write_head` (not `fast_forward_attached`):
        // we're attaching to a *new* thread.
        //
        // The HEAD we want to update is the *main repo's*. When the
        // user is `cd`'d into a thread's dedicated worktree, `repo`
        // refers to that worktree (which has its own `.heddle/HEAD`
        // pointing at the source thread). Writing through `repo.refs()`
        // would clobber the source worktree's HEAD with the target
        // thread name — which (a) loses the source-worktree's
        // identity, and (b) makes the next auto-capture-on-switch
        // think source == target and skip itself. Found in dogfood.
        // Fix: route the HEAD write to the main repo when we're
        // inside a worktree.
        let head_target_repo = open_main_repo_from_worktree_if_needed(repo)?;
        let head_repo = head_target_repo.as_ref().unwrap_or(repo);
        head_repo.refs().write_head(&Head::Attached {
            thread: ThreadName::new(&name),
        })?;
    } else if open_main_repo_from_worktree_if_needed(repo)?.is_some() {
        // Switching to a target thread that has *no* dedicated
        // worktree, from *inside* another thread's dedicated worktree.
        // The legacy `goto` path would materialize the target's tree
        // at the current worktree's root — overwriting the source
        // worktree's files with the target's content. The bytes
        // aren't lost (the auto-capture above wrote them to the
        // source thread's history) but the disk state suddenly
        // shows a different thread's content, which is jarring and
        // makes it look like edits vanished.
        //
        // Refuse with a clear next step. The user either runs
        // `heddle start --workspace materialized <target>` to give the
        // target its own worktree, or cd's to the main repo root
        // first.
        return Err(anyhow!(thread_switch_would_overwrite_worktree_advice(
            &name
        )));
    } else {
        // Legacy shared-worktree path: materialize the target tree at
        // CWD and reattach HEAD to the thread. Intentional raw `goto`:
        // `fast_forward_attached` would re-attach to the previously
        // attached thread, which is the wrong behavior here.
        if force {
            repo.goto_discard_local(&state)?;
        } else {
            repo.goto(&state)?;
        }
        repo.refs().write_head(&Head::Attached {
            thread: ThreadName::new(&name),
        })?;
        if repo.capability() == repo::RepositoryCapability::GitOverlay
            && repo.root().join(".git").exists()
        {
            let mut bridge = crate::bridge::GitBridge::new(repo);
            match bridge.write_through_thread_checkout(&name)? {
                crate::bridge::WriteThroughOutcome::Wrote(_) => {}
                crate::bridge::WriteThroughOutcome::Skipped(reason) => {
                    return Err(anyhow!(thread_switch_git_checkout_skipped_advice(
                        &name,
                        reason.to_string()
                    )));
                }
            }
        }
    }

    let summary = find_thread_summary(repo, &name)?;

    // Shell-hook mode: print only the target's on-disk path so the
    // wrapper function can `cd` into it. Auto-capture still ran
    // above; only the rich JSON/text output is suppressed.
    if print_cd_path {
        let path = summary
            .as_ref()
            .and_then(|t| t.execution_path.clone())
            .ok_or_else(|| {
                anyhow!(RecoveryAdvice::thread_checkout_unavailable(
                    &name,
                    "--print-cd-path",
                ))
            })?;
        println!("{path}");
        return Ok(());
    }

    let mut message = format!("Switched to thread '{}'", name);
    if let Some(thread) = &summary
        && thread.coordination_status != CoordinationStatus::Clean
    {
        message.push_str(&format!(" [{}]", thread.coordination_status));
    }
    if let Some((source_thread, ThreadCaptureOutcome::Captured { state_id })) = auto_capture_outcome
    {
        message.push_str(&format!(
            " (auto-captured '{source_thread}' → {})",
            state_id.short()
        ));
    }

    render_thread_op(
        cli,
        thread_op_output(
            "thread_switch",
            "thread switch",
            name,
            message,
            summary.as_ref().and_then(|thread| thread.path.clone()),
            summary
                .as_ref()
                .and_then(|thread| thread.execution_path.clone()),
            Some(build_repository_verification_state(repo)),
            summary,
        ),
    )
}

fn thread_switch_would_overwrite_worktree_advice(thread: &str) -> RecoveryAdvice {
    let primary_command = format!("heddle start --workspace materialized {thread}");
    RecoveryAdvice::safety_refusal(
        "thread_switch_would_overwrite_worktree",
        format!("thread '{thread}' has no dedicated worktree"),
        format!(
            "Run `{primary_command}` to give it a dedicated worktree, or cd to the main repo root and retry `heddle thread switch {thread}`."
        ),
        "the current directory is another thread's dedicated worktree",
        "switching here would overwrite this directory's files with the target thread tree",
        "the source thread was auto-captured when needed; no checkout files were overwritten",
        primary_command.clone(),
        vec![primary_command, format!("heddle thread switch {thread}")],
    )
}

fn thread_switch_git_checkout_skipped_advice(thread: &str, reason: String) -> RecoveryAdvice {
    let primary_command = canonical_bridge_reconcile_ref_preview_command(Some("heddle"), thread);
    RecoveryAdvice::safety_refusal(
        "thread_switch_git_checkout_skipped",
        format!("switched Heddle to '{thread}', but could not update Git checkout: {reason}"),
        format!("Inspect the Git/Heddle checkout mapping with `{primary_command}`."),
        format!(
            "Git checkout write-through was skipped after Heddle switched to '{thread}': {reason}"
        ),
        "Git and Heddle may now point at different checkout states until reconciliation runs",
        format!("Heddle HEAD was switched to '{thread}'; Git checkout was left unchanged"),
        primary_command.clone(),
        vec![primary_command],
    )
}

pub fn cmd_thread_show(cli: &Cli, repo: &Repository, name: Option<String>) -> Result<()> {
    let name = super::thread_cmd::resolve_thread_name_or_current(
        repo,
        name,
        "thread show",
        "heddle thread show <THREAD>",
    )?;

    let summary = find_thread_summary(repo, &name)?
        .ok_or_else(|| anyhow!(thread_not_found_advice(&name, "show thread")))?;

    show_thread_summary(cli, repo, &summary)
}

pub(crate) fn show_thread_summary(
    cli: &Cli,
    repo: &Repository,
    summary: &ThreadSummary,
) -> Result<()> {
    let mut trust = build_repository_verification_state(repo);
    let mut summary = summary.clone();
    if !trust.verified {
        summary.thread_health = trust.status.clone();
        summary.recommended_action = trust.recommended_action.clone();
        summary.recommended_action_template = trust.recommended_action_template.clone();
    } else {
        let action = primary_next_action_with_verification(
            None,
            None,
            None,
            Some(&summary.recommended_action),
            &trust,
        );
        let action = contextual_thread_action(
            repo,
            &summary.name,
            summary.target_thread.as_deref(),
            &action,
        );
        if !action.is_empty() {
            summary.recommended_action = action;
            summary.recommended_action_template =
                recommended_action_template(&summary.recommended_action);
        }
    }
    if !trust.recommended_action.is_empty() {
        let contextual = contextual_thread_action(
            repo,
            &summary.name,
            summary.target_thread.as_deref(),
            &trust.recommended_action,
        );
        if contextual != trust.recommended_action {
            override_trust_recommended_action(&mut trust, contextual);
        }
    }
    if trust.verified
        && !summary.recommended_action.is_empty()
        && trust.recommended_action != summary.recommended_action
        && thread_recovery_action_is_primary(
            Some(&summary.thread_health),
            &summary.recommended_action,
        )
    {
        override_trust_recommended_action(&mut trust, summary.recommended_action.clone());
    }
    let presentation = crate::cli::render::repository_presentation(
        repo,
        summary.target_thread.as_deref(),
        summary.parent_thread.as_deref(),
    );
    if should_output_json(cli, Some(repo.config())) {
        let output = ThreadShowOutput {
            output_kind: "thread_show",
            repository_label: presentation.label,
            repository_context: presentation.context,
            next_action: summary.recommended_action.clone(),
            next_action_template: recommended_action_template(&summary.recommended_action),
            recommended_action_template: recommended_action_template(&summary.recommended_action),
            summary,
            recovery_commands: trust.recovery_commands.clone(),
            trust,
        };
        write_full_command_json(
            &output,
            NextActionValidationContext::new(&["thread", "show"], repo.capability()),
        )?;
    } else {
        println!("Repository: {}", presentation.label);
        render_repository_context_lines(presentation.context.as_ref());
        if repo.hosted_enabled() {
            println!("Hosted: enabled");
        }
        let trust_only_blocks_on_this_ready_thread = trust.workflow_status == "ready"
            && trust.recommended_action == summary.recommended_action;
        let mut next_step_printed = false;
        if !trust.verified
            && !trust_only_blocks_on_this_ready_thread
            && !trust.recommended_action.is_empty()
        {
            println!("Verification: {}", style::warn(&trust.summary));
            print_next_step(&trust.recommended_action);
            next_step_printed = true;
        }
        if let Some(operation) = &summary.operation {
            println!(
                "In progress: {} {} ({})",
                operation.scope, operation.kind, operation.state
            );
        }
        if let Some(remote_tracking) = &summary.remote_tracking {
            if remote_tracking.behind == 0 && remote_tracking.ahead > 0 {
                println!("Remote sync: {}", remote_tracking.message);
            } else {
                println!("Remote drift: {}", remote_tracking.message);
            }
        }
        println!();
        if summary.is_current {
            println!("Thread: {} {}", summary.name, style::dim("(current)"));
        } else {
            println!("Thread: {}", summary.name);
        }
        println!("Status: {}", summary.coordination_status);
        if cli.verbose > 0
            && let Some(base) = &summary.base_state
        {
            println!("Base: {}", base);
        }
        if cli.verbose > 0
            && let Some(base_root) = &summary.base_root
            && !base_root.is_empty()
        {
            println!("Base tree: {}", base_root);
        }
        if cli.verbose > 0
            && let Some(current) = &summary.current_state
        {
            println!("Current: {}", current);
        }
        if cli.verbose > 0
            && let Some(git_branch_tip) = &summary.git_branch_tip
        {
            println!("Git tip: {}", git_branch_tip);
            println!("History: {}", git_history_label(summary.history_imported));
        }
        if let Some(path) = &summary.path {
            println!("Path: {}", path);
        } else if let Some(path) = &summary.execution_path {
            println!("Execution root: {}", path);
        }
        if let Some(mode) = &summary.thread_mode {
            let checkout = if summary.is_isolated {
                thread_workspace_label(mode)
            } else {
                "no dedicated checkout"
            };
            println!("Checkout: {}", checkout);
        } else {
            println!("Checkout: {}", summary.visibility);
        }
        if cli.verbose > 0
            && let Some(shared) = &summary.shared_target_dir
        {
            println!("Shared cargo target: {}", shared);
        }
        if let Some(state) = &summary.thread_state
            && (cli.verbose > 0 || matches!(state, ThreadState::Merged | ThreadState::Abandoned))
        {
            println!("Lifecycle: {}", state);
        }
        if let Some(freshness) = &summary.freshness
            && *freshness != ThreadFreshness::Unknown
            && !matches!(
                summary.thread_state,
                Some(ThreadState::Merged | ThreadState::Abandoned)
            )
        {
            println!("Sync: {}", freshness);
        }
        if let Some(target) = &summary.target_thread {
            println!("Target thread: {}", target);
        }
        if cli.verbose > 0
            && let Some(parent) = &summary.parent_thread
        {
            println!("Parent thread: {}", parent);
        }
        if cli.verbose > 0 && !summary.child_threads.is_empty() {
            println!("Child threads: {}", summary.child_threads.join(", "));
        }
        if cli.verbose > 0 && !summary.sibling_threads.is_empty() {
            println!("Sibling threads: {}", summary.sibling_threads.join(", "));
        }
        if cli.verbose > 0 && summary.stack_depth > 0 {
            println!("Stack depth: {}", summary.stack_depth);
        }
        if summary.stale_from_parent {
            println!("Parent drift: parent moved since this thread last refreshed");
        }
        if cli.verbose > 0 {
            if let Some(actor) = &summary.actor
                && let Some(text) = crate::cli::render::actor_display(
                    actor.provider.as_deref(),
                    actor.model.as_deref(),
                )
            {
                println!("Actor: {text}");
            }
            if let Some(session_id) = &summary.session_id {
                println!("Session: {}", session_id);
            }
            if let Some(session) = &summary.heddle_session_id {
                println!("Heddle session: {}", session);
            }
            if let Some(harness) = &summary.harness {
                println!("Harness: {}", harness);
            }
            if let Some(thinking_level) = &summary.thinking_level {
                println!("Thinking: {}", thinking_level);
            }
            if let Some(last_progress_at) = &summary.last_progress_at {
                println!("Last progress: {}", last_progress_at);
            }
        }
        if cli.verbose > 0
            && let Some(last_activity_at) = &summary.last_activity_at
        {
            println!("Last activity: {}", last_activity_at);
        }
        if cli.verbose > 0
            && let Some(report_flush_state) = &summary.report_flush_state
        {
            println!("Report flush: {}", report_flush_state);
        }
        if cli.verbose > 0
            && let Some(attach_reason) = &summary.attach_reason
        {
            println!("Attach: {}", attach_reason);
        }
        if cli.verbose > 0
            && let Some(usage_summary) = &summary.usage_summary
        {
            let mut parts = Vec::new();
            if let Some(input) = usage_summary.input_tokens {
                parts.push(format!("input {}", input));
            }
            if let Some(output) = usage_summary.output_tokens {
                parts.push(format!("output {}", output));
            }
            if let Some(reasoning) = usage_summary.reasoning_tokens {
                parts.push(format!("reasoning {}", reasoning));
            }
            if let Some(tool_calls) = usage_summary.tool_calls {
                parts.push(format!("tools {}", tool_calls));
            }
            if let Some(cost) = usage_summary.cost_micros_usd {
                parts.push(format!("cost {}uUSD", cost));
            }
            if !parts.is_empty() {
                println!("Usage: {}", parts.join(" · "));
            }
        }
        if let Some(task) = &summary.task {
            println!("Task: {}", task);
        }
        let captures = if cli.verbose > 0 {
            collect_thread_captures(repo, &summary.name, 5).unwrap_or_default()
        } else {
            Vec::new()
        };
        if !captures.is_empty() {
            println!();
            println!("{}", style::section("Recent saved states"));
            for capture in captures {
                println!(
                    "  {} {}",
                    style::accent(&capture.change_id),
                    capture.message
                );
            }
        }
        if summary.promotion_suggested && !summary.heavy_impact_paths.is_empty() {
            println!(
                "Promotion suggested: {}",
                crate::cli::render::preview_list(
                    &summary.heavy_impact_paths,
                    summary.heavy_impact_paths.len(),
                )
            );
        }
        if !summary.impact_categories.is_empty() {
            println!(
                "Impact categories: {}",
                summary
                    .impact_categories
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if !summary.blockers.is_empty() {
            println!("Blocked by: {}", summary.blockers.join(" | "));
        }
        if !summary.recommended_action.is_empty() && !next_step_printed {
            print_next_step(&summary.recommended_action);
        }
    }

    Ok(())
}

/// The destructive intent the user originally expressed, so the
/// recovery hint can suggest a retry that PRESERVES it. A bare
/// `heddle thread drop {current}` retry only removes a thread that owns a
/// managed record; a lightweight ref (no `Thread` record) needs the
/// ref-deleting form, or the retry dead-ends at `thread_not_found`
/// (heddle#258 r2).
#[derive(Clone, Copy)]
pub(crate) enum DropMode {
    /// Plain `heddle thread drop` — keeps the managed-record teardown.
    Drop,
    /// Destructive `--delete-thread` / `branch -d` — removes the ref even
    /// when no managed record exists.
    DeleteThread,
}

impl DropMode {
    /// The retry command to suggest, preserving the destructive mode.
    fn retry_command(self, current: &str) -> String {
        match self {
            DropMode::Drop => format!("heddle thread drop {current}"),
            DropMode::DeleteThread => format!("heddle thread drop {current} --delete-thread"),
        }
    }
}

/// Recovery advice for refusing to drop or delete the *current*
/// checkout thread. The original advice pointed users at
/// `heddle thread list`, which loops a junior who sees only the current
/// thread (heddle#258): the real fix is to switch to a sibling thread
/// first, or create one when none exists, then retry the drop. The
/// retry command preserves the caller's [`DropMode`] so a lightweight
/// ref can actually be removed on retry (heddle#258 r2). Returns
/// `(primary_command, recovery_commands, hint)`; both the switch and
/// create paths are exposed as `<other>` templates so JSON callers can
/// fill in a real thread name.
pub(crate) fn current_thread_drop_recovery(
    repo: &Repository,
    current: &str,
    mode: DropMode,
) -> (String, Vec<String>, String) {
    const SWITCH: &str = "heddle thread switch <other>";
    const CREATE: &str = "heddle thread create <other>";
    let retry = mode.retry_command(current);
    let has_other = repo
        .refs()
        .list_threads()
        .map(|threads| threads.iter().any(|name| name.as_str() != current))
        .unwrap_or(false);
    if has_other {
        (
            SWITCH.to_string(),
            vec![SWITCH.to_string(), CREATE.to_string()],
            format!(
                "Switch to another thread with `{SWITCH}` (or start one with `{CREATE}`), then retry `{retry}`."
            ),
        )
    } else {
        (
            CREATE.to_string(),
            vec![CREATE.to_string(), SWITCH.to_string()],
            format!(
                "No other thread exists yet. Create one with `{CREATE}`, switch to it, then retry `{retry}`."
            ),
        )
    }
}

pub(crate) fn cmd_thread_delete(cli: &Cli, repo: &Repository, name: String) -> Result<()> {
    if let Head::Attached { thread } = repo.head_ref()?
        && thread == name
    {
        let (primary, recovery, hint) =
            current_thread_drop_recovery(repo, &name, DropMode::DeleteThread);
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "branch_delete_current",
            format!("Refusing to delete current thread '{name}'"),
            hint,
            format!("HEAD is attached to '{name}'"),
            "deleting the attached thread would strand the current checkout without its branch ref",
            "no refs were moved or deleted",
            primary,
            recovery,
        )));
    }

    let thread_name = ThreadName::new(&name);
    let state = repo
        .refs()
        .delete_thread(&thread_name)?
        .ok_or_else(|| anyhow!(thread_not_found_advice(&name, "delete thread")))?;

    repo.oplog()
        .record_thread_delete(&thread_name, &state, Some(&repo.op_scope()))?;

    let output = thread_op_output(
        "thread_drop",
        "thread drop",
        name.clone(),
        format!("Deleted thread '{}'", name),
        None,
        None,
        Some(build_repository_verification_state(repo)),
        None,
    );

    render_thread_op(cli, output)
}

pub(crate) fn cmd_thread_rename(
    cli: &Cli,
    repo: &Repository,
    old: String,
    new: String,
) -> Result<()> {
    // Renaming persists a new thread id, so the destination name is a
    // user/external creation boundary too — reject an unsafe name here.
    // (heddle#464 close-the-class.)
    ThreadId::new(new.as_str()).map_err(|err| anyhow!(thread_name_invalid_advice(&err)))?;
    let old_tn = ThreadName::new(&old);
    let new_tn = ThreadName::new(&new);
    let state = repo
        .refs()
        .get_thread(&old_tn)?
        .ok_or_else(|| anyhow!(thread_not_found_advice(&old, "rename thread")))?;

    let mut updates = vec![
        RefUpdate::Thread {
            name: new_tn.clone(),
            expected: RefExpectation::Missing,
            new: Some(state),
        },
        RefUpdate::Thread {
            name: old_tn.clone(),
            expected: RefExpectation::Value(state),
            new: None,
        },
    ];

    if let Head::Attached { thread } = repo.head_ref()?
        && thread == old
    {
        updates.push(RefUpdate::Head {
            expected: RefExpectation::Value(Head::Attached {
                thread: old_tn.clone(),
            }),
            new: Head::Attached {
                thread: new_tn.clone(),
            },
        });
    }

    repo.refs().update_refs(&updates)?;
    repo.oplog()
        .record_thread_rename(&old_tn, &new_tn, &state, Some(&repo.op_scope()))?;

    let output = thread_op_output(
        "thread_rename",
        "thread rename",
        new.clone(),
        format!("Renamed thread '{}' to '{}'", old, new),
        None,
        None,
        Some(build_repository_verification_state(repo)),
        find_thread_summary(repo, &new)?,
    );

    render_thread_op(cli, output)
}

fn render_thread_op(cli: &Cli, output: ThreadOpOutput) -> Result<()> {
    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{}", style::accent(&output.message));
        if let Some(thread) = &output.thread {
            if let Some(path) = &thread.path {
                println!("Path: {}", style::dim(path));
                // The CLI itself can't change the parent shell's cwd. Print a
                // copy-pasteable `cd` hint so the next manual step is
                // obvious; shell wrappers can prefer `heddle start <name>
                // --print-cd-path` to capture the path directly.
                println!("Run this to switch shells:");
                println!(
                    "    cd {}",
                    style::accent(&crate::cli::render::shell_quote(path))
                );
            } else if let Some(path) = non_empty_string(thread.execution_path.as_deref()) {
                println!("Execution root: {}", style::dim(path));
            }
            if !thread.recommended_action.is_empty() {
                print_next_step(&thread.recommended_action);
            }
        }
    }
    Ok(())
}

fn non_empty_string(value: Option<&str>) -> Option<&str> {
    value.and_then(|value| {
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn thread_op_output(
    output_kind: &'static str,
    action: &'static str,
    name: String,
    message: String,
    path: Option<String>,
    execution_path: Option<String>,
    trust: Option<RepositoryVerificationState>,
    thread: Option<ThreadSummary>,
) -> ThreadOpOutput {
    let recommended_action = thread
        .as_ref()
        .and_then(|thread| non_empty_action(&thread.recommended_action))
        .or_else(|| {
            trust
                .as_ref()
                .and_then(|trust| non_empty_action(&trust.recommended_action))
        });
    let recommended_action_template = recommended_action
        .as_deref()
        .and_then(recommended_action_template);
    ThreadOpOutput {
        output_kind,
        status: "completed",
        action,
        name,
        message,
        next_action: recommended_action.clone(),
        next_action_template: recommended_action_template.clone(),
        recommended_action,
        recommended_action_template,
        thread,
        path,
        execution_path,
        trust,
    }
}

fn non_empty_action(action: &str) -> Option<String> {
    (!action.trim().is_empty()).then(|| action.to_string())
}

/// Default checkout directory for a thread, for every workspace mode:
/// `<repo>/.heddle/threads/<encoded>/root`.
///
/// Keyed off the SAME `thread_manifest::thread_dir` derivation the
/// per-thread `manifest.toml` sidecar uses — the prefix-safe single-segment
/// encoding of the thread name, NOT a re-sanitised copy. A local
/// sanitisation would diverge from the manifest and could collide two
/// distinct ids onto one directory; sharing `thread_dir` guarantees the
/// checkout root and the manifest sit in the same per-thread directory and
/// that no id can ever be a directory prefix of another (heddle#572 r2).
///
/// The `root/` leaf is load-bearing, not cosmetic: nesting the worktree
/// bytes one level down at `<encoded>/root` keeps `manifest.toml` a
/// *sibling* of the checkout rather than a stray file inside it.
fn default_thread_checkout_path(repo: &Repository, name: &str) -> PathBuf {
    repo::thread_manifest::thread_dir(repo.heddle_dir(), name).join("root")
}

fn default_thread_path(repo: &Repository, name: &str) -> PathBuf {
    default_thread_checkout_path(repo, name)
}

/// Guard an explicit `--path`. A checkout may live either OUTSIDE the
/// repository (the classic sibling-directory escape hatch) or under the
/// repo's reserved `.heddle/` metadata dir (where the new defaults live,
/// and which is excluded from overlay/status traversal). What it must NOT
/// do is land in the repo's *tracked* working tree — a checkout there
/// would surface as nested unsaved work in the parent repo's status.
fn ensure_explicit_start_path_outside_tracked_tree(
    repo: &Repository,
    name: &str,
    path: &Path,
) -> Result<()> {
    if repo.capability() != repo::RepositoryCapability::GitOverlay {
        return Ok(());
    }
    let requested = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let requested_for_check = normalize_path_for_containment(&requested)?;
    let heddle_dir = normalize_path_for_containment(repo.heddle_dir())?;
    // Under `.heddle/` is allowed — that's where managed checkouts live.
    // (`validate_worktree_target` further restricts this to the
    // `.heddle/threads` subtree so a checkout can't target the store.)
    // Check this BEFORE the repo-root test, since `.heddle` is itself a
    // child of the repo root.
    if requested_for_check == heddle_dir || requested_for_check.starts_with(&heddle_dir) {
        return Ok(());
    }
    let repo_root = normalize_path_for_containment(repo.root())?;
    if requested_for_check == repo_root || requested_for_check.starts_with(&repo_root) {
        let suggested = default_thread_path(repo, name);
        let suggested_command = format!("heddle start {name} --path {}", suggested.display());
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "thread_start_path_inside_repo",
            format!(
                "Refusing to start thread '{name}' inside the current repository at '{}'",
                requested_for_check.display()
            ),
            format!(
                "Choose a checkout under `.heddle/threads` (the default) or a sibling outside the repository, for example `{suggested_command}`."
            ),
            format!(
                "requested checkout path '{}' is inside the tracked working tree of repository '{}'",
                requested_for_check.display(),
                repo_root.display()
            ),
            "starting an isolated checkout inside the source worktree would make Heddle report the nested checkout as unsaved work",
            "no thread refs, checkout directories, mounts, or worktree files were changed",
            suggested_command.clone(),
            vec![suggested_command],
        )));
    }
    Ok(())
}

fn normalize_path_for_containment(path: &Path) -> Result<PathBuf> {
    let mut ancestor = path;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| anyhow!("path '{}' has no usable ancestor", path.display()))?;
    }

    let mut normalized = ancestor.canonicalize()?;
    let remainder = path.strip_prefix(ancestor).with_context(|| {
        format!(
            "path '{}' could not be normalized relative to '{}'",
            path.display(),
            ancestor.display()
        )
    })?;

    for component in remainder.components() {
        match component {
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {}
        }
    }

    Ok(normalized)
}

/// Lightweight (materialized) checkout path:
/// `<repo>/.heddle/threads/<name>/root`.
fn default_lightweight_thread_path(repo: &Repository, name: &str) -> PathBuf {
    default_thread_checkout_path(repo, name)
}

/// Mount-point path for a virtualized thread:
/// `<repo>/.heddle/threads/<name>/root`. Shares the same managed
/// `.heddle/threads/<name>/root` layout as solid/lightweight checkouts
/// (thread names are unique per repo), keeping the per-thread
/// `manifest.toml` sidecar a sibling of the mount point rather than a
/// stray entry inside it.
fn default_virtualized_thread_path(repo: &Repository, name: &str) -> PathBuf {
    default_thread_checkout_path(repo, name)
}

fn absolute_path(path: &std::path::Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Return the most recently started *active* `AgentEntry` for `thread`, if
/// any. Used by `heddle status` to surface the actor for a thread, and
/// (since the Phase-D demo work) by `heddle capture` to inherit the
/// thread's actor as the captured state's `attribution.agent` — without
/// it, every state on an agent thread shows `Principal: Unknown`.
pub(crate) fn find_active_thread_entry(
    repo: &Repository,
    thread: &str,
) -> Result<Option<AgentEntry>> {
    let registry = AgentRegistry::new(repo.heddle_dir());
    Ok(registry
        .list()?
        .into_iter()
        .filter(|entry| entry.thread == thread && entry.status == AgentStatus::Active)
        .max_by_key(|entry| entry.started_at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_execution_paths_are_suppressed() {
        assert_eq!(display_path_string(&PathBuf::new()), None);
        assert_eq!(non_empty_string(Some("")), None);
        assert_eq!(non_empty_string(Some("   ")), None);
    }

    #[test]
    fn git_checkout_skipped_after_thread_switch_uses_reconcile_advice() {
        let advice =
            thread_switch_git_checkout_skipped_advice("feature/git", "dirty Git index".to_string());

        assert_eq!(advice.kind, "thread_switch_git_checkout_skipped");
        assert!(advice.error.contains("switched Heddle to 'feature/git'"));
        assert!(advice.unsafe_condition.contains("dirty Git index"));
        assert_eq!(
            advice.primary_command,
            "heddle bridge git reconcile --prefer heddle --ref feature/git --preview"
        );
        assert!(advice.preserved.contains("Git checkout was left unchanged"));
    }

    /// A slashed thread id must map the default checkout root and the
    /// per-thread manifest to the SAME `.heddle/threads/<encoded>` directory
    /// (the checkout's `root/` a sibling of `manifest.toml`), neither may
    /// escape `.heddle/threads/`, and the encoded segment must be a single,
    /// prefix-safe path component so a slashed id can never nest under
    /// another thread's directory (heddle#572 r2).
    #[test]
    fn slashed_thread_id_checkout_and_manifest_agree() {
        let repo_dir = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();

        let checkout = default_thread_checkout_path(&repo, "foo/bar");
        let manifest = repo::thread_manifest::manifest_path(repo.heddle_dir(), "foo/bar");
        let threads_root = repo.heddle_dir().join("threads");

        // Both live in the same per-thread directory: checkout at
        // `<dir>/root`, manifest at `<dir>/manifest.toml`.
        assert_eq!(checkout.parent().unwrap(), manifest.parent().unwrap());
        // The slash is encoded into ONE segment directly under `threads/`.
        assert_eq!(checkout.parent().unwrap(), threads_root.join("foo%2Fbar"));
        assert_eq!(checkout.file_name().unwrap(), "root");

        // Neither escapes `.heddle/threads/`.
        assert!(checkout.starts_with(&threads_root));
        assert!(manifest.starts_with(&threads_root));

        // The bare `foo` thread's directory is NOT an ancestor of `foo/bar`'s
        // (the prefix-nesting class): `foo` → `threads/foo`, `foo/bar` →
        // `threads/foo%2Fbar`, which are disjoint siblings.
        let foo = default_thread_checkout_path(&repo, "foo");
        let foo_dir = foo.parent().unwrap();
        let bar_dir = checkout.parent().unwrap();
        assert!(!bar_dir.starts_with(foo_dir) && !foo_dir.starts_with(bar_dir));

        // Distinct slashed/dashed ids no longer collide.
        let other = default_thread_checkout_path(&repo, "foo-bar");
        assert_ne!(checkout, other);
    }
}
