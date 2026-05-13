// SPDX-License-Identifier: Apache-2.0
//! Actor management commands.
//!
//! Actors are Heddle's native record of an active harness or agent identity
//! working on a thread. They are user-facing handles over the lightweight
//! registry stored in `.heddle/agents/`.

use anyhow::{Result, anyhow};
use chrono::Utc;
use objects::store::{ActorChainNode, AgentEntry, AgentRegistry, AgentStatus, AgentUsageSummary};
use refs::Head;
use repo::Repository;
use serde::Serialize;

use super::thread::find_thread_summary;
use crate::cli::{Cli, should_output_json};

#[derive(Serialize)]
struct ActorOutput {
    session_id: String,
    client_instance_id: Option<String>,
    native_actor_key: Option<String>,
    native_parent_actor_key: Option<String>,
    native_instance_key: Option<String>,
    heddle_session_id: Option<String>,
    thread: String,
    thread_id: Option<String>,
    base_state: String,
    path: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    harness: Option<String>,
    thinking_level: Option<String>,
    usage_summary: AgentUsageSummary,
    last_progress_at: Option<String>,
    report_flush_state: Option<String>,
    attach_reason: Option<String>,
    attach_precedence: Vec<String>,
    winning_attach_rule: Option<String>,
    probe_source: Option<String>,
    probe_confidence: Option<f32>,
    status: String,
    started_at: String,
    actor_chain: Vec<ActorChainOutput>,
}

#[derive(Serialize)]
struct ActorChainOutput {
    session_id: String,
    native_actor_key: Option<String>,
    native_parent_actor_key: Option<String>,
    thread: String,
    status: String,
    provider: Option<String>,
    model: Option<String>,
    harness: Option<String>,
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
    provider: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    let base_state = repo
        .head()?
        .ok_or_else(|| anyhow!("Repository has no HEAD state - take a snapshot first"))?;

    let registry = AgentRegistry::new(repo.heddle_dir());
    let entry = registry.create_generated_entry(|session_id| {
        let thread_name = thread
            .clone()
            .unwrap_or_else(|| format!("actor/{session_id}"));

        if repo.refs().get_thread(&thread_name)?.is_none() {
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
            thread: thread_name.clone(),
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
            provider: provider.clone(),
            model: model.clone(),
            harness: None,
            thinking_level: None,
            usage_summary: AgentUsageSummary::default(),
            last_progress_at: None,
            report_flush_state: None,
            attach_reason: Some(format!(
                "actor {session_id} was spawned explicitly on thread {thread_name}"
            )),
            attach_precedence: vec!["explicit-actor-spawn".to_string()],
            winning_attach_rule: Some("explicit-actor-spawn".to_string()),
            probe_source: Some("explicit_payload".to_string()),
            probe_confidence: Some(1.0),
            status: AgentStatus::Active,
            completed_at: None,
            context_queries: vec![],
        })
    })?;

    if should_output_json(cli, None) {
        let chain = registry.actor_chain_for_session(&entry.session_id)?;
        println!(
            "{}",
            serde_json::to_string(&ActorOutput::from(&entry).with_chain(chain))?
        );
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
        let output: Vec<ActorOutput> = entries.iter().map(ActorOutput::from).collect();
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
        }
    }

    Ok(())
}

pub async fn cmd_actor_show(cli: &Cli, session_id: Option<String>) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let registry = AgentRegistry::new(repo.heddle_dir());
    let entry = resolve_actor_entry(&repo, &registry, session_id.as_deref())?;

    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(&ActorOutput::from(&entry))?);
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

    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::json!({
                "session_id": entry.session_id,
                "status": "complete",
                "thread": entry.thread,
                "coordination_status": summary.as_ref().map(|thread| thread.coordination_status.to_string()),
            })
        );
    } else {
        println!("Actor '{}' marked as complete.", entry.session_id);
        if let Some(thread) = summary {
            println!(
                "Thread '{}' is {}.",
                thread.name, thread.coordination_status
            );
            if thread.coordination_status.to_string() == "merge-ready" {
                println!("Next: heddle merge {}", thread.name);
            }
        }
    }

    Ok(())
}

pub async fn cmd_actor_explain(cli: &Cli, session_id: Option<String>) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let registry = AgentRegistry::new(repo.heddle_dir());
    let entry = resolve_actor_entry(&repo, &registry, session_id.as_deref())?;
    let reason = entry
        .attach_reason
        .clone()
        .unwrap_or_else(|| "no persisted attach reason is available for this actor".to_string());

    if should_output_json(cli, None) {
        println!(
            "{}",
            serde_json::json!({
                "session_id": entry.session_id,
                "thread": entry.thread,
                "heddle_session_id": entry.heddle_session_id,
                "client_instance_id": entry.client_instance_id,
                "native_actor_key": entry.native_actor_key,
                "native_parent_actor_key": entry.native_parent_actor_key,
                "native_instance_key": entry.native_instance_key,
                "probe_source": entry.probe_source,
                "probe_confidence": entry.probe_confidence,
                "attach_reason": reason,
                "attach_precedence": entry.attach_precedence,
                "winning_rule": entry.winning_attach_rule,
            })
        );
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
            .filter(|entry| entry.status == AgentStatus::Active && entry.thread == thread)
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
        .ok_or_else(|| anyhow!("No active actor found"))
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