// SPDX-License-Identifier: Apache-2.0
//! Stable JSON-first agent reservation API.

use anyhow::{Result, anyhow};
use chrono::Utc;
use objects::{
    object::ThreadName,
    store::{
        AgentEntry, AgentRegistry, AgentStatus, AgentUsageSummary, ObjectStore, ReserveOutcome,
        current_boot_id,
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
    git_overlay_health::{RepositoryVerificationState, build_repository_verification_state},
    thread::thread_name_invalid_advice,
};
use crate::cli::{
    Cli,
    cli_args::{
        AgentApiListArgs, AgentHeartbeatArgs, AgentReleaseArgs, AgentReleaseStatusArg,
        AgentReserveArgs,
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

impl From<&AgentEntry> for AgentReservationOutput {
    fn from(entry: &AgentEntry) -> Self {
        Self {
            session_id: entry.session_id.clone(),
            reservation_token: entry.reservation_token.clone(),
            thread: entry.thread.clone(),
            anchor_state: entry.anchor_state.clone(),
            anchor_root: entry.anchor_root.clone(),
            status: entry.status.to_string(),
            path: entry.path.as_ref().map(|path| path.display().to_string()),
            task: entry.attach_reason.clone(),
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            harness: entry.harness.clone(),
            thinking_level: entry.thinking_level.clone(),
            probe_source: entry.probe_source.clone(),
            probe_confidence: entry.probe_confidence,
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
    let thread_name = args.thread.clone();

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
    anchor: &objects::object::ChangeId,
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
    let entry = registry
        .update_entry(&args.session, |entry| {
            entry.heartbeat_at = Some(Utc::now());
            entry.last_progress_at = Some(Utc::now());
        })?
        .ok_or_else(|| anyhow!(agent_session_not_found_advice(&args.session)))?;
    render_agent_reservation_envelope(&repo, &entry)
}

pub fn cmd_agent_release(cli: &Cli, args: AgentReleaseArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let registry = AgentRegistry::new(repo.heddle_dir());
    let status = match args.status {
        AgentReleaseStatusArg::Complete => AgentStatus::Complete,
        AgentReleaseStatusArg::Abandoned => AgentStatus::Abandoned,
    };
    let entry = registry
        .update_entry(&args.session, |entry| {
            entry.status = status.clone();
            entry.completed_at = match entry.status {
                AgentStatus::Active => None,
                AgentStatus::Abandoned | AgentStatus::Complete | AgentStatus::Merged => {
                    Some(Utc::now())
                }
            };
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
    let entries: Vec<_> = registry
        .list()?
        .into_iter()
        .filter(|entry| {
            args.thread
                .as_ref()
                .is_none_or(|thread| &entry.thread == thread)
        })
        .filter(|entry| !args.alive_only || entry.status == AgentStatus::Active)
        .map(|entry| AgentReservationOutput::from(&entry))
        .collect();
    render_agent_list(
        AgentReservationListOutput {
            reservations: entries,
            alive_only: args.alive_only,
            thread: args.thread.clone(),
            trust: build_repository_verification_state(&repo),
        },
        should_output_json(cli, Some(repo.config())),
    )
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
    let entry = registry
        .update_entry(session_id, |entry| {
            entry.heartbeat_at = Some(Utc::now());
            entry.last_progress_at = Some(Utc::now());
        })?
        .ok_or_else(|| anyhow!(agent_session_not_found_advice(session_id)))?;
    if entry.status != AgentStatus::Active {
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
    let repo_path = cli
        .repo
        .clone()
        .unwrap_or(std::env::current_dir().map_err(anyhow::Error::from)?);
    let repo = Repository::open(&repo_path)?;
    let registry = AgentRegistry::new(repo.heddle_dir());
    let entry = validate_active_session(&registry, &args.session)?;

    // Confirm the reservation still names the thread the caller is
    // attached to. We don't switch threads here — the agent must
    // already be on its reserved thread when invoking capture.
    if let Some(current) = repo.current_lane()?
        && current != entry.thread
    {
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "agent_session_thread_mismatch",
            format!(
                "agent session '{}' reserved thread '{}', but the current thread is '{}'",
                args.session, entry.thread, current
            ),
            format!(
                "Switch to the reserved thread with `heddle switch {}` before capturing.",
                entry.thread
            ),
            format!(
                "session {} owns thread {}, current checkout is attached to {}",
                args.session, entry.thread, current
            ),
            "capturing from the wrong thread would attribute work to a reservation that does not own this checkout",
            "the session heartbeat was refreshed, but no capture, refs, or worktree changes were applied",
            format!("heddle switch {}", entry.thread),
            vec![format!("heddle switch {}", entry.thread)],
        )));
    }

    super::snapshot::cmd_snapshot(
        cli,
        args.message.clone(),
        args.confidence,
        false,
        super::snapshot::SnapshotAgentOverrides {
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            session: Some(args.session.clone()),
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
    let repo_path = cli
        .repo
        .clone()
        .unwrap_or(std::env::current_dir().map_err(anyhow::Error::from)?);
    let repo = Repository::open(&repo_path)?;
    let registry = AgentRegistry::new(repo.heddle_dir());
    let entry = validate_active_session(&registry, &args.session)?;

    super::ready_cmd::cmd_ready(
        cli,
        crate::cli::cli_args::ReadyArgs {
            thread: Some(entry.thread.clone()),
            message: args.message.clone(),
            confidence: args.confidence,
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
    })
}
