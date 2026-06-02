// SPDX-License-Identifier: Apache-2.0
//! `heddle conflict list|show` — read-only conflict inspection.
//!
//! Active Heddle merges write text conflict markers in the working tree and
//! track unresolved paths in [`MergeState`]. Historical structured-conflict
//! blobs can also be persisted on a state. `list` and `show` inspect the
//! active merge first so the command users see during recovery is the same
//! command that can show the conflict they just listed.

use std::fs;

use anyhow::{Context, Result};
use objects::{
    object::{ConflictSymbol, StructuredConflict},
    store::ObjectStore,
};
use repo::{MergeState, Repository};
use serde::Serialize;

use crate::cli::{
    cli_args::{Cli, ConflictCommands, ConflictShowArgs},
    commands::command_catalog::{ActionTemplate, recommended_action_template},
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

#[derive(Serialize)]
struct ActiveMergeConflictShowOutput {
    output_kind: &'static str,
    kind: &'static str,
    id: String,
    file: String,
    symbol: String,
    resolved: bool,
    ours_state: String,
    theirs_state: String,
    base_state: Option<String>,
    worktree_content: Option<String>,
    recommended_action: String,
    recommended_action_template: Option<ActionTemplate>,
    next_action: String,
    next_action_template: Option<ActionTemplate>,
}

pub async fn run(cli: &Cli, command: &ConflictCommands) -> Result<()> {
    match command {
        ConflictCommands::List => run_list(cli).await,
        ConflictCommands::Show(args) => run_show(cli, args).await,
    }
}

async fn run_list(cli: &Cli) -> Result<()> {
    let repo = open_repo(cli)?;
    if let Some(merge_state) = repo.merge_state_manager().load()? {
        let view = active_merge_conflict_view(&merge_state);
        render_conflict_list(cli, &repo, &view, true)?;
        return Ok(());
    }

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
    render_conflict_list(cli, &repo, &view, false)?;
    Ok(())
}

fn render_conflict_list(
    cli: &Cli,
    repo: &Repository,
    view: &ConflictListOutput,
    active_merge: bool,
) -> Result<()> {
    if should_output_json(cli, Some(repo.config())) {
        let context = if active_merge {
            "serialize active merge conflict list"
        } else {
            "serialize conflict list"
        };
        println!("{}", serde_json::to_string(view).context(context)?);
    } else if view.conflicts.is_empty() {
        if active_merge {
            println!("No unresolved merge conflicts");
        } else {
            println!("(no structured conflicts on current state)");
        }
    } else {
        for c in &view.conflicts {
            if active_merge {
                println!("{}", c.file);
            } else {
                println!("{} {}:{}", c.id, c.file, c.symbol);
            }
        }
    }
    Ok(())
}

fn active_merge_conflict_view(merge_state: &MergeState) -> ConflictListOutput {
    ConflictListOutput {
        conflicts: merge_state
            .conflicts
            .iter()
            .filter(|path| !merge_state.resolved.contains(path))
            .map(|path| ConflictView {
                id: path.clone(),
                file: path.clone(),
                symbol: "text_merge".to_string(),
            })
            .collect(),
    }
}

async fn run_show(cli: &Cli, args: &ConflictShowArgs) -> Result<()> {
    let repo = open_repo(cli)?;
    if let Some(merge_state) = repo.merge_state_manager().load()? {
        return render_active_merge_conflict(cli, &repo, &merge_state, args);
    }

    let conflicts = load_head_conflicts(&repo)?;
    let conflict = conflicts
        .conflicts
        .iter()
        .find(|c| c.id == args.conflict_id);
    let Some(conflict) = conflict else {
        render_conflict_not_found(cli, &repo, &args.conflict_id);
        return Ok(());
    };
    render_structured_conflict(cli, &repo, conflict)
}

fn render_conflict_not_found(cli: &Cli, repo: &Repository, conflict_id: &str) {
    if should_output_json(cli, Some(repo.config())) {
        println!("null");
    } else {
        println!("conflict {conflict_id} not found");
    }
}

fn render_structured_conflict(
    cli: &Cli,
    repo: &Repository,
    conflict: &ConflictSymbol,
) -> Result<()> {
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

fn render_active_merge_conflict(
    cli: &Cli,
    repo: &Repository,
    merge_state: &MergeState,
    args: &ConflictShowArgs,
) -> Result<()> {
    let Some(path) = merge_state
        .conflicts
        .iter()
        .find(|path| path.as_str() == args.conflict_id)
    else {
        render_conflict_not_found(cli, repo, &args.conflict_id);
        return Ok(());
    };
    let resolved = merge_state.resolved.iter().any(|resolved| resolved == path);
    let worktree_content = fs::read_to_string(repo.root().join(path)).ok();
    let recommended_action = if resolved {
        "heddle continue".to_string()
    } else {
        format!("heddle resolve {path}")
    };
    let recommended_action_template = recommended_action_template(&recommended_action);
    let view = ActiveMergeConflictShowOutput {
        output_kind: "conflict_show",
        kind: "active_merge_conflict",
        id: path.clone(),
        file: path.clone(),
        symbol: "text_merge".to_string(),
        resolved,
        ours_state: merge_state.ours.short(),
        theirs_state: merge_state.theirs.short(),
        base_state: merge_state.base.as_ref().map(|state| state.short()),
        worktree_content,
        recommended_action: recommended_action.clone(),
        recommended_action_template: recommended_action_template.clone(),
        next_action: recommended_action,
        next_action_template: recommended_action_template,
    };

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&view).context("serialize active merge conflict")?
        );
    } else {
        println!("conflict {}", view.id);
        println!("  file: {}", view.file);
        println!("  kind: active text merge");
        println!("  resolved: {}", view.resolved);
        println!("  ours:   {}", view.ours_state);
        println!("  theirs: {}", view.theirs_state);
        if let Some(base) = &view.base_state {
            println!("  base:   {base}");
        }
        if let Some(content) = &view.worktree_content {
            println!("  worktree:");
            for line in content.lines() {
                println!("    {line}");
            }
        }
        println!("  next: {}", view.next_action);
    }
    Ok(())
}

fn open_repo(cli: &Cli) -> Result<Repository> {
    let cwd;
    let repo_path = if let Some(path) = cli.repo.as_ref() {
        path
    } else {
        cwd = std::env::current_dir().context("get current working directory")?;
        &cwd
    };
    Repository::open(repo_path).context("open Heddle repository")
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
