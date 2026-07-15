// SPDX-License-Identifier: Apache-2.0
//! Thread list/show domain: collection, auto filter, and Git-ref splitting.
//!
//! Owns the rich thread summary assembly used by `heddle thread list` /
//! `heddle thread show` (git branch tips, task assignment, recommended
//! action, auto flag). CLI opens the repo, calls [`list_threads`] /
//! [`find_thread_summary`], then attaches verification and renders.
//!
//! Status keeps its own thinner [`crate::status::StatusThreadSummary`]
//! path; the two share underlying repo primitives but not the same
//! report shape.

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
};

use anyhow::Result;
use chrono::Utc;
use cli_shared::UserConfig;
use objects::{
    object::{ThreadName, Tree},
    store::{
        ActorPresence, ActorPresenceStatus, ActorPresenceStore, AgentTaskRecord, AgentTaskStore,
    },
};
use repo::{
    AgentUsageSummary, GitOverlayBranchTip, GitRemoteTrackingStatus, Repository,
    RepositoryOperationStatus, Thread, ThreadConfidenceSummary, ThreadFreshness,
    ThreadImpactCategory, ThreadIntegrationPolicy, ThreadManager, ThreadMode, ThreadRuntimeOverlay,
    ThreadState, ThreadVerificationSummary, ThreadView, describe_thread_advice,
    refresh_thread_freshness, shell_quote,
};
use serde::Serialize;
use sley::Repository as SleyRepository;

use crate::{
    ActionTemplate,
    status::{
        CoordinationStatus,
        next_action::{
            NextActionInput, canonical_git_repair_ref_preview_command, contextual_thread_action,
            effective_next_action, heddle_action,
        },
    },
    verify::{action_template, serialize_empty_action_as_null},
};

/// Options for [`list_threads`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThreadListOptions {
    /// When `false` (default for CLI), harness-created (`auto`) threads are
    /// omitted unless they are the current checkout lane.
    pub include_auto: bool,
}

impl ThreadListOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn include_auto(mut self, include_auto: bool) -> Self {
        self.include_auto = include_auto;
        self
    }
}

/// Machine domain report for `heddle thread list` (threads + git-only refs).
///
/// Presentation fields (repository label/context, verification, top-level
/// recommended action) stay on the CLI attach path so JSON output_kind
/// contract for the domain rows remains stable here.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadListReport {
    pub output_kind: &'static str,
    pub threads: Vec<ThreadListEntry>,
    pub available_git_refs: Vec<AvailableGitRef>,
    pub current: Option<String>,
}

/// One thread row for list/show machine output.
///
/// Field names match the historical CLI `ThreadSummary` JSON contract.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadListEntry {
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
    pub task_assignment_id: Option<String>,
    pub task_summary: Option<ThreadTaskSummary>,
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
    /// Mirror of [`repo::ThreadRecord::shared_target_dir`].
    pub shared_target_dir: Option<String>,
}

/// Stable alias matching historical CLI naming (`ThreadSummary`).
pub type ThreadSummary = ThreadListEntry;

/// Git-only branch tip that is not yet a Heddle thread tip.
#[derive(Debug, Clone, Serialize)]
pub struct AvailableGitRef {
    pub name: String,
    pub git_commit: String,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    pub recommended_action: String,
    pub recommended_action_template: Option<ActionTemplate>,
}

/// Actor attribution nested on a thread list entry.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ThreadActorInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Assigned agent task nested on a thread list entry.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ThreadTaskSummary {
    pub task_id: String,
    pub title: String,
    pub status: String,
    pub target_thread: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub coordination_discussion_id: Option<String>,
}

impl From<&AgentTaskRecord> for ThreadTaskSummary {
    fn from(task: &AgentTaskRecord) -> Self {
        Self {
            task_id: task.task_id.clone(),
            title: task.title.clone(),
            status: task.status.to_string(),
            target_thread: task.target_thread.clone(),
            updated_at: task.updated_at.to_rfc3339(),
            completed_at: task.completed_at.map(|time| time.to_rfc3339()),
            coordination_discussion_id: task.coordination_discussion_id.clone(),
        }
    }
}

impl ThreadListEntry {
    fn from_view(view: ThreadView, coordination_status: CoordinationStatus) -> Self {
        let mode = view.record.mode.clone();
        Self {
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
            task_assignment_id: None,
            task_summary: None,
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

/// Collect, filter, and split thread list domain for an opened repository.
pub fn list_threads(repo: &Repository, options: ThreadListOptions) -> Result<ThreadListReport> {
    let mut summaries = collect_thread_summaries(repo)?;
    if !options.include_auto {
        // Always keep the current thread visible even if it's auto:
        // hiding it from the user who is *standing in it* would be
        // worse than the noise it adds.
        summaries.retain(|summary| summary.is_current || !summary.auto);
    }
    let available_git_refs = split_available_git_refs(&mut summaries);
    let current = summaries
        .iter()
        .find(|summary| summary.is_current)
        .map(|summary| summary.name.clone())
        .or(repo.current_lane()?);
    Ok(ThreadListReport {
        output_kind: "thread_list",
        threads: summaries,
        available_git_refs,
        current,
    })
}

/// Collect full thread summaries (no auto filter / git-ref split).
pub fn collect_thread_summaries(repo: &Repository) -> Result<Vec<ThreadListEntry>> {
    let thread_refs = repo.refs().list_threads()?;
    let current = repo.current_lane()?;
    let operation = repo.operation_status()?;
    let remote_tracking = repo.git_remote_tracking_status().unwrap_or(None);
    let import_hint = repo.git_import_guidance().unwrap_or(None);
    let branch_tips = repo
        .git_overlay_branch_tips()
        .unwrap_or_default()
        .into_iter()
        .map(|tip| (tip.branch.clone(), tip))
        .collect::<HashMap<_, _>>();
    let registry = ActorPresenceStore::new(repo.heddle_dir());
    let task_store = AgentTaskStore::new(repo.heddle_dir());
    let thread_manager = ThreadManager::new(repo.heddle_dir());
    let mut entries_by_thread: HashMap<String, Vec<ActorPresence>> = HashMap::new();
    let mut threads_by_name: HashMap<String, Thread> = HashMap::new();
    for entry in registry.list()? {
        entries_by_thread
            .entry(entry.thread.clone())
            .or_default()
            .push(entry);
    }
    for mut thread in thread_manager.list()? {
        if thread.state == ThreadState::Abandoned
            && repo
                .refs()
                .get_thread(&ThreadName::new(&thread.thread))?
                .is_none()
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
        let entries = entries_by_thread.remove(&name).unwrap_or_default();
        let task_assignment_id = task_assignment_id_from_entries(&entries);
        let task_summary = task_summary_for_assignment(&task_store, task_assignment_id.as_deref())?;
        let (view, coordination_status) = build_thread_view(
            repo,
            current.as_ref() == Some(&name),
            name.clone(),
            entries,
            threads_by_name.remove(&name),
            branch_tips.get(&name).cloned(),
        )?;
        let mut summary = ThreadListEntry::from_view(view, coordination_status);
        summary.task_assignment_id = task_assignment_id;
        summary.task_summary = task_summary;
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
        apply_materialized_merge_advice(repo, &mut summary);
        if let Some(branch_tip) = branch_tips.get(&summary.name)
            && !has_heddle_tip
        {
            summary.blockers.clear();
            summary.thread_health = if branch_tip.history_imported {
                "imported".to_string()
            } else {
                "git_backed".to_string()
            };
            if summary.is_current {
                summary.recommended_action.clear();
            } else {
                summary.recommended_action = if branch_tip.branch.starts_with('-') {
                    format!(
                        "heddle thread switch -- {}",
                        shell_quote(&branch_tip.branch)
                    )
                } else {
                    format!("heddle thread switch {}", shell_quote(&branch_tip.branch))
                };
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
                canonical_git_repair_ref_preview_command(None, &summary.name);
        }
        if summary.is_current {
            enrich_current_summary_with_dirty_paths(repo, &mut summary)?;
            summary.operation = operation.clone();
            summary.remote_tracking = remote_tracking.clone();
            summary.recommended_action = effective_next_action(
                NextActionInput::default(
                    operation.as_ref(),
                    remote_tracking.as_ref(),
                    import_hint.as_ref(),
                    Some(&summary.recommended_action),
                )
                .with_source_authority(repo.source_authority())
                .current_thread(Some(&summary.thread_health)),
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
        summary.recommended_action_template = action_template(&summary.recommended_action);
    }

    summaries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(summaries)
}

/// Look up a single thread summary by name.
pub fn find_thread_summary(repo: &Repository, name: &str) -> Result<Option<ThreadListEntry>> {
    Ok(collect_thread_summaries(repo)?
        .into_iter()
        .find(|summary| summary.name == name))
}

/// Move available Git-only branch tips out of the main thread list.
pub fn split_available_git_refs(summaries: &mut Vec<ThreadListEntry>) -> Vec<AvailableGitRef> {
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

/// True when the entry is an imported Git branch already mapped into Heddle.
pub fn thread_is_imported_git_ref(entry: &ThreadListEntry) -> bool {
    !entry.is_current
        && entry.path.is_none()
        && entry.execution_path.is_none()
        && entry.target_thread.is_none()
        && entry.current_state.is_some()
        && entry.history_imported
        && (entry.git_branch_tip.is_some() || entry.name.starts_with("origin/"))
}

/// True when the entry is a Git branch tip without a Heddle tip.
pub fn thread_is_available_git_ref(entry: &ThreadListEntry) -> bool {
    !entry.is_current
        && entry.path.is_none()
        && entry.execution_path.is_none()
        && entry.target_thread.is_none()
        && entry.current_state.is_none()
        && entry.git_branch_tip.is_some()
}

/// Visibility label for an isolated thread mode.
pub fn visibility_label(mode: &ThreadMode) -> &'static str {
    match mode {
        ThreadMode::Materialized => "materialized",
        ThreadMode::Virtualized => "virtualized",
        ThreadMode::Solid => "solid",
    }
}

fn available_git_ref_from_summary(summary: &ThreadListEntry) -> AvailableGitRef {
    AvailableGitRef {
        name: summary.name.clone(),
        git_commit: summary.git_branch_tip.clone().unwrap_or_default(),
        recommended_action: summary.recommended_action.clone(),
        recommended_action_template: summary
            .recommended_action_template
            .clone()
            .or_else(|| action_template(&summary.recommended_action)),
    }
}

fn enrich_current_summary_with_dirty_paths(
    repo: &Repository,
    summary: &mut ThreadListEntry,
) -> Result<()> {
    let baseline = match repo.current_state()? {
        Some(state) => repo.require_tree(&state.tree)?,
        None => Tree::new(),
    };
    let options = UserConfig::default().worktree_status_options(Some(repo.config()));
    let status = repo.compare_worktree_cached_with_options(&baseline, &options)?;
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

fn stack_depth(summaries_by_name: &HashMap<String, ThreadListEntry>, thread: &str) -> usize {
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

fn primary_agent_entry(entries: &[ActorPresence]) -> Option<&ActorPresence> {
    entries
        .iter()
        .filter(|entry| entry.status == ActorPresenceStatus::Active)
        .max_by_key(|entry| entry.started_at)
        .or_else(|| entries.iter().max_by_key(|entry| entry.started_at))
}

fn task_assignment_id_from_entries(entries: &[ActorPresence]) -> Option<String> {
    primary_agent_entry(entries).and_then(|entry| entry.task_assignment_id.clone())
}

fn task_summary_for_assignment(
    store: &AgentTaskStore,
    task_assignment_id: Option<&str>,
) -> Result<Option<ThreadTaskSummary>> {
    let Some(task_assignment_id) = task_assignment_id else {
        return Ok(None);
    };
    Ok(store
        .load(task_assignment_id)?
        .as_ref()
        .map(ThreadTaskSummary::from))
}

fn build_thread_view(
    repo: &Repository,
    is_current: bool,
    name: String,
    entries: Vec<ActorPresence>,
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
                        .and_then(|tip| tip.mapped_state)
                        .or_else(|| {
                            repo.git_overlay_mapped_state_for_branch(&name)
                                .ok()
                                .flatten()
                        })
                })
                .flatten()
        })
        .map(|id| id.short());
    let has_heddle_tip = current_state.is_some();
    let active: Vec<&ActorPresence> = entries
        .iter()
        .filter(|entry| entry.status == ActorPresenceStatus::Active)
        .collect();
    let complete: Vec<&ActorPresence> = entries
        .iter()
        .filter(|entry| entry.status == ActorPresenceStatus::Complete)
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
                "using Git-backed branch tip {}",
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

fn apply_materialized_merge_advice(repo: &Repository, summary: &mut ThreadListEntry) {
    let Some(action) = materialized_merge_resolve_action(repo, summary) else {
        return;
    };
    summary.thread_health = "blocked".to_string();
    if summary.blockers.is_empty() {
        summary
            .blockers
            .push("Merge conflicts need resolution".to_string());
    }
    summary.recommended_action = action;
    summary.recommended_action_template = action_template(&summary.recommended_action);
}

fn materialized_merge_resolve_action(
    repo: &Repository,
    summary: &ThreadListEntry,
) -> Option<String> {
    if let Some(path) = summary.execution_path.as_deref() {
        let path = PathBuf::from(path);
        if !path.exists() {
            return None;
        }
        let thread_repo = Repository::open(&path).ok()?;
        return thread_repo
            .merge_state_manager()
            .is_merge_in_progress()
            .then(|| {
                heddle_action(vec![
                    "--repo".to_string(),
                    path.display().to_string(),
                    "resolve".to_string(),
                    "--list".to_string(),
                ])
            });
    }

    (summary.is_current && repo.merge_state_manager().is_merge_in_progress())
        .then(|| heddle_action(["resolve", "--list"]))
}

fn apply_terminal_thread_advice(summary: &mut ThreadListEntry) {
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

fn remote_tracking_local_ref(repo: &Repository, thread_name: &str) -> Option<String> {
    let git = SleyRepository::discover(repo.root()).ok()?;
    let remotes = git.remote_names().ok()?;
    remotes
        .iter()
        .find_map(|remote| thread_name.strip_prefix(&format!("{remote}/")))
        .filter(|branch| !branch.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use objects::object::ThreadName;
    use repo::{
        Thread, ThreadConfidenceSummary, ThreadFreshness, ThreadIntegrationPolicy, ThreadManager,
        ThreadMode, ThreadState, ThreadVerificationSummary,
    };
    use tempfile::TempDir;

    use super::*;

    fn sample_thread(name: &str, auto: bool) -> Thread {
        Thread {
            id: name.to_string(),
            thread: name.to_string(),
            target_thread: None,
            parent_thread: None,
            mode: ThreadMode::Materialized,
            state: ThreadState::Active,
            base_state: String::new(),
            base_root: String::new(),
            current_state: None,
            merged_state: None,
            task: None,
            execution_path: PathBuf::from("/tmp"),
            materialized_path: None,
            changed_paths: Vec::new(),
            impact_categories: Vec::new(),
            heavy_impact_paths: Vec::new(),
            promotion_suggested: false,
            freshness: ThreadFreshness::Unknown,
            verification_summary: ThreadVerificationSummary::default(),
            confidence_summary: ThreadConfidenceSummary::default(),
            integration_policy_result: ThreadIntegrationPolicy::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            ephemeral: None,
            auto,
            shared_target_dir: None,
        }
    }

    #[test]
    fn list_threads_empty_repo_returns_empty_domain_lists() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        // init_default may seed `main`; filter to pure empty by checking
        // that list_threads always succeeds and reports output_kind.
        let report = list_threads(&repo, ThreadListOptions::new()).unwrap();
        assert_eq!(report.output_kind, "thread_list");
        assert!(report.available_git_refs.is_empty());
        // A fresh default repo may include the default lane only.
        assert!(
            report
                .threads
                .iter()
                .all(|t| t.name == "main" || t.is_current),
            "unexpected threads: {:?}",
            report.threads.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["output_kind"], "thread_list");
        assert!(value["threads"].is_array());
        assert!(value["available_git_refs"].is_array());
    }

    #[test]
    fn list_threads_sorts_by_name() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let manager = ThreadManager::new(repo.heddle_dir());
        for name in ["zeta", "alpha", "mid"] {
            manager.save(&sample_thread(name, false)).unwrap();
            let _ = repo.refs().set_thread(
                &ThreadName::new(name),
                &objects::object::StateId::from_bytes([0u8; 32]),
            );
        }

        let report = list_threads(&repo, ThreadListOptions::new().include_auto(true)).unwrap();
        let names: Vec<&str> = report.threads.iter().map(|t| t.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "thread list must be sorted by name");
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"mid"));
        assert!(names.contains(&"zeta"));
    }

    #[test]
    fn list_threads_hides_auto_unless_include_auto_or_current() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let manager = ThreadManager::new(repo.heddle_dir());
        manager.save(&sample_thread("user-feature", false)).unwrap();
        manager
            .save(&sample_thread("harness-session", true))
            .unwrap();

        let filtered = list_threads(&repo, ThreadListOptions::new()).unwrap();
        let filtered_names: Vec<&str> = filtered.threads.iter().map(|t| t.name.as_str()).collect();
        assert!(
            filtered_names.contains(&"user-feature"),
            "user thread should remain: {filtered_names:?}"
        );
        assert!(
            !filtered_names.contains(&"harness-session"),
            "auto thread should be hidden by default: {filtered_names:?}"
        );

        let all = list_threads(&repo, ThreadListOptions::new().include_auto(true)).unwrap();
        let all_names: Vec<&str> = all.threads.iter().map(|t| t.name.as_str()).collect();
        assert!(
            all_names.contains(&"harness-session"),
            "include_auto must surface harness thread: {all_names:?}"
        );
    }

    #[test]
    fn available_git_ref_serializes_empty_recommended_action_as_null() {
        let value = serde_json::to_value(AvailableGitRef {
            name: "main".to_string(),
            git_commit: "0123456789abcdef".to_string(),
            recommended_action: String::new(),
            recommended_action_template: None,
        })
        .unwrap();
        assert!(value["recommended_action"].is_null());
    }

    #[test]
    fn split_available_git_refs_moves_git_only_tips() {
        let mut summaries = vec![
            ThreadListEntry {
                name: "feature".into(),
                operation: None,
                remote_tracking: None,
                base_state: None,
                base_root: None,
                current_state: Some("abc".into()),
                path: None,
                execution_path: None,
                session_id: None,
                heddle_session_id: None,
                actor: None,
                harness: None,
                thinking_level: None,
                native_actor_key: None,
                native_parent_actor_key: None,
                probe_source: None,
                probe_confidence: None,
                usage_summary: None,
                last_progress_at: None,
                last_activity_at: None,
                report_flush_state: None,
                attach_reason: None,
                thread_mode: None,
                thread_state: None,
                freshness: None,
                visibility: "ref_only".into(),
                target_thread: None,
                parent_thread: None,
                child_threads: vec![],
                sibling_threads: vec![],
                stack_depth: 0,
                stale_from_parent: false,
                task: None,
                task_assignment_id: None,
                task_summary: None,
                changed_paths: vec![],
                promotion_suggested: false,
                impact_categories: vec![],
                heavy_impact_paths: vec![],
                verification_summary: Default::default(),
                confidence_summary: Default::default(),
                integration_policy_result: Default::default(),
                coordination_status: CoordinationStatus::Clean,
                is_current: false,
                is_isolated: false,
                thread_health: "clean".into(),
                blockers: vec![],
                recommended_action: String::new(),
                recommended_action_template: None,
                git_branch_tip: None,
                history_imported: true,
                auto: false,
                shared_target_dir: None,
            },
            ThreadListEntry {
                name: "origin/main".into(),
                operation: None,
                remote_tracking: None,
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
                native_actor_key: None,
                native_parent_actor_key: None,
                probe_source: None,
                probe_confidence: None,
                usage_summary: None,
                last_progress_at: None,
                last_activity_at: None,
                report_flush_state: None,
                attach_reason: None,
                thread_mode: None,
                thread_state: None,
                freshness: None,
                visibility: "ref_only".into(),
                target_thread: None,
                parent_thread: None,
                child_threads: vec![],
                sibling_threads: vec![],
                stack_depth: 0,
                stale_from_parent: false,
                task: None,
                task_assignment_id: None,
                task_summary: None,
                changed_paths: vec![],
                promotion_suggested: false,
                impact_categories: vec![],
                heavy_impact_paths: vec![],
                verification_summary: Default::default(),
                confidence_summary: Default::default(),
                integration_policy_result: Default::default(),
                coordination_status: CoordinationStatus::Clean,
                is_current: false,
                is_isolated: false,
                thread_health: "git_backed".into(),
                blockers: vec![],
                recommended_action: "heddle thread switch origin/main".into(),
                recommended_action_template: None,
                git_branch_tip: Some("deadbeef".into()),
                history_imported: false,
                auto: false,
                shared_target_dir: None,
            },
        ];

        let available = split_available_git_refs(&mut summaries);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "feature");
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].name, "origin/main");
        assert_eq!(available[0].git_commit, "deadbeef");
    }
}
