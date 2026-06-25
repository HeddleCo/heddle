// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use base64::Engine as _;
use chrono::Utc;
use objects::{
    fs_atomic::write_file_atomic,
    object::{
        ChangeId, ContentHash, DiffKind, NativeToolCallRefV1, Session, ThreadName,
        TimelineBranchId, TimelineLabel, TimelineOperationBodyV1, TimelineOperationEnvelope,
        TimelineStepId, TimelineToolCallStatus, TimelineToolPayloadMetadata, ToolCallFinishedV1,
        ToolCallStartedV1, Tree,
    },
    store::{AgentEntry, AgentRegistry, AgentStatus, AgentUsageSummary, ObjectStore},
};
use oplog::OpLogRecorder;
use refs::Head;
use repo::{
    Repository, SessionManager, Thread, ThreadFreshness, ThreadIntegrationPolicy, ThreadManager,
    ThreadMode, ThreadState, TimelineStore, TimelineView,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wire::{
    HarnessIdentity, ProgressCheckpoint, SessionDiffSummary, SessionReportEnvelope,
    TranscriptAttachmentRef, UsageTotals, WorktreeChangeBaseline,
};

mod claude_hook;
mod probe;

use self::probe::{HarnessProbeInput, HarnessProbeResult, probe_harness_actor};
use crate::{
    cli::{
        Cli,
        commands::{
            snapshot::{
                SnapshotAgentOverrides, create_snapshot, summarize_confidence,
                summarize_verification,
            },
            worktree_cmd::helpers::{prepare_worktree_target, write_isolated_checkout},
        },
        style, worktree_status_options,
    },
    config::{
        HarnessMode, HarnessTranscriptMode, HarnessTransport, UserConfig, UserHarnessOverride,
        UserHarnessRootThreadPolicy, UserHarnessSubagentThreadPolicy, UserThreadWorkspaceMode,
    },
};

pub(crate) fn probe_current_process_harness(
    repo: &Repository,
    current_provider: Option<String>,
    current_model: Option<String>,
    current_policy: Option<String>,
) -> Result<HarnessProbeResult> {
    probe_harness_actor(&HarnessProbeInput {
        argv: detected_harness_argv().or_else(|| Some(std::env::args().collect())),
        env_hints: harness_env_hints(),
        explicit_harness: None,
        explicit_provider: None,
        explicit_model: None,
        explicit_thinking_level: None,
        explicit_policy: None,
        probe_metadata: BTreeMap::new(),
        current_provider,
        current_model,
        current_policy,
        repo_root: repo.root().display().to_string(),
    })
}

fn harness_env_hints() -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(key, value)| {
            !value.trim().is_empty()
                && (key.starts_with("HEDDLE_AGENT_")
                    || key.starts_with("CODEX_")
                    || key.starts_with("CLAUDE")
                    || key.starts_with("ANTHROPIC_")
                    || key.starts_with("OPENAI_")
                    || key.starts_with("OPENCODE_")
                    || key.starts_with("AIDER_")
                    || matches!(
                        key.as_str(),
                        "MODEL" | "REASONING_EFFORT" | "THINKING_LEVEL"
                    ))
        })
        .collect()
}

fn detected_harness_argv() -> Option<Vec<String>> {
    detected_harness_argv_impl()
}

#[cfg(target_os = "linux")]
fn detected_harness_argv_impl() -> Option<Vec<String>> {
    let mut pid = std::process::id();
    for _ in 0..8 {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let ppid = stat.split_whitespace().nth(3)?.parse::<u32>().ok()?;
        if ppid == 0 || ppid == pid {
            return None;
        }
        pid = ppid;
        let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let argv = raw
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).to_string())
            .collect::<Vec<_>>();
        let program = argv.first().map(|arg| arg.to_ascii_lowercase())?;
        if ["codex", "claude", "opencode", "aider"]
            .iter()
            .any(|needle| program.contains(needle))
        {
            return Some(argv);
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn detected_harness_argv_impl() -> Option<Vec<String>> {
    None
}

pub fn cmd_harness_bridge(cli: &Cli) -> Result<()> {
    let repo = cli.open_repo()?;
    let mut runtime = init_harness_runtime(&repo)?;

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<BridgeRequest>(&line) {
            Ok(request) => runtime.handle_request(request),
            Err(err) => BridgeResponse::error(
                None,
                "invalid_request",
                format!("failed to parse request: {err}"),
            ),
        };
        serde_json::to_writer(&mut writer, &response)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }

    Ok(())
}

pub(crate) fn relay_harness_event(
    repo: &Repository,
    harness: &str,
    event: &str,
    payload: &str,
) -> Result<()> {
    let mut runtime = init_harness_runtime(repo)?;
    let (json, warning) = parse_relay_payload(payload);
    if let Some(warning) = warning {
        eprintln!("{}", style::warn(&warning));
    }
    match harness {
        "codex" => relay_codex(&mut runtime, event, &json),
        "claude-code" => relay_claude(&mut runtime, event, &json),
        "opencode" => relay_opencode(&mut runtime, event, &json),
        other => Err(anyhow!("unsupported harness relay: {other}")),
    }
}

fn init_harness_runtime(repo: &Repository) -> Result<HarnessBridgeRuntime> {
    let (user_config, warning) = load_harness_user_config(UserConfig::default_path());
    if let Some(warning) = warning {
        eprintln!("{}", style::warn(&warning));
    }
    Ok(HarnessBridgeRuntime::new(
        Repository::open(repo.root())?,
        user_config,
    ))
}

fn load_harness_user_config(default_path: Option<PathBuf>) -> (UserConfig, Option<String>) {
    let Some(path) = default_path else {
        return (UserConfig::default(), None);
    };
    match UserConfig::load(&path) {
        Ok(config) => (config, None),
        Err(err) if is_not_found(&err) => (UserConfig::default(), None),
        Err(err) => {
            let warning = format!(
                "warning: failed to load user config from {}: {err}; continuing with defaults",
                path.display()
            );
            (UserConfig::default(), Some(warning))
        }
    }
}

fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
}

fn parse_relay_payload(payload: &str) -> (Value, Option<String>) {
    if payload.trim().is_empty() {
        return (Value::Null, None);
    }
    match serde_json::from_str::<Value>(payload) {
        Ok(value) => (value, None),
        Err(err) => (
            Value::Null,
            Some(format!(
                "warning: failed to parse harness relay payload as JSON: {err}; continuing with null payload"
            )),
        ),
    }
}

struct HarnessBridgeRuntime {
    repo: Repository,
    user_config: UserConfig,
    reports: SessionReportStore,
}

struct RegistryEntryRequest<'a> {
    heddle_session_id: &'a str,
    thread_name: Option<&'a str>,
    thread_id: Option<&'a str>,
    identity: &'a ResolvedIdentity,
    probe: &'a HarnessProbeResult,
    attach: &'a ResolvedAttachment,
    client_instance_id: Option<&'a str>,
    requested_entry: Option<&'a AgentEntry>,
}

struct CanonicalActorSessionRequest<'a> {
    tentative_session: Session,
    tentative_owns_session: bool,
    entry: &'a AgentEntry,
    probe: &'a HarnessProbeResult,
    attach: &'a mut ResolvedAttachment,
}

struct AttachmentResolutionInput<'a> {
    requested_entry: Option<&'a AgentEntry>,
    explicit_heddle_session_id: Option<&'a str>,
    client_instance_id: Option<&'a str>,
    probe: &'a HarnessProbeResult,
    token_claims: Option<&'a TokenClaims>,
}

fn relay_codex(runtime: &mut HarnessBridgeRuntime, _event: &str, payload: &Value) -> Result<()> {
    let metadata = map_from_pairs([
        (
            "client_name",
            value_string(payload, &["client"]).or_else(|| value_string(payload, &["client_name"])),
        ),
        ("model", value_string(payload, &["model"])),
        (
            "model_provider",
            value_string(payload, &["model_provider"])
                .or_else(|| value_string(payload, &["provider"])),
        ),
        (
            "model_reasoning_effort",
            value_string(payload, &["reasoning_effort"]),
        ),
    ]);
    let opened = runtime.open_session(OpenSessionParams {
        harness: Some("codex".to_string()),
        summary: value_string(payload, &["message"]),
        probe_metadata: metadata,
        ..OpenSessionParams::default()
    })?;
    runtime.update_progress(UpdateProgressParams {
        heddle_session_id: opened.heddle_session_id,
        summary: value_string(payload, &["message"]),
        harness: Some("codex".to_string()),
        ..UpdateProgressParams::default()
    })?;
    Ok(())
}

fn relay_claude(runtime: &mut HarnessBridgeRuntime, event: &str, payload: &Value) -> Result<()> {
    let metadata = map_from_pairs([
        ("session_id", value_string(payload, &["session_id"])),
        ("agent_id", value_string(payload, &["agent_id"])),
        ("session_name", value_string(payload, &["session_name"])),
        (
            "transcript_path",
            value_string(payload, &["transcript_path"]),
        ),
        (
            "model",
            value_string(payload, &["model", "id"]).or_else(|| value_string(payload, &["model"])),
        ),
        (
            "model_display_name",
            value_string(payload, &["model", "display_name"]),
        ),
        ("effort", value_string(payload, &["effort"])),
        ("hook_event", Some(event.to_string())),
        (
            "status_line",
            (event == "StatusLine").then(|| "1".to_string()),
        ),
        (
            "touched_paths",
            value_array_join(payload, &["tool_response", "filePaths"])
                .or_else(|| value_string(payload, &["file_path"])),
        ),
        (
            "input_tokens",
            value_u64_string(payload, &["context_window", "total_input_tokens"]),
        ),
        (
            "output_tokens",
            value_u64_string(payload, &["context_window", "total_output_tokens"]),
        ),
        (
            "cost_micros_usd",
            value_cost_micros(payload, &["cost", "total_cost_usd"]),
        ),
    ]);
    let opened = runtime.open_session(OpenSessionParams {
        harness: Some("claude-code".to_string()),
        model: value_string(payload, &["model", "display_name"])
            .or_else(|| value_string(payload, &["model", "id"]))
            .or_else(|| value_string(payload, &["model"])),
        summary: value_string(payload, &["message"]).or_else(|| value_string(payload, &["reason"])),
        probe_metadata: metadata.clone(),
        ..OpenSessionParams::default()
    })?;
    match event {
        "SessionEnd" => {
            runtime.close_session(CloseSessionParams {
                heddle_session_id: opened.heddle_session_id,
                summary: value_string(payload, &["reason"])
                    .or_else(|| value_string(payload, &["stop_hook_active"])),
                outcome: Some("completed".to_string()),
                ..CloseSessionParams::default()
            })?;
        }
        "StatusLine" => {
            runtime.update_progress(UpdateProgressParams {
                heddle_session_id: opened.heddle_session_id.clone(),
                harness: Some("claude-code".to_string()),
                status: Some("StatusLine".to_string()),
                message: value_string(payload, &["session_name"])
                    .or_else(|| value_string(payload, &["cwd"]))
                    .or_else(|| value_string(payload, &["workspace", "current_dir"])),
                probe_metadata: metadata.clone(),
                ..UpdateProgressParams::default()
            })?;
            runtime.record_usage(RecordUsageParams {
                heddle_session_id: opened.heddle_session_id,
                input_tokens: value_u64(payload, &["context_window", "total_input_tokens"]),
                output_tokens: value_u64(payload, &["context_window", "total_output_tokens"]),
                reasoning_tokens: value_u64(payload, &["context_window", "total_reasoning_tokens"]),
                cache_creation_tokens: None,
                cache_read_tokens: None,
                tool_calls: None,
                cost_micros_usd: value_cost_micros_u64(payload, &["cost", "total_cost_usd"]),
            })?;
        }
        "Stop" => {
            runtime.update_progress(UpdateProgressParams {
                heddle_session_id: opened.heddle_session_id,
                harness: Some("claude-code".to_string()),
                status: Some("Stop".to_string()),
                message: value_string(payload, &["message"])
                    .or_else(|| value_string(payload, &["result"]))
                    .or_else(|| value_string(payload, &["stop_reason"])),
                probe_metadata: metadata,
                ..UpdateProgressParams::default()
            })?;
            if let Err(err) = claude_hook::handle_stop_capture(
                &runtime.repo,
                &runtime.user_config,
                payload,
                "Claude Code turn",
            ) {
                tracing::warn!(?err, "heddle Stop hook capture failed");
            }
        }
        "SubagentStop" => {
            runtime.update_progress(UpdateProgressParams {
                heddle_session_id: opened.heddle_session_id,
                harness: Some("claude-code".to_string()),
                status: Some("SubagentStop".to_string()),
                touched_paths: csv_from_value(metadata.get("touched_paths")),
                probe_metadata: metadata,
                ..UpdateProgressParams::default()
            })?;
            if let Err(err) = claude_hook::handle_stop_capture(
                &runtime.repo,
                &runtime.user_config,
                payload,
                "Claude Code subagent turn",
            ) {
                tracing::warn!(?err, "heddle SubagentStop hook capture failed");
            }
            if let Err(err) = claude_hook::mark_subagent_complete(&runtime.repo, payload) {
                tracing::debug!(?err, "heddle SubagentStop mark-complete failed");
            }
        }
        "SubagentStart" => {
            // open_session above has already created (or reattached) the
            // child `AgentEntry` with `native_parent_actor_key` pointing at
            // the parent session via the claude-code probe. The explicit
            // branch exists so the relay's behaviour is traceable in tests
            // and logs, and to preserve room for future subagent-specific
            // bookkeeping.
            runtime.update_progress(UpdateProgressParams {
                heddle_session_id: opened.heddle_session_id,
                harness: Some("claude-code".to_string()),
                status: Some("SubagentStart".to_string()),
                touched_paths: csv_from_value(metadata.get("touched_paths")),
                probe_metadata: metadata,
                ..UpdateProgressParams::default()
            })?;
        }
        "UserPromptSubmit" => {
            runtime.update_progress(UpdateProgressParams {
                heddle_session_id: opened.heddle_session_id.clone(),
                harness: Some("claude-code".to_string()),
                status: Some("UserPromptSubmit".to_string()),
                touched_paths: csv_from_value(metadata.get("touched_paths")),
                probe_metadata: metadata,
                ..UpdateProgressParams::default()
            })?;
            if let Err(err) = claude_hook::handle_user_prompt_segment_rotate(
                &runtime.repo,
                &opened.heddle_session_id,
                payload,
            ) {
                tracing::debug!(?err, "heddle UserPromptSubmit segment rotation failed");
            }
        }
        "PreToolUse" => {
            runtime.update_progress(UpdateProgressParams {
                heddle_session_id: opened.heddle_session_id,
                harness: Some("claude-code".to_string()),
                status: Some("PreToolUse".to_string()),
                touched_paths: csv_from_value(metadata.get("touched_paths")),
                probe_metadata: metadata,
                ..UpdateProgressParams::default()
            })?;
            if let Err(err) = claude_hook::handle_pre_tool_use(&runtime.repo, payload) {
                tracing::debug!(?err, "heddle PreToolUse context inject skipped");
            }
        }
        _ => {
            runtime.update_progress(UpdateProgressParams {
                heddle_session_id: opened.heddle_session_id,
                harness: Some("claude-code".to_string()),
                status: Some(event.to_string()),
                touched_paths: csv_from_value(metadata.get("touched_paths")),
                probe_metadata: metadata,
                ..UpdateProgressParams::default()
            })?;
        }
    }
    Ok(())
}

fn relay_opencode(runtime: &mut HarnessBridgeRuntime, event: &str, payload: &Value) -> Result<()> {
    let metadata = map_from_pairs([
        (
            "session_id",
            value_string(payload, &["sessionID"])
                .or_else(|| value_string(payload, &["session_id"])),
        ),
        (
            "parent_id",
            value_string(payload, &["parentID"]).or_else(|| value_string(payload, &["parent_id"])),
        ),
        (
            "client_name",
            value_string(payload, &["client"]).or_else(|| std::env::var("OPENCODE_CLIENT").ok()),
        ),
        ("model", value_string(payload, &["model"])),
        ("provider", value_string(payload, &["provider"])),
        ("hook_event", Some(event.to_string())),
        (
            "touched_paths",
            value_string(payload, &["file", "path"]).or_else(|| value_string(payload, &["path"])),
        ),
    ]);
    let opened = runtime.open_session(OpenSessionParams {
        harness: Some("opencode".to_string()),
        model: value_string(payload, &["model"]),
        provider: value_string(payload, &["provider"]),
        probe_metadata: metadata.clone(),
        ..OpenSessionParams::default()
    })?;
    let session_id = opened.heddle_session_id.clone();
    runtime.update_progress(UpdateProgressParams {
        heddle_session_id: session_id.clone(),
        harness: Some("opencode".to_string()),
        status: Some(event.to_string()),
        touched_paths: csv_from_value(metadata.get("touched_paths")),
        probe_metadata: metadata,
        ..UpdateProgressParams::default()
    })?;
    if let Err(err) = record_opencode_timeline_event(runtime, event, payload, &opened) {
        tracing::debug!(?err, event, "heddle OpenCode timeline recording skipped");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimelineToolEvent {
    Started,
    Finished,
}

trait HarnessTimelineExtractor {
    fn timeline_event(&self, event: &str) -> Option<TimelineToolEvent>;
    fn native_tool_call(&self, payload: &Value) -> Option<NativeToolCallRefV1>;
    fn tool_name(&self, payload: &Value) -> String;
    fn tool_status(&self, payload: &Value) -> TimelineToolCallStatus;
    fn payload_metadata(&self, event: &str, payload: &Value)
    -> Result<TimelineToolPayloadMetadata>;
    fn touched_paths(&self, payload: &Value) -> Vec<String>;
    fn capture_intent(&self, native: &NativeToolCallRefV1, payload: &Value) -> String;

    fn timeline_thread(
        &self,
        runtime: &HarnessBridgeRuntime,
        opened: &OpenSessionResult,
    ) -> Result<String> {
        if let Some(report) = runtime.reports.load(&opened.heddle_session_id)?
            && let Some(thread) = report.thread
        {
            return Ok(thread);
        }
        match runtime.repo.head_ref()? {
            Head::Attached { thread } => Ok(thread.to_string()),
            Head::Detached { .. } => Ok("main".to_string()),
        }
    }

    fn stable_step_id(&self, native: &NativeToolCallRefV1) -> TimelineStepId {
        let key = format!(
            "{}\0{}\0{}\0{}",
            native.harness,
            native.session_id.as_deref().unwrap_or(""),
            native.message_id.as_deref().unwrap_or(""),
            native.tool_call_id
        );
        let hash =
            ContentHash::compute_typed("timeline-native-tool-call-v1", key.as_bytes()).to_hex();
        TimelineStepId::new(format!("tls-{}", &hash[..24]))
    }

    fn started_labels(&self, _payload: &Value) -> Vec<TimelineLabel> {
        vec![TimelineLabel::ExternalSideEffectsUnknown]
    }

    fn finished_labels(&self, changed: bool, _payload: &Value) -> Vec<TimelineLabel> {
        if changed {
            vec![
                TimelineLabel::RepoReversible,
                TimelineLabel::ExternalSideEffectsUnknown,
            ]
        } else {
            vec![TimelineLabel::ExternalSideEffectsUnknown]
        }
    }
}

struct OpenCodeTimelineExtractor;

impl HarnessTimelineExtractor for OpenCodeTimelineExtractor {
    fn timeline_event(&self, event: &str) -> Option<TimelineToolEvent> {
        match event {
            "tool.execute.before" => Some(TimelineToolEvent::Started),
            "tool.execute.after" => Some(TimelineToolEvent::Finished),
            _ => None,
        }
    }

    fn native_tool_call(&self, payload: &Value) -> Option<NativeToolCallRefV1> {
        opencode_native_tool_call(payload)
    }

    fn tool_name(&self, payload: &Value) -> String {
        opencode_tool_name(payload)
    }

    fn tool_status(&self, payload: &Value) -> TimelineToolCallStatus {
        opencode_tool_status(payload)
    }

    fn payload_metadata(
        &self,
        event: &str,
        payload: &Value,
    ) -> Result<TimelineToolPayloadMetadata> {
        opencode_payload_metadata(event, payload)
    }

    fn touched_paths(&self, payload: &Value) -> Vec<String> {
        opencode_touched_paths(payload)
    }

    fn capture_intent(&self, native: &NativeToolCallRefV1, payload: &Value) -> String {
        format!(
            "OpenCode {} tool call {}",
            self.tool_name(payload),
            native.tool_call_id
        )
    }
}

fn record_opencode_timeline_event(
    runtime: &mut HarnessBridgeRuntime,
    event: &str,
    payload: &Value,
    opened: &OpenSessionResult,
) -> Result<()> {
    record_timeline_event(runtime, event, payload, opened, &OpenCodeTimelineExtractor)
}

fn record_timeline_event<E: HarnessTimelineExtractor>(
    runtime: &mut HarnessBridgeRuntime,
    event: &str,
    payload: &Value,
    opened: &OpenSessionResult,
    extractor: &E,
) -> Result<()> {
    match extractor.timeline_event(event) {
        Some(TimelineToolEvent::Started) => {
            record_timeline_tool_started(runtime, event, payload, opened, extractor)
        }
        Some(TimelineToolEvent::Finished) => {
            record_timeline_tool_finished(runtime, event, payload, opened, extractor)
        }
        None => Ok(()),
    }
}

fn record_timeline_tool_started<E: HarnessTimelineExtractor>(
    runtime: &mut HarnessBridgeRuntime,
    event: &str,
    payload: &Value,
    opened: &OpenSessionResult,
    extractor: &E,
) -> Result<()> {
    let Some(native) = extractor.native_tool_call(payload) else {
        return Ok(());
    };
    let Some(before_state) = current_change_id(&runtime.repo)? else {
        return Ok(());
    };
    let thread = extractor.timeline_thread(runtime, opened)?;
    let store = TimelineStore::open(runtime.repo.heddle_dir())?;
    let _record_guard = store.lock_recording(&thread)?;
    let view = TimelineView::rebuild(&store)?;
    let step_id = extractor.stable_step_id(&native);
    let (branch_id, parent_step_id) = timeline_position_for_new_tool_step(&view, &thread, &step_id);
    let envelope = TimelineOperationEnvelope::new(
        TimelineOperationBodyV1::ToolCallStarted(ToolCallStartedV1 {
            thread,
            step_id,
            branch_id,
            parent_step_id,
            native,
            tool_name: extractor.tool_name(payload),
            before_state,
            payload: Some(extractor.payload_metadata(event, payload)?),
            started_at_ms: Utc::now().timestamp_millis(),
        }),
        extractor.started_labels(payload),
    );
    store.write_operation(&envelope)?;
    Ok(())
}

fn record_timeline_tool_finished<E: HarnessTimelineExtractor>(
    runtime: &mut HarnessBridgeRuntime,
    event: &str,
    payload: &Value,
    opened: &OpenSessionResult,
    extractor: &E,
) -> Result<()> {
    let Some(native) = extractor.native_tool_call(payload) else {
        return Ok(());
    };
    let Some(fallback_state) = current_change_id(&runtime.repo)? else {
        return Ok(());
    };
    let thread = extractor.timeline_thread(runtime, opened)?;
    let store = TimelineStore::open(runtime.repo.heddle_dir())?;
    let _record_guard = store.lock_recording(&thread)?;
    let before_view = TimelineView::rebuild(&store)?;
    let step_id = extractor.stable_step_id(&native);
    let (branch_id, _) = timeline_position_for_new_tool_step(&before_view, &thread, &step_id);
    let before_state = before_view
        .step(&thread, &step_id)
        .and_then(|step| step.before_state)
        .unwrap_or(fallback_state);
    let has_worktree_changes_before_capture = !collect_worktree_changes(&runtime.repo)?.is_empty();
    let mut capture_failed = false;
    let capture_state = if !has_worktree_changes_before_capture {
        None
    } else {
        let intent = extractor.capture_intent(&native, payload);
        match create_snapshot(
            &runtime.repo,
            &runtime.user_config,
            Some(intent),
            None,
            SnapshotAgentOverrides {
                provider: opened.provider.clone(),
                model: opened.model.clone(),
                session: native.session_id.clone(),
                segment: None,
                policy: None,
                no_policy: false,
                no_agent: false,
            },
        ) {
            Ok(_) => runtime.repo.head()?,
            Err(err) => {
                capture_failed = true;
                tracing::warn!(?err, "heddle timeline tool capture failed");
                None
            }
        }
    };
    let after_state = current_change_id(&runtime.repo)?.unwrap_or(fallback_state);
    let mut touched_paths = extractor.touched_paths(payload);
    merge_string_vec(
        &mut touched_paths,
        changed_paths_between_states(&runtime.repo, before_state, after_state)?,
    );
    let changed = before_state != after_state;
    let mut labels = extractor.finished_labels(changed, payload);
    if capture_failed {
        merge_timeline_labels(&mut labels, vec![TimelineLabel::CaptureFailed]);
    }
    let envelope = TimelineOperationEnvelope::new(
        TimelineOperationBodyV1::ToolCallFinished(ToolCallFinishedV1 {
            thread,
            step_id,
            branch_id,
            native,
            status: extractor.tool_status(payload),
            before_state,
            after_state,
            capture_state,
            capture_oplog_batch_id: None,
            changed,
            touched_paths,
            payload: Some(extractor.payload_metadata(event, payload)?),
            finished_at_ms: Utc::now().timestamp_millis(),
        }),
        labels,
    );
    store.write_operation(&envelope)?;
    Ok(())
}

fn opencode_native_tool_call(payload: &Value) -> Option<NativeToolCallRefV1> {
    let tool_call_id = first_value_string(
        payload,
        &[
            &["toolCallID"],
            &["tool_call_id"],
            &["toolCallId"],
            &["callID"],
            &["call_id"],
            &["tool", "callID"],
            &["tool", "call_id"],
            &["tool", "id"],
            &["toolCall", "id"],
            &["tool_call", "id"],
            &["id"],
        ],
    )?;
    Some(NativeToolCallRefV1 {
        harness: "opencode".to_string(),
        session_id: value_string(payload, &["sessionID"])
            .or_else(|| value_string(payload, &["session_id"])),
        message_id: value_string(payload, &["messageID"])
            .or_else(|| value_string(payload, &["message_id"]))
            .or_else(|| value_string(payload, &["message", "id"])),
        tool_call_id,
    })
}

fn timeline_position_for_new_tool_step(
    view: &TimelineView,
    thread: &str,
    step_id: &TimelineStepId,
) -> (TimelineBranchId, Option<TimelineStepId>) {
    let branch_id = view
        .status(thread)
        .and_then(|status| status.current_branch_id.clone())
        .unwrap_or_else(|| TimelineBranchId::new("tlb-main"));
    let parent_step_id = view
        .status(thread)
        .and_then(|status| status.current_step_id.clone())
        .filter(|current| current != step_id);
    (branch_id, parent_step_id)
}

fn current_change_id(repo: &Repository) -> Result<Option<ChangeId>> {
    Ok(repo
        .current_state()?
        .map(|state| state.change_id)
        .or(repo.head()?))
}

fn opencode_tool_name(payload: &Value) -> String {
    first_value_string(
        payload,
        &[
            &["tool", "name"],
            &["toolName"],
            &["tool_name"],
            &["tool"],
            &["name"],
        ],
    )
    .unwrap_or_else(|| "tool".to_string())
}

fn opencode_tool_status(payload: &Value) -> TimelineToolCallStatus {
    let status = first_value_string(
        payload,
        &[
            &["status"],
            &["tool", "status"],
            &["result", "status"],
            &["output", "status"],
        ],
    )
    .unwrap_or_default()
    .to_ascii_lowercase();
    if status.contains("cancel") {
        TimelineToolCallStatus::Cancelled
    } else if status.contains("fail")
        || status.contains("error")
        || payload.get("error").is_some()
        || payload.get("exception").is_some()
    {
        TimelineToolCallStatus::Failed
    } else {
        TimelineToolCallStatus::Succeeded
    }
}

fn opencode_payload_metadata(event: &str, payload: &Value) -> Result<TimelineToolPayloadMetadata> {
    let tool_name = opencode_tool_name(payload);
    let tool_call_id = opencode_native_tool_call(payload)
        .map(|native| native.tool_call_id)
        .unwrap_or_default();
    let raw = serde_json::to_vec(payload)?;
    let hash = ContentHash::compute_typed("timeline-tool-payload", &raw);
    let summary = if tool_call_id.is_empty() {
        format!("OpenCode {event}: {tool_name}")
    } else {
        format!("OpenCode {event}: {tool_name} ({tool_call_id})")
    };
    Ok(TimelineToolPayloadMetadata {
        summary: Some(summary),
        hash: Some(hash),
    })
}

fn opencode_touched_paths(payload: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    for path in [
        value_string(payload, &["file", "path"]),
        value_string(payload, &["path"]),
        value_string(payload, &["tool", "path"]),
        value_string(payload, &["tool", "input", "file_path"]),
        value_string(payload, &["input", "file_path"]),
    ]
    .into_iter()
    .flatten()
    {
        if !path.trim().is_empty() && !paths.contains(&path) {
            paths.push(path);
        }
    }
    for value_path in [
        &["paths"][..],
        &["files"][..],
        &["tool", "input", "paths"][..],
        &["input", "paths"][..],
    ] {
        if let Some(items) = value_string_array(payload, value_path) {
            merge_string_vec(&mut paths, items);
        }
    }
    paths
}

fn first_value_string(value: &Value, paths: &[&[&str]]) -> Option<String> {
    paths.iter().find_map(|path| value_string(value, path))
}

fn value_string_array(value: &Value, path: &[&str]) -> Option<Vec<String>> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_array().map(|items| {
        items
            .iter()
            .filter_map(|item| item.as_str().map(ToString::to_string))
            .collect()
    })
}

fn merge_string_vec(target: &mut Vec<String>, incoming: Vec<String>) {
    for item in incoming {
        if !item.trim().is_empty() && !target.contains(&item) {
            target.push(item);
        }
    }
}

fn merge_timeline_labels(target: &mut Vec<TimelineLabel>, incoming: Vec<TimelineLabel>) {
    for label in incoming {
        if !target.contains(&label) {
            target.push(label);
        }
    }
}

fn value_string(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    match current {
        Value::String(s) => Some(s.clone()),
        Value::Bool(v) => Some(v.to_string()),
        Value::Number(v) => Some(v.to_string()),
        _ => None,
    }
}

fn value_array_join(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_array().map(|items| {
        items
            .iter()
            .filter_map(|item| item.as_str().map(ToString::to_string))
            .collect::<Vec<_>>()
            .join(",")
    })
}

fn value_u64_string(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_u64().map(|v| v.to_string())
}

fn value_u64(value: &Value, path: &[&str]) -> Option<u64> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_u64()
}

fn value_cost_micros(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current
        .as_f64()
        .map(|v| ((v * 1_000_000.0).round() as u64).to_string())
}

fn value_cost_micros_u64(value: &Value, path: &[&str]) -> Option<u64> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_f64().map(|v| (v * 1_000_000.0).round() as u64)
}

fn map_from_pairs<const N: usize>(pairs: [(&str, Option<String>); N]) -> BTreeMap<String, String> {
    pairs
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key.to_string(), value)))
        .collect()
}

fn csv_from_value(value: Option<&String>) -> Vec<String> {
    value
        .map(|value| {
            value
                .split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

impl HarnessBridgeRuntime {
    fn new(repo: Repository, user_config: UserConfig) -> Self {
        let reports = SessionReportStore::new(repo.root());
        Self {
            repo,
            user_config,
            reports,
        }
    }

    fn handle_request(&mut self, request: BridgeRequest) -> BridgeResponse {
        let response = match request.method.as_str() {
            "open_session" => self
                .decode_params::<OpenSessionParams>(request.params)
                .and_then(|params| self.open_session(params))
                .and_then(to_json_value),
            "update_progress" => self
                .decode_params::<UpdateProgressParams>(request.params)
                .and_then(|params| self.update_progress(params))
                .and_then(to_json_value),
            "record_usage" => self
                .decode_params::<RecordUsageParams>(request.params)
                .and_then(|params| self.record_usage(params))
                .and_then(to_json_value),
            "record_touched_paths" => self
                .decode_params::<RecordTouchedPathsParams>(request.params)
                .and_then(|params| self.record_touched_paths(params))
                .and_then(to_json_value),
            "close_session" => self
                .decode_params::<CloseSessionParams>(request.params)
                .and_then(|params| self.close_session(params))
                .and_then(to_json_value),
            "flush_reports" => self
                .decode_params::<FlushReportsParams>(request.params)
                .and_then(|params| self.flush_reports(params))
                .and_then(to_json_value),
            other => Err(anyhow!("unknown method '{other}'")),
        };

        match response {
            Ok(result) => BridgeResponse::ok(request.id, result),
            Err(err) => BridgeResponse::error(request.id, "bridge_error", err.to_string()),
        }
    }

    fn decode_params<T: for<'de> Deserialize<'de>>(&self, value: Value) -> Result<T> {
        serde_json::from_value(value).map_err(|err| anyhow!(err))
    }

    fn open_session(&mut self, params: OpenSessionParams) -> Result<OpenSessionResult> {
        if self.user_config.harness.mode == HarnessMode::Off {
            return Err(anyhow!("harness integration is disabled in user config"));
        }

        let requested_transport = params
            .transport
            .unwrap_or(self.user_config.harness.transport);
        let transcript_mode = params
            .transcript_mode
            .unwrap_or(self.user_config.harness.transcript);
        let env_hints = merged_env_hints(&params.env_hints);
        let token_claims = user_config_token_claims(&self.user_config);
        let current_session = SessionManager::new(self.repo.root()).get_current_session()?;
        let current_segment = current_session
            .as_ref()
            .and_then(|session| session.current_segment());
        let probe = probe_harness_actor(&HarnessProbeInput {
            argv: params.argv.clone(),
            env_hints: env_hints.clone(),
            explicit_harness: params.harness.clone(),
            explicit_provider: params.provider.clone(),
            explicit_model: params.model.clone(),
            explicit_thinking_level: params.thinking_level.clone(),
            explicit_policy: params.policy.clone(),
            probe_metadata: params.probe_metadata.clone(),
            current_provider: current_segment.map(|segment| segment.provider.clone()),
            current_model: current_segment.map(|segment| segment.model.clone()),
            current_policy: current_segment.and_then(|segment| segment.policy_id.clone()),
            repo_root: self.repo.root().display().to_string(),
        })?;
        let identity = resolve_identity(
            &self.repo,
            &self.user_config,
            IdentityHints {
                harness: params.harness.clone(),
                provider: params.provider.clone(),
                model: params.model.clone(),
                thinking_level: params.thinking_level.clone(),
                policy: params.policy.clone(),
                probe: probe.clone(),
            },
        )?;
        let registry = AgentRegistry::new(self.repo.heddle_dir());
        let requested_entry = resolve_requested_registry_entry(
            &registry,
            params.agent_session_id.as_deref(),
            params.client_instance_id.as_deref(),
        )?;

        if self.user_config.harness.mode == HarnessMode::Required
            && (identity.harness.is_none()
                || identity.provider.is_none()
                || identity.model.is_none())
        {
            return Err(anyhow!(
                "harness mode is 'required' but harness/provider/model could not be resolved"
            ));
        }

        let mut sessions = SessionManager::new(self.repo.root());
        let principal = self.repo.get_principal()?;
        let mut attach = resolve_actor_attachment(
            &registry,
            &self.repo,
            &mut sessions,
            AttachmentResolutionInput {
                requested_entry: requested_entry.as_ref(),
                explicit_heddle_session_id: params.heddle_session_id.as_deref(),
                client_instance_id: params.client_instance_id.as_deref(),
                probe: &probe,
                token_claims: token_claims.as_ref(),
            },
        )?;
        let (session, owns_session) = match &attach.target {
            AttachTarget::ExistingSession(session) => {
                let segment_id = session.current_segment_id.clone().unwrap_or_default();
                sessions.set_current_session(&session.id, &segment_id)?;
                (session.clone(), false)
            }
            AttachTarget::CreateNew {
                _because_claimed: _,
            } => {
                let session = sessions.start_session(
                    principal,
                    identity
                        .provider
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string()),
                    identity
                        .model
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string()),
                    identity.policy.clone(),
                )?;
                (session, true)
            }
        };

        let (thread_name, thread_id) =
            self.resolve_harness_thread_binding(&params, &probe, &identity)?;
        let entry = self.ensure_registry_entry(RegistryEntryRequest {
            heddle_session_id: &session.id,
            thread_name: thread_name.as_deref(),
            thread_id: thread_id.as_deref(),
            identity: &identity,
            probe: &probe,
            attach: &attach,
            client_instance_id: params.client_instance_id.as_deref(),
            requested_entry: requested_entry.as_ref(),
        })?;
        let (session, owns_session) = self.reuse_canonical_actor_session(
            &mut sessions,
            CanonicalActorSessionRequest {
                tentative_session: session,
                tentative_owns_session: owns_session,
                entry: &entry,
                probe: &probe,
                attach: &mut attach,
            },
        )?;

        let mut segment_id = session.current_segment_id.clone().unwrap_or_default();
        if should_rotate_segment(&session, &identity) {
            let segment = sessions.add_segment(
                &session.id,
                identity
                    .provider
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                identity
                    .model
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                identity.policy.clone(),
            )?;
            segment_id = segment.id;
        }

        let base_state = self
            .repo
            .current_state()?
            .map(|state| state.change_id.to_string_full())
            .or_else(|| {
                self.repo
                    .head()
                    .ok()
                    .flatten()
                    .map(|id| id.to_string_full())
            });
        let worktree_changes_at_open = capture_worktree_change_snapshot(&self.repo)?;
        let opened_at = Utc::now().to_rfc3339();
        let mut report = SessionReportEnvelope {
            version: 1,
            heddle_session_id: session.id.clone(),
            heddle_segment_id: (!segment_id.is_empty()).then_some(segment_id.clone()),
            agent_session_id: Some(entry.session_id.clone()),
            client_instance_id: entry.client_instance_id.clone(),
            native_actor_key: entry.native_actor_key.clone(),
            native_parent_actor_key: entry.native_parent_actor_key.clone(),
            native_instance_key: entry.native_instance_key.clone(),
            repo_root: self.repo.root().display().to_string(),
            thread: thread_name.clone(),
            thread_id,
            task: params.task.clone(),
            summary: params.summary.clone(),
            opened_at,
            closed_at: None,
            base_state_at_open: base_state.clone(),
            worktree_changes_at_open,
            head_state_at_close: None,
            transport_mode: transport_mode_name(requested_transport).to_string(),
            transcript_mode: transcript_mode_name(transcript_mode).to_string(),
            outcome: None,
            harness: identity.to_transport_identity(),
            progress: Vec::new(),
            usage: UsageTotals::default(),
            touched_paths: Vec::new(),
            changed_paths: Vec::new(),
            diff_summary: None,
            transcript_refs: Vec::new(),
            last_progress_at: None,
            report_flush_state: Some("pending-local".to_string()),
            attach_reason: Some(attach.attach_reason.clone()),
            attach_precedence: attach.precedence.clone(),
            winning_attach_rule: Some(attach.winning_rule.clone()),
            probe_source: probe.probe_source.clone(),
            probe_confidence: probe.confidence,
            pending_flush: true,
            last_flushed_at: None,
            owns_session,
        };
        merge_unique_paths(&mut report.touched_paths, probe.touched_paths.clone());
        merge_usage(&mut report.usage, &probe.usage_totals);
        if transcript_mode != HarnessTranscriptMode::Off {
            report.transcript_refs = probe.transcript_refs.clone();
        }
        self.reports.save(&report)?;
        self.sync_registry_from_report(&report, AgentStatus::Active)?;
        if matches!(requested_transport, HarnessTransport::Direct) {
            enqueue_report(&self.reports, &mut report)?;
            self.sync_registry_from_report(&report, AgentStatus::Active)?;
        }

        Ok(OpenSessionResult {
            heddle_session_id: report.heddle_session_id.clone(),
            heddle_segment_id: report.heddle_segment_id.clone(),
            agent_session_id: report.agent_session_id.clone(),
            created_session: owns_session,
            harness: report.harness.harness.clone(),
            provider: report.harness.provider.clone(),
            model: report.harness.model.clone(),
            thinking_level: report.harness.thinking_level.clone(),
            report_flush_state: report.report_flush_state.clone(),
            attach_reason: report.attach_reason.clone(),
        })
    }

    fn update_progress(&mut self, params: UpdateProgressParams) -> Result<SessionMutationResult> {
        let mut report = self
            .reports
            .load(&params.heddle_session_id)?
            .ok_or_else(|| anyhow!("session report not found for {}", params.heddle_session_id))?;
        let current_session = SessionManager::new(self.repo.root()).get_current_session()?;
        let current_segment = current_session
            .as_ref()
            .and_then(|session| session.current_segment());
        let probe = probe_harness_actor(&HarnessProbeInput {
            argv: params.argv.clone(),
            env_hints: merged_env_hints(&params.env_hints),
            explicit_harness: params.harness.clone(),
            explicit_provider: params.provider.clone(),
            explicit_model: params.model.clone(),
            explicit_thinking_level: params.thinking_level.clone(),
            explicit_policy: params.policy.clone(),
            probe_metadata: params.probe_metadata.clone(),
            current_provider: current_segment.map(|segment| segment.provider.clone()),
            current_model: current_segment.map(|segment| segment.model.clone()),
            current_policy: current_segment.and_then(|segment| segment.policy_id.clone()),
            repo_root: self.repo.root().display().to_string(),
        })?;
        let identity = resolve_identity(
            &self.repo,
            &self.user_config,
            IdentityHints {
                harness: params.harness.clone(),
                provider: params.provider.clone(),
                model: params.model.clone(),
                thinking_level: params.thinking_level.clone(),
                policy: params.policy.clone(),
                probe: probe.clone(),
            },
        )?;
        self.ensure_segment_for_report(&mut report, &identity)?;
        if report.harness.harness.is_none() {
            report.harness.harness = identity.harness.clone();
        }
        if report.harness.provider.is_none() {
            report.harness.provider = identity.provider.clone();
        }
        if report.harness.model.is_none() {
            report.harness.model = identity.model.clone();
        }
        if report.harness.thinking_level.is_none() {
            report.harness.thinking_level = identity.thinking_level.clone();
        }
        if report.harness.policy.is_none() {
            report.harness.policy = identity.policy.clone();
        }
        if report.native_actor_key.is_none() {
            report.native_actor_key = probe.native_actor_key.clone();
        }
        if report.native_parent_actor_key.is_none() {
            report.native_parent_actor_key = probe.native_parent_actor_key.clone();
        }
        if report.native_instance_key.is_none() {
            report.native_instance_key = probe.native_instance_key.clone();
        }
        if report.probe_source.is_none() {
            report.probe_source = probe.probe_source.clone();
        }
        if report.probe_confidence.is_none() {
            report.probe_confidence = probe.confidence;
        }

        let recorded_at = Utc::now().to_rfc3339();
        let checkpoint = ProgressCheckpoint {
            status: params.status.clone(),
            message: params.message.clone(),
            completed_steps: params.completed_steps,
            total_steps: params.total_steps,
            touched_paths: normalize_paths(
                params
                    .touched_paths
                    .into_iter()
                    .chain(probe.touched_paths)
                    .collect::<Vec<_>>(),
            ),
            recorded_at: recorded_at.clone(),
        };
        merge_unique_paths(
            &mut report.touched_paths,
            checkpoint.touched_paths.iter().cloned(),
        );
        merge_usage(&mut report.usage, &probe.usage_totals);
        if report.transcript_mode != "off" && report.transcript_refs.is_empty() {
            report.transcript_refs = probe.transcript_refs;
        }
        report.progress.push(checkpoint);
        if let Some(summary) = params.summary {
            report.summary = Some(summary);
        }
        report.last_progress_at = Some(recorded_at);
        mark_pending_flush(&mut report);
        self.persist_report(report)
    }

    fn resolve_harness_thread_binding(
        &self,
        params: &OpenSessionParams,
        probe: &HarnessProbeResult,
        identity: &ResolvedIdentity,
    ) -> Result<(Option<String>, Option<String>)> {
        if let Some(thread) = params.thread.clone() {
            let thread_id = thread_id_for_name(&self.repo, Some(&thread))?;
            return Ok((Some(thread), thread_id));
        }

        let current_attached = match self.repo.head_ref()? {
            Head::Attached { thread } => Some(thread.to_string()),
            Head::Detached { .. } => None,
        };

        if !probe.attach_hints.root_actor
            && self.user_config.harness.threading.subagent
                == UserHarnessSubagentThreadPolicy::CreateChild
            && let Some(parent_thread) =
                resolve_parent_thread_for_subagent(&self.repo, probe, current_attached.as_deref())?
            && can_create_harness_thread(&self.repo, Some(&parent_thread), Some(&parent_thread))?
        {
            let name = allocate_thread_name(
                &self.repo,
                &format!(
                    "{}/{}",
                    parent_thread,
                    sanitize_name(&preferred_thread_slug(params, probe, identity))
                ),
            )?;
            self.ensure_harness_thread(
                &name,
                Some(&parent_thread),
                Some(&parent_thread),
                params.task.clone(),
            )?;
            let thread_id = thread_id_for_name(&self.repo, Some(&name))?;
            return Ok((Some(name), thread_id));
        }

        if probe.attach_hints.root_actor
            && self.user_config.harness.threading.root_actor
                == UserHarnessRootThreadPolicy::CreateNew
            && let Some(current) = current_attached.clone()
            && can_create_harness_thread(&self.repo, Some(&current), None)?
        {
            let name = allocate_thread_name(
                &self.repo,
                &format!(
                    "{}/{}",
                    current,
                    sanitize_name(&preferred_thread_slug(params, probe, identity))
                ),
            )?;
            self.ensure_harness_thread(&name, Some(&current), None, params.task.clone())?;
            let thread_id = thread_id_for_name(&self.repo, Some(&name))?;
            return Ok((Some(name), thread_id));
        }

        let thread_id = thread_id_for_name(&self.repo, current_attached.as_deref())?;
        Ok((current_attached, thread_id))
    }

    fn ensure_harness_thread(
        &self,
        name: &str,
        target_thread: Option<&str>,
        parent_thread: Option<&str>,
        task: Option<String>,
    ) -> Result<()> {
        let manager = ThreadManager::new(self.repo.heddle_dir());
        if manager.load(name)?.is_some() {
            return Ok(());
        }

        let base_state = self
            .resolve_harness_thread_base_state(target_thread, parent_thread)?
            .ok_or_else(|| anyhow!("No current state to start a thread from"))?;
        let tn = ThreadName::new(name);
        if self.repo.refs().get_thread(&tn)?.is_none() {
            self.repo
                .refs()
                .set_thread_cas(&tn, refs::RefExpectation::Missing, &base_state)?;
            // Harness writes the ThreadManager record later in this
            // function (after materializing); no record exists to
            // snapshot at recording time. `None` matches the pattern
            // used by `cmd_start` / agent reservation. heddle#23 r2.
            self.repo.oplog().record_thread_create(
                &tn,
                &base_state,
                None,
                Some(&self.repo.op_scope()),
            )?;
        }

        let workspace_mode = self
            .user_config
            .harness
            .threading
            .workspace_default
            .unwrap_or(UserThreadWorkspaceMode::Materialized);
        let thread_mode = match workspace_mode {
            UserThreadWorkspaceMode::Materialized | UserThreadWorkspaceMode::Auto => {
                ThreadMode::Materialized
            }
            UserThreadWorkspaceMode::Virtualized => ThreadMode::Virtualized,
            UserThreadWorkspaceMode::Solid => ThreadMode::Solid,
        };
        let path = match thread_mode {
            ThreadMode::Solid | ThreadMode::Materialized => {
                default_private_thread_path(&self.repo, name)
            }
            // Harness-managed light workspaces still need mount lifecycle
            // wiring before they can become the default execution root.
            ThreadMode::Virtualized => default_private_thread_path(&self.repo, name),
        };
        let abs_path = prepare_worktree_target(&self.repo, &path, Some(name))?.path;
        write_isolated_checkout(&self.repo, &abs_path, &base_state, Some(name))?;

        let base_state_obj = self
            .repo
            .store()
            .get_state(&base_state)?
            .ok_or_else(|| anyhow!("Base state '{}' not found", base_state.short()))?;
        let thread = Thread {
            id: name.to_string(),
            thread: name.to_string(),
            target_thread: target_thread.map(ToString::to_string),
            parent_thread: parent_thread.map(ToString::to_string),
            mode: thread_mode.clone(),
            state: ThreadState::Active,
            base_state: base_state.short(),
            base_root: base_state_obj.tree.short(),
            current_state: Some(base_state.short()),
            merged_state: None,
            task,
            execution_path: abs_path.clone(),
            materialized_path: match thread_mode {
                ThreadMode::Solid => Some(abs_path),
                // See note above: harness can't currently produce
                // Virtualized, so defaulting to None matches the
                // Lightweight branch.
                ThreadMode::Materialized | ThreadMode::Virtualized => None,
            },
            changed_paths: vec![],
            impact_categories: vec![],
            heavy_impact_paths: vec![],
            promotion_suggested: false,
            freshness: if target_thread.is_some() {
                ThreadFreshness::Current
            } else {
                ThreadFreshness::Unknown
            },
            verification_summary: summarize_verification(base_state_obj.verification.as_ref()),
            confidence_summary: summarize_confidence(base_state_obj.confidence),
            integration_policy_result: ThreadIntegrationPolicy::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            ephemeral: None,
            // Mark this as harness-created so `heddle thread list`
            // hides it by default and `heddle thread cleanup --auto`
            // can sweep it once stale. (Item 2.2 of the heddle 6→8
            // plan.)
            auto: true,
            // The harness's create-on-rotate path doesn't materialize
            // a heavy checkout, so there's nothing to redirect.
            shared_target_dir: None,
        };
        manager.save(&thread)?;
        Ok(())
    }

    fn resolve_harness_thread_base_state(
        &self,
        target_thread: Option<&str>,
        parent_thread: Option<&str>,
    ) -> Result<Option<objects::object::ChangeId>> {
        resolve_harness_thread_base_state(&self.repo, target_thread, parent_thread)
    }

    fn record_usage(&mut self, params: RecordUsageParams) -> Result<SessionMutationResult> {
        let mut report = self
            .reports
            .load(&params.heddle_session_id)?
            .ok_or_else(|| anyhow!("session report not found for {}", params.heddle_session_id))?;
        if let Some(input) = params.input_tokens {
            report.usage.input_tokens = Some(max_u64(report.usage.input_tokens, input));
        }
        if let Some(output) = params.output_tokens {
            report.usage.output_tokens = Some(max_u64(report.usage.output_tokens, output));
        }
        if let Some(reasoning) = params.reasoning_tokens {
            report.usage.reasoning_tokens = Some(max_u64(report.usage.reasoning_tokens, reasoning));
        }
        if let Some(cache_creation) = params.cache_creation_tokens {
            report.usage.cache_creation_tokens =
                Some(max_u64(report.usage.cache_creation_tokens, cache_creation));
        }
        if let Some(cache_read) = params.cache_read_tokens {
            report.usage.cache_read_tokens =
                Some(max_u64(report.usage.cache_read_tokens, cache_read));
        }
        if let Some(tool_calls) = params.tool_calls {
            report.usage.tool_calls = Some(max_u32(report.usage.tool_calls, tool_calls));
        }
        if let Some(cost) = params.cost_micros_usd {
            report.usage.cost_micros_usd = Some(max_u64(report.usage.cost_micros_usd, cost));
        }
        mark_pending_flush(&mut report);
        self.persist_report(report)
    }

    fn record_touched_paths(
        &mut self,
        params: RecordTouchedPathsParams,
    ) -> Result<SessionMutationResult> {
        let mut report = self
            .reports
            .load(&params.heddle_session_id)?
            .ok_or_else(|| anyhow!("session report not found for {}", params.heddle_session_id))?;
        merge_unique_paths(&mut report.touched_paths, normalize_paths(params.paths));
        mark_pending_flush(&mut report);
        self.persist_report(report)
    }

    fn close_session(&mut self, params: CloseSessionParams) -> Result<CloseSessionResult> {
        let mut report = self
            .reports
            .load(&params.heddle_session_id)?
            .ok_or_else(|| anyhow!("session report not found for {}", params.heddle_session_id))?;
        report.closed_at = Some(Utc::now().to_rfc3339());
        report.outcome = params.outcome.clone();
        if let Some(summary) = params.summary {
            report.summary = Some(summary);
        }
        if let Some(transcript_refs) = params.transcript_refs {
            report.transcript_refs = transcript_refs;
        }
        let final_diff = compute_final_diff(
            &self.repo,
            report.base_state_at_open.as_deref(),
            &report.worktree_changes_at_open,
        )?;
        report.head_state_at_close = final_diff.head_state;
        report.changed_paths = final_diff.changed_paths;
        report.diff_summary = Some(final_diff.diff_summary);
        mark_pending_flush(&mut report);
        if report.owns_session {
            let mut sessions = SessionManager::new(self.repo.root());
            if let Ok(Some(session)) = sessions.get_session(&report.heddle_session_id)
                && session.is_active()
            {
                let _ = sessions.end_session(Some(&report.heddle_session_id));
            }
        }

        let transport = params
            .transport
            .unwrap_or(self.user_config.harness.transport);
        if matches!(transport, HarnessTransport::Direct | HarnessTransport::End) {
            enqueue_report(&self.reports, &mut report)?;
        } else {
            self.reports.save(&report)?;
        }
        self.sync_registry_from_report(&report, AgentStatus::Complete)?;
        Ok(CloseSessionResult {
            heddle_session_id: report.heddle_session_id,
            changed_paths: report.changed_paths,
            diff_summary: report.diff_summary.unwrap_or_default(),
            report_flush_state: report.report_flush_state,
        })
    }

    fn flush_reports(&mut self, params: FlushReportsParams) -> Result<FlushReportsResult> {
        let mut flushed = 0usize;
        let session_ids = match params.heddle_session_id {
            Some(session_id) => vec![session_id],
            None => self.reports.list_pending()?,
        };
        for session_id in session_ids {
            let Some(mut report) = self.reports.load(&session_id)? else {
                continue;
            };
            if !report.pending_flush {
                continue;
            }
            enqueue_report(&self.reports, &mut report)?;
            let status = if report.closed_at.is_some() {
                AgentStatus::Complete
            } else {
                AgentStatus::Active
            };
            self.sync_registry_from_report(&report, status)?;
            flushed += 1;
        }
        Ok(FlushReportsResult { flushed })
    }

    fn persist_report(
        &mut self,
        mut report: SessionReportEnvelope,
    ) -> Result<SessionMutationResult> {
        let transport = transport_from_report(&report, self.user_config.harness.transport);
        match transport {
            HarnessTransport::Direct => {
                enqueue_report(&self.reports, &mut report)?;
            }
            HarnessTransport::Spool | HarnessTransport::End => {
                self.reports.save(&report)?;
            }
        }
        self.sync_registry_from_report(&report, AgentStatus::Active)?;
        Ok(SessionMutationResult {
            heddle_session_id: report.heddle_session_id,
            heddle_segment_id: report.heddle_segment_id,
            report_flush_state: report.report_flush_state,
        })
    }

    fn ensure_segment_for_report(
        &self,
        report: &mut SessionReportEnvelope,
        identity: &ResolvedIdentity,
    ) -> Result<()> {
        let mut sessions = SessionManager::new(self.repo.root());
        let Some(session) = sessions.get_session(&report.heddle_session_id)? else {
            return Ok(());
        };
        if !session.is_active() || !should_rotate_segment(&session, identity) {
            return Ok(());
        }
        let segment = sessions.add_segment(
            &report.heddle_session_id,
            identity
                .provider
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            identity
                .model
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            identity.policy.clone(),
        )?;
        report.heddle_segment_id = Some(segment.id);
        if identity.provider.is_some() {
            report.harness.provider = identity.provider.clone();
        }
        if identity.model.is_some() {
            report.harness.model = identity.model.clone();
        }
        if identity.policy.is_some() {
            report.harness.policy = identity.policy.clone();
        }
        if identity.thinking_level.is_some() {
            report.harness.thinking_level = identity.thinking_level.clone();
        }
        Ok(())
    }

    fn ensure_registry_entry(&self, request: RegistryEntryRequest<'_>) -> Result<AgentEntry> {
        let RegistryEntryRequest {
            heddle_session_id,
            thread_name,
            thread_id,
            identity,
            probe,
            attach,
            client_instance_id,
            requested_entry,
        } = request;
        let registry = AgentRegistry::new(self.repo.heddle_dir());
        let fallback_entry = if client_instance_id.is_some()
            || probe.native_actor_key.is_some()
            || probe.native_instance_key.is_some()
        {
            None
        } else {
            find_matching_registry_entry(&registry, &self.repo, heddle_session_id, thread_name)?
        };
        if let Some(entry) = requested_entry
            .cloned()
            .or_else(|| attach.matched_entry.clone())
            .or(fallback_entry)
        {
            return registry
                .update_entry(&entry.session_id, |existing| {
                    if client_instance_id.is_some() {
                        existing.client_instance_id = client_instance_id.map(ToString::to_string);
                    }
                    if probe.native_actor_key.is_some() {
                        existing.native_actor_key = probe.native_actor_key.clone();
                    }
                    if probe.native_parent_actor_key.is_some() {
                        existing.native_parent_actor_key = probe.native_parent_actor_key.clone();
                    }
                    if probe.native_instance_key.is_some() {
                        existing.native_instance_key = probe.native_instance_key.clone();
                    }
                    existing.heddle_session_id = Some(heddle_session_id.to_string());
                    existing.thread_id = thread_id.map(ToString::to_string);
                    if let Some(thread_name) = thread_name {
                        existing.thread = thread_name.to_string();
                    }
                    existing.path = Some(self.repo.root().to_path_buf());
                    if identity.provider.is_some() {
                        existing.provider = identity.provider.clone();
                    }
                    if identity.model.is_some() {
                        existing.model = identity.model.clone();
                    }
                    if identity.harness.is_some() {
                        existing.harness = identity.harness.clone();
                    }
                    if identity.thinking_level.is_some() {
                        existing.thinking_level = identity.thinking_level.clone();
                    }
                    existing.attach_reason = Some(attach.attach_reason.clone());
                    existing.attach_precedence = attach.precedence.clone();
                    existing.winning_attach_rule = Some(attach.winning_rule.clone());
                    existing.probe_source = probe.probe_source.clone();
                    existing.probe_confidence = probe.confidence;
                    existing.status = AgentStatus::Active;
                })?
                .ok_or_else(|| anyhow!("registry entry disappeared during update"));
        }

        if client_instance_id.is_none() && probe.native_actor_key.is_some() {
            let (entry, _) = registry.find_or_create_active_entry(
                |entry| {
                    claude_actor_compatible(entry, probe, self.repo.root())
                        && entry.native_actor_key == probe.native_actor_key
                },
                |existing| {
                    if client_instance_id.is_some() {
                        existing.client_instance_id = client_instance_id.map(ToString::to_string);
                    }
                    if existing.heddle_session_id.is_none() {
                        existing.heddle_session_id = Some(heddle_session_id.to_string());
                    }
                    existing.thread_id = thread_id.map(ToString::to_string);
                    if let Some(thread_name) = thread_name {
                        existing.thread = thread_name.to_string();
                    }
                    existing.path = Some(self.repo.root().to_path_buf());
                    if identity.provider.is_some() {
                        existing.provider = identity.provider.clone();
                    }
                    if identity.model.is_some() {
                        existing.model = identity.model.clone();
                    }
                    if identity.harness.is_some() {
                        existing.harness = identity.harness.clone();
                    }
                    if identity.thinking_level.is_some() {
                        existing.thinking_level = identity.thinking_level.clone();
                    }
                    if probe.native_parent_actor_key.is_some() {
                        existing.native_parent_actor_key = probe.native_parent_actor_key.clone();
                    }
                    if probe.native_instance_key.is_some() {
                        existing.native_instance_key = probe.native_instance_key.clone();
                    }
                    existing.attach_reason = Some(attach.attach_reason.clone());
                    existing.attach_precedence = attach.precedence.clone();
                    existing.winning_attach_rule = Some(attach.winning_rule.clone());
                    existing.probe_source = probe.probe_source.clone();
                    existing.probe_confidence = probe.confidence;
                    existing.status = AgentStatus::Active;
                },
                |session_id| {
                    Ok(AgentEntry {
                        session_id: session_id.to_string(),
                        client_instance_id: client_instance_id.map(ToString::to_string),
                        native_actor_key: probe.native_actor_key.clone(),
                        native_parent_actor_key: probe.native_parent_actor_key.clone(),
                        native_instance_key: probe.native_instance_key.clone(),
                        heddle_session_id: Some(heddle_session_id.to_string()),
                        thread_id: thread_id.map(ToString::to_string),
                        thread: thread_name.unwrap_or("detached").to_string(),
                        pid: Some(std::process::id()),
                        boot_id: None,
                        liveness_path: None,
                        heartbeat_at: Some(Utc::now()),
                        anchor_state: self.repo.head()?.map(|id| id.to_string_full()),
                        anchor_root: None,
                        reservation_token: Some(objects::store::generate_agent_id()),
                        path: Some(self.repo.root().to_path_buf()),
                        base_state: self.repo.head()?.map(|id| id.short()).unwrap_or_default(),
                        started_at: Utc::now(),
                        provider: identity.provider.clone(),
                        model: identity.model.clone(),
                        harness: identity.harness.clone(),
                        thinking_level: identity.thinking_level.clone(),
                        usage_summary: AgentUsageSummary::default(),
                        last_progress_at: None,
                        report_flush_state: Some("pending-local".to_string()),
                        attach_reason: Some(attach.attach_reason.clone()),
                        attach_precedence: attach.precedence.clone(),
                        winning_attach_rule: Some(attach.winning_rule.clone()),
                        probe_source: probe.probe_source.clone(),
                        probe_confidence: probe.confidence,
                        status: AgentStatus::Active,
                        completed_at: None,
                        context_queries: vec![],
                    })
                },
            )?;
            return Ok(entry);
        }

        Ok(registry.create_generated_entry(|session_id| {
            Ok(AgentEntry {
                session_id: session_id.to_string(),
                client_instance_id: client_instance_id.map(ToString::to_string),
                native_actor_key: probe.native_actor_key.clone(),
                native_parent_actor_key: probe.native_parent_actor_key.clone(),
                native_instance_key: probe.native_instance_key.clone(),
                heddle_session_id: Some(heddle_session_id.to_string()),
                thread_id: thread_id.map(ToString::to_string),
                thread: thread_name.unwrap_or("detached").to_string(),
                pid: Some(std::process::id()),
                boot_id: None,
                liveness_path: None,
                heartbeat_at: Some(Utc::now()),
                anchor_state: self.repo.head()?.map(|id| id.to_string_full()),
                anchor_root: None,
                reservation_token: Some(objects::store::generate_agent_id()),
                path: Some(self.repo.root().to_path_buf()),
                base_state: self.repo.head()?.map(|id| id.short()).unwrap_or_default(),
                started_at: Utc::now(),
                provider: identity.provider.clone(),
                model: identity.model.clone(),
                harness: identity.harness.clone(),
                thinking_level: identity.thinking_level.clone(),
                usage_summary: AgentUsageSummary::default(),
                last_progress_at: None,
                report_flush_state: Some("pending-local".to_string()),
                attach_reason: Some(attach.attach_reason.clone()),
                attach_precedence: attach.precedence.clone(),
                winning_attach_rule: Some(attach.winning_rule.clone()),
                probe_source: probe.probe_source.clone(),
                probe_confidence: probe.confidence,
                status: AgentStatus::Active,
                completed_at: None,
                context_queries: vec![],
            })
        })?)
    }

    fn reuse_canonical_actor_session(
        &self,
        sessions: &mut SessionManager,
        request: CanonicalActorSessionRequest<'_>,
    ) -> Result<(Session, bool)> {
        let CanonicalActorSessionRequest {
            tentative_session,
            tentative_owns_session,
            entry,
            probe,
            attach,
        } = request;
        let Some(canonical_session_id) = entry.heddle_session_id.as_deref() else {
            return Ok((tentative_session, tentative_owns_session));
        };
        if canonical_session_id == tentative_session.id {
            return Ok((tentative_session, tentative_owns_session));
        }

        if tentative_owns_session
            && let Ok(Some(session)) = sessions.get_session(&tentative_session.id)
            && session.is_active()
        {
            let _ = sessions.end_session(Some(&tentative_session.id));
        }

        let canonical_session = sessions
            .get_session(canonical_session_id)?
            .ok_or_else(|| anyhow!("session not found: {canonical_session_id}"))?;
        let canonical_segment_id = canonical_session
            .current_segment_id
            .clone()
            .unwrap_or_default();
        sessions.set_current_session(canonical_session_id, &canonical_segment_id)?;

        if let Some(native_actor_key) = probe
            .native_actor_key
            .as_deref()
            .or(entry.native_actor_key.as_deref())
        {
            attach.precedence.push(format!(
                "post-create-native-actor-key:{native_actor_key}:matched"
            ));
            attach.attach_reason = format!(
                "reused existing native actor {} on Heddle session {}",
                native_actor_key, canonical_session_id
            );
            attach.winning_rule = "native-actor-key-post-create".to_string();
        }

        Ok((canonical_session, false))
    }

    fn sync_registry_from_report(
        &self,
        report: &SessionReportEnvelope,
        status: AgentStatus,
    ) -> Result<()> {
        let registry = AgentRegistry::new(self.repo.heddle_dir());
        let entry = if let Some(agent_session_id) = &report.agent_session_id {
            registry.update_entry(agent_session_id, |entry| {
                if report.client_instance_id.is_some() {
                    entry.client_instance_id = report.client_instance_id.clone();
                }
                if report.native_actor_key.is_some() {
                    entry.native_actor_key = report.native_actor_key.clone();
                }
                if report.native_parent_actor_key.is_some() {
                    entry.native_parent_actor_key = report.native_parent_actor_key.clone();
                }
                if report.native_instance_key.is_some() {
                    entry.native_instance_key = report.native_instance_key.clone();
                }
                entry.heddle_session_id = Some(report.heddle_session_id.clone());
                entry.path = Some(self.repo.root().to_path_buf());
                entry.harness = report.harness.harness.clone();
                entry.provider = report.harness.provider.clone();
                entry.model = report.harness.model.clone();
                entry.thinking_level = report.harness.thinking_level.clone();
                entry.usage_summary = usage_to_summary(&report.usage);
                entry.last_progress_at =
                    report.last_progress_at.as_deref().and_then(parse_timestamp);
                entry.report_flush_state = report.report_flush_state.clone();
                entry.attach_reason = report.attach_reason.clone();
                entry.attach_precedence = report.attach_precedence.clone();
                entry.winning_attach_rule = report.winning_attach_rule.clone();
                entry.probe_source = report.probe_source.clone();
                entry.probe_confidence = report.probe_confidence;
                entry.status = status.clone();
                entry.completed_at = match status {
                    AgentStatus::Active => None,
                    AgentStatus::Abandoned | AgentStatus::Complete | AgentStatus::Merged => {
                        Some(Utc::now())
                    }
                };
            })?
        } else {
            None
        };

        if entry.is_none() {
            let resolved = self.ensure_registry_entry(RegistryEntryRequest {
                heddle_session_id: &report.heddle_session_id,
                thread_name: report.thread.as_deref(),
                thread_id: report.thread_id.as_deref(),
                identity: &ResolvedIdentity {
                    harness: report.harness.harness.clone(),
                    provider: report.harness.provider.clone(),
                    model: report.harness.model.clone(),
                    thinking_level: report.harness.thinking_level.clone(),
                    policy: report.harness.policy.clone(),
                },
                probe: &HarnessProbeResult {
                    native_actor_key: report.native_actor_key.clone(),
                    native_parent_actor_key: report.native_parent_actor_key.clone(),
                    native_instance_key: report.native_instance_key.clone(),
                    probe_source: report.probe_source.clone(),
                    confidence: report.probe_confidence,
                    ..HarnessProbeResult::default()
                },
                attach: &ResolvedAttachment {
                    target: AttachTarget::CreateNew {
                        _because_claimed: false,
                    },
                    matched_entry: None,
                    attach_reason: report.attach_reason.clone().unwrap_or_else(|| {
                        format!(
                            "created actor for Heddle session {}",
                            report.heddle_session_id
                        )
                    }),
                    precedence: report.attach_precedence.clone(),
                    winning_rule: report
                        .winning_attach_rule
                        .clone()
                        .unwrap_or_else(|| "report-sync".to_string()),
                },
                client_instance_id: report.client_instance_id.as_deref(),
                requested_entry: None,
            })?;
            let mut report = report.clone();
            report.agent_session_id = Some(resolved.session_id);
            self.reports.save(&report)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
struct ResolvedIdentity {
    harness: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    thinking_level: Option<String>,
    policy: Option<String>,
}

impl ResolvedIdentity {
    fn to_transport_identity(&self) -> HarnessIdentity {
        HarnessIdentity {
            harness: self.harness.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            thinking_level: self.thinking_level.clone(),
            policy: self.policy.clone(),
        }
    }
}

struct IdentityHints {
    harness: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    thinking_level: Option<String>,
    policy: Option<String>,
    probe: HarnessProbeResult,
}

fn resolve_identity(
    repo: &Repository,
    user_config: &UserConfig,
    hints: IdentityHints,
) -> Result<ResolvedIdentity> {
    let current_session = SessionManager::new(repo.root()).get_current_session()?;
    let current_segment = current_session
        .as_ref()
        .and_then(|session| session.current_segment());
    let token_claims = if user_config.harness.auto_infer {
        user_config_token_claims(user_config)
    } else {
        None
    };
    let harness_override = resolved_harness_override(
        user_config,
        hints.harness.as_deref(),
        hints.probe.harness.as_deref(),
    );

    Ok(ResolvedIdentity {
        harness: hints.harness.or(hints.probe.harness),
        provider: hints
            .provider
            .or(hints.probe.provider)
            .or_else(|| current_segment.map(|segment| segment.provider.clone()))
            .or_else(|| {
                token_claims
                    .as_ref()
                    .and_then(|claims| claims.agent_provider.clone())
            })
            .or_else(|| harness_override.and_then(|entry| entry.provider.clone()))
            .or_else(|| user_config.agent.provider.clone()),
        model: hints
            .model
            .or(hints.probe.model)
            .or_else(|| current_segment.map(|segment| segment.model.clone()))
            .or_else(|| {
                token_claims
                    .as_ref()
                    .and_then(|claims| claims.agent_model.clone())
            })
            .or_else(|| harness_override.and_then(|entry| entry.model.clone()))
            .or_else(|| user_config.agent.model.clone()),
        thinking_level: hints
            .thinking_level
            .or(hints.probe.thinking_level)
            .or_else(|| harness_override.and_then(|entry| entry.thinking_level.clone())),
        policy: hints
            .policy
            .or(hints.probe.policy)
            .or_else(|| current_segment.and_then(|segment| segment.policy_id.clone()))
            .or_else(|| harness_override.and_then(|entry| entry.policy.clone()))
            .or_else(|| user_config.agent.default_policy.clone()),
    })
}

fn resolved_harness_override<'a>(
    user_config: &'a UserConfig,
    explicit: Option<&str>,
    fingerprint: Option<&str>,
) -> Option<&'a UserHarnessOverride> {
    explicit
        .and_then(|name| user_config.harness.harnesses.get(name))
        .or_else(|| fingerprint.and_then(|name| user_config.harness.harnesses.get(name)))
}

enum AttachTarget {
    ExistingSession(objects::object::Session),
    CreateNew { _because_claimed: bool },
}

struct ResolvedAttachment {
    target: AttachTarget,
    matched_entry: Option<AgentEntry>,
    attach_reason: String,
    precedence: Vec<String>,
    winning_rule: String,
}

fn resolve_actor_attachment(
    registry: &AgentRegistry,
    repo: &Repository,
    sessions: &mut SessionManager,
    input: AttachmentResolutionInput<'_>,
) -> Result<ResolvedAttachment> {
    let AttachmentResolutionInput {
        requested_entry,
        explicit_heddle_session_id,
        client_instance_id,
        probe,
        token_claims,
    } = input;
    let mut precedence = Vec::new();
    if let Some(entry) = requested_entry
        && let Some(bound_session_id) = entry.heddle_session_id.as_deref()
    {
        precedence.push(format!(
            "explicit-agent-session:{}:matched",
            entry.session_id
        ));
        let session = sessions
            .get_session(bound_session_id)?
            .ok_or_else(|| anyhow!("session not found: {bound_session_id}"))?;
        if !session.is_active() {
            return Err(anyhow!("session is not active: {bound_session_id}"));
        }
        return Ok(ResolvedAttachment {
            target: AttachTarget::ExistingSession(session),
            matched_entry: Some(entry.clone()),
            attach_reason: format!(
                "reattached actor {} to existing Heddle session {}",
                entry.session_id, bound_session_id
            ),
            precedence,
            winning_rule: "explicit-agent-session".to_string(),
        });
    }
    precedence.push("explicit-agent-session:miss".to_string());

    if let Some(session_id) = explicit_heddle_session_id {
        precedence.push(format!("explicit-heddle-session:{session_id}:matched"));
        ensure_requested_entry_matches_session(requested_entry, session_id)?;
        let session = sessions
            .get_session(session_id)?
            .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        if !session.is_active() {
            return Err(anyhow!("session is not active: {session_id}"));
        }
        return Ok(ResolvedAttachment {
            target: AttachTarget::ExistingSession(session),
            matched_entry: None,
            attach_reason: format!("attached to explicit Heddle session {session_id}"),
            precedence,
            winning_rule: "explicit-heddle-session".to_string(),
        });
    }
    precedence.push("explicit-heddle-session:miss".to_string());

    if client_instance_id.is_none()
        && let Some(native_actor_key) = probe.native_actor_key.as_deref()
    {
        if let Some(entry) = registry.find_active_by_native_actor_key(native_actor_key)?
            && claude_actor_compatible(&entry, probe, repo.root())
            && let Some(bound_session_id) = entry.heddle_session_id.clone()
        {
            precedence.push(format!("native-actor-key:{native_actor_key}:matched"));
            let session = sessions
                .get_session(&bound_session_id)?
                .ok_or_else(|| anyhow!("session not found: {bound_session_id}"))?;
            if session.is_active() {
                return Ok(ResolvedAttachment {
                    target: AttachTarget::ExistingSession(session),
                    matched_entry: Some(entry),
                    attach_reason: format!(
                        "reattached native actor {} to Heddle session {}",
                        native_actor_key, bound_session_id
                    ),
                    precedence,
                    winning_rule: "native-actor-key".to_string(),
                });
            }
        }
        precedence.push(format!("native-actor-key:{native_actor_key}:miss"));
    } else {
        precedence.push("native-actor-key:miss".to_string());
    }

    if let Some(client_instance_id) = client_instance_id {
        if let Some(entry) = registry.find_active_by_client_instance_id(client_instance_id)?
            && let Some(bound_session_id) = entry.heddle_session_id.clone()
        {
            precedence.push(format!("client-instance-id:{client_instance_id}:matched"));
            let session = sessions
                .get_session(&bound_session_id)?
                .ok_or_else(|| anyhow!("session not found: {bound_session_id}"))?;
            if session.is_active() {
                return Ok(ResolvedAttachment {
                    target: AttachTarget::ExistingSession(session),
                    matched_entry: Some(entry),
                    attach_reason: format!(
                        "reattached client instance {client_instance_id} to Heddle session {bound_session_id}"
                    ),
                    precedence,
                    winning_rule: "client-instance-id".to_string(),
                });
            }
        }
        precedence.push(format!("client-instance-id:{client_instance_id}:miss"));
        return Ok(ResolvedAttachment {
            target: AttachTarget::CreateNew {
                _because_claimed: false,
            },
            matched_entry: None,
            attach_reason: format!(
                "started new Heddle session for distinct client instance {client_instance_id}"
            ),
            precedence,
            winning_rule: "create-new-session".to_string(),
        });
    } else {
        precedence.push("client-instance-id:miss".to_string());
    }

    if client_instance_id.is_none() && probe.native_actor_key.is_some() {
        precedence.push("native-instance-key:skipped-strong-native-key".to_string());
        return Ok(ResolvedAttachment {
            target: AttachTarget::CreateNew {
                _because_claimed: false,
            },
            matched_entry: None,
            attach_reason:
                "started new Heddle session because no compatible native actor match was found"
                    .to_string(),
            precedence,
            winning_rule: "create-new-session".to_string(),
        });
    }

    if let Some(native_instance_key) = probe.native_instance_key.as_deref() {
        if let Some(entry) =
            registry.find_active_by_native_instance_key_at_path(native_instance_key, repo.root())?
            && claude_actor_compatible(&entry, probe, repo.root())
            && let Some(bound_session_id) = entry.heddle_session_id.clone()
        {
            precedence.push(format!("native-instance-key:{native_instance_key}:matched"));
            let session = sessions
                .get_session(&bound_session_id)?
                .ok_or_else(|| anyhow!("session not found: {bound_session_id}"))?;
            if session.is_active() {
                return Ok(ResolvedAttachment {
                    target: AttachTarget::ExistingSession(session),
                    matched_entry: Some(entry),
                    attach_reason: format!(
                        "reattached native instance {} to Heddle session {}",
                        native_instance_key, bound_session_id
                    ),
                    precedence,
                    winning_rule: "native-instance-key".to_string(),
                });
            }
        }
        precedence.push(format!("native-instance-key:{native_instance_key}:miss"));
    } else {
        precedence.push("native-instance-key:miss".to_string());
    }

    if probe.attach_hints.root_actor
        && let Some(current) = sessions.get_current_session()?
        && current.is_active()
    {
        let claimed = session_claimed_by_other(
            registry,
            &current.id,
            requested_entry,
            client_instance_id,
            probe.native_actor_key.as_deref(),
        )?;
        if !claimed {
            precedence.push(format!("current-worktree-session:{}:matched", current.id));
            return Ok(ResolvedAttachment {
                target: AttachTarget::ExistingSession(current.clone()),
                matched_entry: None,
                attach_reason: format!("attached to active worktree Heddle session {}", current.id),
                precedence,
                winning_rule: "current-worktree-session".to_string(),
            });
        }
        precedence.push(format!("current-worktree-session:{}:claimed", current.id));
        return Ok(ResolvedAttachment {
            target: AttachTarget::CreateNew {
                _because_claimed: true,
            },
            matched_entry: None,
            attach_reason: "started a new Heddle session because the current session was already claimed by another active actor".to_string(),
            precedence,
            winning_rule: "create-new-session".to_string(),
        });
    }
    precedence.push("current-worktree-session:miss".to_string());

    if let Some(claims) = token_claims
        && let Some(token_sid) = claims.sid.as_deref()
        && let Some(session) = sessions.get_session(token_sid)?
        && session.is_active()
    {
        let claimed = session_claimed_by_other(
            registry,
            &session.id,
            requested_entry,
            client_instance_id,
            probe.native_actor_key.as_deref(),
        )?;
        if !claimed {
            precedence.push(format!("token-sid:{token_sid}:matched"));
            return Ok(ResolvedAttachment {
                target: AttachTarget::ExistingSession(session),
                matched_entry: None,
                attach_reason: format!(
                    "attached to Heddle session {token_sid} from auth token sid"
                ),
                precedence,
                winning_rule: "token-sid".to_string(),
            });
        }
        precedence.push(format!("token-sid:{token_sid}:claimed"));
        return Ok(ResolvedAttachment {
            target: AttachTarget::CreateNew {
                _because_claimed: true,
            },
            matched_entry: None,
            attach_reason: "started a new Heddle session because the current session was already claimed by another active actor".to_string(),
            precedence,
            winning_rule: "create-new-session".to_string(),
        });
    }
    precedence.push("token-sid:miss".to_string());

    Ok(ResolvedAttachment {
        target: AttachTarget::CreateNew {
            _because_claimed: false,
        },
        matched_entry: None,
        attach_reason: "started new Heddle session".to_string(),
        precedence,
        winning_rule: "create-new-session".to_string(),
    })
}

fn claude_actor_compatible(
    entry: &AgentEntry,
    probe: &HarnessProbeResult,
    repo_root: &Path,
) -> bool {
    let Some(native_actor_key) = probe.native_actor_key.as_deref() else {
        return true;
    };
    if !native_actor_key.starts_with("claude-code:") {
        return true;
    }
    if native_actor_key.starts_with("claude-code:agent:") {
        return entry.native_actor_key.as_deref() == Some(native_actor_key);
    }
    if let Some(native_instance_key) = probe.native_instance_key.as_deref() {
        return entry.native_actor_key.as_deref() == Some(native_actor_key)
            && entry.native_instance_key.as_deref() == Some(native_instance_key);
    }
    let same_repo = entry
        .path
        .as_ref()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
        .unwrap_or_default()
        == repo_root
            .canonicalize()
            .unwrap_or_else(|_| repo_root.to_path_buf());
    entry.native_actor_key.as_deref() == Some(native_actor_key)
        && same_repo
        && probe.confidence.unwrap_or_default() >= 0.9
}

fn decode_token_claims(token: &str) -> Option<TokenClaims> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.as_bytes())
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn user_config_token_claims(user_config: &UserConfig) -> Option<TokenClaims> {
    user_config
        .remote_token()
        .ok()
        .flatten()
        .and_then(|token| decode_token_claims(&token.id))
}

#[derive(Debug, Deserialize)]
struct TokenClaims {
    #[serde(default)]
    sid: Option<String>,
    #[serde(default)]
    agent_provider: Option<String>,
    #[serde(default)]
    agent_model: Option<String>,
}

fn should_rotate_segment(session: &objects::object::Session, identity: &ResolvedIdentity) -> bool {
    let Some(segment) = session.current_segment() else {
        return false;
    };
    let provider_changed = identity
        .provider
        .as_deref()
        .is_some_and(|provider| provider != segment.provider);
    let model_changed = identity
        .model
        .as_deref()
        .is_some_and(|model| model != segment.model);
    provider_changed || model_changed
}

fn thread_id_for_name(repo: &Repository, thread_name: Option<&str>) -> Result<Option<String>> {
    let Some(thread_name) = thread_name else {
        return Ok(None);
    };
    Ok(ThreadManager::new(repo.heddle_dir())
        .load(thread_name)?
        .map(|thread| thread.id))
}

fn can_create_harness_thread(
    repo: &Repository,
    target_thread: Option<&str>,
    parent_thread: Option<&str>,
) -> Result<bool> {
    Ok(resolve_harness_thread_base_state(repo, target_thread, parent_thread)?.is_some())
}

fn resolve_harness_thread_base_state(
    repo: &Repository,
    target_thread: Option<&str>,
    parent_thread: Option<&str>,
) -> Result<Option<objects::object::ChangeId>> {
    if let Some(head_state) = repo.head()? {
        return Ok(Some(head_state));
    }

    for thread_name in [parent_thread, target_thread].into_iter().flatten() {
        if let Some(state) = resolve_named_thread_base_state(repo, thread_name)? {
            return Ok(Some(state));
        }
    }

    Ok(None)
}

fn resolve_named_thread_base_state(
    repo: &Repository,
    thread_name: &str,
) -> Result<Option<objects::object::ChangeId>> {
    if let Some(thread) = ThreadManager::new(repo.heddle_dir()).load(thread_name)?
        && let Some(state_spec) = thread
            .current_state
            .as_deref()
            .or(Some(thread.base_state.as_str()))
        && let Some(state_id) = repo
            .resolve_state(state_spec)?
            .or_else(|| objects::object::ChangeId::parse(state_spec).ok())
    {
        return Ok(Some(state_id));
    }

    Ok(repo.refs().get_thread(&ThreadName::new(thread_name))?)
}

fn resolve_parent_thread_for_subagent(
    repo: &Repository,
    probe: &HarnessProbeResult,
    current_attached: Option<&str>,
) -> Result<Option<String>> {
    if let Some(parent_key) = probe.native_parent_actor_key.as_deref() {
        let registry = AgentRegistry::new(repo.heddle_dir());
        if let Some(entry) = registry.find_active_by_native_actor_key(parent_key)? {
            return Ok(Some(entry.thread));
        }
    }
    Ok(current_attached.map(ToString::to_string))
}

fn preferred_thread_slug(
    params: &OpenSessionParams,
    probe: &HarnessProbeResult,
    identity: &ResolvedIdentity,
) -> String {
    params
        .task
        .clone()
        .or_else(|| params.summary.clone())
        .or_else(|| probe.native_actor_key.as_deref().map(native_key_slug))
        .or_else(|| probe.native_instance_key.as_deref().map(native_key_slug))
        .or_else(|| identity.harness.clone())
        .unwrap_or_else(|| "work".to_string())
}

fn native_key_slug(value: &str) -> String {
    value
        .rsplit(':')
        .next()
        .map(ToString::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn allocate_thread_name(repo: &Repository, base: &str) -> Result<String> {
    if ThreadManager::new(repo.heddle_dir()).load(base)?.is_none()
        && repo.refs().get_thread(&ThreadName::new(base))?.is_none()
    {
        return Ok(base.to_string());
    }
    for idx in 2..1000 {
        let candidate = format!("{base}-{idx}");
        if ThreadManager::new(repo.heddle_dir())
            .load(&candidate)?
            .is_none()
            && repo
                .refs()
                .get_thread(&ThreadName::new(&candidate))?
                .is_none()
        {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "could not allocate a unique thread name from '{base}'"
    ))
}

fn default_private_thread_path(repo: &Repository, name: &str) -> PathBuf {
    // Route through the ONE canonical `thread_manifest::thread_dir`
    // derivation `heddle start` and the per-thread `manifest.toml` sidecar
    // use — NOT a harness-local re-sanitisation. Harness subagent/root-actor
    // names are commonly slash-namespaced (`parent/task`); a local
    // `sanitize_name` flattened `parent/task` and `parent-task` onto the same
    // `.heddle/threads/parent-task/<repo-name>`, colliding two distinct threads and
    // diverging from the manifest/checkout layout (heddle#572 r2).
    repo.managed_checkout_path(name)
}

fn sanitize_name(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn resolve_requested_registry_entry(
    registry: &AgentRegistry,
    agent_session_id: Option<&str>,
    client_instance_id: Option<&str>,
) -> Result<Option<AgentEntry>> {
    if let Some(agent_session_id) = agent_session_id {
        let entry = registry
            .load(agent_session_id)?
            .ok_or_else(|| anyhow!("agent session not found: {agent_session_id}"))?;
        if entry.status != AgentStatus::Active {
            return Err(anyhow!("agent session is not active: {agent_session_id}"));
        }
        return Ok(Some(entry));
    }

    if let Some(client_instance_id) = client_instance_id {
        return Ok(registry.find_active_by_client_instance_id(client_instance_id)?);
    }

    Ok(None)
}

fn ensure_requested_entry_matches_session(
    requested_entry: Option<&AgentEntry>,
    heddle_session_id: &str,
) -> Result<()> {
    if let Some(entry) = requested_entry
        && let Some(bound_session_id) = entry.heddle_session_id.as_deref()
        && bound_session_id != heddle_session_id
    {
        return Err(anyhow!(
            "requested agent is already bound to a different heddle session: {}",
            entry.session_id
        ));
    }
    Ok(())
}

fn session_claimed_by_other(
    registry: &AgentRegistry,
    heddle_session_id: &str,
    requested_entry: Option<&AgentEntry>,
    client_instance_id: Option<&str>,
    native_actor_key: Option<&str>,
) -> Result<bool> {
    if requested_entry.is_none() && client_instance_id.is_none() && native_actor_key.is_none() {
        return Ok(false);
    }

    let Some(existing) = registry.find_active_by_heddle_session_id(heddle_session_id)? else {
        return Ok(false);
    };
    if let Some(requested) = requested_entry {
        return Ok(requested.session_id != existing.session_id);
    }
    if let Some(client_instance_id) = client_instance_id
        && existing.client_instance_id.as_deref() == Some(client_instance_id)
    {
        return Ok(false);
    }
    if let Some(native_actor_key) = native_actor_key
        && existing.native_actor_key.as_deref() == Some(native_actor_key)
    {
        return Ok(false);
    }
    Ok(true)
}

fn find_matching_registry_entry(
    registry: &AgentRegistry,
    repo: &Repository,
    heddle_session_id: &str,
    thread_name: Option<&str>,
) -> Result<Option<AgentEntry>> {
    if let Some(entry) = registry.find_active_by_heddle_session_id(heddle_session_id)? {
        return Ok(Some(entry));
    }
    let canonical_root = repo
        .root()
        .canonicalize()
        .unwrap_or_else(|_| repo.root().to_path_buf());
    Ok(registry
        .list()?
        .into_iter()
        .filter(|entry| entry.status == AgentStatus::Active)
        .find(|entry| {
            entry
                .path
                .as_ref()
                .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()) == canonical_root)
                .unwrap_or(false)
                || thread_name.is_some_and(|thread| entry.thread == thread)
        }))
}

fn merged_env_hints(extra: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut merged: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, _)| inherited_harness_hint(key))
        .collect();
    for (key, value) in extra {
        merged.insert(key.clone(), value.clone());
    }
    merged
}

fn inherited_harness_hint(key: &str) -> bool {
    if matches!(
        key,
        "OPENAI_MODEL"
            | "ANTHROPIC_MODEL"
            | "CLAUDE_MODEL"
            | "MODEL"
            | "OPENAI_REASONING_EFFORT"
            | "REASONING_EFFORT"
            | "THINKING_LEVEL"
            | "PROMPT_POLICY"
    ) {
        return false;
    }

    key.starts_with("HEDDLE_")
        || key.starts_with("CODEX_")
        || key == "CLAUDECODE"
        || key.starts_with("OPENCODE_")
}

fn to_json_value<T: Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(|err| anyhow!(err))
}

fn normalize_paths<I>(paths: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut ordered = BTreeSet::new();
    for path in paths {
        let normalized = path.trim().replace('\\', "/");
        if !normalized.is_empty() {
            ordered.insert(normalized);
        }
    }
    ordered.into_iter().collect()
}

fn merge_unique_paths<I>(target: &mut Vec<String>, paths: I)
where
    I: IntoIterator<Item = String>,
{
    let mut merged: BTreeSet<String> = target.iter().cloned().collect();
    merged.extend(paths);
    *target = merged.into_iter().collect();
}

fn max_u64(current: Option<u64>, candidate: u64) -> u64 {
    current
        .map(|value| value.max(candidate))
        .unwrap_or(candidate)
}

fn max_u32(current: Option<u32>, candidate: u32) -> u32 {
    current
        .map(|value| value.max(candidate))
        .unwrap_or(candidate)
}

fn merge_usage(target: &mut UsageTotals, incoming: &UsageTotals) {
    if let Some(input) = incoming.input_tokens {
        target.input_tokens = Some(max_u64(target.input_tokens, input));
    }
    if let Some(output) = incoming.output_tokens {
        target.output_tokens = Some(max_u64(target.output_tokens, output));
    }
    if let Some(reasoning) = incoming.reasoning_tokens {
        target.reasoning_tokens = Some(max_u64(target.reasoning_tokens, reasoning));
    }
    if let Some(cache_creation) = incoming.cache_creation_tokens {
        target.cache_creation_tokens = Some(max_u64(target.cache_creation_tokens, cache_creation));
    }
    if let Some(cache_read) = incoming.cache_read_tokens {
        target.cache_read_tokens = Some(max_u64(target.cache_read_tokens, cache_read));
    }
    if let Some(tool_calls) = incoming.tool_calls {
        target.tool_calls = Some(max_u32(target.tool_calls, tool_calls));
    }
    if let Some(cost) = incoming.cost_micros_usd {
        target.cost_micros_usd = Some(max_u64(target.cost_micros_usd, cost));
    }
}

fn parse_timestamp(value: &str) -> Option<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn transport_from_report(
    report: &SessionReportEnvelope,
    fallback: HarnessTransport,
) -> HarnessTransport {
    match report.transport_mode.as_str() {
        "spool" => HarnessTransport::Spool,
        "direct" => HarnessTransport::Direct,
        "end" => HarnessTransport::End,
        _ => fallback,
    }
}

fn mark_pending_flush(report: &mut SessionReportEnvelope) {
    report.pending_flush = true;
    report.report_flush_state = Some("pending-local".to_string());
}

fn enqueue_report(store: &SessionReportStore, report: &mut SessionReportEnvelope) -> Result<()> {
    store.append_outbox(report)?;
    report.pending_flush = false;
    let flushed_at = Utc::now().to_rfc3339();
    report.last_flushed_at = Some(flushed_at);
    report.report_flush_state = Some("queued-local".to_string());
    store.save(report)?;
    Ok(())
}

fn usage_to_summary(usage: &UsageTotals) -> AgentUsageSummary {
    AgentUsageSummary {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        tool_calls: usage.tool_calls,
        cost_micros_usd: usage.cost_micros_usd,
    }
}

fn transcript_mode_name(mode: HarnessTranscriptMode) -> &'static str {
    match mode {
        HarnessTranscriptMode::Off => "off",
        HarnessTranscriptMode::Summary => "summary",
        HarnessTranscriptMode::Full => "full",
    }
}

fn transport_mode_name(mode: HarnessTransport) -> &'static str {
    match mode {
        HarnessTransport::Spool => "spool",
        HarnessTransport::Direct => "direct",
        HarnessTransport::End => "end",
    }
}

struct FinalDiff {
    changed_paths: Vec<String>,
    diff_summary: SessionDiffSummary,
    head_state: Option<String>,
}

fn compute_final_diff(
    repo: &Repository,
    base_state: Option<&str>,
    worktree_baseline: &[WorktreeChangeBaseline],
) -> Result<FinalDiff> {
    let mut changes: BTreeMap<String, DiffKind> = BTreeMap::new();

    let head_state = repo.head()?;
    if let (Some(base_spec), Some(head_id)) = (base_state, head_state) {
        let base_id = repo
            .resolve_state(base_spec)?
            .or_else(|| objects::object::ChangeId::parse(base_spec).ok());
        if let Some(base_id) = base_id
            && base_id != head_id
        {
            let Some(base_state_obj) = repo.store().get_state(&base_id)? else {
                return Err(anyhow!("base state not found: {base_spec}"));
            };
            let Some(head_state_obj) = repo.store().get_state(&head_id)? else {
                return Err(anyhow!("head state not found: {}", head_id.short()));
            };
            for change in repo.diff_trees(&base_state_obj.tree, &head_state_obj.tree)? {
                changes.insert(change.path, change.kind);
            }
        }
    }

    let baseline_paths: BTreeSet<(String, String)> = worktree_baseline
        .iter()
        .map(|change| (change.path.clone(), change.kind.clone()))
        .collect();
    for (path, kind) in collect_worktree_changes(repo)? {
        let kind_name = diff_kind_name(kind);
        if !baseline_paths.contains(&(path.clone(), kind_name.to_string())) {
            changes.insert(path, kind);
        }
    }

    let diff_summary = SessionDiffSummary {
        changed_file_count: changes.len() as u32,
        added_files: changes
            .values()
            .filter(|kind| **kind == DiffKind::Added)
            .count() as u32,
        modified_files: changes
            .values()
            .filter(|kind| **kind == DiffKind::Modified)
            .count() as u32,
        deleted_files: changes
            .values()
            .filter(|kind| **kind == DiffKind::Deleted)
            .count() as u32,
    };

    Ok(FinalDiff {
        changed_paths: changes.into_keys().collect(),
        diff_summary,
        head_state: head_state.map(|id| id.to_string_full()),
    })
}

fn capture_worktree_change_snapshot(repo: &Repository) -> Result<Vec<WorktreeChangeBaseline>> {
    Ok(collect_worktree_changes(repo)?
        .into_iter()
        .map(|(path, kind)| WorktreeChangeBaseline {
            path,
            kind: diff_kind_name(kind).to_string(),
        })
        .collect())
}

fn collect_worktree_changes(repo: &Repository) -> Result<BTreeMap<String, DiffKind>> {
    let status_options = worktree_status_options(Some(repo.config()));
    let worktree_tree = match repo.current_state()? {
        Some(state) => repo.require_tree(&state.tree)?,
        None => Tree::new(),
    };
    let status = repo.compare_worktree_cached_with_options(&worktree_tree, &status_options)?;
    let mut changes = BTreeMap::new();
    for path in status.added {
        changes.insert(path.display().to_string(), DiffKind::Added);
    }
    for path in status.modified {
        changes.insert(path.display().to_string(), DiffKind::Modified);
    }
    for path in status.deleted {
        changes.insert(path.display().to_string(), DiffKind::Deleted);
    }
    Ok(changes)
}

fn changed_paths_between_states(
    repo: &Repository,
    before_state: ChangeId,
    after_state: ChangeId,
) -> Result<Vec<String>> {
    if before_state == after_state {
        return Ok(Vec::new());
    }
    let Some(before_state_obj) = repo.store().get_state(&before_state)? else {
        return Err(anyhow!(
            "timeline before state not found: {}",
            before_state.short()
        ));
    };
    let Some(after_state_obj) = repo.store().get_state(&after_state)? else {
        return Err(anyhow!(
            "timeline after state not found: {}",
            after_state.short()
        ));
    };
    let mut paths = BTreeSet::new();
    for change in repo.diff_trees(&before_state_obj.tree, &after_state_obj.tree)? {
        paths.insert(change.path);
    }
    Ok(paths.into_iter().collect())
}

fn diff_kind_name(kind: DiffKind) -> &'static str {
    match kind {
        DiffKind::Added => "added",
        DiffKind::Modified => "modified",
        DiffKind::Deleted => "deleted",
        DiffKind::Unchanged => "unchanged",
    }
}

struct SessionReportStore {
    dir: PathBuf,
}

impl SessionReportStore {
    fn new(repo_root: &Path) -> Self {
        Self {
            dir: repo_root.join(".heddle/state").join("session-reports"),
        }
    }

    fn session_path(&self, heddle_session_id: &str) -> PathBuf {
        self.dir.join(format!("{heddle_session_id}.json"))
    }

    fn outbox_path(&self) -> PathBuf {
        self.dir.join("outbox.jsonl")
    }

    fn load(&self, heddle_session_id: &str) -> Result<Option<SessionReportEnvelope>> {
        let path = self.session_path(heddle_session_id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path)?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    fn save(&self, report: &SessionReportEnvelope) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.session_path(&report.heddle_session_id);
        let bytes = serde_json::to_vec_pretty(report)?;
        write_file_atomic(&path, &bytes)?;
        Ok(())
    }

    fn append_outbox(&self, report: &SessionReportEnvelope) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.outbox_path())?;
        serde_json::to_writer(&mut file, report)?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }

    fn list_pending(&self) -> Result<Vec<String>> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(&path)?;
            let report: SessionReportEnvelope = serde_json::from_slice(&bytes)?;
            if report.pending_flush {
                ids.push(report.heddle_session_id);
            }
        }
        ids.sort();
        Ok(ids)
    }
}

#[derive(Debug, Deserialize)]
struct BridgeRequest {
    #[serde(default)]
    id: Option<String>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct BridgeResponse {
    #[serde(default)]
    id: Option<String>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<BridgeError>,
}

impl BridgeResponse {
    fn ok(id: Option<String>, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Option<String>, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(BridgeError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Serialize)]
struct BridgeError {
    code: String,
    message: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct OpenSessionParams {
    #[serde(default)]
    heddle_session_id: Option<String>,
    #[serde(default)]
    agent_session_id: Option<String>,
    #[serde(default)]
    client_instance_id: Option<String>,
    #[serde(default)]
    thread: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    thinking_level: Option<String>,
    #[serde(default)]
    policy: Option<String>,
    #[serde(default)]
    transport: Option<HarnessTransport>,
    #[serde(default)]
    transcript_mode: Option<HarnessTranscriptMode>,
    #[serde(default)]
    argv: Option<Vec<String>>,
    #[serde(default)]
    env_hints: BTreeMap<String, String>,
    #[serde(default)]
    probe_metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct UpdateProgressParams {
    heddle_session_id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    completed_steps: Option<u32>,
    #[serde(default)]
    total_steps: Option<u32>,
    #[serde(default)]
    touched_paths: Vec<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    thinking_level: Option<String>,
    #[serde(default)]
    policy: Option<String>,
    #[serde(default)]
    argv: Option<Vec<String>>,
    #[serde(default)]
    env_hints: BTreeMap<String, String>,
    #[serde(default)]
    probe_metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RecordUsageParams {
    heddle_session_id: String,
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    reasoning_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_tokens: Option<u64>,
    #[serde(default)]
    cache_read_tokens: Option<u64>,
    #[serde(default)]
    tool_calls: Option<u32>,
    #[serde(default)]
    cost_micros_usd: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RecordTouchedPathsParams {
    heddle_session_id: String,
    #[serde(default)]
    paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct CloseSessionParams {
    heddle_session_id: String,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    transcript_refs: Option<Vec<TranscriptAttachmentRef>>,
    #[serde(default)]
    transport: Option<HarnessTransport>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct FlushReportsParams {
    #[serde(default)]
    heddle_session_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct OpenSessionResult {
    heddle_session_id: String,
    heddle_segment_id: Option<String>,
    agent_session_id: Option<String>,
    created_session: bool,
    harness: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    thinking_level: Option<String>,
    report_flush_state: Option<String>,
    attach_reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct SessionMutationResult {
    heddle_session_id: String,
    heddle_segment_id: Option<String>,
    report_flush_state: Option<String>,
}

#[derive(Debug, Serialize)]
struct CloseSessionResult {
    heddle_session_id: String,
    changed_paths: Vec<String>,
    diff_summary: SessionDiffSummary,
    report_flush_state: Option<String>,
}

#[derive(Debug, Serialize)]
struct FlushReportsResult {
    flushed: usize,
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        (temp, repo)
    }

    #[test]
    fn harness_config_load_missing_path_defaults_without_warning() {
        let temp = tempfile::TempDir::new().unwrap();
        let missing = temp.path().join("missing-config.toml");

        let (config, warning) = load_harness_user_config(Some(missing));

        assert_eq!(config.harness.transport, HarnessTransport::Spool);
        assert!(warning.is_none());
    }

    #[test]
    fn harness_config_load_malformed_path_warns_and_defaults() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "[harness\ntransport = \"direct\"\n").unwrap();

        let (config, warning) = load_harness_user_config(Some(path.clone()));

        assert_eq!(config.harness.transport, HarnessTransport::Spool);
        let warning = warning.expect("malformed config should produce a warning");
        assert!(warning.contains("failed to load user config"));
        assert!(warning.contains(&path.display().to_string()));
        assert!(warning.contains("continuing with defaults"));
    }

    #[test]
    fn harness_config_load_valid_path_loads_without_warning() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            "[harness]\ntransport = \"direct\"\ntranscript = \"summary\"\n",
        )
        .unwrap();

        let (config, warning) = load_harness_user_config(Some(path));

        assert_eq!(config.harness.transport, HarnessTransport::Direct);
        assert_eq!(config.harness.transcript, HarnessTranscriptMode::Summary);
        assert!(warning.is_none());
    }

    #[test]
    fn relay_payload_parse_invalid_json_warns_and_uses_null() {
        let (value, warning) = parse_relay_payload("{not-json");

        assert_eq!(value, Value::Null);
        let warning = warning.expect("invalid JSON should produce a warning");
        assert!(warning.contains("failed to parse harness relay payload as JSON"));
        assert!(warning.contains("continuing with null payload"));
    }

    #[test]
    fn relay_payload_parse_empty_payload_uses_null_without_warning() {
        let (value, warning) = parse_relay_payload("  \n");

        assert_eq!(value, Value::Null);
        assert!(warning.is_none());
    }

    #[test]
    fn relay_payload_parse_valid_json_without_warning() {
        let (value, warning) = parse_relay_payload(r#"{"message":"hello"}"#);

        assert_eq!(value["message"], "hello");
        assert!(warning.is_none());
    }

    /// Harness subagent/root-actor checkout paths must use the SAME canonical
    /// managed checkout path derivation `start` and the per-thread manifest use
    /// — for the slash-namespaced names the harness commonly mints
    /// (`parent/task`). Before this, a harness-local `sanitize_name` flattened
    /// `parent/task` and `parent-task` onto the same
    /// `.heddle/threads/parent-task/<repo-name>`, colliding distinct threads
    /// (heddle#572 r2).
    #[test]
    fn harness_default_path_matches_canonical_thread_dir() {
        let (_temp, repo) = init_repo();
        for id in ["foo", "parent/task", "feature/foo", "team@scope"] {
            let harness_path = default_private_thread_path(&repo, id);
            let canonical = repo.managed_checkout_path(id);
            assert_eq!(
                harness_path, canonical,
                "harness default must match the canonical thread_dir for {id:?}"
            );
        }
    }

    #[test]
    fn inherited_harness_hints_exclude_ambient_model_identity() {
        assert!(!inherited_harness_hint("OPENAI_MODEL"));
        assert!(!inherited_harness_hint("ANTHROPIC_MODEL"));
        assert!(!inherited_harness_hint("CLAUDE_MODEL"));
        assert!(!inherited_harness_hint("MODEL"));
        assert!(!inherited_harness_hint("OPENAI_REASONING_EFFORT"));
        assert!(inherited_harness_hint("HEDDLE_AGENT_MODEL"));
        assert!(inherited_harness_hint("CODEX_SANDBOX"));
        assert!(inherited_harness_hint("CLAUDECODE"));
    }

    #[test]
    fn open_session_creates_or_attaches() {
        let (_temp, repo) = init_repo();
        let user_config = UserConfig::default();
        let mut runtime = HarnessBridgeRuntime::new(repo, user_config);

        let created = runtime
            .open_session(OpenSessionParams {
                harness: Some("codex".to_string()),
                provider: Some("openai".to_string()),
                model: Some("gpt-5.4".to_string()),
                ..OpenSessionParams::default()
            })
            .unwrap();
        assert!(created.created_session);

        let attached = runtime
            .open_session(OpenSessionParams {
                harness: Some("codex".to_string()),
                provider: Some("openai".to_string()),
                model: Some("gpt-5.4".to_string()),
                ..OpenSessionParams::default()
            })
            .unwrap();
        assert!(!attached.created_session);
        assert_eq!(created.heddle_session_id, attached.heddle_session_id);
    }

    #[test]
    fn same_client_instance_reattaches_to_its_existing_session() {
        let (_temp, repo) = init_repo();
        let user_config = UserConfig::default();
        let mut runtime = HarnessBridgeRuntime::new(repo, user_config);

        let first = runtime
            .open_session(OpenSessionParams {
                client_instance_id: Some("client-a".to_string()),
                harness: Some("codex".to_string()),
                provider: Some("openai".to_string()),
                model: Some("gpt-5.4".to_string()),
                ..OpenSessionParams::default()
            })
            .unwrap();
        let second = runtime
            .open_session(OpenSessionParams {
                client_instance_id: Some("client-b".to_string()),
                harness: Some("codex".to_string()),
                provider: Some("openai".to_string()),
                model: Some("gpt-5.4".to_string()),
                ..OpenSessionParams::default()
            })
            .unwrap();
        let reopened = runtime
            .open_session(OpenSessionParams {
                client_instance_id: Some("client-a".to_string()),
                harness: Some("codex".to_string()),
                provider: Some("openai".to_string()),
                model: Some("gpt-5.4".to_string()),
                ..OpenSessionParams::default()
            })
            .unwrap();

        assert_ne!(first.heddle_session_id, second.heddle_session_id);
        assert_eq!(first.heddle_session_id, reopened.heddle_session_id);
        assert_eq!(first.agent_session_id, reopened.agent_session_id);
    }

    #[test]
    fn different_client_instances_do_not_share_the_current_session() {
        let (_temp, repo) = init_repo();
        let user_config = UserConfig::default();
        let mut runtime = HarnessBridgeRuntime::new(repo, user_config);

        let first = runtime
            .open_session(OpenSessionParams {
                client_instance_id: Some("client-a".to_string()),
                harness: Some("codex".to_string()),
                provider: Some("openai".to_string()),
                model: Some("gpt-5.4".to_string()),
                ..OpenSessionParams::default()
            })
            .unwrap();
        let second = runtime
            .open_session(OpenSessionParams {
                client_instance_id: Some("client-b".to_string()),
                harness: Some("codex".to_string()),
                provider: Some("openai".to_string()),
                model: Some("gpt-5.4".to_string()),
                ..OpenSessionParams::default()
            })
            .unwrap();

        assert_ne!(first.heddle_session_id, second.heddle_session_id);
        assert_ne!(first.agent_session_id, second.agent_session_id);
    }

    #[test]
    fn provider_model_change_creates_segment() {
        let (_temp, repo) = init_repo();
        let user_config = UserConfig::default();
        let mut runtime = HarnessBridgeRuntime::new(repo, user_config);

        let opened = runtime
            .open_session(OpenSessionParams {
                harness: Some("claude-code".to_string()),
                provider: Some("anthropic".to_string()),
                model: Some("claude-sonnet".to_string()),
                ..OpenSessionParams::default()
            })
            .unwrap();
        runtime
            .update_progress(UpdateProgressParams {
                heddle_session_id: opened.heddle_session_id.clone(),
                provider: Some("openai".to_string()),
                model: Some("gpt-5.4".to_string()),
                ..UpdateProgressParams::default()
            })
            .unwrap();

        let report = runtime
            .reports
            .load(&opened.heddle_session_id)
            .unwrap()
            .unwrap();
        let expected_segment = format!("{}-seg-2", opened.heddle_session_id);
        assert_eq!(
            report.heddle_segment_id.as_deref(),
            Some(expected_segment.as_str())
        );
    }

    #[test]
    fn blank_agent_model_hint_falls_through_to_detected_model_without_segment_rotation() {
        let (_temp, repo) = init_repo();
        let user_config = UserConfig::default();
        let mut runtime = HarnessBridgeRuntime::new(repo, user_config);
        let blank_model_env = BTreeMap::from([
            ("HEDDLE_AGENT_PROVIDER".to_string(), "anthropic".to_string()),
            ("HEDDLE_AGENT_MODEL".to_string(), String::new()),
        ]);

        let opened = runtime
            .open_session(OpenSessionParams {
                harness: Some("claude-code".to_string()),
                env_hints: blank_model_env.clone(),
                probe_metadata: BTreeMap::from([
                    ("session_id".to_string(), "claude-sess-blank".to_string()),
                    ("model".to_string(), "claude-opus-4-8[1m]".to_string()),
                ]),
                ..OpenSessionParams::default()
            })
            .unwrap();
        assert_eq!(opened.model.as_deref(), Some("claude-opus-4-8[1m]"));

        let original_segment = opened.heddle_segment_id.clone();
        runtime
            .update_progress(UpdateProgressParams {
                heddle_session_id: opened.heddle_session_id.clone(),
                env_hints: blank_model_env,
                probe_metadata: BTreeMap::from([
                    ("session_id".to_string(), "claude-sess-blank".to_string()),
                    ("model".to_string(), "claude-opus-4-8[1m]".to_string()),
                ]),
                ..UpdateProgressParams::default()
            })
            .unwrap();

        let report = runtime
            .reports
            .load(&opened.heddle_session_id)
            .unwrap()
            .unwrap();
        assert_eq!(report.harness.model.as_deref(), Some("claude-opus-4-8[1m]"));
        assert_eq!(report.heddle_segment_id, original_segment);
    }

    #[test]
    fn close_session_captures_changed_paths_from_status_and_hints() {
        let (temp, repo) = init_repo();
        let config = UserConfig::default();
        let mut runtime = HarnessBridgeRuntime::new(repo, config);

        let opened = runtime
            .open_session(OpenSessionParams {
                harness: Some("codex".to_string()),
                provider: Some("openai".to_string()),
                model: Some("gpt-5.4".to_string()),
                ..OpenSessionParams::default()
            })
            .unwrap();
        std::fs::write(temp.path().join("src.txt"), "hello\n").unwrap();
        runtime
            .record_touched_paths(RecordTouchedPathsParams {
                heddle_session_id: opened.heddle_session_id.clone(),
                paths: vec!["src.txt".to_string(), "notes.md".to_string()],
            })
            .unwrap();
        let closed = runtime
            .close_session(CloseSessionParams {
                heddle_session_id: opened.heddle_session_id.clone(),
                outcome: Some("completed".to_string()),
                ..CloseSessionParams::default()
            })
            .unwrap();
        let report = runtime
            .reports
            .load(&opened.heddle_session_id)
            .unwrap()
            .unwrap();
        assert!(closed.changed_paths.iter().any(|path| path == "src.txt"));
        assert!(!closed.changed_paths.iter().any(|path| path == "notes.md"));
        assert!(report.touched_paths.iter().any(|path| path == "src.txt"));
        assert!(report.touched_paths.iter().any(|path| path == "notes.md"));
        assert_eq!(
            closed.diff_summary.changed_file_count,
            closed.changed_paths.len() as u32
        );
    }

    #[test]
    fn flush_reports_moves_pending_report_to_outbox() {
        let (_temp, repo) = init_repo();
        let user_config = UserConfig::default();
        let mut runtime = HarnessBridgeRuntime::new(repo, user_config);

        let opened = runtime
            .open_session(OpenSessionParams {
                harness: Some("codex".to_string()),
                provider: Some("openai".to_string()),
                model: Some("gpt-5.4".to_string()),
                ..OpenSessionParams::default()
            })
            .unwrap();
        let flushed = runtime
            .flush_reports(FlushReportsParams {
                heddle_session_id: Some(opened.heddle_session_id.clone()),
            })
            .unwrap();
        assert_eq!(flushed.flushed, 1);
        let report = runtime
            .reports
            .load(&opened.heddle_session_id)
            .unwrap()
            .unwrap();
        assert!(!report.pending_flush);
        assert_eq!(report.report_flush_state.as_deref(), Some("queued-local"));
        assert!(runtime.reports.outbox_path().exists());
    }

    #[test]
    fn explicit_overrides_beat_fingerprint_and_user_defaults() {
        let (_temp, repo) = init_repo();
        let mut user_config = UserConfig::default();
        user_config.harness.harnesses.insert(
            "codex".to_string(),
            UserHarnessOverride {
                provider: Some("openai".to_string()),
                model: Some("gpt-default".to_string()),
                thinking_level: Some("medium".to_string()),
                policy: Some("default".to_string()),
            },
        );
        let identity = resolve_identity(
            &repo,
            &user_config,
            IdentityHints {
                harness: Some("codex".to_string()),
                provider: Some("openai".to_string()),
                model: Some("gpt-5.4".to_string()),
                thinking_level: Some("high".to_string()),
                policy: Some("custom".to_string()),
                probe: HarnessProbeResult::default(),
            },
        )
        .unwrap();
        assert_eq!(identity.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(identity.thinking_level.as_deref(), Some("high"));
        assert_eq!(identity.policy.as_deref(), Some("custom"));
    }

    #[test]
    fn transcript_mode_defaults_to_off_and_keeps_refs_empty() {
        let (_temp, repo) = init_repo();
        let user_config = UserConfig::default();
        let mut runtime = HarnessBridgeRuntime::new(repo, user_config);

        let opened = runtime
            .open_session(OpenSessionParams {
                harness: Some("codex".to_string()),
                provider: Some("openai".to_string()),
                model: Some("gpt-5.4".to_string()),
                ..OpenSessionParams::default()
            })
            .unwrap();
        let report = runtime
            .reports
            .load(&opened.heddle_session_id)
            .unwrap()
            .unwrap();
        assert_eq!(report.transcript_mode, "off");
        assert!(report.transcript_refs.is_empty());
    }

    #[test]
    fn codex_thread_probe_reattaches_same_actor() {
        let (_temp, repo) = init_repo();
        let user_config = UserConfig::default();
        let mut runtime = HarnessBridgeRuntime::new(repo, user_config);

        let first = runtime
            .open_session(OpenSessionParams {
                harness: Some("codex".to_string()),
                probe_metadata: BTreeMap::from([
                    ("thread_id".to_string(), "thr_123".to_string()),
                    ("client_name".to_string(), "codex-tui".to_string()),
                ]),
                ..OpenSessionParams::default()
            })
            .unwrap();
        let second = runtime
            .open_session(OpenSessionParams {
                harness: Some("codex".to_string()),
                probe_metadata: BTreeMap::from([
                    ("thread_id".to_string(), "thr_123".to_string()),
                    ("client_name".to_string(), "codex-tui".to_string()),
                ]),
                ..OpenSessionParams::default()
            })
            .unwrap();

        assert_eq!(first.agent_session_id, second.agent_session_id);
        assert_eq!(first.heddle_session_id, second.heddle_session_id);
    }

    #[test]
    fn opencode_child_session_creates_distinct_actor_with_parent_key() {
        let (_temp, repo) = init_repo();
        let user_config = UserConfig::default();
        let mut runtime = HarnessBridgeRuntime::new(repo, user_config);

        let root = runtime
            .open_session(OpenSessionParams {
                harness: Some("opencode".to_string()),
                probe_metadata: BTreeMap::from([("session_id".to_string(), "root-1".to_string())]),
                ..OpenSessionParams::default()
            })
            .unwrap();
        let child = runtime
            .open_session(OpenSessionParams {
                harness: Some("opencode".to_string()),
                probe_metadata: BTreeMap::from([
                    ("session_id".to_string(), "child-1".to_string()),
                    ("parent_id".to_string(), "root-1".to_string()),
                ]),
                ..OpenSessionParams::default()
            })
            .unwrap();

        assert_ne!(root.agent_session_id, child.agent_session_id);
        let report = runtime
            .reports
            .load(&child.heddle_session_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            report.native_parent_actor_key.as_deref(),
            Some("opencode:session:root-1")
        );
    }

    #[test]
    fn claude_resume_with_new_session_id_does_not_steal_existing_actor() {
        let (_temp, repo) = init_repo();
        let user_config = UserConfig::default();
        let mut runtime = HarnessBridgeRuntime::new(repo, user_config);

        let first = runtime
            .open_session(OpenSessionParams {
                harness: Some("claude-code".to_string()),
                probe_metadata: BTreeMap::from([
                    ("session_id".to_string(), "sess-old".to_string()),
                    (
                        "transcript_path".to_string(),
                        "/tmp/claude/session-a.jsonl".to_string(),
                    ),
                ]),
                ..OpenSessionParams::default()
            })
            .unwrap();
        let resumed = runtime
            .open_session(OpenSessionParams {
                harness: Some("claude-code".to_string()),
                probe_metadata: BTreeMap::from([
                    ("session_id".to_string(), "sess-new".to_string()),
                    (
                        "transcript_path".to_string(),
                        "/tmp/claude/session-a.jsonl".to_string(),
                    ),
                ]),
                ..OpenSessionParams::default()
            })
            .unwrap();

        assert_ne!(first.agent_session_id, resumed.agent_session_id);
        assert_ne!(first.heddle_session_id, resumed.heddle_session_id);
    }

    #[test]
    fn explicit_claude_harness_beats_generic_session_id_probe_match() {
        let (_temp, repo) = init_repo();
        let user_config = UserConfig::default();
        let mut runtime = HarnessBridgeRuntime::new(repo, user_config);

        let opened = runtime
            .open_session(OpenSessionParams {
                harness: Some("claude-code".to_string()),
                probe_metadata: BTreeMap::from([
                    ("session_id".to_string(), "claude-sess-1".to_string()),
                    ("hook_event".to_string(), "SubagentStop".to_string()),
                ]),
                ..OpenSessionParams::default()
            })
            .unwrap();
        let report = runtime
            .reports
            .load(&opened.heddle_session_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            report.native_actor_key.as_deref(),
            Some("claude-code:session:claude-sess-1")
        );
        assert_eq!(report.harness.harness.as_deref(), Some("claude-code"));
    }

    #[test]
    fn same_native_actor_key_reuses_existing_actor_after_tentative_session_creation() {
        let (_temp, repo) = init_repo();
        let user_config = UserConfig::default();
        let runtime = HarnessBridgeRuntime::new(repo, user_config);
        let principal = runtime.repo.get_principal().unwrap();
        let mut sessions = SessionManager::new(runtime.repo.root());
        let existing_session = sessions
            .start_session(
                principal.clone(),
                "anthropic".to_string(),
                "claude-opus-4-7[1m]".to_string(),
                None,
            )
            .unwrap();
        let tentative_session = sessions
            .start_session(
                principal,
                "anthropic".to_string(),
                "claude-opus-4-7[1m]".to_string(),
                None,
            )
            .unwrap();

        let registry = AgentRegistry::new(runtime.repo.heddle_dir());
        let existing_entry = registry
            .create_generated_entry(|session_id| {
                Ok(AgentEntry {
                    session_id: session_id.to_string(),
                    client_instance_id: None,
                    native_actor_key: Some(
                        "claude-code:session:282396d3-554a-48aa-a9a8-8d1f0bd15fa5".to_string(),
                    ),
                    native_parent_actor_key: None,
                    native_instance_key: Some(
                        "claude-code:transcript:/tmp/claude/282396d3.jsonl".to_string(),
                    ),
                    heddle_session_id: Some(existing_session.id.clone()),
                    thread_id: None,
                    thread: "detached".to_string(),
                    pid: Some(std::process::id()),
                    boot_id: None,
                    liveness_path: None,
                    heartbeat_at: Some(Utc::now()),
                    anchor_state: None,
                    anchor_root: None,
                    reservation_token: Some(objects::store::generate_agent_id()),
                    path: Some(runtime.repo.root().to_path_buf()),
                    base_state: String::new(),
                    started_at: Utc::now(),
                    provider: Some("anthropic".to_string()),
                    model: Some("claude-opus-4-7[1m]".to_string()),
                    harness: Some("claude-code".to_string()),
                    thinking_level: None,
                    usage_summary: AgentUsageSummary::default(),
                    last_progress_at: None,
                    report_flush_state: Some("pending-local".to_string()),
                    attach_reason: None,
                    attach_precedence: vec![],
                    winning_attach_rule: None,
                    probe_source: Some("hook_payload".to_string()),
                    probe_confidence: Some(1.0),
                    status: AgentStatus::Active,
                    completed_at: None,
                    context_queries: vec![],
                })
            })
            .unwrap();

        let probe = HarnessProbeResult {
            harness: Some("claude-code".to_string()),
            provider: Some("anthropic".to_string()),
            model: Some("claude-opus-4-7[1m]".to_string()),
            native_actor_key: Some(
                "claude-code:session:282396d3-554a-48aa-a9a8-8d1f0bd15fa5".to_string(),
            ),
            native_instance_key: Some(
                "claude-code:transcript:/tmp/claude/282396d3.jsonl".to_string(),
            ),
            probe_source: Some("hook_payload".to_string()),
            confidence: Some(1.0),
            ..HarnessProbeResult::default()
        };
        let identity = ResolvedIdentity {
            harness: Some("claude-code".to_string()),
            provider: Some("anthropic".to_string()),
            model: Some("claude-opus-4-7[1m]".to_string()),
            thinking_level: None,
            policy: None,
        };
        let mut attach = ResolvedAttachment {
            target: AttachTarget::CreateNew {
                _because_claimed: false,
            },
            matched_entry: None,
            attach_reason:
                "started new Heddle session because no compatible native actor match was found"
                    .to_string(),
            precedence: vec!["native-actor-key:miss".to_string()],
            winning_rule: "create-new-session".to_string(),
        };

        let resolved_entry = runtime
            .ensure_registry_entry(RegistryEntryRequest {
                heddle_session_id: &tentative_session.id,
                thread_name: None,
                thread_id: None,
                identity: &identity,
                probe: &probe,
                attach: &attach,
                client_instance_id: None,
                requested_entry: None,
            })
            .unwrap();
        assert_eq!(resolved_entry.session_id, existing_entry.session_id);
        assert_eq!(
            resolved_entry.heddle_session_id.as_deref(),
            Some(existing_session.id.as_str())
        );

        let (canonical_session, owns_session) = runtime
            .reuse_canonical_actor_session(
                &mut sessions,
                CanonicalActorSessionRequest {
                    tentative_session: tentative_session.clone(),
                    tentative_owns_session: true,
                    entry: &resolved_entry,
                    probe: &probe,
                    attach: &mut attach,
                },
            )
            .unwrap();
        assert_eq!(canonical_session.id, existing_session.id);
        assert!(!owns_session);
        assert!(
            attach
                .precedence
                .iter()
                .any(|step| step.starts_with("post-create-native-actor-key:"))
        );
        assert_eq!(attach.winning_rule, "native-actor-key-post-create");
        assert!(
            !sessions
                .get_session(&tentative_session.id)
                .unwrap()
                .unwrap()
                .is_active()
        );
    }

    #[test]
    fn close_session_does_not_blame_preexisting_dirty_worktree() {
        let (temp, repo) = init_repo();
        std::fs::write(temp.path().join("preexisting.txt"), "already dirty\n").unwrap();
        let user_config = UserConfig::default();
        let mut runtime = HarnessBridgeRuntime::new(repo, user_config);

        let opened = runtime
            .open_session(OpenSessionParams {
                harness: Some("claude-code".to_string()),
                provider: Some("anthropic".to_string()),
                model: Some("claude-opus-4-7[1m]".to_string()),
                ..OpenSessionParams::default()
            })
            .unwrap();
        let closed = runtime
            .close_session(CloseSessionParams {
                heddle_session_id: opened.heddle_session_id.clone(),
                outcome: Some("completed".to_string()),
                ..CloseSessionParams::default()
            })
            .unwrap();
        let report = runtime
            .reports
            .load(&opened.heddle_session_id)
            .unwrap()
            .unwrap();

        assert!(
            report
                .worktree_changes_at_open
                .iter()
                .any(|change| change.path == "preexisting.txt")
        );
        assert!(
            !closed
                .changed_paths
                .iter()
                .any(|path| path == "preexisting.txt")
        );
        assert_eq!(closed.diff_summary.changed_file_count, 0);
    }

    #[test]
    fn timeline_state_delta_paths_ignore_uncaptured_worktree_changes() {
        let (temp, repo) = init_repo();
        let repo_root = repo.root().to_path_buf();
        std::fs::write(repo_root.join("tracked.txt"), b"one\n").unwrap();
        let before = repo.snapshot(Some("seed".into()), None).unwrap();
        std::fs::write(repo_root.join("tracked.txt"), b"two\n").unwrap();
        let after = repo.snapshot(Some("advance".into()), None).unwrap();
        std::fs::write(temp.path().join("ambient.txt"), b"not in the state delta\n").unwrap();

        assert_eq!(
            changed_paths_between_states(&repo, before.change_id, after.change_id).unwrap(),
            vec!["tracked.txt"]
        );
    }

    #[test]
    fn relay_claude_stop_captures_state_with_agent_attribution() {
        let (temp, repo) = init_repo();
        let repo_root = repo.root().to_path_buf();

        // Establish HEAD with an initial snapshot.
        std::fs::write(repo_root.join("seed.txt"), b"hello").unwrap();
        let _ = repo.snapshot(Some("seed".into()), None).unwrap();

        // Make a dirty change that the Stop hook should capture.
        std::fs::write(repo_root.join("seed.txt"), b"hello, heddle").unwrap();

        drop(repo);

        let fresh_repo = Repository::open(temp.path()).unwrap();
        let user_config = UserConfig {
            principal: Some(crate::config::UserPrincipalConfig {
                name: "Ada Lovelace".to_string(),
                email: "ada@example.com".to_string(),
            }),
            ..UserConfig::default()
        };
        let mut runtime = HarnessBridgeRuntime::new(fresh_repo, user_config);
        let payload = serde_json::json!({
            "session_id": "claude-sess-123",
            "transcript_path": "/tmp/claude/x.jsonl",
            "model": {
                "id": "claude-opus-4-7",
                "display_name": "Claude Opus 4.7",
            },
            "message": "hook-driven capture test",
            "hook_event_name": "Stop",
        });
        relay_claude(&mut runtime, "Stop", &payload).unwrap();
        drop(runtime);

        let verify = Repository::open(temp.path()).unwrap();
        let head_id = verify.head().unwrap().expect("HEAD after Stop capture");
        let state = verify
            .store()
            .get_state(&head_id)
            .unwrap()
            .expect("state for HEAD");
        let agent = state.attribution.agent.expect("agent attribution on state");
        assert_eq!(agent.provider, "anthropic");
        assert_eq!(agent.model, "Claude Opus 4.7");
        assert_eq!(
            state.intent.as_deref(),
            Some("hook-driven capture test"),
            "intent should be pulled from payload message",
        );
    }

    #[test]
    fn relay_claude_stop_is_idempotent_when_clean() {
        let (temp, repo) = init_repo();
        let repo_root = repo.root().to_path_buf();
        std::fs::write(repo_root.join("seed.txt"), b"hello").unwrap();
        let seed = repo.snapshot(Some("seed".into()), None).unwrap();
        drop(repo);

        let fresh_repo = Repository::open(temp.path()).unwrap();
        let mut runtime = HarnessBridgeRuntime::new(fresh_repo, UserConfig::default());
        let payload = serde_json::json!({
            "session_id": "claude-sess-clean",
            "model": {"id": "claude-sonnet-4-6"},
        });
        relay_claude(&mut runtime, "Stop", &payload).unwrap();
        drop(runtime);

        let verify = Repository::open(temp.path()).unwrap();
        let head_id = verify.head().unwrap().expect("HEAD preserved");
        assert_eq!(
            head_id, seed.change_id,
            "no change expected when worktree is clean",
        );
    }

    #[test]
    fn relay_claude_pre_tool_use_ignores_non_file_tool() {
        let (temp, repo) = init_repo();
        drop(repo);
        let fresh_repo = Repository::open(temp.path()).unwrap();
        let mut runtime = HarnessBridgeRuntime::new(fresh_repo, UserConfig::default());
        let payload = serde_json::json!({
            "session_id": "claude-sess-bash",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
        });
        // Should succeed without writing any stdout or erroring.
        relay_claude(&mut runtime, "PreToolUse", &payload).unwrap();
    }

    #[test]
    fn relay_opencode_tool_execute_before_records_timeline_step() {
        let (_temp, repo) = init_repo();
        let root = repo.root().to_path_buf();
        std::fs::write(root.join("seed.txt"), b"hello").unwrap();
        let seed = repo.snapshot(Some("seed".into()), None).unwrap();
        let mut runtime = HarnessBridgeRuntime::new(repo, UserConfig::default());
        let payload = opencode_tool_payload("call-1");

        relay_opencode(&mut runtime, "tool.execute.before", &payload).unwrap();

        let store = TimelineStore::open(runtime.repo.heddle_dir()).unwrap();
        let view = TimelineView::rebuild(&store).unwrap();
        let steps = view.steps_for_thread("main");
        assert_eq!(steps.len(), 1);
        let step = steps[0];
        assert_eq!(step.native.as_ref().unwrap().harness, "opencode");
        assert_eq!(step.native.as_ref().unwrap().tool_call_id, "call-1");
        assert_eq!(step.tool_name.as_deref(), Some("bash"));
        assert_eq!(step.before_state, Some(seed.change_id));
        assert!(step.status.is_none());
        assert!(step.payload_summary.as_deref().unwrap().contains("call-1"));
        assert!(step.payload_hash.is_some());
        assert!(
            step.labels
                .contains(&TimelineLabel::ExternalSideEffectsUnknown)
        );
    }

    #[test]
    fn relay_opencode_tool_execute_after_captures_dirty_worktree() {
        let (_temp, repo) = init_repo();
        let root = repo.root().to_path_buf();
        std::fs::write(root.join("tracked.txt"), b"one\n").unwrap();
        let seed = repo.snapshot(Some("seed".into()), None).unwrap();
        let user_config = UserConfig {
            principal: Some(crate::config::UserPrincipalConfig {
                name: "Ada Lovelace".to_string(),
                email: "ada@example.com".to_string(),
            }),
            ..UserConfig::default()
        };
        let mut runtime = HarnessBridgeRuntime::new(repo, user_config);
        let payload = opencode_tool_payload("call-2");

        relay_opencode(&mut runtime, "tool.execute.before", &payload).unwrap();
        std::fs::write(root.join("tracked.txt"), b"two\n").unwrap();
        relay_opencode(&mut runtime, "tool.execute.after", &payload).unwrap();

        let head = runtime.repo.head().unwrap().expect("capture advanced HEAD");
        assert_ne!(head, seed.change_id);
        let store = TimelineStore::open(runtime.repo.heddle_dir()).unwrap();
        let view = TimelineView::rebuild(&store).unwrap();
        let steps = view.steps_for_thread("main");
        assert_eq!(steps.len(), 1, "before/after should merge by native id");
        let step = steps[0];
        assert_eq!(step.operation_ids.len(), 2);
        assert_eq!(step.status, Some(TimelineToolCallStatus::Succeeded));
        assert_eq!(step.before_state, Some(seed.change_id));
        assert_eq!(step.after_state, Some(head));
        assert_eq!(step.capture_state, Some(head));
        assert_eq!(step.changed, Some(true));
        assert!(step.touched_paths.contains(&"tracked.txt".to_string()));
        assert!(step.labels.contains(&TimelineLabel::RepoReversible));
        assert!(
            step.labels
                .contains(&TimelineLabel::ExternalSideEffectsUnknown)
        );
        assert!(!step.payload_summary.as_deref().unwrap().contains("SECRET"));
        assert!(step.payload_hash.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn relay_opencode_tool_execute_after_records_capture_failed_without_ambient_paths() {
        let (_temp, repo) = init_repo();
        let root = repo.root().to_path_buf();
        std::fs::write(root.join("seed.txt"), b"seed\n").unwrap();
        let seed = repo.snapshot(Some("seed".into()), None).unwrap();
        let mut runtime = HarnessBridgeRuntime::new(repo, UserConfig::default());
        let mut payload = opencode_tool_payload("call-capture-failed");
        payload["tool"]["input"]["file_path"] = serde_json::json!("hinted.txt");
        let hooks_dir = root.join(".heddle/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-snapshot");
        std::fs::write(&hook_path, "#!/bin/sh\nexit 1\n").unwrap();
        let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms).unwrap();

        relay_opencode(&mut runtime, "tool.execute.before", &payload).unwrap();
        std::fs::write(root.join("ambient.txt"), b"dirty but uncaptured\n").unwrap();
        relay_opencode(&mut runtime, "tool.execute.after", &payload).unwrap();

        assert_eq!(
            runtime.repo.head().unwrap(),
            Some(seed.change_id),
            "capture failure must not advance HEAD"
        );
        let store = TimelineStore::open(runtime.repo.heddle_dir()).unwrap();
        let view = TimelineView::rebuild(&store).unwrap();
        let steps = view.steps_for_thread("main");
        assert_eq!(steps.len(), 1, "before/after should merge by native id");
        let step = steps[0];
        assert_eq!(step.operation_ids.len(), 2);
        assert_eq!(step.before_state, Some(seed.change_id));
        assert_eq!(step.after_state, Some(seed.change_id));
        assert_eq!(step.capture_state, None);
        assert_eq!(step.changed, Some(false));
        assert!(step.labels.contains(&TimelineLabel::CaptureFailed));
        assert!(
            !step.labels.contains(&TimelineLabel::RepoReversible),
            "failed captures are not repo-reversible"
        );
        assert_eq!(step.touched_paths, vec!["hinted.txt"]);
    }

    #[test]
    fn relay_opencode_tool_execute_missing_tool_id_does_not_fail_or_record_timeline() {
        let (_temp, repo) = init_repo();
        let root = repo.root().to_path_buf();
        std::fs::write(root.join("seed.txt"), b"hello").unwrap();
        let _ = repo.snapshot(Some("seed".into()), None).unwrap();
        let mut runtime = HarnessBridgeRuntime::new(repo, UserConfig::default());
        let payload = serde_json::json!({
            "sessionID": "opencode-session",
            "model": "gpt-5.4",
            "provider": "openai",
            "tool": {"name": "bash"},
        });

        relay_opencode(&mut runtime, "tool.execute.before", &payload).unwrap();

        let store = TimelineStore::open(runtime.repo.heddle_dir()).unwrap();
        let view = TimelineView::rebuild(&store).unwrap();
        assert!(view.steps_for_thread("main").is_empty());
        let report_count = std::fs::read_dir(root.join(".heddle/state/session-reports"))
            .unwrap()
            .count();
        assert!(
            report_count > 0,
            "session progress should still be recorded"
        );
    }

    fn opencode_tool_payload(call_id: &str) -> Value {
        serde_json::json!({
            "sessionID": "opencode-session",
            "messageID": "message-1",
            "toolCallID": call_id,
            "model": "gpt-5.4",
            "provider": "openai",
            "tool": {
                "name": "bash",
                "input": {
                    "command": "echo SECRET",
                    "file_path": "tracked.txt"
                }
            },
            "status": "success"
        })
    }

    #[test]
    fn relay_claude_subagent_start_creates_child_entry_with_parent_key() {
        let (temp, repo) = init_repo();
        drop(repo);
        let fresh_repo = Repository::open(temp.path()).unwrap();
        let mut runtime = HarnessBridgeRuntime::new(fresh_repo, UserConfig::default());
        let payload = serde_json::json!({
            "session_id": "parent-claude-sess",
            "agent_id": "child-subagent-xyz",
            "model": {"id": "claude-sonnet-4-6"},
        });
        relay_claude(&mut runtime, "SubagentStart", &payload).unwrap();
        drop(runtime);

        let verify = Repository::open(temp.path()).unwrap();
        let registry = AgentRegistry::new(verify.heddle_dir());
        let child = registry
            .find_active_by_native_actor_key("claude-code:agent:child-subagent-xyz")
            .unwrap()
            .expect("subagent AgentEntry should exist after SubagentStart");
        assert_eq!(
            child.native_parent_actor_key.as_deref(),
            Some("claude-code:session:parent-claude-sess"),
            "subagent must carry parent session linkage",
        );
        assert_eq!(child.status, AgentStatus::Active);
    }

    #[test]
    fn relay_claude_subagent_stop_marks_child_entry_complete() {
        let (temp, repo) = init_repo();
        let repo_root = repo.root().to_path_buf();
        drop(repo);

        // Start: create the child entry.
        let fresh = Repository::open(temp.path()).unwrap();
        let mut runtime = HarnessBridgeRuntime::new(fresh, UserConfig::default());
        let start_payload = serde_json::json!({
            "session_id": "parent-sess",
            "agent_id": "worker-1",
            "model": {"id": "claude-sonnet-4-6"},
        });
        relay_claude(&mut runtime, "SubagentStart", &start_payload).unwrap();
        drop(runtime);

        // Dirty the worktree so SubagentStop also captures a state.
        std::fs::write(
            repo_root.join("child-output.txt"),
            b"subagent produced this",
        )
        .unwrap();

        let fresh = Repository::open(temp.path()).unwrap();
        let mut runtime = HarnessBridgeRuntime::new(fresh, UserConfig::default());
        let stop_payload = serde_json::json!({
            "session_id": "parent-sess",
            "agent_id": "worker-1",
            "model": {
                "id": "claude-sonnet-4-6",
                "display_name": "Claude Sonnet 4.6",
            },
        });
        relay_claude(&mut runtime, "SubagentStop", &stop_payload).unwrap();
        drop(runtime);

        let verify = Repository::open(temp.path()).unwrap();
        let registry = AgentRegistry::new(verify.heddle_dir());
        let child = registry
            .list()
            .unwrap()
            .into_iter()
            .find(|e| e.native_actor_key.as_deref() == Some("claude-code:agent:worker-1"))
            .expect("child entry should still exist");
        assert_eq!(
            child.status,
            AgentStatus::Complete,
            "SubagentStop should mark the child entry Complete",
        );
    }

    #[test]
    fn relay_claude_user_prompt_submit_rotates_segment() {
        let (temp, repo) = init_repo();
        drop(repo);

        let fresh = Repository::open(temp.path()).unwrap();
        let mut runtime = HarnessBridgeRuntime::new(fresh, UserConfig::default());
        // SessionStart establishes the Heddle session + initial segment.
        let session_payload = serde_json::json!({
            "session_id": "claude-prompt-sess",
            "model": {"id": "claude-opus-4-7", "display_name": "Claude Opus 4.7"},
        });
        relay_claude(&mut runtime, "SessionStart", &session_payload).unwrap();
        let sessions_before = SessionManager::new(runtime.repo.root())
            .list_sessions(true)
            .unwrap();
        let initial_segments = sessions_before
            .iter()
            .find(|s| !s.segments.is_empty())
            .map(|s| s.segments.len())
            .unwrap_or(0);

        // UserPromptSubmit should force a new segment.
        let prompt_payload = serde_json::json!({
            "session_id": "claude-prompt-sess",
            "model": {"id": "claude-opus-4-7", "display_name": "Claude Opus 4.7"},
            "prompt": "write a new feature",
        });
        relay_claude(&mut runtime, "UserPromptSubmit", &prompt_payload).unwrap();
        drop(runtime);

        let verify = Repository::open(temp.path()).unwrap();
        let sessions_after = SessionManager::new(verify.root())
            .list_sessions(true)
            .unwrap();
        let rotated = sessions_after
            .iter()
            .any(|s| s.segments.len() > initial_segments);
        assert!(
            rotated,
            "UserPromptSubmit must add at least one segment beyond the SessionStart baseline \
             (initial={initial_segments}, sessions_after={:?})",
            sessions_after
                .iter()
                .map(|s| (s.id.clone(), s.segments.len()))
                .collect::<Vec<_>>(),
        );
    }
}
