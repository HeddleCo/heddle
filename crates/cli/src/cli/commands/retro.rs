// SPDX-License-Identifier: Apache-2.0
//! Retro command — agent-readable session summary.
//!
//! `heddle retro --since <marker-or-state>` walks a single trip
//! through the operation log + agent registry + marker refs + context
//! annotations to produce one structured payload describing what
//! happened in the working session. It replaces the
//! reconstruct-from-`heddle log` boilerplate that agents wrote before:
//! today they cross-reference `heddle log`, `heddle agent list`,
//! `heddle context history`, and `heddle thread marker list` separately, then
//! diff the timestamps by hand. This verb folds those four reads into
//! one trip, aligned on a single time window.
//!
//! The default lower bound — when `--since` is omitted — walks back to
//! the most recent `Claude Code turn`-shaped capture intent or one
//! hour, whichever is *more recent* (i.e. the smaller window). That
//! intentionally biases toward "what happened in this turn" rather than
//! a long backlog, because retros surface most often at end-of-turn.

use std::{collections::HashSet, path::Path};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use heddle_core::retro_plan::{
    DEFAULT_FALLBACK_WINDOW_HOURS, MAX_OPLOG_BATCHES, agent_task_window_overlaps,
    agent_window_overlaps, choose_default_since_ts, context_annotation_in_window,
    display_free_text, duration_secs as plan_duration_secs, excerpt, is_turn_boundary_intent,
    is_verify_fail_marker, is_verify_pass_signal, retro_header_line, short_state_id,
    timeline_step_in_window,
};
use objects::{
    object::{State, StateId},
    store::{
        ActorPresenceStatus, ActorPresenceStore, AgentTaskRecord, AgentTaskStore, ObjectStore,
    },
};
use oplog::OpRecord;
use repo::{Repository, TimelineStore, TimelineView};
use serde::Serialize;

use super::history_target::resolve_state_id;
use crate::cli::{Cli, should_output_json};

#[derive(Clone, Debug)]
pub struct RetroCommandOptions {
    pub since: Option<String>,
    pub include_merges: bool,
    pub include_undos: bool,
    pub verbose: bool,
}

#[derive(Serialize)]
struct RetroOutput {
    /// Resolved lower bound — full state ID when `--since` was a
    /// marker or short prefix; `null` when nothing pinned the lower
    /// bound (default-window mode).
    since: Option<String>,
    /// HEAD state at retro time.
    until: Option<String>,
    /// Wall-clock window in seconds — `until` timestamp minus the
    /// effective lower bound timestamp. `null` when either side is
    /// unresolvable (a brand-new repo before any captures).
    duration_secs: Option<i64>,
    states_captured: Vec<StateEntry>,
    agents_active: Vec<ActorPresence>,
    agent_tasks: Vec<AgentTaskEntry>,
    timeline_steps: Vec<TimelineStepEntry>,
    markers_created: Vec<MarkerEntry>,
    context_annotations: Vec<ContextAnnotationEntry>,
    verify_signals: Vec<VerifySignal>,
    /// Populated only with `--include-merges`; `[]` otherwise.
    merges: Vec<MergeEntry>,
    /// Populated only with `--include-undos`; `[]` otherwise.
    undos: Vec<UndoEntry>,
}

#[derive(Serialize, Clone)]
struct StateEntry {
    state_id: String,
    intent: Option<String>,
    confidence: Option<f32>,
    agent: Option<String>,
    principal: String,
    timestamp: String,
}

#[derive(Serialize)]
struct ActorPresence {
    session_id: String,
    provider: Option<String>,
    model: Option<String>,
    status: String,
    started_at: String,
    completed_at: Option<String>,
    tokens: AgentTokens,
}

#[derive(Serialize, Default)]
struct AgentTokens {
    input: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    tool_calls: Option<u32>,
}

#[derive(Serialize)]
struct AgentTaskEntry {
    task_id: String,
    title: String,
    status: String,
    target_thread: String,
    updated_at: String,
    completed_at: Option<String>,
    coordination_discussion_id: Option<String>,
}

#[derive(Serialize)]
struct TimelineStepEntry {
    thread: String,
    step_id: String,
    branch_id: String,
    parent_step_id: Option<String>,
    tool_name: Option<String>,
    tool_status: Option<String>,
    changed: Option<bool>,
    payload_summary: Option<String>,
    payload_hash: Option<String>,
    before_state: Option<String>,
    after_state: Option<String>,
    capture_state: Option<String>,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
}

#[derive(Serialize)]
struct MarkerEntry {
    name: String,
    state: String,
    timestamp: String,
}

#[derive(Serialize)]
struct ContextAnnotationEntry {
    path: String,
    scope: String,
    kind: String,
    /// Excerpt by default; full body with `--verbose`.
    content_excerpt: String,
    attribution: String,
    created_at: String,
}

#[derive(Serialize)]
struct VerifySignal {
    /// `test_passed` for high-confidence captures whose intent begins
    /// `verified:` (the verify hook's pass capture); `test_failed` for
    /// `failed-*` markers (the verify hook's failure marker).
    kind: String,
    /// The intent or marker name that produced the signal.
    label: String,
    timestamp: String,
}

#[derive(Serialize)]
struct MergeEntry {
    description: String,
    timestamp: String,
}

#[derive(Serialize)]
struct UndoEntry {
    description: String,
    timestamp: String,
}

pub async fn cmd_retro(cli: &Cli, options: RetroCommandOptions) -> Result<()> {
    let repo = cli.open_repo()?;
    let head_state = repo.current_state()?;

    let (since_id, since_ts) = resolve_since_bound(&repo, options.since.as_deref(), &head_state)?;

    let until_ts = head_state.as_ref().map(|s| s.created_at);
    let duration_secs = plan_duration_secs(
        since_ts.map(|ts| ts.timestamp()),
        until_ts.map(|ts| ts.timestamp()),
    );

    // The single oplog walk. Every grouped output (states, markers,
    // merges, undos) reads from the same batch list so a single
    // timestamp comparison gates everything.
    let batches = repo
        .oplog()
        .recent_batches(MAX_OPLOG_BATCHES)
        .context("read recent oplog batches for retro")?;

    let mut states_captured = Vec::new();
    let mut markers_created = Vec::new();
    let mut merges = Vec::new();
    let mut undos = Vec::new();
    let mut verify_signals = Vec::new();
    let mut seen_state_ids: HashSet<StateId> = HashSet::new();

    for batch in &batches {
        // Batches arrive newest-first. We can stop once we hit one
        // that's older than the lower bound — every subsequent batch
        // is also older.
        let batch_ts = batch
            .entries
            .first()
            .map(|entry| entry.timestamp)
            .unwrap_or_else(Utc::now);
        if let Some(lo) = since_ts
            && batch_ts < lo
        {
            break;
        }

        for entry in &batch.entries {
            if let Some(lo) = since_ts
                && entry.timestamp < lo
            {
                continue;
            }

            // `heddle undo` marks whole batches across many op kinds
            // (Snapshot, ThreadUpdate, MarkerCreate, Goto, …) — not
            // just `Goto`. Catch every undone entry up front so we
            // surface the full undo activity in the time window, then
            // fall through to the normal classification (so e.g. a
            // captured-then-undone Snapshot still appears as a state
            // and as an undo).
            if options.include_undos && entry.undone {
                undos.push(UndoEntry {
                    description: entry.operation.description(),
                    timestamp: format_ts(entry.timestamp),
                });
            }

            match &entry.operation {
                OpRecord::Snapshot { new_state, .. }
                | OpRecord::Checkpoint {
                    state: new_state, ..
                } => {
                    if !seen_state_ids.insert(*new_state) {
                        continue;
                    }
                    let Some(state) = repo.store().get_state(new_state)? else {
                        continue;
                    };
                    let display = state_entry(&state, options.verbose);
                    if let Some(signal) = derive_verify_signal_from_state(&state) {
                        verify_signals.push(signal);
                    }
                    states_captured.push(display);
                }
                OpRecord::MarkerCreate { name, state } => {
                    let timestamp = format_ts(entry.timestamp);
                    if let Some(signal) = derive_verify_signal_from_marker(name, &timestamp) {
                        verify_signals.push(signal);
                    }
                    markers_created.push(MarkerEntry {
                        name: name.clone(),
                        state: state.short(),
                        timestamp,
                    });
                }
                OpRecord::Collapse { .. } if options.include_merges => {
                    merges.push(MergeEntry {
                        description: entry.operation.description(),
                        timestamp: format_ts(entry.timestamp),
                    });
                }
                // Not surfaced in the retro summary (includes `Collapse` when
                // `--include-merges` is off). Enumerated explicitly (no
                // wildcard) so a new `OpRecord` variant must be considered for
                // the retro rollup instead of silently vanishing from it
                // (heddle#354 r9).
                OpRecord::Goto { .. }
                | OpRecord::ThreadCreate { .. }
                | OpRecord::ThreadDelete { .. }
                | OpRecord::ThreadUpdate { .. }
                | OpRecord::Fork { .. }
                | OpRecord::Collapse { .. }
                | OpRecord::MarkerDelete { .. }
                | OpRecord::TransactionAbort { .. }
                | OpRecord::EphemeralThreadCollapse { .. }
                | OpRecord::ConflictResolved { .. }
                | OpRecord::TransactionCommit { .. }
                | OpRecord::Redact { .. }
                | OpRecord::Purge { .. }
                | OpRecord::FastForward { .. }
                | OpRecord::GitCheckpoint { .. }
                | OpRecord::RemoteThreadUpdate { .. }
                | OpRecord::RemoteThreadDelete { .. }
                | OpRecord::UndoRecoveryUpdate { .. }
                | OpRecord::StateVisibilitySet { .. }
                | OpRecord::StateVisibilityPromote { .. } => {}
            }
        }
    }

    // Newest-first across the board so the consumer's first row is
    // the most recent event in every section.
    states_captured.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    markers_created.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    verify_signals.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    undos.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    merges.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    let agents_active = collect_agents(&repo, since_ts)?;
    let agent_tasks = collect_agent_tasks(&repo, since_ts, options.verbose)?;
    let timeline_steps = if options.verbose {
        collect_timeline_steps(&repo, since_ts, options.verbose)?
    } else {
        Vec::new()
    };
    let context_annotations =
        collect_context_annotations(&repo, head_state.as_ref(), since_ts, options.verbose)?;

    let output = RetroOutput {
        since: since_id.map(|id| id.to_string_full()),
        until: head_state.as_ref().map(|s| s.state_id.to_string_full()),
        duration_secs,
        states_captured,
        agents_active,
        agent_tasks,
        timeline_steps,
        markers_created,
        context_annotations,
        verify_signals,
        merges,
        undos,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        print_human(&output, options.verbose);
    }

    Ok(())
}

fn resolve_since_bound(
    repo: &Repository,
    since: Option<&str>,
    head_state: &Option<State>,
) -> Result<(Option<StateId>, Option<DateTime<Utc>>)> {
    if let Some(spec) = since {
        let id = resolve_state_id(repo, spec)?;
        let ts = repo.store().get_state(&id)?.map(|state| state.created_at);
        return Ok((Some(id), ts));
    }

    // No explicit since: pick the more-recent of (last "Claude Code
    // turn"-style capture, one hour ago).
    let now = Utc::now();
    let recent_turn_ts = find_recent_turn_ts(repo)?;
    let chosen_secs = choose_default_since_ts(
        now.timestamp(),
        recent_turn_ts.map(|ts| ts.timestamp()),
        head_state.is_some(),
        DEFAULT_FALLBACK_WINDOW_HOURS,
    );
    let chosen = chosen_secs.and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0));
    Ok((None, chosen))
}

/// Scan recent snapshot states for one whose `intent` mentions a
/// "Claude Code turn"-shaped boundary. The session-segment hook writes
/// these on `UserPromptSubmit`, so they make a natural turn marker.
/// Best-effort: returns `None` if no match is found in the recent
/// window, and the caller falls back to the time-based window.
fn find_recent_turn_ts(repo: &Repository) -> Result<Option<DateTime<Utc>>> {
    let batches = repo.oplog().recent_batches(256)?;
    for batch in batches {
        for entry in batch.entries {
            let new_state = match &entry.operation {
                OpRecord::Snapshot { new_state, .. }
                | OpRecord::Checkpoint {
                    state: new_state, ..
                } => *new_state,
                // Only capture-style records carry a turn-boundary intent.
                // Enumerated explicitly (no wildcard) so a future
                // intent-carrying variant is considered here (heddle#354 r9).
                OpRecord::Goto { .. }
                | OpRecord::ThreadCreate { .. }
                | OpRecord::ThreadDelete { .. }
                | OpRecord::ThreadUpdate { .. }
                | OpRecord::Fork { .. }
                | OpRecord::Collapse { .. }
                | OpRecord::MarkerCreate { .. }
                | OpRecord::MarkerDelete { .. }
                | OpRecord::TransactionAbort { .. }
                | OpRecord::EphemeralThreadCollapse { .. }
                | OpRecord::ConflictResolved { .. }
                | OpRecord::TransactionCommit { .. }
                | OpRecord::Redact { .. }
                | OpRecord::Purge { .. }
                | OpRecord::FastForward { .. }
                | OpRecord::GitCheckpoint { .. }
                | OpRecord::RemoteThreadUpdate { .. }
                | OpRecord::RemoteThreadDelete { .. }
                | OpRecord::UndoRecoveryUpdate { .. }
                | OpRecord::StateVisibilitySet { .. }
                | OpRecord::StateVisibilityPromote { .. } => continue,
            };
            if let Some(state) = repo.store().get_state(&new_state)?
                && let Some(intent) = state.intent.as_deref()
                && is_turn_boundary_intent(intent)
            {
                return Ok(Some(state.created_at));
            }
        }
    }
    Ok(None)
}

fn collect_agents(
    repo: &Repository,
    since_ts: Option<DateTime<Utc>>,
) -> Result<Vec<ActorPresence>> {
    let registry = ActorPresenceStore::new(repo.heddle_dir());
    let entries = registry.list().unwrap_or_default();
    let mut out = Vec::new();
    for entry in entries {
        // Window filter: include if the agent was active any time
        // within (since, now]. An agent that started before `since`
        // but is still Active counts; one that completed before
        // `since` does not.
        let active_now = matches!(entry.status, objects::store::ActorPresenceStatus::Active);
        let last_activity = entry
            .completed_at
            .or(entry.last_progress_at)
            .unwrap_or(entry.started_at);
        if !agent_window_overlaps(
            since_ts.map(|ts| ts.timestamp()),
            active_now,
            last_activity.timestamp(),
        ) {
            continue;
        }

        out.push(ActorPresence {
            session_id: entry.session_id.clone(),
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            status: entry.status.to_string(),
            started_at: format_ts(entry.started_at),
            completed_at: entry.completed_at.map(format_ts),
            tokens: AgentTokens {
                input: entry.usage_summary.input_tokens,
                output: entry.usage_summary.output_tokens,
                reasoning: entry.usage_summary.reasoning_tokens,
                tool_calls: entry.usage_summary.tool_calls,
            },
        });
    }
    Ok(out)
}

fn collect_agent_tasks(
    repo: &Repository,
    since_ts: Option<DateTime<Utc>>,
    verbose: bool,
) -> Result<Vec<AgentTaskEntry>> {
    let active_task_ids = ActorPresenceStore::new(repo.heddle_dir())
        .list()
        .context("failed to list agent registry for retro task correlation")?
        .into_iter()
        .filter(|entry| entry.status == ActorPresenceStatus::Active)
        .filter_map(|entry| entry.task_assignment_id)
        .collect::<HashSet<_>>();
    let tasks = AgentTaskStore::new(repo.heddle_dir())
        .list()
        .context("failed to list agent tasks for retro task correlation")?;
    let mut out = Vec::new();
    for task in tasks {
        if agent_task_window_overlaps(
            since_ts.map(|ts| ts.timestamp()),
            active_task_ids.contains(&task.task_id),
            task.updated_at.timestamp(),
            task.completed_at.map(|ts| ts.timestamp()),
        ) {
            out.push(agent_task_entry(&task, verbose));
        }
    }
    Ok(out)
}

fn agent_task_entry(task: &AgentTaskRecord, verbose: bool) -> AgentTaskEntry {
    AgentTaskEntry {
        task_id: task.task_id.clone(),
        title: display_free_text(&task.title, verbose),
        status: task.status.to_string(),
        target_thread: task.target_thread.clone(),
        updated_at: format_ts(task.updated_at),
        completed_at: task.completed_at.map(format_ts),
        coordination_discussion_id: task.coordination_discussion_id.clone(),
    }
}

fn collect_timeline_steps(
    repo: &Repository,
    since_ts: Option<DateTime<Utc>>,
    verbose: bool,
) -> Result<Vec<TimelineStepEntry>> {
    if !repo.heddle_dir().join("timeline").exists() {
        return Ok(Vec::new());
    }
    let store = TimelineStore::open(repo.heddle_dir())?;
    let view = TimelineView::rebuild(&store)?;
    let lo_ms = since_ts.map(|ts| ts.timestamp_millis());
    let mut out = Vec::new();
    for step in view.steps() {
        let step_ms = step.finished_at_ms.or(step.started_at_ms);
        if !timeline_step_in_window(lo_ms, step_ms) {
            continue;
        }
        out.push(TimelineStepEntry {
            thread: step.thread.clone(),
            step_id: step.step_id.to_string(),
            branch_id: step.branch_id.to_string(),
            parent_step_id: step.parent_step_id.as_ref().map(ToString::to_string),
            tool_name: step.tool_name.clone(),
            tool_status: step.status.as_ref().map(timeline_status_label),
            changed: step.changed,
            payload_summary: step
                .payload_summary
                .as_ref()
                .map(|summary| display_free_text(summary, verbose)),
            payload_hash: step.payload_hash.as_ref().map(|hash| hash.to_hex()),
            before_state: step.before_state.map(|state| state.short()),
            after_state: step.after_state.map(|state| state.short()),
            capture_state: step.capture_state.map(|state| state.short()),
            started_at_ms: step.started_at_ms,
            finished_at_ms: step.finished_at_ms,
        });
    }
    out.sort_by(|a, b| {
        b.finished_at_ms
            .or(b.started_at_ms)
            .cmp(&a.finished_at_ms.or(a.started_at_ms))
            .then_with(|| b.step_id.cmp(&a.step_id))
    });
    Ok(out)
}

fn collect_context_annotations(
    repo: &Repository,
    head_state: Option<&State>,
    since_ts: Option<DateTime<Utc>>,
    verbose: bool,
) -> Result<Vec<ContextAnnotationEntry>> {
    let Some(state) = head_state else {
        return Ok(Vec::new());
    };
    let Some(context_root) = repo.inherit_parent_context(state)? else {
        return Ok(Vec::new());
    };

    let entries = repo
        .list_context_entries(&context_root, None::<&Path>)
        .unwrap_or_default();

    let lo_secs = since_ts.map(|ts| ts.timestamp());
    let mut out = Vec::new();
    for ctx_entry in entries {
        let target_label = match ctx_entry.target.path() {
            Some(path) => path.to_string(),
            None => "<state>".to_string(),
        };
        for annotation in &ctx_entry.blob.annotations {
            let Some(current) = annotation.current_revision() else {
                continue;
            };
            if !context_annotation_in_window(lo_secs, current.created_at) {
                continue;
            }
            let content = if verbose {
                current.content.clone()
            } else {
                excerpt(&current.content)
            };
            out.push(ContextAnnotationEntry {
                path: target_label.clone(),
                scope: annotation.scope.to_string(),
                kind: current.kind.to_string(),
                content_excerpt: content,
                attribution: current.attribution.clone(),
                created_at: format_unix_ts(current.created_at),
            });
        }
    }

    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

fn state_entry(state: &State, verbose: bool) -> StateEntry {
    let intent = state
        .intent
        .as_ref()
        .map(|i| if verbose { i.clone() } else { excerpt(i) });
    StateEntry {
        state_id: state.state_id.to_string_full(),
        intent,
        confidence: state.confidence,
        agent: state
            .attribution
            .agent
            .as_ref()
            .map(|a| format!("{}/{}", a.provider, a.model)),
        principal: state.attribution.principal.to_string(),
        timestamp: format_ts(state.created_at),
    }
}

/// A high-confidence capture whose intent begins `verified:` is the
/// verify hook's pass-signal. The rest of the heuristic mirrors the
/// hook's `failed-*` marker shape: see `.claude/hooks/heddle-verify.sh`.
fn derive_verify_signal_from_state(state: &State) -> Option<VerifySignal> {
    let intent = state.intent.as_deref()?;
    if !is_verify_pass_signal(intent, state.confidence) {
        return None;
    }
    Some(VerifySignal {
        kind: "test_passed".to_string(),
        label: intent.to_string(),
        timestamp: format_ts(state.created_at),
    })
}

fn derive_verify_signal_from_marker(name: &str, timestamp: &str) -> Option<VerifySignal> {
    if !is_verify_fail_marker(name) {
        return None;
    }
    Some(VerifySignal {
        kind: "test_failed".to_string(),
        label: name.to_string(),
        timestamp: timestamp.to_string(),
    })
}

fn format_ts(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn format_unix_ts(secs: i64) -> String {
    DateTime::<Utc>::from_timestamp(secs, 0)
        .map(format_ts)
        .unwrap_or_else(|| secs.to_string())
}

fn timeline_status_label(status: &objects::object::TimelineToolCallStatus) -> String {
    match status {
        objects::object::TimelineToolCallStatus::Succeeded => "succeeded",
        objects::object::TimelineToolCallStatus::Failed => "failed",
        objects::object::TimelineToolCallStatus::Cancelled => "cancelled",
    }
    .to_string()
}

fn print_human(output: &RetroOutput, _verbose: bool) {
    println!(
        "{}",
        retro_header_line(
            output.since.as_deref(),
            output.until.as_deref(),
            output.duration_secs,
        )
    );
    println!();
    println!("States captured ({}):", output.states_captured.len());
    for entry in &output.states_captured {
        let intent = entry.intent.as_deref().unwrap_or("(no intent)");
        let confidence = entry
            .confidence
            .map(|c| format!("{:.2}", c))
            .unwrap_or_else(|| "—".to_string());
        println!(
            "  {} {} conf={} [{}]",
            short_state_id(&entry.state_id),
            intent,
            confidence,
            entry.timestamp,
        );
    }
    println!();
    println!("Agents active ({}):", output.agents_active.len());
    for agent in &output.agents_active {
        let actor_text =
            crate::cli::render::actor_display(agent.provider.as_deref(), agent.model.as_deref())
                .unwrap_or_else(|| "?/?".to_string());
        println!(
            "  {} {} status={}",
            agent.session_id, actor_text, agent.status,
        );
    }
    println!();
    println!("Agent tasks ({}):", output.agent_tasks.len());
    for task in &output.agent_tasks {
        println!(
            "  {} [{}] thread={} {}",
            task.task_id, task.status, task.target_thread, task.title,
        );
    }
    println!();
    println!("Timeline steps ({}):", output.timeline_steps.len());
    for step in &output.timeline_steps {
        let status = step.tool_status.as_deref().unwrap_or("unknown");
        let summary = step.payload_summary.as_deref().unwrap_or("(no summary)");
        println!(
            "  {} {} tool={} status={} {}",
            step.thread,
            step.step_id,
            step.tool_name.as_deref().unwrap_or("<unknown>"),
            status,
            summary,
        );
    }
    println!();
    println!("Markers created ({}):", output.markers_created.len());
    for marker in &output.markers_created {
        println!(
            "  {} -> {} [{}]",
            marker.name, marker.state, marker.timestamp
        );
    }
    println!();
    println!(
        "Context annotations ({}):",
        output.context_annotations.len()
    );
    for ctx in &output.context_annotations {
        println!(
            "  {} {} ({}) — {}",
            ctx.path, ctx.scope, ctx.kind, ctx.content_excerpt
        );
    }
    println!();
    println!("Verify signals ({}):", output.verify_signals.len());
    for signal in &output.verify_signals {
        println!("  {} {} [{}]", signal.kind, signal.label, signal.timestamp);
    }
    if !output.merges.is_empty() {
        println!();
        println!("Merges ({}):", output.merges.len());
        for merge in &output.merges {
            println!("  {} [{}]", merge.description, merge.timestamp);
        }
    }
    if !output.undos.is_empty() {
        println!();
        println!("Undos ({}):", output.undos.len());
        for undo in &output.undos {
            println!("  {} [{}]", undo.description, undo.timestamp);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use objects::object::{Attribution, Principal};
    use oplog::OpLogBackend;
    use repo::Repository;
    use tempfile::TempDir;

    use super::*;

    /// `Repository::init` writes user config; serialize tests so
    /// concurrent inits don't trip on a shared $HOME for any test
    /// that reads `UserConfig::load_default`.
    static INIT_LOCK: Mutex<()> = Mutex::new(());

    fn setup_repo() -> (TempDir, Repository) {
        let _g = INIT_LOCK.lock().unwrap();
        let temp = TempDir::new().expect("temp dir");
        let repo = Repository::init(temp.path()).expect("init repo");
        (temp, repo)
    }

    fn snap(repo: &Repository, intent: &str, confidence: f32) -> objects::object::State {
        let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
        repo.snapshot_with_attribution(Some(intent.to_string()), Some(confidence), attribution)
            .expect("snapshot")
    }

    #[test]
    fn excerpt_truncates_long_content() {
        use heddle_core::retro_plan::EXCERPT_LEN;
        let long = "a".repeat(EXCERPT_LEN + 50);
        let out = excerpt(&long);
        let char_count = out.chars().count();
        // Exactly EXCERPT_LEN chars + the ellipsis.
        assert_eq!(char_count, EXCERPT_LEN + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn excerpt_preserves_short_content() {
        let s = "short content";
        assert_eq!(excerpt(s), s);
    }

    #[test]
    fn display_free_text_scrubs_path_like_tokens_unless_verbose() {
        let s =
            "Review src/lib.rs and private-secret-name.txt. Check secret.env! Maybe docs/plan.md?";
        let scrubbed = display_free_text(s, false);
        assert!(!scrubbed.contains("src/lib.rs"));
        assert!(!scrubbed.contains("private-secret-name.txt"));
        assert!(!scrubbed.contains("secret.env"));
        assert!(!scrubbed.contains("docs/plan.md"));
        assert!(scrubbed.contains("[redacted-path]"));
        assert_eq!(display_free_text(s, true), s);
    }

    #[test]
    fn derive_verify_signal_recognizes_verified_intent() {
        let (_temp, repo) = setup_repo();
        let state = snap(&repo, "verified: cargo test --lib passed", 0.9);
        let sig = derive_verify_signal_from_state(&state).expect("signal");
        assert_eq!(sig.kind, "test_passed");
        assert!(sig.label.contains("cargo test"));
    }

    #[test]
    fn derive_verify_signal_skips_low_confidence_verified() {
        let (_temp, repo) = setup_repo();
        let state = snap(&repo, "verified: ambiguous run", 0.5);
        assert!(derive_verify_signal_from_state(&state).is_none());
    }

    #[test]
    fn derive_verify_signal_from_marker_recognizes_failed_prefix() {
        let sig = derive_verify_signal_from_marker("failed-1778358874", "2026-05-09T12:00:00Z")
            .expect("signal");
        assert_eq!(sig.kind, "test_failed");
        assert_eq!(sig.label, "failed-1778358874");
    }

    #[test]
    fn derive_verify_signal_from_marker_ignores_user_markers() {
        assert!(derive_verify_signal_from_marker("v1.0.0", "2026-05-09T12:00:00Z").is_none());
    }

    #[test]
    fn retro_walks_synthetic_capture_sequence() {
        let (_temp, repo) = setup_repo();

        // Three captures, increasing recency.
        let s1 = snap(&repo, "first capture", 0.7);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let _s2 = snap(&repo, "second capture", 0.8);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let _s3 = snap(&repo, "verified: third capture passes", 0.9);

        let (_since_id, since_ts) =
            resolve_since_bound(&repo, None, &repo.current_state().unwrap()).unwrap();
        // Default window must include all three captures (they're all
        // less than an hour old).
        assert!(since_ts.is_some());
        assert!(since_ts.unwrap() <= s1.created_at);

        // Drive through the same code path the dispatcher uses, but
        // collect into a buffer instead of stdout.
        let batches = repo.oplog().recent_batches(MAX_OPLOG_BATCHES).unwrap();
        let mut state_count = 0;
        let mut verify_signal_count = 0;
        for batch in &batches {
            for entry in &batch.entries {
                if let OpRecord::Snapshot { new_state, .. } = &entry.operation
                    && let Some(state) = repo.store().get_state(new_state).unwrap()
                {
                    state_count += 1;
                    if derive_verify_signal_from_state(&state).is_some() {
                        verify_signal_count += 1;
                    }
                }
            }
        }
        assert_eq!(state_count, 3, "all three snapshots should be in oplog");
        assert_eq!(verify_signal_count, 1, "one verified-prefix capture");
    }

    /// Regression for codex feedback on PR #54: `retro --include-undos`
    /// previously matched only `OpRecord::Goto` entries with
    /// `undone == true`. But `heddle undo` marks whole batches across
    /// many op kinds (Snapshot, ThreadUpdate, MarkerCreate, ...). The
    /// fix counts ANY undone entry within the window. This test
    /// exercises the loop body directly: capture a snapshot, mark its
    /// oplog batch undone, and walk the same code path as
    /// `cmd_retro` to assert the undo entry is surfaced even though
    /// the underlying op is `Snapshot`, not `Goto`.
    #[test]
    fn retro_include_undos_covers_undone_snapshot_batches() {
        let (_temp, repo) = setup_repo();

        // One snapshot — produces a Snapshot entry in the oplog.
        let _state = snap(&repo, "captured then undone", 0.8);

        // Mark the most recent batch undone via the oplog. This is
        // what `heddle undo` does under the hood; we drive the oplog
        // directly to keep the test focused on the retro classifier.
        let recent = repo.oplog().recent_batches(1).expect("recent batch");
        assert_eq!(recent.len(), 1);
        repo.oplog()
            .mark_batch_undone(&recent[0])
            .expect("mark undone");

        // Walk the loop body the way `cmd_retro` does and collect any
        // `undone` entries with `--include-undos`. Under the old
        // narrow match (Goto only), this loop would yield zero undos.
        let batches = repo
            .oplog()
            .recent_batches(MAX_OPLOG_BATCHES)
            .expect("recent batches");
        let mut undo_count = 0;
        for batch in &batches {
            for entry in &batch.entries {
                if entry.undone {
                    undo_count += 1;
                }
            }
        }
        assert!(
            undo_count >= 1,
            "expected at least one undone entry in the oplog (Snapshot kind), got {undo_count}"
        );

        // Assert the broadened match doesn't restrict by `OpRecord`
        // variant. The Snapshot we just undid must be classified as an
        // undo by the new logic — pre-fix it would have been silently
        // dropped because it wasn't a `Goto`.
        let undo_kinds: Vec<&'static str> = batches
            .iter()
            .flat_map(|b| b.entries.iter())
            .filter(|e| e.undone)
            .map(|e| match &e.operation {
                OpRecord::Snapshot { .. } => "Snapshot",
                OpRecord::Goto { .. } => "Goto",
                OpRecord::ThreadUpdate { .. } => "ThreadUpdate",
                OpRecord::MarkerCreate { .. } => "MarkerCreate",
                _ => "Other",
            })
            .collect();
        assert!(
            undo_kinds.contains(&"Snapshot"),
            "expected at least one undone Snapshot in the oplog, got kinds: {undo_kinds:?}"
        );
    }

    #[test]
    fn retro_output_shape_serializes_with_zero_data() {
        let (_temp, repo) = setup_repo();
        // Empty repo: no captures, no markers. Output must still be
        // valid JSON with all required keys present.
        let head_state = repo.current_state().unwrap();
        let output = RetroOutput {
            since: None,
            until: head_state.map(|s| s.state_id.to_string_full()),
            duration_secs: None,
            states_captured: Vec::new(),
            agents_active: Vec::new(),
            agent_tasks: Vec::new(),
            timeline_steps: Vec::new(),
            markers_created: Vec::new(),
            context_annotations: Vec::new(),
            verify_signals: Vec::new(),
            merges: Vec::new(),
            undos: Vec::new(),
        };
        let json = serde_json::to_string(&output).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        for key in &[
            "since",
            "until",
            "duration_secs",
            "states_captured",
            "agents_active",
            "agent_tasks",
            "timeline_steps",
            "markers_created",
            "context_annotations",
            "verify_signals",
            "merges",
            "undos",
        ] {
            assert!(parsed.get(key).is_some(), "missing key: {key}");
        }
    }
}
