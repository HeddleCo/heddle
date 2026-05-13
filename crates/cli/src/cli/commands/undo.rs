// SPDX-License-Identifier: Apache-2.0
//! Undo and redo commands.

use anyhow::{Result, anyhow};
use oplog::OpBatch;
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