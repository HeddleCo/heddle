// SPDX-License-Identifier: Apache-2.0
//! Actor management commands.
//!
//! Actors are Heddle's native record of an active harness or agent identity
//! working on a thread. They are user-facing handles over the lightweight
//! presence stored in `.heddle/actor-presence/`.
//!
//! List/show/spawn/done domain assembly and pure planning live in
//! `heddle_core::actor`. This module owns implicit session resolution,
//! thread-ref minting, harness probing, recovery advice, and human/JSON render.

use anyhow::{Result, anyhow};
use heddle_core::{
    ActorEntryReport, ActorListReport, ActorSpawnError, ActorSpawnOptions, assemble_actor_entry,
    build_spawn_entry, list_actors, mark_actor_done, plan_actor_done, plan_actor_spawn,
    show_actor_from_entry,
};
use objects::{
    object::ThreadName,
    store::{ActorPresence, ActorPresenceStore},
};
use repo::Repository;
use serde::Serialize;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    command_catalog::ActionTemplate,
    thread::{find_thread_summary, thread_name_invalid_advice},
    verification_health::{
        RepositoryVerificationState, action_template, build_repository_verification_state,
    },
};
use crate::cli::{Cli, should_output_json};

#[derive(Serialize)]
struct ActorSingleOutput {
    output_kind: &'static str,
    actor: ActorEntryReport,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct ActorListOutput {
    output_kind: &'static str,
    actors: Vec<ActorEntryReport>,
    active_only: bool,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct ActorDoneOutput {
    output_kind: &'static str,
    session_id: String,
    status: &'static str,
    thread: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    coordination_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_action_template: Option<ActionTemplate>,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct ActorExplainOutput {
    output_kind: &'static str,
    session_id: String,
    thread: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    heddle_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_actor_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_parent_actor_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_instance_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_confidence: Option<f32>,
    attach_reason: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attach_precedence: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    winning_rule: Option<String>,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct ActorExplainDetectedOutput {
    output_kind: &'static str,
    attached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_actor: Option<serde_json::Value>,
    reason: &'static str,
    repository: String,
    detected: DetectedActorOutput,
    environment: ActorEnvironmentOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_action_template: Option<ActionTemplate>,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct DetectedActorOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_actor_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_parent_actor_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_instance_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_confidence: Option<f32>,
}

#[derive(Serialize)]
struct ActorEnvironmentOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    principal_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    principal_email: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    signals: Vec<String>,
}

pub async fn cmd_actor_spawn(
    cli: &Cli,
    thread: Option<String>,
    no_thread: bool,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let repo = cli.open_repo()?;

    let base_state = repo.head()?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::repository_no_head_capture_first(
            "actor spawn"
        ))
    })?;

    // `current_lane()` is the single source of truth for "is there a lane to
    // attach to?": it consults the git-overlay HEAD state, so a detached Git
    // HEAD reports no lane even when `.heddle/HEAD` still names a stale
    // attached thread. The same predicate drives the `actor explain`
    // recommendation, so recommend and execute never disagree.
    let current_lane = repo.current_lane()?;

    let probe = crate::harness::probe_current_process_harness(
        &repo,
        provider.clone().filter(|value| !value.trim().is_empty()),
        model.clone().filter(|value| !value.trim().is_empty()),
        None,
    )?;

    let plan = plan_actor_spawn(&ActorSpawnOptions {
        thread,
        no_thread,
        current_lane,
        provider,
        model,
        probe_provider: probe.provider.clone(),
        probe_model: probe.model.clone(),
        harness: probe.harness.clone(),
        thinking_level: probe.thinking_level.clone(),
        probe_source: probe.probe_source.clone(),
        probe_confidence: probe.confidence,
        base_state_full: base_state.to_string_full(),
        base_state_short: base_state.short(),
    })
    .map_err(map_actor_spawn_error)?;

    let registry = ActorPresenceStore::new(repo.heddle_dir());
    let entry = registry.create_generated_entry(|session_id| {
        let entry = build_spawn_entry(&plan, session_id, chrono::Utc::now());
        let thread_name = ThreadName::new(entry.thread.clone());

        // In `--no-thread` mode we attach to the existing current thread
        // and never create a ref, so no stray thread is left behind.
        if plan.mint_thread_if_missing && repo.refs().get_thread(&thread_name)?.is_none() {
            repo.refs().set_thread(&thread_name, &base_state)?;
        }

        Ok(entry)
    })?;

    if should_output_json(cli, None) {
        let output = ActorSingleOutput {
            output_kind: "actor_spawn",
            actor: assemble_actor_entry(&registry, &entry)?,
            trust: build_repository_verification_state(&repo),
        };
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("Spawned actor: {}", entry.session_id);
        println!("Thread: {}", entry.thread);
        println!("Base state: {}", entry.base_state);
        if let Some(path) = &entry.path {
            println!("Path: {}", path.display());
        }
        if let Some(provider) = &entry.provider {
            println!("Provider: {}", provider);
        }
        if let Some(model) = &entry.model {
            println!("Model: {}", model);
        }
    }

    Ok(())
}

fn map_actor_spawn_error(err: ActorSpawnError) -> anyhow::Error {
    match err {
        ActorSpawnError::InvalidThreadName(err) => anyhow!(thread_name_invalid_advice(&err)),
        ActorSpawnError::NoCurrentLane => anyhow!(actor_spawn_no_thread_detached_advice()),
    }
}

pub async fn cmd_actor_list(cli: &Cli, active_only: bool) -> Result<()> {
    let repo = cli.open_repo()?;
    let report = list_actors(&repo, active_only)?;

    if should_output_json(cli, None) {
        let output = ActorListOutput {
            output_kind: report.output_kind,
            actors: report.actors,
            active_only: report.active_only,
            trust: build_repository_verification_state(&repo),
        };
        println!("{}", serde_json::to_string(&output)?);
    } else {
        render_actor_list(&report);
    }

    Ok(())
}

pub async fn cmd_actor_show(cli: &Cli, session_id: Option<String>) -> Result<()> {
    let repo = cli.open_repo()?;
    let registry = ActorPresenceStore::new(repo.heddle_dir());
    let entry = resolve_actor_entry(&repo, &registry, session_id.as_deref())?;
    let show = show_actor_from_entry(&registry, &entry)?;

    if should_output_json(cli, None) {
        let output = ActorSingleOutput {
            output_kind: "actor_show",
            actor: show.actor,
            trust: build_repository_verification_state(&repo),
        };
        println!("{}", serde_json::to_string(&output)?);
    } else {
        render_actor_show(&show.actor);
    }

    Ok(())
}

fn render_actor_list(report: &ActorListReport) {
    if report.actors.is_empty() {
        println!("No actors.");
        return;
    }
    println!("Actors:");
    for entry in &report.actors {
        println!(
            "  {} [{}] thread:{} base:{}",
            entry.session_id, entry.status, entry.thread, entry.base_state
        );
        if let Some(path) = &entry.path {
            println!("    path: {}", path);
        }
        if let Some(harness) = &entry.harness {
            println!("    harness: {}", harness);
        }
        if let Some(actor) =
            crate::cli::render::actor_display(entry.provider.as_deref(), entry.model.as_deref())
        {
            println!("    actor: {}", actor);
        }
        if let Some(thinking_level) = &entry.thinking_level {
            println!("    thinking: {}", thinking_level);
        }
        if let Some(probe_source) = &entry.probe_source {
            if let Some(confidence) = entry.probe_confidence {
                println!("    detected: {} ({:.2})", probe_source, confidence);
            } else {
                println!("    detected: {}", probe_source);
            }
        }
    }
}

fn render_actor_show(actor: &ActorEntryReport) {
    println!("Actor: {}", actor.session_id);
    println!("Thread: {}", actor.thread);
    println!("Status: {}", actor.status);
    println!("Base state: {}", actor.base_state);
    if let Some(heddle_session_id) = &actor.heddle_session_id {
        println!("Heddle session: {}", heddle_session_id);
    }
    if let Some(client_instance_id) = &actor.client_instance_id {
        println!("Client instance: {}", client_instance_id);
    }
    if let Some(native_actor_key) = &actor.native_actor_key {
        println!("Native actor: {}", native_actor_key);
    }
    if let Some(native_parent_actor_key) = &actor.native_parent_actor_key {
        println!("Native parent: {}", native_parent_actor_key);
    }
    if let Some(native_instance_key) = &actor.native_instance_key {
        println!("Native instance: {}", native_instance_key);
    }
    if let Some(path) = &actor.path {
        println!("Path: {}", path);
    }
    if let Some(last_progress_at) = &actor.last_progress_at {
        println!("Last progress: {}", last_progress_at);
    }
    if let Some(provider) = &actor.provider {
        println!("Provider: {}", provider);
    }
    if let Some(model) = &actor.model {
        println!("Model: {}", model);
    }
    if let Some(harness) = &actor.harness {
        println!("Harness: {}", harness);
    }
    if let Some(thinking_level) = &actor.thinking_level {
        println!("Thinking: {}", thinking_level);
    }
    if let Some(report_flush_state) = &actor.report_flush_state {
        println!("Report flush: {}", report_flush_state);
    }
    if let Some(attach_reason) = &actor.attach_reason {
        println!("Attach: {}", attach_reason);
    }
    print_actor_chain(&actor.actor_chain);
}

pub async fn cmd_actor_done(cli: &Cli, session_id: Option<String>) -> Result<()> {
    let repo = cli.open_repo()?;
    let registry = ActorPresenceStore::new(repo.heddle_dir());
    let entry = resolve_actor_entry(&repo, &registry, session_id.as_deref())?;
    let plan = plan_actor_done(&entry);
    mark_actor_done(&registry, &plan.session_id)?;
    let summary = find_thread_summary(&repo, &plan.thread)?;
    let recommended_action = summary.as_ref().and_then(|thread| {
        actor_done_recommended_action(&thread.name, &thread.coordination_status.to_string())
    });
    let recommended_action_template = recommended_action.as_deref().and_then(action_template);

    if should_output_json(cli, None) {
        let output = ActorDoneOutput {
            output_kind: "actor_done",
            session_id: plan.session_id.clone(),
            status: "complete",
            thread: plan.thread.clone(),
            coordination_status: summary
                .as_ref()
                .map(|thread| thread.coordination_status.to_string()),
            recommended_action,
            recommended_action_template,
            trust: build_repository_verification_state(&repo),
        };
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("Actor '{}' marked as complete.", plan.session_id);
        if let Some(thread) = summary {
            println!(
                "Thread '{}' is {}.",
                thread.name, thread.coordination_status
            );
            if let Some(action) = recommended_action {
                print_next(&action);
            }
        }
    }

    Ok(())
}

fn actor_done_recommended_action(thread: &str, coordination_status: &str) -> Option<String> {
    (coordination_status == "merge-ready")
        .then(|| super::thread_landing::land_local_command(thread))
}

pub async fn cmd_actor_explain(cli: &Cli, session_id: Option<String>) -> Result<()> {
    let repo = cli.open_repo()?;
    let registry = ActorPresenceStore::new(repo.heddle_dir());
    let entry = match resolve_actor_entry(&repo, &registry, session_id.as_deref()) {
        Ok(entry) => entry,
        Err(err) if session_id.is_none() && is_no_active_actor_error(&err) => {
            return explain_detected_actor_identity(cli, &repo);
        }
        Err(err) => return Err(err),
    };
    let reason = entry
        .attach_reason
        .clone()
        .unwrap_or_else(|| "no persisted attach reason is available for this actor".to_string());

    if should_output_json(cli, None) {
        let output = ActorExplainOutput {
            output_kind: "actor_explain",
            session_id: entry.session_id,
            thread: entry.thread,
            heddle_session_id: entry.heddle_session_id,
            client_instance_id: entry.client_instance_id,
            native_actor_key: entry.native_actor_key,
            native_parent_actor_key: entry.native_parent_actor_key,
            native_instance_key: entry.native_instance_key,
            probe_source: entry.probe_source,
            probe_confidence: entry.probe_confidence,
            attach_reason: reason,
            attach_precedence: entry.attach_precedence,
            winning_rule: entry.winning_attach_rule,
            trust: build_repository_verification_state(&repo),
        };
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("Actor: {}", entry.session_id);
        println!("Thread: {}", entry.thread);
        if let Some(heddle_session_id) = &entry.heddle_session_id {
            println!("Heddle session: {}", heddle_session_id);
        }
        if let Some(client_instance_id) = &entry.client_instance_id {
            println!("Client instance: {}", client_instance_id);
        }
        if let Some(native_actor_key) = &entry.native_actor_key {
            println!("Native actor: {}", native_actor_key);
        }
        if let Some(native_parent_actor_key) = &entry.native_parent_actor_key {
            println!("Native parent: {}", native_parent_actor_key);
        }
        if let Some(native_instance_key) = &entry.native_instance_key {
            println!("Native instance: {}", native_instance_key);
        }
        if let Some(probe_source) = &entry.probe_source {
            println!("Probe source: {}", probe_source);
        }
        if let Some(probe_confidence) = entry.probe_confidence {
            println!("Probe confidence: {:.2}", probe_confidence);
        }
        println!("Why attached: {}", reason);
        if let Some(winning_rule) = &entry.winning_attach_rule {
            println!("Winning rule: {}", winning_rule);
        }
        if !entry.attach_precedence.is_empty() {
            println!("Attach precedence:");
            for rule in &entry.attach_precedence {
                println!("  {}", rule);
            }
        }
    }

    Ok(())
}

fn explain_detected_actor_identity(cli: &Cli, repo: &Repository) -> Result<()> {
    let probe = crate::harness::probe_current_process_harness(
        repo,
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
    let env_signals = actor_identity_env_signals();
    // Route through the same "is there a current lane?" predicate that
    // `actor spawn --no-thread` uses, so the recommendation is always runnable
    // in this context. `current_lane()` is git-overlay-aware: a detached Git
    // HEAD reports no lane even when `.heddle/HEAD` still names a stale thread.
    let no_current_lane = repo.current_lane()?.is_none();
    let next_action = detected_actor_next_action(
        probe.provider.as_deref(),
        probe.model.as_deref(),
        no_current_lane,
    );
    let next_action_template = next_action.as_deref().and_then(action_template);

    if should_output_json(cli, None) {
        let output = ActorExplainDetectedOutput {
            output_kind: "actor_explain",
            attached: false,
            active_actor: None,
            reason: "No active actor is registered for this checkout.",
            repository: repo.root().display().to_string(),
            detected: DetectedActorOutput {
                harness: probe.harness,
                provider: probe.provider,
                model: probe.model,
                thinking_level: probe.thinking_level,
                policy: probe.policy,
                native_actor_key: probe.native_actor_key,
                native_parent_actor_key: probe.native_parent_actor_key,
                native_instance_key: probe.native_instance_key,
                probe_source: probe.probe_source,
                probe_confidence: probe.confidence,
            },
            environment: ActorEnvironmentOutput {
                agent_provider: std::env::var("HEDDLE_AGENT_PROVIDER").ok(),
                agent_model: std::env::var("HEDDLE_AGENT_MODEL").ok(),
                agent_policy: std::env::var("HEDDLE_AGENT_POLICY").ok(),
                principal_name: std::env::var("HEDDLE_PRINCIPAL_NAME").ok(),
                principal_email: std::env::var("HEDDLE_PRINCIPAL_EMAIL").ok(),
                signals: env_signals,
            },
            recommended_action: next_action,
            recommended_action_template: next_action_template,
            trust: build_repository_verification_state(repo),
        };
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("Actor: none attached");
        println!("Repository: {}", repo.root().display());
        println!("Why: no active actor is registered for this checkout.");
        if let Some(harness) = &probe.harness {
            println!("Detected harness: {}", harness);
        }
        if let Some(provider) = &probe.provider {
            println!("Provider: {}", provider);
        }
        if let Some(model) = &probe.model {
            println!("Model: {}", model);
        }
        if let Some(thinking_level) = &probe.thinking_level {
            println!("Thinking: {}", thinking_level);
        }
        if let Some(probe_source) = &probe.probe_source {
            println!("Probe source: {}", probe_source);
        }
        if let Some(confidence) = probe.confidence {
            println!("Probe confidence: {:.2}", confidence);
        }
        if !env_signals.is_empty() {
            println!("Environment signals: {}", env_signals.join(", "));
        }
        if let Some(action) = next_action {
            print_next(&action);
        } else {
            print_next("heddle actor spawn");
        }
    }

    Ok(())
}

fn actor_identity_env_signals() -> Vec<String> {
    let mut signals = std::env::vars()
        .filter_map(|(key, value)| {
            if value.trim().is_empty() {
                return None;
            }
            let is_signal = key.starts_with("HEDDLE_AGENT_")
                || key.starts_with("CODEX_")
                || key.starts_with("CLAUDE")
                || key.starts_with("ANTHROPIC_")
                || key.starts_with("OPENAI_")
                || key.starts_with("OPENCODE_")
                || key.starts_with("AIDER_")
                || matches!(
                    key.as_str(),
                    "MODEL" | "REASONING_EFFORT" | "THINKING_LEVEL"
                );
            is_signal.then_some(key)
        })
        .collect::<Vec<_>>();
    signals.sort();
    signals
}

fn detected_actor_next_action(
    provider: Option<&str>,
    model: Option<&str>,
    no_current_lane: bool,
) -> Option<String> {
    match (provider, model) {
        // The recommendation must be runnable as-is from the current context.
        // With no current lane (e.g. a detached HEAD) there is no thread to
        // attach to, so `--no-thread` would fail
        // (`actor_spawn_no_thread_detached`); mint a dedicated thread instead.
        // On a lane, `--no-thread` attaches the detected identity to the current
        // thread without leaving a stray `actor/<session>`.
        (Some(provider), Some(model)) if no_current_lane => Some(format!(
            "heddle actor spawn --provider {provider} --model {model}"
        )),
        (Some(provider), Some(model)) => Some(format!(
            "heddle actor spawn --no-thread --provider {provider} --model {model}"
        )),
        _ => None,
    }
}

fn resolve_actor_entry(
    repo: &Repository,
    registry: &ActorPresenceStore,
    session_id: Option<&str>,
) -> Result<ActorPresence> {
    if let Some(session_id) = session_id {
        return registry
            .load(session_id)?
            .ok_or_else(|| anyhow!("Actor not found for session: {}", session_id));
    }

    // Single git-overlay-aware oracle for "what lane is THIS checkout on?".
    // `current_lane()` consults the git-overlay HEAD state, so a detached Git
    // HEAD reports no lane even when `.heddle/HEAD` still names a stale attached
    // thread. Deriving the implicit actor lookup from it — instead of
    // `head_ref()` / `.heddle/HEAD` directly — keeps `actor explain`/`show`/`done`
    // in agreement with `actor spawn --no-thread`, which rejects on the same
    // no-lane predicate. There must be exactly one answer to "current lane?".
    let current_lane = repo.current_lane()?;

    if let Some(thread) = current_lane.as_deref()
        && let Some(entry) = registry
            .active_entries()?
            .into_iter()
            .filter(|entry| entry.thread == thread)
            .max_by_key(|entry| entry.started_at)
    {
        return Ok(entry);
    }

    if let Some(entry) = registry.find_active_by_path(repo.root())? {
        return Ok(entry);
    }

    // The "any active actor" fallback only applies when this checkout is on a
    // lane. With no current lane (a detached Git HEAD whose `.heddle/HEAD` is
    // stale, or a genuinely detached HEAD), resolving an arbitrary active actor
    // would contradict `actor spawn --no-thread`'s rejection and re-introduce
    // the recommend/execute split this oracle exists to prevent.
    if current_lane.is_some()
        && let Some(entry) = registry.active_entries()?.into_iter().next()
    {
        return Ok(entry);
    }

    Err(anyhow!(no_active_actor_advice()))
}

fn is_no_active_actor_error(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<RecoveryAdvice>())
        .any(|advice| advice.kind == "no_active_actor")
}

fn actor_spawn_no_thread_detached_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "actor_spawn_no_thread_detached",
        "Cannot attach to the current thread",
        "`--no-thread` attaches the actor to the thread HEAD points at. Check out a thread first (`heddle thread switch <name>`), or run `heddle actor spawn` without `--no-thread` to mint a dedicated thread.",
        "HEAD is not attached to a thread, so there is no current thread to attach the actor to",
        "minting a thread implicitly would create exactly the stray thread `--no-thread` is meant to avoid",
        "no actor registry entries, refs, repository objects, or worktree files were changed",
        "heddle thread switch <name>",
        vec![
            "heddle thread list".to_string(),
            "heddle thread switch <name>".to_string(),
            "heddle actor spawn".to_string(),
        ],
    )
}

fn no_active_actor_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "no_active_actor",
        "No active actor for this checkout",
        "After a thread is landed or an actor is marked done, it is no longer selected implicitly. Run `heddle actor list` to inspect completed actors, or pass a session id to `heddle actor show <session>`.",
        "no active actor registry entry matches the current thread or checkout path",
        "choosing a completed actor implicitly could show the wrong session",
        "no actor registry entries, refs, repository objects, or worktree files were changed",
        "heddle actor list",
        vec![
            "heddle actor list".to_string(),
            "heddle actor explain".to_string(),
            "heddle actor show <session>".to_string(),
        ],
    )
}

fn print_actor_chain(chain: &[heddle_core::ActorChainEntry]) {
    if chain.len() <= 1 {
        return;
    }
    println!("Actor chain:");
    for (idx, node) in chain.iter().enumerate() {
        let label = node
            .native_actor_key
            .as_deref()
            .unwrap_or(node.session_id.as_str());
        let parent = node
            .native_parent_actor_key
            .as_deref()
            .map(|key| format!(" parent:{key}"))
            .unwrap_or_default();
        let arrow = if idx == 0 { "  " } else { "→ " };
        println!(
            "{}{} [{}] thread:{}{}",
            arrow, label, node.status, node.thread, parent
        );
    }
}
