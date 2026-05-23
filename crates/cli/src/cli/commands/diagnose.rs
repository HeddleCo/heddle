// SPDX-License-Identifier: Apache-2.0
//! Diagnose command.

use std::time::Instant;

use anyhow::Result;
use chrono::Utc;
use objects::object::Tree;
use repo::{
    GitRemoteTrackingStatus, Repository, RepositoryOperationStatus, Thread, ThreadFreshness,
    ThreadImpactCategory, ThreadMode, ThreadState, describe_thread_advice_with_initial,
    is_synthetic_root,
};
use serde::Serialize;

use super::{
    git_overlay_health::{
        GitOverlayHealth, GitOverlayHealthCheck, RepositoryTrustState, build_git_overlay_health,
        build_plain_git_trust_probe, build_repository_trust_state,
    },
    operator_loop::primary_next_action,
    thread::{CoordinationStatus, ThreadActorInfo, ThreadSummary, collect_thread_summaries},
};
use crate::cli::{Cli, DiagnoseArgs, should_output_json, style, worktree_status_options};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DiagnoseOutput {
    repository: String,
    repository_capability: String,
    storage_model: String,
    hosted_enabled: bool,
    git_overlay_import_hint: Option<DiagnoseGitOverlayImportHintOutput>,
    git_overlay_health: GitOverlayHealth,
    trust: RepositoryTrustState,
    operation: Option<RepositoryOperationStatus>,
    remote_tracking: Option<GitRemoteTrackingStatus>,
    thread: Option<DiagnoseThreadOutput>,
    state: Option<DiagnoseStateOutput>,
    changes: DiagnoseChangesOutput,
    workspace: DiagnoseWorkspaceOutput,
    health: DiagnoseHealthOutput,
    profile: Option<DiagnoseProfileOutput>,
}

#[derive(Debug, Clone, Serialize)]
struct DiagnoseThreadOutput {
    name: String,
    visibility: String,
    coordination_status: CoordinationStatus,
    is_isolated: bool,
    mode: Option<ThreadMode>,
    state: Option<ThreadState>,
    freshness: Option<ThreadFreshness>,
    path: Option<String>,
    execution_path: Option<String>,
    target_thread: Option<String>,
    parent_thread: Option<String>,
    child_threads: Vec<String>,
    task: Option<String>,
    actor: Option<ThreadActorInfo>,
    session_id: Option<String>,
    heddle_session_id: Option<String>,
    harness: Option<String>,
    changed_path_count: usize,
    impact_categories: Vec<ThreadImpactCategory>,
    heavy_impact_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DiagnoseStateOutput {
    change_id: String,
    tree: String,
    intent: Option<String>,
    git_checkpoint: Option<DiagnoseCheckpointOutput>,
}

#[derive(Debug, Clone, Serialize)]
struct DiagnoseCheckpointOutput {
    git_commit: String,
    committed_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct DiagnoseChangesOutput {
    modified: Vec<String>,
    added: Vec<String>,
    deleted: Vec<String>,
    total: usize,
}

#[derive(Debug, Clone, Serialize)]
struct DiagnoseWorkspaceOutput {
    thread_count: usize,
    parallel_count: usize,
    ready_count: usize,
    blocked_count: usize,
    active_actor_count: usize,
}

#[derive(Debug, Clone, Serialize)]
struct DiagnoseHealthOutput {
    status: String,
    blockers: Vec<String>,
    recommended_action: String,
}

#[derive(Debug, Clone, Serialize)]
struct DiagnoseGitOverlayImportHintOutput {
    current_branch: String,
    missing_branch_count: usize,
    missing_branches: Vec<String>,
    recommended_command: String,
}

#[derive(Debug, Clone, Serialize)]
struct DiagnoseProfileOutput {
    repo_open_ms: u128,
    current_state_ms: u128,
    worktree_status_ms: u128,
    thread_summary_ms: u128,
    total_ms: u128,
}

pub fn cmd_diagnose(cli: &Cli, args: DiagnoseArgs) -> Result<()> {
    let output = build_diagnose_output(cli, args.profile)?;
    render_diagnose(cli, &output);
    Ok(())
}

fn build_plain_git_diagnose_output(cli: &Cli) -> Result<Option<DiagnoseOutput>> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    let Some(probe) = build_plain_git_trust_probe(start)? else {
        return Ok(None);
    };
    let changes = DiagnoseChangesOutput {
        modified: probe
            .changes
            .modified
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        added: probe
            .changes
            .added
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        deleted: probe
            .changes
            .deleted
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        total: probe.changes.change_count(),
    };
    let import_hint = probe
        .git_branch
        .as_ref()
        .map(|branch| DiagnoseGitOverlayImportHintOutput {
            current_branch: branch.clone(),
            missing_branch_count: 1,
            missing_branches: vec![branch.clone()],
            recommended_command: format!("heddle bridge git import --ref {branch}"),
        });
    let trust = probe.trust.clone();
    let git_overlay_health = GitOverlayHealth {
        status: trust.status.clone(),
        clean: trust.trusted,
        summary: trust.summary.clone(),
        recovery_commands: trust.recovery_commands.clone(),
        checks: trust
            .checks
            .iter()
            .map(|check| GitOverlayHealthCheck {
                name: check.name.clone(),
                status: check.status.clone(),
                summary: check.summary.clone(),
            })
            .collect(),
    };
    Ok(Some(DiagnoseOutput {
        repository: probe.root.display().to_string(),
        repository_capability: "plain-git".to_string(),
        storage_model: "git-only".to_string(),
        hosted_enabled: false,
        git_overlay_import_hint: import_hint,
        git_overlay_health,
        trust: trust.clone(),
        operation: None,
        remote_tracking: None,
        thread: None,
        state: None,
        changes,
        workspace: DiagnoseWorkspaceOutput {
            thread_count: 0,
            parallel_count: 0,
            ready_count: 0,
            blocked_count: 0,
            active_actor_count: 0,
        },
        health: DiagnoseHealthOutput {
            status: trust.status,
            blockers: vec![trust.summary],
            recommended_action: trust.recommended_action,
        },
        profile: None,
    }))
}

pub(crate) fn build_diagnose_output(cli: &Cli, include_profile: bool) -> Result<DiagnoseOutput> {
    if let Some(output) = build_plain_git_diagnose_output(cli)? {
        return Ok(output);
    }
    let total_start = Instant::now();

    let repo_open_start = Instant::now();
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let repo_open_ms = repo_open_start.elapsed().as_millis();

    let current_state_start = Instant::now();
    let current_state = repo.current_state()?;
    let operation = repo.operation_status()?;
    let remote_tracking = repo.git_remote_tracking_status()?;
    let import_hint = repo.git_overlay_import_hint()?;
    let git_overlay_health = build_git_overlay_health(&repo);
    let trust = build_repository_trust_state(&repo);
    let current_state_ms = current_state_start.elapsed().as_millis();

    let status_start = Instant::now();
    let status_options = worktree_status_options(Some(repo.config()));
    let status = if let Some(state) = current_state.as_ref() {
        let tree = repo.require_tree(&state.tree)?;
        repo.compare_worktree_cached_with_options(&tree, &status_options)?
    } else if let Some(status) = repo.git_overlay_worktree_status()? {
        status
    } else {
        let tree = Tree::new();
        repo.compare_worktree_cached_with_options(&tree, &status_options)?
    };
    let changes = DiagnoseChangesOutput {
        modified: status
            .modified
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        added: status
            .added
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        deleted: status
            .deleted
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        total: status.modified.len() + status.added.len() + status.deleted.len(),
    };
    let worktree_status_ms = status_start.elapsed().as_millis();

    let thread_summary_start = Instant::now();
    let summaries = collect_thread_summaries(&repo)?;
    let attached_thread = repo.current_lane()?;
    let current_summary = summaries
        .iter()
        .find(|summary| summary.is_current)
        .or_else(|| {
            attached_thread
                .as_deref()
                .and_then(|thread| summaries.iter().find(|summary| summary.name == thread))
        });
    let thread_summary_ms = thread_summary_start.elapsed().as_millis();

    let initial_state = current_state
        .as_ref()
        .map(is_synthetic_root)
        .unwrap_or(true);
    let mut health = diagnose_health(&repo, current_summary, changes.total > 0, initial_state);
    if !git_overlay_health.clean && operation.is_none() {
        health.status = git_overlay_health.status.clone();
        health.recommended_action = git_overlay_health
            .primary_recovery_command()
            .unwrap_or("heddle doctor")
            .to_string();
        if health.blockers.is_empty() {
            health.blockers.push(git_overlay_health.summary.clone());
        }
    }
    let workspace = diagnose_workspace(&summaries);
    let thread = current_summary.map(diagnose_thread);
    let state = current_state.as_ref().map(|state| DiagnoseStateOutput {
        change_id: state.change_id.short(),
        tree: state.tree.short(),
        intent: state.intent.clone(),
        git_checkpoint: repo
            .latest_git_checkpoint_for_change(&state.change_id)
            .ok()
            .flatten()
            .map(|record| DiagnoseCheckpointOutput {
                git_commit: record.git_commit,
                committed_at: record.committed_at,
            }),
    });

    let profile = include_profile.then(|| DiagnoseProfileOutput {
        repo_open_ms,
        current_state_ms,
        worktree_status_ms,
        thread_summary_ms,
        total_ms: total_start.elapsed().as_millis(),
    });

    Ok(DiagnoseOutput {
        repository: repo.root().display().to_string(),
        repository_capability: repo.capability_label().to_string(),
        storage_model: repo.storage_model_label().to_string(),
        hosted_enabled: repo.hosted_enabled(),
        git_overlay_import_hint: import_hint.clone().map(|hint| {
            DiagnoseGitOverlayImportHintOutput {
                current_branch: hint.current_branch,
                missing_branch_count: hint.missing_branch_count,
                missing_branches: hint.missing_branches,
                recommended_command: hint.recommended_command,
            }
        }),
        git_overlay_health,
        trust,
        operation: operation.clone(),
        remote_tracking: remote_tracking.clone(),
        thread,
        state,
        changes,
        workspace,
        health: DiagnoseHealthOutput {
            recommended_action: if health.status == "clean" || operation.is_some() {
                primary_next_action(
                    operation.as_ref(),
                    remote_tracking.as_ref(),
                    import_hint.as_ref(),
                    Some(&health.recommended_action),
                )
            } else {
                health.recommended_action.clone()
            },
            ..health
        },
        profile,
    })
}

fn diagnose_thread(summary: &ThreadSummary) -> DiagnoseThreadOutput {
    DiagnoseThreadOutput {
        name: summary.name.clone(),
        visibility: summary.visibility.clone(),
        coordination_status: summary.coordination_status.clone(),
        is_isolated: summary.is_isolated,
        mode: summary.thread_mode.clone(),
        state: summary.thread_state.clone(),
        freshness: summary.freshness.clone(),
        path: summary.path.clone(),
        execution_path: summary.execution_path.clone(),
        target_thread: summary.target_thread.clone(),
        parent_thread: summary.parent_thread.clone(),
        child_threads: summary.child_threads.clone(),
        task: summary.task.clone(),
        actor: summary.actor.clone(),
        session_id: summary.session_id.clone(),
        heddle_session_id: summary.heddle_session_id.clone(),
        harness: summary.harness.clone(),
        changed_path_count: summary.changed_paths.len(),
        impact_categories: summary.impact_categories.clone(),
        heavy_impact_paths: summary.heavy_impact_paths.clone(),
    }
}

fn diagnose_workspace(summaries: &[ThreadSummary]) -> DiagnoseWorkspaceOutput {
    DiagnoseWorkspaceOutput {
        thread_count: summaries.len(),
        parallel_count: summaries
            .iter()
            .filter(|summary| !summary.is_current)
            .count(),
        ready_count: summaries
            .iter()
            .filter(|summary| {
                summary.thread_state == Some(ThreadState::Ready)
                    || summary.coordination_status == CoordinationStatus::MergeReady
            })
            .count(),
        blocked_count: summaries
            .iter()
            .filter(|summary| {
                summary.thread_health == "blocked"
                    || !summary.blockers.is_empty()
                    || matches!(
                        summary.coordination_status,
                        CoordinationStatus::Blocked | CoordinationStatus::Diverged
                    )
            })
            .count(),
        active_actor_count: summaries
            .iter()
            .filter(|summary| summary.actor.is_some())
            .count(),
    }
}

fn diagnose_health(
    repo: &Repository,
    current_summary: Option<&ThreadSummary>,
    worktree_dirty: bool,
    initial_state: bool,
) -> DiagnoseHealthOutput {
    let Some(summary) = current_summary else {
        return DiagnoseHealthOutput {
            status: if worktree_dirty && initial_state {
                "uncaptured"
            } else if worktree_dirty {
                "dirty_worktree"
            } else {
                "detached"
            }
            .to_string(),
            blockers: Vec::new(),
            recommended_action: if worktree_dirty {
                "heddle capture".to_string()
            } else {
                String::new()
            },
        };
    };

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
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| repo.root().to_path_buf()),
        materialized_path: summary.path.as_ref().map(std::path::PathBuf::from),
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
        auto: false,
        shared_target_dir: None,
    };
    let advice =
        describe_thread_advice_with_initial(&thread, worktree_dirty, 0, false, initial_state);
    DiagnoseHealthOutput {
        status: advice.thread_health,
        blockers: advice.blockers,
        recommended_action: advice.recommended_action,
    }
}

fn render_diagnose(cli: &Cli, output: &DiagnoseOutput) {
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::to_string(output).expect("diagnose JSON serializes")
        );
        return;
    }

    println!(
        "{} {}",
        style::bold("Doctor"),
        style::dim(&output.repository)
    );
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
    }
    if let Some(remote_tracking) = &output.remote_tracking {
        println!("Sync: {}", remote_tracking.message);
    }
    if let Some(hint) = &output.git_overlay_import_hint {
        println!(
            "Git overlay: {} branch tip(s) available for import ({})",
            hint.missing_branch_count,
            crate::cli::render::preview_list(&hint.missing_branches, hint.missing_branch_count,)
        );
    }
    if !output.git_overlay_health.clean {
        println!("Git overlay health: {}", output.git_overlay_health.summary);
    }
    if let Some(thread) = &output.thread {
        println!(
            "Thread: {} [{} · {}]",
            thread.name, thread.visibility, thread.coordination_status
        );
        if let Some(path) = thread.path.as_ref().or(thread.execution_path.as_ref()) {
            println!("Execution root: {path}");
        }
        if let Some(actor) = &thread.actor
            && let Some(text) =
                crate::cli::render::actor_display(actor.provider.as_deref(), actor.model.as_deref())
        {
            println!("Actor: {text}");
        }
        // Agent-machinery (session / harness IDs) behind `-v` — these
        // are load-bearing for orchestrators reading JSON but pure
        // noise for humans running `heddle diagnose`.
        if cli.verbose > 0 {
            if let Some(session_id) = &thread.session_id {
                println!("Session: {session_id}");
            }
            if let Some(heddle_session_id) = &thread.heddle_session_id {
                println!("Heddle session: {heddle_session_id}");
            }
            if let Some(harness) = &thread.harness {
                println!("Harness: {harness}");
            }
        }
    } else {
        println!("Thread: detached");
    }

    if let Some(state) = &output.state {
        println!("State: {} ({})", state.change_id, state.tree);
        if let Some(intent) = &state.intent {
            println!("Intent: \"{intent}\"");
        }
        if let Some(checkpoint) = &state.git_checkpoint {
            println!(
                "Git checkpoint: {} ({})",
                &checkpoint.git_commit[..std::cmp::min(12, checkpoint.git_commit.len())],
                checkpoint.committed_at
            );
        } else if cli.verbose > 0 {
            // Same restraint rule as status/show: "Capture durability:
            // local only" is the default; the absence of a Git checkpoint
            // line above already encodes it.
            println!("Capture durability: local only");
        }
    } else {
        println!("State: (initial)");
    }

    println!(
        "Changes: {} modified, {} added, {} deleted",
        output.changes.modified.len(),
        output.changes.added.len(),
        output.changes.deleted.len()
    );
    if output.changes.total > 0 {
        println!("Changed paths: {}", changed_path_preview(&output.changes));
    }

    println!(
        "Workspace: {} thread(s), {} parallel, {} ready, {} blocked, {} actor(s)",
        output.workspace.thread_count,
        output.workspace.parallel_count,
        output.workspace.ready_count,
        output.workspace.blocked_count,
        output.workspace.active_actor_count
    );
    println!("Health: {}", style::bold(&output.health.status));
    if !output.health.blockers.is_empty() {
        println!("Blocked by: {}", output.health.blockers.join(" | "));
    }
    if !output.health.recommended_action.is_empty() {
        println!(
            "Next step: {}",
            style::bold(&output.health.recommended_action)
        );
    }
    if let Some(profile) = &output.profile {
        println!(
            "Profile: repo_open={}ms current_state={}ms worktree_status={}ms thread_summary={}ms total={}ms",
            profile.repo_open_ms,
            profile.current_state_ms,
            profile.worktree_status_ms,
            profile.thread_summary_ms,
            profile.total_ms
        );
    }
}

fn changed_path_preview(changes: &DiagnoseChangesOutput) -> String {
    let mut paths = changes
        .modified
        .iter()
        .chain(changes.added.iter())
        .chain(changes.deleted.iter())
        .take(5)
        .cloned()
        .collect::<Vec<_>>();
    if changes.total > paths.len() {
        paths.push(format!("+{} more", changes.total - paths.len()));
    }
    paths.join(", ")
}
