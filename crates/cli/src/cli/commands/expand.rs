// SPDX-License-Identifier: Apache-2.0
//! Expand a squashed land back to its constituent captures.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow};
use objects::object::{Agent, State, StateId};
use oplog::{OpLogBackend, OpRecord};
use repo::{Repository, format_confidence};
use serde::Serialize;

use super::{
    advice::RecoveryAdvice,
    history_target::{require_resolved_state, resolve_state_id},
};
use crate::cli::{Cli, should_output_json, style};

const EXPAND_OUTPUT_KIND: &str = "expand";

#[derive(Serialize)]
struct ExpandOutput {
    output_kind: &'static str,
    status: &'static str,
    requested: String,
    collapsed: CollapsedLandOutput,
    captures: Vec<ExpandedCaptureOutput>,
}

#[derive(Serialize)]
struct CollapsedLandOutput {
    state_id: String,
    state_id_full: String,
    git_commit: Option<String>,
    thread: Option<String>,
    source_count: usize,
}

#[derive(Serialize)]
struct ExpandedCaptureOutput {
    state_id: String,
    state_id_full: String,
    content_hash: String,
    intent: Option<String>,
    principal: String,
    agent: Option<String>,
    confidence: Option<f32>,
    created_at: String,
    parents: Vec<String>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CollapseAnnotation {
    pub source_count: usize,
}

struct CollapseRecord {
    sources: Vec<StateId>,
    result: StateId,
    thread: Option<String>,
}

pub fn cmd_expand(cli: &Cli, reference: String) -> Result<()> {
    let repo = cli.open_repo()?;
    let target = resolve_expand_target(&repo, &reference)?;
    let collapse = find_collapse_for_result(&repo, &target)?
        .ok_or_else(|| anyhow!(not_expandable_advice(&reference, &target)))?;
    let captures = collapse
        .sources
        .iter()
        .map(|source| require_resolved_state(&repo, source).map(ExpandedCaptureOutput::from))
        .collect::<Result<Vec<_>>>()?;
    let git_commit = repo
        .latest_git_checkpoint_for_state(&collapse.result)
        .ok()
        .flatten()
        .map(|record| record.git_commit);

    let output = ExpandOutput {
        output_kind: EXPAND_OUTPUT_KIND,
        status: "completed",
        requested: reference,
        collapsed: CollapsedLandOutput {
            state_id: collapse.result.short(),
            state_id_full: collapse.result.to_string_full(),
            git_commit,
            thread: collapse.thread,
            source_count: captures.len(),
        },
        captures,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        print_human(&output);
    }
    Ok(())
}

pub(crate) fn collapse_annotations_for_states<'a>(
    repo: &Repository,
    states: impl IntoIterator<Item = &'a StateId>,
) -> Result<BTreeMap<StateId, CollapseAnnotation>> {
    let wanted = states.into_iter().copied().collect::<BTreeSet<_>>();
    let mut annotations = BTreeMap::new();
    if wanted.is_empty() {
        return Ok(annotations);
    }
    for entry in repo.oplog().recent(usize::MAX)? {
        if entry.undone {
            continue;
        }
        if let OpRecord::Collapse {
            sources, result, ..
        } = entry.operation
            && wanted.contains(&result)
        {
            annotations.insert(
                result,
                CollapseAnnotation {
                    source_count: sources.len(),
                },
            );
        }
    }
    Ok(annotations)
}

fn resolve_expand_target(repo: &Repository, reference: &str) -> Result<StateId> {
    if let Some(change) = mapped_change_for_git_oid(repo, reference)? {
        return Ok(change);
    }
    resolve_state_id(repo, reference)
}

fn mapped_change_for_git_oid(repo: &Repository, git_oid: &str) -> Result<Option<StateId>> {
    repo.git_overlay_mapped_state_for_git_commit(git_oid)
        .map_err(Into::into)
}

fn find_collapse_for_result(repo: &Repository, result: &StateId) -> Result<Option<CollapseRecord>> {
    for entry in repo.oplog().recent(usize::MAX)? {
        if entry.undone {
            continue;
        }
        if let OpRecord::Collapse {
            sources,
            result: collapse_result,
            thread,
            ..
        } = entry.operation
            && collapse_result == *result
        {
            return Ok(Some(CollapseRecord {
                sources,
                result: collapse_result,
                thread,
            }));
        }
    }
    Ok(None)
}

fn not_expandable_advice(reference: &str, target: &StateId) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "collapse_not_found",
        format!("No collapse found for {reference}"),
        "Run `heddle log` to find entries marked `[collapsed]`, then retry `heddle expand <state>` with one of those entries.",
        format!(
            "reference '{reference}' resolved to {}, but no matching OpRecord::Collapse result exists",
            target.short()
        ),
        "expanding needs the ordered source list recorded by the original collapse",
        "repository state and worktree files were left unchanged",
        "heddle log",
        vec!["heddle log".to_string()],
    )
}

fn print_human(output: &ExpandOutput) {
    let mut target = output.collapsed.state_id.clone();
    if let Some(git_commit) = &output.collapsed.git_commit {
        target.push_str(&format!(" git:{}", short_oid(git_commit)));
    }
    if let Some(thread) = &output.collapsed.thread {
        target.push_str(&format!(" thread:{thread}"));
    }
    println!(
        "Collapsed land {} contains {} capture(s):",
        style::state_id(&target),
        output.collapsed.source_count
    );
    for (index, capture) in output.captures.iter().enumerate() {
        let intent = capture.intent.as_deref().unwrap_or("(no intent)");
        match capture.confidence {
            Some(confidence) => println!(
                "  {}. {} {} {}",
                index + 1,
                style::state_id(&capture.state_id),
                style::bold(intent),
                style::dim(&format!(
                    "confidence {}",
                    format_confidence(Some(confidence))
                )),
            ),
            None => println!(
                "  {}. {} {}",
                index + 1,
                style::state_id(&capture.state_id),
                style::bold(intent),
            ),
        }
    }
}

fn short_oid(oid: &str) -> &str {
    heddle_core::short_oid(oid)
}

impl From<State> for ExpandedCaptureOutput {
    fn from(state: State) -> Self {
        Self {
            state_id: state.state_id.short(),
            state_id_full: state.state_id.to_string_full(),
            content_hash: state.compute_hash().short(),
            intent: state.intent,
            principal: state.attribution.principal.to_string(),
            agent: state.attribution.agent.as_ref().map(Agent::to_string),
            confidence: state.confidence,
            created_at: state.created_at.format("%Y-%m-%d %H:%M:%S").to_string(),
            parents: state.parents.iter().map(StateId::short).collect(),
        }
    }
}
