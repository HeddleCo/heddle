// SPDX-License-Identifier: Apache-2.0
//! Provider, model, and policy provenance commands.

use anyhow::Result;
use repo::SessionManager;
use verbs::session_list_status;

use super::{
    advice::RecoveryAdvice,
    next_action::{NextActionValidationContext, write_full_command_json},
    verification_health::build_repository_verification_state,
};
use crate::cli::{Cli, should_output_json};

// The provenance wire payloads live in cli-contract so the schema registry
// registers the real serialization types.
pub(crate) use heddle_cli_contract::cli::commands::wire::agent::{
    SegmentEnvelope, SegmentOutput, SessionEnvelope, SessionListOutput, SessionOutput,
};

pub async fn begin(
    cli: &Cli,
    provider: String,
    model: String,
    policy: Option<String>,
) -> Result<()> {
    let repo = cli.open_repo()?;
    let mut manager = SessionManager::new(repo.root());
    let principal = repo.get_principal()?;

    let session = manager.start_session(principal, provider, model, policy)?;

    if should_output_json(cli, None) {
        let output = SessionEnvelope {
            session: SessionOutput::from(&session),
            trust: build_repository_verification_state(&repo),
        };
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["agent", "provenance", "begin"]),
        )?;
    } else {
        println!("Session: {}", session.id);
        let segment = session.current_segment().unwrap();
        println!("Segment: {}", segment.id);
    }

    Ok(())
}

pub async fn segment(
    cli: &Cli,
    provider: String,
    model: String,
    policy: Option<String>,
) -> Result<()> {
    let repo = cli.open_repo()?;
    let mut manager = SessionManager::new(repo.root());

    let current_id = manager.get_current_session_id()?.ok_or_else(|| {
        anyhow::anyhow!(RecoveryAdvice::no_current_session(
            "agent provenance segment",
            None,
            "heddle agent provenance begin",
        ))
    })?;

    let segment = manager.add_segment(&current_id, provider, model, policy)?;

    if should_output_json(cli, None) {
        let output = SegmentEnvelope {
            segment: SegmentOutput::from(&segment),
            trust: build_repository_verification_state(&repo),
        };
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["agent", "provenance", "end"]),
        )?;
    } else {
        println!("Segment: {}", segment.id);
    }

    Ok(())
}

pub async fn end(cli: &Cli, session_id: Option<String>) -> Result<()> {
    let repo = cli.open_repo()?;
    let mut manager = SessionManager::new(repo.root());

    let session = manager.end_session(session_id.as_deref())?;

    if should_output_json(cli, None) {
        let output = SessionEnvelope {
            session: SessionOutput::from(&session),
            trust: build_repository_verification_state(&repo),
        };
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["agent", "provenance", "segment"]),
        )?;
    } else {
        println!("Session ended: {}", session.id);
    }

    Ok(())
}

pub async fn show(cli: &Cli, session_id: Option<String>) -> Result<()> {
    let repo = cli.open_repo()?;
    let manager = SessionManager::new(repo.root());

    let id = match session_id {
        Some(id) => id,
        None => manager.get_current_session_id()?.ok_or_else(|| {
            anyhow::anyhow!(RecoveryAdvice::no_current_session(
                "agent provenance show",
                Some("<SESSION_ID>"),
                "heddle agent provenance begin",
            ))
        })?,
    };

    let session = manager
        .get_session(&id)?
        .ok_or_else(|| anyhow::anyhow!("Session not found: {}", id))?;

    if should_output_json(cli, None) {
        let output = SessionEnvelope {
            session: SessionOutput::from(&session),
            trust: build_repository_verification_state(&repo),
        };
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["agent", "provenance", "show"]),
        )?;
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

pub async fn list(cli: &Cli, active_only: bool) -> Result<()> {
    let repo = cli.open_repo()?;
    let manager = SessionManager::new(repo.root());

    let sessions = manager.list_sessions(active_only)?;

    if should_output_json(cli, None) {
        let output = SessionListOutput {
            sessions: sessions.iter().map(SessionOutput::from).collect(),
            active_only,
            trust: build_repository_verification_state(&repo),
        };
        write_full_command_json(
            &output,
            NextActionValidationContext::without_repo(&["agent", "provenance", "list"]),
        )?;
    } else {
        if sessions.is_empty() {
            println!("No sessions found.");
            return Ok(());
        }

        println!("Sessions:");
        for session in sessions {
            println!(
                "  {} [{}] - {} segments - {}",
                session.id,
                session_list_status(session.is_active()),
                session.segments.len(),
                session.created_at.format("%Y-%m-%d %H:%M")
            );
        }
    }

    Ok(())
}
