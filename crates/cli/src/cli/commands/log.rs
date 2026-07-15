// SPDX-License-Identifier: Apache-2.0
//! Log command.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use heddle_core::{
    parse_reflog_line, short_oid, status::next_action::canonical_git_import_ref_command,
    summarize_paths, timeline_branch_reason as core_timeline_branch_reason,
    timeline_cursor_reason as core_timeline_cursor_reason, timeline_label as core_timeline_label,
    timeline_recovery_status as core_timeline_recovery_status,
    timeline_tool_status as core_timeline_tool_status, yes_no,
};
use objects::object::{
    Agent, State, StateId, TimelineBranchReason, TimelineCursorMoveReason, TimelineLabel,
    TimelineToolCallStatus,
};
use repo::{
    ChangedPathFilters, HistoryQuery, Repository, TimelineNavigationRecoveryStatus,
    TimelineNavigationSnapshot, TimelineNavigationStep, TimelineStore, format_confidence,
    is_synthetic_root,
};
use serde::Serialize;

use super::{
    action_line::{format_next_step_dim, print_next_step},
    advice::RecoveryAdvice,
    expand::{CollapseAnnotation, collapse_annotations_for_states},
    history_target::resolve_state_id,
    snapshot::ensure_current_state,
    verification_health::{PlainGitVerificationProbe, build_plain_git_verification_probe},
};
use crate::{
    cli::{Cli, should_output_json, style},
    config::UserConfig,
};

#[derive(Clone, Debug)]
pub struct LogCommandOptions {
    pub state: Option<String>,
    pub limit: usize,
    pub all: bool,
    pub graph: bool,
    pub oneline: bool,
    pub reflog: bool,
    pub timeline: bool,
    pub thread: String,
    pub agent: Option<String>,
    pub paths: Vec<String>,
    /// Lower bound for the walk. Resolved through
    /// `resolve_state_id` so it accepts marker names, short
    /// state IDs, or any other spec the state resolver supports. Walk
    /// stops as soon as we encounter this state — the bound itself is
    /// excluded from the output (matches `git log --since`'s
    /// half-open semantics for time bounds).
    pub since: Option<String>,
}

#[derive(Serialize)]
struct LogOutput {
    output_kind: &'static str,
    status: &'static str,
    repository_capability: String,
    storage_model: String,
    states: Vec<StateEntry>,
    /// Carried for the human-readable renderer only. Not part of the
    /// JSON contract: import-hint information is exposed via
    /// `heddle status --output json` instead.
    #[serde(skip)]
    import_guidance: Option<LogImportGuidanceOutput>,
}

#[derive(Serialize)]
struct LogImportGuidanceOutput {
    current_branch: String,
    missing_branch_count: usize,
    missing_branches: Vec<String>,
    recommended_command: String,
}

#[derive(Serialize)]
struct StateEntry {
    state_id: String,
    content_hash: String,
    intent: Option<String>,
    principal: String,
    /// Raw principal name + email so we can render a styled
    /// `name <email>` pair (bold/dim) without re-parsing the
    /// pre-formatted `principal` string. Skipped from JSON
    /// serialization to keep the wire format unchanged — only the
    /// human-readable renderer reads them.
    #[serde(skip)]
    principal_name: String,
    #[serde(skip)]
    principal_email: String,
    agent: Option<String>,
    confidence: Option<f32>,
    created_at: String,
    parents: Vec<String>,
    git_checkpoint: Option<String>,
    collapsed: Option<CollapsedEntry>,
}

#[derive(Serialize)]
struct CollapsedEntry {
    expandable: bool,
    source_count: usize,
}

#[derive(Serialize)]
struct ReflogOutput {
    output_kind: &'static str,
    status: &'static str,
    repository_capability: String,
    storage_model: String,
    entries: Vec<ReflogEntry>,
}

#[derive(Serialize)]
struct TimelineLogOutput {
    output_kind: &'static str,
    status: &'static str,
    repository_capability: String,
    storage_model: String,
    thread: String,
    cursor: TimelineCursorOutput,
    branches: Vec<TimelineBranchOutput>,
    steps: Vec<TimelineStepOutput>,
    active_branch_path: Vec<String>,
    actions: TimelineActionsOutput,
    recovery: Option<TimelineRecoveryOutput>,
}

#[derive(Serialize)]
struct TimelineCursorOutput {
    branch_id: Option<String>,
    step_id: Option<String>,
    state: Option<String>,
    state_full: Option<String>,
}

#[derive(Serialize)]
struct TimelineBranchOutput {
    branch_id: String,
    parent_branch_id: Option<String>,
    forked_from_step_id: Option<String>,
    forked_from_state: Option<String>,
    reason: Option<String>,
    created_at_ms: Option<i64>,
    step_ids: Vec<String>,
    is_active: bool,
    is_on_active_path: bool,
}

#[derive(Serialize)]
struct TimelineStepOutput {
    step_id: String,
    branch_id: String,
    parent_step_id: Option<String>,
    native: Option<TimelineNativeOutput>,
    tool_name: Option<String>,
    status: Option<String>,
    changed: Option<bool>,
    touched_paths: Vec<String>,
    labels: Vec<String>,
    before_state: Option<String>,
    after_state: Option<String>,
    capture_state: Option<String>,
    cursor_state: Option<String>,
    cursor_state_full: Option<String>,
    payload_summary: Option<String>,
    payload_hash: Option<String>,
    capture_oplog_batch_id: Option<u64>,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
    operation_ids: Vec<String>,
    is_current: bool,
    is_on_active_branch_path: bool,
    can_seek: bool,
    can_fork: bool,
    can_reset: bool,
    can_materialize: bool,
    has_boundary_warning: bool,
}

#[derive(Serialize)]
struct TimelineNativeOutput {
    harness: String,
    session_id: Option<String>,
    message_id: Option<String>,
    tool_call_id: String,
}

#[derive(Serialize)]
struct TimelineActionsOutput {
    can_undo: bool,
    can_redo: bool,
}

#[derive(Serialize)]
struct TimelineRecoveryOutput {
    status: String,
    branch_id: String,
    from_step_id: Option<String>,
    to_step_id: Option<String>,
    from_state: String,
    to_state: String,
    reason: String,
    moved_at_ms: i64,
    checkout_state: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct ReflogEntry {
    source: String,
    reference: String,
    old_oid: String,
    new_oid: String,
    actor: String,
    timestamp: Option<String>,
    message: String,
}

impl From<heddle_core::ReflogLine> for ReflogEntry {
    fn from(line: heddle_core::ReflogLine) -> Self {
        Self {
            source: line.source,
            reference: line.reference,
            old_oid: line.old_oid,
            new_oid: line.new_oid,
            actor: line.actor,
            timestamp: line.timestamp,
            message: line.message,
        }
    }
}

impl From<&State> for StateEntry {
    fn from(state: &State) -> Self {
        Self {
            state_id: state.state_id.short(),
            content_hash: state.compute_hash().short(),
            intent: state.intent.clone(),
            principal: state.attribution.principal.to_string(),
            principal_name: state.attribution.principal.name.clone(),
            principal_email: state.attribution.principal.email.clone(),
            agent: state.attribution.agent.as_ref().map(Agent::to_string),
            confidence: state.confidence,
            created_at: state.created_at.format("%Y-%m-%d %H:%M:%S").to_string(),
            parents: state.parents.iter().map(StateId::short).collect(),
            git_checkpoint: None,
            collapsed: None,
        }
    }
}

pub async fn cmd_log(cli: &Cli, options: LogCommandOptions) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let start = cli.repo.as_ref().unwrap_or(&cwd);
    if !options.timeline
        && !options.reflog
        && options.state.is_none()
        && options.since.is_none()
        && options.paths.is_empty()
        && let Some(probe) = build_plain_git_verification_probe(start)?
    {
        return render_plain_git_log(cli, &probe, options.oneline);
    }

    let repo = Repository::open(start)?;

    if options.timeline && options.reflog {
        return Err(anyhow!(RecoveryAdvice::invalid_usage(
            "log_timeline_reflog_conflict",
            "--timeline cannot be combined with --reflog",
            "Choose either `heddle log --timeline` for agent timeline state or `heddle log --reflog` for ref movement history.",
            "heddle log --timeline",
        )));
    }

    if options.timeline {
        return cmd_log_timeline(cli, &repo, &options.thread, options.oneline);
    }

    if options.reflog {
        return cmd_log_reflog(cli, &repo, options.limit, options.oneline);
    }

    if repo.capability() == repo::RepositoryCapability::GitOverlay
        && repo.current_state()?.is_none()
    {
        let revision = options.state.as_deref().unwrap_or("HEAD");
        if ingest::GitSource::open(repo.root())?
            .resolve_history_revision(revision)
            .is_ok()
        {
            return render_unbound_overlay_log(cli, &repo, &options);
        }
    }

    // Get starting state
    let start_id = if let Some(ref spec) = options.state {
        if matches!(spec.as_str(), "HEAD" | "@") && repo.current_state()?.is_none() {
            ensure_current_state(
                &repo,
                &UserConfig::load_default()?,
                Some("Bootstrap git-overlay before viewing log".to_string()),
            )?;
        }
        Some(resolve_state_id(&repo, spec)?)
    } else {
        Some(ensure_current_state(
            &repo,
            &UserConfig::load_default()?,
            Some("Bootstrap git-overlay before viewing log".to_string()),
        )?)
    };

    // Resolve the `--since` bound (if any) and pass it down as the
    // walker's `stop_at`. The bound is honored *before* `--agent` /
    // `--path` filters, so a bound state that itself doesn't match the
    // active filter still terminates the walk correctly. (Previously
    // we applied the bound *after* filtering, which silently leaked
    // matches older than the bound when the bound state was filtered
    // out.) The bound is exclusive — matches git's `--since` shape.
    let since_id = if let Some(ref spec) = options.since {
        Some(resolve_state_id(&repo, spec)?)
    } else {
        None
    };

    let changed_paths = ChangedPathFilters::try_from_paths(options.paths)?;
    let query = HistoryQuery::new(start_id)
        .with_limit(options.limit)
        .with_agent_filter(options.agent)
        .with_changed_paths(changed_paths)
        .with_stop_at(since_id);
    let states = repo.query_history(&query)?;

    // Hide the synthetic genesis root (`heddle init` seed). It carries
    // a stable system attribution rather than the user's principal —
    // see `repo::seed_default_thread`. Surfacing it in user-facing log
    // output would always show the seed state above the user's first
    // real snapshot and report a "Heddle <init@heddle>" or (in older
    // repos) "Unknown" principal that the user never authored.
    let visible_states = states
        .iter()
        .filter(|state| !is_synthetic_root(state))
        .collect::<Vec<_>>();
    let collapsed_annotations =
        collapse_annotations_for_states(&repo, visible_states.iter().map(|state| &state.state_id))?;

    let output = LogOutput {
        output_kind: "log",
        status: "completed",
        repository_capability: repo.capability_label().to_string(),
        storage_model: repo.storage_model_label().to_string(),
        import_guidance: repo
            .git_import_guidance()?
            .map(|hint| LogImportGuidanceOutput {
                current_branch: hint.current_branch,
                missing_branch_count: hint.missing_branch_count,
                missing_branches: hint.missing_branches,
                recommended_command: hint.recommended_command,
            }),
        states: visible_states
            .into_iter()
            .map(|state| {
                let mut entry = StateEntry::from(state);
                entry.git_checkpoint = repo
                    .latest_git_checkpoint_for_state(&state.state_id)
                    .ok()
                    .flatten()
                    .map(|record| record.git_commit);
                entry.collapsed = collapsed_annotations
                    .get(&state.state_id)
                    .copied()
                    .map(CollapsedEntry::from);
                entry
            })
            .collect(),
    };

    let as_json = should_output_json(cli, Some(repo.config()));
    if as_json {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        // Render to stdout via the writer-based helpers so the same
        // formatting logic is unit-testable against a `Vec<u8>` —
        // see `tests::print_oneline_no_ansi_when_disabled`.
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        if options.oneline {
            let _ = write_oneline(&mut handle, &output, cli.verbose > 0);
        } else {
            let _ = write_full(&mut handle, &output, cli.verbose > 0);
        }
    }

    // Discoverability tip after a successful log: nudge toward
    // `heddle query` for filtered history. Once per session per repo.
    crate::cli::tips::maybe_emit(
        repo.root(),
        Some(repo.config()),
        crate::cli::tips::Tip::QueryFromLog,
        as_json,
        cli.quiet,
    );

    Ok(())
}

fn render_unbound_overlay_log(
    cli: &Cli,
    repo: &Repository,
    options: &LogCommandOptions,
) -> Result<()> {
    let revision = options.state.as_deref().unwrap_or("HEAD");
    let history = ingest::OverlayHistory::open(repo.root(), revision)?;
    let start_id = history.tip().map(|(_, state)| state.state_id);
    let since_id = options
        .since
        .as_deref()
        .map(|revision| history.state_id_for_revision(revision))
        .transpose()?;
    let changed_paths = ChangedPathFilters::try_from_paths(options.paths.clone())?;
    let query = HistoryQuery::new(start_id)
        .with_limit(options.limit)
        .with_agent_filter(options.agent.clone())
        .with_changed_paths(changed_paths)
        .with_stop_at(since_id);
    let entries = repo::query_history_from_source(history.source(), &query)?
        .into_iter()
        .map(|state| {
            let mut entry = StateEntry::from(&state);
            entry.git_checkpoint = history
                .git_oid_for_state(&state.state_id)
                .map(str::to_string);
            entry
        })
        .collect();
    let output = LogOutput {
        output_kind: "log",
        status: "completed",
        repository_capability: repo.capability_label().to_string(),
        storage_model: repo.storage_model_label().to_string(),
        import_guidance: repo
            .git_import_guidance()?
            .map(|hint| LogImportGuidanceOutput {
                current_branch: hint.current_branch,
                missing_branch_count: hint.missing_branch_count,
                missing_branches: hint.missing_branches,
                recommended_command: hint.recommended_command,
            }),
        states: entries,
    };
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        if options.oneline {
            let _ = write_oneline(&mut handle, &output, cli.verbose > 0);
        } else {
            let _ = write_full(&mut handle, &output, cli.verbose > 0);
        }
    }
    Ok(())
}

fn render_plain_git_log(cli: &Cli, probe: &PlainGitVerificationProbe, oneline: bool) -> Result<()> {
    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "output_kind": "log",
                "status": "blocked",
                "repository_capability": "plain-git",
                "storage_model": "git",
                "states": [],
                "verification": &probe.trust,
                "recommended_action": &probe.trust.recommended_action,
                "recovery_commands": &probe.trust.recovery_commands,
            }))?
        );
    } else if oneline {
        println!("plain-git Heddle not initialized; next: heddle init");
    } else {
        println!("Git repo, Heddle not initialized");
        if let Some(branch) = &probe.git_branch {
            println!("Git branch: {}", style::bold(branch));
        }
        println!("History: unavailable until this Git repo is initialized and imported");
        print_next_step("heddle init");
        if let Some(branch) = &probe.git_branch {
            println!(
                "Then: {}",
                style::bold(&canonical_git_import_ref_command(branch))
            );
        }
    }
    Ok(())
}

fn cmd_log_timeline(cli: &Cli, repo: &Repository, thread: &str, oneline: bool) -> Result<()> {
    let store = TimelineStore::open(repo.heddle_dir())?;
    let snapshot = repo.timeline_navigation_snapshot(&store, thread)?;
    let output = TimelineLogOutput::from_snapshot(repo, snapshot);

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        if oneline {
            let _ = write_timeline_oneline(&mut handle, &output);
        } else {
            let _ = write_timeline_full(&mut handle, &output, cli.verbose > 0);
        }
    }

    Ok(())
}

fn cmd_log_reflog(cli: &Cli, repo: &Repository, limit: usize, oneline: bool) -> Result<()> {
    let output = ReflogOutput {
        output_kind: "log_reflog",
        status: "completed",
        repository_capability: repo.capability_label().to_string(),
        storage_model: repo.storage_model_label().to_string(),
        entries: collect_reflog_entries(repo.root(), limit)?,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        if oneline {
            let _ = write_reflog_oneline(&mut handle, &output);
        } else {
            let _ = write_reflog_full(&mut handle, &output);
        }
    }

    Ok(())
}

impl TimelineLogOutput {
    fn from_snapshot(repo: &Repository, snapshot: TimelineNavigationSnapshot) -> Self {
        Self {
            output_kind: "timeline_log",
            status: "completed",
            repository_capability: repo.capability_label().to_string(),
            storage_model: repo.storage_model_label().to_string(),
            thread: snapshot.thread,
            cursor: TimelineCursorOutput {
                branch_id: snapshot.cursor.branch_id.map(|id| id.to_string()),
                step_id: snapshot.cursor.step_id.map(|id| id.to_string()),
                state: snapshot.cursor.state.map(|state| state.short()),
                state_full: snapshot.cursor.state.map(|state| state.to_string_full()),
            },
            branches: snapshot
                .branches
                .into_iter()
                .map(|branch| TimelineBranchOutput {
                    branch_id: branch.branch_id.to_string(),
                    parent_branch_id: branch.parent_branch_id.map(|id| id.to_string()),
                    forked_from_step_id: branch.forked_from_step_id.map(|id| id.to_string()),
                    forked_from_state: branch.forked_from_state.map(|state| state.short()),
                    reason: branch.reason.as_ref().map(timeline_branch_reason),
                    created_at_ms: branch.created_at_ms,
                    step_ids: branch.step_ids.iter().map(ToString::to_string).collect(),
                    is_active: branch.is_active,
                    is_on_active_path: branch.is_on_active_path,
                })
                .collect(),
            steps: snapshot
                .steps
                .into_iter()
                .map(TimelineStepOutput::from_step)
                .collect(),
            active_branch_path: snapshot
                .active_branch_path
                .iter()
                .map(ToString::to_string)
                .collect(),
            actions: TimelineActionsOutput {
                can_undo: snapshot.actions.can_undo,
                can_redo: snapshot.actions.can_redo,
            },
            recovery: snapshot.recovery.map(|recovery| TimelineRecoveryOutput {
                status: timeline_recovery_status(recovery.status).to_string(),
                branch_id: recovery.branch_id.to_string(),
                from_step_id: recovery.from_step_id.map(|id| id.to_string()),
                to_step_id: recovery.to_step_id.map(|id| id.to_string()),
                from_state: recovery.from_state.short(),
                to_state: recovery.to_state.short(),
                reason: timeline_cursor_reason(&recovery.reason).to_string(),
                moved_at_ms: recovery.moved_at_ms,
                checkout_state: recovery.checkout_state.map(|state| state.short()),
            }),
        }
    }
}

impl TimelineStepOutput {
    fn from_step(step: TimelineNavigationStep) -> Self {
        Self {
            step_id: step.step_id.to_string(),
            branch_id: step.branch_id.to_string(),
            parent_step_id: step.parent_step_id.map(|id| id.to_string()),
            native: step.native.map(|native| TimelineNativeOutput {
                harness: native.harness,
                session_id: native.session_id,
                message_id: native.message_id,
                tool_call_id: native.tool_call_id,
            }),
            tool_name: step.tool_name,
            status: step.status.as_ref().map(timeline_tool_status),
            changed: step.changed,
            touched_paths: step.touched_paths,
            labels: step.labels.iter().map(timeline_label).collect(),
            before_state: step.before_state.map(|state| state.short()),
            after_state: step.after_state.map(|state| state.short()),
            capture_state: step.capture_state.map(|state| state.short()),
            cursor_state: step.cursor_state.map(|state| state.short()),
            cursor_state_full: step.cursor_state.map(|state| state.to_string_full()),
            payload_summary: step.payload_summary,
            payload_hash: step.payload_hash.map(|hash| hash.short()),
            capture_oplog_batch_id: step.capture_oplog_batch_id,
            started_at_ms: step.started_at_ms,
            finished_at_ms: step.finished_at_ms,
            operation_ids: step
                .operation_ids
                .iter()
                .map(|id| id.to_string_full())
                .collect(),
            is_current: step.is_current,
            is_on_active_branch_path: step.is_on_active_branch_path,
            can_seek: step.can_seek,
            can_fork: step.can_fork,
            can_reset: step.can_reset,
            can_materialize: step.can_materialize,
            has_boundary_warning: step.has_boundary_warning,
        }
    }
}

fn write_timeline_oneline<W: std::io::Write>(
    out: &mut W,
    output: &TimelineLogOutput,
) -> std::io::Result<()> {
    for step in &output.steps {
        writeln!(out, "{}", timeline_step_line(step, false))?;
    }
    Ok(())
}

fn write_timeline_full<W: std::io::Write>(
    out: &mut W,
    output: &TimelineLogOutput,
    verbose: bool,
) -> std::io::Result<()> {
    writeln!(out, "Timeline: {}", style::bold(&output.thread))?;
    writeln!(
        out,
        "Cursor: {} {} {}",
        output.cursor.branch_id.as_deref().unwrap_or("-"),
        output.cursor.step_id.as_deref().unwrap_or("-"),
        output.cursor.state.as_deref().unwrap_or("-")
    )?;
    writeln!(
        out,
        "Actions: undo={} redo={}",
        yes_no(output.actions.can_undo),
        yes_no(output.actions.can_redo)
    )?;
    if let Some(recovery) = &output.recovery {
        writeln!(
            out,
            "Recovery: {} {} -> {}",
            recovery.status, recovery.from_state, recovery.to_state
        )?;
    }

    for branch in &output.branches {
        writeln!(out)?;
        let active = if branch.is_active {
            " current"
        } else if branch.is_on_active_path {
            " path"
        } else {
            ""
        };
        let parent = branch
            .parent_branch_id
            .as_deref()
            .map(|parent| format!(" <- {parent}"))
            .unwrap_or_default();
        writeln!(
            out,
            "{}{}{}",
            style::bold(&branch.branch_id),
            style::dim(&parent),
            style::dim(active)
        )?;

        for step in output
            .steps
            .iter()
            .filter(|step| step.branch_id == branch.branch_id)
        {
            writeln!(out, "  {}", timeline_step_line(step, verbose))?;
            if verbose {
                if let Some(summary) = &step.payload_summary {
                    writeln!(out, "    {}", style::dim(summary))?;
                }
                if !step.labels.is_empty() {
                    writeln!(out, "    labels: {}", style::dim(&step.labels.join(", ")))?;
                }
            }
        }
    }

    Ok(())
}

fn timeline_step_line(step: &TimelineStepOutput, verbose: bool) -> String {
    let marker = if step.is_current { "*" } else { " " };
    let tool = step.tool_name.as_deref().unwrap_or("tool");
    let native = step
        .native
        .as_ref()
        .map(|native| format!("{}:{}", native.harness, native.tool_call_id))
        .unwrap_or_else(|| "-".to_string());
    let state = step.cursor_state.as_deref().unwrap_or("-");
    let status = step.status.as_deref().unwrap_or("-");
    let paths = summarize_paths(&step.touched_paths);

    if verbose {
        format!(
            "{} {} {} {} {} {} {}",
            marker,
            style::state_id(&step.step_id),
            style::dim(&step.branch_id),
            style::bold(tool),
            style::dim(status),
            style::dim(&native),
            style::dim(&format!("{state} {paths}"))
        )
    } else {
        format!(
            "{} {} {} {} {}",
            marker,
            style::state_id(&step.step_id),
            style::bold(tool),
            style::dim(&native),
            style::dim(&format!("{state} {paths}"))
        )
    }
}

fn timeline_label(label: &TimelineLabel) -> String {
    core_timeline_label(label).to_string()
}

fn timeline_tool_status(status: &TimelineToolCallStatus) -> String {
    core_timeline_tool_status(status).to_string()
}

fn timeline_branch_reason(reason: &TimelineBranchReason) -> String {
    core_timeline_branch_reason(reason).to_string()
}

fn timeline_cursor_reason(reason: &TimelineCursorMoveReason) -> &'static str {
    core_timeline_cursor_reason(reason)
}

fn timeline_recovery_status(status: TimelineNavigationRecoveryStatus) -> &'static str {
    core_timeline_recovery_status(status)
}

fn collect_reflog_entries(root: &Path, limit: usize) -> Result<Vec<ReflogEntry>> {
    let mut entries = Vec::new();
    for (source, logs_dir) in reflog_roots(root)? {
        collect_reflog_dir(&source, &logs_dir, &mut entries)
            .with_context(|| format!("reading reflog entries from {}", logs_dir.display()))?;
    }

    entries.sort_by(|a, b| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| a.reference.cmp(&b.reference))
            .then_with(|| a.message.cmp(&b.message))
    });
    entries.truncate(limit);
    Ok(entries)
}

fn reflog_roots(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut roots = Vec::new();

    if let Some(git_dir) = checkout_git_dir(root)? {
        let logs = git_dir.join("logs");
        if logs.is_dir() {
            roots.push(("checkout".to_string(), logs));
        }
    }

    let mirror_logs = root.join(".heddle").join("git").join("logs");
    if mirror_logs.is_dir() {
        roots.push(("mirror".to_string(), mirror_logs));
    }

    Ok(roots)
}

fn checkout_git_dir(root: &Path) -> Result<Option<PathBuf>> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Ok(Some(dot_git));
    }
    if !dot_git.is_file() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&dot_git)
        .with_context(|| format!("reading gitdir pointer {}", dot_git.display()))?;
    let Some(path) = contents.trim().strip_prefix("gitdir:") else {
        return Ok(None);
    };
    let path = PathBuf::from(path.trim());
    if path.is_absolute() {
        Ok(Some(path))
    } else {
        Ok(Some(root.join(path)))
    }
}

fn collect_reflog_dir(source: &str, logs_dir: &Path, entries: &mut Vec<ReflogEntry>) -> Result<()> {
    let mut stack = vec![logs_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(reference) = path.strip_prefix(logs_dir) else {
                continue;
            };
            let reference = reference
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            read_reflog_file(source, &reference, &path, entries)?;
        }
    }
    Ok(())
}

fn read_reflog_file(
    source: &str,
    reference: &str,
    path: &Path,
    entries: &mut Vec<ReflogEntry>,
) -> Result<()> {
    let file = fs::File::open(path)?;
    for line in io::BufReader::new(file).lines() {
        if let Some(entry) = parse_reflog_entry(source, reference, &line?) {
            entries.push(entry);
        }
    }
    Ok(())
}

fn parse_reflog_entry(source: &str, reference: &str, line: &str) -> Option<ReflogEntry> {
    parse_reflog_line(source, reference, line).map(ReflogEntry::from)
}

fn write_reflog_oneline<W: std::io::Write>(
    out: &mut W,
    output: &ReflogOutput,
) -> std::io::Result<()> {
    for entry in &output.entries {
        writeln!(
            out,
            "{} {} {} {}",
            style::dim(&entry.source),
            style::state_id(short_oid(&entry.new_oid)),
            style::dim(&entry.reference),
            style::bold(&entry.message)
        )?;
    }
    Ok(())
}

fn write_reflog_full<W: std::io::Write>(out: &mut W, output: &ReflogOutput) -> std::io::Result<()> {
    writeln!(
        out,
        "Repository: {}",
        crate::cli::render::repository_mode_label(
            &output.repository_capability,
            &output.storage_model
        )
    )?;
    writeln!(out, "Reflog: {} entrie(s)", output.entries.len())?;
    if output.entries.is_empty() {
        if let Some(line) = format_next_step_dim(
            "run `heddle commit` after capturing work, `heddle pull`, or `heddle import git`",
            0,
        ) {
            writeln!(out, "{line}")?;
        }
        return Ok(());
    }

    let mut by_ref: BTreeMap<(&str, &str), Vec<&ReflogEntry>> = BTreeMap::new();
    for entry in &output.entries {
        by_ref
            .entry((&entry.source, &entry.reference))
            .or_default()
            .push(entry);
    }

    for ((source, reference), entries) in by_ref {
        writeln!(out)?;
        writeln!(
            out,
            "{} {}",
            style::bold(reference),
            style::dim(&format!("({source})"))
        )?;
        for entry in entries {
            writeln!(
                out,
                "  {} {} -> {} {}",
                style::dim(entry.timestamp.as_deref().unwrap_or("unknown-time")),
                style::dim(short_oid(&entry.old_oid)),
                style::accent(short_oid(&entry.new_oid)),
                style::bold(&entry.message)
            )?;
            if !entry.actor.is_empty() {
                writeln!(out, "    by {}", style::dim(&entry.actor))?;
            }
        }
    }
    Ok(())
}

fn write_oneline<W: std::io::Write>(
    out: &mut W,
    output: &LogOutput,
    verbose: bool,
) -> std::io::Result<()> {
    for entry in &output.states {
        let intent = entry.intent.as_deref().unwrap_or("(no intent)");
        let checkpoint = if verbose && entry.git_checkpoint.is_some() {
            " [git]"
        } else {
            ""
        };
        let collapsed = if let Some(collapsed) = &entry.collapsed {
            format!(" [collapsed:{}]", collapsed.source_count)
        } else {
            String::new()
        };
        if verbose {
            // Three columns of decreasing emphasis: id (dim,
            // structural), hash (dim, structural), intent (bold, the
            // part you read). Default text hides the content hash
            // because the stable change id is the user-facing anchor.
            writeln!(
                out,
                "{} {} {}{}{}",
                style::state_id(&entry.state_id),
                style::dim(&entry.content_hash),
                style::bold(intent),
                checkpoint,
                style::dim(&collapsed),
            )?;
        } else {
            writeln!(
                out,
                "{} {}{}",
                style::state_id(&entry.state_id),
                style::bold(intent),
                style::dim(&collapsed),
            )?;
        }
    }
    Ok(())
}

fn write_full<W: std::io::Write>(
    out: &mut W,
    output: &LogOutput,
    verbose: bool,
) -> std::io::Result<()> {
    // The mode preamble is diagnostic noise on the common read path
    // (heddle#275); `heddle status` already exposes it. Keep it under
    // `-v` for troubleshooting.
    let mut wrote_header = false;
    if verbose {
        writeln!(
            out,
            "Repository: {}",
            crate::cli::render::repository_mode_label(
                &output.repository_capability,
                &output.storage_model
            )
        )?;
        wrote_header = true;
    }
    if let Some(hint) = &output.import_guidance {
        writeln!(
            out,
            "{}",
            crate::cli::render::git_only_branch_summary(
                &hint.missing_branches,
                hint.missing_branch_count,
            )
        )?;
        if let Some(line) = format_next_step_dim(&hint.recommended_command, 0) {
            writeln!(out, "{line}")?;
        }
        wrote_header = true;
    }
    // Only emit the spacer when a header preceded it; otherwise it would be
    // an orphaned leading blank line (heddle#275 r2).
    if wrote_header {
        writeln!(out)?;
    }
    for (i, entry) in output.states.iter().enumerate() {
        if i > 0 {
            writeln!(out)?;
        }

        if verbose {
            // Header: change-id and tree hash dim, timestamp dim.
            // Nothing carries semantic color here — these are
            // structurally important but not the focus.
            writeln!(
                out,
                "{} ({}) {}",
                style::state_id(&entry.state_id),
                style::dim(&entry.content_hash),
                style::dim(&entry.created_at),
            )?;
        } else {
            writeln!(
                out,
                "{} {}",
                style::state_id(&entry.state_id),
                style::dim(&entry.created_at),
            )?;
        }

        if let Some(intent) = &entry.intent {
            // Intent is the editorial line — bold, no color.
            writeln!(out, "  {}", style::bold(intent))?;
        }
        if let Some(collapsed) = &entry.collapsed {
            writeln!(
                out,
                "  {}",
                style::dim(&format!(
                    "Collapsed: expandable with `heddle expand {}` ({} captures)",
                    entry.state_id, collapsed.source_count
                ))
            )?;
        }

        if verbose {
            writeln!(
                out,
                "  Principal: {}",
                style::principal(&entry.principal_name, &entry.principal_email)
            )?;
        }

        if verbose && let Some(agent) = &entry.agent {
            writeln!(out, "  Agent: {}", style::dim(agent))?;
        }

        // Confidence only renders when set — the per-entry "Confidence: —"
        // sentinel was high-noise on long logs and conveyed "value absent"
        // information the absence-of-line itself already encodes. JSON output
        // still carries `confidence: null` so agents can distinguish.
        if entry.confidence.is_some() {
            let confidence_text = format_confidence(entry.confidence);
            writeln!(
                out,
                "  Confidence: {}",
                style::confidence(entry.confidence, &confidence_text)
            )?;
        }
        // Durability only renders when there's something to say — a Git
        // checkpoint binds the capture into Git history. The "Capture
        // durability: local only" fallback used to repeat on every entry and
        // told the user nothing they couldn't infer from the absence of a
        // Git-checkpoint line.
        if verbose && let Some(git_checkpoint) = &entry.git_checkpoint {
            writeln!(
                out,
                "  Git checkpoint: {}",
                style::dim(&git_checkpoint[..std::cmp::min(12, git_checkpoint.len())])
            )?;
        }
    }
    Ok(())
}

impl From<CollapseAnnotation> for CollapsedEntry {
    fn from(annotation: CollapseAnnotation) -> Self {
        Self {
            expandable: true,
            source_count: annotation.source_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    fn sample_entry() -> StateEntry {
        StateEntry {
            state_id: "hs-abc123".to_string(),
            content_hash: "deadbeef".to_string(),
            intent: Some("Capture audit pipeline".to_string()),
            principal: "Ada <ada@example.com>".to_string(),
            principal_name: "Ada".to_string(),
            principal_email: "ada@example.com".to_string(),
            agent: Some("anthropic/claude-opus-4".to_string()),
            confidence: Some(0.95),
            created_at: "2026-05-01 12:00:00".to_string(),
            parents: vec![],
            git_checkpoint: Some("abc123def456".to_string()),
            collapsed: None,
        }
    }

    fn sample_timeline_output() -> TimelineLogOutput {
        TimelineLogOutput {
            output_kind: "timeline_log",
            status: "completed",
            repository_capability: "git-overlay".to_string(),
            storage_model: "git+heddle-sidecar".to_string(),
            thread: "main".to_string(),
            cursor: TimelineCursorOutput {
                branch_id: Some("tlb-main".to_string()),
                step_id: Some("tls-two".to_string()),
                state: Some("hs-cursor".to_string()),
                state_full: Some("hs-cursor-full".to_string()),
            },
            branches: vec![TimelineBranchOutput {
                branch_id: "tlb-main".to_string(),
                parent_branch_id: None,
                forked_from_step_id: None,
                forked_from_state: None,
                reason: Some("explicit-fork".to_string()),
                created_at_ms: Some(1),
                step_ids: vec!["tls-one".to_string(), "tls-two".to_string()],
                is_active: true,
                is_on_active_path: true,
            }],
            steps: vec![
                TimelineStepOutput {
                    step_id: "tls-one".to_string(),
                    branch_id: "tlb-main".to_string(),
                    parent_step_id: None,
                    native: Some(TimelineNativeOutput {
                        harness: "opencode".to_string(),
                        session_id: Some("session-1".to_string()),
                        message_id: Some("message-1".to_string()),
                        tool_call_id: "call-1".to_string(),
                    }),
                    tool_name: Some("shell".to_string()),
                    status: Some("succeeded".to_string()),
                    changed: Some(true),
                    touched_paths: vec!["src/one.rs".to_string()],
                    labels: vec!["repo-reversible".to_string()],
                    before_state: Some("hs-before".to_string()),
                    after_state: Some("hs-one".to_string()),
                    capture_state: Some("hs-one".to_string()),
                    cursor_state: Some("hs-one".to_string()),
                    cursor_state_full: Some("hs-one-full".to_string()),
                    payload_summary: Some("first call".to_string()),
                    payload_hash: None,
                    capture_oplog_batch_id: Some(1),
                    started_at_ms: None,
                    finished_at_ms: Some(2),
                    operation_ids: vec!["hto-one".to_string()],
                    is_current: false,
                    is_on_active_branch_path: true,
                    can_seek: true,
                    can_fork: true,
                    can_reset: true,
                    can_materialize: true,
                    has_boundary_warning: false,
                },
                TimelineStepOutput {
                    step_id: "tls-two".to_string(),
                    branch_id: "tlb-main".to_string(),
                    parent_step_id: Some("tls-one".to_string()),
                    native: Some(TimelineNativeOutput {
                        harness: "opencode".to_string(),
                        session_id: Some("session-1".to_string()),
                        message_id: Some("message-1".to_string()),
                        tool_call_id: "call-2".to_string(),
                    }),
                    tool_name: Some("edit".to_string()),
                    status: Some("succeeded".to_string()),
                    changed: Some(true),
                    touched_paths: vec!["src/two.rs".to_string()],
                    labels: vec!["repo-reversible".to_string()],
                    before_state: Some("hs-one".to_string()),
                    after_state: Some("hs-cursor".to_string()),
                    capture_state: Some("hs-cursor".to_string()),
                    cursor_state: Some("hs-cursor".to_string()),
                    cursor_state_full: Some("hs-cursor-full".to_string()),
                    payload_summary: Some("second call".to_string()),
                    payload_hash: None,
                    capture_oplog_batch_id: Some(2),
                    started_at_ms: None,
                    finished_at_ms: Some(3),
                    operation_ids: vec!["hto-two".to_string()],
                    is_current: true,
                    is_on_active_branch_path: true,
                    can_seek: true,
                    can_fork: true,
                    can_reset: true,
                    can_materialize: true,
                    has_boundary_warning: false,
                },
            ],
            active_branch_path: vec!["tlb-main".to_string()],
            actions: TimelineActionsOutput {
                can_undo: true,
                can_redo: false,
            },
            recovery: None,
        }
    }

    /// Render-site smoke test: with color disabled, neither the
    /// oneline nor the full renderer must emit any ANSI escape.
    /// This is the integration check that `style::*` helpers
    /// short-circuit to plain text — if a future helper forgets
    /// the gate, this test catches the leak before it reaches a
    /// user pipe or test snapshot.
    #[test]
    #[serial(color_state)]
    fn render_sites_no_ansi_when_disabled() {
        style::force_for_test(false);
        let output = LogOutput {
            output_kind: "log",
            status: "completed",
            repository_capability: "git-overlay".to_string(),
            storage_model: "git+heddle-sidecar".to_string(),
            import_guidance: None,
            states: vec![sample_entry()],
        };

        let mut buf = Vec::new();
        write_oneline(&mut buf, &output, false).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains('\x1b'), "oneline leaked ANSI: {s:?}");
        assert!(s.contains("hs-abc123"));
        assert!(s.contains("Capture audit pipeline"));

        let mut buf = Vec::new();
        write_full(&mut buf, &output, false).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains('\x1b'), "full leaked ANSI: {s:?}");
        assert!(!s.contains("Ada <ada@example.com>"));
        assert!(s.contains("Confidence: 0.95"));

        let mut buf = Vec::new();
        write_full(&mut buf, &output, true).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Ada <ada@example.com>"));
    }

    /// With color enabled, the same renderer must emit escapes —
    /// otherwise the gate is stuck off and we'd silently ship a
    /// monochrome CLI. This pairs with the negative test above to
    /// pin both directions of the gate.
    #[test]
    #[serial(color_state)]
    fn render_sites_emit_ansi_when_enabled() {
        style::force_for_test(true);
        let output = LogOutput {
            output_kind: "log",
            status: "completed",
            repository_capability: "git-overlay".to_string(),
            storage_model: "git+heddle-sidecar".to_string(),
            import_guidance: None,
            states: vec![sample_entry()],
        };
        let mut buf = Vec::new();
        write_oneline(&mut buf, &output, true).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains('\x1b'), "expected ANSI in oneline: {s:?}");
    }

    #[test]
    #[serial(color_state)]
    fn timeline_renderer_marks_current_step_without_ansi_when_disabled() {
        style::force_for_test(false);
        let output = sample_timeline_output();

        let mut buf = Vec::new();
        write_timeline_oneline(&mut buf, &output).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains('\x1b'), "timeline oneline leaked ANSI: {s:?}");
        assert!(s.contains("* tls-two"));
        assert!(s.contains("opencode:call-2"));

        let mut buf = Vec::new();
        write_timeline_full(&mut buf, &output, true).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Timeline: main"));
        assert!(s.contains("Actions: undo=yes redo=no"));
        assert!(s.contains("labels: repo-reversible"));
    }

    /// The default full text view must lead with log data, not the
    /// `Repository:` mode preamble — that line is noise on every read
    /// (heddle#275). `-v` keeps it for diagnostic context.
    #[test]
    #[serial(color_state)]
    fn write_full_gates_repository_preamble_on_verbose() {
        style::force_for_test(false);
        let output = LogOutput {
            output_kind: "log",
            status: "completed",
            repository_capability: "git-overlay".to_string(),
            storage_model: "git+heddle-sidecar".to_string(),
            import_guidance: None,
            states: vec![sample_entry()],
        };

        let mut buf = Vec::new();
        write_full(&mut buf, &output, false).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            !s.contains("Repository:"),
            "default full log leaked the mode preamble: {s:?}"
        );
        // Dropping the preamble must also drop the spacer that followed it;
        // the default view must lead with log data, not a blank line
        // (heddle#275 r2).
        assert!(
            !s.starts_with('\n'),
            "default full log starts with an orphaned blank line: {s:?}"
        );

        let mut buf = Vec::new();
        write_full(&mut buf, &output, true).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.contains("Repository:"),
            "verbose full log should retain the mode preamble: {s:?}"
        );
        // With the preamble present, the spacer separating it from the log
        // entries is still expected.
        assert!(
            s.contains("\n\n"),
            "verbose full log should keep the spacer after the preamble: {s:?}"
        );
    }
}
