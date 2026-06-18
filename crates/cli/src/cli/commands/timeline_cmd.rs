// SPDX-License-Identifier: Apache-2.0
//! Timeline navigation action commands.

use anyhow::{Result, anyhow};
use repo::{
    Repository, TimelineBranchId, TimelineBranchReason, TimelineMaterializationRecoveryStatus,
    TimelineMaterializeMode, TimelineMaterializeStatus, TimelineNativeToolKey,
    TimelineSeekBranchConstraint, TimelineSeekSelector, TimelineStepId, TimelineStore,
};
use serde::Serialize;

use super::advice::RecoveryAdvice;
use crate::cli::{
    Cli, TimelineArgs, TimelineCommands, TimelineForkArgs, TimelineRecoverArgs, TimelineResetArgs,
    TimelineTargetArgs, should_output_json, style,
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
        TimelineCommands::Fork(args) => cmd_timeline_fork(cli, &repo, &store, args),
        TimelineCommands::Reset(args) => cmd_timeline_reset(cli, &repo, &store, args),
        TimelineCommands::Recover(args) => cmd_timeline_recover(cli, &repo, &store, args),
    }
}

fn cmd_timeline_fork(
    cli: &Cli,
    repo: &Repository,
    store: &TimelineStore,
    args: TimelineForkArgs,
) -> Result<()> {
    let selection = target_selection(&args.target)?;
    let reason = parse_branch_reason(&args.reason)?;
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
    let mode = parse_materialize_mode(&args.mode)?;
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
        .map(|materialization| materialize_status_label(&materialization.status).to_string());
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
            recovery_status_label(&materialization.recovery.status).to_string()
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
        recovery_status: Some(recovery_status_label(&outcome.recovery.status).to_string()),
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

#[derive(Debug)]
struct TimelineSelection {
    thread: String,
    selector: TimelineSeekSelector,
    branch_constraint: Option<TimelineSeekBranchConstraint>,
}

fn target_selection(args: &TimelineTargetArgs) -> Result<TimelineSelection> {
    if args.thread.trim().is_empty() {
        return Err(anyhow!(RecoveryAdvice::missing_option(
            "timeline_thread_required",
            "--thread",
            "timeline navigation",
            TIMELINE_RESET_CURRENT_COMMAND,
        )));
    }
    let selected = args.step.is_some() as u8
        + args.tool_call.is_some() as u8
        + args.undo as u8
        + args.redo as u8
        + args.current as u8;
    if selected != 1 {
        return Err(anyhow!(RecoveryAdvice::invalid_usage(
            "timeline_target_required",
            "select exactly one timeline target: --step, --tool-call, --undo, --redo, or --current",
            "Set exactly one timeline selector. Use `--current` to target the current cursor.",
            TIMELINE_RESET_CURRENT_COMMAND,
        )));
    }

    let branch = args
        .from_branch
        .as_ref()
        .map(|branch| TimelineBranchId::new(branch.clone()));
    let (selector, branch_constraint) = if let Some(step_id) = &args.step {
        (
            TimelineSeekSelector::StepId(TimelineStepId::new(step_id.clone())),
            branch.map(TimelineSeekBranchConstraint::Target),
        )
    } else if let Some(tool_call_id) = &args.tool_call {
        if args.harness.trim().is_empty() {
            return Err(anyhow!(RecoveryAdvice::missing_option(
                "timeline_tool_call_harness_required",
                "--harness",
                "--tool-call timeline targets",
                TIMELINE_TOOL_CALL_COMMAND,
            )));
        }
        (
            TimelineSeekSelector::NativeToolCall(TimelineNativeToolKey {
                harness: args.harness.clone(),
                session_id: args.session.clone(),
                message_id: args.message.clone(),
                tool_call_id: tool_call_id.clone(),
            }),
            None,
        )
    } else if args.undo {
        (
            TimelineSeekSelector::Undo,
            branch.map(TimelineSeekBranchConstraint::Current),
        )
    } else if args.redo {
        (
            TimelineSeekSelector::Redo,
            branch.map(TimelineSeekBranchConstraint::Current),
        )
    } else {
        (
            TimelineSeekSelector::CurrentCursor,
            branch.map(TimelineSeekBranchConstraint::Current),
        )
    };

    Ok(TimelineSelection {
        thread: args.thread.clone(),
        selector,
        branch_constraint,
    })
}

fn parse_branch_reason(value: &str) -> Result<TimelineBranchReason> {
    match value {
        "explicit-fork" => Ok(TimelineBranchReason::ExplicitFork),
        "edit-from-rewound-cursor" => Ok(TimelineBranchReason::EditFromRewoundCursor),
        "retry" => Ok(TimelineBranchReason::Retry),
        "fan-out" => Ok(TimelineBranchReason::FanOut),
        other => Err(anyhow!(RecoveryAdvice::malformed_option_value(
            "timeline_branch_reason_invalid",
            "--reason",
            other,
            "explicit-fork, edit-from-rewound-cursor, retry, or fan-out",
            "heddle timeline fork --thread <thread> --current --reason explicit-fork",
        ))),
    }
}

fn parse_materialize_mode(value: &str) -> Result<TimelineMaterializeMode> {
    match value {
        "fail-if-dirty" => Ok(TimelineMaterializeMode::FailIfDirty),
        "capture-current-then-seek" => Ok(TimelineMaterializeMode::CaptureCurrentThenSeek),
        other => Err(anyhow!(RecoveryAdvice::malformed_option_value(
            "timeline_materialize_mode_invalid",
            "--mode",
            other,
            "fail-if-dirty or capture-current-then-seek",
            "heddle timeline reset --thread <thread> --current --mode fail-if-dirty",
        ))),
    }
}

fn materialize_status_label(status: &TimelineMaterializeStatus) -> &'static str {
    match status {
        TimelineMaterializeStatus::Materialized => "materialized",
        TimelineMaterializeStatus::AlreadyAtTarget => "already-at-target",
        TimelineMaterializeStatus::Refused => "refused",
        TimelineMaterializeStatus::Unsupported => "unsupported",
        TimelineMaterializeStatus::RecoveryBlocked => "recovery-blocked",
    }
}

fn recovery_status_label(status: &TimelineMaterializationRecoveryStatus) -> &'static str {
    match status {
        TimelineMaterializationRecoveryStatus::NoPending => "no-pending",
        TimelineMaterializationRecoveryStatus::CursorRecorded => "cursor-recorded",
        TimelineMaterializationRecoveryStatus::AlreadyApplied => "already-applied",
        TimelineMaterializationRecoveryStatus::Blocked => "blocked",
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
