// SPDX-License-Identifier: Apache-2.0
//! Actor management commands.
//!
//! Actors are Heddle's native record of an active harness or agent identity
//! working on a thread. They are user-facing handles over the lightweight
//! registry stored in `.heddle/agents/`.

use anyhow::{Result, anyhow};
use chrono::Utc;
use objects::object::ThreadName;
use objects::store::{ActorChainNode, AgentEntry, AgentRegistry, AgentStatus, AgentUsageSummary};
use refs::Head;
use repo::Repository;
use serde::Serialize;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    command_catalog::ActionTemplate,
    git_overlay_health::{
        RepositoryVerificationState, action_template, build_repository_verification_state,
    },
    thread::find_thread_summary,
};
use crate::cli::{Cli, should_output_json};

#[derive(Serialize)]
struct ActorOutput {
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_actor_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_parent_actor_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_instance_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    heddle_session_id: Option<String>,
    thread: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_id: Option<String>,
    base_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_level: Option<String>,
    usage_summary: AgentUsageSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_progress_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    report_flush_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attach_reason: Option<String>,
    attach_precedence: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    winning_attach_rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_confidence: Option<f32>,
    status: String,
    started_at: String,
    actor_chain: Vec<ActorChainOutput>,
}

#[derive(Serialize)]
struct ActorChainOutput {
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_actor_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_parent_actor_key: Option<String>,
    thread: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    harness: Option<String>,
}

#[derive(Serialize)]
struct ActorSingleOutput {
    actor: ActorOutput,
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct ActorListOutput {
    actors: Vec<ActorOutput>,
    active_only: bool,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct ActorDoneOutput {
    session_id: String,
    status: &'static str,
    thread: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    coordination_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_action_template: Option<ActionTemplate>,
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

#[derive(Serialize)]
struct ActorExplainOutput {
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

impl From<ActorChainNode> for ActorChainOutput {
    fn from(node: ActorChainNode) -> Self {
        Self {
            session_id: node.session_id,
            native_actor_key: node.native_actor_key,
            native_parent_actor_key: node.native_parent_actor_key,
            thread: node.thread,
            status: node.status.to_string(),
            provider: node.provider,
            model: node.model,
            harness: node.harness,
        }
    }
}

impl From<&AgentEntry> for ActorOutput {
    fn from(entry: &AgentEntry) -> Self {
        Self {
            session_id: entry.session_id.clone(),
            client_instance_id: entry.client_instance_id.clone(),
            native_actor_key: entry.native_actor_key.clone(),
            native_parent_actor_key: entry.native_parent_actor_key.clone(),
            native_instance_key: entry.native_instance_key.clone(),
            heddle_session_id: entry.heddle_session_id.clone(),
            thread: entry.thread.clone(),
            thread_id: entry.thread_id.clone(),
            base_state: entry.base_state.clone(),
            path: entry.path.as_ref().map(|path| path.display().to_string()),
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            harness: entry.harness.clone(),
            thinking_level: entry.thinking_level.clone(),
            usage_summary: entry.usage_summary.clone(),
            last_progress_at: entry.last_progress_at.map(|ts| ts.to_rfc3339()),
            report_flush_state: entry.report_flush_state.clone(),
            attach_reason: entry.attach_reason.clone(),
            attach_precedence: entry.attach_precedence.clone(),
            winning_attach_rule: entry.winning_attach_rule.clone(),
            probe_source: entry.probe_source.clone(),
            probe_confidence: entry.probe_confidence,
            status: entry.status.to_string(),
            started_at: entry.started_at.to_rfc3339(),
            actor_chain: vec![],
        }
    }
}

impl ActorOutput {
    fn with_chain(mut self, chain: Vec<ActorChainNode>) -> Self {
        self.actor_chain = chain.into_iter().map(ActorChainOutput::from).collect();
        self
    }
}

pub async fn cmd_actor_spawn(
    cli: &Cli,
    thread: Option<String>,
    no_thread: bool,
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    let base_state = repo.head()?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::repository_no_head_capture_first(
            "actor spawn"
        ))
    })?;

    // `--no-thread` attaches the actor to the current thread rather than
    // minting a fresh `actor/<session>` thread. Resolve it up front so a
    // detached HEAD fails cleanly before we create any registry entry.
    let attach_thread = if no_thread {
        match repo.head_ref()? {
            Head::Attached { thread } => Some(thread),
            _ => return Err(anyhow!(actor_spawn_no_thread_detached_advice())),
        }
    } else {
        None
    };

    let explicit_identity = provider
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty())
        || model.as_ref().is_some_and(|value| !value.trim().is_empty());
    let probe = crate::harness::probe_current_process_harness(
        &repo,
        provider.clone().filter(|value| !value.trim().is_empty()),
        model.clone().filter(|value| !value.trim().is_empty()),
        None,
    )?;
    let resolved_provider = provider
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| probe.provider.clone());
    let resolved_model = model
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| probe.model.clone());
    let probe_source = if explicit_identity {
        Some("explicit_payload".to_string())
    } else {
        probe.probe_source.clone()
    };
    let probe_confidence = if explicit_identity {
        Some(1.0)
    } else {
        probe.confidence
    };

    let registry = AgentRegistry::new(repo.heddle_dir());
    let entry = registry.create_generated_entry(|session_id| {
        let thread_name = match &attach_thread {
            Some(current) => current.clone(),
            None => ThreadName::new(
                thread
                    .clone()
                    .unwrap_or_else(|| format!("actor/{session_id}")),
            ),
        };

        // In `--no-thread` mode we attach to the existing current thread
        // and never create a ref, so no stray thread is left behind.
        if attach_thread.is_none() && repo.refs().get_thread(&thread_name)?.is_none() {
            repo.refs().set_thread(&thread_name, &base_state)?;
        }

        Ok(AgentEntry {
            session_id: session_id.to_string(),
            client_instance_id: None,
            native_actor_key: None,
            native_parent_actor_key: None,
            native_instance_key: None,
            heddle_session_id: None,
            thread_id: None,
            thread: thread_name.to_string(),
            pid: Some(std::process::id()),
            boot_id: None,
            liveness_path: None,
            heartbeat_at: Some(Utc::now()),
            anchor_state: Some(base_state.to_string_full()),
            anchor_root: None,
            reservation_token: Some(objects::store::generate_agent_id()),
            path: None,
            base_state: base_state.short(),
            started_at: Utc::now(),
            provider: resolved_provider.clone(),
            model: resolved_model.clone(),
            harness: probe.harness.clone(),
            thinking_level: probe.thinking_level.clone(),
            usage_summary: AgentUsageSummary::default(),
            last_progress_at: None,
            report_flush_state: None,
            attach_reason: Some(if attach_thread.is_some() {
                format!(
                    "actor {session_id} was attached to the current thread {thread_name} without minting a new thread"
                )
            } else {
                format!("actor {session_id} was spawned explicitly on thread {thread_name}")
            }),
            attach_precedence: vec![
                if attach_thread.is_some() {
                    "no-thread-attach".to_string()
                } else {
                    "explicit-actor-spawn".to_string()
                },
            ],
            winning_attach_rule: Some(if attach_thread.is_some() {
                "no-thread-attach".to_string()
            } else {
                "explicit-actor-spawn".to_string()
            }),
            probe_source: probe_source.clone(),
            probe_confidence,
            status: AgentStatus::Active,
            completed_at: None,
            context_queries: vec![],
        })
    })?;

    if should_output_json(cli, None) {
        let chain = registry.actor_chain_for_session(&entry.session_id)?;
        let output = ActorSingleOutput {
            actor: ActorOutput::from(&entry).with_chain(chain),
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

pub async fn cmd_actor_list(cli: &Cli, active_only: bool) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let registry = AgentRegistry::new(repo.heddle_dir());

    let mut entries = registry.list()?;
    if active_only {
        entries.retain(|entry| entry.status == AgentStatus::Active);
    }

    if should_output_json(cli, None) {
        let output = ActorListOutput {
            actors: entries.iter().map(ActorOutput::from).collect(),
            active_only,
            trust: build_repository_verification_state(&repo),
        };
        println!("{}", serde_json::to_string(&output)?);
    } else if entries.is_empty() {
        println!("No actors.");
    } else {
        println!("Actors:");
        for entry in &entries {
            println!(
                "  {} [{}] thread:{} base:{}",
                entry.session_id, entry.status, entry.thread, entry.base_state
            );
            if let Some(path) = &entry.path {
                println!("    path: {}", path.display());
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

    Ok(())
}

pub async fn cmd_actor_show(cli: &Cli, session_id: Option<String>) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let registry = AgentRegistry::new(repo.heddle_dir());
    let entry = resolve_actor_entry(&repo, &registry, session_id.as_deref())?;

    if should_output_json(cli, None) {
        let chain = registry.actor_chain_for_session(&entry.session_id)?;
        let output = ActorSingleOutput {
            actor: ActorOutput::from(&entry).with_chain(chain),
            trust: build_repository_verification_state(&repo),
        };
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("Actor: {}", entry.session_id);
        println!("Thread: {}", entry.thread);
        println!("Status: {}", entry.status);
        println!("Base state: {}", entry.base_state);
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
        if let Some(path) = &entry.path {
            println!("Path: {}", path.display());
        }
        if let Some(provider) = &entry.provider {
            println!("Provider: {}", provider);
        }
        if let Some(model) = &entry.model {
            println!("Model: {}", model);
        }
        if let Some(harness) = &entry.harness {
            println!("Harness: {}", harness);
        }
        if let Some(thinking_level) = &entry.thinking_level {
            println!("Thinking: {}", thinking_level);
        }
        if let Some(report_flush_state) = &entry.report_flush_state {
            println!("Report flush: {}", report_flush_state);
        }
        if let Some(attach_reason) = &entry.attach_reason {
            println!("Attach: {}", attach_reason);
        }
        print_actor_chain(&registry, &entry)?;
    }

    Ok(())
}

pub async fn cmd_actor_done(cli: &Cli, session_id: Option<String>) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let registry = AgentRegistry::new(repo.heddle_dir());
    let entry = resolve_actor_entry(&repo, &registry, session_id.as_deref())?;
    registry.update_status(&entry.session_id, AgentStatus::Complete)?;
    let summary = find_thread_summary(&repo, &entry.thread)?;
    let recommended_action = summary.as_ref().and_then(|thread| {
        actor_done_recommended_action(&thread.name, &thread.coordination_status.to_string())
    });
    let recommended_action_template = recommended_action.as_deref().and_then(action_template);

    if should_output_json(cli, None) {
        let output = ActorDoneOutput {
            session_id: entry.session_id,
            status: "complete",
            thread: entry.thread,
            coordination_status: summary
                .as_ref()
                .map(|thread| thread.coordination_status.to_string()),
            recommended_action,
            recommended_action_template,
            trust: build_repository_verification_state(&repo),
        };
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("Actor '{}' marked as complete.", entry.session_id);
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
        .then(|| super::thread_landing::merge_preview_command(thread))
}

pub async fn cmd_actor_explain(cli: &Cli, session_id: Option<String>) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let registry = AgentRegistry::new(repo.heddle_dir());
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
        std::env::var("HEDDLE_AGENT_PROVIDER").ok(),
        std::env::var("HEDDLE_AGENT_MODEL").ok(),
        std::env::var("HEDDLE_AGENT_POLICY").ok(),
    )?;
    let env_signals = actor_identity_env_signals();
    let head_detached = matches!(repo.head_ref()?, Head::Detached { .. });
    let next_action = detected_actor_next_action(
        probe.provider.as_deref(),
        probe.model.as_deref(),
        head_detached,
    );
    let next_action_template = next_action.as_deref().and_then(action_template);

    if should_output_json(cli, None) {
        let output = ActorExplainDetectedOutput {
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
    head_detached: bool,
) -> Option<String> {
    match (provider, model) {
        // The recommendation must be runnable as-is from the current context.
        // On a detached HEAD there is no current thread to attach to, so
        // `--no-thread` would fail (`actor_spawn_no_thread_detached`); mint a
        // dedicated thread instead. On a thread, `--no-thread` attaches the
        // detected identity to the current thread without leaving a stray
        // `actor/<session>`.
        (Some(provider), Some(model)) if head_detached => Some(format!(
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
    registry: &AgentRegistry,
    session_id: Option<&str>,
) -> Result<AgentEntry> {
    if let Some(session_id) = session_id {
        return registry
            .load(session_id)?
            .ok_or_else(|| anyhow!("Actor not found for session: {}", session_id));
    }

    if let Head::Attached { thread } = repo.head_ref()?
        && let Some(entry) = registry
            .list()?
            .into_iter()
            .filter(|entry| entry.status == AgentStatus::Active && thread == entry.thread)
            .max_by_key(|entry| entry.started_at)
    {
        return Ok(entry);
    }

    if let Some(entry) = registry.find_active_by_path(repo.root())? {
        return Ok(entry);
    }

    registry
        .list()?
        .into_iter()
        .find(|entry| entry.status == AgentStatus::Active)
        .ok_or_else(|| anyhow!(no_active_actor_advice()))
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
        "After a thread is shipped or an actor is marked done, it is no longer selected implicitly. Run `heddle actor list` to inspect completed actors, or pass a session id to `heddle actor show <session>`.",
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

fn print_actor_chain(registry: &AgentRegistry, entry: &AgentEntry) -> Result<()> {
    let chain = registry.actor_chain_for_session(&entry.session_id)?;
    if chain.len() <= 1 {
        return Ok(());
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
    Ok(())
}
