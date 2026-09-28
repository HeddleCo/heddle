//! The only local-to-hosted timeline projection. No display or harness text is copied.
use anyhow::{Context, Result};
use api::heddle::api::v1alpha2::{
    RunRecord, TimelineRecord, UploadRunSummary, UploadTimelineEvent, UploadTimelineEventKind,
    UploadTimelineTool, operation_record::State,
};

pub fn snapshot(run: &RunRecord) -> Result<UploadRunSummary> {
    let state = State::try_from(run.state).context("run state is unknown")?;
    anyhow::ensure!(
        matches!(
            state,
            State::Queued
                | State::Running
                | State::Completed
                | State::Failed
                | State::Canceled
                | State::WaitingForHuman
                | State::Paused
        ),
        "run state is not uploadable"
    );
    let summary = UploadRunSummary {
        state: state as i32,
        harness: match run.harness.as_str() {
            "claude-code" => "claude-code",
            "codex" => "codex",
            _ => "other",
        }
        .to_owned(),
    };
    api::timeline_upload::validate_summary(&summary)?;
    Ok(summary)
}

/// Unsupported local-only checkpoints do not consume hosted positions. The
/// outbox assigns dense hosted positions to this closed projection in local
/// position order and remembers the last scanned local position.
pub fn event(local: &TimelineRecord, hosted_position: u64) -> Result<Option<UploadTimelineEvent>> {
    use UploadTimelineEventKind as Kind;
    let kind = match local.kind.as_str() {
        "session_opened" | "run_started" => Kind::RunStarted,
        "session_closed" | "run_finished" => Kind::RunFinished,
        "run_failed" => Kind::RunFailed,
        "SubagentStart" | "turn_started" => Kind::TurnStarted,
        "Stop" | "SubagentStop" | "turn_complete" | "agent_done" | "session.idle"
        | "session.end" | "turn_finished" => Kind::TurnFinished,
        "PreToolUse" | "tool.execute.before" | "tool_started" => Kind::ToolStarted,
        "PostToolUse" | "tool.execute.after" | "tool_finished" => Kind::ToolFinished,
        "UserPromptSubmit" | "prompt_submitted" => Kind::PromptSubmitted,
        _ => return Ok(None),
    };
    let tool_name = if matches!(kind, Kind::ToolStarted | Kind::ToolFinished) {
        Some(match local.tool_name.as_deref() {
            Some("Bash") => UploadTimelineTool::Bash,
            Some("Edit") => UploadTimelineTool::Edit,
            Some("Read") => UploadTimelineTool::Read,
            Some("Write") => UploadTimelineTool::Write,
            Some("Grep") => UploadTimelineTool::Grep,
            Some("Glob") => UploadTimelineTool::Glob,
            Some("Task") => UploadTimelineTool::Task,
            _ => UploadTimelineTool::Other,
        } as i32)
    } else {
        None
    };
    let mut recorded_at = local
        .recorded_at
        .context("local timeline event has no timestamp")?;
    recorded_at.nanos -= recorded_at.nanos.rem_euclid(1000);
    Ok(Some(UploadTimelineEvent {
        position: hosted_position,
        kind: kind as i32,
        recorded_at: Some(recorded_at),
        tool_name,
    }))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use prost::Message;

    use super::*;

    #[test]
    fn local_free_text_never_reaches_upload_messages() {
        let canary = "PROMPT COMMAND TOOL_INPUT OUTPUT /secret/path API_KEY=secret";
        let run = RunRecord {
            harness: canary.into(),
            model: canary.into(),
            state: State::Running as i32,
            ..Default::default()
        };
        let projected_run = snapshot(&run).expect("closed summary");
        assert_eq!(projected_run.harness, "other");
        assert!(
            !projected_run
                .encode_to_vec()
                .windows(canary.len())
                .any(|w| w == canary.as_bytes())
        );
        let local = TimelineRecord {
            kind: "tool.execute.before".into(),
            summary: canary.into(),
            detail: Some(canary.into()),
            tool_name: Some(canary.into()),
            recorded_at: Some(prost_types::Timestamp {
                seconds: 1_700_000_000,
                nanos: 123_456_789,
            }),
            ..Default::default()
        };
        let projected = event(&local, 0).expect("projection").expect("tool event");
        assert_eq!(projected.tool_name, Some(UploadTimelineTool::Other as i32));
        assert!(
            !projected
                .encode_to_vec()
                .windows(canary.len())
                .any(|w| w == canary.as_bytes())
        );
        assert_eq!(
            projected.recorded_at.as_ref().map(|at| at.nanos),
            Some(123_456_000)
        );
    }

    #[test]
    fn unknown_local_checkpoint_does_not_consume_hosted_position() {
        let local = TimelineRecord {
            kind: "StatusLine".into(),
            ..Default::default()
        };
        assert!(event(&local, 7).expect("projection").is_none());
    }

    proptest! {
        #[test]
        fn arbitrary_local_free_text_cannot_cross_projection_boundary(
            free_text in "[A-Za-z0-9]{24,40}"
        ) {
            let run = RunRecord {
                state: State::Running as i32,
                harness: free_text.clone(),
                model: free_text.clone(),
                ..Default::default()
            };
            let summary = snapshot(&run).expect("closed run projection");
            prop_assert_eq!(&summary.harness, "other");
            prop_assert!(!summary.encode_to_vec().windows(free_text.len())
                .any(|bytes| bytes == free_text.as_bytes()));
            let local = TimelineRecord {
                kind: "tool.execute.after".into(),
                summary: free_text.clone(),
                detail: Some(free_text.clone()),
                tool_name: Some(free_text.clone()),
                recorded_at: Some(prost_types::Timestamp { seconds: 1_700_000_000, nanos: 0 }),
                ..Default::default()
            };
            let hosted = event(&local, 0).expect("closed event projection").expect("event");
            prop_assert_eq!(hosted.tool_name, Some(UploadTimelineTool::Other as i32));
            prop_assert!(!hosted.encode_to_vec().windows(free_text.len())
                .any(|bytes| bytes == free_text.as_bytes()));
        }
    }
}
