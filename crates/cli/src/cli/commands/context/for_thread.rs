// SPDX-License-Identifier: Apache-2.0
//! One bounded, deterministic pre-turn Thread briefing.

use std::collections::BTreeSet;

use anyhow::{Result, anyhow};
use objects::{
    object::{
        AnnotationStatus, CollaborationAnchor, ContentHash, ContextTarget, ThreadName,
        thread_replication::{
            ThreadOperationBody,
            metadata::{Control, Property, ThreadControl},
        },
    },
    store::ObjectStore,
};
use repo::{
    CollaborationStore, ContextConsumptionReceipt, Repository, SuppliedAnnotation, SuppliedRevision,
};
use serde::Serialize;

use super::super::{
    next_action::{NextActionValidationContext, write_full_command_json},
    thread::find_thread_summary,
};
use crate::cli::{Cli, should_output_json, style};

const MAX_ANNOTATIONS: usize = 64;
const MAX_DISCUSSIONS: usize = 32;
const MAX_TURNS_PER_DISCUSSION: usize = 8;

#[derive(Serialize)]
struct IntentItem {
    version: String,
    outcome: String,
    acceptance_criteria: Vec<String>,
}

#[derive(Serialize)]
struct AnnotationItem {
    target: String,
    scope: String,
    status: &'static str,
    annotation_id: String,
    revisions: Vec<super::RevisionOutput>,
}

#[derive(Serialize)]
struct DiscussionItem {
    title: String,
    anchor: String,
    blocking: bool,
    status: &'static str,
    turns: Vec<String>,
    omitted_turns: usize,
}

#[derive(Serialize)]
struct ThreadBriefing {
    output_kind: &'static str,
    thread: String,
    intent: Vec<IntentItem>,
    annotations: Vec<AnnotationItem>,
    discussions: Vec<DiscussionItem>,
    blockers: Vec<String>,
    omitted_annotations: usize,
    omitted_discussions: usize,
    briefing_hash: String,
}

pub fn cmd_context_for_thread(cli: &Cli, thread: &str) -> Result<()> {
    let repo = cli.open_repo()?;
    let head_id = repo
        .refs()
        .get_thread(&ThreadName::new(thread))?
        .ok_or_else(|| anyhow!("Thread {thread:?} not found"))?;
    let head = repo
        .store()
        .get_state(&head_id)?
        .ok_or_else(|| anyhow!("Thread {thread:?} has no local source state"))?;
    let summary = find_thread_summary(&repo, thread)?
        .ok_or_else(|| anyhow!("Thread {thread:?} has no local summary"))?;
    let changed_paths: BTreeSet<String> = summary.changed_paths.iter().cloned().collect();
    let base_id = summary
        .base_state
        .as_deref()
        .map(|value| repo.resolve_state(value))
        .transpose()?
        .flatten();
    let touched_symbols: BTreeSet<(String, String)> = if let Some(base) = base_id {
        repo.semantic_diff_symbols(&base, &head.id())?
            .into_iter()
            .map(|delta| (delta.anchor.file, delta.anchor.symbol))
            .collect()
    } else {
        BTreeSet::new()
    };

    let intent = intent_items(&repo, thread)?;
    let mut annotations = annotation_items(&repo, &head, &changed_paths)?;
    let omitted_annotations = annotations.len().saturating_sub(MAX_ANNOTATIONS);
    annotations.truncate(MAX_ANNOTATIONS);
    let mut discussions = discussion_items(&repo, thread, &changed_paths, &touched_symbols)?;
    let omitted_discussions = discussions.len().saturating_sub(MAX_DISCUSSIONS);
    discussions.truncate(MAX_DISCUSSIONS);
    let mut blockers = summary.blockers;
    blockers.extend(
        discussions
            .iter()
            .filter(|item| item.blocking)
            .map(|item| item.title.clone()),
    );
    blockers.sort();
    blockers.dedup();

    let mut briefing = ThreadBriefing {
        output_kind: "context_for_thread",
        thread: thread.to_owned(),
        intent,
        annotations,
        discussions,
        blockers,
        omitted_annotations,
        omitted_discussions,
        briefing_hash: String::new(),
    };
    let bytes = serde_json::to_vec(&briefing)?;
    briefing.briefing_hash = ContentHash::compute_typed("context-briefing", &bytes).to_string();
    let receipt = ContextConsumptionReceipt {
        format_version: 1,
        thread: thread.to_owned(),
        intent_versions: briefing
            .intent
            .iter()
            .map(|item| item.version.clone())
            .collect(),
        annotations: briefing
            .annotations
            .iter()
            .map(|item| SuppliedAnnotation {
                target: item.target.clone(),
                annotation_id: item.annotation_id.clone(),
                revisions: item
                    .revisions
                    .iter()
                    .map(|revision| SuppliedRevision {
                        revision_id: revision.revision_id.clone(),
                        kind: revision.kind.clone(),
                        content: revision.content.clone(),
                    })
                    .collect(),
            })
            .collect(),
        briefing_hash: briefing.briefing_hash.clone(),
    };

    if should_output_json(cli, Some(repo.config())) {
        write_full_command_json(
            &briefing,
            NextActionValidationContext::new(&["context"], repo.capability()),
        )?;
    } else {
        print_briefing(&briefing);
    }
    repo.write_pending_context_receipt(&receipt)?;
    Ok(())
}

fn intent_items(repo: &Repository, thread: &str) -> Result<Vec<IntentItem>> {
    let Some((_, replica)) = repo
        .list_native_threads()?
        .into_iter()
        .find(|(name, _)| name == thread)
    else {
        return Ok(Vec::new());
    };
    let mut items = Vec::new();
    for (version, signed) in replica.metadata_frontier(&Property::Intent)? {
        let operation = signed.verify()?;
        let ThreadOperationBody::Metadata(bytes) = operation.body else {
            return Err(anyhow!("Thread intent frontier contains another operation"));
        };
        let Control::Intent(intent) = ThreadControl::decode(&bytes)?.control else {
            return Err(anyhow!("Thread intent frontier contains another property"));
        };
        items.push(IntentItem {
            version: version.to_string(),
            outcome: intent.outcome,
            acceptance_criteria: intent.acceptance_criteria,
        });
    }
    items.sort_by(|left, right| left.version.cmp(&right.version));
    Ok(items)
}

fn annotation_items(
    repo: &Repository,
    head: &objects::object::State,
    changed_paths: &BTreeSet<String>,
) -> Result<Vec<AnnotationItem>> {
    let Some(root) = repo.inherit_parent_context(head)? else {
        return Ok(Vec::new());
    };
    let mut items = Vec::new();
    for entry in repo.list_context_entries(&root, None)? {
        if let ContextTarget::File { path } = &entry.target
            && !changed_paths.is_empty()
            && !changed_paths.contains(path)
        {
            continue;
        }
        let target = entry.target.path().unwrap_or("state context").to_owned();
        for annotation in entry.blob.annotations {
            if annotation.status == AnnotationStatus::Superseded {
                continue;
            }
            let current_ids: BTreeSet<&str> =
                annotation.current_revision_ids().into_iter().collect();
            let revisions = annotation
                .revisions
                .iter()
                .filter(|revision| current_ids.contains(revision.revision_id.as_str()))
                .map(|revision| super::RevisionOutput {
                    revision_id: revision.revision_id.clone(),
                    kind: revision.kind.to_string(),
                    content: revision.content.clone(),
                    tags: revision.tags.clone(),
                    attribution: revision.attribution.clone(),
                    created_at: revision.created_at,
                })
                .collect();
            items.push(AnnotationItem {
                target: target.clone(),
                scope: annotation.scope.to_string(),
                status: if annotation.divergent_revision_ids.is_empty() {
                    "current"
                } else {
                    "diverged: needs a decision"
                },
                annotation_id: annotation.annotation_id,
                revisions,
            });
        }
    }
    items.sort_by(|left, right| {
        (&left.target, &left.annotation_id).cmp(&(&right.target, &right.annotation_id))
    });
    Ok(items)
}

fn discussion_items(
    repo: &Repository,
    thread: &str,
    changed_paths: &BTreeSet<String>,
    touched_symbols: &BTreeSet<(String, String)>,
) -> Result<Vec<DiscussionItem>> {
    if !repo.heddle_dir().join("collaboration").exists() {
        return Ok(Vec::new());
    }
    let store = CollaborationStore::open(repo.heddle_dir())?;
    let materialized = store.materialize()?;
    let mut items = Vec::new();
    for discussion in materialized.discussions.into_values() {
        if discussion.resolution.is_some() && discussion.conflict_operations.is_empty() {
            continue;
        }
        let applicable = discussion.thread_ref.as_deref() == Some(thread)
            || match &discussion.anchor {
                CollaborationAnchor::Path { path, .. } => changed_paths.contains(path),
                CollaborationAnchor::Symbol { path, symbol, .. } => {
                    touched_symbols.contains(&(path.clone(), symbol.clone()))
                }
                _ => false,
            };
        if !applicable {
            continue;
        }
        let anchor = match &discussion.anchor {
            CollaborationAnchor::Path { path, .. } => path.clone(),
            CollaborationAnchor::Symbol { path, symbol, .. } => format!("{path}::{symbol}"),
            CollaborationAnchor::Repository => "repository".into(),
            _ => "thread".into(),
        };
        let omitted_turns = discussion
            .turns
            .len()
            .saturating_sub(MAX_TURNS_PER_DISCUSSION);
        let turns = discussion
            .turns
            .iter()
            .rev()
            .take(MAX_TURNS_PER_DISCUSSION)
            .map(|(_, turn)| turn.body.clone())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        items.push(DiscussionItem {
            title: discussion.title,
            anchor,
            blocking: discussion.blocking,
            status: if discussion.conflict_operations.is_empty() {
                "open"
            } else {
                "conflicted"
            },
            turns,
            omitted_turns,
        });
    }
    items.sort_by(|left, right| (&left.anchor, &left.title).cmp(&(&right.anchor, &right.title)));
    Ok(items)
}

fn print_briefing(briefing: &ThreadBriefing) {
    println!("Thread: {}", style::human_text(&briefing.thread));
    if briefing.intent.is_empty() {
        println!("Intent: none recorded");
    } else {
        for intent in &briefing.intent {
            println!("Intent: {}", style::human_text(&intent.outcome));
            for criterion in &intent.acceptance_criteria {
                println!("  Acceptance: {}", style::human_text(criterion));
            }
        }
        if briefing.intent.len() > 1 {
            println!("Intent diverged: needs a decision");
        }
    }
    println!("Context:");
    for annotation in &briefing.annotations {
        println!(
            "  {} {} ({})",
            style::human_text(&annotation.target),
            annotation.scope,
            annotation.status
        );
        for revision in &annotation.revisions {
            println!(
                "    {}: {}",
                revision.kind,
                style::human_text(&revision.content)
            );
        }
    }
    if briefing.annotations.is_empty() {
        println!("  none");
    }
    if briefing.omitted_annotations > 0 {
        println!(
            "  {} more annotations omitted",
            briefing.omitted_annotations
        );
    }
    println!("Open discussions:");
    for discussion in &briefing.discussions {
        println!(
            "  {} — {}{}",
            style::human_text(&discussion.anchor),
            style::human_text(&discussion.title),
            if discussion.blocking {
                " (blocker)"
            } else {
                ""
            }
        );
        for turn in &discussion.turns {
            println!("    {}", style::human_text(turn));
        }
        if discussion.omitted_turns > 0 {
            println!("    {} earlier turns omitted", discussion.omitted_turns);
        }
    }
    if briefing.discussions.is_empty() {
        println!("  none");
    }
    if briefing.omitted_discussions > 0 {
        println!(
            "  {} more discussions omitted",
            briefing.omitted_discussions
        );
    }
    if !briefing.blockers.is_empty() {
        println!("Blockers:");
        for blocker in &briefing.blockers {
            println!("  {}", style::human_text(blocker));
        }
    }
}
