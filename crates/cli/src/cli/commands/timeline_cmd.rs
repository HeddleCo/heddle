// SPDX-License-Identifier: Apache-2.0
//! Timeline navigation action commands.

use anyhow::{Result, anyhow};
use heddle_core::{
    timeline_cursor_reason, timeline_label,
    timeline_plan::{
        TimelinePlanError, TimelineSelection, TimelineTargetOptions, parse_branch_reason,
        parse_materialize_mode, parse_tool_status, plan_timeline_target,
        timeline_materialization_recovery_status, timeline_materialize_status,
    },
    timeline_recovery_status, timeline_tool_status,
};
use objects::object::{ChangeId, ContentHash};
use repo::{
    NativeToolCallRefV1, Repository, TimelineBranchId, TimelineLabel, TimelineMaterializeStatus,
    TimelineOperationBodyV1, TimelineOperationEnvelope, TimelineStore, TimelineToolPayloadMetadata,
    TimelineView, ToolCallFinishedV1, ToolCallStartedV1,
};
use serde::Serialize;

use super::advice::RecoveryAdvice;
use crate::cli::{
    Cli, TimelineArgs, TimelineCommands, TimelineForkArgs, TimelineRecordFinishArgs,
    TimelineRecordStartArgs, TimelineRecordToolArgs, TimelineRecoverArgs, TimelineResetArgs,
    TimelineStatusArgs, TimelineTargetArgs, should_output_json, style,
};

const TIMELINE_RESET_CURRENT_COMMAND: &str = "heddle timeline reset --thread <thread> --current";
const TIMELINE_TOOL_CALL_COMMAND: &str =
    "heddle timeline reset --thread <thread> --tool-call <tool-call-id> --harness opencode";

pub fn cmd_timeline(cli: &Cli, args: TimelineArgs) -> Result<()> {
    let start = cli
        .repo
        .clone()
        .unwrap_or(std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")));
    let repo = Repository::open(start)?;
    let store = TimelineStore::open(repo.heddle_dir())?;

    match args.command {
        TimelineCommands::Status(args) => cmd_timeline_status(cli, &repo, &store, args),
        TimelineCommands::RecordStart(args) => cmd_timeline_record_start(cli, &repo, &store, args),
        TimelineCommands::RecordFinish(args) => {
            cmd_timeline_record_finish(cli, &repo, &store, args)
        }
        TimelineCommands::Fork(args) => cmd_timeline_fork(cli, &repo, &store, args),
        TimelineCommands::Reset(args) => cmd_timeline_reset(cli, &repo, &store, args),
        TimelineCommands::Recover(args) => cmd_timeline_recover(cli, &repo, &store, args),
    }
}

fn cmd_timeline_status(
    cli: &Cli,
    repo: &Repository,
    store: &TimelineStore,
    args: TimelineStatusArgs,
) -> Result<()> {
    if args.thread.trim().is_empty() {
        return Err(anyhow!(RecoveryAdvice::missing_option(
            "timeline_thread_required",
            "--thread",
            "timeline status",
            "heddle timeline status --thread <thread>",
        )));
    }
    let snapshot = repo.timeline_navigation_snapshot(store, &args.thread)?;
    let current_step = snapshot
        .cursor
        .step_id
        .as_ref()
        .and_then(|cursor| snapshot.steps.iter().find(|step| &step.step_id == cursor))
        .map(|step| TimelineStatusStepOutput {
            step_id: step.step_id.to_string(),
            branch_id: step.branch_id.to_string(),
            parent_step_id: step.parent_step_id.as_ref().map(ToString::to_string),
            tool_name: step.tool_name.clone(),
            tool_status: step.status.as_ref().map(timeline_tool_status),
            changed: step.changed,
            payload_summary: step.payload_summary.clone(),
            payload_hash: step.payload_hash.map(|hash| hash.to_hex()),
            labels: step.labels.iter().map(timeline_label).collect(),
            started_at_ms: step.started_at_ms,
            finished_at_ms: step.finished_at_ms,
            can_seek: step.can_seek,
            can_fork: step.can_fork,
            can_reset: step.can_reset,
            can_materialize: step.can_materialize,
            has_boundary_warning: step.has_boundary_warning,
        });
    let output = TimelineStatusOutput {
        output_kind: "timeline_status",
        status: "ok",
        thread: snapshot.thread,
        cursor_branch_id: snapshot.cursor.branch_id.as_ref().map(ToString::to_string),
        cursor_step_id: snapshot.cursor.step_id.as_ref().map(ToString::to_string),
        cursor_state: snapshot.cursor.state.map(|state| state.to_string_full()),
        current_step,
        active_branch_path: snapshot
            .active_branch_path
            .iter()
            .map(ToString::to_string)
            .collect(),
        can_undo: snapshot.actions.can_undo,
        can_redo: snapshot.actions.can_redo,
        branch_count: snapshot.branches.len(),
        step_count: snapshot.steps.len(),
        recovery: snapshot
            .recovery
            .map(|recovery| TimelineStatusRecoveryOutput {
                status: timeline_recovery_status(recovery.status),
                branch_id: recovery.branch_id.to_string(),
                from_step_id: recovery.from_step_id.as_ref().map(ToString::to_string),
                to_step_id: recovery.to_step_id.as_ref().map(ToString::to_string),
                from_state: recovery.from_state.to_string_full(),
                to_state: recovery.to_state.to_string_full(),
                reason: timeline_cursor_reason(&recovery.reason).to_string(),
                moved_at_ms: recovery.moved_at_ms,
                checkout_state: recovery.checkout_state.map(|state| state.to_string_full()),
            }),
    };
    print_timeline_status(cli, repo, output)
}

fn cmd_timeline_record_start(
    cli: &Cli,
    repo: &Repository,
    store: &TimelineStore,
    args: TimelineRecordStartArgs,
) -> Result<()> {
    let native = native_tool_ref(&args.tool)?;
    let step_id = recording_step_id(&args.tool, &native);
    let before_state = require_current_change(repo, "timeline record-start")?;
    let payload = payload_metadata(&args.tool)?;
    let _record_guard = store.lock_recording(&args.tool.thread)?;
    let view = TimelineView::rebuild(store)?;
    let (branch_id, parent_step_id) = timeline_position_for_recording(&view, &args.tool, &step_id);
    let envelope = TimelineOperationEnvelope::new(
        TimelineOperationBodyV1::ToolCallStarted(ToolCallStartedV1 {
            thread: args.tool.thread.clone(),
            step_id: step_id.clone(),
            branch_id: branch_id.clone(),
            parent_step_id: parent_step_id.clone(),
            native,
            tool_name: args.tool_name,
            before_state,
            payload,
            started_at_ms: now_ms(),
        }),
        vec![TimelineLabel::ExternalSideEffectsUnknown],
    );
    let operation_id = store.write_operation(&envelope)?;
    let snapshot = repo.timeline_navigation_snapshot(store, &args.tool.thread)?;
    let output = TimelineRecordingOutput {
        output_kind: "timeline_record_start",
        status: "ok",
        action: "record-start",
        thread: args.tool.thread,
        step_id: step_id.to_string(),
        branch_id: branch_id.to_string(),
        parent_step_id: parent_step_id.as_ref().map(ToString::to_string),
        operation_id: operation_id.to_string_full(),
        before_state: Some(before_state.to_string_full()),
        after_state: None,
        changed: None,
        tool_status: None,
        payload_summary: args.tool.summary,
        payload_hash: payload_hash_string(args.tool.payload_hash.as_deref())?,
        branch_count: snapshot.branches.len(),
        step_count: snapshot.steps.len(),
    };
    print_timeline_recording(cli, repo, output)
}

fn cmd_timeline_record_finish(
    cli: &Cli,
    repo: &Repository,
    store: &TimelineStore,
    args: TimelineRecordFinishArgs,
) -> Result<()> {
    let tool_status = parse_tool_status(&args.status).map_err(map_timeline_plan_error)?;
    let tool_status_label = timeline_tool_status(&tool_status);
    let native = native_tool_ref(&args.tool)?;
    let step_id = recording_step_id(&args.tool, &native);
    let payload = payload_metadata(&args.tool)?;
    let _record_guard = store.lock_recording(&args.tool.thread)?;
    let view = TimelineView::rebuild(store)?;
    let existing_step = view.step(&args.tool.thread, &step_id).cloned();
    let (branch_id, parent_step_id) = existing_step
        .as_ref()
        .map(|step| (step.branch_id.clone(), step.parent_step_id.clone()))
        .unwrap_or_else(|| timeline_position_for_recording(&view, &args.tool, &step_id));
    let current_state = require_current_change(repo, "timeline record-finish")?;
    let before_state = existing_step
        .as_ref()
        .and_then(|step| step.before_state)
        .unwrap_or(current_state);
    let after_state = current_state;
    let changed = before_state != after_state;
    let mut labels = vec![TimelineLabel::ExternalSideEffectsUnknown];
    if changed {
        labels.push(TimelineLabel::RepoReversible);
    }
    let envelope = TimelineOperationEnvelope::new(
        TimelineOperationBodyV1::ToolCallFinished(ToolCallFinishedV1 {
            thread: args.tool.thread.clone(),
            step_id: step_id.clone(),
            branch_id: branch_id.clone(),
            native,
            status: tool_status,
            before_state,
            after_state,
            capture_state: None,
            capture_oplog_batch_id: None,
            changed,
            touched_paths: Vec::new(),
            payload,
            finished_at_ms: now_ms(),
        }),
        labels,
    );
    let operation_id = store.write_operation(&envelope)?;
    let snapshot = repo.timeline_navigation_snapshot(store, &args.tool.thread)?;
    let output = TimelineRecordingOutput {
        output_kind: "timeline_record_finish",
        status: "ok",
        action: "record-finish",
        thread: args.tool.thread,
        step_id: step_id.to_string(),
        branch_id: branch_id.to_string(),
        parent_step_id: parent_step_id.as_ref().map(ToString::to_string),
        operation_id: operation_id.to_string_full(),
        before_state: Some(before_state.to_string_full()),
        after_state: Some(after_state.to_string_full()),
        changed: Some(changed),
        tool_status: Some(tool_status_label),
        payload_summary: args.tool.summary,
        payload_hash: payload_hash_string(args.tool.payload_hash.as_deref())?,
        branch_count: snapshot.branches.len(),
        step_count: snapshot.steps.len(),
    };
    print_timeline_recording(cli, repo, output)
}

fn cmd_timeline_fork(
    cli: &Cli,
    repo: &Repository,
    store: &TimelineStore,
    args: TimelineForkArgs,
) -> Result<()> {
    let selection = target_selection(&args.target)?;
    let reason = parse_branch_reason(&args.reason).map_err(map_timeline_plan_error)?;
    let outcome = repo.fork_timeline_from_selector(
        store,
        &selection.thread,
        &selection.selector,
        selection.branch_constraint.as_ref(),
        args.branch.map(TimelineBranchId::new),
        reason,
        now_ms(),
    )?;
    let output = TimelineActionOutput {
        output_kind: "timeline_action",
        status: "completed",
        action: "fork",
        thread: selection.thread,
        branch_id: Some(outcome.branch_id.to_string()),
        parent_branch_id: Some(outcome.parent_branch_id.to_string()),
        from_step_id: outcome.from_step_id.map(|id| id.to_string()),
        cursor_branch_id: outcome
            .navigation
            .cursor
            .branch_id
            .as_ref()
            .map(ToString::to_string),
        cursor_step_id: outcome
            .navigation
            .cursor
            .step_id
            .as_ref()
            .map(ToString::to_string),
        operation_id: Some(outcome.operation_id.to_string_full()),
        recovered_operation_id: None,
        materialized: None,
        materialization_status: None,
        recovery_status: None,
        blocker_count: 0,
        branch_count: outcome.navigation.branches.len(),
        step_count: outcome.navigation.steps.len(),
    };
    print_timeline_action(cli, repo, output)
}

fn cmd_timeline_reset(
    cli: &Cli,
    repo: &Repository,
    store: &TimelineStore,
    args: TimelineResetArgs,
) -> Result<()> {
    let selection = target_selection(&args.target)?;
    let mode = parse_materialize_mode(&args.mode).map_err(map_timeline_plan_error)?;
    let outcome = repo.reset_timeline_cursor(
        store,
        &selection.thread,
        &selection.selector,
        mode,
        selection.branch_constraint.as_ref(),
        args.materialize,
        now_ms(),
    )?;
    let materialized = outcome.materialization.as_ref().map(|materialization| {
        matches!(
            materialization.status,
            TimelineMaterializeStatus::Materialized | TimelineMaterializeStatus::AlreadyAtTarget
        )
    });
    let materialization_status = outcome
        .materialization
        .as_ref()
        .map(|materialization| timeline_materialize_status(&materialization.status).to_string());
    let blocker_count = outcome
        .materialization
        .as_ref()
        .map(|materialization| {
            materialization.preview.blockers.len()
                + usize::from(materialization.recovery.blocker.is_some())
        })
        .unwrap_or_default();
    let output = TimelineActionOutput {
        output_kind: "timeline_action",
        status: "completed",
        action: "reset",
        thread: selection.thread,
        branch_id: outcome
            .navigation
            .cursor
            .branch_id
            .as_ref()
            .map(ToString::to_string),
        parent_branch_id: None,
        from_step_id: None,
        cursor_branch_id: outcome
            .navigation
            .cursor
            .branch_id
            .as_ref()
            .map(ToString::to_string),
        cursor_step_id: outcome
            .navigation
            .cursor
            .step_id
            .as_ref()
            .map(ToString::to_string),
        operation_id: outcome.cursor_operation_id.map(|id| id.to_string_full()),
        recovered_operation_id: outcome
            .materialization
            .as_ref()
            .and_then(|materialization| materialization.recovery.cursor_operation_id)
            .map(|id| id.to_string_full()),
        materialized,
        materialization_status,
        recovery_status: outcome.materialization.as_ref().map(|materialization| {
            timeline_materialization_recovery_status(&materialization.recovery.status).to_string()
        }),
        blocker_count,
        branch_count: outcome.navigation.branches.len(),
        step_count: outcome.navigation.steps.len(),
    };
    print_timeline_action(cli, repo, output)
}

fn cmd_timeline_recover(
    cli: &Cli,
    repo: &Repository,
    store: &TimelineStore,
    args: TimelineRecoverArgs,
) -> Result<()> {
    let outcome = repo.recover_timeline_materialization_action(store, &args.thread)?;
    let output = TimelineActionOutput {
        output_kind: "timeline_action",
        status: "completed",
        action: "recover",
        thread: args.thread,
        branch_id: outcome
            .navigation
            .cursor
            .branch_id
            .as_ref()
            .map(ToString::to_string),
        parent_branch_id: None,
        from_step_id: None,
        cursor_branch_id: outcome
            .navigation
            .cursor
            .branch_id
            .as_ref()
            .map(ToString::to_string),
        cursor_step_id: outcome
            .navigation
            .cursor
            .step_id
            .as_ref()
            .map(ToString::to_string),
        operation_id: None,
        recovered_operation_id: outcome
            .recovery
            .cursor_operation_id
            .map(|id| id.to_string_full()),
        materialized: None,
        materialization_status: None,
        recovery_status: Some(
            timeline_materialization_recovery_status(&outcome.recovery.status).to_string(),
        ),
        blocker_count: usize::from(outcome.recovery.blocker.is_some()),
        branch_count: outcome.navigation.branches.len(),
        step_count: outcome.navigation.steps.len(),
    };
    print_timeline_action(cli, repo, output)
}

fn print_timeline_action(cli: &Cli, repo: &Repository, output: TimelineActionOutput) -> Result<()> {
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    match output.action {
        "fork" => {
            let branch = output.branch_id.as_deref().unwrap_or("-");
            let parent = output.parent_branch_id.as_deref().unwrap_or("-");
            let step = output.from_step_id.as_deref().unwrap_or("cursor");
            println!(
                "Forked timeline branch {} from {}/{}",
                style::bold(branch),
                parent,
                step
            );
        }
        "reset" => {
            let branch = output.cursor_branch_id.as_deref().unwrap_or("-");
            let step = output.cursor_step_id.as_deref().unwrap_or("-");
            println!("Reset timeline cursor to {branch}/{step}");
            if let Some(status) = &output.materialization_status {
                println!(
                    "Materialization: {}{}",
                    status,
                    if output.blocker_count > 0 {
                        format!(
                            " ({} blocker{})",
                            output.blocker_count,
                            plural(output.blocker_count)
                        )
                    } else {
                        String::new()
                    }
                );
            }
        }
        "recover" => {
            println!(
                "Timeline recovery: {}",
                output.recovery_status.as_deref().unwrap_or("unknown")
            );
            if output.blocker_count > 0 {
                println!(
                    "Blockers: {} item{}",
                    output.blocker_count,
                    plural(output.blocker_count)
                );
            }
        }
        _ => {}
    }
    Ok(())
}

fn print_timeline_status(cli: &Cli, repo: &Repository, output: TimelineStatusOutput) -> Result<()> {
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    println!(
        "Timeline {}: {} step{} on {} branch{}",
        style::bold(&output.thread),
        output.step_count,
        plural(output.step_count),
        output.branch_count,
        plural(output.branch_count)
    );
    if let Some(step) = &output.cursor_step_id {
        println!(
            "Cursor: {}/{}",
            output.cursor_branch_id.as_deref().unwrap_or("-"),
            step
        );
    } else {
        println!("Cursor: none");
    }
    if let Some(recovery) = &output.recovery {
        println!("Recovery: {}", recovery.status);
    }
    Ok(())
}

fn print_timeline_recording(
    cli: &Cli,
    repo: &Repository,
    output: TimelineRecordingOutput,
) -> Result<()> {
    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    println!(
        "Recorded timeline {} {} ({})",
        output.action,
        style::bold(&output.step_id),
        output.operation_id
    );
    Ok(())
}

fn native_tool_ref(args: &TimelineRecordToolArgs) -> Result<NativeToolCallRefV1> {
    if args.thread.trim().is_empty() {
        return Err(anyhow!(RecoveryAdvice::missing_option(
            "timeline_thread_required",
            "--thread",
            "timeline recording",
            "heddle timeline record-start --thread <thread> --tool-call <id>",
        )));
    }
    if args.harness.trim().is_empty() {
        return Err(anyhow!(RecoveryAdvice::missing_option(
            "timeline_record_harness_required",
            "--harness",
            "timeline recording",
            "heddle timeline record-start --harness opencode --tool-call <id>",
        )));
    }
    if args.tool_call.trim().is_empty() {
        return Err(anyhow!(RecoveryAdvice::missing_option(
            "timeline_record_tool_call_required",
            "--tool-call",
            "timeline recording",
            "heddle timeline record-start --tool-call <id>",
        )));
    }
    Ok(NativeToolCallRefV1 {
        harness: args.harness.clone(),
        session_id: args.session.clone(),
        message_id: args.message.clone(),
        tool_call_id: args.tool_call.clone(),
    })
}

fn recording_step_id(
    args: &TimelineRecordToolArgs,
    native: &NativeToolCallRefV1,
) -> repo::TimelineStepId {
    args.step_id
        .as_ref()
        .map(|step| repo::TimelineStepId::new(step.clone()))
        .unwrap_or_else(|| {
            let key = format!(
                "{}\0{}\0{}\0{}",
                native.harness,
                native.session_id.as_deref().unwrap_or(""),
                native.message_id.as_deref().unwrap_or(""),
                native.tool_call_id
            );
            let hash =
                ContentHash::compute_typed("timeline-native-tool-call-v1", key.as_bytes()).to_hex();
            repo::TimelineStepId::new(format!("tls-{}", &hash[..24]))
        })
}

fn timeline_position_for_recording(
    view: &TimelineView,
    args: &TimelineRecordToolArgs,
    step_id: &repo::TimelineStepId,
) -> (TimelineBranchId, Option<repo::TimelineStepId>) {
    let branch_id = args
        .branch
        .as_ref()
        .map(|branch| TimelineBranchId::new(branch.clone()))
        .or_else(|| {
            view.status(&args.thread)
                .and_then(|status| status.current_branch_id.clone())
        })
        .unwrap_or_else(|| TimelineBranchId::new("tlb-main"));
    let parent_step_id = view
        .status(&args.thread)
        .and_then(|status| status.current_step_id.clone())
        .filter(|current| current != step_id);
    (branch_id, parent_step_id)
}

fn payload_metadata(args: &TimelineRecordToolArgs) -> Result<Option<TimelineToolPayloadMetadata>> {
    let hash = match args.payload_hash.as_deref() {
        Some(hash) => Some(parse_payload_hash(hash)?),
        None => None,
    };
    if args.summary.is_none() && hash.is_none() {
        return Ok(None);
    }
    Ok(Some(TimelineToolPayloadMetadata {
        summary: args.summary.clone(),
        hash,
    }))
}

fn parse_payload_hash(value: &str) -> Result<ContentHash> {
    ContentHash::from_hex(value).map_err(|_| {
        anyhow!(RecoveryAdvice::malformed_option_value(
            "timeline_payload_hash_invalid",
            "--payload-hash",
            value,
            "a 64-character hex content hash",
            "heddle timeline record-start --tool-call <id> --payload-hash <hex>",
        ))
    })
}

fn payload_hash_string(value: Option<&str>) -> Result<Option<String>> {
    value
        .map(|hash| parse_payload_hash(hash).map(|parsed| parsed.to_hex()))
        .transpose()
}

fn require_current_change(repo: &Repository, context: &str) -> Result<ChangeId> {
    repo.current_state()?
        .map(|state| state.change_id)
        .or(repo.head()?)
        .ok_or_else(|| anyhow!("{context} requires a repository state"))
}

fn target_selection(args: &TimelineTargetArgs) -> Result<TimelineSelection> {
    plan_timeline_target(&TimelineTargetOptions {
        thread: args.thread.clone(),
        from_branch: args.from_branch.clone(),
        step: args.step.clone(),
        tool_call: args.tool_call.clone(),
        harness: args.harness.clone(),
        session: args.session.clone(),
        message: args.message.clone(),
        undo: args.undo,
        redo: args.redo,
        current: args.current,
    })
    .map_err(map_timeline_plan_error)
}

fn map_timeline_plan_error(err: TimelinePlanError) -> anyhow::Error {
    match err {
        TimelinePlanError::InvalidToolStatus { raw } => {
            anyhow!(RecoveryAdvice::malformed_option_value(
                "timeline_tool_status_invalid",
                "--status",
                &raw,
                "succeeded, failed, or cancelled",
                "heddle timeline record-finish --tool-call <id> --status succeeded",
            ))
        }
        TimelinePlanError::InvalidBranchReason { raw } => {
            anyhow!(RecoveryAdvice::malformed_option_value(
                "timeline_branch_reason_invalid",
                "--reason",
                &raw,
                "explicit-fork, edit-from-rewound-cursor, retry, or fan-out",
                "heddle timeline fork --thread <thread> --current --reason explicit-fork",
            ))
        }
        TimelinePlanError::InvalidMaterializeMode { raw } => {
            anyhow!(RecoveryAdvice::malformed_option_value(
                "timeline_materialize_mode_invalid",
                "--mode",
                &raw,
                "fail-if-dirty or capture-current-then-seek",
                "heddle timeline reset --thread <thread> --current --mode fail-if-dirty",
            ))
        }
        TimelinePlanError::ThreadRequired => anyhow!(RecoveryAdvice::missing_option(
            "timeline_thread_required",
            "--thread",
            "timeline navigation",
            TIMELINE_RESET_CURRENT_COMMAND,
        )),
        TimelinePlanError::TargetRequired => anyhow!(RecoveryAdvice::invalid_usage(
            "timeline_target_required",
            "select exactly one timeline target: --step, --tool-call, --undo, --redo, or --current",
            "Set exactly one timeline selector. Use `--current` to target the current cursor.",
            TIMELINE_RESET_CURRENT_COMMAND,
        )),
        TimelinePlanError::ToolCallHarnessRequired => anyhow!(RecoveryAdvice::missing_option(
            "timeline_tool_call_harness_required",
            "--harness",
            "--tool-call timeline targets",
            TIMELINE_TOOL_CALL_COMMAND,
        )),
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[derive(Serialize)]
struct TimelineStatusOutput {
    output_kind: &'static str,
    status: &'static str,
    thread: String,
    cursor_branch_id: Option<String>,
    cursor_step_id: Option<String>,
    cursor_state: Option<String>,
    current_step: Option<TimelineStatusStepOutput>,
    active_branch_path: Vec<String>,
    can_undo: bool,
    can_redo: bool,
    branch_count: usize,
    step_count: usize,
    recovery: Option<TimelineStatusRecoveryOutput>,
}

#[derive(Serialize)]
struct TimelineStatusStepOutput {
    step_id: String,
    branch_id: String,
    parent_step_id: Option<String>,
    tool_name: Option<String>,
    tool_status: Option<&'static str>,
    changed: Option<bool>,
    payload_summary: Option<String>,
    payload_hash: Option<String>,
    labels: Vec<&'static str>,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
    can_seek: bool,
    can_fork: bool,
    can_reset: bool,
    can_materialize: bool,
    has_boundary_warning: bool,
}

#[derive(Serialize)]
struct TimelineStatusRecoveryOutput {
    status: &'static str,
    branch_id: String,
    from_step_id: Option<String>,
    to_step_id: Option<String>,
    from_state: String,
    to_state: String,
    reason: String,
    moved_at_ms: i64,
    checkout_state: Option<String>,
}

#[derive(Serialize)]
struct TimelineRecordingOutput {
    output_kind: &'static str,
    status: &'static str,
    action: &'static str,
    thread: String,
    step_id: String,
    branch_id: String,
    parent_step_id: Option<String>,
    operation_id: String,
    before_state: Option<String>,
    after_state: Option<String>,
    changed: Option<bool>,
    tool_status: Option<&'static str>,
    payload_summary: Option<String>,
    payload_hash: Option<String>,
    branch_count: usize,
    step_count: usize,
}

#[derive(Serialize)]
struct TimelineActionOutput {
    output_kind: &'static str,
    status: &'static str,
    action: &'static str,
    thread: String,
    branch_id: Option<String>,
    parent_branch_id: Option<String>,
    from_step_id: Option<String>,
    cursor_branch_id: Option<String>,
    cursor_step_id: Option<String>,
    operation_id: Option<String>,
    recovered_operation_id: Option<String>,
    materialized: Option<bool>,
    materialization_status: Option<String>,
    recovery_status: Option<String>,
    blocker_count: usize,
    branch_count: usize,
    step_count: usize,
}

#[cfg(test)]
mod tests {
    use repo::TimelineSeekSelector;

    use super::*;

    fn target() -> TimelineTargetArgs {
        TimelineTargetArgs {
            thread: "main".to_string(),
            from_branch: None,
            step: None,
            tool_call: None,
            harness: "opencode".to_string(),
            session: None,
            message: None,
            undo: false,
            redo: false,
            current: false,
        }
    }

    #[test]
    fn target_selection_requires_one_target() {
        assert!(target_selection(&target()).is_err());

        let mut args = target();
        args.step = Some("tls-one".to_string());
        args.tool_call = Some("call-1".to_string());
        assert!(target_selection(&args).is_err());
    }

    #[test]
    fn target_selection_builds_native_tool_call_selector() {
        let mut args = target();
        args.tool_call = Some("call-1".to_string());
        args.session = Some("session-1".to_string());

        let selection = target_selection(&args).unwrap();
        let TimelineSeekSelector::NativeToolCall(native) = selection.selector else {
            panic!("expected native tool call selector");
        };
        assert_eq!(native.harness, "opencode");
        assert_eq!(native.session_id.as_deref(), Some("session-1"));
        assert_eq!(native.tool_call_id, "call-1");
        assert!(selection.branch_constraint.is_none());
    }
}
