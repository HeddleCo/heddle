// SPDX-License-Identifier: Apache-2.0
//! Undo and redo commands.

use anyhow::{Result, anyhow};
use objects::object::ChangeId;
use oplog::{OpBatch, OpRecord};
use repo::Repository;
use serde::Serialize;

use super::{
    undo_apply::{apply_redo_batch, apply_undo_batch},
    worktree_safety::ensure_worktree_clean,
};
use crate::cli::{Cli, should_output_json};

#[derive(Serialize)]
struct OpListOutput {
    batches: Vec<OpBatchOutput>,
}

#[derive(Serialize)]
struct OpBatchOutput {
    batch_id: u64,
    timestamp: String,
    undone: bool,
    partial: bool,
    operations: Vec<OpListEntry>,
}

#[derive(Serialize)]
struct OpListEntry {
    id: u64,
    description: String,
    timestamp: String,
    undone: bool,
}

#[derive(Serialize)]
struct UndoRedoOutput {
    action: String,
    message: String,
    batches: Vec<OpBatchOutput>,
}

pub fn cmd_undo(cli: &Cli, steps: usize, list: bool, depth: usize, preview: bool) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    if list && preview {
        return Err(anyhow!("Use either --list or --preview, not both"));
    }

    if list {
        let scope = repo.op_scope();
        let batches = repo.oplog().recent_batches_scoped(depth, Some(&scope))?;
        let output = OpListOutput {
            batches: batches.iter().map(build_batch_output).collect(),
        };

        if should_output_json(cli, Some(repo.config())) {
            println!("{}", serde_json::to_string(&output)?);
        } else {
            println!("Recent operation batches (showing up to {}):", depth);
            if output.batches.is_empty() {
                println!("  No operations");
            } else {
                print_batches(&output.batches);
            }
        }

        return Ok(());
    }

    let scope = repo.op_scope();
    let batches = repo.oplog().undo_batches_scoped(steps, Some(&scope))?;

    if batches.is_empty() {
        return Err(anyhow!("Nothing to undo"));
    }

    if preview {
        let output = UndoRedoOutput {
            action: "undo".to_string(),
            message: format!(
                "Would undo {} batch{}",
                batches.len(),
                if batches.len() == 1 { "" } else { "es" }
            ),
            batches: batches.iter().map(build_batch_output).collect(),
        };

        if should_output_json(cli, Some(repo.config())) {
            println!("{}", serde_json::to_string(&output)?);
        } else {
            println!("{}", output.message);
            print_batches(&output.batches);
        }

        return Ok(());
    }

    ensure_worktree_clean(&repo, "undo")?;
    // Refuse before mutating anything when a state the batch needs to
    // restore is missing from the object store — typically because `gc
    // --prune` or a truncated oplog has reached past the live window.
    // Letting `apply_undo_batch` discover this mid-apply would leave the
    // repo half-undone (worktree partially rewritten, batch not marked).
    ensure_undo_states_reachable(&repo, &batches)?;

    let mut updated_batches = Vec::with_capacity(batches.len());
    for batch in batches {
        apply_undo_batch(&repo, &batch)?;
        updated_batches.push(repo.oplog().mark_batch_undone(&batch)?);
    }

    let output = UndoRedoOutput {
        action: "undo".to_string(),
        message: format!(
            "Undone {} batch{}",
            updated_batches.len(),
            if updated_batches.len() == 1 { "" } else { "es" }
        ),
        batches: updated_batches.iter().map(build_batch_output).collect(),
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{}", output.message);
        print_batches(&output.batches);
        print_head(&repo)?;
    }

    Ok(())
}

pub fn cmd_redo(cli: &Cli, steps: usize, preview: bool) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    let scope = repo.op_scope();
    let batches = repo.oplog().redo_batches_scoped(steps, Some(&scope))?;

    if batches.is_empty() {
        return Err(anyhow!("Nothing to redo"));
    }

    if preview {
        let output = UndoRedoOutput {
            action: "redo".to_string(),
            message: format!(
                "Would redo {} batch{}",
                batches.len(),
                if batches.len() == 1 { "" } else { "es" }
            ),
            batches: batches.iter().map(build_batch_output).collect(),
        };

        if should_output_json(cli, Some(repo.config())) {
            println!("{}", serde_json::to_string(&output)?);
        } else {
            println!("{}", output.message);
            print_batches(&output.batches);
        }

        return Ok(());
    }

    ensure_worktree_clean(&repo, "redo")?;

    let mut updated_batches = Vec::with_capacity(batches.len());
    for batch in batches {
        apply_redo_batch(&repo, &batch)?;
        updated_batches.push(repo.oplog().mark_batch_redone(&batch)?);
    }

    let output = UndoRedoOutput {
        action: "redo".to_string(),
        message: format!(
            "Redone {} batch{}",
            updated_batches.len(),
            if updated_batches.len() == 1 { "" } else { "es" }
        ),
        batches: updated_batches.iter().map(build_batch_output).collect(),
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{}", output.message);
        print_batches(&output.batches);
        print_head(&repo)?;
    }

    Ok(())
}

fn build_batch_output(batch: &OpBatch) -> OpBatchOutput {
    let (undone, partial) = batch_status(batch);
    let timestamp = batch
        .entries
        .iter()
        .map(|entry| entry.timestamp)
        .max()
        .map(format_timestamp)
        .unwrap_or_else(|| "unknown".to_string());

    OpBatchOutput {
        batch_id: batch.id,
        timestamp,
        undone,
        partial,
        operations: batch
            .entries
            .iter()
            .map(|entry| OpListEntry {
                id: entry.id,
                description: entry.operation.description(),
                timestamp: format_timestamp(entry.timestamp),
                undone: entry.undone,
            })
            .collect(),
    }
}

fn batch_status(batch: &OpBatch) -> (bool, bool) {
    let any_undone = batch.entries.iter().any(|entry| entry.undone);
    let all_undone = batch.entries.iter().all(|entry| entry.undone);
    (all_undone, any_undone && !all_undone)
}

fn format_timestamp(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    timestamp.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn print_batches(batches: &[OpBatchOutput]) {
    for batch in batches {
        let status = if batch.undone {
            " (undone)"
        } else if batch.partial {
            " (partial)"
        } else {
            ""
        };
        let op_count = batch.operations.len();
        println!(
            "  Batch {}{} {} op{}",
            batch.batch_id,
            status,
            op_count,
            if op_count == 1 { "" } else { "s" }
        );
        for entry in &batch.operations {
            let entry_status = if entry.undone { " (undone)" } else { "" };
            println!(
                "    {} {} {}{}",
                entry.id, entry.timestamp, entry.description, entry_status
            );
        }
    }
}

fn print_head(repo: &Repository) -> Result<()> {
    if let Some(id) = repo.head()? {
        println!("Now at: {}", id.short());
    }
    Ok(())
}

/// Walk every batch we're about to undo and verify that each state the
/// inverse would restore is still present in the object store. If any state
/// is missing we refuse before touching the worktree or marking batches
/// undone — letting the apply path discover the gap mid-flight would leave
/// the repository half-rewound (partial worktree apply, batch unmarked).
///
/// "Missing" here means a destructive boundary has been crossed: typically
/// `gc --prune` reached past the live oplog window, or an oplog backup was
/// restored without its underlying objects. The user gets a single clear
/// message instead of a raw `state not found` from deep in `goto`.
fn ensure_undo_states_reachable(repo: &Repository, batches: &[OpBatch]) -> Result<()> {
    let mut missing: Vec<(u64, ChangeId)> = Vec::new();
    for batch in batches {
        for entry in &batch.entries {
            for needed in states_required_for_undo(&entry.operation) {
                if !repo.store().has_state(&needed)? {
                    missing.push((entry.id, needed));
                }
            }
        }
    }
    if missing.is_empty() {
        return Ok(());
    }

    let shorts: Vec<String> = missing
        .iter()
        .map(|(op_id, id)| format!("op {} -> {}", op_id, id.short()))
        .collect();
    Err(anyhow!(
        "Refusing to undo: prior state(s) needed to restore have been garbage-collected or are otherwise missing from the object store ({}). \
         A destructive boundary (likely `heddle gc --prune`) has been crossed past the live oplog window — \
         undo cannot rewind here. Restore the missing states from a backup, or run `heddle undo --list` and pick an entry past the boundary.",
        shorts.join(", "),
    ))
}

/// Identify the state IDs that an inverse for `op` would need to load.
/// Variants whose undo is a no-op (e.g. `Fork`, `Collapse`, `Redact`,
/// `Purge`, `Checkpoint`) return an empty list — they don't reach into the
/// object store, so a missing object can't trip them.
fn states_required_for_undo(op: &OpRecord) -> Vec<ChangeId> {
    match op {
        OpRecord::Snapshot {
            prev_head: Some(prev),
            ..
        } => vec![*prev],
        OpRecord::Goto {
            prev_head: Some(prev),
            ..
        } => vec![*prev],
        OpRecord::ThreadDelete { state, .. } => vec![*state],
        OpRecord::ThreadUpdate { old_state, .. } => vec![*old_state],
        OpRecord::MarkerDelete { state, .. } => vec![*state],
        _ => Vec::new(),
    }
}