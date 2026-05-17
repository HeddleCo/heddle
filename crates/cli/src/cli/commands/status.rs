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
use repo::{
    AgentUsageSummary, GitRemoteTrackingStatus, Repository, RepositoryOperationStatus, Thread,
    ThreadFreshness, ThreadImpactCategory, ThreadMode, ThreadState, describe_thread_advice,
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
    operator_loop::primary_next_action,
    thread::{
        CoordinationStatus, collect_thread_summaries, find_thread_summary,
        find_thread_summary_single,
    },
};
use crate::cli::{Cli, should_output_json, style, worktree_status_options};
#[cfg(feature = "client")]
use crate::config::UserConfig;

#[derive(Serialize)]
pub(crate) struct StatusOutput {
    repository_capability: String,
    storage_model: String,
    hosted_enabled: bool,
    operation: Option<RepositoryOperationStatus>,
    remote_tracking: Option<GitRemoteTrackingStatus>,
    /// Carried for the human-readable renderer only. Not part of the
    /// JSON contract: import-hint information is exposed via
    /// `heddle bridge git status --json` instead, which is the
    /// command whose subject is the bridge.
    #[serde(skip)]
    git_overlay_import_hint: Option<GitOverlayImportHintOutput>,
    thread: Option<String>,
    base_state: Option<String>,
    base_root: Option<String>,
    current_state: Option<String>,
    path: Option<String>,
    execution_path: Option<String>,
    session_id: Option<String>,
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
    recommended_action: String,
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
    provider: Option<String>,
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

#[derive(Serialize)]
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
    let output = build_status_output(cli, short)?;
    render_status(cli, &output, short);
    Ok(())
}

pub(crate) fn build_status_output(cli: &Cli, short: bool) -> Result<StatusOutput> {
    let repo_open_start = Instant::now();
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let repo_open_ms = repo_open_start.elapsed().as_millis();
    let body_start = Instant::now();
    let current_state = repo.current_state()?;
    let operation = repo.operation_status()?;
    // `git_remote_tracking_status` spawns `git rev-list` and `git
    // rev-parse` subprocesses (~30ms on macOS, dominated by process
    // fork). The output is the "Remote drift: …" line which only
    // renders when ahead/behind != 0 — frequently absent. Skip the
    // subprocess work in the default text fast path; JSON and -v pay
    // for the contract.
    // Single gating predicate for the slow "walk every thread / spawn
    // git subprocesses / populate cross-thread relations" path. JSON
    // and `-v` text actually display the data; default text doesn't.
    let needs_full_walk = cli.verbose > 0 || should_output_json(cli, Some(repo.config()));
    let remote_tracking = if needs_full_walk {
        repo.git_remote_tracking_status().unwrap_or(None)
    } else {
        None
    };
    let import_hint = repo.git_overlay_import_hint().unwrap_or(None);
    let status_options = worktree_status_options(Some(repo.config()));

    // Get worktree status
    let changes = if let Some(ref state) = current_state {
        let tree = repo.store().get_tree(&state.tree)?.unwrap_or_default();
        let status = repo.compare_worktree_cached_with_options(&tree, &status_options)?;
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
    } else if let Some(status) = repo.git_overlay_worktree_status()? {
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
    } else {
        let tree = objects::object::Tree::new();
        let status = repo.compare_worktree_cached_with_options(&tree, &status_options)?;
        ChangesInfo {
            modified: vec![],
            added: status
                .added
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            deleted: vec![],
        }
    };

    if short && !should_output_json(cli, Some(repo.config())) {
        debug!(
            repo_open_ms,
            body_ms = body_start.elapsed().as_millis(),
            total_ms = repo_open_ms + body_start.elapsed().as_millis(),
            "Status command complete"
        );
        return Ok(StatusOutput {
            repository_capability: repo.capability_label().to_string(),
            storage_model: repo.storage_model_label().to_string(),
            hosted_enabled: repo.hosted_enabled(),
            git_overlay_import_hint: import_hint.clone().map(|hint| GitOverlayImportHintOutput {
                current_branch: hint.current_branch,
                missing_branch_count: hint.missing_branch_count,
                missing_branches: hint.missing_branches,
                recommended_command: hint.recommended_command,
            }),
            operation,
            remote_tracking,
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
            blockers: Vec::new(),
            recommended_action: String::new(),
            thread_health: "clean".to_string(),
            coordination_status: CoordinationStatus::Clean,
            is_isolated: false,
            parallel_threads: Vec::new(),
            state: None,
            git_checkpoint: None,
            changes,
            materialized_threads: assess_materialized_threads(&repo),
        });
    }

    let track_name = repo.current_lane()?;
    // Use the fast single-thread path when default text won't display
    // the child/sibling fields anyway. JSON and -v go through the full
    // walk (`find_thread_summary` → `collect_thread_summaries`) which
    // populates the cross-thread relationships. Saves ~45ms on the
    // common path.
    let thread_summary = if needs_full_walk {
        track_name
            .as_deref()
            .map(|thread| find_thread_summary(&repo, thread))
            .transpose()?
            .flatten()
    } else {
        track_name
            .as_deref()
            .map(|thread| find_thread_summary_single(&repo, thread))
            .transpose()?
            .flatten()
    };
    // `collect_thread_summaries` walks every thread record in the repo
    // (60ms on a 69-thread sibling worktree). The result is then filtered
    // to threads that are Ahead/Blocked/Diverged/MergeReady — frequently
    // an empty list, so the work is wasted. Skip the walk when the
    // default text renderer will discard the field anyway. JSON and -v
    // still pay the cost because they actually display it.
    let parallel_threads = if needs_full_walk {
        collect_thread_summaries(&repo)?
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

    let state_info = current_state.as_ref().map(|s| StateInfo {
        change_id: s.change_id.short(),
        content_hash: s.tree.short(),
        intent: s.intent.clone(),
    });
    let git_checkpoint = current_state
        .as_ref()
        .and_then(|state| {
            repo.latest_git_checkpoint_for_change(&state.change_id)
                .ok()
                .flatten()
        })
        .map(|record| GitCheckpointInfo {
            git_commit: record.git_commit,
            committed_at: record.committed_at,
        });

    let output = StatusOutput {
        repository_capability: repo.capability_label().to_string(),
        storage_model: repo.storage_model_label().to_string(),
        hosted_enabled: repo.hosted_enabled(),
        git_overlay_import_hint: import_hint.clone().map(|hint| GitOverlayImportHintOutput {
            current_branch: hint.current_branch,
            missing_branch_count: hint.missing_branch_count,
            missing_branches: hint.missing_branches,
            recommended_command: hint.recommended_command,
        }),
        operation,
        remote_tracking,
        thread: track_name.clone(),
        base_state: thread_summary
            .as_ref()
            .and_then(|thread| thread.base_state.clone()),
        base_root: thread_summary
            .as_ref()
            .and_then(|thread| thread.base_root.clone()),
        current_state: thread_summary
            .as_ref()
            .and_then(|thread| thread.current_state.clone()),
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
        target_thread: thread_summary
            .as_ref()
            .and_then(|thread| thread.target_thread.clone()),
        parent_thread: thread_summary
            .as_ref()
            .and_then(|thread| thread.parent_thread.clone()),
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
        recommended_action: String::new(),
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
        materialized_threads: assess_materialized_threads(&repo),
    };
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
    let advice = thread_stub
        .as_ref()
        .map(|thread| describe_thread_advice(thread, has_changes, 0, false));
    let output = StatusOutput {
        blockers: advice
            .as_ref()
            .map(|advice| advice.blockers.clone())
            .unwrap_or_default(),
        recommended_action: primary_next_action(
            output.operation.as_ref(),
            output.remote_tracking.as_ref(),
            import_hint.as_ref(),
            advice
                .as_ref()
                .map(|advice| advice.recommended_action.as_str()),
        ),
        thread_health: advice
            .as_ref()
            .map(|advice| advice.thread_health.clone())
            .unwrap_or_else(|| "clean".to_string()),
        changed_path_count: changed_path_count(thread_summary.as_ref(), &output.changes),
        ..output
    };

    debug!(
        repo_open_ms,
        body_ms = body_start.elapsed().as_millis(),
        total_ms = repo_open_ms + body_start.elapsed().as_millis(),
        "Status command complete"
    );

    Ok(output)
}

pub(crate) fn render_status(cli: &Cli, output: &StatusOutput, short: bool) {
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::to_string(output).expect("status JSON serializes")
        );
    } else if short {
        render_short_status(output);
    } else {
        render_long_status(output, cli.verbose > 0);
    }
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
        if !should_output_json(cli, None) && redraw {
            print!("\x1B[2J\x1B[H");
            println!(
                "{}",
                style::dim(&format!(
                    "Watching status · refreshed {} · Ctrl-C to stop",
                    chrono::Local::now().format("%H:%M:%S")
                ))
            );
            io::stdout().flush().ok();
        } else if !should_output_json(cli, None) && watch_iterations.is_some() {
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
        render_status(cli, &output, short);
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
    render_materialized_advisory(output);
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
        let stale: Vec<&MaterializedThreadInfo> =
            threads.iter().filter(|t| t.stale).collect();
        if stale.is_empty() {
            return;
        }
        println!();
        println!("{}", style::bold("Materialized threads (stale)"));
        for t in stale {
            println!(
                "  {} {} {} {}",
                style::bold(&t.name),
                style::dim(&t.state_id),
                style::dim(&t.tree_hash_short),
                style::warn("stale"),
            );
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
    println!(
        "Repository: {} {}",
        output.repository_capability,
        style::dim(&format!("({})", output.storage_model))
    );
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
        println!("Remote drift: {}", style::warn(&remote_tracking.message));
    }
    if let Some(hint) = &output.git_overlay_import_hint {
        println!(
            "Git import: {} other branch(es) are still Git-only ({})",
            hint.missing_branch_count,
            crate::cli::render::preview_list(&hint.missing_branches, hint.missing_branch_count,)
        );
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
    println!("Health: {}", style::thread_state(&output.thread_health));
    println!("Coordination: {}", output.coordination_status);
    if let Some(base) = &output.base_state {
        println!("Base: {}", style::dim(base));
    }
    if let Some(base_root) = &output.base_root {
        println!("Base root: {}", style::dim(base_root));
    }

    if let Some(state) = &output.state {
        println!(
            "State: {} ({})",
            style::change_id(&state.change_id),
            style::dim(&state.content_hash)
        );
        if let Some(intent) = &state.intent {
            // Quote stays plain; the inner intent string is the
            // editorial line, so it's bolded.
            println!("Intent: \"{}\"", style::bold(intent));
        }
        if let Some(checkpoint) = &output.git_checkpoint {
            println!(
                "Git checkpoint: {} ({})",
                style::dim(
                    &checkpoint.git_commit[..std::cmp::min(12, checkpoint.git_commit.len())]
                ),
                style::dim(&checkpoint.committed_at)
            );
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
        println!("Workspace: {}", mode);
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
    if let Some(actor) = &output.actor
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
    if let Some(usage_summary) = &output.usage_summary {
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

fn render_status_advice(output: &StatusOutput) {
    println!();
    if !output.parallel_threads.is_empty() {
        println!(
            "Parallel work: {}",
            style::bold(&output.parallel_threads.len().to_string())
        );
    }
    if let Some(task) = &output.task {
        println!("Task: {}", task);
    }
    println!(
        "Changed paths: {}",
        style::bold(&output.changed_path_count.to_string())
    );
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
        println!("{}", style::warn("Blocked by"));
        for blocker in &output.blockers {
            println!("  - {}", style::warn(blocker));
        }
    }
    if !output.recommended_action.is_empty() {
        println!("Next step: {}", style::bold(&output.recommended_action));
    }
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
    let summaries =
        match repo::thread_manifest::list_thread_manifests(repo.heddle_dir()) {
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

fn render_status_changes(output: &StatusOutput) {
    // Changes
    let has_changes = !output.changes.modified.is_empty()
        || !output.changes.added.is_empty()
        || !output.changes.deleted.is_empty();

    println!();
    if has_changes {
        println!("{}", style::bold("Changes not yet captured"));
        for path in &output.changes.modified {
            println!("  {}: {}", style::warn("modified"), path);
        }
        for path in &output.changes.added {
            println!("  {}:    {}", style::accent("added"), path);
        }
        for path in &output.changes.deleted {
            println!("  {}:  {}", style::error("deleted"), path);
        }
    } else {
        println!("{}", style::dim("Nothing to capture, worktree clean"));
    }
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

    use super::{assess_materialized_threads, MaterializedThreadInfo, render_status_materialized};

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
        let (_repo_dir, _dest_holder, repo) =
            init_repo_with_materialized_thread(b"hello\n");
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