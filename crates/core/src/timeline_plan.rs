// SPDX-License-Identifier: Apache-2.0
//! Pure timeline CLI planning: parse/label helpers and target selection.
//!
//! Owns string ↔ enum mapping for timeline action flags and pure validation of
//! seek/fork/reset target selectors. Repository I/O, store access, recovery
//! advice rendering, and terminal output stay CLI-owned.
//!
//! Label helpers that already live in [`crate::log_plan`] are not duplicated
//! here; callers should reuse those for tool status, branch reason, cursor
//! reason, navigation recovery, and timeline labels.

use objects::object::{TimelineBranchReason, TimelineToolCallStatus};
use repo::{
    TimelineBranchId, TimelineMaterializationRecoveryStatus, TimelineMaterializeMode,
    TimelineMaterializeStatus, TimelineNativeToolKey, TimelineSeekBranchConstraint,
    TimelineSeekSelector, TimelineStepId,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failures from pure timeline parse / target planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelinePlanError {
    /// `--status` was not one of the known tool-call statuses.
    InvalidToolStatus { raw: String },
    /// `--reason` was not one of the known branch reasons.
    InvalidBranchReason { raw: String },
    /// `--mode` was not one of the known materialize modes.
    InvalidMaterializeMode { raw: String },
    /// Timeline thread name was empty.
    ThreadRequired,
    /// Zero or more than one of `--step` / `--tool-call` / `--undo` / `--redo` / `--current`.
    TargetRequired,
    /// `--tool-call` was set without a non-empty `--harness`.
    ToolCallHarnessRequired,
}

impl std::fmt::Display for TimelinePlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidToolStatus { raw } => {
                write!(
                    f,
                    "--status expects succeeded, failed, or cancelled, got '{raw}'"
                )
            }
            Self::InvalidBranchReason { raw } => write!(
                f,
                "--reason expects explicit-fork, edit-from-rewound-cursor, retry, or fan-out, got '{raw}'"
            ),
            Self::InvalidMaterializeMode { raw } => write!(
                f,
                "--mode expects fail-if-dirty or capture-current-then-seek, got '{raw}'"
            ),
            Self::ThreadRequired => write!(f, "--thread is required for timeline navigation"),
            Self::TargetRequired => write!(
                f,
                "select exactly one timeline target: --step, --tool-call, --undo, --redo, or --current"
            ),
            Self::ToolCallHarnessRequired => {
                write!(f, "--harness is required for --tool-call timeline targets")
            }
        }
    }
}

impl std::error::Error for TimelinePlanError {}

// ---------------------------------------------------------------------------
// Parse helpers
// ---------------------------------------------------------------------------

/// Parse a tool-call finish status string (`succeeded` / `failed` / `cancelled`).
pub fn parse_tool_status(value: &str) -> Result<TimelineToolCallStatus, TimelinePlanError> {
    match value {
        "succeeded" => Ok(TimelineToolCallStatus::Succeeded),
        "failed" => Ok(TimelineToolCallStatus::Failed),
        "cancelled" => Ok(TimelineToolCallStatus::Cancelled),
        other => Err(TimelinePlanError::InvalidToolStatus {
            raw: other.to_string(),
        }),
    }
}

/// Parse a timeline branch reason string.
pub fn parse_branch_reason(value: &str) -> Result<TimelineBranchReason, TimelinePlanError> {
    match value {
        "explicit-fork" => Ok(TimelineBranchReason::ExplicitFork),
        "edit-from-rewound-cursor" => Ok(TimelineBranchReason::EditFromRewoundCursor),
        "retry" => Ok(TimelineBranchReason::Retry),
        "fan-out" => Ok(TimelineBranchReason::FanOut),
        other => Err(TimelinePlanError::InvalidBranchReason {
            raw: other.to_string(),
        }),
    }
}

/// Parse a materialization mode string.
pub fn parse_materialize_mode(value: &str) -> Result<TimelineMaterializeMode, TimelinePlanError> {
    match value {
        "fail-if-dirty" => Ok(TimelineMaterializeMode::FailIfDirty),
        "capture-current-then-seek" => Ok(TimelineMaterializeMode::CaptureCurrentThenSeek),
        other => Err(TimelinePlanError::InvalidMaterializeMode {
            raw: other.to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Label helpers (not already in log_plan)
// ---------------------------------------------------------------------------

/// Materialize attempt status label for machine/text output.
pub fn timeline_materialize_status(status: &TimelineMaterializeStatus) -> &'static str {
    match status {
        TimelineMaterializeStatus::Materialized => "materialized",
        TimelineMaterializeStatus::AlreadyAtTarget => "already-at-target",
        TimelineMaterializeStatus::Refused => "refused",
        TimelineMaterializeStatus::Unsupported => "unsupported",
        TimelineMaterializeStatus::RecoveryBlocked => "recovery-blocked",
    }
}

/// Materialization recovery status label (distinct from navigation recovery).
pub fn timeline_materialization_recovery_status(
    status: &TimelineMaterializationRecoveryStatus,
) -> &'static str {
    match status {
        TimelineMaterializationRecoveryStatus::NoPending => "no-pending",
        TimelineMaterializationRecoveryStatus::CursorRecorded => "cursor-recorded",
        TimelineMaterializationRecoveryStatus::AlreadyApplied => "already-applied",
        TimelineMaterializationRecoveryStatus::Blocked => "blocked",
    }
}

// ---------------------------------------------------------------------------
// Target selection
// ---------------------------------------------------------------------------

/// Caller-supplied timeline target flags for pure seek/fork/reset planning.
///
/// Field names mirror the CLI `TimelineTargetArgs` surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineTargetOptions {
    pub thread: String,
    pub from_branch: Option<String>,
    pub step: Option<String>,
    pub tool_call: Option<String>,
    pub harness: String,
    pub session: Option<String>,
    pub message: Option<String>,
    pub undo: bool,
    pub redo: bool,
    pub current: bool,
}

/// Pure result of selecting a timeline seek/fork/reset target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineSelection {
    pub thread: String,
    pub selector: TimelineSeekSelector,
    pub branch_constraint: Option<TimelineSeekBranchConstraint>,
}

/// Plan a timeline target selector from pure flag inputs (no I/O).
pub fn plan_timeline_target(
    opts: &TimelineTargetOptions,
) -> Result<TimelineSelection, TimelinePlanError> {
    if opts.thread.trim().is_empty() {
        return Err(TimelinePlanError::ThreadRequired);
    }

    let selected = opts.step.is_some() as u8
        + opts.tool_call.is_some() as u8
        + opts.undo as u8
        + opts.redo as u8
        + opts.current as u8;
    if selected != 1 {
        return Err(TimelinePlanError::TargetRequired);
    }

    let branch = opts
        .from_branch
        .as_ref()
        .map(|branch| TimelineBranchId::new(branch.clone()));
    let (selector, branch_constraint) = if let Some(step_id) = &opts.step {
        (
            TimelineSeekSelector::StepId(TimelineStepId::new(step_id.clone())),
            branch.map(TimelineSeekBranchConstraint::Target),
        )
    } else if let Some(tool_call_id) = &opts.tool_call {
        if opts.harness.trim().is_empty() {
            return Err(TimelinePlanError::ToolCallHarnessRequired);
        }
        (
            TimelineSeekSelector::NativeToolCall(TimelineNativeToolKey {
                harness: opts.harness.clone(),
                session_id: opts.session.clone(),
                message_id: opts.message.clone(),
                tool_call_id: tool_call_id.clone(),
            }),
            None,
        )
    } else if opts.undo {
        (
            TimelineSeekSelector::Undo,
            branch.map(TimelineSeekBranchConstraint::Current),
        )
    } else if opts.redo {
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
        thread: opts.thread.clone(),
        selector,
        branch_constraint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_plan::{timeline_branch_reason, timeline_tool_status};

    fn target() -> TimelineTargetOptions {
        TimelineTargetOptions {
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
    fn parse_tool_status_round_trips() {
        for (raw, expected) in [
            ("succeeded", TimelineToolCallStatus::Succeeded),
            ("failed", TimelineToolCallStatus::Failed),
            ("cancelled", TimelineToolCallStatus::Cancelled),
        ] {
            let parsed = parse_tool_status(raw).expect("parse");
            assert_eq!(parsed, expected);
            assert_eq!(timeline_tool_status(&parsed), raw);
        }
        assert!(matches!(
            parse_tool_status("nope"),
            Err(TimelinePlanError::InvalidToolStatus { .. })
        ));
    }

    #[test]
    fn parse_branch_reason_round_trips() {
        for (raw, expected) in [
            ("explicit-fork", TimelineBranchReason::ExplicitFork),
            (
                "edit-from-rewound-cursor",
                TimelineBranchReason::EditFromRewoundCursor,
            ),
            ("retry", TimelineBranchReason::Retry),
            ("fan-out", TimelineBranchReason::FanOut),
        ] {
            let parsed = parse_branch_reason(raw).expect("parse");
            assert_eq!(parsed, expected);
            assert_eq!(timeline_branch_reason(&parsed), raw);
        }
        assert!(matches!(
            parse_branch_reason("side-quest"),
            Err(TimelinePlanError::InvalidBranchReason { .. })
        ));
    }

    #[test]
    fn parse_materialize_mode_and_labels() {
        assert_eq!(
            parse_materialize_mode("fail-if-dirty").unwrap(),
            TimelineMaterializeMode::FailIfDirty
        );
        assert_eq!(
            parse_materialize_mode("capture-current-then-seek").unwrap(),
            TimelineMaterializeMode::CaptureCurrentThenSeek
        );
        assert!(matches!(
            parse_materialize_mode("auto"),
            Err(TimelinePlanError::InvalidMaterializeMode { .. })
        ));

        assert_eq!(
            timeline_materialize_status(&TimelineMaterializeStatus::Materialized),
            "materialized"
        );
        assert_eq!(
            timeline_materialize_status(&TimelineMaterializeStatus::AlreadyAtTarget),
            "already-at-target"
        );
        assert_eq!(
            timeline_materialize_status(&TimelineMaterializeStatus::Refused),
            "refused"
        );
        assert_eq!(
            timeline_materialize_status(&TimelineMaterializeStatus::Unsupported),
            "unsupported"
        );
        assert_eq!(
            timeline_materialize_status(&TimelineMaterializeStatus::RecoveryBlocked),
            "recovery-blocked"
        );

        assert_eq!(
            timeline_materialization_recovery_status(
                &TimelineMaterializationRecoveryStatus::NoPending
            ),
            "no-pending"
        );
        assert_eq!(
            timeline_materialization_recovery_status(
                &TimelineMaterializationRecoveryStatus::CursorRecorded
            ),
            "cursor-recorded"
        );
        assert_eq!(
            timeline_materialization_recovery_status(
                &TimelineMaterializationRecoveryStatus::AlreadyApplied
            ),
            "already-applied"
        );
        assert_eq!(
            timeline_materialization_recovery_status(
                &TimelineMaterializationRecoveryStatus::Blocked
            ),
            "blocked"
        );
    }

    #[test]
    fn plan_timeline_target_requires_one_target() {
        assert_eq!(
            plan_timeline_target(&target()),
            Err(TimelinePlanError::TargetRequired)
        );

        let mut opts = target();
        opts.step = Some("tls-one".to_string());
        opts.tool_call = Some("call-1".to_string());
        assert_eq!(
            plan_timeline_target(&opts),
            Err(TimelinePlanError::TargetRequired)
        );
    }

    #[test]
    fn plan_timeline_target_builds_native_tool_call_selector() {
        let mut opts = target();
        opts.tool_call = Some("call-1".to_string());
        opts.session = Some("session-1".to_string());

        let selection = plan_timeline_target(&opts).unwrap();
        let TimelineSeekSelector::NativeToolCall(native) = selection.selector else {
            panic!("expected native tool call selector");
        };
        assert_eq!(native.harness, "opencode");
        assert_eq!(native.session_id.as_deref(), Some("session-1"));
        assert_eq!(native.tool_call_id, "call-1");
        assert!(selection.branch_constraint.is_none());
        assert_eq!(selection.thread, "main");
    }

    #[test]
    fn plan_timeline_target_step_and_undo_constraints() {
        let mut opts = target();
        opts.step = Some("tls-x".to_string());
        opts.from_branch = Some("tlb-a".to_string());
        let selection = plan_timeline_target(&opts).unwrap();
        assert!(matches!(
            selection.selector,
            TimelineSeekSelector::StepId(ref id) if id.as_str() == "tls-x"
        ));
        assert!(matches!(
            selection.branch_constraint,
            Some(TimelineSeekBranchConstraint::Target(_))
        ));

        let mut opts = target();
        opts.undo = true;
        opts.from_branch = Some("tlb-b".to_string());
        let selection = plan_timeline_target(&opts).unwrap();
        assert!(matches!(selection.selector, TimelineSeekSelector::Undo));
        assert!(matches!(
            selection.branch_constraint,
            Some(TimelineSeekBranchConstraint::Current(_))
        ));
    }

    #[test]
    fn plan_timeline_target_rejects_empty_thread_and_harness() {
        let mut opts = target();
        opts.thread = "  ".to_string();
        opts.current = true;
        assert_eq!(
            plan_timeline_target(&opts),
            Err(TimelinePlanError::ThreadRequired)
        );

        let mut opts = target();
        opts.tool_call = Some("call-1".to_string());
        opts.harness = String::new();
        assert_eq!(
            plan_timeline_target(&opts),
            Err(TimelinePlanError::ToolCallHarnessRequired)
        );
    }
}
