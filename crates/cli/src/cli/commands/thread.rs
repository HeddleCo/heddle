// SPDX-License-Identifier: Apache-2.0
//! Thread commands.

use std::{
    collections::{BTreeSet, HashMap},
    path::PathBuf,
    process,
};

use anyhow::{Result, anyhow};
use chrono::Utc;
use objects::{
    object::{ChangeId, State},
    store::{AgentEntry, AgentRegistry, AgentStatus, current_boot_id},
};
use refs::{Head, RefExpectation, RefUpdate};
use repo::{
    AgentUsageSummary, GitOverlayBranchTip, GitRemoteTrackingStatus, Repository,
    RepositoryOperationStatus, Thread, ThreadConfidenceSummary, ThreadFreshness,
    ThreadImpactCategory, ThreadIntegrationPolicy, ThreadManager, ThreadMode, ThreadRuntimeOverlay,
    ThreadState, ThreadVerificationSummary, ThreadView, describe_thread_advice,
};
use serde::Serialize;

use super::{
    mount_lifecycle,
    operator_loop::primary_next_action,
    snapshot::{ensure_current_state, summarize_confidence, summarize_verification},
    thread_cmd::refresh_thread_freshness,
    worktree_cmd::{
        helpers::{prepare_worktree_target, write_isolated_checkout},
        shared_target,
    },
};
use crate::{
    cli::{Cli, ThreadListArgs, ThreadStartArgs, WorkspaceModeArg, should_output_json, style},
    config::{UserConfig, UserThreadWorkspaceMode},
};

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
    pub session_id: Option<String>,
    pub heddle_session_id: Option<String>,
    pub actor: Option<ThreadActorInfo>,
    pub harness: Option<String>,
    pub thinking_level: Option<String>,
    pub native_actor_key: Option<String>,
    pub native_parent_actor_key: Option<String>,
    pub probe_source: Option<String>,
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
    pub recommended_action: String,
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
pub struct ThreadActorInfo {
    pub provider: Option<String>,
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
                .map(|path| path.display().to_string()),
            execution_path: view
                .runtime
                .execution_path
                .as_ref()
                .map(|path| path.display().to_string()),
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
            visibility: visibility_label(&mode).to_string(),
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

#[derive(Serialize)]
struct ThreadListOutput {
    repository_capability: String,
    storage_model: String,
    hosted_enabled: bool,
    threads: Vec<ThreadSummary>,
    current: Option<String>,
    /// Carried for the human-readable renderer only. Not part of the
    /// JSON contract: import-hint information is exposed via
    /// `heddle bridge git status --json` instead.
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
pub(crate) struct ThreadOpOutput {
    pub name: String,
    pub message: String,
    pub thread: Option<ThreadSummary>,
    pub path: Option<String>,
    pub execution_path: Option<String>,
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
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
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
    let path = output
        .thread
        .as_ref()
        .and_then(|t| t.path.as_deref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "this thread has no filesystem checkout path; `--print-cd-path` only works for materialized workspaces"
            )
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

    println!("{}", style::section(&format!("Captures on {thread}")));
    if captures.is_empty() {
        println!(
            "  {}",
            style::dim("No captures recorded on this thread yet.")
        );
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
        .get_thread(thread)?
        .ok_or_else(|| anyhow!("Thread not found: {thread}"))?;
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
    let threads = repo.refs().list_threads()?;
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
            && repo.refs().get_thread(&thread.thread)?.is_none()
        {
            continue;
        }
        refresh_thread_freshness(repo, &mut thread)?;
        threads_by_name.insert(thread.thread.clone(), thread);
    }

    let mut names: BTreeSet<String> = threads.into_iter().collect();
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
        let thread = Thread {
            id: summary.name.clone(),
            thread: summary.name.clone(),
            target_thread: summary.target_thread.clone(),
            parent_thread: summary.parent_thread.clone(),
            mode: summary
                .thread_mode
                .clone()
                .unwrap_or(ThreadMode::Lightweight),
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
        if matches!(
            summary.thread_state,
            Some(ThreadState::Merged | ThreadState::Abandoned)
        ) {
            summary.thread_health = "clean".to_string();
            summary.blockers.clear();
            summary.recommended_action.clear();
            summary.coordination_status = CoordinationStatus::Clean;
        }
        if let Some(branch_tip) = branch_tips.get(&summary.name)
            && !branch_tip.history_imported
        {
            summary.thread_health = "tip_only".to_string();
            summary.blockers = vec![
                "Git branch is visible as a tip-only mirror; import its history to use history-oriented Heddle commands".to_string(),
            ];
            summary.recommended_action =
                format!("heddle bridge git import --ref {}", branch_tip.branch);
        }
        if summary.is_current {
            summary.operation = operation.clone();
            summary.remote_tracking = remote_tracking.clone();
            summary.recommended_action = primary_next_action(
                operation.as_ref(),
                remote_tracking.as_ref(),
                import_hint.as_ref(),
                Some(&summary.recommended_action),
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
    }

    summaries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(summaries)
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
    let current_state = repo.refs().get_thread(&name)?.map(|id| id.short());
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
                mode: ThreadMode::Lightweight,
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
            .unwrap_or(ThreadMode::Lightweight),
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
    if matches!(
        summary.thread_state,
        Some(ThreadState::Merged | ThreadState::Abandoned)
    ) {
        summary.thread_health = "clean".to_string();
        summary.blockers.clear();
        summary.recommended_action.clear();
        summary.coordination_status = CoordinationStatus::Clean;
    }
    if is_current {
        // Current-thread next-action enrichment. Same as the full path,
        // but we skip the operation/remote_tracking/import_hint reads
        // because the caller (status) already has those and threads
        // through different fields anyway.
        summary.recommended_action =
            primary_next_action(None, None, None, Some(&summary.recommended_action));
    }
    Ok(Some(summary))
}

pub(crate) fn visibility_label(mode: &ThreadMode) -> &'static str {
    match mode {
        ThreadMode::Materialized | ThreadMode::Lightweight => "heavy",
        ThreadMode::Virtualized => "light",
    }
}

pub(crate) fn git_history_label(history_imported: bool) -> &'static str {
    if history_imported {
        "full history available"
    } else {
        "tip available"
    }
}

pub(crate) fn cmd_thread_list(cli: &Cli, repo: &Repository, args: ThreadListArgs) -> Result<()> {
    let current = repo.current_lane()?;
    let mut summaries = collect_thread_summaries(repo)?;
    if !args.include_auto {
        // Always keep the current thread visible even if it's auto:
        // hiding it from the user who is *standing in it* would be
        // worse than the noise it adds.
        summaries.retain(|summary| summary.is_current || !summary.auto);
    }
    let output = ThreadListOutput {
        repository_capability: repo.capability_label().to_string(),
        storage_model: repo.storage_model_label().to_string(),
        hosted_enabled: repo.hosted_enabled(),
        git_overlay_import_hint: repo.git_overlay_import_hint()?.map(|hint| {
            ThreadListGitOverlayImportHintOutput {
                current_branch: hint.current_branch,
                missing_branch_count: hint.missing_branch_count,
                missing_branches: hint.missing_branches,
                recommended_command: hint.recommended_command,
            }
        }),
        threads: summaries,
        current,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else if output.threads.is_empty() {
        println!("No threads");
    } else {
        println!(
            "{} {} {}",
            style::bold("Threads"),
            style::dim("in"),
            output.repository_capability
        );
        println!(
            "Repository mode: {} {}",
            output.repository_capability,
            style::dim(&format!("({})", output.storage_model))
        );
        if output.hosted_enabled {
            println!("Hosted: {}", style::accent("enabled"));
        }
        if let Some(hint) = &output.git_overlay_import_hint {
            println!(
                "Git import: {} other Git branch(es) are available to import ({})",
                hint.missing_branch_count,
                crate::cli::render::preview_list(&hint.missing_branches, hint.missing_branch_count,)
            );
            println!("Next step: {}", style::bold(&hint.recommended_command));
        }
        render_thread_sections(&output.threads);
    }

    Ok(())
}

type ThreadSectionPredicate = fn(&ThreadSummary) -> bool;
type ThreadSection = (&'static str, ThreadSectionPredicate);

fn render_thread_sections(threads: &[ThreadSummary]) {
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
            render_thread_entry(&threads[index]);
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

fn thread_is_imported_git_ref(entry: &ThreadSummary) -> bool {
    entry.git_branch_tip.is_some()
        || (entry.path.is_none()
            && entry.execution_path.is_none()
            && entry.target_thread.is_none()
            && entry.history_imported
            && entry.name.starts_with("origin/"))
}

fn render_thread_entry(entry: &ThreadSummary) {
    let prefix = if entry.is_current {
        style::accent("*")
    } else {
        style::dim("-")
    };
    let state = entry.current_state.as_deref().unwrap_or("(no state)");
    println!(
        "{} {} {} {} {}",
        prefix,
        style::bold(&entry.name),
        style::dim(state),
        style::thread_state(&entry.coordination_status.to_string()),
        style::dim(&entry.visibility)
    );
    if let Some(path) = &entry.path {
        println!("    path: {}", path);
    } else if let Some(path) = &entry.execution_path {
        println!("    execution root: {}", path);
    }
    if let Some(git_branch_tip) = &entry.git_branch_tip {
        println!(
            "    git tip: {} {}",
            style::dim(git_branch_tip),
            style::dim(&format!("({})", git_history_label(entry.history_imported)))
        );
    }
    if let Some(state) = &entry.thread_state {
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
        println!("    sync: {}", style::warn(&remote_tracking.message));
    }
    if let Some(actor) = &entry.actor
        && let Some(text) =
            crate::cli::render::actor_display(actor.provider.as_deref(), actor.model.as_deref())
    {
        println!("    actor: {text}");
    }
    if let Some(task) = &entry.task {
        println!("    task: {}", task);
    }
    if let Some(parent) = &entry.parent_thread {
        println!("    parent: {}", parent);
    }
    if !entry.child_threads.is_empty() {
        println!("    children: {}", entry.child_threads.join(", "));
    }
    if entry.promotion_suggested && !entry.heavy_impact_paths.is_empty() {
        println!(
            "    promotion: suggested ({})",
            crate::cli::render::preview_list(
                &entry.heavy_impact_paths,
                entry.heavy_impact_paths.len(),
            )
        );
    }
    if !entry.impact_categories.is_empty() {
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
    if !entry.recommended_action.is_empty() {
        println!("    next step: {}", style::bold(&entry.recommended_action));
    }
}

pub(crate) fn start_thread(repo: &Repository, args: ThreadStartArgs) -> Result<ThreadOpOutput> {
    let existing = find_active_thread_entry(repo, &args.name)?;
    if let Some(entry) = existing {
        if let Some(ref requested_path) = args.path {
            let requested = absolute_path(requested_path)?;
            let existing_path = entry
                .path
                .as_ref()
                .ok_or_else(|| anyhow!("Thread '{}' is already active", args.name))?;
            if *existing_path != requested {
                return Err(anyhow!(
                    "Thread '{}' already has an active reservation at '{}'. Use `heddle thread show {}` to inspect it, or release that session before starting another writer.",
                    args.name,
                    existing_path.display(),
                    args.name
                ));
            }
        }

        let message = if let Some(path) = entry.path {
            format!(
                "Thread '{}' already has an active reservation at '{}'. Use `heddle thread show {}` to inspect it, or release that session before starting another writer.",
                args.name,
                path.display(),
                args.name
            )
        } else {
            format!(
                "Thread '{}' already has an active reservation. Use `heddle thread show {}` to inspect it, or release that session before starting another writer.",
                args.name, args.name
            )
        };
        return Err(anyhow!(message));
    }

    let existing_thread_state = repo.refs().get_thread(&args.name)?;
    let base_state = match (&args.from, existing_thread_state) {
        (Some(spec), Some(existing)) => {
            let requested = repo
                .resolve_state(spec)?
                .ok_or_else(|| anyhow!("State '{}' not found", spec))?;
            if requested != existing {
                return Err(anyhow!(
                    "Thread '{}' is anchored at {}, but --from resolved to {}. Start a new thread name or refresh/rebase this thread before attaching another workspace.",
                    args.name,
                    existing.short(),
                    requested.short()
                ));
            }
            existing
        }
        (None, Some(existing)) => existing,
        (Some(spec), None) => repo
            .resolve_state(spec)?
            .ok_or_else(|| anyhow!("State '{}' not found", spec))?,
        (None, None) => ensure_current_state(
            repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some(format!(
                "Bootstrap git-overlay before starting {}",
                args.name
            )),
        )?,
    };

    if let Some(existing) = existing_thread_state {
        repo.refs()
            .set_thread_cas(&args.name, RefExpectation::Value(existing), &base_state)?;
    } else {
        repo.refs()
            .set_thread_cas(&args.name, RefExpectation::Missing, &base_state)?;
        repo.oplog()
            .record_thread_create(&args.name, &base_state, Some(&repo.op_scope()))?;
    }

    let thread_mode = resolve_thread_mode(repo, &args);
    let path = match thread_mode {
        ThreadMode::Materialized => args
            .path
            .clone()
            .unwrap_or_else(|| default_thread_path(repo, &args.name)),
        ThreadMode::Lightweight => default_lightweight_thread_path(repo, &args.name),
        ThreadMode::Virtualized => default_virtualized_thread_path(repo, &args.name),
    };
    let abs_path = prepare_worktree_target(repo, &path)?;

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
        && matches!(
            thread_mode,
            ThreadMode::Materialized | ThreadMode::Lightweight
        ) {
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
        && matches!(
            thread_mode,
            ThreadMode::Materialized | ThreadMode::Lightweight
        )
        && shared_target::should_advise_shared_target(repo)
    {
        shared_target::print_advisory(&args.name);
    }

    // Track whether `write_cargo_config` actually applied the
    // redirect. When the user has pre-staged `.cargo/config.toml`
    // the writer is a no-op and we must NOT advertise a
    // `shared_target_dir` on the thread record — `thread show`
    // would otherwise lie about a redirect that isn't in effect.
    let mut shared_target_dir_path = shared_target_dir_path;
    match thread_mode {
        ThreadMode::Materialized | ThreadMode::Lightweight => {
            write_isolated_checkout(repo, &abs_path, &base_state, Some(&args.name))?;
            if let Some(dir) = shared_target_dir_path.as_ref() {
                let applied = shared_target::write_cargo_config(&abs_path, dir)?;
                if !applied {
                    tracing::info!(
                        thread = %args.name,
                        config = %abs_path.join(".cargo").join("config.toml").display(),
                        "existing .cargo/config.toml preserved; --shared-target redirect not applied"
                    );
                    shared_target_dir_path = None;
                }
            }
        }
        ThreadMode::Virtualized => {
            // Light workspaces use the daemon-owned mount by default:
            // `heddled` keeps the FUSE
            // session alive after this CLI exits and across subsequent
            // invocations. `--no-daemon` opts out and pins the mount to
            // this process (the legacy behaviour). When the daemon is
            // unavailable on this host (no `fusermount`, exec failed,
            // etc.) we silently fall back to the in-process path with
            // a warning so the user still gets a working mount. See
            // `docs/design/mount-daemon.md` § History.
            let ownership =
                mount_lifecycle::MountOwnership::from_flags(args.daemon, args.no_daemon);
            mount_lifecycle::establish_virtualized_mount(
                repo.root(),
                &args.name,
                &abs_path,
                ownership,
            )?;
        }
    }

    let registry = AgentRegistry::new(repo.heddle_dir());
    let provider = args.agent_provider.clone();
    let model = args.agent_model.clone();
    let task = args.task.clone();
    let path_for_entry = abs_path.clone();
    let thread_name = args.name.clone();
    let current_target_thread = match repo.head_ref()? {
        Head::Attached { thread } => Some(thread),
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
        .ok_or_else(|| anyhow!("Base state '{}' not found", base_state.short()))?;
    let (base_root, verification_summary, confidence_summary) = base_state_summary;
    let thread_manager = ThreadManager::new(repo.heddle_dir());
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
        task: task.clone(),
        execution_path: abs_path.clone(),
        materialized_path: match thread_mode {
            ThreadMode::Materialized | ThreadMode::Lightweight => Some(abs_path.clone()),
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
        created_at: Utc::now(),
        updated_at: Utc::now(),
        ephemeral: None,
        auto: false,
        shared_target_dir: shared_target_dir_path.clone(),
    };
    thread_manager.save(&thread_state)?;
    let entry = registry.create_generated_entry_for_thread(&thread_name, |session_id| {
        Ok(AgentEntry {
            session_id: session_id.to_string(),
            client_instance_id: None,
            native_actor_key: None,
            native_parent_actor_key: None,
            native_instance_key: None,
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
            anchor_root: Some(base_root.clone()),
            reservation_token: Some(objects::store::generate_agent_id()),
            path: match thread_mode {
                ThreadMode::Materialized | ThreadMode::Lightweight | ThreadMode::Virtualized => {
                    Some(path_for_entry.clone())
                }
            },
            base_state: base_short.clone(),
            started_at: Utc::now(),
            provider: provider.clone(),
            model: model.clone(),
            harness: None,
            thinking_level: None,
            usage_summary: AgentUsageSummary::default(),
            last_progress_at: None,
            report_flush_state: None,
            attach_reason: Some(format!(
                "actor {session_id} was created when thread {} started",
                thread_name
            )),
            attach_precedence: vec!["thread-start".to_string()],
            winning_attach_rule: Some("thread-start".to_string()),
            probe_source: Some("explicit_payload".to_string()),
            probe_confidence: Some(1.0),
            status: AgentStatus::Active,
            completed_at: None,
            context_queries: vec![],
        })
    })?;

    let summary = find_thread_summary(repo, &args.name)?;
    let message = match thread_mode {
        ThreadMode::Lightweight | ThreadMode::Materialized => {
            format!(
                "Started heavy thread '{}' at '{}'",
                args.name,
                abs_path.display()
            )
        }
        ThreadMode::Virtualized => {
            // Print the mount path so the user knows where to `cd`.
            // The trailing newline before the next status line keeps
            // the path easy to copy-paste from terminal output.
            format!(
                "Started light thread '{}' mounted at '{}'",
                args.name,
                abs_path.display()
            )
        }
    };

    Ok(ThreadOpOutput {
        name: args.name,
        message,
        path: summary.as_ref().and_then(|thread| thread.path.clone()),
        execution_path: Some(abs_path.display().to_string()),
        thread: summary.map(|mut thread| {
            thread.session_id = Some(entry.session_id.clone());
            thread
        }),
    })
}

fn resolve_thread_mode(repo: &Repository, args: &ThreadStartArgs) -> ThreadMode {
    if args.path.is_some() {
        // An explicit `--path` means "put the heavy checkout here".
        // Light workspaces stay Heddle-managed so a mount never shadows
        // a user-named directory.
        return ThreadMode::Materialized;
    }

    match args.workspace {
        WorkspaceModeArg::Heavy => ThreadMode::Lightweight,
        WorkspaceModeArg::Light => ThreadMode::Virtualized,
        WorkspaceModeArg::Auto => match resolve_auto_workspace_default(repo, args) {
            UserThreadWorkspaceMode::Heavy => ThreadMode::Lightweight,
            UserThreadWorkspaceMode::Light => ThreadMode::Virtualized,
            UserThreadWorkspaceMode::Auto => ThreadMode::Lightweight,
        },
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
            .unwrap_or(UserThreadWorkspaceMode::Heavy)
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
        .set_thread_cas(&name, RefExpectation::Missing, &current)?;
    repo.oplog()
        .record_thread_create(&name, &current, Some(&repo.op_scope()))?;

    // Persist a Thread record so subsequent commands that go through
    // `ThreadManager::load` (delegate, ship, integration policy,
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
        .ok_or_else(|| anyhow!("Base state '{}' not found", base_short))?;
    let target_thread = match repo.head_ref()? {
        Head::Attached { thread } => Some(thread),
        Head::Detached { .. } => None,
    };
    let thread_manager = ThreadManager::new(repo.heddle_dir());
    let now = Utc::now();
    let thread_state = Thread {
        id: name.clone(),
        thread: name.clone(),
        target_thread,
        parent_thread: None,
        mode: ThreadMode::Lightweight,
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

    let output = ThreadOpOutput {
        name: name.clone(),
        message: format!("Created thread '{}' at {}", name, current.short()),
        path: None,
        execution_path: None,
        thread: find_thread_summary(repo, &name)?,
    };

    render_thread_op(cli, output)
}

pub(crate) fn cmd_thread_switch(cli: &Cli, repo: &Repository, name: String) -> Result<()> {
    let state = repo
        .refs()
        .get_thread(&name)?
        .ok_or_else(|| anyhow!("Thread not found: {}", name))?;

    // "Invisible thread directories" rule: switching to a thread that has
    // its *own* dedicated worktree (the one `heddle start --workspace
    // private|virtualized` recorded under `.run-heddle-threads/<name>/`)
    // is a metadata-only operation. The on-disk worktree at the
    // recorded path is already X's worktree — it was set up by `start`
    // and is kept in sync by the metadata-driven merge/rebase/goto/ship
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
        repo.refs().write_head(&Head::Attached {
            thread: name.clone(),
        })?;
    } else {
        // Legacy shared-worktree path: materialize the target tree at
        // CWD and reattach HEAD to the thread. Intentional raw `goto`:
        // `fast_forward_attached` would re-attach to the previously
        // attached thread, which is the wrong behavior here.
        repo.goto(&state)?;
        repo.refs().write_head(&Head::Attached {
            thread: name.clone(),
        })?;
    }

    let summary = find_thread_summary(repo, &name)?;
    let mut message = format!("Switched to thread '{}'", name);
    if let Some(thread) = &summary
        && thread.coordination_status != CoordinationStatus::Clean
    {
        message.push_str(&format!(" [{}]", thread.coordination_status));
    }

    render_thread_op(
        cli,
        ThreadOpOutput {
            name,
            message,
            path: summary.as_ref().and_then(|thread| thread.path.clone()),
            execution_path: summary
                .as_ref()
                .and_then(|thread| thread.execution_path.clone()),
            thread: summary,
        },
    )
}

pub fn cmd_thread_show(cli: &Cli, repo: &Repository, name: Option<String>) -> Result<()> {
    let name = super::thread_cmd::resolve_thread_name_or_current(repo, name)?;

    let summary =
        find_thread_summary(repo, &name)?.ok_or_else(|| anyhow!("Thread not found: {}", name))?;

    show_thread_summary(cli, repo, &summary)
}

pub(crate) fn show_thread_summary(
    cli: &Cli,
    repo: &Repository,
    summary: &ThreadSummary,
) -> Result<()> {
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(summary)?);
    } else {
        println!(
            "Repository mode: {} ({})",
            repo.capability_label(),
            repo.storage_model_label()
        );
        if repo.hosted_enabled() {
            println!("Hosted: enabled");
        }
        if let Some(operation) = &summary.operation {
            println!(
                "In progress: {} {} ({})",
                operation.scope, operation.kind, operation.state
            );
        }
        if let Some(remote_tracking) = &summary.remote_tracking {
            println!("Remote drift: {}", remote_tracking.message);
        }
        println!();
        println!("Thread: {}", summary.name);
        println!("Status: {}", summary.coordination_status);
        if let Some(base) = &summary.base_state {
            println!("Base: {}", base);
        }
        if let Some(base_root) = &summary.base_root {
            println!("Base root: {}", base_root);
        }
        if let Some(current) = &summary.current_state {
            println!("Current: {}", current);
        }
        if let Some(git_branch_tip) = &summary.git_branch_tip {
            println!("Git tip: {}", git_branch_tip);
            println!("History: {}", git_history_label(summary.history_imported));
        }
        if let Some(path) = &summary.path {
            println!("Path: {}", path);
        } else if let Some(path) = &summary.execution_path {
            println!("Execution root: {}", path);
        }
        println!("Workspace: {}", summary.visibility);
        if let Some(shared) = &summary.shared_target_dir {
            println!("Shared cargo target: {}", shared);
        }
        if let Some(state) = &summary.thread_state {
            println!("Lifecycle: {}", state);
        }
        if let Some(freshness) = &summary.freshness
            && *freshness != ThreadFreshness::Unknown
        {
            println!("Sync: {}", freshness);
        }
        if let Some(target) = &summary.target_thread {
            println!("Target thread: {}", target);
        }
        if let Some(parent) = &summary.parent_thread {
            println!("Parent thread: {}", parent);
        }
        if !summary.child_threads.is_empty() {
            println!("Child threads: {}", summary.child_threads.join(", "));
        }
        if !summary.sibling_threads.is_empty() {
            println!("Sibling threads: {}", summary.sibling_threads.join(", "));
        }
        if summary.stack_depth > 0 {
            println!("Stack depth: {}", summary.stack_depth);
        }
        if summary.stale_from_parent {
            println!("Parent drift: parent moved since this thread last refreshed");
        }
        if let Some(actor) = &summary.actor
            && let Some(text) =
                crate::cli::render::actor_display(actor.provider.as_deref(), actor.model.as_deref())
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
        if let Some(last_activity_at) = &summary.last_activity_at {
            println!("Last activity: {}", last_activity_at);
        }
        if let Some(report_flush_state) = &summary.report_flush_state {
            println!("Report flush: {}", report_flush_state);
        }
        if let Some(attach_reason) = &summary.attach_reason {
            println!("Attach: {}", attach_reason);
        }
        if let Some(usage_summary) = &summary.usage_summary {
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
        let captures = collect_thread_captures(repo, &summary.name, 5).unwrap_or_default();
        if !captures.is_empty() {
            println!();
            println!("{}", style::section("Last 5 captures"));
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
        if !summary.recommended_action.is_empty() {
            println!("Next step: {}", summary.recommended_action);
        }
    }

    Ok(())
}

pub(crate) fn cmd_thread_delete(cli: &Cli, repo: &Repository, name: String) -> Result<()> {
    if let Head::Attached { thread } = repo.head_ref()?
        && thread == name
    {
        return Err(anyhow!(
            "Cannot delete current thread. Switch to another thread first."
        ));
    }

    let state = repo
        .refs()
        .delete_thread(&name)?
        .ok_or_else(|| anyhow!("Thread not found: {}", name))?;

    repo.oplog()
        .record_thread_delete(&name, &state, Some(&repo.op_scope()))?;

    let output = ThreadOpOutput {
        name: name.clone(),
        message: format!("Deleted thread '{}'", name),
        path: None,
        execution_path: None,
        thread: None,
    };

    render_thread_op(cli, output)
}

pub(crate) fn cmd_thread_rename(
    cli: &Cli,
    repo: &Repository,
    old: String,
    new: String,
) -> Result<()> {
    let state = repo
        .refs()
        .get_thread(&old)?
        .ok_or_else(|| anyhow!("Thread not found: {}", old))?;

    let mut updates = vec![
        RefUpdate::Thread {
            name: new.clone(),
            expected: RefExpectation::Missing,
            new: Some(state),
        },
        RefUpdate::Thread {
            name: old.clone(),
            expected: RefExpectation::Value(state),
            new: None,
        },
    ];

    if let Head::Attached { thread } = repo.head_ref()?
        && thread == old
    {
        updates.push(RefUpdate::Head {
            expected: RefExpectation::Value(Head::Attached {
                thread: old.clone(),
            }),
            new: Head::Attached {
                thread: new.clone(),
            },
        });
    }

    repo.refs().update_refs(&updates)?;
    repo.oplog()
        .record_thread_rename(&old, &new, &state, Some(&repo.op_scope()))?;

    let output = ThreadOpOutput {
        name: new.clone(),
        message: format!("Renamed thread '{}' to '{}'", old, new),
        path: None,
        execution_path: None,
        thread: find_thread_summary(repo, &new)?,
    };

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
                println!("    cd {}", style::accent(path));
            } else if let Some(path) = &thread.execution_path {
                println!("Execution root: {}", style::dim(path));
            }
            if !thread.recommended_action.is_empty() {
                println!("Next step: {}", style::bold(&thread.recommended_action));
            }
        }
    }
    Ok(())
}

fn default_thread_path(repo: &Repository, name: &str) -> PathBuf {
    let workspace_root = shared_workspace_root(repo);
    let repo_name = workspace_root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("heddle");
    let parent = workspace_root
        .parent()
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| workspace_root.to_path_buf());
    parent.join(format!("{repo_name}-{}", sanitize_name(name)))
}

fn default_lightweight_thread_path(repo: &Repository, name: &str) -> PathBuf {
    let workspace_root = shared_workspace_root(repo);
    let repo_name = workspace_root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("heddle");
    let parent = workspace_root
        .parent()
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| workspace_root.to_path_buf());
    parent
        .join(format!(".{repo_name}-heddle-threads"))
        .join(sanitize_name(name))
        .join("root")
}

/// Mount-point path for a virtualized thread. Sibling to the
/// lightweight checkout path so a single repo can host both kinds
/// of threads side-by-side without colliding.
///
/// Template: `<repo_parent>/.<repo_name>-heddle-mounts/<sanitized_name>/`
fn default_virtualized_thread_path(repo: &Repository, name: &str) -> PathBuf {
    let workspace_root = shared_workspace_root(repo);
    let repo_name = workspace_root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("heddle");
    let parent = workspace_root
        .parent()
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| workspace_root.to_path_buf());
    mount_lifecycle::default_virtualized_mount_path(&parent, repo_name, &sanitize_name(name))
}

fn shared_workspace_root(repo: &Repository) -> &std::path::Path {
    repo.heddle_dir().parent().unwrap_or_else(|| repo.root())
}

fn sanitize_name(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.chars() {
        let keep = ch.is_ascii_alphanumeric();
        if keep {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
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