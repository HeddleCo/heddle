// SPDX-License-Identifier: Apache-2.0
//! Fork command: create exploration branch.

use objects::store::ObjectStore;
use anyhow::Result;
use objects::object::{State, ThreadName};
use oplog::OpRecord;
use refs::{Head, RefExpectation, RefUpdate};
use repo::Repository;
use serde::Serialize;

use super::{
    history_target::{require_resolved_state, resolve_state_id},
    snapshot::{ensure_current_state, resolve_attribution},
};
use crate::{
    cli::{should_output_json, Cli},
    config::UserConfig,
};

#[derive(Serialize)]
struct ForkOutput {
    output_kind: &'static str,
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
    let user_config = UserConfig::load_default()?;

    // Determine the source state
    let source_state = if let Some(ref state_spec) = from {
        let change_id = resolve_state_id(&repo, state_spec)?;
        require_resolved_state(&repo, &change_id)?
    } else {
        // Use current HEAD
        let change_id = ensure_current_state(
            &repo,
            &user_config,
            Some("Bootstrap git-overlay before forking".to_string()),
        )?;
        require_resolved_state(&repo, &change_id)?
    };

    // Create a new state with the same tree but a new change ID
    // The new state has the source state as its parent
    let attribution = resolve_attribution(&repo, &user_config)?;
    let mut new_state =
        State::new_fork_of(source_state.tree, vec![source_state.change_id], attribution);

    // Copy over intent (modified to indicate fork)
    if let Some(ref intent) = source_state.intent {
        new_state = new_state.with_intent(format!("Fork: {}", intent));
    } else {
        new_state = new_state.with_intent(format!("Fork from {}", source_state.change_id.short()));
    }

    // Store the new state (orphan until a ref points at it).
    repo.store().put_state(&new_state)?;

    // Build the atomic ref batch + its matching `Fork` record, then publish
    // record-first through the write chokepoint (heddle#330 §2.2): the oplog
    // record is the commit point (phase 4) and the ref publish is a post-commit
    // materialization (phase 5), so a crash between them leaves a re-derivable
    // ref — never a reader-visible ref with no undo record (the prior
    // publish-then-record order). The `Fork` record carries the published ref
    // identity and the corrected `from = source_state` argument order (r15).
    let (ref_updates, fork_record) = if let Some(ref track_name) = name {
        let tn = ThreadName::new(track_name.as_str());
        (
            vec![
                RefUpdate::Thread {
                    name: tn.clone(),
                    expected: RefExpectation::Missing,
                    new: Some(new_state.change_id),
                },
                RefUpdate::Head {
                    expected: RefExpectation::Any,
                    new: Head::Attached { thread: tn },
                },
            ],
            OpRecord::Fork {
                from: source_state.change_id,
                new_state: new_state.change_id,
                thread: Some(track_name.clone()),
                head: None,
            },
        )
    } else {
        (
            vec![RefUpdate::Head {
                expected: RefExpectation::Any,
                new: Head::Detached {
                    state: new_state.change_id,
                },
            }],
            OpRecord::Fork {
                from: source_state.change_id,
                new_state: new_state.change_id,
                thread: None,
                head: Some(new_state.change_id),
            },
        )
    };
    repo.commit_and_publish(vec![fork_record], &ref_updates)?;

    let output = ForkOutput {
        output_kind: "fork",
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
