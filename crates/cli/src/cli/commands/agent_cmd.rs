// SPDX-License-Identifier: Apache-2.0
//! Stable JSON-first agent reservation API.

use anyhow::{Result, anyhow};
use chrono::Utc;
use objects::store::{
    AgentEntry, AgentRegistry, AgentStatus, AgentUsageSummary, ReserveOutcome, current_boot_id,
};
use refs::{Head, RefExpectation};
use repo::{
    Repository, Thread, ThreadConfidenceSummary, ThreadFreshness, ThreadIntegrationPolicy,
    ThreadManager, ThreadMode, ThreadState, ThreadVerificationSummary,
};
use schemars::JsonSchema;
use serde::Serialize;

use super::advice::RecoveryAdvice;
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
        }
    }
}

/// Stable structured conflict shape emitted on stdout when `agent
/// reserve` cannot proceed. Orchestrators parse this; humans see the
/// shorter `Error: ...` message anyhow renders to stderr.
#[derive(Serialize, JsonSchema)]
pub struct AgentReservationConflict {
    /// `"live_owner"` (existing reservation matches the requested
    /// anchor — wait or release) or `"anchor_drift"` (existing
    /// reservation is on a different anchor — refresh and retry).
    pub kind: &'static str,
    pub thread: String,
    pub requested_anchor: String,
    /// `Some` when a live agent already holds the thread; `None` when
    /// the thread ref exists at a different state with no live owner.
    pub owner: Option<AgentReservationOutput>,
    /// Anchor recorded against the existing reservation or thread ref,
    /// when known. Always present for anchor-drift conflicts so
    /// orchestrators can decide whether to refresh.
    pub reserved_anchor: Option<String>,
    pub message: String,
}

fn emit_live_owner_conflict(
    thread: &str,
    requested_anchor_full: &str,
    owner: &AgentEntry,
) -> anyhow::Error {
    let kind = if owner.anchor_state.as_deref() == Some(requested_anchor_full) {
        "live_owner"
    } else {
        "anchor_drift"
    };
    let message = if kind == "live_owner" {
        format!(
            "thread '{}' already has a live reservation on session '{}'. Use `heddle thread show {}` or release the session before starting another writer.",
            thread, owner.session_id, thread
        )
    } else {
        format!(
            "thread '{}' is reserved by session '{}' on anchor {}, but you requested {}. Refresh the thread or rebase before retrying.",
            thread,
            owner.session_id,
            owner.anchor_state.as_deref().unwrap_or("<unknown>"),
            requested_anchor_full
        )
    };
    let conflict = AgentReservationConflict {
        kind,
        thread: thread.to_string(),
        requested_anchor: requested_anchor_full.to_string(),
        owner: Some(AgentReservationOutput::from(owner)),
        reserved_anchor: owner.anchor_state.clone(),
        message: message.clone(),
    };
    if let Ok(json) = serde_json::to_string(&conflict) {
        println!("{}", json);
    }
    anyhow!(message)
}

fn emit_anchor_drift_no_owner(
    thread: &str,
    requested_anchor_full: &str,
    reserved_anchor: &str,
) -> anyhow::Error {
    let message = format!(
        "thread '{}' is anchored at {}, but reservation requested {}. Refresh the thread or rebase before retrying.",
        thread, reserved_anchor, requested_anchor_full
    );
    let conflict = AgentReservationConflict {
        kind: "anchor_drift",
        thread: thread.to_string(),
        requested_anchor: requested_anchor_full.to_string(),
        owner: None,
        reserved_anchor: Some(reserved_anchor.to_string()),
        message: message.clone(),
    };
    if let Ok(json) = serde_json::to_string(&conflict) {
        println!("{}", json);
    }
    anyhow!(message)
}

pub fn cmd_agent_reserve(cli: &Cli, args: AgentReserveArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
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
    // structured JSON conflict is emitted on stdout.
    let existing_ref = repo.refs().get_thread(&thread_name)?;
    if let Some(existing) = existing_ref
        && existing != anchor
    {
        // Look for a live owner first — if one exists, route through
        // emit_live_owner_conflict so the caller sees the owner's
        // session_id alongside the drift.
        let registry = AgentRegistry::new(repo.heddle_dir());
        registry.reap_dead_for_thread(&thread_name)?;
        if let Some(owner) = registry
            .list()?
            .into_iter()
            .find(|entry| entry.status == AgentStatus::Active && entry.thread == thread_name)
        {
            return Err(emit_live_owner_conflict(&thread_name, &anchor_full, &owner));
        }
        return Err(emit_anchor_drift_no_owner(
            &thread_name,
            &anchor_full,
            &existing.to_string_full(),
        ));
    }

    let registry = AgentRegistry::new(repo.heddle_dir());
    let task = args.task.clone();
    let anchor_full_for_entry = anchor_full.clone();
    let anchor_short = anchor.short();
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
            path: None,
            base_state: anchor_short.clone(),
            started_at: Utc::now(),
            provider: None,
            model: None,
            harness: Some("heddle-agent-api".to_string()),
            thinking_level: None,
            usage_summary: AgentUsageSummary::default(),
            last_progress_at: None,
            report_flush_state: None,
            attach_reason: task.clone(),
            attach_precedence: vec!["agent-reserve".to_string()],
            winning_attach_rule: Some("agent-reserve".to_string()),
            probe_source: Some("agent_api".to_string()),
            probe_confidence: Some(1.0),
            status: AgentStatus::Active,
            completed_at: None,
            context_queries: vec![],
        })
    })?;

    let entry = match outcome {
        ReserveOutcome::Reserved(entry) => entry,
        ReserveOutcome::LiveOwner(existing) => {
            return Err(emit_live_owner_conflict(
                &thread_name,
                &anchor_full,
                &existing,
            ));
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
    let post_reserve = (|| -> Result<()> {
        if let Some(existing) = existing_ref {
            repo.refs()
                .set_thread_cas(&thread_name, RefExpectation::Value(existing), &anchor)?;
        } else {
            repo.refs()
                .set_thread_cas(&thread_name, RefExpectation::Missing, &anchor)?;
            // Agent-reservation flow writes the ThreadManager record via
            // `ensure_thread_record` below, after this op is recorded —
            // so there's no record to snapshot at recording time. Pass
            // `None`; the op records as `ThreadCreateV2` with no
            // manager snapshot. Reservations are an agent-internal API
            // that aren't expected to participate in human undo/redo
            // flows in 0.3. heddle#23 r2.
            repo.oplog().record_thread_create(
                &thread_name,
                &anchor,
                None,
                Some(&repo.op_scope()),
            )?;
        }

        // Ensure a Thread record exists so downstream commands
        // (`agent ready`, `thread show`, `ready`, `merge --preview`) have
        // first-class metadata to work with. `start_thread` does the
        // same; we mirror just the minimum required for the agent API.
        ensure_thread_record(&repo, &thread_name, &anchor, &args.task)?;

        println!(
            "{}",
            serde_json::to_string(&AgentReservationOutput::from(&entry))?
        );
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
        Head::Attached { thread } if thread != thread_name => Some(thread),
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
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let registry = AgentRegistry::new(repo.heddle_dir());
    let entry = registry
        .update_entry(&args.session, |entry| {
            entry.heartbeat_at = Some(Utc::now());
            entry.last_progress_at = Some(Utc::now());
        })?
        .ok_or_else(|| anyhow!(agent_session_not_found_advice(&args.session)))?;
    println!(
        "{}",
        serde_json::to_string(&AgentReservationOutput::from(&entry))?
    );
    Ok(())
}

pub fn cmd_agent_release(cli: &Cli, args: AgentReleaseArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
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
    println!(
        "{}",
        serde_json::to_string(&AgentReservationOutput::from(&entry))?
    );
    Ok(())
}

pub fn cmd_agent_list(cli: &Cli, args: AgentApiListArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
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
    render_agent_list(&entries, should_output_json(cli, Some(repo.config())))
}

fn render_agent_list(entries: &[AgentReservationOutput], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(entries)?);
        return Ok(());
    }
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
            no_agent: entry.provider.is_none() && entry.model.is_none(),
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
        },
    )
    .await
}

/// Return the combined JSON schema for the public agent-API output
/// types. Snapshot-tested in `tests/agent_api_schema.rs` so any
/// breaking change to the wire shape is caught at PR review.
pub fn agent_api_schema() -> serde_json::Value {
    serde_json::json!({
        "AgentReservationOutput": schemars::schema_for!(AgentReservationOutput),
        "AgentReservationConflict": schemars::schema_for!(AgentReservationConflict),
    })
}
