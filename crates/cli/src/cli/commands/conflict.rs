// SPDX-License-Identifier: Apache-2.0
//! `heddle conflict list|show` — read-only inspection of W1
//! [`StructuredConflict`] blobs persisted on a state.
//!
//! Resolution is intentionally not exposed here yet: the merge flow does
//! not currently emit structured-conflict objects (it writes text
//! markers in the working tree), so a `conflict resolve` verb would have
//! nothing to act on. The verb returns when the merge integration lands.
//! Use `heddle resolve` for the existing text-marker flow.

use anyhow::{Context, Result};
use objects::object::StructuredConflict;
use repo::Repository;
use serde::Serialize;

use crate::cli::{
    cli_args::{Cli, ConflictCommands, ConflictShowArgs},
    should_output_json,
};

#[derive(Serialize)]
struct ConflictListOutput {
    conflicts: Vec<ConflictView>,
}

#[derive(Serialize)]
struct ConflictView {
    id: String,
    file: String,
    symbol: String,
}

pub async fn run(cli: &Cli, command: &ConflictCommands) -> Result<()> {
    match command {
        ConflictCommands::List => run_list(cli).await,
        ConflictCommands::Show(args) => run_show(cli, args).await,
    }
}

async fn run_list(cli: &Cli) -> Result<()> {
    let repo = open_repo()?;
    let conflicts = load_head_conflicts(&repo)?;
    let view = ConflictListOutput {
        conflicts: conflicts
            .conflicts
            .iter()
            .map(|c| ConflictView {
                id: c.id.clone(),
                file: c.anchor.file.clone(),
                symbol: c.anchor.symbol.clone(),
            })
            .collect(),
    };
    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&view).context("serialize conflict list")?
        );
    } else if view.conflicts.is_empty() {
        println!("(no structured conflicts on current state)");
    } else {
        for c in &view.conflicts {
            println!("{} {}:{}", c.id, c.file, c.symbol);
        }
    }
    Ok(())
}

async fn run_show(cli: &Cli, args: &ConflictShowArgs) -> Result<()> {
    let repo = open_repo()?;
    let conflicts = load_head_conflicts(&repo)?;
    let conflict = conflicts
        .conflicts
        .iter()
        .find(|c| c.id == args.conflict_id);
    let Some(conflict) = conflict else {
        if should_output_json(cli, Some(repo.config())) {
            println!("null");
        } else {
            println!("conflict {} not found", args.conflict_id);
        }
        return Ok(());
    };
    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(conflict).context("serialize conflict")?
        );
    } else {
        println!("conflict {}", conflict.id);
        println!(
            "  anchor: {}:{}",
            conflict.anchor.file, conflict.anchor.symbol
        );
        println!("  base:   {}", short_body(&conflict.base.body));
        println!("  ours:   {}", short_body(&conflict.ours.body));
        println!("  theirs: {}", short_body(&conflict.theirs.body));
        if !conflict.candidate_resolutions.is_empty() {
            println!("  candidates:");
            for cand in &conflict.candidate_resolutions {
                println!("    {cand:?}");
            }
        }
    }
    Ok(())
}

fn open_repo() -> Result<Repository> {
    let cwd = std::env::current_dir().context("get current working directory")?;
    Repository::open(&cwd).context("open Heddle repository")
}

fn load_head_conflicts(repo: &Repository) -> Result<StructuredConflict> {
    let Some(head) = repo.head().context("read HEAD")? else {
        return Ok(StructuredConflict::new(Vec::new()));
    };
    let state = repo
        .store()
        .get_state(&head)
        .context("load HEAD state")?
        .ok_or_else(|| anyhow::anyhow!("HEAD state {head} missing from object store"))?;
    let Some(hash) = state.structured_conflicts else {
        return Ok(StructuredConflict::new(Vec::new()));
    };
    let blob = repo
        .store()
        .get_blob(&hash)
        .context("load structured-conflicts blob")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "structured_conflicts blob {hash} referenced by state {head} is missing"
            )
        })?;
    StructuredConflict::decode(blob.content()).context("decode structured-conflicts blob")
}

fn short_body(s: &str) -> String {
    let first = s.lines().next().unwrap_or("");
    if first.len() > 60 {
        format!("{}…", &first[..60])
    } else {
        first.to_string()
    }
}
