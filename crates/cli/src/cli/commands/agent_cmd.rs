// SPDX-License-Identifier: Apache-2.0
//! Stable JSON-first agent reservation API.

use anyhow::{Result, anyhow};
use chrono::Utc;
use heddle_core::{
    AgentCaptureOptions, AgentCaptureThreadCheck, AgentReadyOptions, AgentReleaseKind,
    AgentReservationReport, FanoutLaneAvailability, FanoutLanePreflightBlock, FanoutNodeSpec,
    FanoutPlan, FanoutPlanError, FanoutPlanRequest, assemble_agent_reservation,
    assemble_agent_reservation_list, assemble_fanout_plan_report, check_agent_capture_thread,
    check_fanout_start_preflight, fanout_child_body, fanout_parent_delegated_by,
    fanout_start_attach_rule, plan_agent_capture, plan_agent_ready, plan_fanout,
    select_fanout_parent_thread, session_is_active, touch_agent_heartbeat, touch_agent_release,
};
use objects::{
    object::ThreadName,
    store::{
        AgentEntry, AgentRegistry, AgentStatus, AgentTaskRecord, AgentTaskStatus, AgentTaskStore,
        AgentUsageSummary, ObjectStore, ReserveOutcome, current_boot_id, validate_task_id,
    },
};
use oplog::OpLogRecorder;
use refs::{Head, RefExpectation};
use repo::{
    Repository, Thread, ThreadConfidenceSummary, ThreadFreshness, ThreadId,
    ThreadIntegrationPolicy, ThreadManager, ThreadMode, ThreadState, ThreadVerificationSummary,
};
use schemars::JsonSchema;
use serde::Serialize;

use super::{
    advice::RecoveryAdvice,
    thread::thread_name_invalid_advice,
    verification_health::{
        GitOverlayMutationPreflight, RepositoryVerificationState,
        build_repository_verification_state, git_overlay_mutation_preflight_advice,
    },
    worktree_cmd::helpers::plan_worktree_target,
    worktree_safety::ensure_worktree_clean,
};
use crate::cli::{
    Cli,
    cli_args::{
        AgentApiListArgs, AgentFanoutCommands, AgentFanoutPlanArgs, AgentFanoutStartArgs,
        AgentHeartbeatArgs, AgentReleaseArgs, AgentReleaseStatusArg, AgentReserveArgs,
        AgentTaskCommands, AgentTaskCreateArgs, AgentTaskListArgs, AgentTaskShowArgs,
        AgentTaskStatusArg, AgentTaskUpdateArgs, ThreadStartArgs, WorkspaceModeArg,
    },
    should_output_json,
};

#[derive(Serialize, JsonSchema)]
pub struct AgentReservationOutput {
    pub session_id: String,
    pub reservation_token: Option<String>,
    pub thread: String,
    pub anchor_state: Option<String>,
    pub anchor_root: Option<String>,
    pub task_assignment_id: Option<String>,
    /// Lifecycle status as a stable kebab-case string
    /// (`active|abandoned|complete|merged`). Mirrors
    /// `objects::store::AgentStatus` but kept as a `String` here so
    /// the schema lives entirely in the CLI crate.
    pub status: String,
    pub path: Option<String>,
    pub task: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub harness: Option<String>,
    pub thinking_level: Option<String>,
    pub probe_source: Option<String>,
    pub probe_confidence: Option<f32>,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct AgentReservationEnvelope {
    pub reservation: AgentReservationOutput,
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct AgentReservationListOutput {
    pub reservations: Vec<AgentReservationOutput>,
    pub alive_only: bool,
    pub thread: Option<String>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct AgentTaskEnvelope {
    pub output_kind: &'static str,
    pub task: AgentTaskOutput,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct AgentTaskListOutput {
    pub output_kind: &'static str,
    pub tasks: Vec<AgentTaskOutput>,
    pub thread: Option<String>,
    pub status: Option<String>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct AgentTaskOutput {
    pub schema_version: u32,
    pub task_id: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub target_thread: String,
    pub base_state: Option<String>,
    pub base_root: Option<String>,
    pub parent_task_id: Option<String>,
    pub coordination_discussion_id: Option<String>,
    pub allow_offline: bool,
    pub delegated_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct AgentFanoutOutput {
    pub output_kind: &'static str,
    pub title: String,
    pub parent_thread: String,
    pub base_state: String,
    pub base_root: String,
    pub coordination_discussion_id: Option<String>,
    pub parent_task: Option<AgentTaskOutput>,
    pub lanes: Vec<AgentFanoutLaneOutput>,
    pub commands: Vec<AgentFanoutCommandOutput>,
    #[serde(rename = "verification")]
    pub trust: RepositoryVerificationState,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct AgentFanoutLaneOutput {
    pub thread: String,
    pub path: String,
    pub title: String,
    pub task: Option<AgentTaskOutput>,
    pub session_id: Option<String>,
    pub status: String,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct AgentFanoutCommandOutput {
    pub lane_thread: String,
    pub command: String,
    pub argv: Vec<String>,
}

impl From<&AgentEntry> for AgentReservationOutput {
    fn from(entry: &AgentEntry) -> Self {
        AgentReservationOutput::from(assemble_agent_reservation(entry))
    }
}

impl From<AgentReservationReport> for AgentReservationOutput {
    fn from(report: AgentReservationReport) -> Self {
        Self {
            session_id: report.session_id,
            reservation_token: report.reservation_token,
            thread: report.thread,
            anchor_state: report.anchor_state,
            anchor_root: report.anchor_root,
            task_assignment_id: report.task_assignment_id,
            status: report.status,
            path: report.path,
            task: report.task,
            provider: report.provider,
            model: report.model,
            harness: report.harness,
            thinking_level: report.thinking_level,
            probe_source: report.probe_source,
            probe_confidence: report.probe_confidence,
        }
    }
}

impl From<&AgentTaskRecord> for AgentTaskOutput {
    fn from(task: &AgentTaskRecord) -> Self {
        Self {
            schema_version: task.schema_version,
            task_id: task.task_id.clone(),
            title: task.title.clone(),
            body: task.body.clone(),
            status: task.status.to_string(),
            target_thread: task.target_thread.clone(),
            base_state: task.base_state.clone(),
            base_root: task.base_root.clone(),
            parent_task_id: task.parent_task_id.clone(),
            coordination_discussion_id: task.coordination_discussion_id.clone(),
            allow_offline: task.allow_offline,
            delegated_by: task.delegated_by.clone(),
            created_at: task.created_at.to_rfc3339(),
            updated_at: task.updated_at.to_rfc3339(),
            completed_at: task.completed_at.map(|time| time.to_rfc3339()),
        }
    }
}

fn live_owner_conflict_advice(
    thread: &str,
    requested_anchor_full: &str,
    owner: &AgentEntry,
) -> RecoveryAdvice {
    let kind = if owner.anchor_state.as_deref() == Some(requested_anchor_full) {
        "live_owner"
    } else {
        "anchor_drift"
    };
    let primary_command = format!("heddle thread show {thread}");
    if kind == "live_owner" {
        RecoveryAdvice::safety_refusal(
            "live_owner",
            format!(
                "thread '{thread}' already has a live reservation on session '{}'",
                owner.session_id
            ),
            format!(
                "Inspect it with `{primary_command}`, or release that session before starting another writer."
            ),
            format!(
                "thread '{thread}' is reserved by live session '{}' at anchor {}",
                owner.session_id,
                owner.anchor_state.as_deref().unwrap_or("<unknown>")
            ),
            "starting another writer could create competing histories for the same thread",
            "no thread refs or reservation records were changed",
            primary_command.clone(),
            vec![primary_command],
        )
    } else {
        RecoveryAdvice::safety_refusal(
            "anchor_drift",
            format!(
                "thread '{thread}' is reserved by session '{}' on anchor {}, but reservation requested {requested_anchor_full}",
                owner.session_id,
                owner.anchor_state.as_deref().unwrap_or("<unknown>")
            ),
            "Refresh the thread or rebase before retrying.".to_string(),
            format!("thread '{thread}' has an active reservation at a different anchor"),
            "starting from the requested anchor could fork the same thread name into competing histories",
            "no thread refs or reservation records were changed",
            primary_command.clone(),
            vec![primary_command],
        )
    }
}

fn anchor_drift_no_owner_advice(
    thread: &str,
    requested_anchor_full: &str,
    reserved_anchor: &str,
) -> RecoveryAdvice {
    let primary_command = format!("heddle thread show {thread}");
    RecoveryAdvice::safety_refusal(
        "anchor_drift",
        format!(
            "thread '{thread}' is anchored at {reserved_anchor}, but reservation requested {requested_anchor_full}"
        ),
        "Refresh the thread or rebase before retrying.".to_string(),
        format!("thread '{thread}' already points at a different anchor"),
        "starting from the requested anchor could fork the same thread name into competing histories",
        "no thread refs or reservation records were changed",
        primary_command.clone(),
        vec![primary_command],
    )
}

fn agent_task_not_found_advice(task_id: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "agent_task_not_found",
        format!("agent task '{task_id}' not found"),
        "Create the task locally, or reserve without --task-id if no task assignment exists.",
        format!("no task record exists for {task_id}"),
        "continuing would attach an unverifiable task provenance id to the reservation",
        "no thread refs or reservation records were changed",
        format!("heddle agent task show {task_id}"),
        vec![format!("heddle agent task show {task_id}")],
    )
}

fn agent_task_mismatch_advice(task_id: &str, message: String) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "agent_task_mismatch",
        message.clone(),
        "Reserve the task on its target thread and base, or update the local task record first.",
        message,
        "continuing would attach task provenance to work outside the delegated target",
        "no thread refs or reservation records were changed",
        format!("heddle agent task show {task_id}"),
        vec![format!("heddle agent task show {task_id}")],
    )
}

fn load_task_for_reservation(
    repo: &Repository,
    task_id: &str,
    thread_name: &str,
    anchor_full: &str,
    anchor_short: &str,
    anchor_root: &str,
) -> Result<AgentTaskRecord> {
    validate_task_id(task_id).map_err(|err| anyhow!(err))?;
    let store = AgentTaskStore::new(repo.heddle_dir());
    let task = store
        .load(task_id)?
        .ok_or_else(|| anyhow!(agent_task_not_found_advice(task_id)))?;
    if task.target_thread != thread_name {
        return Err(anyhow!(agent_task_mismatch_advice(
            task_id,
            format!(
                "agent task '{task_id}' targets thread '{}', but reservation requested '{}'",
                task.target_thread, thread_name
            ),
        )));
    }
    if let Some(base_state) = task.base_state.as_deref()
        && base_state != anchor_full
        && base_state != anchor_short
    {
        return Err(anyhow!(agent_task_mismatch_advice(
            task_id,
            format!(
                "agent task '{task_id}' base_state is {base_state}, but reservation anchor is {anchor_full}"
            ),
        )));
    }
    if let Some(base_root) = task.base_root.as_deref()
        && base_root != anchor_root
    {
        return Err(anyhow!(agent_task_mismatch_advice(
            task_id,
            format!(
                "agent task '{task_id}' base_root is {base_root}, but reservation anchor root is {anchor_root}"
            ),
        )));
    }
    Ok(task)
}

pub fn cmd_agent_reserve(cli: &Cli, args: AgentReserveArgs) -> Result<()> {
    // User/external creation boundary: `agent reserve` persists a thread
    // record, so reject an unsafe thread name here too (same early-reject
    // rule as `start_thread` / `thread create`). (heddle#464 close-the-class.)
    ThreadId::new(args.thread.as_str()).map_err(|err| anyhow!(thread_name_invalid_advice(&err)))?;
    let repo = cli.open_repo()?;
    let anchor = match &args.anchor {
        Some(spec) => repo
            .resolve_state(spec)?
            .ok_or_else(|| anyhow!("anchor state '{}' not found", spec))?,
        None => repo
            .head()?
            .ok_or_else(|| anyhow!("repository has no HEAD state to reserve from"))?,
    };
    let anchor_root = repo
        .store()
        .get_state(&anchor)?
        .map(|state| state.tree.short())
        .unwrap_or_default();
    let anchor_full = anchor.to_string_full();
    let anchor_short = anchor.short();
    let thread_name = args.thread.clone();
    let task_record = match args.task_id.as_deref() {
        Some(task_id) => Some(load_task_for_reservation(
            &repo,
            task_id,
            &thread_name,
            &anchor_full,
            &anchor_short,
            &anchor_root,
        )?),
        None => None,
    };

    // Hard pre-check: a thread ref already pointing at a different
    // state without any live owner is an anchor-drift case the caller
    // must resolve before we hand them a fresh reservation. We surface
    // it here (rather than letting set_thread_cas fail later) so the
    // standard JSON error envelope can describe the conflict.
    let existing_ref = repo.refs().get_thread(&ThreadName::new(&thread_name))?;
    if let Some(existing) = existing_ref
        && existing != anchor
    {
        // Look for a live owner first — if one exists, route through
        // the shared reservation advice so the caller sees the owner's
        // session_id alongside the drift in the standard error envelope.
        let registry = AgentRegistry::new(repo.heddle_dir());
        registry.reap_dead_for_thread(&thread_name)?;
        if let Some(owner) = registry
            .list()?
            .into_iter()
            .find(|entry| entry.status == AgentStatus::Active && entry.thread == thread_name)
        {
            return Err(anyhow!(live_owner_conflict_advice(
                &thread_name,
                &anchor_full,
                &owner
            )));
        }
        return Err(anyhow!(anchor_drift_no_owner_advice(
            &thread_name,
            &anchor_full,
            &existing.to_string_full(),
        )));
    }

    let registry = AgentRegistry::new(repo.heddle_dir());
    let task = args.task.clone();
    let task_assignment_id = task_record.as_ref().map(|task| task.task_id.clone());
    let anchor_full_for_entry = anchor_full.clone();
    let anchor_short = anchor.short();
    let reservation_path = existing_thread_execution_path(&repo, &thread_name)?;
    let probe = crate::harness::probe_current_process_harness(
        &repo,
        std::env::var("HEDDLE_AGENT_PROVIDER")
            .ok()
            .and_then(crate::attribution::clean_attribution_value),
        std::env::var("HEDDLE_AGENT_MODEL")
            .ok()
            .and_then(crate::attribution::clean_attribution_value),
        std::env::var("HEDDLE_AGENT_POLICY")
            .ok()
            .and_then(crate::attribution::clean_attribution_value),
    )?;
    // `--hold-for-pid PID` binds the reservation to an external
    // process (typically the orchestrator that wraps the heddle
    // CLI). Without it we record this one-shot CLI's pid, which
    // exits before the next liveness check — fine when the calling
    // script doesn't care about reaping, but means the dead-pid
    // reaper would recycle the reservation immediately if a second
    // agent races in. With `--hold-for-pid` the reservation tracks
    // the orchestrator's lifetime instead.
    let recorded_pid = args.hold_for_pid.unwrap_or_else(std::process::id);
    let outcome = registry.try_reserve_thread(&thread_name, |session_id| {
        Ok(AgentEntry {
            session_id: session_id.to_string(),
            client_instance_id: None,
            native_actor_key: None,
            native_parent_actor_key: None,
            native_instance_key: None,
            heddle_session_id: None,
            thread_id: Some(thread_name.clone()),
            thread: thread_name.clone(),
            pid: Some(recorded_pid),
            boot_id: current_boot_id(),
            liveness_path: Some(
                repo.heddle_dir()
                    .join("agents")
                    .join(format!("{session_id}.live")),
            ),
            heartbeat_at: Some(Utc::now()),
            anchor_state: Some(anchor_full_for_entry.clone()),
            anchor_root: Some(anchor_root.clone()),
            reservation_token: Some(objects::store::generate_agent_id()),
            path: reservation_path.clone(),
            base_state: anchor_short.clone(),
            started_at: Utc::now(),
            provider: probe.provider.clone(),
            model: probe.model.clone(),
            harness: probe
                .harness
                .clone()
                .or_else(|| Some("heddle-agent-api".to_string())),
            thinking_level: probe.thinking_level.clone(),
            usage_summary: AgentUsageSummary::default(),
            last_progress_at: None,
            report_flush_state: None,
            attach_reason: task.clone(),
            task_assignment_id: task_assignment_id.clone(),
            attach_precedence: vec!["agent-reserve".to_string()],
            winning_attach_rule: Some("agent-reserve".to_string()),
            probe_source: probe
                .probe_source
                .clone()
                .or_else(|| Some("agent_api".to_string())),
            probe_confidence: probe.confidence.or(Some(1.0)),
            status: AgentStatus::Active,
            completed_at: None,
            context_queries: vec![],
        })
    })?;

    let entry = match outcome {
        ReserveOutcome::Reserved(entry) => entry,
        ReserveOutcome::LiveOwner(existing) => {
            return Err(anyhow!(live_owner_conflict_advice(
                &thread_name,
                &anchor_full,
                &existing,
            )));
        }
    };

    // We hold the reservation. The remaining steps (CAS, oplog record,
    // thread metadata, JSON emit) must be all-or-nothing from the
    // caller's perspective: if any step fails after `try_reserve_thread`
    // wrote the Active entry, we have to mark that entry Abandoned
    // before returning the error. Otherwise the caller never sees a
    // session_id (no successful JSON output), but the registry retains
    // a live-owner row that ghost-blocks subsequent reservers — which
    // is exactly what `try_reserve_thread`'s own conflict logic would
    // hit, until pid-liveness reaping eventually clears it.
    //
    // The race the reviewer flagged: another writer advances the
    // thread ref between the pre-check at line 161 and the CAS below,
    // causing `set_thread_cas` to return ExpectationViolated. The
    // fallible closure here ensures we abandon the orphaned reservation
    // before bubbling that error up.
    let tn = ThreadName::new(&thread_name);
    let post_reserve = (|| -> Result<()> {
        if let Some(existing) = existing_ref {
            repo.refs()
                .set_thread_cas(&tn, RefExpectation::Value(existing), &anchor)?;
        } else {
            repo.refs()
                .set_thread_cas(&tn, RefExpectation::Missing, &anchor)?;
            // Agent-reservation flow writes the ThreadManager record via
            // `ensure_thread_record` below, after this op is recorded —
            // so there's no record to snapshot at recording time. Pass
            // `None`; the op records as `ThreadCreate` with no
            // manager snapshot. Reservations are an agent-internal API
            // that aren't expected to participate in human undo/redo
            // flows in 0.3. heddle#23 r2.
            repo.oplog()
                .record_thread_create(&tn, &anchor, None, Some(&repo.op_scope()))?;
        }

        // Ensure a Thread record exists so downstream commands
        // (`agent ready`, `thread show`, `ready`, `merge --preview`) have
        // first-class metadata to work with. `start_thread` does the
        // same; we mirror just the minimum required for the agent API.
        ensure_thread_record(&repo, &thread_name, &anchor, &args.task)?;

        render_agent_reservation_envelope(&repo, &entry)?;
        Ok(())
    })();

    if let Err(err) = post_reserve {
        // Best-effort abandon: if the registry write itself fails (FS
        // error mid-cleanup), surface the original error and let the
        // pid-based dead-owner reaper recycle the orphan on the next
        // reserve. Logging the secondary failure would be ideal but
        // would make the error wire-format dependent on transient FS
        // state, so we keep the structured surface clean.
        let _ = registry.update_entry(&entry.session_id, |e| {
            e.status = AgentStatus::Abandoned;
            e.completed_at = Some(Utc::now());
        });
        return Err(err);
    }

    Ok(())
}

fn existing_thread_execution_path(
    repo: &Repository,
    thread_name: &str,
) -> Result<Option<std::path::PathBuf>> {
    let Some(thread) = ThreadManager::new(repo.heddle_dir()).find_by_thread(thread_name)? else {
        return Ok(None);
    };
    let path = if !thread.execution_path.as_os_str().is_empty() {
        Some(thread.execution_path)
    } else {
        thread.materialized_path
    };
    Ok(path.map(|path| path.canonicalize().unwrap_or(path)))
}

/// Persist a minimal `Thread` record for `thread_name` if one does not
/// already exist. Mirrors the relevant fields from `start_thread`.
fn ensure_thread_record(
    repo: &Repository,
    thread_name: &str,
    anchor: &objects::object::StateId,
    task: &Option<String>,
) -> Result<()> {
    let manager = ThreadManager::new(repo.heddle_dir());
    if manager.load(thread_name)?.is_some() {
        return Ok(());
    }
    let state = repo
        .store()
        .get_state(anchor)?
        .ok_or_else(|| anyhow!("anchor state '{}' not found", anchor.short()))?;
    let base_short = anchor.short();
    let base_root = state.tree.short();
    let target_thread = match repo.head_ref()? {
        Head::Attached { thread } if thread != thread_name => Some(thread.to_string()),
        _ => None,
    };
    let thread_state = Thread {
        id: thread_name.to_string(),
        thread: thread_name.to_string(),
        target_thread,
        parent_thread: None,
        mode: ThreadMode::Materialized,
        state: ThreadState::Active,
        base_state: base_short.clone(),
        base_root,
        current_state: Some(base_short),
        merged_state: None,
        task: task.clone(),
        execution_path: repo.root().to_path_buf(),
        materialized_path: None,
        changed_paths: vec![],
        impact_categories: vec![],
        heavy_impact_paths: vec![],
        promotion_suggested: false,
        freshness: ThreadFreshness::Current,
        verification_summary: ThreadVerificationSummary::default(),
        confidence_summary: ThreadConfidenceSummary::default(),
        integration_policy_result: ThreadIntegrationPolicy::default(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        // Reservation-API-created threads aren't ephemeral by
        // default; orchestrators that want TTL-bounded threads pass
        // through `heddle thread create --ephemeral` instead.
        ephemeral: None,
        // Reservation API threads are user-orchestrated, not
        // harness-auto-created — leave them visible in the default
        // `thread list` view.
        auto: false,
        shared_target_dir: None,
    };
    manager.save(&thread_state)?;
    Ok(())
}

pub fn cmd_agent_heartbeat(cli: &Cli, args: AgentHeartbeatArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let registry = AgentRegistry::new(repo.heddle_dir());
    let now = Utc::now();
    let entry = registry
        .update_entry(&args.session, |entry| {
            touch_agent_heartbeat(entry, now);
        })?
        .ok_or_else(|| anyhow!(agent_session_not_found_advice(&args.session)))?;
    render_agent_reservation_envelope(&repo, &entry)
}

pub fn cmd_agent_release(cli: &Cli, args: AgentReleaseArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let registry = AgentRegistry::new(repo.heddle_dir());
    let status = match args.status {
        AgentReleaseStatusArg::Complete => AgentReleaseKind::Complete.to_status(),
        AgentReleaseStatusArg::Abandoned => AgentReleaseKind::Abandoned.to_status(),
    };
    let now = Utc::now();
    let entry = registry
        .update_entry(&args.session, |entry| {
            touch_agent_release(entry, status.clone(), now);
        })?
        .ok_or_else(|| anyhow!(agent_session_not_found_advice(&args.session)))?;
    render_agent_reservation_envelope(&repo, &entry)
}

pub fn cmd_agent_list(cli: &Cli, args: AgentApiListArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let registry = AgentRegistry::new(repo.heddle_dir());
    if args.alive_only {
        // Sweep dead reservations before reporting so callers asking
        // "who is alive?" see a pid-checked, current view.
        registry.reap_dead()?;
    }
    let list =
        assemble_agent_reservation_list(registry.list()?, args.thread.clone(), args.alive_only);
    render_agent_list(
        AgentReservationListOutput {
            reservations: list
                .reservations
                .into_iter()
                .map(AgentReservationOutput::from)
                .collect(),
            alive_only: list.alive_only,
            thread: list.thread,
            trust: build_repository_verification_state(&repo),
        },
        should_output_json(cli, Some(repo.config())),
    )
}

pub fn cmd_agent_task(cli: &Cli, command: AgentTaskCommands) -> Result<()> {
    match command {
        AgentTaskCommands::Create(args) => cmd_agent_task_create(cli, args),
        AgentTaskCommands::List(args) => cmd_agent_task_list(cli, args),
        AgentTaskCommands::Show(args) => cmd_agent_task_show(cli, args),
        AgentTaskCommands::Update(args) => cmd_agent_task_update(cli, args),
    }
}

pub fn cmd_agent_fanout(cli: &Cli, command: AgentFanoutCommands) -> Result<()> {
    match command {
        AgentFanoutCommands::Plan(args) => cmd_agent_fanout_plan(cli, args),
        AgentFanoutCommands::Start(args) => cmd_agent_fanout_start(cli, args),
    }
}

fn cmd_agent_fanout_plan(cli: &Cli, args: AgentFanoutPlanArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let (base_state, base_root) = fanout_base(&repo)?;
    let parent_thread = fanout_parent_thread(&repo)?;
    let plan = plan_fanout(&FanoutPlanRequest {
        title: args.title,
        lanes: args.lane,
        coordination_discussion_id: args.coordination_discussion_id,
        base_state,
        base_root,
        parent_thread,
    })
    .map_err(map_fanout_plan_error)?;
    let report = assemble_fanout_plan_report(&plan);
    let output = AgentFanoutOutput {
        output_kind: report.output_kind,
        title: report.title,
        parent_thread: report.parent_thread,
        base_state: report.base_state,
        base_root: report.base_root,
        coordination_discussion_id: report.coordination_discussion_id,
        parent_task: None,
        lanes: report
            .lanes
            .into_iter()
            .map(|lane| AgentFanoutLaneOutput {
                thread: lane.thread,
                path: lane.path,
                title: lane.title,
                task: None,
                session_id: lane.session_id,
                status: lane.status,
            })
            .collect(),
        commands: report
            .commands
            .into_iter()
            .map(|command| AgentFanoutCommandOutput {
                lane_thread: command.lane_thread,
                command: command.command,
                argv: command.argv,
            })
            .collect(),
        trust: build_repository_verification_state(&repo),
    };
    render_agent_fanout_output(output, should_output_json(cli, Some(repo.config())))
}

fn cmd_agent_fanout_start(cli: &Cli, args: AgentFanoutStartArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    if let Some(advice) = git_overlay_mutation_preflight_advice(
        &repo,
        "agent fanout start",
        GitOverlayMutationPreflight::capture_like(),
    )? {
        return Err(anyhow!(advice));
    }
    ensure_worktree_clean(&repo, "agent fanout start")?;

    let (base_state, base_root) = fanout_base(&repo)?;
    let parent_thread = fanout_parent_thread(&repo)?;
    let plan = plan_fanout(&FanoutPlanRequest {
        title: args.title.clone(),
        lanes: args.lane,
        coordination_discussion_id: args.coordination_discussion_id.clone(),
        base_state: base_state.clone(),
        base_root: base_root.clone(),
        parent_thread: parent_thread.clone(),
    })
    .map_err(map_fanout_plan_error)?;
    preflight_fanout_start_io(&repo, &plan.nodes)?;
    let store = AgentTaskStore::new(repo.heddle_dir());

    let mut parent = AgentTaskRecord::new(String::new(), plan.title.clone(), parent_thread.clone());
    parent.body = plan.parent_body.clone();
    parent.base_state = Some(base_state.clone());
    parent.base_root = Some(base_root.clone());
    parent.coordination_discussion_id = plan.coordination_discussion_id.clone();
    parent.allow_offline = true;
    parent.delegated_by = Some(fanout_parent_delegated_by().to_string());
    parent.status = AgentTaskStatus::InProgress;
    let parent = store.create(parent)?;

    let attach_rule = fanout_start_attach_rule();
    let mut created_task_ids = vec![parent.task_id.clone()];
    let start_result = (|| -> Result<Vec<AgentFanoutLaneOutput>> {
        let mut outputs = Vec::new();
        for lane in &plan.nodes {
            let mut child =
                AgentTaskRecord::new(String::new(), lane.title.clone(), lane.thread.clone());
            child.body = fanout_child_body(&parent.task_id);
            child.base_state = Some(base_state.clone());
            child.base_root = Some(base_root.clone());
            child.parent_task_id = Some(parent.task_id.clone());
            child.coordination_discussion_id = plan.coordination_discussion_id.clone();
            child.allow_offline = true;
            child.delegated_by = Some(parent.task_id.clone());
            child.status = AgentTaskStatus::InProgress;
            let child = store.create(child)?;
            created_task_ids.push(child.task_id.clone());

            let started = super::thread::start_thread(
                &repo,
                ThreadStartArgs {
                    name: lane.thread.clone(),
                    from: Some(base_state.clone()),
                    path: Some(lane.path.clone()),
                    workspace: WorkspaceModeArg::Auto,
                    agent_provider: None,
                    agent_model: None,
                    task: Some(lane.title.clone()),
                    parent_thread: Some(parent_thread.clone()),
                    automated: true,
                    print_cd_path: false,
                    daemon: true,
                    no_daemon: false,
                    shared_target: false,
                    hydrate: false,
                },
            )?;
            let session_id = started
                .thread
                .as_ref()
                .and_then(|thread| thread.session_id.clone());
            if let Some(session_id) = session_id.as_deref() {
                let registry = AgentRegistry::new(repo.heddle_dir());
                let _ = registry.update_entry(session_id, |entry| {
                    entry.task_assignment_id = Some(child.task_id.clone());
                    entry.attach_reason = Some(lane.title.clone());
                    entry.attach_precedence.push(attach_rule.to_string());
                    entry.winning_attach_rule = Some(attach_rule.to_string());
                })?;
            }
            outputs.push(AgentFanoutLaneOutput {
                thread: lane.thread.clone(),
                path: lane.path.display().to_string(),
                title: lane.title.clone(),
                task: Some(AgentTaskOutput::from(&child)),
                session_id,
                status: "started".to_string(),
            });
        }
        Ok(outputs)
    })();

    let outputs = match start_result {
        Ok(outputs) => outputs,
        Err(err) => {
            abandon_fanout_tasks(&store, &created_task_ids);
            return Err(err);
        }
    };

    let output =
        build_fanout_start_output(&repo, &plan, Some(AgentTaskOutput::from(&parent)), outputs);
    render_agent_fanout_output(output, should_output_json(cli, Some(repo.config())))
}

fn cmd_agent_task_create(cli: &Cli, args: AgentTaskCreateArgs) -> Result<()> {
    ThreadId::new(args.thread.as_str()).map_err(|err| anyhow!(thread_name_invalid_advice(&err)))?;
    if let Some(task_id) = args.task_id.as_deref() {
        validate_task_id(task_id).map_err(|err| anyhow!(err))?;
    }
    let repo = cli.open_repo()?;
    let mut record = AgentTaskRecord::new(
        args.task_id.unwrap_or_default(),
        args.title,
        args.thread.clone(),
    );
    record.body = args.body.unwrap_or_default();
    record.base_state = args.base_state;
    record.base_root = args.base_root;
    record.parent_task_id = args.parent_task_id;
    record.coordination_discussion_id = args.coordination_discussion_id;
    record.allow_offline = args.allow_offline;
    record.delegated_by = args.delegated_by;
    let store = AgentTaskStore::new(repo.heddle_dir());
    let created = store.create(record)?;
    render_agent_task_envelope(
        &repo,
        &created,
        "agent_task_create",
        should_output_json(cli, Some(repo.config())),
    )
}

fn cmd_agent_task_list(cli: &Cli, args: AgentTaskListArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let status_filter = args.status.as_ref().map(agent_task_status_from_arg);
    let store = AgentTaskStore::new(repo.heddle_dir());
    let tasks: Vec<_> = store
        .list()?
        .into_iter()
        .filter(|task| {
            args.thread
                .as_ref()
                .is_none_or(|thread| &task.target_thread == thread)
        })
        .filter(|task| {
            status_filter
                .as_ref()
                .is_none_or(|status| &task.status == status)
        })
        .map(|task| AgentTaskOutput::from(&task))
        .collect();
    render_agent_task_list(
        AgentTaskListOutput {
            output_kind: "agent_task_list",
            tasks,
            thread: args.thread,
            status: status_filter.map(|status| status.to_string()),
            trust: build_repository_verification_state(&repo),
        },
        should_output_json(cli, Some(repo.config())),
    )
}

fn cmd_agent_task_show(cli: &Cli, args: AgentTaskShowArgs) -> Result<()> {
    validate_task_id(&args.task_id).map_err(|err| anyhow!(err))?;
    let repo = cli.open_repo()?;
    let store = AgentTaskStore::new(repo.heddle_dir());
    let task = store
        .load(&args.task_id)?
        .ok_or_else(|| anyhow!(agent_task_not_found_advice(&args.task_id)))?;
    render_agent_task_envelope(
        &repo,
        &task,
        "agent_task_show",
        should_output_json(cli, Some(repo.config())),
    )
}

fn cmd_agent_task_update(cli: &Cli, args: AgentTaskUpdateArgs) -> Result<()> {
    validate_task_id(&args.task_id).map_err(|err| anyhow!(err))?;
    if let Some(thread) = args.thread.as_deref() {
        ThreadId::new(thread).map_err(|err| anyhow!(thread_name_invalid_advice(&err)))?;
    }
    let repo = cli.open_repo()?;
    let store = AgentTaskStore::new(repo.heddle_dir());
    let updated = store
        .update(&args.task_id, |task| {
            if let Some(title) = args.title.clone() {
                task.title = title;
            }
            if let Some(body) = args.body.clone() {
                task.body = body;
            }
            if let Some(status) = args.status.as_ref() {
                task.status = agent_task_status_from_arg(status);
            }
            if let Some(thread) = args.thread.clone() {
                task.target_thread = thread;
            }
            if let Some(base_state) = args.base_state.clone() {
                task.base_state = Some(base_state);
            }
            if let Some(base_root) = args.base_root.clone() {
                task.base_root = Some(base_root);
            }
            if let Some(parent_task_id) = args.parent_task_id.clone() {
                task.parent_task_id = Some(parent_task_id);
            }
            if let Some(discussion_id) = args.coordination_discussion_id.clone() {
                task.coordination_discussion_id = Some(discussion_id);
            }
            if args.allow_offline {
                task.allow_offline = true;
            }
            if args.no_allow_offline {
                task.allow_offline = false;
            }
            if let Some(delegated_by) = args.delegated_by.clone() {
                task.delegated_by = Some(delegated_by);
            }
        })?
        .ok_or_else(|| anyhow!(agent_task_not_found_advice(&args.task_id)))?;
    render_agent_task_envelope(
        &repo,
        &updated,
        "agent_task_update",
        should_output_json(cli, Some(repo.config())),
    )
}

fn agent_task_status_from_arg(status: &AgentTaskStatusArg) -> AgentTaskStatus {
    match status {
        AgentTaskStatusArg::Open => AgentTaskStatus::Open,
        AgentTaskStatusArg::InProgress => AgentTaskStatus::InProgress,
        AgentTaskStatusArg::Blocked => AgentTaskStatus::Blocked,
        AgentTaskStatusArg::Complete => AgentTaskStatus::Complete,
        AgentTaskStatusArg::Abandoned => AgentTaskStatus::Abandoned,
    }
}

fn map_fanout_plan_error(err: FanoutPlanError) -> anyhow::Error {
    match err {
        FanoutPlanError::LaneRequired => anyhow!(RecoveryAdvice::invalid_usage(
            "agent_fanout_lane_required",
            "agent fanout requires at least one --lane <thread>=<path>:<title>",
            "Pass --lane once for each child checkout to create.",
            "heddle agent fanout plan --title <title> --lane <thread>=<path>:<title>",
        )),
        FanoutPlanError::LaneInvalid { raw } => anyhow!(RecoveryAdvice::invalid_usage(
            "agent_fanout_lane_invalid",
            format!("invalid fanout lane '{raw}'"),
            "Use <thread>=<path>:<title>. Thread, path, and title must all be non-empty.",
            "heddle agent fanout plan --title <title> --lane feature/a=../a:Task title",
        )),
        FanoutPlanError::InvalidThreadName { source, .. } => {
            anyhow!(thread_name_invalid_advice(&source))
        }
        FanoutPlanError::DuplicateThread { thread } => anyhow!(fanout_lane_unavailable_advice(
            "agent_fanout_duplicate_thread",
            &thread,
            format!("fanout lane '{thread}' is listed more than once"),
            "Use each child thread name once per fanout.",
        )),
    }
}

fn fanout_base(repo: &Repository) -> Result<(String, String)> {
    let head = repo
        .head()?
        .ok_or_else(|| anyhow!("repository has no HEAD state for agent fanout"))?;
    let state = repo
        .store()
        .get_state(&head)?
        .ok_or_else(|| anyhow!("HEAD state '{}' not found", head.short()))?;
    Ok((head.to_string_full(), state.tree.short()))
}

fn fanout_parent_thread(repo: &Repository) -> Result<String> {
    Ok(match repo.head_ref()? {
        Head::Attached { thread } => select_fanout_parent_thread(Some(thread.as_str())),
        Head::Detached { .. } => select_fanout_parent_thread(None),
    })
}

fn abandon_fanout_tasks(store: &AgentTaskStore, task_ids: &[String]) {
    for task_id in task_ids {
        let _ = store.update(task_id, |task| {
            task.status = AgentTaskStatus::Abandoned;
        });
    }
}

/// I/O preflight for fanout start after pure [`plan_fanout`].
///
/// Gathers live facts (reservations, refs, thread records, resolved paths),
/// then applies pure [`check_fanout_start_preflight`].
fn preflight_fanout_start_io(repo: &Repository, lanes: &[FanoutNodeSpec]) -> Result<()> {
    let manager = ThreadManager::new(repo.heddle_dir());
    let mut facts = Vec::with_capacity(lanes.len());
    for lane in lanes {
        let prepared = plan_worktree_target(repo, &lane.path, Some(&lane.thread))?;
        let active_thread_record = match manager.find_by_thread(&lane.thread)? {
            Some(existing) => existing.state == ThreadState::Active,
            None => false,
        };
        facts.push(FanoutLaneAvailability {
            thread: lane.thread.clone(),
            has_live_owner: super::thread::find_active_thread_entry(repo, &lane.thread)?.is_some(),
            thread_ref_exists: repo
                .refs()
                .get_thread(&ThreadName::new(&lane.thread))?
                .is_some(),
            active_thread_record,
            resolved_path: prepared.path,
        });
    }
    if let Err(block) = check_fanout_start_preflight(&facts) {
        return Err(anyhow!(fanout_lane_preflight_block_advice(block)));
    }
    Ok(())
}

fn fanout_lane_preflight_block_advice(block: FanoutLanePreflightBlock) -> RecoveryAdvice {
    let thread = block.thread().to_string();
    match block {
        FanoutLanePreflightBlock::LiveOwner { .. } => fanout_lane_unavailable_advice(
            "agent_fanout_live_owner",
            &thread,
            format!("fanout lane '{thread}' already has an active agent reservation"),
            "Release the active reservation or choose a fresh child thread.",
        ),
        FanoutLanePreflightBlock::ThreadExists { .. } => fanout_lane_unavailable_advice(
            "agent_fanout_thread_exists",
            &thread,
            format!("fanout lane '{thread}' already exists"),
            "Choose a fresh child thread or inspect the existing thread before retrying.",
        ),
        FanoutLanePreflightBlock::ActiveThreadRecord { .. } => fanout_lane_unavailable_advice(
            "agent_fanout_thread_exists",
            &thread,
            format!("fanout lane '{thread}' already has an active thread record"),
            "Drop or finish the existing thread before reusing the lane name.",
        ),
        FanoutLanePreflightBlock::DuplicatePath { .. } => fanout_lane_unavailable_advice(
            "agent_fanout_duplicate_path",
            &thread,
            format!("fanout lane '{thread}' resolves to a checkout path used by another lane"),
            "Use a distinct checkout path for each child lane.",
        ),
    }
}

fn fanout_lane_unavailable_advice(
    kind: &'static str,
    thread: &str,
    error: String,
    guidance: &'static str,
) -> RecoveryAdvice {
    RecoveryAdvice::invalid_usage(
        kind,
        error,
        guidance,
        format!("heddle agent fanout plan --title <title> --lane {thread}=<path>:<title>"),
    )
}

fn build_fanout_start_output(
    repo: &Repository,
    plan: &FanoutPlan,
    parent_task: Option<AgentTaskOutput>,
    lanes: Vec<AgentFanoutLaneOutput>,
) -> AgentFanoutOutput {
    AgentFanoutOutput {
        output_kind: "agent_fanout_start",
        title: plan.title.clone(),
        parent_thread: plan.parent_thread.clone(),
        base_state: plan.base_state.clone(),
        base_root: plan.base_root.clone(),
        coordination_discussion_id: plan.coordination_discussion_id.clone(),
        parent_task,
        lanes,
        commands: Vec::new(),
        trust: build_repository_verification_state(repo),
    }
}

fn render_agent_fanout_output(output: AgentFanoutOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }
    println!(
        "Agent fanout {}: {}",
        output
            .output_kind
            .strip_prefix("agent_fanout_")
            .unwrap_or(output.output_kind),
        output.title
    );
    if let Some(parent_task) = &output.parent_task {
        println!(
            "  parent task: {}",
            crate::cli::style::accent(&parent_task.task_id)
        );
    }
    for lane in &output.lanes {
        println!(
            "  {} [{}] {}",
            crate::cli::style::accent(&lane.thread),
            lane.status,
            crate::cli::style::dim(&lane.path),
        );
        if let Some(task) = &lane.task {
            println!("    task: {}", crate::cli::style::dim(&task.task_id));
        }
    }
    if output.output_kind == "agent_fanout_plan" {
        println!("Commands:");
        for command in &output.commands {
            println!("  {}", command.command);
        }
    }
    Ok(())
}

fn render_agent_list(output: AgentReservationListOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }
    let entries = output.reservations;
    if entries.is_empty() {
        println!("No agent reservations.");
        return Ok(());
    }
    println!("Agent reservations ({}):", entries.len());
    for entry in entries {
        println!(
            "  {} [{}] thread={}",
            crate::cli::style::accent(&entry.session_id),
            entry.status,
            entry.thread,
        );
        if let Some(task) = &entry.task {
            println!("    task: {}", crate::cli::style::dim(task));
        }
        if let Some(path) = &entry.path
            && !path.is_empty()
        {
            println!("    path: {}", crate::cli::style::dim(path));
        }
    }
    Ok(())
}

fn render_agent_task_envelope(
    repo: &Repository,
    task: &AgentTaskRecord,
    output_kind: &'static str,
    json: bool,
) -> Result<()> {
    let output = AgentTaskEnvelope {
        output_kind,
        task: AgentTaskOutput::from(task),
        trust: build_repository_verification_state(repo),
    };
    if json {
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }
    println!(
        "Agent task {} [{}]",
        crate::cli::style::accent(&output.task.task_id),
        output.task.status
    );
    println!("  title: {}", output.task.title);
    println!("  thread: {}", output.task.target_thread);
    if !output.task.body.is_empty() {
        println!("  body: {}", output.task.body);
    }
    if let Some(base_state) = &output.task.base_state {
        println!("  base_state: {}", crate::cli::style::dim(base_state));
    }
    if let Some(base_root) = &output.task.base_root {
        println!("  base_root: {}", crate::cli::style::dim(base_root));
    }
    Ok(())
}

fn render_agent_task_list(output: AgentTaskListOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }
    if output.tasks.is_empty() {
        println!("No agent tasks.");
        return Ok(());
    }
    println!("Agent tasks ({}):", output.tasks.len());
    for task in output.tasks {
        println!(
            "  {} [{}] thread={} title={}",
            crate::cli::style::accent(&task.task_id),
            task.status,
            task.target_thread,
            task.title,
        );
    }
    Ok(())
}

fn reservation_envelope(repo: &Repository, entry: &AgentEntry) -> AgentReservationEnvelope {
    AgentReservationEnvelope {
        reservation: AgentReservationOutput::from(entry),
        trust: build_repository_verification_state(repo),
    }
}

fn render_agent_reservation_envelope(repo: &Repository, entry: &AgentEntry) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string(&reservation_envelope(repo, entry))?
    );
    Ok(())
}

fn agent_session_not_found_advice(session_id: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "agent_session_not_found",
        format!("agent session '{session_id}' not found"),
        "Reserve the thread again before retrying the guarded agent command.",
        format!("no reservation entry exists for session {session_id}"),
        "continuing would let an unknown session mutate repository state",
        "no session heartbeat, capture, readiness, refs, or worktree changes were applied",
        "heddle agent reserve --thread <thread>",
        vec!["heddle agent reserve --thread <thread>".to_string()],
    )
}

/// Resolve `--session SID` to an Active reservation, refresh its
/// heartbeat, and return the entry. Errors out cleanly when the
/// session is missing, terminal, or reaped between calls — that's the
/// signal the orchestrator must re-reserve before continuing.
fn validate_active_session(
    registry: &AgentRegistry,
    session_id: &str,
) -> Result<objects::store::AgentEntry> {
    let now = Utc::now();
    let entry = registry
        .update_entry(session_id, |entry| {
            touch_agent_heartbeat(entry, now);
        })?
        .ok_or_else(|| anyhow!(agent_session_not_found_advice(session_id)))?;
    if !session_is_active(&entry.status) {
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "agent_session_inactive",
            format!(
                "agent session '{}' is no longer active (status: {})",
                session_id, entry.status
            ),
            "Reserve the thread again before retrying the guarded agent command.",
            format!("session {session_id} has terminal status {}", entry.status),
            "continuing would let a stale reservation write or mark readiness after ownership ended",
            "the session heartbeat was refreshed, but no capture, ready, refs, or worktree changes were applied",
            "heddle agent reserve --thread <thread>",
            vec!["heddle agent reserve --thread <thread>".to_string()],
        )));
    }
    Ok(entry)
}

/// `heddle agent capture --session <SID>`: a session-validated
/// alias for `heddle capture` that proves the caller still owns the
/// reservation it claims to before writing.
pub async fn cmd_agent_capture(
    cli: &Cli,
    args: crate::cli::cli_args::AgentCaptureArgs,
) -> Result<()> {
    let plan = plan_agent_capture(&AgentCaptureOptions {
        session: args.session.clone(),
        message: args.message.clone(),
        confidence: args.confidence,
    })
    .map_err(|err| anyhow!(err))?;
    let repo_path = cli
        .repo
        .clone()
        .unwrap_or(std::env::current_dir().map_err(anyhow::Error::from)?);
    let repo = Repository::open(&repo_path)?;
    let registry = AgentRegistry::new(repo.heddle_dir());
    let entry = validate_active_session(&registry, &plan.session)?;

    // Confirm the reservation still names the thread the caller is
    // attached to. We don't switch threads here — the agent must
    // already be on its reserved thread when invoking capture.
    if let AgentCaptureThreadCheck::Mismatch {
        reserved_thread,
        current_thread,
    } = check_agent_capture_thread(&entry.thread, repo.current_lane()?.as_deref())
    {
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "agent_session_thread_mismatch",
            format!(
                "agent session '{}' reserved thread '{reserved_thread}', but the current thread is '{current_thread}'",
                plan.session
            ),
            format!(
                "Switch to the reserved thread with `heddle switch {reserved_thread}` before capturing."
            ),
            format!(
                "session {} owns thread {reserved_thread}, current checkout is attached to {current_thread}",
                plan.session
            ),
            "capturing from the wrong thread would attribute work to a reservation that does not own this checkout",
            "the session heartbeat was refreshed, but no capture, refs, or worktree changes were applied",
            format!("heddle switch {reserved_thread}"),
            vec![format!("heddle switch {reserved_thread}")],
        )));
    }

    super::snapshot::cmd_snapshot(
        cli,
        plan.message.clone(),
        plan.confidence,
        false,
        super::snapshot::SnapshotAgentOverrides {
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            session: Some(plan.session.clone()),
            segment: None,
            policy: None,
            no_policy: false,
            no_agent: false,
        },
    )
    .await
}

/// `heddle agent ready --session <SID>`: a session-validated alias
/// for `heddle ready` that ensures the caller still owns the
/// reservation it's trying to mark ready.
pub async fn cmd_agent_ready(cli: &Cli, args: crate::cli::cli_args::AgentReadyArgs) -> Result<()> {
    let options = AgentReadyOptions {
        session: args.session.clone(),
        message: args.message.clone(),
        confidence: args.confidence,
    };
    let repo_path = cli
        .repo
        .clone()
        .unwrap_or(std::env::current_dir().map_err(anyhow::Error::from)?);
    let repo = Repository::open(&repo_path)?;
    let registry = AgentRegistry::new(repo.heddle_dir());
    let entry = validate_active_session(&registry, &options.session)?;
    let plan = plan_agent_ready(&entry, &options).map_err(|err| anyhow!(err))?;

    super::ready_cmd::cmd_ready(
        cli,
        crate::cli::cli_args::ReadyArgs {
            thread: Some(plan.thread),
            message: plan.message,
            confidence: plan.confidence,
        },
    )
    .await
}

/// Return the combined JSON schema for the public agent-API output
/// types. Snapshot-tested in `tests/agent_api_schema.rs` so any
/// breaking change to the wire shape is caught at PR review.
pub fn agent_api_schema() -> serde_json::Value {
    serde_json::json!({
        "AgentReservationEnvelope": schemars::schema_for!(AgentReservationEnvelope),
        "AgentReservationListOutput": schemars::schema_for!(AgentReservationListOutput),
        "AgentReservationOutput": schemars::schema_for!(AgentReservationOutput),
        "AgentTaskEnvelope": schemars::schema_for!(AgentTaskEnvelope),
        "AgentTaskListOutput": schemars::schema_for!(AgentTaskListOutput),
        "AgentTaskOutput": schemars::schema_for!(AgentTaskOutput),
        "AgentFanoutOutput": schemars::schema_for!(AgentFanoutOutput),
        "AgentFanoutLaneOutput": schemars::schema_for!(AgentFanoutLaneOutput),
        "AgentFanoutCommandOutput": schemars::schema_for!(AgentFanoutCommandOutput),
    })
}
