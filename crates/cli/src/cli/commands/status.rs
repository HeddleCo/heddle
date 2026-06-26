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
    AgentUsageSummary, GitRemoteTrackingStatus, Repository, RepositoryCapability,
    RepositoryOperationStatus, Thread, ThreadFreshness, ThreadImpactCategory, ThreadMode,
    ThreadState, WorktreeCompareProfile, describe_thread_advice_with_initial, is_synthetic_root,
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
        GitOverlayHealth, RepositoryVerificationState,
        build_git_overlay_health_with_worktree_status, build_plain_git_verification_probe,
        override_trust_recommended_action, remote_tracking_with_verification_action,
        repository_setup_guidance, serialize_empty_action_as_null,
    },
    next_action::{NextActionValidationContext, write_command_json},
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
    cli::{Cli, output_is_compact, should_output_json, style, worktree_status_options},
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
    #[serde(skip)]
    validation_capability: RepositoryCapability,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    heddle_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actor: Option<ActorInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_summary: Option<AgentUsageSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_progress_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    report_flush_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attach_reason: Option<String>,
    thread_mode: Option<ThreadMode>,
    thread_state: Option<ThreadState>,
    freshness: Option<ThreadFreshness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_thread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_thread: Option<String>,
    child_threads: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<String>,
    promotion_suggested: bool,
    impact_categories: Vec<ThreadImpactCategory>,
    heavy_impact_paths: Vec<String>,
    #[serde(skip)]
    changed_paths: Vec<String>,
    changed_path_count: usize,
    worktree_changed_path_count: usize,
    thread_changed_path_count: usize,
    blockers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity_notice: Option<String>,
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    recommended_action: String,
    recommended_action_template: Option<super::command_catalog::ActionTemplate>,
    recovery_commands: Vec<String>,
    recovery_action_templates: Vec<super::command_catalog::ActionTemplate>,
    thread_health: String,
    coordination_status: CoordinationStatus,
    /// Provenance of a `coordination_status == Blocked`: `true` when the
    /// Blocked is the trust/health re-encoding (the status builder forces
    /// `coordination_status = Blocked` to surface a dirty / uncaptured /
    /// unverified *health* blocker via the coordination field — sites
    /// ~571 short-path, ~956 trust override), `false` when it is a
    /// genuine inter-thread block from `build_thread_view`
    /// (`ThreadState::Blocked` or concurrent actives). Carried so the
    /// effective-coordination mask keys on *why* the axis is Blocked
    /// rather than guessing from `thread_health` cleanliness — a genuine
    /// inter-thread block can co-exist with a dirty worktree and must
    /// still surface. Render-only; excluded from the JSON contract.
    #[serde(skip)]
    coordination_blocked_by_trust: bool,
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
    #[serde(serialize_with = "serialize_empty_action_as_null")]
    recommended_action: String,
    recommended_action_template: Option<super::command_catalog::ActionTemplate>,
    recovery_commands: Vec<String>,
    recovery_action_templates: Vec<super::command_catalog::ActionTemplate>,
    thread_health: String,
    changed_path_count: usize,
    changes: ChangesInfo,
    git_index: Option<GitIndexPlan>,
}

/// Recommend the first save when a native Heddle repository has been
/// initialized but has no user-visible history yet (the log shows only
/// the filtered synthetic root) and the worktree is clean — i.e. there
/// is genuinely nothing to act on yet. The repo is already initialized,
/// so recommending `heddle init --quickstart` here read as "you
/// initialized wrong" (heddle#644); point at the first save instead.
/// A dirty worktree already has its own advice, and
/// Git-overlay repos have their own onboarding (import/adopt), so both
/// are left alone.
fn first_save_recommendation(
    repo: &Repository,
    current_state: Option<&objects::object::State>,
    worktree_clean: bool,
) -> Option<String> {
    if !worktree_clean || repo.capability() != RepositoryCapability::NativeHeddle {
        return None;
    }
    let empty_log = current_state.map(is_synthetic_root).unwrap_or(true);
    empty_log.then(|| "heddle commit -m \"...\"".to_string())
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

pub(crate) fn prompt_segment(cli: &Cli) -> Result<Option<String>> {
    let Ok(output) = build_status_output(cli, true) else {
        return Ok(None);
    };
    let repo = cli.open_repo().ok();
    let current_lane = repo
        .as_ref()
        .and_then(|repo| repo.current_lane().ok())
        .flatten();
    let subject = output
        .thread
        .as_deref()
        .or(current_lane.as_deref())
        .or_else(|| output.current_state.as_ref().map(|_| "detached"));
    let Some(subject) = subject else {
        return Ok(None);
    };

    let mut segment = subject.to_string();
    if output.changed_path_count > 0 || !output.changes.is_empty() {
        segment.push('*');
    }
    if let Some(remote) = output.remote_tracking.as_ref() {
        if remote.ahead > 0 {
            segment.push_str(&format!(" +{}", remote.ahead));
        }
        if remote.behind > 0 {
            segment.push_str(&format!(" -{}", remote.behind));
        }
    }
    Ok(Some(segment))
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
        recommended_action_template: trust.recommended_action_template.clone(),
        recovery_commands: trust.recovery_commands.clone(),
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
        write_command_json(
            output,
            output_is_compact(cli),
            NextActionValidationContext::without_repo(&["status"]),
        )?;
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
    let repo = cli.open_repo()?;
    let repo_open_ms = repo_open_start.elapsed().as_millis();
    let body_start = Instant::now();
    let as_json = should_output_json(cli, Some(repo.config()));
    let compact_json = as_json && output_is_compact(cli);
    let short_text = short && !as_json;
    let short_path = short_text || compact_json;

    let current_state_start = Instant::now();
    let current_state = repo.current_state()?;
    let current_state_ms = current_state_start.elapsed().as_millis();

    let operation_start = Instant::now();
    let operation = repo.operation_status()?;
    let operation_ms = operation_start.elapsed().as_millis();
    // Single gating predicate for the slower "walk every thread /
    // inspect remote tracking / populate cross-thread relations" path.
    // Full JSON and `-v` text display thread topology and cross-thread
    // relations. Compact JSON is the lightweight decision surface and uses
    // the same construction path as short text.
    let needs_full_walk = cli.verbose > 0 || (as_json && !compact_json);
    let needs_remote_tracking = needs_full_walk || short_text;
    let remote_tracking_start = Instant::now();
    let remote_tracking = if needs_remote_tracking {
        repo.git_remote_tracking_status().unwrap_or(None)
    } else {
        None
    };
    let remote_tracking_ms = remote_tracking_start.elapsed().as_millis();

    let import_hint_start = Instant::now();
    let import_hint = if short_path {
        None
    } else {
        repo.git_overlay_import_hint().unwrap_or(None)
    };
    let import_hint_ms = import_hint_start.elapsed().as_millis();
    // Compute the git-overlay worktree status ONCE and thread it through both
    // consumers (the health build below + the changes computation here). It
    // re-reads + SHA-1s every tracked file (~950ms on a 10k-file worktree);
    // previously `status` paid that twice.
    let git_overlay_status_start = Instant::now();
    let git_worktree_status_result = repo.git_overlay_worktree_status();
    let git_overlay_status_ms = git_overlay_status_start.elapsed().as_millis();
    let git_overlay_health_start = Instant::now();
    let git_overlay_health =
        build_git_overlay_health_with_worktree_status(&repo, &git_worktree_status_result);
    let git_overlay_health_ms = git_overlay_health_start.elapsed().as_millis();
    let verification_start = Instant::now();
    let trust = RepositoryVerificationState::from_health(&repo, git_overlay_health.clone());
    let verification_ms = verification_start.elapsed().as_millis();
    let remote_tracking =
        remote_tracking.map(|remote| remote_tracking_with_verification_action(remote, &trust));
    let status_options = worktree_status_options(Some(repo.config()));
    let git_worktree_status = git_worktree_status_result.unwrap_or(None);
    let git_index_start = Instant::now();
    let git_index = git_index_plan_for_repo(&repo)?;
    let git_index_ms = git_index_start.elapsed().as_millis();
    let identity_notice = first_capture_identity_notice(&repo, current_state.as_ref())?;
    let git_clean_mapping_blocker = matches!(
        trust.status.as_str(),
        "needs_import" | "needs_reconcile" | "git_branch_advanced"
    ) && git_worktree_status
        .as_ref()
        .is_some_and(WorktreeStatus::is_clean);
    let git_backed_mapping = trust.mapping_state == "git_backed";

    // Get worktree status
    let worktree_status_start = Instant::now();
    let (changes, worktree_profile) = if git_clean_mapping_blocker {
        (ChangesInfo::default(), None)
    } else if let Some(status) = git_worktree_status.as_ref()
        && !status.is_clean()
        && trust.status != "needs_checkpoint"
    {
        (changes_from_status(status), None)
    } else if git_backed_mapping {
        (
            git_worktree_status
                .as_ref()
                .map(changes_from_status)
                .unwrap_or_default(),
            None,
        )
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

    if short_path {
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
        let worktree_clean =
            changes.modified.is_empty() && changes.added.is_empty() && changes.deleted.is_empty();
        let recommended_action =
            first_save_recommendation(&repo, current_state.as_ref(), worktree_clean)
                .unwrap_or(recommended_action);
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
                    ProfileField::millis("git_overlay_status_ms", git_overlay_status_ms),
                    ProfileField::millis("git_overlay_health_ms", git_overlay_health_ms),
                    ProfileField::millis("verification_ms", verification_ms),
                    ProfileField::millis("git_index_ms", git_index_ms),
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
            validation_capability: repo.capability(),
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
            changed_paths: changes_paths(&changes).into_iter().collect(),
            changed_path_count: changes_path_count(&changes),
            worktree_changed_path_count: changes_path_count(&changes),
            thread_changed_path_count: 0,
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
            recommended_action_template: recommended_action_fields.template,
            recommended_action,
            recovery_commands: trust.recovery_commands.clone(),
            recovery_action_templates: trust.recovery_action_templates.clone(),
            thread_health: trust.status.clone(),
            coordination_status: if trust.verified {
                CoordinationStatus::Clean
            } else {
                CoordinationStatus::Blocked
            },
            // This short-path Blocked is purely the unverified-trust
            // re-encoding (the only non-Clean branch above is the
            // `!trust.verified` one), so it is trust-derived.
            coordination_blocked_by_trust: !trust.verified,
            is_isolated: false,
            parallel_threads: Vec::new(),
            state: None,
            git_checkpoint: None,
            changes,
            // Short text still renders the materialized-thread advisory
            // (see `render_short_status` → `render_materialized_advisory`).
            // The check is cheap (one ref lookup per manifest), so keep
            // the fast path honest and let stale threads surface.
            materialized_threads: assess_materialized_threads(&repo),
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
        validation_capability: repo.capability(),
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
            .map(|thread| thread.coordination_status.clone())
            .unwrap_or(CoordinationStatus::Clean),
        // This coordination_status comes straight from `build_thread_view`
        // (genuine inter-thread state). The trust re-encoding, if any, is
        // applied in the rebuild below; here it is never trust-derived.
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
    };
    let late_state_ms = late_state_start.elapsed().as_millis();
    let advice_start = Instant::now();
    let has_changes = !output.changes.modified.is_empty()
        || !output.changes.added.is_empty()
        || !output.changes.deleted.is_empty();
    let git_backed_current_thread = git_backed_mapping;
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
    let recommended_action = if git_backed_current_thread {
        if has_changes {
            "heddle commit -m \"...\"".to_string()
        } else {
            String::new()
        }
    } else {
        // A freshly-`init`'d native repo whose log is still empty (only the
        // synthetic root) and whose worktree is clean has nothing to act on
        // yet — point the user at the first save.
        first_save_recommendation(&repo, current_state.as_ref(), !has_changes)
            .unwrap_or(recommended_action)
    };
    let recommended_action_fields = ActionFields::from_action(&recommended_action);
    let thread_health = if trust.verified {
        if git_backed_current_thread {
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
    let display_thread_summary = (!git_backed_current_thread)
        .then_some(thread_summary.as_ref())
        .flatten();
    let worktree_changed_path_count = changes_path_count(&output.changes);
    let thread_changed_path_count =
        captured_thread_path_count(display_thread_summary, &output.changes);
    // Resolve the trust/health override against the PRE-override (genuine,
    // from `build_thread_view`) coordination status. A genuine inter-thread
    // `Blocked` captured here must survive the override and stay surfaceable.
    let (coordination_status, coordination_blocked_by_trust) = resolve_coordination_with_trust(
        output.coordination_status.clone(),
        blocked_by_trust,
        needs_checkpoint,
    );
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
        recommended_action_template: recommended_action_fields.template,
        recovery_commands: trust.recovery_commands.clone(),
        recovery_action_templates: trust.recovery_action_templates.clone(),
        thread_health,
        coordination_status,
        // `coordination_blocked_by_trust` is true ONLY when the resulting
        // `Blocked`'s SOLE source is the trust/health override; a genuine
        // inter-thread `Blocked` captured before the override keeps it false
        // so the mask never hides it (even with a dirty worktree). See
        // `resolve_coordination_with_trust`.
        coordination_blocked_by_trust,
        // `thread_state` is lifecycle-only and must match `thread list` for the
        // same thread/instant (heddle#306). The verification/dirty-worktree
        // blocker is a health signal, surfaced via `coordination_status` above.
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
                ProfileField::millis("git_overlay_status_ms", git_overlay_status_ms),
                ProfileField::millis("git_overlay_health_ms", git_overlay_health_ms),
                ProfileField::millis("verification_ms", verification_ms),
                ProfileField::millis("git_index_ms", git_index_ms),
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

/// Project a `recommended_action` String into the compact
/// `next_action` (+ template) pair: an empty action is the contract's
/// "no action" and maps to `None` so the template is dropped too.
fn compact_next_action(
    recommended_action: &str,
    template: &Option<super::command_catalog::ActionTemplate>,
) -> (
    Option<String>,
    Option<super::command_catalog::ActionTemplate>,
) {
    if recommended_action.trim().is_empty() {
        (None, None)
    } else {
        (Some(recommended_action.to_string()), template.clone())
    }
}

impl super::compact::CompactProjection for StatusOutput {
    fn compact(&self) -> super::compact::CompactOutput {
        let (next_action, next_action_template) =
            compact_next_action(&self.recommended_action, &self.recommended_action_template);
        let mut compact = super::compact::CompactOutput::new(self.output_kind);
        compact.coordination_status = Some(self.coordination_status.clone());
        compact.blockers = self.blockers.clone();
        compact.next_action = next_action;
        compact.next_action_template = next_action_template;
        compact.changed_paths = Some(self.changed_paths.clone());
        compact.changed_path_count = Some(self.changed_paths.len());
        compact
    }
}

impl super::compact::CompactProjection for PlainGitStatusOutput {
    fn compact(&self) -> super::compact::CompactOutput {
        let (next_action, next_action_template) =
            compact_next_action(&self.recommended_action, &self.recommended_action_template);
        let changed_paths: Vec<String> = changes_paths(&self.changes).into_iter().collect();
        let mut compact = super::compact::CompactOutput::new(self.output_kind);
        compact.next_action = next_action;
        compact.next_action_template = next_action_template;
        compact.changed_path_count = Some(changed_paths.len());
        compact.changed_paths = Some(changed_paths);
        compact
    }
}

pub(crate) fn render_status(cli: &Cli, output: &StatusOutput, short: bool) -> Result<()> {
    let render_start = Instant::now();
    if output.render_json {
        write_command_json(
            output,
            output_is_compact(cli),
            NextActionValidationContext::new(&["status"], output.validation_capability),
        )?;
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
    if let Some(hint) = &output.git_overlay_import_hint
        && !hint
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
                    "clean; .heddle metadata is present, Git refs stay in Git storage, and the Git worktree stays clean"
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
    // Progressive disclosure: the default view shows ONE combined
    // verdict answering "is my checkout OK?". The two component axes
    // (Health = local checkout state, Coordination = cross-thread
    // integration state) overlap and create read-carefully overhead,
    // so they only appear under `-v`. The combined verdict still
    // signals non-clean whenever either axis is, so the default reader
    // never loses the "something's wrong" signal — `-v` then tells
    // them which axis. JSON is unaffected (both fields always emitted).
    let (verdict, verdict_reason) = status_combined_verdict(output);
    println!("Verdict: {}", style::thread_state(&verdict));
    if let Some(reason) = verdict_reason {
        println!("  {}", style::dim(reason));
    }
    if verbose {
        // Health text is short ("clean" / "blocked" / etc.) — colour it
        // so a glance tells you the state without reading the word.
        println!(
            "Health: {}",
            style::thread_state(&human_thread_health(&output.thread_health))
        );
        println!("Coordination: {}", human_coordination_status(output));
    }
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
        println!("Lifecycle: {}", style::thread_state(&state.to_string()));
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
    // `heddle doctor -v`.
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
        if output.repository_capability != "git-overlay" {
            return "there are uncommitted worktree changes; commit captures them as a Heddle state";
        }
        return "there are uncommitted worktree changes; commit captures them and writes the matching Git commit";
    }
    if output.repository_capability == "git-overlay" && output.recommended_action.contains("commit")
    {
        return "the work is saved in Heddle; commit writes the matching Git commit";
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
    if output.recommended_action.contains("land") {
        return "the thread is ready to land into its target";
    }
    "this is the safest command for the current repository and thread state"
}

fn status_next_follow_up(output: &StatusOutput) -> Option<&'static str> {
    let action = output.recommended_action.as_str();
    if action.contains("commit") && status_has_publish_target(output) {
        Some("run `heddle push` when the Git commit is ready to publish")
    } else if action.contains("ready") {
        Some("run `heddle land --thread <thread> --no-push` after readiness passes")
    } else if action.contains("land") {
        Some("add `--push` only when a remote is configured and the thread should be published")
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
            let stale = match repo
                .refs()
                .get_thread(&objects::object::ThreadName::new(&summary.thread))
            {
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

fn changed_paths(
    thread: Option<&super::thread::ThreadSummary>,
    changes: &ChangesInfo,
) -> Vec<String> {
    let mut paths = BTreeSet::new();
    if let Some(thread) = thread {
        paths.extend(thread.changed_paths.iter().cloned());
    }
    paths.extend(changes.modified.iter().cloned());
    paths.extend(changes.added.iter().cloned());
    paths.extend(changes.deleted.iter().cloned());
    paths.into_iter().collect()
}

fn changes_path_count(changes: &ChangesInfo) -> usize {
    changes_paths(changes).len()
}

fn captured_thread_path_count(
    thread: Option<&super::thread::ThreadSummary>,
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

fn changes_paths(changes: &ChangesInfo) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    paths.extend(changes.modified.iter().cloned());
    paths.extend(changes.added.iter().cloned());
    paths.extend(changes.deleted.iter().cloned());
    paths
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
    }
    if !has_changes && output.trust.verified {
        println!("{}", style::dim("No unsaved changes, worktree clean"));
    } else if !has_changes && output.trust.worktree_state == "not_checked" {
        let message = if output.trust.status == "git_branch_advanced" {
            "No unsaved worktree changes detected; import the external Git branch tip before comparing Heddle state"
        } else {
            "No unsaved worktree changes detected; finish setup before comparing Heddle state"
        };
        println!("{}", style::dim(message));
    } else if !has_changes && output.trust.worktree_state == "clean" {
        println!(
            "{}",
            style::dim(&format!(
                "No unsaved worktree changes detected; repository verification is {}",
                output.trust.status
            ))
        );
    } else if !has_changes {
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
        "staged_index" => "plain `heddle commit` checkpoints staged paths only",
        "worktree_all" => "plain `heddle commit` captures and checkpoints all current paths",
        "worktree_all_explicit" => {
            "`heddle commit --all` captures and checkpoints staged, unstaged, and untracked paths"
        }
        "none" => "no Git paths are ready to commit",
        _ => "`heddle commit` captures and checkpoints the current Git worktree",
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

/// Severity rank for the `thread_health` axis. Higher = more
/// blocking. Drives which axis a non-clean combined verdict surfaces.
fn health_severity(thread_health: &str) -> u8 {
    match thread_health {
        "clean" => 0,
        "needs_reconcile" | "git_branch_advanced" => 4,
        "needs_init" | "needs_import" => 3,
        "needs_checkpoint" => 2,
        // dirty_worktree / uncaptured / unknown: local work in progress.
        _ => 1,
    }
}

/// Severity rank for the coordination axis. Ahead / merge-ready are
/// non-clean but benign forward states, so they rank below the
/// integration blockers (diverged / blocked).
fn coordination_severity(status: &CoordinationStatus) -> u8 {
    match status {
        CoordinationStatus::Clean => 0,
        CoordinationStatus::Ahead | CoordinationStatus::MergeReady => 1,
        CoordinationStatus::Diverged => 3,
        CoordinationStatus::Blocked => 4,
    }
}

/// Combine the PRE-override (genuine, from `build_thread_view`) coordination
/// status with the trust/health override. The override may re-encode a
/// dirty / uncaptured / unverified-trust *health* blocker onto the
/// coordination axis (as a maskable trust-derived `Blocked`) ONLY when the
/// pre-override axis was genuinely CLEAN. Any genuine non-clean state from
/// `build_thread_view` — `Blocked`, `Diverged`, `Ahead`, `MergeReady`, or a
/// variant added later — wins: it is preserved as-is and stays surfaceable
/// even when the worktree is also dirty, so the health blocker never hides
/// the real coordination state.
///
/// Returns the final `coordination_status` and `coordination_blocked_by_trust`
/// — the latter true ONLY when the resulting `Blocked`'s sole source is the
/// trust/health path (i.e. a genuinely-clean axis was re-encoded). Keying the
/// whole rule on the single "was the pre-override axis genuinely clean?"
/// predicate — derived from `coordination_axis_clean`, an exhaustive match,
/// NOT a hardcoded state list — is what closes the masking class: a new
/// `CoordinationStatus` variant is covered automatically and can never be
/// silently re-stamped as trust-derived (cid 3328810941).
fn resolve_coordination_with_trust(
    pre_override: CoordinationStatus,
    blocked_by_trust: bool,
    needs_checkpoint: bool,
) -> (CoordinationStatus, bool) {
    // The pre-override status comes straight from `build_thread_view`, so it
    // carries no trust encoding — read its genuine cleanliness with
    // `blocked_by_trust = false`.
    let pre_override_clean = coordination_axis_clean(&pre_override, false);
    let trust_override = blocked_by_trust && !needs_checkpoint;
    // Re-encode (and mark maskable) ONLY a genuinely-clean axis; a genuine
    // non-clean state is preserved and never marked trust-only.
    let mask_as_trust = trust_override && pre_override_clean;
    let coordination_status = if mask_as_trust {
        CoordinationStatus::Blocked
    } else {
        pre_override
    };
    (coordination_status, mask_as_trust)
}

/// Single source of truth for "is the coordination axis genuinely
/// (non-trust) clean?". `coordination_status` is overloaded: the status
/// builder re-encodes a dirty / uncaptured / unverified-trust *health*
/// blocker by forcing `coordination_status = Blocked` (build sites ~571
/// short-path, ~956 trust override), carrying `blocked_by_trust = true`.
/// That Blocked is a health signal, so the coordination axis is
/// effectively clean — the health axis owns the blocker. A genuine
/// inter-thread Blocked from `build_thread_view` (`ThreadState::Blocked`
/// or concurrent actives) carries `blocked_by_trust = false` and is
/// NEVER masked, even when the worktree is also dirty — a real
/// inter-thread block can co-exist with local WIP and must still
/// surface. Keying on the Blocked's *provenance* (this flag), not on
/// `thread_health` cleanliness, is what keeps those two cases apart.
/// Both the human `-v` render and the combined verdict route through
/// here so they can't drift.
fn coordination_axis_clean(coordination: &CoordinationStatus, blocked_by_trust: bool) -> bool {
    match coordination {
        CoordinationStatus::Clean => true,
        CoordinationStatus::Blocked => blocked_by_trust,
        CoordinationStatus::Ahead
        | CoordinationStatus::Diverged
        | CoordinationStatus::MergeReady => false,
    }
}

/// Pure core of the combined verdict: which axes are effectively clean
/// and the resulting reason line. Split from rendering so it is
/// unit-testable without constructing a full `StatusOutput`.
fn combined_verdict_axes(
    thread_health: &str,
    coordination: &CoordinationStatus,
    coordination_blocked_by_trust: bool,
) -> (bool, bool, Option<&'static str>) {
    let health_clean = thread_health == "clean";
    let coordination_clean = coordination_axis_clean(coordination, coordination_blocked_by_trust);
    let reason = match (health_clean, coordination_clean) {
        (true, true) => None,
        (false, false) => Some("checkout health and thread coordination both need attention"),
        (false, true) => Some("checkout health needs attention"),
        (true, false) => Some("thread coordination needs attention"),
    };
    (health_clean, coordination_clean, reason)
}

/// Combined top-line verdict for the default long view. Returns the
/// styled verdict word plus an optional one-line reason.
///
/// `clean` only when BOTH the health and coordination axes are
/// *effectively* clean; otherwise the more-severe axis is surfaced as
/// the verdict word so a reader of the default view still learns the
/// checkout is not clean. Ties favour the local health axis — that's the
/// blocker the user usually acts on first. The reason names which axis
/// (or both) is at fault; `-v` then prints the per-axis detail.
fn status_combined_verdict(output: &StatusOutput) -> (String, Option<&'static str>) {
    let (health_clean, coordination_clean, reason) = combined_verdict_axes(
        &output.thread_health,
        &output.coordination_status,
        output.coordination_blocked_by_trust,
    );
    if health_clean && coordination_clean {
        return ("clean".to_string(), None);
    }
    // Surface health when it's the (or the more-severe) non-clean axis,
    // and always when the coordination axis is only health-encoded — in
    // that case the health blocker is the real story.
    let surface_health = !health_clean
        && (coordination_clean
            || health_severity(&output.thread_health)
                >= coordination_severity(&output.coordination_status));
    let word = if surface_health {
        human_thread_health(&output.thread_health)
    } else {
        human_coordination_status(output)
    };
    (word, reason)
}

fn human_coordination_status(output: &StatusOutput) -> String {
    coordination_label(
        &output.coordination_status,
        output.coordination_blocked_by_trust,
    )
}

/// Render the coordination axis for the `-v` view. A trust-derived `Blocked`
/// (sole source = the health override, so `coordination_axis_clean` reports
/// the axis effectively clean) shows as "work in progress" — the health axis
/// owns the blocker. Every genuine coordination state renders under its own
/// name: a genuine inter-thread `Blocked` and the non-clean siblings
/// (`Diverged` / `Ahead` / `MergeReady`) are never hidden behind the WIP mask.
/// Split from the `StatusOutput` wrapper so the render path is unit-testable.
fn coordination_label(coordination: &CoordinationStatus, blocked_by_trust: bool) -> String {
    if matches!(coordination, CoordinationStatus::Blocked)
        && coordination_axis_clean(coordination, blocked_by_trust)
    {
        "work in progress".to_string()
    } else {
        coordination.to_string()
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
        let repo = cli.open_repo().ok()?;
        let upstream = repo.config().hosted.upstream_url.as_deref()?.trim();
        let namespace = repo.config().hosted.namespace.as_deref()?.trim();
        if upstream.is_empty() || namespace.is_empty() {
            return None;
        }

        let token = UserConfig::load_default().ok()?.remote_token().ok()??;
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
        stream.send(Message::Text(hello.into())).await.ok()?;
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

    use clap::Parser as _;
    use repo::{AgentUsageSummary, Repository};
    use serde_json::Value;
    use tempfile::TempDir;

    use super::{
        ActorInfo, ChangesInfo, CoordinationStatus, GitOverlayHealth, MaterializedThreadInfo,
        PlainGitStatusOutput, RepositoryVerificationState, assess_materialized_threads,
        build_status_output, combined_verdict_axes, coordination_axis_clean, coordination_label,
        render_status_materialized, resolve_coordination_with_trust,
    };

    const AGENT_CONTEXT_STATUS_KEYS: &[&str] = &[
        "path",
        "execution_path",
        "session_id",
        "heddle_session_id",
        "actor",
        "harness",
        "thinking_level",
        "usage_summary",
        "last_progress_at",
        "report_flush_state",
        "attach_reason",
        "target_thread",
        "parent_thread",
        "task",
    ];

    fn status_cli(repo_dir: &std::path::Path) -> crate::cli::Cli {
        crate::cli::Cli::parse_from([
            "heddle",
            "--repo",
            repo_dir.to_str().expect("utf-8 temp path"),
            "--output",
            "json",
            "status",
        ])
    }

    fn status_json(repo_dir: &std::path::Path) -> Value {
        let cli = status_cli(repo_dir);
        let output = build_status_output(&cli, false).expect("build status output");
        serde_json::to_value(&output).expect("serialize status")
    }

    fn init_repo_with_materialized_thread(content: &[u8]) -> (TempDir, TempDir, Repository) {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("hello.txt"), content).unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let dest_holder = TempDir::new().unwrap();
        let dest = dest_holder.path().join("out");
        repo.materialize_thread("main", &dest, &repo::AudienceTier::Internal)
            .unwrap();
        (repo_dir, dest_holder, repo)
    }

    #[test]
    fn assess_returns_empty_when_no_materialized_threads() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init_default(dir.path()).unwrap();
        assert!(assess_materialized_threads(&repo).is_empty());
    }

    #[test]
    fn status_omits_agent_context_fields_when_unset() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("hello.txt"), b"hello\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let json = status_json(repo_dir.path());
        for key in AGENT_CONTEXT_STATUS_KEYS {
            assert!(
                json.get(*key).is_none(),
                "status must omit unset agent-context key `{key}`: {json}"
            );
        }
    }

    #[tokio::test]
    async fn status_serializes_agent_context_fields_when_set() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("hello.txt"), b"hello\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let cli = status_cli(repo_dir.path());
        super::super::actor_cmd::cmd_actor_spawn(
            &cli,
            None,
            true,
            Some("codex".to_string()),
            Some("gpt-5".to_string()),
        )
        .await
        .expect("spawn attached actor");

        let mut output = build_status_output(&cli, false).expect("build status output");
        output.path = Some(repo_dir.path().display().to_string());
        output.execution_path = Some(repo_dir.path().display().to_string());
        output.heddle_session_id = Some("heddle-session-1".to_string());
        output.actor = Some(ActorInfo {
            provider: Some("codex".to_string()),
            model: Some("gpt-5".to_string()),
        });
        output.harness = Some("codex-cli".to_string());
        output.thinking_level = Some("high".to_string());
        output.usage_summary = Some(AgentUsageSummary::default());
        output.last_progress_at = Some("2026-06-12T00:00:00Z".to_string());
        output.report_flush_state = Some("flushed".to_string());
        output.target_thread = Some("main".to_string());
        output.parent_thread = Some("main".to_string());
        output.task = Some("status surface".to_string());

        let json = serde_json::to_value(&output).expect("serialize status");
        for key in AGENT_CONTEXT_STATUS_KEYS {
            assert!(
                json.get(*key).is_some(),
                "status must serialize set agent-context key `{key}`: {json}"
            );
        }
        assert_eq!(json["actor"]["provider"], "codex");
        assert_eq!(json["actor"]["model"], "gpt-5");
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
        let mat = repo
            .materialize_thread("main", &dest, &repo::AudienceTier::Internal)
            .unwrap();

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

    #[test]
    fn dirty_wip_combined_verdict_reason_is_health_only() {
        // (a) Repro: a dirty/uncaptured checkout re-encodes its health
        // blocker as a *trust-derived* `coordination_status = Blocked`
        // (`coordination_blocked_by_trust = true`). The combined verdict
        // must NOT double-count that as a coordination failure — the
        // reason is the health/WIP reason alone, and the axis masks.
        for health in ["dirty_worktree", "uncaptured"] {
            let (health_clean, coordination_clean, reason) =
                combined_verdict_axes(health, &CoordinationStatus::Blocked, true);
            assert!(!health_clean, "{health} is a non-clean health state");
            assert!(
                coordination_clean,
                "{health}'s trust-derived Blocked is a health-signal encoding → coordination effectively clean"
            );
            assert!(
                coordination_axis_clean(&CoordinationStatus::Blocked, true),
                "{health}: trust-derived Blocked must mask (work in progress)"
            );
            assert_eq!(
                reason,
                Some("checkout health needs attention"),
                "{health}: reason must be health-only, not a coordination/both warning"
            );
            let reason = reason.unwrap();
            assert!(
                !reason.contains("coordination") && !reason.contains("both need attention"),
                "{health}: reason must not mention coordination: {reason}"
            );
        }
    }

    #[test]
    fn trust_blocked_combined_verdict_reason_is_health_only() {
        // (a') The same masking covers a trust-blocked WIP checkout: its
        // Blocked is the verification health signal (trust-derived), not
        // coordination.
        let (_, coordination_clean, reason) =
            combined_verdict_axes("git_branch_advanced", &CoordinationStatus::Blocked, true);
        assert!(coordination_clean);
        assert_eq!(reason, Some("checkout health needs attention"));
    }

    #[test]
    fn genuine_blocked_surfaces_even_when_worktree_dirty() {
        // (b) THE LEAK (cid 3327990627). A *genuine* inter-thread block —
        // `build_thread_view` set `coordination_status = Blocked` from
        // `ThreadState::Blocked`, carrying `coordination_blocked_by_trust
        // = false` — can co-exist with local WIP, where
        // `describe_thread_advice` reports a non-clean `thread_health`
        // (e.g. `dirty_worktree`). r2 keyed the mask on health
        // cleanliness, so this Blocked was wrongly masked as "work in
        // progress" and the verdict named only checkout health, hiding
        // the real coordination block until the worktree was cleaned.
        // The provenance-keyed mask must NOT mask it: a non-trust Blocked
        // is always a genuine coordination block.
        //
        // The current (a)/(b) inputs differ ONLY in provenance — same
        // `thread_health="dirty_worktree"`, same `Blocked` — which is why
        // a `thread_health`-keyed helper cannot tell them apart and the
        // provenance flag is required.
        for health in ["dirty_worktree", "uncaptured"] {
            assert!(
                !coordination_axis_clean(&CoordinationStatus::Blocked, false),
                "{health}: a genuine (non-trust) Blocked must never mask, even when health is dirty"
            );
            let (health_clean, coordination_clean, reason) =
                combined_verdict_axes(health, &CoordinationStatus::Blocked, false);
            assert!(!health_clean, "{health} is a non-clean health state");
            assert!(
                !coordination_clean,
                "{health}: a genuine inter-thread Blocked is a real coordination block"
            );
            assert_eq!(
                reason,
                Some("checkout health and thread coordination both need attention"),
                "{health}: the verdict reason must name the coordination block, not just health"
            );
            assert!(
                reason.unwrap().contains("coordination"),
                "{health}: reason must surface coordination: {reason:?}"
            );
        }
    }

    #[test]
    fn genuine_coordination_states_still_surface() {
        // (c) A real inter-thread coordination state with clean health
        // must still be reported by the combined verdict. A clean-health
        // Blocked is genuine (`coordination_blocked_by_trust = false`).
        for coordination in [
            CoordinationStatus::Ahead,
            CoordinationStatus::Diverged,
            CoordinationStatus::MergeReady,
            CoordinationStatus::Blocked,
        ] {
            assert!(
                !coordination_axis_clean(&coordination, false),
                "{coordination:?} as a genuine (non-trust) state is never clean"
            );
            let (health_clean, coordination_clean, reason) =
                combined_verdict_axes("clean", &coordination, false);
            assert!(health_clean && !coordination_clean);
            assert_eq!(
                reason,
                Some("thread coordination needs attention"),
                "{coordination:?}: combined verdict must name coordination"
            );
        }
    }

    #[test]
    fn both_axes_clean_verdict_has_no_reason() {
        // (d) All clean → clean verdict, no reason.
        let (health_clean, coordination_clean, reason) =
            combined_verdict_axes("clean", &CoordinationStatus::Clean, false);
        assert!(health_clean && coordination_clean);
        assert_eq!(reason, None);
    }

    /// Every `CoordinationStatus` variant, so the table below is driven from
    /// the enum rather than a hardcoded subset — a newly added variant fails
    /// to compile here until it is listed, which is what keeps the
    /// close-the-class coverage honest.
    const ALL_COORDINATION_STATES: [CoordinationStatus; 5] = [
        CoordinationStatus::Clean,
        CoordinationStatus::Ahead,
        CoordinationStatus::Diverged,
        CoordinationStatus::Blocked,
        CoordinationStatus::MergeReady,
    ];

    #[test]
    fn coordination_provenance_survives_trust_override_across_all_states() {
        // (b'') CLOSE-THE-CLASS (cid 3328810941), generalising r4's
        // Blocked-only ordering fix (cid 3328765243). The trust/health
        // override re-encodes a dirty / unverified checkout as a maskable
        // `coordination_status = Blocked`. It may do so — and mark the axis
        // trust-only/maskable — ONLY when the PRE-override coordination was
        // genuinely CLEAN. Every genuine non-clean state from
        // `build_thread_view` (`Blocked`, `Diverged`, `Ahead`, `MergeReady`,
        // …) must WIN over the override: preserved as-is, never marked
        // trust-only, surfacing in the combined verdict and the `-v` label
        // even with a dirty worktree. r4 special-cased only `Blocked`, so on
        // the pre-fix code the `Diverged` / `Ahead` / `MergeReady` × dirty
        // cells leaked (re-stamped to a masked trust-only `Blocked`).
        //
        // The table is keyed off `coordination_axis_clean(&state, false)` —
        // the single source of truth for "is the genuine axis clean?" — NOT a
        // hardcoded clean/non-clean split, which is what proves the class is
        // closed: a new variant is classified automatically.
        for pre_override in ALL_COORDINATION_STATES {
            let genuinely_clean = coordination_axis_clean(&pre_override, false);
            for &trust_verified in &[true, false] {
                // `blocked_by_trust = !trust.verified`; a dirty/unverified
                // worktree (trust_verified == false) drives the override.
                let blocked_by_trust = !trust_verified;
                let (coordination, blocked_by_trust_only) =
                    resolve_coordination_with_trust(pre_override.clone(), blocked_by_trust, false);
                let health = if trust_verified {
                    "clean"
                } else {
                    "dirty_worktree"
                };
                let (health_clean, coordination_clean, reason) =
                    combined_verdict_axes(health, &coordination, blocked_by_trust_only);
                let label = coordination_label(&coordination, blocked_by_trust_only);
                let ctx = format!("{pre_override:?} / trust_verified={trust_verified}");

                assert_eq!(
                    health_clean, trust_verified,
                    "{ctx}: health axis cleanliness"
                );

                if genuinely_clean {
                    // Genuinely-clean axis: any override is its SOLE source, so
                    // the axis masks as work-in-progress (the health axis owns
                    // the blocker) — this cell must STAY masked.
                    assert!(
                        coordination_clean,
                        "{ctx}: a genuinely-clean axis stays effectively clean"
                    );
                    if trust_verified {
                        assert_eq!(reason, None, "{ctx}: all-clean → no reason");
                        assert_eq!(label, "clean", "{ctx}: clean axis renders as clean");
                    } else {
                        assert_eq!(
                            reason,
                            Some("checkout health needs attention"),
                            "{ctx}: clean axis + dirty worktree → health-only WIP (coordination masked)"
                        );
                        assert_eq!(
                            label, "work in progress",
                            "{ctx}: a sole-trust-derived Blocked renders as WIP, never a coordination state"
                        );
                    }
                } else {
                    // Genuine non-clean state: WINS over the override.
                    assert_eq!(
                        coordination, pre_override,
                        "{ctx}: a genuine non-clean state must be preserved, not re-stamped to Blocked"
                    );
                    assert!(
                        !blocked_by_trust_only,
                        "{ctx}: a genuine non-clean axis is never marked trust-only/maskable"
                    );
                    assert!(
                        !coordination_clean,
                        "{ctx}: a genuine non-clean axis must surface, even with a dirty worktree"
                    );
                    let reason = reason.expect("a non-clean axis always yields a verdict reason");
                    assert!(
                        reason.contains("coordination"),
                        "{ctx}: the default verdict reason must name coordination: {reason}"
                    );
                    if !trust_verified {
                        assert_eq!(
                            reason, "checkout health and thread coordination both need attention",
                            "{ctx}: dirty worktree + genuine coordination state → BOTH axes surface"
                        );
                    }
                    assert_eq!(
                        label,
                        pre_override.to_string(),
                        "{ctx}: -v must show the genuine Coordination state, not the WIP mask"
                    );
                    assert_ne!(
                        label, "work in progress",
                        "{ctx}: a genuine coordination state must never be hidden behind WIP"
                    );
                }
            }
        }
    }

    #[test]
    fn needs_checkpoint_suppresses_the_trust_override() {
        // `needs_checkpoint` short-circuits the override regardless of state:
        // a genuine block is preserved, and a clean axis is left clean rather
        // than re-encoded to a trust-derived Blocked.
        let (coordination, blocked_by_trust_only) =
            resolve_coordination_with_trust(CoordinationStatus::Blocked, true, true);
        assert!(matches!(coordination, CoordinationStatus::Blocked));
        assert!(
            !blocked_by_trust_only,
            "needs_checkpoint suppresses the override; genuine block wins"
        );

        let (coordination, blocked_by_trust_only) =
            resolve_coordination_with_trust(CoordinationStatus::Clean, true, true);
        assert!(
            matches!(coordination, CoordinationStatus::Clean),
            "no override → axis stays clean"
        );
        assert!(!blocked_by_trust_only);
    }

    /// Action-field presence contract (HeddleCo/heddle#645): an empty
    /// `recommended_action` must serialize as `null`, never `""` — the
    /// serialization-boundary walker hard-fails the whole command on a
    /// raw empty. `PlainGitStatusOutput.recommended_action` is cloned
    /// from `trust.recommended_action`, which CAN legitimately be empty;
    /// this pins the safe-by-construction wire shape.
    #[test]
    fn plain_git_status_serializes_empty_recommended_action_as_null() {
        let machine_contract_coverage =
            crate::cli::commands::git_overlay_health::machine_contract_coverage();
        let trust = RepositoryVerificationState {
            verified: true,
            status: "verified".to_string(),
            repository_mode: "plain-git".to_string(),
            heddle_initialized: false,
            git_branch: Some("main".to_string()),
            heddle_thread: None,
            worktree_dirty: false,
            worktree_state: "clean".to_string(),
            import_state: "not_applicable".to_string(),
            mapping_state: "not_applicable".to_string(),
            remote_drift: "clean".to_string(),
            active_operation: None,
            default_remote: None,
            clone_verification: "not_applicable".to_string(),
            machine_contract: crate::cli::commands::git_overlay_health::machine_contract_status(
                &machine_contract_coverage,
            )
            .to_string(),
            machine_contract_coverage,
            workflow_status: "clean".to_string(),
            workflow_summary: "no ready threads are waiting to land".to_string(),
            summary: "plain Git repository".to_string(),
            recommended_action: String::new(),
            recommended_action_template: None,
            recovery_commands: Vec::new(),
            recovery_action_templates: Vec::new(),
            checks: Vec::new(),
        };
        let output = PlainGitStatusOutput {
            output_kind: "status",
            repository_capability: "plain-git".to_string(),
            repository_label: crate::cli::render::repository_mode_label("plain-git", "git-only"),
            storage_model: "git-only".to_string(),
            heddle_initialized: false,
            git_branch: Some("main".to_string()),
            path: "/tmp/repo".to_string(),
            git_overlay_health: GitOverlayHealth {
                status: "healthy".to_string(),
                clean: true,
                summary: "plain Git repository".to_string(),
                recovery_commands: Vec::new(),
                checks: Vec::new(),
            },
            recommended_action: trust.recommended_action.clone(),
            recommended_action_template: trust.recommended_action_template.clone(),
            recovery_commands: trust.recovery_commands.clone(),
            recovery_action_templates: trust.recovery_action_templates.clone(),
            thread_health: trust.status.clone(),
            changed_path_count: 0,
            changes: ChangesInfo {
                modified: Vec::new(),
                added: Vec::new(),
                deleted: Vec::new(),
            },
            git_index: None,
            trust,
        };

        let value = serde_json::to_value(&output).unwrap();
        assert!(value["recommended_action"].is_null());
        assert!(value["verification"]["recommended_action"].is_null());
    }
}
