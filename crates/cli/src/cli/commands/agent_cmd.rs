// SPDX-License-Identifier: Apache-2.0
//! Stable JSON-first agent reservation API.

use std::{
    path::Path,
    process::{Child, Command, Stdio},
};

use anyhow::{Result, anyhow};
use chrono::Utc;
// The agent wire payloads live in cli-contract so the schema registry
// registers the real serialization types.
pub(crate) use heddle_cli_contract::cli::commands::wire::agent::{
    AgentFanoutCommandOutput, AgentFanoutLaneOutput, AgentFanoutOutput, AgentReservationEnvelope,
    AgentReservationListOutput, AgentReservationOutput, AgentTaskEnvelope, AgentTaskListOutput,
    AgentTaskOutput,
};
use objects::{
    object::ThreadName,
    store::{
        ObjectStore, WriterLease, WriterLeaseAuthOutcome, WriterLeaseDraft,
        WriterLeaseReserveOutcome, WriterLeaseStatus, WriterLeaseStore, current_boot_id,
    },
};
use refs::{Head, RefExpectation};
use repo::{
    ActorPresence, ActorPresenceStatus, ActorPresenceStore, AgentTaskRecord, AgentTaskStatus,
    AgentTaskStore, AgentUsageSummary, Repository, RepositoryCapability, Thread,
    ThreadConfidenceSummary, ThreadFreshness, ThreadId, ThreadIntegrationPolicy, ThreadManager,
    ThreadMode, ThreadState, ThreadVerificationSummary,
    checkout_writer::{
        lock_checkout_writer_handoff, remove_checkout_writer_credential,
        write_checkout_writer_credential,
    },
    shell_quote, validate_task_id,
};
use sley::{
    HeadUpdateOptions, IndexWriteOptions, RefChange, ReferenceTarget, Repository as SleyRepository,
};
use verbs::{
    AgentCaptureOptions, AgentCaptureThreadCheck, AgentReadyOptions, FanoutLaneAvailability,
    FanoutLanePreflightBlock, FanoutNodeSpec, FanoutPlan, FanoutPlanError, FanoutPlanRequest,
    assemble_agent_reservation_list, check_agent_capture_thread, check_fanout_start_preflight,
    fanout_child_body, fanout_parent_delegated_by, fanout_start_attach_rule, plan_agent_capture,
    plan_agent_ready, plan_fanout, select_fanout_parent_thread,
};

use super::{
    advice::RecoveryAdvice,
    next_action::{NextActionValidationContext, write_full_command_json},
    snapshot::ensure_current_state,
    thread::thread_name_invalid_advice,
    verification_health::{
        GitOverlayMutationPreflight, build_repository_verification_state,
        git_overlay_mutation_preflight_advice,
    },
    worktree_cmd::helpers::plan_worktree_target,
    worktree_safety::ensure_worktree_clean,
};
use crate::{
    cli::{
        Cli,
        cli_args::{
            AgentApiListArgs, AgentFanoutCommands, AgentFanoutPlanArgs, AgentFanoutStartArgs,
            AgentHeartbeatArgs, AgentReleaseArgs, AgentReleaseStatusArg, AgentReserveArgs,
            AgentTaskCommands, AgentTaskCreateArgs, AgentTaskListArgs, AgentTaskShowArgs,
            AgentTaskStatusArg, AgentTaskUpdateArgs, FanoutHarnessArg, ThreadStartArgs,
            WorkspaceModeArg,
        },
        should_output_json,
    },
    config::UserConfig,
};

fn live_owner_conflict_advice(
    thread: &str,
    requested_anchor_full: &str,
    owner: &WriterLease,
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
                "thread '{thread}' already has active writer lease '{}'",
                owner.lease_id
            ),
            format!(
                "Inspect it with `{primary_command}`, or release that lease before starting another writer."
            ),
            format!(
                "thread '{thread}' is reserved by writer lease '{}' at anchor {}",
                owner.lease_id,
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
                "thread '{thread}' is reserved by lease '{}' on anchor {}, but reservation requested {requested_anchor_full}",
                owner.lease_id,
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
    ThreadId::new(args.thread.as_str()).map_err(|err| anyhow!(thread_name_invalid_advice(&err)))?;
    let repo = cli.open_repo()?;
    let thread_name = args.thread.clone();
    let reservation_path = existing_thread_execution_path(&repo, &thread_name)?;
    let handoff_lock = reservation_path
        .as_deref()
        .map(|path| lock_checkout_writer_handoff(repo.heddle_dir(), path))
        .transpose()?;
    let anchor = match &args.anchor {
        Some(spec) => repo.resolve_state(spec)?.ok_or_else(|| {
            anyhow!(RecoveryAdvice::invalid_usage(
                "agent_anchor_not_found",
                format!("anchor state '{spec}' not found"),
                "Choose a state shown by `heddle log`.",
                "heddle log",
            ))
        })?,
        None => repo.head()?.ok_or_else(|| {
            anyhow!(RecoveryAdvice::invalid_usage(
                "agent_reservation_missing_head",
                "repository has no HEAD state to reserve from",
                "Capture a state before reserving this thread.",
                "heddle status",
            ))
        })?,
    };
    let state = repo.store().get_state(&anchor)?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::invalid_usage(
            "agent_anchor_not_found",
            format!("anchor state '{}' not found", anchor.short()),
            "Run `heddle verify` to inspect the missing state.",
            "heddle verify",
        ))
    })?;
    let anchor_full = anchor.to_string_full();
    let anchor_short = anchor.short();
    let anchor_root = state.tree.short();
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

    let lease_store = WriterLeaseStore::new(repo.heddle_dir());
    let owner =
        if let (Some(path), Some(guard)) = (reservation_path.as_deref(), handoff_lock.as_ref()) {
            lease_store.live_owner_with_checkout_lock(&thread_name, path, guard)?
        } else {
            lease_store.live_owner(&thread_name, reservation_path.as_deref())?
        };
    if let Some(owner) = owner {
        return Err(anyhow!(live_owner_conflict_advice(
            &thread_name,
            &anchor_full,
            &owner,
        )));
    }

    let existing_ref = repo.refs().get_thread(&ThreadName::new(&thread_name))?;
    if let Some(existing) = existing_ref
        && existing != anchor
    {
        return Err(anyhow!(anchor_drift_no_owner_advice(
            &thread_name,
            &anchor_full,
            &existing.to_string_full(),
        )));
    }

    let probe = crate::harness::probe_current_process_harness(
        &repo,
        std::env::var("HEDDLE_AGENT_PROVIDER")
            .ok()
            .and_then(hosted_client::attribution::clean_attribution_value),
        std::env::var("HEDDLE_AGENT_MODEL")
            .ok()
            .and_then(hosted_client::attribution::clean_attribution_value),
        std::env::var("HEDDLE_AGENT_POLICY")
            .ok()
            .and_then(hosted_client::attribution::clean_attribution_value),
    )?;
    let presence_store = ActorPresenceStore::new(repo.heddle_dir());
    let task_assignment_id = task_record.as_ref().map(|task| task.task_id.clone());
    let presence = presence_store.create_generated_entry(|session_id| {
        Ok(ActorPresence {
            session_id: session_id.to_string(),
            client_instance_id: None,
            native_actor_key: None,
            native_parent_actor_key: None,
            native_instance_key: None,
            heddle_session_id: None,
            thread_id: Some(thread_name.clone()),
            thread: thread_name.clone(),
            anchor_state: Some(anchor_full.clone()),
            anchor_root: Some(anchor_root.clone()),
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
            attach_reason: args.task.clone(),
            task_assignment_id: task_assignment_id.clone(),
            attach_precedence: vec!["agent-reserve".to_string()],
            winning_attach_rule: Some("agent-reserve".to_string()),
            probe_source: probe
                .probe_source
                .clone()
                .or_else(|| Some("agent_api".to_string())),
            probe_confidence: probe.confidence.or(Some(1.0)),
            status: ActorPresenceStatus::Active,
            completed_at: None,
            context_queries: vec![],
        })
    })?;

    let thread = ThreadName::new(&thread_name);
    if let Some(existing) = existing_ref {
        repo.set_thread_recorded_cas(&thread, RefExpectation::Value(existing), &anchor)?;
    } else {
        repo.set_thread_recorded_cas(&thread, RefExpectation::Missing, &anchor)?;
    }
    ensure_thread_record(&repo, &thread_name, &anchor, &args.task)?;

    let recorded_pid = args.hold_for_pid;
    let outcome = lease_store.reserve_with_checkout_lock(
        WriterLeaseDraft {
            thread: thread_name.clone(),
            actor_session_id: Some(presence.session_id),
            task_assignment_id,
            anchor_state: Some(anchor_full.clone()),
            anchor_root: Some(anchor_root),
            path: reservation_path,
            pid: recorded_pid,
            boot_id: recorded_pid.and_then(|_| current_boot_id()),
        },
        Utc::now(),
    )?;
    let grant = match outcome {
        WriterLeaseReserveOutcome::Reserved(grant) => grant,
        WriterLeaseReserveOutcome::LiveOwner(owner) => {
            return Err(anyhow!(live_owner_conflict_advice(
                &thread_name,
                &anchor_full,
                &owner,
            )));
        }
    };

    render_agent_reservation_envelope(&repo, &grant.lease, Some(grant.token))
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
    if manager.load_id_or_name(thread_name)?.is_some() {
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
    let store = WriterLeaseStore::new(repo.heddle_dir());
    let lease = authenticate_writer_lease(&store, &args.lease, &args.token)?;
    render_agent_reservation_envelope(&repo, &lease, None)
}

pub fn cmd_agent_release(cli: &Cli, args: AgentReleaseArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let store = WriterLeaseStore::new(repo.heddle_dir());
    let current = store.load(&args.lease)?;
    let _handoff_lock = current
        .as_ref()
        .and_then(|lease| lease.path.as_deref())
        .map(|path| lock_checkout_writer_handoff(repo.heddle_dir(), path))
        .transpose()?;
    let status = match args.status {
        AgentReleaseStatusArg::Complete => WriterLeaseStatus::Complete,
        AgentReleaseStatusArg::Abandoned => WriterLeaseStatus::Abandoned,
    };
    let outcome = store.release_with_checkout_lock(&args.lease, &args.token, status, Utc::now())?;
    let lease = match outcome {
        WriterLeaseAuthOutcome::Authorized(lease) | WriterLeaseAuthOutcome::Inactive(lease) => {
            lease
        }
        other => authorized_lease_outcome(other, &args.lease)?,
    };
    if lease.path != current.and_then(|lease| lease.path) {
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "writer_lease_checkout_changed",
            "writer lease checkout changed during release",
            "Inspect the reservation before retrying cleanup.",
            "the lease checkout changed while release was in progress",
            "removing a credential from the wrong checkout could revoke another writer",
            "the credential was left in place; the lease release may already be recorded",
            "heddle agent list",
            vec!["heddle agent list".to_string()],
        )));
    }
    if let Some(path) = lease.path.as_deref() {
        remove_checkout_writer_credential(path, &args.lease)?;
    }
    render_agent_reservation_envelope(&repo, &lease, None)
}

pub fn cmd_agent_list(cli: &Cli, args: AgentApiListArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let store = WriterLeaseStore::new(repo.heddle_dir());
    let list = assemble_agent_reservation_list(store.list()?, args.thread.clone(), args.alive_only);
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
    let output = AgentFanoutOutput {
        output_kind: "agent_fanout_plan",
        title: plan.title.clone(),
        parent_thread: plan.parent_thread.clone(),
        base_state: plan.base_state.clone(),
        base_root: plan.base_root.clone(),
        coordination_discussion_id: plan.coordination_discussion_id.clone(),
        parent_task: None,
        lanes: plan
            .nodes
            .iter()
            .map(|lane| AgentFanoutLaneOutput {
                thread: lane.thread.clone(),
                path: repo
                    .managed_checkout_path(&lane.thread)
                    .display()
                    .to_string(),
                title: lane.title.clone(),
                task: None,
                session_id: None,
                lease_id: None,
                status: "planned".to_string(),
            })
            .collect(),
        commands: plan
            .start_commands
            .iter()
            .map(|command| AgentFanoutCommandOutput {
                lane_thread: command.lane_thread.clone(),
                command: command.command.clone(),
                argv: command.argv.clone(),
                env_unset: Vec::new(),
                cwd: None,
                harness: None,
                credential_file: None,
            })
            .collect(),
        trust: build_repository_verification_state(&repo),
    };
    render_agent_fanout_output(output, should_output_json(cli, Some(repo.config())))
}

fn cmd_agent_fanout_start(cli: &Cli, args: AgentFanoutStartArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    if !args.harness.is_empty() && args.harness.len() != args.lane.len() {
        return Err(anyhow!(RecoveryAdvice::invalid_usage(
            "agent_fanout_harness_count",
            "supply one --harness for each --lane",
            "Pair each lane with its harness in argument order.",
            "heddle agent fanout start --title <title> --lane <thread>=<title> --harness codex",
        )));
    }
    let harnesses = if args.harness.is_empty() {
        vec![FanoutHarnessArg::Codex; args.lane.len()]
    } else {
        args.harness.clone()
    };
    if let Some(advice) = git_overlay_mutation_preflight_advice(
        &repo,
        "agent fanout start",
        GitOverlayMutationPreflight::capture_like(),
    )? {
        return Err(anyhow!(advice));
    }
    if repo.capability() == RepositoryCapability::GitOverlay {
        preflight_fanout_git_overlay(&repo)?;
        super::snapshot::bind_git_overlay_active_tip(&repo)?;
    } else {
        ensure_worktree_clean(&repo, "agent fanout start")?;
    }
    if repo.head()?.is_none() {
        ensure_current_state(
            &repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some("Bootstrap git-overlay before agent fanout".to_string()),
        )?;
    }
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
    let mut entered_threads = Vec::new();
    let start_result = (|| -> Result<Vec<AgentFanoutLaneOutput>> {
        let mut outputs = Vec::new();
        for lane in &plan.nodes {
            entered_threads.push(lane.thread.clone());
            let checkout_path = repo.managed_checkout_path(&lane.thread);
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
                    path: Some(checkout_path.clone()),
                    workspace: Some(if repo.capability() == RepositoryCapability::GitOverlay {
                        WorkspaceModeArg::Solid
                    } else {
                        WorkspaceModeArg::Auto
                    }),
                    agent_provider: None,
                    agent_model: None,
                    task: Some(lane.title.clone()),
                    parent_thread: Some(parent_thread.clone()),
                    automated: true,
                    print_cd_path: false,
                    daemon: true,
                    no_daemon: false,
                    interactive_setup: false,
                    shared_target: false,
                    no_shared_target: false,
                    hydrate: false,
                },
            )?;
            if repo.capability() == RepositoryCapability::GitOverlay {
                link_fanout_child_git(&repo, &checkout_path, &lane.thread)?;
            }
            let session_id = started
                .thread
                .as_ref()
                .and_then(|thread| thread.session_id.clone());
            if let Some(session_id) = session_id.as_deref() {
                let registry = ActorPresenceStore::new(repo.heddle_dir());
                let _ = registry.update_entry(session_id, |entry| {
                    entry.task_assignment_id = Some(child.task_id.clone());
                    entry.attach_reason = Some(lane.title.clone());
                    entry.attach_precedence.push(attach_rule.to_string());
                    entry.winning_attach_rule = Some(attach_rule.to_string());
                })?;
            }
            let _handoff_lock = lock_checkout_writer_handoff(repo.heddle_dir(), &checkout_path)?;
            let lease = WriterLeaseStore::new(repo.heddle_dir()).reserve_with_checkout_lock(
                WriterLeaseDraft {
                    thread: lane.thread.clone(),
                    actor_session_id: session_id.clone(),
                    task_assignment_id: Some(child.task_id.clone()),
                    anchor_state: Some(base_state.clone()),
                    anchor_root: Some(base_root.clone()),
                    path: Some(checkout_path.clone()),
                    pid: None,
                    boot_id: None,
                },
                Utc::now(),
            )?;
            let grant = match lease {
                WriterLeaseReserveOutcome::Reserved(grant) => grant,
                WriterLeaseReserveOutcome::LiveOwner(owner) => {
                    return Err(anyhow!(live_owner_conflict_advice(
                        &lane.thread,
                        &base_state,
                        &owner,
                    )));
                }
            };
            write_checkout_writer_credential(&checkout_path, &grant.lease.lease_id, &grant.token)?;
            outputs.push(AgentFanoutLaneOutput {
                thread: lane.thread.clone(),
                path: checkout_path.display().to_string(),
                title: lane.title.clone(),
                task: Some(AgentTaskOutput::from(&child)),
                session_id,
                lease_id: Some(grant.lease.lease_id),
                status: "started".to_string(),
            });
        }
        Ok(outputs)
    })();

    let outputs = match start_result {
        Ok(outputs) => outputs,
        Err(err) => {
            if let Err(rollback_error) =
                rollback_fanout_start(&repo, &store, &entered_threads, &created_task_ids)
            {
                return Err(anyhow!(RecoveryAdvice::safety_refusal(
                    "agent_fanout_rollback_failed",
                    format!("{err}; fanout rollback failed: {rollback_error}"),
                    "Inspect the remaining lanes and writer leases before retrying.",
                    "fanout creation failed and cleanup could not remove every created lane",
                    "a retry could collide with a remaining checkout or writer lease",
                    "cleanup was attempted for every lane, lease, and task in this batch",
                    "heddle status",
                    vec!["heddle status".to_string()],
                )));
            }
            return Err(err);
        }
    };

    let mut output =
        build_fanout_start_output(&repo, &plan, Some(AgentTaskOutput::from(&parent)), outputs);
    output.commands = output
        .lanes
        .iter()
        .zip(harnesses.iter().copied())
        .map(|(lane, harness)| fanout_launch_command(lane, harness))
        .collect();
    if args.run {
        let json = should_output_json(cli, Some(repo.config()));
        let mut children = Vec::new();
        for command in &output.commands {
            match run_fanout_harness(command, json) {
                Ok(child) => children.push((command.lane_thread.clone(), child)),
                Err(error) => {
                    for (_, mut child) in children {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    return Err(error);
                }
            }
        }
        let mut failure = None;
        for (lane, mut child) in children {
            match child.wait() {
                Ok(status) if !status.success() && failure.is_none() => {
                    failure = Some(anyhow!(fanout_launch_failure_advice(
                        &lane,
                        format!("harness exited with {status}"),
                    )));
                }
                Err(error) if failure.is_none() => {
                    failure = Some(anyhow!(fanout_launch_failure_advice(
                        &lane,
                        format!("waiting for harness failed: {error}"),
                    )));
                }
                _ => {}
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
    }
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
            "agent fanout requires at least one --lane <thread>=<title>",
            "Pass --lane once for each Child Thread to create.",
            "heddle agent fanout plan --title <title> --lane <thread>=<title>",
        )),
        FanoutPlanError::LaneInvalid { raw } => anyhow!(RecoveryAdvice::invalid_usage(
            "agent_fanout_lane_invalid",
            format!("invalid fanout lane '{raw}'"),
            "Use <thread>=<title>. Thread and title must both be non-empty.",
            "heddle agent fanout plan --title <title> --lane feature/a=Task title",
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
    let head = repo.head()?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::invalid_usage(
            "agent_fanout_missing_head",
            "repository has no HEAD state for agent fanout",
            "Capture a state before starting fanout.",
            "heddle status",
        ))
    })?;
    let state = repo.store().get_state(&head)?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::invalid_usage(
            "agent_fanout_head_missing",
            format!("HEAD state '{}' not found", head.short()),
            "Run `heddle verify` to inspect the missing state.",
            "heddle verify",
        ))
    })?;
    Ok((head.to_string_full(), state.tree.short()))
}

fn fanout_parent_thread(repo: &Repository) -> Result<String> {
    Ok(match repo.head_ref()? {
        Head::Attached { thread } => select_fanout_parent_thread(Some(thread.as_str())),
        Head::Detached { .. } => select_fanout_parent_thread(None),
    })
}

fn preflight_fanout_git_overlay(repo: &Repository) -> Result<()> {
    let git = repo.git_overlay_sley_repository()?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::invalid_usage(
            "agent_fanout_git_repository_missing",
            "Git overlay has no Git repository",
            "Repair the Git checkout before starting fanout.",
            "heddle doctor",
        ))
    })?;
    if git.head()?.oid.is_none() {
        let command = format!(
            "git -C {path} add -A && git -C {path} commit --allow-empty -m 'Initial commit'",
            path = shell_quote(&repo.root().display().to_string())
        );
        let mut advice = RecoveryAdvice::safety_refusal(
            "agent_fanout_unborn_git_head",
            "Git HEAD has no commit to bind fanout lanes to",
            format!("Create the first Git commit with `{command}`, then retry."),
            "Git overlay has no committed HEAD",
            "a lane without its own Git HEAD could commit into the parent repository",
            "no lane, task, or reservation was created",
            "heddle status",
            vec!["heddle status".to_string()],
        );
        advice.extra_json_fields.insert(
            "git_recovery_command".to_string(),
            serde_json::Value::String(command),
        );
        return Err(anyhow!(advice));
    }
    let status = repo.git_overlay_worktree_status()?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::safety_refusal(
            "agent_fanout_git_status_unavailable",
            "Could not inspect Git overlay worktree status",
            "Repair the Git checkout before starting fanout.",
            "Git worktree status is unavailable",
            "fanout could copy uncommitted work into its lanes",
            "no lane, task, or reservation was created",
            "heddle doctor",
            vec!["heddle doctor".to_string()],
        ))
    })?;
    if !status.is_clean() {
        let command = format!(
            "git -C {path} add -A && git -C {path} commit -m 'Prepare fanout'",
            path = shell_quote(&repo.root().display().to_string())
        );
        let mut advice = RecoveryAdvice::safety_refusal(
            "agent_fanout_dirty_git_overlay",
            "Git overlay has uncommitted work",
            format!("Commit the intended work with `{command}`, then retry."),
            format!(
                "{} staged, unstaged, or untracked path(s) differ from committed Git HEAD",
                status.change_count()
            ),
            "fanout would copy uncommitted work into every lane",
            "parent work and Git index were left unchanged; no lane, task, or reservation was created",
            "heddle status",
            vec!["heddle status".to_string()],
        );
        advice.extra_json_fields.insert(
            "git_recovery_command".to_string(),
            serde_json::Value::String(command),
        );
        return Err(anyhow!(advice));
    }
    Ok(())
}

fn fanout_git_link_failure_advice(thread: &str, detail: impl Into<String>) -> RecoveryAdvice {
    let primary = "heddle status".to_string();
    RecoveryAdvice::safety_refusal(
        "agent_fanout_git_link_failed",
        format!("Could not create Git linkage for lane '{thread}'"),
        format!("Inspect the repository with `{primary}` before retrying."),
        detail.into(),
        "continuing without child Git metadata would let Git commands resolve to the parent",
        "the failed lane checkout and thread record were removed",
        primary.clone(),
        vec![primary],
    )
}

fn fanout_launch_failure_advice(thread: &str, detail: impl Into<String>) -> RecoveryAdvice {
    let primary = format!("heddle thread show {thread}");
    RecoveryAdvice::safety_refusal(
        "agent_fanout_launch_failed",
        format!("Harness for lane '{thread}' did not complete"),
        format!("Inspect the lane with `{primary}` before launching it again."),
        detail.into(),
        "blindly relaunching could duplicate agent work in the same checkout",
        "the lane checkout and task remain; any harness edits remain",
        primary.clone(),
        vec![primary],
    )
}

/// Give a nested lane its own Git index, HEAD and refs. Sley copies the
/// committed base history; the Heddle-materialized checkout bytes stay put.
fn link_fanout_child_git(parent: &Repository, child: &Path, thread: &str) -> Result<()> {
    let source = parent.git_overlay_sley_repository()?.ok_or_else(|| {
        anyhow!(fanout_git_link_failure_advice(
            thread,
            "Git overlay has no Git repository",
        ))
    })?;
    let tip = source.head()?.oid.ok_or_else(|| {
        anyhow!(fanout_git_link_failure_advice(
            thread,
            "Git overlay has no committed HEAD",
        ))
    })?;
    let git = SleyRepository::init_with_format(child, source.object_format(), false)?;
    git.copy_reachable_from(&source, &[tip])?;
    let branch = format!("refs/heads/{thread}");
    git.apply_ref_changes(&[RefChange::new(
        branch.as_str(),
        ReferenceTarget::Direct(tip),
    )?])?;
    git.set_head_symref(&branch, HeadUpdateOptions::new())?;
    let commit = git.read_commit(&tip)?;
    let index = git.index_from_tree(&commit.tree)?;
    git.write_index(
        &index,
        IndexWriteOptions {
            fsync: true,
            validate_checksum: true,
        },
    )?;
    Repository::ensure_git_overlay_local_excludes(child)?;
    Ok(())
}

fn fanout_launch_command(
    lane: &AgentFanoutLaneOutput,
    harness: FanoutHarnessArg,
) -> AgentFanoutCommandOutput {
    let credential_file = Path::new(&lane.path)
        .join(".heddle/writer-credential.json")
        .display()
        .to_string();
    let mut argv = vec![harness.executable().to_string()];
    argv.push(
        match harness {
            FanoutHarnessArg::ClaudeCode => "--print",
            FanoutHarnessArg::Codex => "exec",
            FanoutHarnessArg::Opencode => "run",
        }
        .to_string(),
    );
    argv.push(lane.title.clone());
    let env_unset = FANOUT_IDENTITY_ENV_PATTERNS
        .iter()
        .map(|pattern| (*pattern).to_string())
        .collect::<Vec<_>>();
    let command = format!(
        "cd {} && (for key in $(env | cut -d= -f1); do case \"$key\" in {}) unset \"$key\" ;; esac; done; export HEDDLE_WRITER_CREDENTIAL_FILE={}; exec {})",
        shell_quote(&lane.path),
        env_unset.join("|"),
        shell_quote(&credential_file),
        argv.iter()
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    );
    AgentFanoutCommandOutput {
        lane_thread: lane.thread.clone(),
        command,
        argv,
        env_unset,
        cwd: Some(lane.path.clone()),
        harness: Some(harness.label().to_string()),
        credential_file: Some(credential_file),
    }
}

fn run_fanout_harness(command: &AgentFanoutCommandOutput, json: bool) -> Result<Child> {
    let path = command.credential_file.as_deref().ok_or_else(|| {
        anyhow!(fanout_launch_failure_advice(
            &command.lane_thread,
            "missing lane credential path"
        ))
    })?;
    let (program, args) = command.argv.split_first().ok_or_else(|| {
        anyhow!(fanout_launch_failure_advice(
            &command.lane_thread,
            "empty harness command"
        ))
    })?;
    let mut launch = Command::new(program);
    launch
        .args(args)
        .current_dir(command.cwd.as_deref().ok_or_else(|| {
            anyhow!(fanout_launch_failure_advice(
                &command.lane_thread,
                "missing lane checkout"
            ))
        })?);
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|key| {
            FANOUT_IDENTITY_ENV_PATTERNS
                .iter()
                .any(|pattern| match pattern.strip_suffix('*') {
                    Some(prefix) => key.starts_with(prefix),
                    None => key == *pattern,
                })
        }) {
            launch.env_remove(key);
        }
    }
    let child = launch
        .env("HEDDLE_WRITER_CREDENTIAL_FILE", path)
        .stdin(Stdio::null())
        .stdout(if json {
            Stdio::null()
        } else {
            Stdio::inherit()
        })
        .spawn()?;
    Ok(child)
}

// One list drives the printed shell command, JSON env_unset, and --run.
// Cover direct credentials, principal/agent attribution, and the paths Heddle
// uses to find stored credentials and device identity.
const FANOUT_IDENTITY_ENV_PATTERNS: &[&str] = &[
    "HEDDLE_CREDENTIAL",
    "HEDDLE_WRITER_*",
    "HEDDLE_RESERVATION_*",
    "HEDDLE_PRINCIPAL_*",
    "HEDDLE_AGENT_*",
    "HEDDLE_SESSION_*",
    "HEDDLE_HOME",
    "HEDDLE_CONFIG",
    "XDG_CONFIG_HOME",
    "HOME",
    "HEDDLE_REMOTE_IROH_DESCRIPTOR_*",
];

fn rollback_fanout_start(
    repo: &Repository,
    store: &AgentTaskStore,
    threads: &[String],
    task_ids: &[String],
) -> Result<()> {
    let manager = ThreadManager::new(repo.heddle_dir());
    let mut errors = Vec::new();
    for thread in threads.iter().rev() {
        let result = (|| -> Result<()> {
            if let Some(record) = manager.load_id_or_name(thread)? {
                super::thread_cmd::drop_thread_silent(repo, thread, true, true)?;
                manager.delete(&record.id)?;
            } else {
                repo::thread_manifest::remove_thread_manifest_dir(repo.heddle_dir(), thread)?;
                let thread_name = ThreadName::new(thread);
                if repo.refs().get_thread(&thread_name)?.is_some() {
                    repo.delete_thread_recorded(&thread_name)?;
                }
            }
            let registry = ActorPresenceStore::new(repo.heddle_dir());
            for entry in registry.list()? {
                if entry.thread == *thread {
                    registry.delete(&entry.session_id)?;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            errors.push(format!("{thread}: {error}"));
        }
    }
    if let Err(error) = WriterLeaseStore::new(repo.heddle_dir()).delete_for_tasks(task_ids) {
        errors.push(format!("writer leases: {error}"));
    }
    for task_id in task_ids.iter().rev() {
        if let Err(error) = store.delete(task_id) {
            errors.push(format!("task {task_id}: {error}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(errors.join("; ")))
    }
}

/// I/O preflight for fanout start after pure [`plan_fanout`].
///
/// Gathers live facts (reservations, refs, thread records, resolved paths),
/// then applies pure [`check_fanout_start_preflight`].
fn preflight_fanout_start_io(repo: &Repository, lanes: &[FanoutNodeSpec]) -> Result<()> {
    let manager = ThreadManager::new(repo.heddle_dir());
    let leases = WriterLeaseStore::new(repo.heddle_dir()).list()?;
    let mut facts = Vec::with_capacity(lanes.len());
    for lane in lanes {
        let path = repo.managed_checkout_path(&lane.thread);
        plan_worktree_target(repo, &path, Some(&lane.thread))?;
        let active_thread_record = match manager.find_by_thread(&lane.thread)? {
            Some(existing) => existing.state == ThreadState::Active,
            None => false,
        };
        facts.push(FanoutLaneAvailability {
            thread: lane.thread.clone(),
            has_live_owner: leases.iter().any(|lease| {
                lease.status == WriterLeaseStatus::Active && lease.thread == lane.thread
            }),
            thread_ref_exists: repo
                .refs()
                .get_thread(&ThreadName::new(&lane.thread))?
                .is_some(),
            active_thread_record,
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
        format!("heddle agent fanout plan --title <title> --lane {thread}=<title>"),
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
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["agent", "fanout"]),
        )?;
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
            crate::cli::style::accent(&crate::cli::style::human_text(&parent_task.task_id))
        );
    }
    for lane in &output.lanes {
        println!(
            "  {} [{}] {}",
            crate::cli::style::accent(&crate::cli::style::thread_label(&lane.thread, None)),
            lane.status,
            crate::cli::style::dim(&lane.path),
        );
        if let Some(task) = &lane.task {
            println!(
                "    task: {}",
                crate::cli::style::dim(&crate::cli::style::human_text(&task.task_id))
            );
        }
    }
    if !output.commands.is_empty() {
        println!(
            "{}",
            if output.output_kind == "agent_fanout_plan" {
                "Commands:"
            } else {
                "Launch commands:"
            }
        );
        for command in &output.commands {
            println!("  {}", crate::cli::style::human_text(&command.command));
        }
    }
    Ok(())
}

fn render_agent_list(output: AgentReservationListOutput, json: bool) -> Result<()> {
    if json {
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["agent", "list"]),
        )?;
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
            "  {} [{}; {}] thread={}",
            crate::cli::style::accent(&crate::cli::style::human_text(&entry.lease_id)),
            entry.status,
            entry.liveness,
            crate::cli::style::thread_label(&entry.thread, None),
        );
        if let Some(actor_session_id) = &entry.actor_session_id {
            println!(
                "    actor: {}",
                crate::cli::style::dim(&crate::cli::style::human_text(actor_session_id))
            );
        }
        if let Some(path) = &entry.path
            && !path.is_empty()
        {
            println!("    path: {}", crate::cli::style::dim(path));
        }
        println!(
            "    lease expires: {}",
            crate::cli::style::dim(&entry.lease_expires_at)
        );
        if entry.status == "abandoned" && entry.path.is_some() {
            println!(
                "    Next: heddle agent release --lease {} --token <token> --status abandoned",
                crate::cli::style::human_text(&entry.lease_id)
            );
            println!("    Read <token> from that lane's .heddle/writer-credential.json.");
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
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["agent", "task"]),
        )?;
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
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["agent", "task"]),
        )?;
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

fn reservation_envelope(
    repo: &Repository,
    lease: &WriterLease,
    token: Option<String>,
) -> AgentReservationEnvelope {
    AgentReservationEnvelope {
        reservation: AgentReservationOutput::from(lease),
        token,
        trust: build_repository_verification_state(repo),
    }
}

fn render_agent_reservation_envelope(
    repo: &Repository,
    lease: &WriterLease,
    token: Option<String>,
) -> Result<()> {
    write_full_command_json(
        &reservation_envelope(repo, lease, token),
        NextActionValidationContext::without_repo(&["agent"]),
    )
}

fn writer_lease_advice(kind: &'static str, lease_id: &str, error: String) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        kind,
        error,
        "Pass the lease token with --token or HEDDLE_RESERVATION_TOKEN. Reserve again if the lease expired.",
        format!("writer lease {lease_id} did not provide current authenticated authority"),
        "continuing could let an unowned agent mutate repository state",
        "no lease renewal, capture, ready, refs, or worktree changes were applied",
        format!("heddle agent heartbeat --lease {lease_id} --token <token>"),
        vec![
            format!("heddle agent heartbeat --lease {lease_id} --token <token>"),
            "heddle agent reserve --thread <thread>".to_string(),
        ],
    )
}

fn authorized_lease_outcome(
    outcome: WriterLeaseAuthOutcome,
    lease_id: &str,
) -> Result<WriterLease> {
    match outcome {
        WriterLeaseAuthOutcome::Authorized(lease) => Ok(lease),
        WriterLeaseAuthOutcome::Missing => Err(anyhow!(writer_lease_advice(
            "writer_lease_not_found",
            lease_id,
            format!("writer lease '{lease_id}' was not found"),
        ))),
        WriterLeaseAuthOutcome::TokenMismatch => Err(anyhow!(writer_lease_advice(
            "writer_lease_token_mismatch",
            lease_id,
            format!("writer lease token did not match lease '{lease_id}'"),
        ))),
        WriterLeaseAuthOutcome::Inactive(lease) => Err(anyhow!(writer_lease_advice(
            "writer_lease_inactive",
            lease_id,
            format!(
                "writer lease '{}' is no longer active (status: {})",
                lease.lease_id, lease.status
            ),
        ))),
    }
}

fn authenticate_writer_lease(
    store: &WriterLeaseStore,
    lease_id: &str,
    token: &str,
) -> Result<WriterLease> {
    authorized_lease_outcome(
        store.authenticate_and_renew(lease_id, token, Utc::now())?,
        lease_id,
    )
}

fn presence_for_lease(repo: &Repository, lease: &WriterLease) -> Result<Option<ActorPresence>> {
    let Some(session_id) = lease.actor_session_id.as_deref() else {
        return Ok(None);
    };
    Ok(ActorPresenceStore::new(repo.heddle_dir()).load(session_id)?)
}

pub async fn cmd_agent_capture(
    cli: &Cli,
    args: crate::cli::cli_args::AgentCaptureArgs,
) -> Result<()> {
    let plan = plan_agent_capture(&AgentCaptureOptions {
        lease: args.lease.clone(),
        message: args.message.clone(),
        confidence: args.confidence,
    })
    .map_err(|err| anyhow!(err))?;
    let repo_path = cli
        .repo
        .clone()
        .unwrap_or(std::env::current_dir().map_err(anyhow::Error::from)?);
    let repo = Repository::open(&repo_path)?;
    let lease = authenticate_writer_lease(
        &WriterLeaseStore::new(repo.heddle_dir()),
        &plan.lease,
        &args.token,
    )?;

    if let AgentCaptureThreadCheck::Mismatch {
        reserved_thread,
        current_thread,
    } = check_agent_capture_thread(&lease.thread, repo.current_lane()?.as_deref())
    {
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "writer_lease_thread_mismatch",
            format!(
                "writer lease '{}' owns thread '{reserved_thread}', but the current thread is '{current_thread}'",
                plan.lease
            ),
            format!("Switch with `heddle thread switch {reserved_thread}` before capturing."),
            format!(
                "lease {} owns thread {reserved_thread}, current checkout is attached to {current_thread}",
                plan.lease
            ),
            "capturing from the wrong thread would violate the lease's ownership scope",
            "the lease was renewed, but no capture, refs, or worktree changes were applied",
            format!("heddle thread switch {reserved_thread}"),
            vec![format!("heddle thread switch {reserved_thread}")],
        )));
    }

    repo.install_checkout_writer_credential(&plan.lease, &args.token)?;

    let presence = presence_for_lease(&repo, &lease)?;
    super::snapshot::cmd_snapshot(
        cli,
        plan.message,
        plan.confidence,
        false,
        super::snapshot::SnapshotAgentOverrides {
            provider: presence.as_ref().and_then(|entry| entry.provider.clone()),
            model: presence.as_ref().and_then(|entry| entry.model.clone()),
            session: lease.actor_session_id,
            segment: None,
            policy: None,
            no_policy: false,
            no_agent: false,
        },
    )
}

pub async fn cmd_agent_ready(cli: &Cli, args: crate::cli::cli_args::AgentReadyArgs) -> Result<()> {
    let options = AgentReadyOptions {
        lease: args.lease.clone(),
        message: args.message,
        confidence: args.confidence,
    };
    let repo_path = cli
        .repo
        .clone()
        .unwrap_or(std::env::current_dir().map_err(anyhow::Error::from)?);
    let repo = Repository::open(&repo_path)?;
    let lease = authenticate_writer_lease(
        &WriterLeaseStore::new(repo.heddle_dir()),
        &options.lease,
        &args.token,
    )?;
    let plan = plan_agent_ready(&lease, &options).map_err(|err| anyhow!(err))?;
    repo.install_checkout_writer_credential(&options.lease, &args.token)?;

    super::ready_cmd::cmd_ready(
        cli,
        crate::cli::cli_args::ReadyArgs {
            thread: Some(plan.thread),
            message: plan.message,
            confidence: plan.confidence,
            dry_run: crate::cli::cli_args::DryRunArgs::default(),
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
        "StatusReport": (verbs::StatusReport::CONTRACT.schema)(),
    })
}

#[cfg(test)]
mod fanout_identity_env_tests {
    use super::FANOUT_IDENTITY_ENV_PATTERNS;

    #[test]
    fn every_credential_or_identity_env_read_is_scrubbed_from_fanout() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("workspace root");
        let mut audited = std::collections::BTreeSet::new();
        for entry in walkdir::WalkDir::new(workspace.join("crates")) {
            let entry = entry.expect("workspace source entry");
            if !entry
                .path()
                .components()
                .any(|part| part.as_os_str() == "src")
                || entry
                    .path()
                    .extension()
                    .is_none_or(|extension| extension != "rs")
            {
                continue;
            }
            let source = std::fs::read_to_string(entry.path()).expect("Rust source");
            for (index, _) in source.match_indices("env::var") {
                let call = &source[index + "env::var".len()..];
                let call = call.strip_prefix("_os").unwrap_or(call);
                let Some(argument) = call.trim_start().strip_prefix('(') else {
                    continue;
                };
                let argument = argument.trim_start();
                let key = if let Some(literal) = argument.strip_prefix('"') {
                    let Some((key, _)) = literal.split_once('"') else {
                        continue;
                    };
                    key.to_string()
                } else {
                    let symbol = argument
                        .chars()
                        .take_while(|character| character.is_ascii_uppercase() || *character == '_')
                        .collect::<String>();
                    if symbol.is_empty() {
                        continue;
                    }
                    let declaration = format!("const {symbol}: &str = \"");
                    let Some((key, _)) = source
                        .split_once(&declaration)
                        .and_then(|(_, value)| value.split_once('"'))
                    else {
                        continue;
                    };
                    key.to_string()
                };
                let identity_read = matches!(key.as_str(), "HOME" | "XDG_CONFIG_HOME")
                    || key.starts_with("HEDDLE_")
                        && [
                            "CREDENTIAL",
                            "TOKEN",
                            "SECRET",
                            "IDENTITY",
                            "PRINCIPAL",
                            "AGENT_",
                            "SESSION_",
                            "HOME",
                            "CONFIG",
                            "DESCRIPTOR_KEY",
                            "DESCRIPTOR_PUBLIC_KEY",
                        ]
                        .iter()
                        .any(|part| key.contains(part));
                if !identity_read {
                    continue;
                }
                audited.insert(key.clone());
                assert!(
                    FANOUT_IDENTITY_ENV_PATTERNS.iter().any(|pattern| {
                        pattern
                            .strip_suffix('*')
                            .is_some_and(|prefix| key.starts_with(prefix))
                            || *pattern == key
                    }),
                    "{} reads {key}, but fanout does not scrub it",
                    entry.path().display()
                );
            }
        }
        assert!(
            audited.contains("HEDDLE_CREDENTIAL"),
            "hosted credential read was not scanned"
        );
        assert!(
            audited.contains("HEDDLE_PRINCIPAL_NAME"),
            "principal read was not scanned"
        );
        assert!(
            audited.contains("HOME"),
            "home based identity read was not scanned"
        );
    }
}
