// SPDX-License-Identifier: Apache-2.0
//! Session management commands.

use anyhow::Result;
use objects::object::{Session, SessionSegment};
use repo::{Repository, SessionManager};
use serde::Serialize;

use crate::cli::{Cli, should_output_json};

#[derive(Serialize)]
struct SessionOutput {
    id: String,
    principal: String,
    created_at: String,
    ended_at: Option<String>,
    active: bool,
    segments: Vec<SegmentOutput>,
}

#[derive(Serialize)]
struct SegmentOutput {
    id: String,
    provider: String,
    model: String,
    started_at: String,
    policy_id: Option<String>,
}

impl From<&Session> for SessionOutput {
    fn from(session: &Session) -> Self {
        Self {
            id: session.id.clone(),
            principal: session.principal.to_string(),
            created_at: session.created_at.to_rfc3339(),
            ended_at: session.ended_at.map(|t| t.to_rfc3339()),
            active: session.is_active(),
            segments: session.segments.iter().map(SegmentOutput::from).collect(),
        }
    }
}

impl From<&SessionSegment> for SegmentOutput {
    fn from(segment: &SessionSegment) -> Self {
        Self {
            id: segment.id.clone(),
            provider: segment.provider.clone(),
            model: segment.model.clone(),
            started_at: segment.started_at.to_rfc3339(),
            policy_id: segment.policy_id.clone(),
        }
    }
}

pub async fn cmd_session_start(
    cli: &Cli,
    provider: String,
    model: String,
    policy: Option<String>,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let mut manager = SessionManager::new(repo.root());
    let principal = repo.get_principal()?;

    let session = manager.start_session(principal, provider, model, policy)?;

    if should_output_json(cli, None) {
        let output = SessionOutput::from(&session);
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("Session: {}", session.id);
        let segment = session.current_segment().unwrap();
        println!("Segment: {}", segment.id);
    }

    Ok(())
}

pub async fn cmd_session_segment(
    cli: &Cli,
    provider: String,
    model: String,
    policy: Option<String>,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let mut manager = SessionManager::new(repo.root());

    let current_id = manager.get_current_session_id()?.ok_or_else(|| {
        anyhow::anyhow!("No active session. Start one with `heddle session start`")
    })?;

    let segment = manager.add_segment(&current_id, provider, model, policy)?;

    if should_output_json(cli, None) {
        let output = SegmentOutput::from(&segment);
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("Segment: {}", segment.id);
    }

    Ok(())
}

pub async fn cmd_session_end(cli: &Cli, session_id: Option<String>) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let mut manager = SessionManager::new(repo.root());

    let session = manager.end_session(session_id.as_deref())?;

    if should_output_json(cli, None) {
        let output = SessionOutput::from(&session);
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("Session ended: {}", session.id);
    }

    Ok(())
}

pub async fn cmd_session_show(cli: &Cli, session_id: Option<String>) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let manager = SessionManager::new(repo.root());

    let id = match session_id {
        Some(id) => id,
        None => manager
            .get_current_session_id()?
            .ok_or_else(|| anyhow::anyhow!("No active session"))?,
    };

    let session = manager
        .get_session(&id)?
        .ok_or_else(|| anyhow::anyhow!("Session not found: {}", id))?;

    if should_output_json(cli, None) {
        let output = SessionOutput::from(&session);
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("Session: {}", session.id);
        println!("Principal: {}", session.principal);
        println!(
            "Created: {}",
            session.created_at.format("%Y-%m-%d %H:%M:%S UTC")
        );
        if let Some(ended) = session.ended_at {
            println!("Ended: {}", ended.format("%Y-%m-%d %H:%M:%S UTC"));
        }
        println!(
            "Status: {}",
            if session.is_active() {
                "active"
            } else {
                "ended"
            }
        );
        println!();
        println!("Segments:");
        for (i, seg) in session.segments.iter().enumerate() {
            println!("  {}. {} ({}/{})", i + 1, seg.id, seg.provider, seg.model);
            if let Some(ref policy) = seg.policy_id {
                println!("     Policy: {}", policy);
            }
        }
    }

    Ok(())
}

pub async fn cmd_session_list(cli: &Cli, active_only: bool) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let manager = SessionManager::new(repo.root());

    let sessions = manager.list_sessions(active_only)?;

    if should_output_json(cli, None) {
        let output: Vec<SessionOutput> = sessions.iter().map(SessionOutput::from).collect();
        println!("{}", serde_json::to_string(&output)?);
    } else {
        if sessions.is_empty() {
            println!("No sessions found.");
            return Ok(());
        }

        println!("Sessions:");
        for session in sessions {
            let status = if session.is_active() {
                "active"
            } else {
                "ended"
            };
            println!(
                "  {} [{}] - {} segments - {}",
                session.id,
                status,
                session.segments.len(),
                session.created_at.format("%Y-%m-%d %H:%M")
            );
        }
    }

    Ok(())
}
