// SPDX-License-Identifier: Apache-2.0
//! Collapse command: squash multiple states into one.

use objects::store::ObjectStore;
use std::time::Instant;

use anyhow::Result;
use objects::object::State;
use oplog::OpRecord;
use refs::{Head, RefExpectation, RefUpdate};
use repo::Repository;
use serde::Serialize;

use super::{
    advice::RecoveryAdvice,
    history_target::{require_resolved_state, resolve_state_id},
};
use crate::{
    cli::{Cli, commands::snapshot::resolve_attribution, should_output_json},
    config::UserConfig,
};

#[derive(Serialize)]
struct CollapseOutput {
    change_id: String,
    content_hash: String,
    collapsed_count: usize,
    intent: Option<String>,
    message: String,
}

/// Collapse (squash) multiple states into a single state.
///
/// The resulting state has:
/// - The tree from the last state in the sequence
/// - The parents of the first state in the sequence
/// - A new intent (from --into) summarizing the collapsed work
/// - A new confidence (optional)
///
/// This is useful for cleaning up exploratory work before sharing.
pub fn cmd_collapse(
    cli: &Cli,
    states: Vec<String>,
    into: String,
    confidence: Option<f32>,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let json = should_output_json(cli, Some(repo.config()));

    if states.is_empty() {
        return Err(anyhow::anyhow!(collapse_requires_states_advice()));
    }

    let started = Instant::now();
    if !json {
        eprintln!("Collapsing {} states: resolving state ids...", states.len());
    }

    // Resolve all state specifiers to actual states
    let mut resolved_states = Vec::new();
    for state_spec in &states {
        let change_id = resolve_state_id(&repo, state_spec)?;
        let state = require_resolved_state(&repo, &change_id)?;
        resolved_states.push(state);
    }

    // The new state uses:
    // - Tree from the LAST state (most recent content)
    // - Parents from the FIRST state (preserving history connection)
    let first_state = &resolved_states[0];
    let last_state = &resolved_states[resolved_states.len() - 1];
    if !json {
        eprintln!("Using final tree from {}", last_state.change_id.short());
    }

    // Get attribution for the new state
    let user_config = UserConfig::load_default()?;
    let attribution = resolve_attribution(&repo, &user_config)?;

    // Create the collapsed state
    let mut new_state =
        State::new_collapse_of(last_state.tree, first_state.parents.clone(), attribution);
    new_state = new_state.with_intent(into.clone());

    if let Some(provenance) = repo.get_state_provenance_root(last_state)? {
        new_state = new_state.with_provenance(provenance);
    }

    if let Some(conf) = confidence {
        new_state = new_state.with_confidence(conf);
    }

    // Store the new state
    repo.store().put_state(&new_state)?;
    if !json {
        eprintln!("Writing collapsed state {}...", new_state.change_id.short());
    }

    // Build the published ref batch + its matching `Collapse` record, then
    // publish record-first through the write chokepoint (heddle#330 §2.2): the
    // record is the commit point and the ref publish is a post-commit
    // materialization, replacing the prior publish-then-record order. The
    // `Collapse` record names the published ref (a thread when HEAD was
    // attached, or a detached HEAD at the collapse result).
    let (ref_updates, published_thread): (Vec<RefUpdate>, Option<String>) =
        match repo.refs().read_head()? {
            Head::Attached { ref thread } => (
                vec![RefUpdate::Thread {
                    name: thread.clone(),
                    expected: RefExpectation::Any,
                    new: Some(new_state.change_id),
                }],
                Some(thread.to_string()),
            ),
            Head::Detached { .. } => (
                vec![RefUpdate::Head {
                    expected: RefExpectation::Any,
                    new: Head::Detached {
                        state: new_state.change_id,
                    },
                }],
                None,
            ),
        };

    let source_ids: Vec<_> = resolved_states.iter().map(|s| s.change_id).collect();
    let collapse_record = OpRecord::Collapse {
        sources: source_ids,
        result: new_state.change_id,
        thread: published_thread,
    };
    repo.commit_and_publish(vec![collapse_record], &ref_updates)?;

    let output = CollapseOutput {
        change_id: new_state.change_id.short(),
        content_hash: new_state.compute_hash().short(),
        collapsed_count: resolved_states.len(),
        intent: Some(into),
        message: format!(
            "Collapsed {} states into {}",
            resolved_states.len(),
            new_state.change_id.short()
        ),
    };

    if json {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!(
            "{} in {:.1}s",
            output.message,
            started.elapsed().as_secs_f32()
        );
    }

    Ok(())
}

fn collapse_requires_states_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "collapse_states_required",
        "No states specified to collapse",
        "List recent states with `heddle log`, then rerun `heddle collapse <state> --into <intent>` with at least one source state.",
        "collapse was invoked without any source state ids",
        "collapsing without source states would have to guess which history range should be replaced",
        "no collapsed state was written; HEAD, refs, oplog, and worktree files were left unchanged",
        "heddle log",
        vec![
            "heddle log".to_string(),
            "heddle collapse <state> --into <intent>".to_string(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_requires_states_advice_is_typed() {
        let advice = collapse_requires_states_advice();

        assert_eq!(advice.kind, "collapse_states_required");
        assert_eq!(advice.primary_command, "heddle log");
        assert!(advice.primary_hint().contains("heddle collapse"));
        assert!(advice.unsafe_condition.contains("without any source"));
        assert!(advice.would_change.contains("guess"));
        assert!(advice.preserved.contains("HEAD"));
    }
}
