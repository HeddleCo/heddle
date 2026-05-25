// SPDX-License-Identifier: Apache-2.0
//! Status command.

use std::{
    collections::BTreeSet,
    io::{self, IsTerminal, Write},
    time::Instant,
};

use anyhow::Result;
#[cfg(feature = "client")]
use futures::{SinkExt, StreamExt};
use objects::worktree::WorktreeStatus;
use repo::{
    AgentUsageSummary, GitRemoteTrackingStatus, Repository, RepositoryOperationStatus, Thread,
    ThreadFreshness, ThreadImpactCategory, ThreadMode, ThreadState, WorktreeCompareProfile,
    describe_thread_advice_with_initial, is_synthetic_root,
};
#[cfg(feature = "client")]
use serde::Deserialize;
use serde::Serialize;
use tokio::time::{Duration, sleep};
#[cfg(feature = "client")]
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::header::AUTHORIZATION, protocol::Message},
};
use tracing::debug;

use super::{
    action_line::print_command,
    command_catalog::ActionFields,
    git_compat::{GitIndexPlan, git_index_plan_for_repo, git_index_plan_for_root},
    git_overlay_health::{
        GitOverlayHealth, RepositoryVerificationState, build_git_overlay_health,
        build_plain_git_verification_probe, command_argvs, override_trust_recommended_action,
        repository_setup_guidance, serialize_empty_action_as_null,
    },
    operator_loop::primary_next_action_with_verification,
    snapshot::resolve_principal,
    thread::{
        CoordinationStatus, collect_thread_summaries, contextual_thread_action,
        current_thread_next_action_with_verification, find_thread_summary_single,
        thread_recovery_action_is_primary,
    },
};
use crate::{
    bridge::git_core::principal_is_default_unknown,
    cli::{Cli, should_output_json, style, worktree_status_options},
    config::UserConfig,
    perf::{ProfileField, emit_profile, profile_enabled},
};

#[derive(Serialize)]
pub(crate) struct StatusOutput {
    output_kind: &'static str,
    repository_capability: String,
    repository_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_context: Option<crate::cli::render::RepositoryContextInfo>,
    storage_model: String,
    hosted_enabled: bool,
    #[serde(skip)]
    render_json: bool,
    operation: Option<RepositoryOperationStatus>,
    remote_tracking: Option<GitRemoteTrackingStatus>,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
    git_index: Option<GitIndexPlan>,
    /// Carried for the human-readable renderer only. Not part of the
    /// JSON contract: import-hint information is exposed via
    /// `heddle bridge git status --output json` instead, which is the
    /// command whose subject is the bridge.
    #[serde(skip)]
    git_overlay_import_hint: Option<GitOverlayImportHintOutput>,
    git_overlay_health: GitOverlayHealth,
    thread: Option<String>,
    base_state: Option<String>,
    base_root: Option<String>,
    current_state: Option<String>,
    path: Option<String>,
    execution_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    heddle_session_id: Option<String>,
    actor: Option<ActorInfo>,
    harness: Option<String>,
    thinking_level: Option<String>,
    usage_summary: Option<AgentUsageSummary>,
    last_progress_at: Option<String>,
    report_flush_state: Option<String>,
    attach_reason: Option<String>,
    thread_mode: Option<ThreadMode>,
    thread_state: Option<ThreadState>,
    freshness: Option<ThreadFreshness>,
    target_thread: Option<String>,
    parent_thread: Option<String>,
    child_threads: Vec<String>,
    task: Option<String>,
    promotion_suggested: bool,
    impact_categories: Vec<ThreadImpactCategory>,
    heavy_impact_paths: Vec<String>,
    changed_path_count: usize,
    blockers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity_notice: Option<String>,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    recommended_action: String,
    recommended_action_argv: Option<Vec<String>>,
    recommended_action_template: Option<super::command_catalog::ActionTemplate>,
    recovery_commands: Vec<String>,
    recovery_command_argv: Vec<Vec<String>>,
    recovery_action_templates: Vec<super::command_catalog::ActionTemplate>,
    thread_health: String,
    coordination_status: CoordinationStatus,
    is_isolated: bool,
    parallel_threads: Vec<ParallelThreadInfo>,
    state: Option<StateInfo>,
    git_checkpoint: Option<GitCheckpointInfo>,
    changes: ChangesInfo,
    /// Inventory of clonefile-backed thread worktrees discovered on
    /// disk. Read-only diagnostic. Included in JSON always (as `[]`
    /// when no threads are materialized — the JSON contract is "the
    /// field is always an array", which lets consumers index into
    /// it without a null-guard) and in verbose text; the default
    /// short text only prints a one-line advisory when at least one
    /// thread is stale (manifest's recorded state lags the thread's
    /// actual head), because that's the case where the user may
    /// want to act (re-materialize or re-capture). Healthy
    /// materialized threads stay invisible.
    #[serde(default)]
    materialized_threads: Vec<MaterializedThreadInfo>,
}

#[derive(Serialize, Default)]
struct MaterializedThreadInfo {
    name: String,
    state_id: String,
    tree_hash_short: String,
    file_count: usize,
    /// `true` when the thread ref has advanced beyond what the
    /// manifest recorded. The on-disk worktree may not reflect the
    /// thread's latest content; users may want `heddle thread
    /// switch <name>` to re-materialize or `heddle capture` to
    /// fast-forward the manifest.
    stale: bool,
}

#[derive(Serialize)]
struct ActorInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Serialize)]
struct ParallelThreadInfo {
    name: String,
    coordination_status: CoordinationStatus,
    current_state: Option<String>,
}

#[derive(Serialize)]
struct StateInfo {
    change_id: String,
    content_hash: String,
    intent: Option<String>,
}

#[derive(Serialize)]
struct GitCheckpointInfo {
    git_commit: String,
    committed_at: String,
}

#[derive(Serialize, Default)]
struct ChangesInfo {
    modified: Vec<String>,
    added: Vec<String>,
    deleted: Vec<String>,
}

#[derive(Serialize)]
struct GitOverlayImportHintOutput {
    current_branch: String,
    missing_branch_count: usize,
    missing_branches: Vec<String>,
    recommended_command: String,
}

#[derive(Serialize)]
struct PlainGitStatusOutput {
    output_kind: &'static str,
    repository_capability: String,
    repository_label: String,
    storage_model: String,
    heddle_initialized: bool,
    git_branch: Option<String>,
    path: String,
    git_overlay_health: GitOverlayHealth,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
    recommended_action: String,
    recommended_action_argv: Option<Vec<String>>,
    recommended_action_template: Option<super::command_catalog::ActionTemplate>,
    recovery_commands: Vec<String>,
    recovery_command_argv: Vec<Vec<String>>,
    recovery_action_templates: Vec<super::command_catalog::ActionTemplate>,
    thread_health: String,
    changed_path_count: usize,
    changes: ChangesInfo,
    git_index: Option<GitIndexPlan>,
}

fn changes_from_status(status: &WorktreeStatus) -> ChangesInfo {
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

fn emit_status_worktree_profile(profile: Option<&WorktreeCompareProfile>) {
    let Some(profile) = profile else {
        return;
    };
    emit_profile(
        "status worktree",
        &[
            ProfileField::millis("index_load_ms", profile.index_load_ms),
            ProfileField::millis("index_snapshot_load_ms", profile.index_snapshot_load_ms),
            ProfileField::millis("index_journal_replay_ms", profile.index_journal_replay_ms),
            ProfileField::millis("monitor_prepare_ms", profile.monitor_prepare_ms),
            ProfileField::millis("compare_ms", profile.compare_ms),
            ProfileField::millis("tracked_refresh_ms", profile.tracked_refresh_ms),
            ProfileField::millis("untracked_scan_ms", profile.untracked_scan_ms),
            ProfileField::millis("hashing_ms", profile.hashing_ms),
            ProfileField::millis(
                "directory_cache_compare_ms",
                profile.directory_cache_compare_ms,
            ),
            ProfileField::millis("index_save_ms", profile.index_save_ms),
            ProfileField::millis("monitor_persist_ms", profile.monitor_persist_ms),
            ProfileField::millis("untracked_flatten_ms", profile.untracked_flatten_ms),
            ProfileField::count(
                "untracked_flattened_paths",
                profile.untracked_flattened_paths as u128,
            ),
            ProfileField::count("directories_scanned", profile.directories_scanned as u128),
            ProfileField::count("directories_skipped", profile.directories_skipped as u128),
            ProfileField::count("files_hashed", profile.files_hashed as u128),
            ProfileField::count("cache_hits", profile.cache_hits as u128),
            ProfileField::count(
                "monitor_changed_paths",
                profile.monitor_changed_paths as u128,
            ),
            ProfileField::count(
                "monitor_skipped_directories",
                profile.monitor_skipped_directories as u128,
            ),
        ],
    );
}

pub async fn cmd_status(
    cli: &Cli,
    short: bool,
    watch: bool,
    watch_iterations: Option<usize>,
    watch_interval_ms: Option<u64>,
) -> Result<()> {
    if watch {
        return watch_status(cli, short, watch_iterations, watch_interval_ms).await;
    }
    if let Some(output) = build_plain_git_status_probe(cli)? {
        render_plain_git_status(cli, &output, short)?;
        return Ok(());
    }
    let output = build_status_output(cli, short)?;
    render_status(cli, &output, short)?;
    Ok(())
}

fn build_plain_git_status_probe(cli: &Cli) -> Result<Option<PlainGitStatusOutput>> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    let Some(probe) = build_plain_git_verification_probe(start)? else {
        return Ok(None);
    };
    let changes = changes_from_status(&probe.changes);
    let changed_path_count = probe.changes.change_count();
    let git_overlay_health = GitOverlayHealth {
        status: probe.trust.status.clone(),
        clean: probe.trust.verified,
        summary: probe.trust.summary.clone(),
        recovery_commands: probe.trust.recovery_commands.clone(),
        checks: probe
            .trust
            .checks
            .iter()
            .map(|check| super::git_overlay_health::GitOverlayHealthCheck {
                name: check.name.clone(),
                status: check.status.clone(),
                summary: check.summary.clone(),
                details: check.details.clone(),
            })
            .collect(),
    };
    let trust = probe.trust;
    let git_index = git_index_plan_for_root(&probe.root)?;
    Ok(Some(PlainGitStatusOutput {
        output_kind: "status",
        repository_capability: "plain-git".to_string(),
        repository_label: crate::cli::render::repository_mode_label("plain-git", "git-only"),
        storage_model: "git-only".to_string(),
        heddle_initialized: false,
        git_branch: probe.git_branch,
        path: probe.root.display().to_string(),
        recommended_action: trust.recommended_action.clone(),
        recommended_action_argv: trust.recommended_action_argv.clone(),
        recommended_action_template: trust.recommended_action_template.clone(),
        recovery_commands: trust.recovery_commands.clone(),
        recovery_command_argv: trust.recovery_command_argv.clone(),
        recovery_action_templates: trust.recovery_action_templates.clone(),
        thread_health: trust.status.clone(),
        changed_path_count,
        changes,
        git_index,
        git_overlay_health,
        trust,
    }))
}

fn render_plain_git_status(cli: &Cli, output: &PlainGitStatusOutput, short: bool) -> Result<()> {
    if should_output_json(cli, None) {
        crate::cli::render::write_json_stdout(output)?;
        return Ok(());
    }
    if short {
        render_short_plain_git_status(output);
        return Ok(());
    }
    println!("{}", style::bold("Heddle status"));
    println!("Repository: {}", output.repository_label);
    if let Some(branch) = &output.git_branch {
        println!("Git branch: {}", style::bold(branch));
    }
    println!(
        "Health: {}",
        style::thread_state(&human_thread_health(&output.thread_health))
    );
    println!(
        "Heddle setup: {}",
        style::warn("not set up for this Git repo yet")
    );
    if let Some(setup) = repository_setup_guidance(&output.trust) {
        println!("Setup needed: {}", style::warn(&setup.setup_line));
        println!("{}", style::dim(&setup.effect));
    }
    println!();
    println!(
        "Changed paths: {}",
        style::bold(&output.changed_path_count.to_string())
    );
    if output.changed_path_count > 0 {
        render_status_changes_plain(&output.changes);
    } else {
        println!("{}", style::dim("Git worktree clean"));
    }
    println!();
    println!("{}", style::bold("Next"));
    print_command(&output.recommended_action);
    Ok(())
}

pub(crate) fn build_status_output(cli: &Cli, short: bool) -> Result<StatusOutput> {
    let repo_open_start = Instant::now();
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let repo_open_ms = repo_open_start.elapsed().as_millis();
    let body_start = Instant::now();
    let as_json = should_output_json(cli, Some(repo.config()));
    let short_text = short && !as_json;

    let current_state_start = Instant::now();
    let current_state = repo.current_state()?;
    let current_state_ms = current_state_start.elapsed().as_millis();

    let operation_start = Instant::now();
    let operation = repo.operation_status()?;
    let operation_ms = operation_start.elapsed().as_millis();
    // Single gating predicate for the slower "walk every thread /
    // inspect remote tracking / populate cross-thread relations" path.
    // JSON and `-v` text actually display the data; default text doesn't.
    let needs_full_walk = cli.verbose > 0 || as_json;
    let needs_remote_tracking = needs_full_walk || short_text;
    let remote_tracking_start = Instant::now();
    let remote_tracking = if needs_remote_tracking {
        repo.git_remote_tracking_status().unwrap_or(None)
    } else {
        None
    };
    let remote_tracking_ms = remote_tracking_start.elapsed().as_millis();

    let import_hint_start = Instant::now();
    let import_hint = if short_text {
        None
    } else {
        repo.git_overlay_import_hint().unwrap_or(None)
    };
    let import_hint_ms = import_hint_start.elapsed().as_millis();
    let git_overlay_health = build_git_overlay_health(&repo);
    let trust = RepositoryVerificationState::from_health(&repo, git_overlay_health.clone());
    let status_options = worktree_status_options(Some(repo.config()));
    let git_worktree_status = repo.git_overlay_worktree_status().unwrap_or(None);
    let git_index = git_index_plan_for_repo(&repo)?;
    let identity_notice = first_capture_identity_notice(&repo, current_state.as_ref())?;
    let git_clean_mapping_blocker = matches!(
        trust.status.as_str(),
        "needs_import" | "needs_reconcile" | "git_branch_advanced"
    ) && git_worktree_status
        .as_ref()
        .is_some_and(WorktreeStatus::is_clean);

    // Get worktree status
    let worktree_status_start = Instant::now();
    let (changes, worktree_profile) = if git_clean_mapping_blocker {
        (ChangesInfo::default(), None)
    } else if let Some(status) = git_worktree_status.as_ref()
        && !status.is_clean()
        && trust.status != "needs_checkpoint"
    {
        (changes_from_status(status), None)
    } else if let Some(ref state) = current_state {
        let tree = repo.require_tree(&state.tree)?;
        let (status, profile) =
            repo.compare_worktree_cached_profiled_with_options(&tree, &status_options)?;
        (changes_from_status(&status), Some(profile))
    } else if let Some(status) = git_worktree_status {
        (changes_from_status(&status), None)
    } else {
        let tree = objects::object::Tree::new();
        let (status, profile) =
            repo.compare_worktree_cached_profiled_with_options(&tree, &status_options)?;
        let mut changes = changes_from_status(&status);
        changes.modified.clear();
        changes.deleted.clear();
        (changes, Some(profile))
    };
    let worktree_status_ms = worktree_status_start.elapsed().as_millis();

    if short_text {
        let recommended_action = if trust.verified {
            primary_next_action_with_verification(
                operation.as_ref(),
                remote_tracking.as_ref(),
                None,
                None,
                &trust,
            )
        } else {
            trust.recommended_action.clone()
        };
        debug!(
            repo_open_ms,
            body_ms = body_start.elapsed().as_millis(),
            total_ms = repo_open_ms + body_start.elapsed().as_millis(),
            "Status command complete"
        );
        if profile_enabled() {
            emit_status_worktree_profile(worktree_profile.as_ref());
            emit_profile(
                "status phases",
                &[
                    ProfileField::millis("repo_open_ms", repo_open_ms),
                    ProfileField::millis("current_state_ms", current_state_ms),
                    ProfileField::millis("operation_ms", operation_ms),
                    ProfileField::millis("remote_tracking_ms", remote_tracking_ms),
                    ProfileField::millis("import_hint_ms", import_hint_ms),
                    ProfileField::millis("worktree_status_ms", worktree_status_ms),
                    ProfileField::duration("build_total_ms", body_start.elapsed()),
                ],
            );
        }
        let presentation = crate::cli::render::repository_presentation(&repo, None, None);
        let recommended_action_fields = ActionFields::from_action(&recommended_action);
        return Ok(StatusOutput {
            output_kind: "status",
            repository_capability: repo.capability_label().to_string(),
            repository_label: presentation.label,
            repository_context: presentation.context,
            storage_model: repo.storage_model_label().to_string(),
            hosted_enabled: repo.hosted_enabled(),
            render_json: as_json,
            git_overlay_import_hint: import_hint.clone().map(|hint| GitOverlayImportHintOutput {
                current_branch: hint.current_branch,
                missing_branch_count: hint.missing_branch_count,
                missing_branches: hint.missing_branches,
                recommended_command: hint.recommended_command,
            }),
            git_overlay_health,
            trust: trust.clone(),
            operation,
            remote_tracking,
            git_index,
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
            changed_path_count: 0,
            blockers: if trust.verified {
                Vec::new()
            } else {
                trust
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
            identity_notice: identity_notice.clone(),
            recommended_action_argv: recommended_action_fields.argv,
            recommended_action_template: recommended_action_fields.template,
            recommended_action,
            recovery_commands: trust.recovery_commands.clone(),
            recovery_command_argv: command_argvs(&trust.recovery_commands),
            recovery_action_templates: trust.recovery_action_templates.clone(),
            thread_health: trust.status.clone(),
            coordination_status: if trust.verified {
                CoordinationStatus::Clean
            } else {
                CoordinationStatus::Blocked
            },
            is_isolated: false,
            parallel_threads: Vec::new(),
            state: None,
            git_checkpoint: None,
            changes,
            materialized_threads: Vec::new(),
        });
    }

    let thread_summary_start = Instant::now();
    let track_name = repo.current_lane()?;
    // Use the fast single-thread path when default text won't display
    // the child/sibling fields anyway. JSON and -v go through the full
    // walk (`find_thread_summary` → `collect_thread_summaries`) which
    // populates the cross-thread relationships. Saves ~45ms on the
    // common path.
    let full_thread_summaries = if needs_full_walk {
        Some(collect_thread_summaries(&repo)?)
    } else {
        None
    };
    let thread_summary = match (track_name.as_deref(), full_thread_summaries.as_ref()) {
        (Some(thread), Some(summaries)) => summaries
            .iter()
            .find(|summary| summary.name == thread)
            .cloned(),
        (Some(thread), None) => find_thread_summary_single(&repo, thread)?,
        (None, _) => None,
    };
    let thread_summary_ms = thread_summary_start.elapsed().as_millis();
    // `collect_thread_summaries` walks every thread record in the repo
    // (60ms on a 69-thread sibling worktree). The result is then filtered
    // to threads that are Ahead/Blocked/Diverged/MergeReady — frequently
    // an empty list, so the work is wasted. Skip the walk when the
    // default text renderer will discard the field anyway. JSON and -v
    // still pay the cost because they actually display it.
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
    let materialized_threads = assess_materialized_threads(&repo);
    let materialized_ms = materialized_start.elapsed().as_millis();
    let target_thread = thread_summary
        .as_ref()
        .and_then(|thread| thread.target_thread.clone());
    let parent_thread = thread_summary
        .as_ref()
        .and_then(|thread| thread.parent_thread.clone());
    let presentation = crate::cli::render::repository_presentation(
        &repo,
        target_thread.as_deref(),
        parent_thread.as_deref(),
    );

    let output = StatusOutput {
        output_kind: "status",
        repository_capability: repo.capability_label().to_string(),
        repository_label: presentation.label,
        repository_context: presentation.context,
        storage_model: repo.storage_model_label().to_string(),
        hosted_enabled: repo.hosted_enabled(),
        render_json: as_json,
        git_overlay_import_hint: import_hint.clone().map(|hint| GitOverlayImportHintOutput {
            current_branch: hint.current_branch,
            missing_branch_count: hint.missing_branch_count,
            missing_branches: hint.missing_branches,
            recommended_command: hint.recommended_command,
        }),
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
        actor: thread_summary.as_ref().and_then(|thread| {
            thread.actor.as_ref().map(|actor| ActorInfo {
                provider: actor.provider.clone(),
                model: actor.model.clone(),
            })
        }),
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
        changed_path_count: thread_summary
            .as_ref()
            .map(|thread| thread.changed_paths.len())
            .unwrap_or_default(),
        blockers: Vec::new(),
        identity_notice,
        recommended_action: String::new(),
        recommended_action_argv: None,
        recommended_action_template: None,
        recovery_commands: trust.recovery_commands.clone(),
        recovery_command_argv: command_argvs(&trust.recovery_commands),
        recovery_action_templates: trust.recovery_action_templates.clone(),
        thread_health: "clean".to_string(),
        coordination_status: thread_summary
            .as_ref()
            .map(|thread| thread.coordination_status.clone())
            .unwrap_or(CoordinationStatus::Clean),
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
    };
    let late_state_ms = late_state_start.elapsed().as_millis();
    let advice_start = Instant::now();
    let has_changes = !output.changes.modified.is_empty()
        || !output.changes.added.is_empty()
        || !output.changes.deleted.is_empty();
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
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| repo.root().to_path_buf()),
        materialized_path: output.path.as_ref().map(std::path::PathBuf::from),
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
    let initial_state = current_state
        .as_ref()
        .map(is_synthetic_root)
        .unwrap_or(true);
    let advice = thread_stub.as_ref().map(|thread| {
        describe_thread_advice_with_initial(thread, has_changes, 0, false, initial_state)
    });
    let mut trust = output.trust.clone();
    if let Some(thread) = output.thread.as_deref()
        && !trust.recommended_action.is_empty()
    {
        let contextual = contextual_thread_action(
            &repo,
            thread,
            output.target_thread.as_deref(),
            &trust.recommended_action,
        );
        if contextual != trust.recommended_action {
            override_trust_recommended_action(&mut trust, contextual);
        }
    }
    let thread_health = advice.as_ref().map(|advice| advice.thread_health.as_str());
    let thread_action = advice
        .as_ref()
        .map(|advice| advice.recommended_action.as_str());
    let recommended_action = current_thread_next_action_with_verification(
        output.operation.as_ref(),
        output.remote_tracking.as_ref(),
        import_hint.as_ref(),
        thread_health,
        thread_action,
        &trust,
    );
    let recommended_action = if let Some(thread) = output.thread.as_deref() {
        contextual_thread_action(
            &repo,
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
        override_trust_recommended_action(&mut trust, recommended_action.clone());
    }
    let recommended_action_fields = ActionFields::from_action(&recommended_action);
    let thread_health = if trust.verified {
        advice
            .as_ref()
            .map(|advice| advice.thread_health.clone())
            .unwrap_or_else(|| "clean".to_string())
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
                && !(check.name == "Clone" && check.status == "blocked")
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
    let output = StatusOutput {
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
        recommended_action_argv: recommended_action_fields.argv,
        recommended_action_template: recommended_action_fields.template,
        recovery_commands: trust.recovery_commands.clone(),
        recovery_command_argv: command_argvs(&trust.recovery_commands),
        recovery_action_templates: trust.recovery_action_templates.clone(),
        thread_health,
        coordination_status: if blocked_by_trust && !needs_checkpoint {
            CoordinationStatus::Blocked
        } else {
            output.coordination_status
        },
        thread_state: if blocked_by_trust && !needs_checkpoint && output.thread_state.is_some() {
            Some(ThreadState::Blocked)
        } else {
            output.thread_state
        },
        changed_path_count: if trust.verified {
            changed_path_count(thread_summary.as_ref(), &output.changes)
        } else {
            changes_path_count(&output.changes)
        },
        trust,
        ..output
    };
    let advice_ms = advice_start.elapsed().as_millis();

    debug!(
        repo_open_ms,
        body_ms = body_start.elapsed().as_millis(),
        total_ms = repo_open_ms + body_start.elapsed().as_millis(),
        "Status command complete"
    );

    if profile_enabled() {
        emit_status_worktree_profile(worktree_profile.as_ref());
        emit_profile(
            "status phases",
            &[
                ProfileField::millis("repo_open_ms", repo_open_ms),
                ProfileField::millis("current_state_ms", current_state_ms),
                ProfileField::millis("operation_ms", operation_ms),
                ProfileField::millis("remote_tracking_ms", remote_tracking_ms),
                ProfileField::millis("import_hint_ms", import_hint_ms),
                ProfileField::millis("worktree_status_ms", worktree_status_ms),
                ProfileField::millis("thread_summary_ms", thread_summary_ms),
                ProfileField::millis("parallel_threads_ms", parallel_threads_ms),
                ProfileField::millis("late_state_ms", late_state_ms),
                ProfileField::millis("materialized_threads_ms", materialized_ms),
                ProfileField::millis("advice_ms", advice_ms),
                ProfileField::duration("build_total_ms", body_start.elapsed()),
            ],
        );
    }

    Ok(output)
}

pub(crate) fn render_status(cli: &Cli, output: &StatusOutput, short: bool) -> Result<()> {
    let render_start = Instant::now();
    if output.render_json {
        crate::cli::render::write_json_stdout(output)?;
    } else if short {
        render_short_status(output);
    } else {
        render_long_status(output, cli.verbose > 0);
    }
    if profile_enabled() {
        emit_profile(
            "status render",
            &[ProfileField::duration("render_ms", render_start.elapsed())],
        );
    }
    Ok(())
}

async fn watch_status(
    cli: &Cli,
    short: bool,
    watch_iterations: Option<usize>,
    watch_interval_ms: Option<u64>,
) -> Result<()> {
    let interval = Duration::from_millis(watch_interval_ms.unwrap_or(1000));
    let mut iterations = 0usize;

    #[cfg(feature = "client")]
    let mut hosted_watch = HostedPresenceWatch::connect_if_configured(cli).await;

    loop {
        let output = build_status_output(cli, short)?;
        let redraw = watch_iterations.is_none() && io::stdout().is_terminal();
        if !output.render_json && redraw {
            print!("\x1B[2J\x1B[H");
            println!(
                "{}",
                style::dim(&format!(
                    "Watching status · refreshed {} · Ctrl-C to stop",
                    chrono::Local::now().format("%H:%M:%S")
                ))
            );
            io::stdout().flush().ok();
        } else if !output.render_json && watch_iterations.is_some() {
            println!(
                "{}",
                style::dim(&format!(
                    "Status snapshot {} of {} · refreshed {}",
                    iterations + 1,
                    watch_iterations.unwrap_or_default(),
                    chrono::Local::now().format("%H:%M:%S")
                ))
            );
        }
        render_status(cli, &output, short)?;
        iterations += 1;
        if watch_iterations.is_some_and(|limit| iterations >= limit) {
            break;
        }

        #[cfg(feature = "client")]
        if let Some(watch) = hosted_watch.as_mut() {
            watch.wait_for_event(interval).await;
            continue;
        }

        sleep(interval).await;
    }

    Ok(())
}

fn render_short_changes(changes: &ChangesInfo) {
    // `git status -s` palette: M=warn (yellow-ish), A=accent (green),
    // D=error (red). The two-character column is the entire signal,
    // so we accept a small amount of saturation here — it's the one
    // column where color is the cheapest read.
    for path in &changes.modified {
        println!("{}  {}", style::warn("M"), path);
    }
    for path in &changes.added {
        println!("{}  {}", style::accent("A"), path);
    }
    for path in &changes.deleted {
        println!("{}  {}", style::error("D"), path);
    }
}

fn render_short_status(output: &StatusOutput) {
    render_short_changes(&output.changes);
    if output.changes.is_empty() {
        println!(
            "{} {}",
            style::bold(short_status_subject(output)),
            style::thread_state(&short_status_health(output))
        );
    }
    render_materialized_advisory(output);
}

fn short_status_health(output: &StatusOutput) -> String {
    if output.recommended_action == "heddle push" && output.thread_health == "clean" {
        "ready to push".to_string()
    } else {
        human_thread_health(&output.thread_health)
    }
}

fn short_status_subject(output: &StatusOutput) -> &str {
    output
        .thread
        .as_deref()
        .or_else(|| output.current_state.as_ref().map(|_| "detached"))
        .unwrap_or("repository")
}

fn render_short_plain_git_status(output: &PlainGitStatusOutput) {
    render_short_changes(&output.changes);
    if output.changes.is_empty() {
        println!(
            "{} {}",
            style::bold(output.git_branch.as_deref().unwrap_or("detached")),
            style::thread_state(&human_thread_health(&output.thread_health))
        );
    }
}

fn render_status_changes_plain(changes: &ChangesInfo) {
    println!("{}", style::bold("Git changes"));
    for path in &changes.modified {
        println!("  {}: {}", style::warn("modified"), path);
    }
    for path in &changes.added {
        println!("  {}:    {}", style::accent("added"), path);
    }
    for path in &changes.deleted {
        println!("  {}:  {}", style::error("deleted"), path);
    }
}

impl ChangesInfo {
    fn is_empty(&self) -> bool {
        self.modified.is_empty() && self.added.is_empty() && self.deleted.is_empty()
    }
}

/// Default short-text advisory for materialized threads. Stays silent
/// unless at least one thread is stale — the user's bar for this
/// surface is "say something only when I might need to act". When
/// stale threads exist, emit a single dim line naming them so the
/// user can `thread switch` (re-materialize) or `capture` (move the
/// manifest's recorded state forward) at their leisure.
fn render_materialized_advisory(output: &StatusOutput) {
    let stale: Vec<&str> = output
        .materialized_threads
        .iter()
        .filter(|t| t.stale)
        .map(|t| t.name.as_str())
        .collect();
    if stale.is_empty() {
        return;
    }
    println!(
        "{} materialized thread(s) lag their head: {}",
        style::dim("·"),
        stale.join(", ")
    );
}

fn render_long_status(output: &StatusOutput, verbose: bool) {
    render_status_header(output);
    render_status_operation(output);
    render_status_thread(output, verbose);
    render_status_details(output, verbose);
    render_status_advice(output);
    render_status_changes(output);
    render_status_parallel(output);
    render_status_materialized(&output.materialized_threads, verbose);
}

/// Long-form inventory of clonefile-backed materialized threads. The
/// default long output keeps it tight — one line per stale thread,
/// silent when everything's in sync — so it has the same "no news is
/// good news" shape as the short renderer's advisory. `-v` widens to
/// the full list with file counts and tree hashes, on the principle
/// that verbose callers want the diagnostic surface even when nothing
/// is wrong.
fn render_status_materialized(threads: &[MaterializedThreadInfo], verbose: bool) {
    if threads.is_empty() {
        return;
    }
    if !verbose {
        let stale: Vec<&MaterializedThreadInfo> = threads.iter().filter(|t| t.stale).collect();
        if stale.is_empty() {
            return;
        }
        println!();
        println!("{}", style::bold("Materialized threads (stale)"));
        for t in stale {
            println!("  {} {}", style::bold(&t.name), style::warn("stale"));
        }
        return;
    }
    println!();
    println!("{}", style::bold("Materialized threads"));
    for t in threads {
        let status_tag = if t.stale {
            style::warn("stale")
        } else {
            style::dim("current")
        };
        println!(
            "  {} {} {} files={} {}",
            style::bold(&t.name),
            style::dim(&t.state_id),
            style::dim(&t.tree_hash_short),
            t.file_count,
            status_tag,
        );
    }
}

fn render_status_header(output: &StatusOutput) {
    println!(
        "{} {} {}",
        style::bold("Heddle status"),
        style::dim("for"),
        output
            .thread
            .as_ref()
            .map(|thread| style::bold(thread))
            .unwrap_or_else(|| style::warn("detached HEAD"))
    );
    println!("Repository: {}", output.repository_label);
    if output.hosted_enabled {
        println!("Hosted: {}", style::accent("enabled"));
    }
}

fn render_status_operation(output: &StatusOutput) {
    if let Some(operation) = &output.operation {
        println!(
            "In progress: {} {} {}",
            style::warn(&operation.scope.to_string()),
            style::warn(&operation.kind.to_string()),
            style::dim(&format!("({})", operation.state))
        );
    }
    if let Some(remote_tracking) = &output.remote_tracking {
        if remote_tracking.upstream.is_empty() {
            println!(
                "Remote publication: {}",
                style::accent(&remote_tracking.message)
            );
        } else if remote_tracking.behind == 0 && remote_tracking.ahead > 0 {
            println!("Remote sync: {}", style::accent(&remote_tracking.message));
        } else {
            println!("Remote drift: {}", style::warn(&remote_tracking.message));
        }
    }
    if let Some(hint) = &output.git_overlay_import_hint {
        if !hint
            .missing_branches
            .iter()
            .any(|branch| branch == &hint.current_branch)
        {
            println!(
                "{}",
                crate::cli::render::git_only_branch_summary(
                    &hint.missing_branches,
                    hint.missing_branch_count,
                )
            );
        }
    }
    if !output.git_overlay_health.clean {
        let label = if matches!(
            output.git_overlay_health.status.as_str(),
            "needs_init" | "needs_import"
        ) {
            "Setup needed"
        } else {
            "Verification"
        };
        if let Some(setup) = git_setup_line(output) {
            println!("{label}: {}", style::warn(&setup));
        } else {
            println!(
                "{label}: {}",
                style::warn(&output.git_overlay_health.summary)
            );
        }
        if output.git_overlay_health.status == "needs_import"
            && output.changed_path_count == 0
            && !has_status_changes(output)
        {
            println!(
                "Git worktree: {}",
                style::accent(
                    "clean; .heddle metadata is present, adoption imports Git history, and the Git worktree stays clean"
                )
            );
        }
    }
}

fn render_status_thread(output: &StatusOutput, verbose: bool) {
    println!();
    if let Some(thread) = &output.thread {
        // Thread name is a primary identifier for the user's
        // current focus — bold it so it reads as the page header.
        println!("Thread: {}", style::bold(thread));
    } else {
        println!("HEAD detached");
    }
    // Health text is short ("clean" / "blocked" / etc.) — colour it
    // so a glance tells you the state without reading the word.
    println!(
        "Health: {}",
        style::thread_state(&human_thread_health(&output.thread_health))
    );
    println!("Coordination: {}", human_coordination_status(output));
    if verbose && let Some(base) = &output.base_state {
        println!("Base: {}", style::dim(base));
    }
    if verbose
        && let Some(base_root) = &output.base_root
        && !base_root.is_empty()
    {
        println!("Base tree: {}", style::dim(base_root));
    }

    if let Some(state) = &output.state {
        if verbose {
            println!(
                "State: {} ({})",
                style::change_id(&state.change_id),
                style::dim(&state.content_hash)
            );
        } else {
            println!("Saved change: {}", style::change_id(&state.change_id));
        }
        if let Some(intent) = &state.intent {
            // Quote stays plain; the inner intent string is the
            // editorial line, so it's bolded.
            if verbose {
                println!("Intent: \"{}\"", style::bold(intent));
            } else {
                println!("Change message: {}", style::bold(intent));
            }
        }
        if verbose && let Some(checkpoint) = &output.git_checkpoint {
            println!(
                "Git checkpoint: {} ({})",
                style::dim(
                    &checkpoint.git_commit[..std::cmp::min(12, checkpoint.git_commit.len())]
                ),
                style::dim(&checkpoint.committed_at)
            );
        } else if output.git_checkpoint.is_some() {
            println!("Git: {}", style::accent("saved to commit"));
        } else if verbose {
            // The fallback "Capture durability: local only" repeats on
            // every status the user runs against a non-checkpointed
            // state. Useful diagnostic on demand, noisy by default —
            // a present `Git checkpoint:` line already tells the user
            // when durability has been promoted.
            println!("Capture durability: {}", style::dim("local only"));
        }
    } else {
        println!("State: {}", style::dim("(initial)"));
    }
}

fn render_status_details(output: &StatusOutput, verbose: bool) {
    let mut emitted = false;
    if let Some(path) = &output.path {
        if !emitted {
            println!();
            println!("{}", style::dim("Worktree"));
            emitted = true;
        }
        println!("Path: {}", path);
    } else if let Some(path) = &output.execution_path {
        if !emitted {
            println!();
            println!("{}", style::dim("Worktree"));
            emitted = true;
        }
        println!("Execution root: {}", path);
    }
    if let Some(mode) = &output.thread_mode {
        if !emitted {
            println!();
            println!("{}", style::dim("Worktree"));
            emitted = true;
        }
        println!("Checkout: {}", status_workspace_label(output, mode));
    }
    if let Some(state) = &output.thread_state {
        if !emitted {
            println!();
            println!("{}", style::dim("Worktree"));
            emitted = true;
        }
        println!(
            "Lifecycle: {}",
            style::thread_state(&human_thread_state(output, state))
        );
    }
    if let Some(freshness) = &output.freshness
        && *freshness != ThreadFreshness::Unknown
    {
        if !emitted {
            println!();
            println!("{}", style::dim("Worktree"));
            emitted = true;
        }
        // Freshness shares the thread-state palette so `current` and
        // `stale` carry the same semantics as `active` and `blocked`
        // would on the thread itself.
        println!("Sync: {}", style::thread_state(&freshness.to_string()));
    }
    if let Some(context) = &output.repository_context
        && let Some(parent_repository) = &context.parent_repository
    {
        if !emitted {
            println!();
            println!("{}", style::dim("Worktree"));
            emitted = true;
        }
        println!("Parent repo: {}", parent_repository);
        if context.kind == "git-overlay-isolated-checkout" {
            println!(
                "Git checkout: {}",
                style::dim("no .git here; raw Git commands belong in the parent repo")
            );
        }
    }
    if let Some(target) = &output.target_thread {
        if !emitted {
            println!();
            println!("{}", style::dim("Worktree"));
            emitted = true;
        }
        println!("Target thread: {}", target);
    }
    if let Some(parent) = &output.parent_thread {
        if !emitted {
            println!();
            println!("{}", style::dim("Worktree"));
            emitted = true;
        }
        println!("Parent thread: {}", parent);
    }
    if !output.child_threads.is_empty() {
        if !emitted {
            println!();
            println!("{}", style::dim("Worktree"));
            emitted = true;
        }
        println!("Child threads: {}", output.child_threads.join(", "));
    }
    if verbose
        && let Some(actor) = &output.actor
        && let Some(text) =
            crate::cli::render::actor_display(actor.provider.as_deref(), actor.model.as_deref())
    {
        if !emitted {
            println!();
            println!("{}", style::dim("Worktree"));
            emitted = true;
        }
        println!("Actor: {text}");
    }
    // The next block is agent-machinery: session IDs, harness name,
    // thinking level, last-progress timestamp, report-flush state, and
    // the reattach reason. It's load-bearing for orchestrators reading
    // JSON output (which is unaffected) but pure noise on the default
    // human-facing text surface — a typical session emits 5-7 lines of
    // it before the user sees their actual changed paths. Hide behind
    // `-v`; everything here is still in `--output json` and
    // `heddle diagnose -v`.
    if verbose {
        if let Some(session_id) = &output.session_id {
            if !emitted {
                println!();
                println!("{}", style::dim("Worktree"));
                emitted = true;
            }
            println!("Session: {}", session_id);
        }
        if let Some(heddle_session_id) = &output.heddle_session_id {
            if !emitted {
                println!();
                println!("{}", style::dim("Worktree"));
                emitted = true;
            }
            println!("Heddle session: {}", heddle_session_id);
        }
        if let Some(harness) = &output.harness {
            if !emitted {
                println!();
                println!("{}", style::dim("Worktree"));
                emitted = true;
            }
            println!("Harness: {}", harness);
        }
        if let Some(thinking_level) = &output.thinking_level {
            if !emitted {
                println!();
                println!("{}", style::dim("Worktree"));
                emitted = true;
            }
            println!("Thinking: {}", thinking_level);
        }
        if let Some(last_progress_at) = &output.last_progress_at {
            if !emitted {
                println!();
                println!("{}", style::dim("Worktree"));
                emitted = true;
            }
            println!("Last progress: {}", last_progress_at);
        }
        if let Some(report_flush_state) = &output.report_flush_state {
            if !emitted {
                println!();
                println!("{}", style::dim("Worktree"));
                emitted = true;
            }
            println!("Report flush: {}", report_flush_state);
        }
        if let Some(attach_reason) = &output.attach_reason {
            if !emitted {
                println!();
                println!("{}", style::dim("Worktree"));
                emitted = true;
            }
            println!("Attach: {}", attach_reason);
        }
    }
    if verbose && let Some(usage_summary) = &output.usage_summary {
        let mut parts = Vec::new();
        if let Some(input) = usage_summary.input_tokens {
            parts.push(format!("input {}", input));
        }
        if let Some(output_tokens) = usage_summary.output_tokens {
            parts.push(format!("output {}", output_tokens));
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
            if !emitted {
                println!();
                println!("{}", style::dim("Worktree"));
            }
            println!("Usage: {}", parts.join(" · "));
        }
    }
}

fn status_workspace_label(output: &StatusOutput, mode: &ThreadMode) -> &'static str {
    if output
        .repository_context
        .as_ref()
        .is_some_and(|context| context.kind == "git-overlay-isolated-checkout")
    {
        return "Git-overlay isolated checkout";
    }
    match mode {
        ThreadMode::Materialized if output.repository_capability == "git-overlay" => {
            "Git branch checkout"
        }
        ThreadMode::Materialized => "main checkout",
        ThreadMode::Solid => "isolated checkout",
        ThreadMode::Virtualized => "virtual checkout",
    }
}

fn render_status_advice(output: &StatusOutput) {
    println!();
    if let Some(notice) = &output.identity_notice {
        println!("Identity: {}", style::warn(notice));
    }
    if !output.parallel_threads.is_empty() {
        println!(
            "Parallel work: {}",
            style::bold(&output.parallel_threads.len().to_string())
        );
    }
    if let Some(task) = &output.task {
        println!("Task: {}", task);
    }
    let checkpoint_needed = output.thread_health == "needs_checkpoint";
    if checkpoint_needed {
        println!(
            "Git checkpoint pending: {}",
            style::bold("saved Heddle state is not yet a Git commit")
        );
    } else if matches!(output.thread_state, Some(ThreadState::Ready)) {
        println!(
            "Thread changes vs target: {}",
            style::bold(&output.changed_path_count.to_string())
        );
    } else {
        println!(
            "Changed paths: {}",
            style::bold(&output.changed_path_count.to_string())
        );
    }
    if output.promotion_suggested && !output.heavy_impact_paths.is_empty() {
        println!(
            "Heavy-impact change: {} — review broader impact before merging",
            crate::cli::render::preview_list(
                &output.heavy_impact_paths,
                output.heavy_impact_paths.len(),
            )
        );
    }
    if !output.impact_categories.is_empty() {
        println!(
            "Impact categories: {}",
            output
                .impact_categories
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !output.blockers.is_empty() {
        if checkpoint_needed {
            println!("{}", style::bold("Saved in Heddle"));
        } else if local_work_in_progress(output) {
            println!("{}", style::bold("Work in progress"));
        } else {
            println!("{}", style::warn("Blocked by"));
        }
        for blocker in &output.blockers {
            let blocker = if checkpoint_needed {
                checkpoint_blocker_text(blocker)
            } else {
                human_status_blocker_text(blocker)
            };
            if checkpoint_needed || local_work_in_progress(output) {
                println!("  - {}", style::dim(&blocker));
            } else {
                println!("  - {}", style::warn(&blocker));
            }
        }
    }
    if !output.recommended_action.is_empty() {
        println!();
        println!("{}", style::bold("Next"));
        print_command(&output.recommended_action);
        println!("  why: {}", status_next_reason(output));
        if let Some(after) = status_next_follow_up(output) {
            println!("  then: {}", style::dim(after));
        }
    }
}

fn status_next_reason(output: &StatusOutput) -> &'static str {
    if output.operation.is_some() {
        return "an operation is in progress; finish or abort it before starting another workflow";
    }
    if output.recommended_action.contains("checkpoint") {
        return "the work is saved in Heddle; checkpoint writes the Git commit for this saved state";
    }
    if output.recommended_action.contains("adopt --ref")
        || output.git_overlay_import_hint.as_ref().is_some_and(|hint| {
            hint.missing_branches
                .iter()
                .any(|branch| branch == &hint.current_branch)
        })
    {
        return "connect this Git branch to Heddle before using history-oriented commands";
    }
    if output.changed_path_count > 0 && output.recommended_action.contains("commit") {
        return "there are uncommitted worktree changes; commit captures them and writes the Git checkpoint";
    }
    if output.changed_path_count > 0 && output.recommended_action.contains("capture") {
        return "there are uncaptured worktree changes; capture records a recoverable state";
    }
    if !output.blockers.is_empty() {
        return "the current thread has blockers that must be cleared before integration";
    }
    if let Some(remote_tracking) = &output.remote_tracking {
        if remote_tracking.behind == 0 && remote_tracking.ahead > 0 {
            return "local commits are safe and waiting to be pushed upstream";
        }
        return "remote tracking reports drift; sync that before integration";
    }
    if output.recommended_action.contains("ready") {
        return "the work is captured; readiness checks merge blockers without landing changes";
    }
    if output.recommended_action.contains("merge") {
        return "the thread is ready to integrate into its target";
    }
    "this is the safest command for the current repository and thread state"
}

fn status_next_follow_up(output: &StatusOutput) -> Option<&'static str> {
    let action = output.recommended_action.as_str();
    if action.contains("commit") && status_has_publish_target(output) {
        Some("run `heddle push` when the checkpoint is ready to publish")
    } else if action.contains("checkpoint") && status_has_publish_target(output) {
        Some("run `heddle push` when the Git checkpoint is ready to publish")
    } else if action.contains("capture") {
        Some("run `heddle ready` when the captured work should be checked for merge")
    } else if action.contains("ready") {
        Some("preview integration with `heddle merge <thread> --preview` before landing it")
    } else if action.contains("merge") && action.contains("--preview") {
        Some(
            "land the previewed thread with `heddle ship --thread <thread> --no-push`; add `--push` only when a remote is configured",
        )
    } else if action.contains("resolve") || action.contains("continue") || action.contains("abort")
    {
        Some("check `heddle status` again after the operation state changes")
    } else {
        None
    }
}

fn status_has_publish_target(output: &StatusOutput) -> bool {
    output.remote_tracking.is_some() || output.trust.default_remote.is_some()
}

fn checkpoint_blocker_text(blocker: &str) -> String {
    blocker
        .strip_prefix("Worktree: ")
        .unwrap_or(blocker)
        .replace(
            "captured in Heddle but not checkpointed to Git",
            "saved in Heddle and ready to checkpoint to Git",
        )
}

fn human_status_blocker_text(blocker: &str) -> String {
    if let Some(summary) = blocker
        .strip_prefix("Mapping: ")
        .or_else(|| blocker.strip_prefix("Heddle: "))
        .or_else(|| blocker.strip_prefix("Verification: "))
    {
        if summary.contains("reconcile") || summary.contains("Git branch") {
            return format!("Git/Heddle mismatch: {summary}");
        }
        return format!(
            "Setup needed: {}",
            summary
                .replace("still need Heddle import", "need Heddle setup")
                .replace(
                    "import this branch tip before comparing Heddle state",
                    "connect this branch before using Heddle history"
                )
        );
    }
    blocker.to_string()
}

fn git_setup_line(output: &StatusOutput) -> Option<String> {
    repository_setup_guidance(&output.trust).map(|setup| setup.setup_line)
}

/// Walk `<heddle_dir>/threads/` and decorate each manifest with a
/// staleness bit by comparing the manifest's recorded `state_id` to
/// the current thread head in the ref backend.
///
/// Best-effort and read-only. Errors from `list_thread_manifests` or
/// `get_thread` collapse to "nothing to advise" — the renderer must
/// not nag the user about a probe that didn't run cleanly. The cost
/// here is one `read_dir` + one `read_to_string` per thread, plus one
/// ref lookup; well within `status`'s existing budget.
fn assess_materialized_threads(repo: &Repository) -> Vec<MaterializedThreadInfo> {
    let summaries = match repo::thread_manifest::list_thread_manifests(repo.heddle_dir()) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    summaries
        .into_iter()
        .map(|summary| {
            let stale = match repo.refs().get_thread(&summary.thread) {
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

fn first_capture_identity_notice(
    repo: &Repository,
    current_state: Option<&objects::object::State>,
) -> Result<Option<String>> {
    if !current_state.map(is_synthetic_root).unwrap_or(true) {
        return Ok(None);
    }
    let user_config = UserConfig::load_default().unwrap_or_default();
    let principal = resolve_principal(repo, &user_config)?;
    if principal_is_default_unknown(&principal) {
        return Ok(Some(
            "no principal configured; the first capture/checkpoint would use Unknown <unknown@example.com>. Set HEDDLE_PRINCIPAL_NAME and HEDDLE_PRINCIPAL_EMAIL or run `heddle init --principal-name <name> --principal-email <email>`.".to_string(),
        ));
    }
    Ok(None)
}

fn changed_path_count(
    thread: Option<&super::thread::ThreadSummary>,
    changes: &ChangesInfo,
) -> usize {
    let mut paths = BTreeSet::new();
    if let Some(thread) = thread {
        paths.extend(thread.changed_paths.iter().cloned());
    }
    paths.extend(changes.modified.iter().cloned());
    paths.extend(changes.added.iter().cloned());
    paths.extend(changes.deleted.iter().cloned());
    paths.len()
}

fn changes_path_count(changes: &ChangesInfo) -> usize {
    let mut paths = BTreeSet::new();
    paths.extend(changes.modified.iter().cloned());
    paths.extend(changes.added.iter().cloned());
    paths.extend(changes.deleted.iter().cloned());
    paths.len()
}

fn render_status_changes(output: &StatusOutput) {
    // Changes
    let has_changes = has_status_changes(output);

    println!();
    if let Some(index) = output.git_index.as_ref()
        && git_index_has_paths(index)
    {
        render_git_index_status(index);
        return;
    }
    if has_changes {
        println!("{}", style::bold("Changes not yet saved"));
        for path in &output.changes.modified {
            println!("  {}: {}", style::warn("modified"), path);
        }
        for path in &output.changes.added {
            println!("  {}:    {}", style::accent("added"), path);
        }
        for path in &output.changes.deleted {
            println!("  {}:  {}", style::error("deleted"), path);
        }
    } else if output.trust.verified {
        println!("{}", style::dim("No unsaved changes, worktree clean"));
    } else if output.trust.worktree_state == "not_checked" {
        let message = if output.trust.status == "git_branch_advanced" {
            "No unsaved worktree changes detected; import the external Git branch tip before comparing Heddle state"
        } else {
            "No unsaved worktree changes detected; finish setup before comparing Heddle state"
        };
        println!("{}", style::dim(message));
    } else if output.trust.worktree_state == "clean" {
        println!(
            "{}",
            style::dim(&format!(
                "No unsaved worktree changes detected; repository verification is {}",
                output.trust.status
            ))
        );
    } else {
        println!("{}", style::dim("No unsaved worktree changes detected"));
    }
}

fn git_index_has_paths(index: &GitIndexPlan) -> bool {
    !index.staged_paths.is_empty()
        || !index.unstaged_paths.is_empty()
        || !index.untracked_paths.is_empty()
}

fn render_git_index_status(index: &GitIndexPlan) {
    println!("{}", style::bold("Git index and worktree"));
    if !index.staged_paths.is_empty() {
        println!("  will commit staged paths:");
        for path in &index.staged_paths {
            println!("    {}", path);
        }
    }
    if !index.unstaged_paths.is_empty() {
        println!("  {}:", git_index_extra_path_label(index, "unstaged"));
        for path in &index.unstaged_paths {
            println!("    {}", path);
        }
    }
    if !index.untracked_paths.is_empty() {
        println!("  {}:", git_index_extra_path_label(index, "untracked"));
        for path in &index.untracked_paths {
            println!("    {}", path);
        }
    }
    println!("  commit scope: {}", git_index_commit_scope_text(index));
    if index.commit_mode == "staged_index" && !index.preserved_after_commit.is_empty() {
        println!(
            "  include the rest with: {}",
            style::bold("heddle commit --all -m \"...\"")
        );
    }
}

fn git_index_extra_path_label(index: &GitIndexPlan, kind: &'static str) -> String {
    if index.commit_mode == "staged_index" {
        format!("will leave {kind} paths")
    } else {
        format!("will commit {kind} paths")
    }
}

fn git_index_commit_scope_text(index: &GitIndexPlan) -> &'static str {
    match index.commit_mode {
        "staged_index" => {
            "plain `heddle commit` checkpoints staged paths only; unstaged and untracked paths stay put"
        }
        "worktree_all" => "plain `heddle commit` checkpoints all unstaged and untracked paths",
        "worktree_all_explicit" => {
            "`heddle commit --all` checkpoints staged, unstaged, and untracked paths"
        }
        "none" => "no Git paths are ready to commit",
        _ => "plain `heddle commit` checkpoints the listed paths",
    }
}

fn human_thread_health(status: &str) -> String {
    match status {
        "needs_init" => "setup needed".to_string(),
        "needs_import" => "setup needed".to_string(),
        "git_branch_advanced" => "Git branch advanced outside Heddle".to_string(),
        "needs_reconcile" => "Git/Heddle mismatch".to_string(),
        "needs_checkpoint" => "checkpoint needed".to_string(),
        "dirty_worktree" | "uncaptured" => "work in progress".to_string(),
        other => other.to_string(),
    }
}

fn human_coordination_status(output: &StatusOutput) -> String {
    if local_work_in_progress(output)
        && matches!(output.coordination_status, CoordinationStatus::Blocked)
    {
        "work in progress".to_string()
    } else {
        output.coordination_status.to_string()
    }
}

fn human_thread_state(output: &StatusOutput, state: &ThreadState) -> String {
    if local_work_in_progress(output) && matches!(state, ThreadState::Blocked) {
        "active".to_string()
    } else {
        state.to_string()
    }
}

fn local_work_in_progress(output: &StatusOutput) -> bool {
    matches!(
        output.thread_health.as_str(),
        "dirty_worktree" | "uncaptured"
    )
}

fn has_status_changes(output: &StatusOutput) -> bool {
    !output.changes.modified.is_empty()
        || !output.changes.added.is_empty()
        || !output.changes.deleted.is_empty()
}

fn render_status_parallel(output: &StatusOutput) {
    if !output.parallel_threads.is_empty() {
        println!();
        println!("{}", style::bold("Other active threads"));
        for thread in &output.parallel_threads {
            let state = thread.current_state.as_deref().unwrap_or("(no state)");
            println!(
                "  {} {} {}",
                style::bold(&thread.name),
                style::dim(state),
                style::thread_state(&thread.coordination_status.to_string())
            );
        }
    }
}

#[cfg(feature = "client")]
struct HostedPresenceWatch {
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

#[cfg(feature = "client")]
impl HostedPresenceWatch {
    async fn connect_if_configured(cli: &Cli) -> Option<Self> {
        let cwd = std::env::current_dir().ok()?;
        let repo = Repository::open(cli.repo.as_ref().unwrap_or(&cwd)).ok()?;
        let upstream = repo.config().hosted.upstream_url.as_deref()?.trim();
        let namespace = repo.config().hosted.namespace.as_deref()?.trim();
        if upstream.is_empty() || namespace.is_empty() {
            return None;
        }

        let token = UserConfig::load_default().ok()?.remote_token()?;
        let mut request = normalize_presence_ws_url(upstream)
            .ok()?
            .into_client_request()
            .ok()?;
        let auth = format!("Bearer {}", token.id);
        request
            .headers_mut()
            .insert(AUTHORIZATION, auth.parse().ok()?);
        let (mut stream, _) = connect_async(request).await.ok()?;
        let hello = serde_json::to_string(&PresenceClientFrame::Hello {
            role: "browser",
            subscribe: vec![namespace.to_string()],
        })
        .ok()?;
        stream.send(Message::Text(hello)).await.ok()?;
        Some(Self { stream })
    }

    async fn wait_for_event(&mut self, timeout: Duration) {
        let delay = sleep(timeout);
        tokio::pin!(delay);
        loop {
            tokio::select! {
                _ = &mut delay => return,
                frame = self.stream.next() => {
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<PresenceServerFrame>(&text) {
                                Ok(PresenceServerFrame::Ready) => continue,
                                Ok(PresenceServerFrame::Event)
                                | Ok(PresenceServerFrame::Error) => return,
                                Err(_) => return,
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = self.stream.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(_)) => return,
                        Some(Err(_)) | None => return,
                    }
                }
            }
        }
    }
}

#[cfg(feature = "client")]
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PresenceClientFrame<'a> {
    Hello {
        role: &'a str,
        subscribe: Vec<String>,
    },
}

#[cfg(feature = "client")]
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PresenceServerFrame {
    Ready,
    Event,
    Error,
}

#[cfg(feature = "client")]
fn normalize_presence_ws_url(upstream: &str) -> Result<String> {
    let trimmed = upstream.trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("https://") {
        return Ok(format!(
            "wss://{}/presence/ws",
            rest.split('/').next().unwrap_or(rest)
        ));
    }
    if let Some(rest) = trimmed.strip_prefix("http://") {
        return Ok(format!(
            "ws://{}/presence/ws",
            rest.split('/').next().unwrap_or(rest)
        ));
    }
    if trimmed.starts_with("wss://") || trimmed.starts_with("ws://") {
        let scheme = if trimmed.starts_with("wss://") {
            "wss://"
        } else {
            "ws://"
        };
        let rest = trimmed
            .trim_start_matches("wss://")
            .trim_start_matches("ws://");
        return Ok(format!(
            "{scheme}{}/presence/ws",
            rest.split('/').next().unwrap_or(rest)
        ));
    }
    Err(anyhow::anyhow!(
        "unsupported hosted upstream url: {upstream}"
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use repo::Repository;
    use tempfile::TempDir;

    use super::{MaterializedThreadInfo, assess_materialized_threads, render_status_materialized};

    fn init_repo_with_materialized_thread(content: &[u8]) -> (TempDir, TempDir, Repository) {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("hello.txt"), content).unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        repo.materialize_thread("main", &dest).unwrap();
        (repo_dir, dest_holder, repo)
    }

    #[test]
    fn assess_returns_empty_when_no_materialized_threads() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init_default(dir.path()).unwrap();
        assert!(assess_materialized_threads(&repo).is_empty());
    }

    #[test]
    fn assess_reports_fresh_materialization_as_not_stale() {
        let (_repo_dir, _dest_holder, repo) = init_repo_with_materialized_thread(b"hello\n");
        let infos = assess_materialized_threads(&repo);
        assert_eq!(infos.len(), 1, "exactly one materialized thread");
        let info = &infos[0];
        assert_eq!(info.name, "main");
        assert_eq!(info.file_count, 1);
        assert!(!info.stale, "thread head unchanged → not stale");
        assert!(!info.state_id.is_empty());
        assert!(!info.tree_hash_short.is_empty());
        assert!(
            info.tree_hash_short.len() <= 12,
            "tree_hash_short caps at 12 chars: got {}",
            info.tree_hash_short
        );
    }

    #[test]
    fn assess_flags_thread_as_stale_when_head_advances_past_manifest() {
        // Setup: materialize "main" at a path *separate from*
        // `repo.root()`. After the post-bugfix snapshot path-gate
        // landed (manifest is only refreshed when `self.root` matches
        // the manifest's recorded worktree_path), running
        // `repo.snapshot()` from the main repo dir advances the
        // thread head WITHOUT auto-healing the manifest. The
        // staleness check should then surface the materialized
        // worktree as stale, which is the user-facing signal
        // `heddle status` exists to deliver.
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("hello.txt"), b"hello\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        let mat = repo.materialize_thread("main", &dest).unwrap();

        // Advance main from the main repo dir (not from dest).
        fs::write(repo_dir.path().join("hello.txt"), b"hello world\n").unwrap();
        let snap = repo.snapshot(Some("advance".into()), None).unwrap();
        assert_ne!(snap.change_id, mat.state_id);

        let infos = assess_materialized_threads(&repo);
        assert_eq!(infos.len(), 1);
        assert!(
            infos[0].stale,
            "manifest still names mat.state_id but main head is at snap.change_id → stale"
        );
    }

    #[test]
    fn render_status_materialized_skips_when_inventory_is_empty() {
        // Renderer is `println!`-based; we can't capture stdout from
        // a unit test, but we *can* assert the early-return path
        // doesn't panic on an empty inventory or a non-verbose call
        // with only fresh threads.
        let empty: Vec<MaterializedThreadInfo> = Vec::new();
        render_status_materialized(&empty, false);
        render_status_materialized(&empty, true);
    }

    #[test]
    fn render_status_materialized_handles_mixed_stale_and_fresh() {
        let threads = vec![
            MaterializedThreadInfo {
                name: "fresh".into(),
                state_id: "abcd".into(),
                tree_hash_short: "1234".into(),
                file_count: 3,
                stale: false,
            },
            MaterializedThreadInfo {
                name: "stale".into(),
                state_id: "efgh".into(),
                tree_hash_short: "5678".into(),
                file_count: 7,
                stale: true,
            },
        ];
        // Short-form: only stale threads listed.
        render_status_materialized(&threads, false);
        // Long-form: every thread listed.
        render_status_materialized(&threads, true);

        // Short-form with only fresh threads: silent (no panic).
        let fresh_only: Vec<MaterializedThreadInfo> = vec![MaterializedThreadInfo {
            name: "fresh".into(),
            state_id: "abcd".into(),
            tree_hash_short: "1234".into(),
            file_count: 3,
            stale: false,
        }];
        render_status_materialized(&fresh_only, false);
    }
}
