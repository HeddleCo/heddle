// SPDX-License-Identifier: Apache-2.0
//! Status command.

use std::{
    io::{self, IsTerminal, Write},
    path::Path,
    time::Instant,
};

use anyhow::Result;
#[cfg(feature = "client")]
use futures::{SinkExt, StreamExt};
use heddle_core::{
    ChangesInfo, CoordinationStatus, FastShortStatusReport, GitIndexPlan as CoreGitIndexPlan,
    MachineContractInput, MaterializedThreadInfo, PlainGitStatusReport, StatusDetail,
    StatusOptions, StatusReport as StatusOutput, changes_paths, coordination_label,
    fast_short_status_report, human_thread_health, plain_git_status_report, status as core_status,
    status_combined_verdict,
};
use repo::{
    RepoConfig, Repository, ThreadFreshness, ThreadMode, ThreadState, WorktreeCompareProfile,
};
#[cfg(feature = "client")]
use serde::Deserialize;
#[cfg(feature = "client")]
use serde::Serialize;
use sley::Repository as SleyRepository;
use tokio::time::{Duration, sleep};
#[cfg(feature = "client")]
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::header::AUTHORIZATION, protocol::Message},
};
use tracing::debug;

use super::{
    action_line::print_command,
    next_action::{NextActionValidationContext, write_command_json},
    verification_health::repository_setup_guidance,
};
#[cfg(feature = "client")]
use crate::config::UserConfig;
use crate::{
    cli::{Cli, output_is_compact, should_output_json, style, worktree_status_options},
    perf::{ProfileField, ProfileMode, emit_profile, profile_enabled, profile_mode},
};

fn emit_status_worktree_profile(profile: Option<&WorktreeCompareProfile>) {
    let Some(profile) = profile else {
        return;
    };
    let fields = [
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
    ];
    match profile_mode() {
        ProfileMode::Off => {}
        ProfileMode::Human => emit_profile("status worktree", &fields),
        ProfileMode::Jsonl => emit_profile("status worktree detail", &fields),
    }
}

pub async fn cmd_status(
    cli: &Cli,
    short: bool,
    watch: bool,
    watch_iterations: Option<usize>,
    watch_interval_ms: Option<u64>,
) -> Result<()> {
    if let Some(start) = existing_heddle_repository_start(cli)? {
        let repo = Repository::open(start)?;
        super::workflow::recover_incomplete_land_if_present(&repo)?;
    }
    if watch {
        return watch_status(cli, short, watch_iterations, watch_interval_ms).await;
    }
    if short
        && cli.verbose == 0
        && let Some(output) = try_fast_short_status_report(cli)?
    {
        render_fast_short_status(&output);
        emit_fast_short_status_profile(&output);
        return Ok(());
    }
    if let Some(output) = build_plain_git_status_probe(cli)? {
        render_plain_git_status(cli, &output, short)?;
        return Ok(());
    }
    let output = build_status_command_output(cli, short)?;
    render_status(cli, &output.report, short, output.render_json)?;
    Ok(())
}

/// Locate an already-initialized Heddle checkout without invoking
/// `Repository::open` on a plain Git tree (that API intentionally bootstraps
/// mutating callers). Stop at the first Git boundary so a nested plain Git
/// repository is never mistaken for its parent's Heddle checkout.
fn existing_heddle_repository_start(cli: &Cli) -> Result<Option<std::path::PathBuf>> {
    let start = match cli.repo.as_ref() {
        Some(path) => path.clone(),
        None => std::env::current_dir()?,
    };
    let start = start.canonicalize()?;
    for ancestor in start.ancestors() {
        if ancestor.join(".heddle").is_dir() {
            return Ok(Some(start));
        }
        if ancestor.join(".git").exists() {
            return Ok(None);
        }
    }
    Ok(None)
}

fn try_fast_short_status_report(cli: &Cli) -> Result<Option<FastShortStatusReport>> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    let repo_config = fast_short_repo_config(start)?;
    if should_output_json(cli, repo_config.as_ref()) {
        return Ok(None);
    }
    Ok(fast_short_status_report(start)?)
}

fn fast_short_repo_config(start: &Path) -> Result<Option<RepoConfig>> {
    let Ok(git) = SleyRepository::discover(start) else {
        return Ok(None);
    };
    let Some(workdir) = git.workdir() else {
        return Ok(None);
    };
    let config_path = workdir.join(".heddle").join("config.toml");
    if config_path.is_file() {
        Ok(Some(RepoConfig::load_for_repository(&config_path)?))
    } else {
        Ok(None)
    }
}

fn render_fast_short_status(output: &FastShortStatusReport) {
    render_short_changes(&output.changes);
    if output.changes.is_empty() {
        println!(
            "{} {}",
            style::bold(&output.subject),
            style::thread_state(&output.health)
        );
    }
}

fn emit_fast_short_status_profile(output: &FastShortStatusReport) {
    if profile_enabled() {
        emit_profile(
            "status fast short",
            &[
                ProfileField::millis("git_discover_ms", output.profile.git_discover_ms),
                ProfileField::millis("config_ms", output.profile.config_ms),
                ProfileField::millis("sley_status_ms", output.profile.sley_status_ms),
                ProfileField::millis("branch_ms", output.profile.branch_ms),
                ProfileField::millis("remote_ms", output.profile.remote_ms),
                ProfileField::millis("total_ms", output.profile.total_ms),
            ],
        );
    }
}

pub(crate) fn prompt_segment(cli: &Cli) -> Result<Option<String>> {
    let Ok(output) = build_status_output(cli, true) else {
        return Ok(None);
    };
    // Short status already carries the current lane on `output.thread` from
    // the single open in `build_status_command_output` — do not re-open.
    let subject = output
        .thread
        .as_deref()
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

fn build_plain_git_status_probe(cli: &Cli) -> Result<Option<PlainGitStatusReport>> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    Ok(plain_git_status_report(
        start,
        &MachineContractInput::from_coverage(
            super::verification_health::machine_contract_coverage(),
        ),
    )?)
}

fn render_plain_git_status(cli: &Cli, output: &PlainGitStatusReport, short: bool) -> Result<()> {
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
    Ok(build_status_command_output(cli, short)?.report)
}

struct StatusCommandOutput {
    report: StatusOutput,
    render_json: bool,
}

fn build_status_command_output(cli: &Cli, short: bool) -> Result<StatusCommandOutput> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd).to_path_buf();
    // Single open for the whole command: inject into ExecutionContext so core
    // does not re-open, and attribute open cost here for truthful profiles.
    let repo_open_start = Instant::now();
    let repo = Repository::open(&start)?;
    let cli_repo_open_ms = repo_open_start.elapsed().as_millis();
    let repo_config = repo.config().clone();
    let as_json = should_output_json(cli, Some(&repo_config));
    let compact_json = as_json && output_is_compact(cli);
    let detail = if short && !as_json {
        StatusDetail::ShortText
    } else if compact_json || (short && as_json) {
        StatusDetail::CompactMachine
    } else if cli.verbose > 0 {
        StatusDetail::Full
    } else {
        StatusDetail::DefaultText
    };
    let status_options = worktree_status_options(Some(&repo_config));
    let ctx = heddle_core::ExecutionContext::builder()
        .start_path(start.clone())
        .repo(repo)
        .build();
    let mut output = core_status(
        &ctx,
        StatusOptions::new(detail, status_options)
            .with_start_path(start)
            .with_machine_contract_input(MachineContractInput::from_coverage(
                super::verification_health::machine_contract_coverage(),
            )),
    )?;
    // Core reports 0 for injected repos; fold the shell open into the profile
    // so `repo_open_ms` reflects real work on this path.
    output.profile.repo_open_ms += cli_repo_open_ms;
    debug!(
        repo_open_ms = output.profile.repo_open_ms,
        body_ms = output.profile.build_total_ms,
        total_ms = output.profile.repo_open_ms + output.profile.build_total_ms,
        "Status command complete"
    );
    emit_status_profile(&output);
    Ok(StatusCommandOutput {
        report: output,
        render_json: as_json,
    })
}

fn cli_coordination_status(status: CoordinationStatus) -> super::thread::CoordinationStatus {
    match status {
        CoordinationStatus::Clean => super::thread::CoordinationStatus::Clean,
        CoordinationStatus::Ahead => super::thread::CoordinationStatus::Ahead,
        CoordinationStatus::Diverged => super::thread::CoordinationStatus::Diverged,
        CoordinationStatus::Blocked => super::thread::CoordinationStatus::Blocked,
        CoordinationStatus::MergeReady => super::thread::CoordinationStatus::MergeReady,
    }
}

fn emit_status_profile(output: &StatusOutput) {
    if !profile_enabled() {
        return;
    }
    emit_status_worktree_profile(output.profile.worktree_profile.as_ref());
    let fields = [
        ProfileField::millis("repo_open_ms", output.profile.repo_open_ms),
        ProfileField::millis("current_state_ms", output.profile.current_state_ms),
        ProfileField::millis("operation_ms", output.profile.operation_ms),
        ProfileField::millis("remote_tracking_ms", output.profile.remote_tracking_ms),
        ProfileField::millis("import_hint_ms", output.profile.import_hint_ms),
        ProfileField::millis(
            "git_overlay_status_ms",
            output.profile.git_overlay_status_ms,
        ),
        ProfileField::millis("verification_ms", output.profile.verification_ms),
        ProfileField::millis("git_index_ms", output.profile.git_index_ms),
        ProfileField::millis("worktree_status_ms", output.profile.worktree_status_ms),
        ProfileField::millis("thread_summary_ms", output.profile.thread_summary_ms),
        ProfileField::millis("parallel_threads_ms", output.profile.parallel_threads_ms),
        ProfileField::millis("late_state_ms", output.profile.late_state_ms),
        ProfileField::millis(
            "materialized_threads_ms",
            output.profile.materialized_threads_ms,
        ),
        ProfileField::millis("advice_ms", output.profile.advice_ms),
        ProfileField::millis("build_total_ms", output.profile.build_total_ms),
    ];
    match profile_mode() {
        ProfileMode::Off => {}
        ProfileMode::Human => emit_profile("status phases", &fields),
        ProfileMode::Jsonl => emit_profile_status_jsonl_phases(output),
    }
}

fn emit_profile_status_jsonl_phases(output: &StatusOutput) {
    emit_profile(
        "status repo open",
        &[ProfileField::millis(
            "repo_open_ms",
            output.profile.repo_open_ms,
        )],
    );
    emit_profile(
        "status current state",
        &[ProfileField::millis(
            "current_state_ms",
            output.profile.current_state_ms,
        )],
    );
    emit_profile(
        "status operation",
        &[ProfileField::millis(
            "operation_ms",
            output.profile.operation_ms,
        )],
    );
    emit_profile(
        "status remote tracking",
        &[ProfileField::millis(
            "remote_tracking_ms",
            output.profile.remote_tracking_ms,
        )],
    );
    emit_profile(
        "status import hint",
        &[ProfileField::millis(
            "import_hint_ms",
            output.profile.import_hint_ms,
        )],
    );
    emit_profile(
        "status git overlay status",
        &[ProfileField::millis(
            "git_overlay_status_ms",
            output.profile.git_overlay_status_ms,
        )],
    );
    emit_profile(
        "status verification",
        &[ProfileField::millis(
            "verification_ms",
            output.profile.verification_ms,
        )],
    );
    emit_profile(
        "status git index",
        &[ProfileField::millis(
            "git_index_ms",
            output.profile.git_index_ms,
        )],
    );
    emit_profile(
        "status worktree status",
        &[ProfileField::millis(
            "worktree_status_ms",
            output.profile.worktree_status_ms,
        )],
    );
    emit_profile(
        "status thread summary",
        &[ProfileField::millis(
            "thread_summary_ms",
            output.profile.thread_summary_ms,
        )],
    );
    emit_profile(
        "status parallel threads",
        &[ProfileField::millis(
            "parallel_threads_ms",
            output.profile.parallel_threads_ms,
        )],
    );
    emit_profile(
        "status late state",
        &[ProfileField::millis(
            "late_state_ms",
            output.profile.late_state_ms,
        )],
    );
    emit_profile(
        "status materialized threads",
        &[ProfileField::millis(
            "materialized_threads_ms",
            output.profile.materialized_threads_ms,
        )],
    );
    emit_profile(
        "status advice",
        &[ProfileField::millis("advice_ms", output.profile.advice_ms)],
    );
    emit_profile(
        "status build total",
        &[ProfileField::millis(
            "build_total_ms",
            output.profile.build_total_ms,
        )],
    );
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
        compact.coordination_status = Some(cli_coordination_status(self.coordination_status));
        compact.blockers = self.blockers.clone();
        compact.next_action = next_action;
        compact.next_action_template = next_action_template;
        compact.changed_paths = Some(self.changed_paths.clone());
        compact.changed_path_count = Some(self.changed_paths.len());
        compact
    }
}

impl super::compact::CompactProjection for PlainGitStatusReport {
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

pub(crate) fn render_status(
    cli: &Cli,
    output: &StatusOutput,
    short: bool,
    render_json: bool,
) -> Result<()> {
    let render_start = Instant::now();
    if render_json {
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
        let output = build_status_command_output(cli, short)?;
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
        render_status(cli, &output.report, short, output.render_json)?;
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
    if matches!(output.recommended_action.as_str(), "heddle push")
        && output.thread_health == "clean"
    {
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

fn render_short_plain_git_status(output: &PlainGitStatusReport) {
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
    if let Some(hint) = &output.import_guidance
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
    if !output.verification_health.clean {
        let label = if matches!(
            output.verification_health.status.as_str(),
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
                style::warn(&output.verification_health.summary)
            );
        }
        if output.verification_health.status == "needs_import"
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
    let verdict = status_combined_verdict(
        &output.thread_health,
        output.coordination_status,
        output.coordination_blocked_by_trust,
    );
    println!("Verdict: {}", style::thread_state(&verdict.word));
    if let Some(reason) = verdict.reason {
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
                style::state_id(&state.state_id),
                style::dim(&state.content_hash)
            );
        } else {
            println!("Saved change: {}", style::state_id(&state.state_id));
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
        || output.import_guidance.as_ref().is_some_and(|hint| {
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
        Some("run `heddle land --thread <thread>` after readiness passes")
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
    if blocker.starts_with("Verification: ") && blocker.contains("Heddle worktree path(s)") {
        return blocker.to_string();
    }
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

#[cfg(test)]
fn assess_materialized_threads(repo: &Repository) -> Vec<MaterializedThreadInfo> {
    heddle_core::assess_materialized_threads(repo)
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

fn git_index_has_paths(index: &CoreGitIndexPlan) -> bool {
    !index.staged_paths.is_empty()
        || !index.unstaged_paths.is_empty()
        || !index.untracked_paths.is_empty()
}

fn render_git_index_status(index: &CoreGitIndexPlan) {
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
            style::bold("heddle capture -m \"...\" && heddle commit -m \"...\"")
        );
    }
}

fn git_index_extra_path_label(index: &CoreGitIndexPlan, kind: &'static str) -> String {
    if index.commit_mode == "staged_index" {
        format!("will leave {kind} paths")
    } else {
        format!("will commit {kind} paths")
    }
}

fn git_index_commit_scope_text(index: &CoreGitIndexPlan) -> &'static str {
    match index.commit_mode {
        "staged_index" => "`heddle commit` records the captured Git state",
        "worktree_all" => {
            "capture records Heddle provenance; `heddle commit` records source history"
        }
        "worktree_all_explicit" => "capture first, then stage and commit the intended Git paths",
        "none" => "no Git paths are ready to commit",
        _ => "capture Heddle provenance, then commit source history with Git",
    }
}

fn human_coordination_status(output: &StatusOutput) -> String {
    coordination_label(
        &output.coordination_status,
        output.coordination_blocked_by_trust,
    )
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
    use heddle_core::{
        ActorInfo, ChangesInfo, PlainGitStatusReport, RepositoryVerificationState,
        repository_mode_label,
    };
    use repo::{AgentUsageSummary, Repository};
    use serde_json::Value;
    use tempfile::TempDir;

    use super::{
        MaterializedThreadInfo, assess_materialized_threads, build_status_output,
        render_status_materialized,
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

    #[test]
    fn status_serializes_agent_context_fields_when_set() {
        let repo_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(repo_dir.path()).unwrap();
        fs::write(repo_dir.path().join("hello.txt"), b"hello\n").unwrap();
        repo.snapshot(Some("seed".into()), None).unwrap();

        let cli = status_cli(repo_dir.path());
        let mut output = build_status_output(&cli, false).expect("build status output");
        output.path = Some(repo_dir.path().display().to_string());
        output.execution_path = Some(repo_dir.path().display().to_string());
        output.session_id = Some("agent-session-1".to_string());
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
        output.attach_reason = Some("matched native actor identity".to_string());
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
        assert_eq!(json["session_id"], "agent-session-1");
        assert_eq!(json["heddle_session_id"], "heddle-session-1");
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
        assert_ne!(snap.state_id, mat.state_id);

        let infos = assess_materialized_threads(&repo);
        assert_eq!(infos.len(), 1);
        assert!(
            infos[0].stale,
            "manifest still names mat.state_id but main head is at snap.state_id → stale"
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

    /// Action-field presence contract (HeddleCo/heddle#645): empty
    /// `recommended_action` serializes as `null` via core's
    /// `PlainGitStatusReport` (assembly + serde live in heddle-core).
    #[test]
    fn plain_git_status_serializes_empty_recommended_action_as_null() {
        let machine_contract_coverage =
            crate::cli::commands::verification_health::machine_contract_coverage();
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
            machine_contract: crate::cli::commands::verification_health::machine_contract_status(
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
        let output = PlainGitStatusReport {
            output_kind: "status",
            repository_capability: "plain-git".to_string(),
            repository_label: repository_mode_label("plain-git", "git-only"),
            storage_model: "git-only".to_string(),
            heddle_initialized: false,
            git_branch: Some("main".to_string()),
            path: "/tmp/repo".to_string(),
            recommended_action: trust.recommended_action.clone(),
            recommended_action_template: trust.recommended_action_template.clone(),
            recovery_commands: trust.recovery_commands.clone(),
            recovery_action_templates: trust.recovery_action_templates.clone(),
            thread_health: trust.status.clone(),
            changed_path_count: 0,
            changes: ChangesInfo::default(),
            git_index: None,
            trust,
        };

        let value = serde_json::to_value(&output).unwrap();
        assert!(value["recommended_action"].is_null());
        assert!(value["verification"]["recommended_action"].is_null());
    }
}
