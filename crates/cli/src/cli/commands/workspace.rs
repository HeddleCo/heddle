// SPDX-License-Identifier: Apache-2.0
//! Workspace control-tower command.

use anyhow::Result;
use repo::{
    GitRemoteTrackingStatus, Repository, RepositoryOperationStatus, ThreadFreshness, ThreadState,
};
use serde::Serialize;
use tokio::time::{Duration, sleep};

use super::{
    git_overlay_health::{
        RepositoryTrustState, build_plain_git_trust_probe, build_repository_trust_state,
    },
    operator_loop::primary_next_action,
    thread::{ThreadSummary, collect_thread_summaries, git_history_label},
};
use crate::cli::{Cli, WorkspaceShowArgs, should_output_json, style};

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceThreadGroup {
    pub id: String,
    pub label: String,
    pub threads: Vec<ThreadSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceSummaryOutput {
    pub repository: String,
    pub repository_capability: String,
    pub storage_model: String,
    pub hosted_enabled: bool,
    pub operation: Option<RepositoryOperationStatus>,
    pub remote_tracking: Option<GitRemoteTrackingStatus>,
    pub trust: RepositoryTrustState,
    pub recommended_action: String,
    pub current_thread: Option<String>,
    pub groups: Vec<WorkspaceThreadGroup>,
    pub thread_count: usize,
    /// Carried for the human-readable renderer only. Not part of the
    /// JSON contract: import-hint information is exposed via
    /// `heddle bridge git status --json` instead.
    #[serde(skip)]
    pub git_overlay_import_hint: Option<WorkspaceGitOverlayImportHintOutput>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceGitOverlayImportHintOutput {
    pub current_branch: String,
    pub missing_branch_count: usize,
    pub missing_branches: Vec<String>,
    pub recommended_command: String,
}

pub async fn cmd_workspace(
    cli: &Cli,
    command: Option<crate::cli::WorkspaceCommands>,
) -> Result<()> {
    match command.unwrap_or(crate::cli::WorkspaceCommands::Show(
        WorkspaceShowArgs::default(),
    )) {
        crate::cli::WorkspaceCommands::Show(args) => cmd_workspace_show(cli, args).await,
    }
}

pub async fn cmd_workspace_show(cli: &Cli, args: WorkspaceShowArgs) -> Result<()> {
    if args.watch {
        return watch_workspace(cli, args.watch_iterations, args.watch_interval_ms).await;
    }

    let output = build_workspace_output(cli)?;
    render_workspace(cli, &output);
    Ok(())
}

pub(crate) fn build_workspace_output(cli: &Cli) -> Result<WorkspaceSummaryOutput> {
    let current_dir = std::env::current_dir()?;
    let repo_path = cli.repo.as_ref().unwrap_or(&current_dir);
    if let Some(probe) = build_plain_git_trust_probe(repo_path)? {
        return Ok(WorkspaceSummaryOutput {
            repository: probe.root.display().to_string(),
            repository_capability: "plain-git".to_string(),
            storage_model: "git".to_string(),
            hosted_enabled: false,
            git_overlay_import_hint: probe.git_branch.clone().map(|branch| {
                WorkspaceGitOverlayImportHintOutput {
                    current_branch: branch.clone(),
                    missing_branch_count: 1,
                    missing_branches: vec![branch.clone()],
                    recommended_command: format!("heddle bridge git import --ref {branch}"),
                }
            }),
            operation: None,
            remote_tracking: None,
            trust: probe.trust.clone(),
            recommended_action: probe.trust.recommended_action,
            current_thread: None,
            groups: Vec::new(),
            thread_count: 0,
        });
    }

    let repo = Repository::open(repo_path)?;
    let mut summaries = collect_thread_summaries(&repo)?;
    let current_thread = repo.current_lane()?;

    let current_name = current_thread.clone();
    let current_stack = current_name
        .as_deref()
        .map(|thread| stack_members(&summaries, thread))
        .unwrap_or_default();

    let mut current = Vec::new();
    let mut stacked = Vec::new();
    let mut parallel = Vec::new();
    let mut ready = Vec::new();
    let mut blocked = Vec::new();
    let mut recent = Vec::new();

    summaries.sort_by(|a, b| a.name.cmp(&b.name));
    for summary in summaries {
        if summary.is_current {
            current.push(summary);
            continue;
        }
        if summary.thread_state == Some(ThreadState::Merged) {
            recent.push(summary);
            continue;
        }
        if is_blocked(&summary) {
            blocked.push(summary);
            continue;
        }
        if summary.thread_state == Some(ThreadState::Ready) {
            ready.push(summary);
            continue;
        }
        if current_stack.contains(&summary.name) {
            stacked.push(summary);
            continue;
        }
        parallel.push(summary);
    }

    let groups = vec![
        WorkspaceThreadGroup {
            id: "current".to_string(),
            label: "Current thread".to_string(),
            threads: current,
        },
        WorkspaceThreadGroup {
            id: "stacked".to_string(),
            label: "Stacked child threads".to_string(),
            threads: stacked,
        },
        WorkspaceThreadGroup {
            id: "parallel".to_string(),
            label: "Parallel threads".to_string(),
            threads: parallel,
        },
        WorkspaceThreadGroup {
            id: "ready".to_string(),
            label: "Ready to merge".to_string(),
            threads: ready,
        },
        WorkspaceThreadGroup {
            id: "blocked".to_string(),
            label: "Blocked or stale".to_string(),
            threads: blocked,
        },
        WorkspaceThreadGroup {
            id: "recent".to_string(),
            label: "Recently merged".to_string(),
            threads: recent,
        },
    ]
    .into_iter()
    .filter(|group| !group.threads.is_empty())
    .collect::<Vec<_>>();

    let thread_count = groups.iter().map(|group| group.threads.len()).sum();

    let operation = repo.operation_status()?;
    let remote_tracking = repo.git_remote_tracking_status()?;
    let import_hint = repo.git_overlay_import_hint()?;
    let trust = build_repository_trust_state(&repo);
    let current_summary = groups
        .iter()
        .flat_map(|group| group.threads.iter())
        .find(|thread| thread.is_current);
    let thread_recommended_action =
        current_summary.map(|thread| thread.recommended_action.as_str());
    let recommended_action = if trust.trusted {
        primary_next_action(
            operation.as_ref(),
            remote_tracking.as_ref(),
            import_hint.as_ref(),
            thread_recommended_action,
        )
    } else {
        trust.recommended_action.clone()
    };

    Ok(WorkspaceSummaryOutput {
        repository: repo.root().display().to_string(),
        repository_capability: repo.capability_label().to_string(),
        storage_model: repo.storage_model_label().to_string(),
        hosted_enabled: repo.hosted_enabled(),
        git_overlay_import_hint: import_hint.clone().map(|hint| {
            WorkspaceGitOverlayImportHintOutput {
                current_branch: hint.current_branch,
                missing_branch_count: hint.missing_branch_count,
                missing_branches: hint.missing_branches,
                recommended_command: hint.recommended_command,
            }
        }),
        operation: operation.clone(),
        remote_tracking: remote_tracking.clone(),
        trust,
        recommended_action,
        current_thread,
        groups,
        thread_count,
    })
}

fn stack_members(summaries: &[ThreadSummary], root: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut frontier = vec![root.to_string()];
    while let Some(parent) = frontier.pop() {
        for summary in summaries
            .iter()
            .filter(|summary| summary.parent_thread.as_deref() == Some(parent.as_str()))
        {
            out.push(summary.name.clone());
            frontier.push(summary.name.clone());
        }
    }
    out
}

fn is_blocked(summary: &ThreadSummary) -> bool {
    summary.stale_from_parent
        || summary.blockers.iter().any(|_| true)
        || summary.thread_health == "blocked"
        || matches!(
            summary.coordination_status,
            super::thread::CoordinationStatus::Blocked
                | super::thread::CoordinationStatus::Diverged
        )
}

fn render_workspace(cli: &Cli, output: &WorkspaceSummaryOutput) {
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::to_string(output).expect("workspace JSON serializes")
        );
        return;
    }

    println!("Workspace: {}", style::bold(&output.repository));
    println!(
        "Repository mode: {} ({})",
        output.repository_capability, output.storage_model
    );
    if output.hosted_enabled {
        println!("Hosted: enabled");
    }
    if let Some(operation) = &output.operation {
        println!(
            "In progress: {} {} ({})",
            operation.scope, operation.kind, operation.state
        );
    } else if let Some(remote_tracking) = &output.remote_tracking {
        println!("Remote drift: {}", remote_tracking.message);
    } else if let Some(hint) = &output.git_overlay_import_hint {
        println!(
            "Git import: {} other branch(es) still live only in Git ({})",
            hint.missing_branch_count,
            crate::cli::render::preview_list(&hint.missing_branches, hint.missing_branch_count,)
        );
    }
    if !output.recommended_action.is_empty() {
        println!("Next step: {}", style::dim(&output.recommended_action));
    }
    if let Some(current) = &output.current_thread {
        println!("Current thread: {}", style::bold(current));
    }
    println!("Threads in flight: {}", output.thread_count);
    println!();

    for group in &output.groups {
        // Group labels (e.g. "Active", "Ready") are headers — bold.
        println!("{}:", style::bold(&group.label));
        for thread in &group.threads {
            // Thread name is the row anchor; the bracketed status
            // pair carries the operational signal so we colour the
            // coordination status by its semantic.
            println!(
                "  {} [{} · {}]",
                style::bold(&thread.name),
                style::dim(&thread.visibility),
                style::thread_state(&thread.coordination_status.to_string()),
            );
            if let Some(task) = &thread.task {
                println!("    task: {task}");
            }
            if let Some(target) = &thread.target_thread {
                println!("    target: {}", style::dim(target));
            }
            if let Some(parent) = &thread.parent_thread {
                println!("    parent: {}", style::dim(parent));
            }
            if !thread.child_threads.is_empty() {
                println!("    children: {}", thread.child_threads.join(", "));
            }
            if let Some(freshness) = &thread.freshness
                && *freshness != ThreadFreshness::Unknown
            {
                println!("    sync: {}", style::thread_state(&freshness.to_string()));
            }
            if let Some(git_branch_tip) = &thread.git_branch_tip {
                println!(
                    "    git tip: {} ({})",
                    style::dim(git_branch_tip),
                    git_history_label(thread.history_imported)
                );
            }
            if let Some(actor) = &thread.actor
                && let Some(text) = crate::cli::render::actor_display(
                    actor.provider.as_deref(),
                    actor.model.as_deref(),
                )
            {
                println!("    actor: {}", style::dim(&text));
            }
            if let Some(last_activity_at) = &thread.last_activity_at {
                println!("    last activity: {}", style::dim(last_activity_at));
            }
            if !thread.blockers.is_empty() {
                println!(
                    "    blockers: {}",
                    style::warn(&thread.blockers.join(" | "))
                );
            }
            if !thread.recommended_action.is_empty() {
                println!("    next: {}", style::bold(&thread.recommended_action));
            }
        }
        println!();
    }
}

async fn watch_workspace(
    cli: &Cli,
    watch_iterations: Option<usize>,
    watch_interval_ms: Option<u64>,
) -> Result<()> {
    let interval = Duration::from_millis(watch_interval_ms.unwrap_or(1000));
    let mut iterations = 0usize;
    loop {
        let output = build_workspace_output(cli)?;
        render_workspace(cli, &output);
        iterations += 1;
        if watch_iterations.is_some_and(|limit| iterations >= limit) {
            break;
        }
        sleep(interval).await;
    }
    Ok(())
}
