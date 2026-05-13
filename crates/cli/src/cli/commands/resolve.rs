// SPDX-License-Identifier: Apache-2.0
//! Resolve command implementation.

use std::fs;

use anyhow::{Result, anyhow};
use repo::Repository;
use serde::Serialize;

use crate::cli::{Cli, should_output_json};

#[derive(Serialize)]
struct ResolveOutput {
    message: String,
    resolved: Vec<String>,
    remaining: Vec<String>,
}

#[derive(Serialize)]
struct ConflictList {
    conflicts: Vec<String>,
}

pub fn cmd_resolve(
    cli: &Cli,
    path: Option<String>,
    all: bool,
    list: bool,
    ours: bool,
    theirs: bool,
    abort: bool,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let merge_manager = repo.merge_state_manager();

    if abort {
        return cmd_resolve_abort(&repo, &merge_manager, cli);
    }

    if list {
        return cmd_resolve_list(&repo, &merge_manager, cli);
    }

    if all {
        return cmd_resolve_all(&repo, &merge_manager, cli, ours, theirs);
    }

    let Some(path) = path else {
        return Err(anyhow!(
            "Specify a file to resolve, or use --all, --list, or --abort"
        ));
    };

    cmd_resolve_file(&repo, &merge_manager, cli, &path, ours, theirs)
}

fn cmd_resolve_abort(
    repo: &Repository,
    merge_manager: &repo::MergeStateManager,
    cli: &Cli,
) -> Result<()> {
    abort_merge_state(repo, merge_manager)?;

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&ResolveOutput {
                message: "Merge aborted".to_string(),
                resolved: vec![],
                remaining: vec![],
            })?
        );
    } else {
        println!("Merge aborted");
    }

    Ok(())
}

pub(crate) fn abort_merge_state(
    repo: &Repository,
    merge_manager: &repo::MergeStateManager,
) -> Result<()> {
    let merge_state = merge_manager
        .load()?
        .ok_or_else(|| anyhow!("No merge in progress"))?;
    repo.fast_forward_attached(&merge_state.ours)?;
    merge_manager.abort()?;
    Ok(())
}

fn cmd_resolve_list(
    repo: &Repository,
    merge_manager: &repo::MergeStateManager,
    cli: &Cli,
) -> Result<()> {
    let unresolved = merge_manager.unresolved()?;

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&ConflictList {
                conflicts: unresolved.clone(),
            })?
        );
    } else if unresolved.is_empty() {
        println!("No unresolved conflicts");
    } else {
        for path in &unresolved {
            println!("{}", path);
        }
    }

    Ok(())
}

fn cmd_resolve_all(
    repo: &Repository,
    merge_manager: &repo::MergeStateManager,
    cli: &Cli,
    ours: bool,
    theirs: bool,
) -> Result<()> {
    let unresolved = merge_manager.unresolved()?;

    if unresolved.is_empty() {
        return Err(anyhow!("No conflicts to resolve"));
    }

    for path in &unresolved {
        resolve_file_with_version(repo, path, ours, theirs)?;
        merge_manager.resolve(path)?;
    }

    let remaining = merge_manager.unresolved()?;

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&ResolveOutput {
                message: format!("Resolved {} conflict(s)", unresolved.len()),
                resolved: unresolved.clone(),
                remaining: remaining.clone(),
            })?
        );
    } else {
        println!("Resolved {} conflict(s)", unresolved.len());
        for path in &unresolved {
            println!("  {}", path);
        }
        if !remaining.is_empty() {
            println!("Remaining: {} conflict(s)", remaining.len());
        }
    }

    Ok(())
}

fn cmd_resolve_file(
    repo: &Repository,
    merge_manager: &repo::MergeStateManager,
    cli: &Cli,
    path: &str,
    ours: bool,
    theirs: bool,
) -> Result<()> {
    resolve_file_with_version(repo, path, ours, theirs)?;
    merge_manager.resolve(path)?;

    let remaining = merge_manager.unresolved()?;

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&ResolveOutput {
                message: format!("Resolved {}", path),
                resolved: vec![path.to_string()],
                remaining,
            })?
        );
    } else {
        println!("Resolved {}", path);
        if !remaining.is_empty() {
            println!("{} conflict(s) remaining", remaining.len());
        }
    }

    Ok(())
}

fn resolve_file_with_version(
    repo: &Repository,
    path: &str,
    ours: bool,
    theirs: bool,
) -> Result<()> {
    if !ours && !theirs {
        return Ok(());
    }

    let merge_state = repo
        .merge_state_manager()
        .load()?
        .ok_or_else(|| anyhow!("No merge in progress"))?;

    let full_path = repo.root().join(path);

    if ours {
        let our_state = repo
            .store()
            .get_state(&merge_state.ours)?
            .ok_or_else(|| anyhow!("Our state not found"))?;
        let our_tree = repo.store().get_tree(&our_state.tree)?.unwrap_or_default();

        if let Some(entry) = our_tree.get(path) {
            let blob = repo.require_blob(&entry.hash)?;
            fs::write(&full_path, blob.content())?;
        }
    } else if theirs {
        let their_state = repo
            .store()
            .get_state(&merge_state.theirs)?
            .ok_or_else(|| anyhow!("Their state not found"))?;
        let their_tree = repo
            .store()
            .get_tree(&their_state.tree)?
            .unwrap_or_default();

        if let Some(entry) = their_tree.get(path) {
            let blob = repo.require_blob(&entry.hash)?;
            fs::write(&full_path, blob.content())?;
        }
    }

    Ok(())
}