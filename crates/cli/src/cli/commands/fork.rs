// SPDX-License-Identifier: Apache-2.0
//! Fork command: create exploration branch.

use anyhow::Result;
use objects::object::State;
use refs::{Head, RefExpectation, RefUpdate};
use repo::Repository;
use serde::Serialize;

use super::snapshot::{ensure_current_state, resolve_attribution};
use crate::{
    cli::{Cli, should_output_json},
    config::UserConfig,
};

#[derive(Serialize)]
struct ForkOutput {
    change_id: String,
    content_hash: String,
    thread: Option<String>,
    from_state: String,
    message: String,
}

/// Create a fork (exploration branch) from the current or specified state.
///
/// A fork creates a new state that is identical to the source state but with
/// a new change ID. This is useful for exploring alternative implementations
/// while preserving the ability to return to the original.
///
/// If `--name` is provided, a new thread is created pointing to the new state.
pub fn cmd_fork(cli: &Cli, name: Option<String>, from: Option<String>) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    // Determine the source state
    let source_state = if let Some(ref state_spec) = from {
        // Resolve the state specifier
        let change_id = repo
            .resolve_state(state_spec)?
            .ok_or_else(|| anyhow::anyhow!("State not found: {}", state_spec))?;
        repo.store()
            .get_state(&change_id)?
            .ok_or_else(|| anyhow::anyhow!("State not found: {}", state_spec))?
    } else {
        // Use current HEAD
        let change_id = ensure_current_state(
            &repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some("Bootstrap git-overlay before forking".to_string()),
        )?;
        repo.store()
            .get_state(&change_id)?
            .ok_or_else(|| anyhow::anyhow!("Current state not found"))?
    };

    // Create a new state with the same tree but a new change ID
    // The new state has the source state as its parent
    let user_config = UserConfig::load_default()?;
    let attribution = resolve_attribution(&repo, &user_config)?;
    let mut new_state =
        State::new_fork_of(source_state.tree, vec![source_state.change_id], attribution);

    // Copy over intent (modified to indicate fork)
    if let Some(ref intent) = source_state.intent {
        new_state = new_state.with_intent(format!("Fork: {}", intent));
    } else {
        new_state = new_state.with_intent(format!("Fork from {}", source_state.change_id.short()));
    }

    // Store the new state
    repo.store().put_state(&new_state)?;

    // If a name was provided, create a new thread
    if let Some(ref track_name) = name {
        let updates = vec![
            RefUpdate::Thread {
                name: track_name.clone(),
                expected: RefExpectation::Missing,
                new: Some(new_state.change_id),
            },
            RefUpdate::Head {
                expected: RefExpectation::Any,
                new: Head::Attached {
                    thread: track_name.clone(),
                },
            },
        ];
        repo.refs().update_refs(&updates)?;
    } else {
        // Detach HEAD to point to the new state
        repo.refs().write_head(&Head::Detached {
            state: new_state.change_id,
        })?;
    }

    // Record in oplog
    repo.oplog()
        .record_fork(&new_state.change_id, &source_state.change_id)?;

    let output = ForkOutput {
        change_id: new_state.change_id.short(),
        content_hash: new_state.compute_hash().short(),
        thread: name.clone(),
        from_state: source_state.change_id.short(),
        message: if let Some(ref track_name) = name {
            format!(
                "Created fork {} on thread '{}' from {}",
                new_state.change_id.short(),
                track_name,
                source_state.change_id.short()
            )
        } else {
            format!(
                "Created fork {} from {}",
                new_state.change_id.short(),
                source_state.change_id.short()
            )
        },
    };

    render_fork(&output, should_output_json(cli, Some(repo.config())))
}

fn render_fork(output: &ForkOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(output)?);
    } else {
        println!("{}", output.message);
    }
    Ok(())
}