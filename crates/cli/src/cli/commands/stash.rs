// SPDX-License-Identifier: Apache-2.0
//! Stash command implementation.

use anyhow::{Result, anyhow};
use objects::object::ContentHash;
use repo::{DiffKind, Repository};
use serde::Serialize;

use super::stash_ops::{apply_stash, build_worktree_tree, restore_worktree};
use crate::cli::{Cli, StashCommands, should_output_json, worktree_status_options};

#[derive(Serialize)]
struct StashOutput {
    message: String,
    stash_index: Option<usize>,
}

#[derive(Serialize)]
struct StashListEntry {
    index: usize,
    message: Option<String>,
    created_at: String,
}

#[derive(Serialize)]
struct StashListOutput {
    stashes: Vec<StashListEntry>,
}

#[derive(Serialize)]
struct StashShowOutput {
    modified: Vec<String>,
    added: Vec<String>,
    deleted: Vec<String>,
}

pub fn cmd_stash(cli: &Cli, command: StashCommands) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    match command {
        StashCommands::Push { message } => cmd_stash_push(cli, &repo, message),
        StashCommands::List => cmd_stash_list(cli, &repo),
        StashCommands::Pop => cmd_stash_pop(cli, &repo),
        StashCommands::Apply => cmd_stash_apply(cli, &repo),
        StashCommands::Drop => cmd_stash_drop(cli, &repo),
        StashCommands::Clear => cmd_stash_clear(cli, &repo),
        StashCommands::Show => cmd_stash_show(cli, &repo),
    }
}

fn cmd_stash_push(cli: &Cli, repo: &Repository, message: Option<String>) -> Result<()> {
    let current_state = repo.current_state()?;
    let current_tree = match current_state.as_ref() {
        Some(s) => repo.require_tree(&s.tree)?,
        None => objects::object::Tree::new(),
    };

    let status = repo.compare_worktree_cached_with_options(
        &current_tree,
        &worktree_status_options(Some(repo.config())),
    )?;

    if status.is_clean() {
        return Err(anyhow!("No changes to stash"));
    }

    let stash_manager = repo.stash_manager();
    stash_manager.init()?;

    let parent_tree_hash = current_tree.hash().to_string();

    let worktree_tree = build_worktree_tree(repo, &status)?;

    let tree_hash = repo.store().put_tree(&worktree_tree)?;

    let entry = stash_manager.push(tree_hash, parent_tree_hash, message)?;

    restore_worktree(repo, &current_tree, &status)?;

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&StashOutput {
                message: format!("Saved stash@{{{}}}", entry.index),
                stash_index: Some(entry.index),
            })?
        );
    } else {
        println!("Saved stash@{{{}}}", entry.index);
    }

    Ok(())
}

fn cmd_stash_list(cli: &Cli, repo: &Repository) -> Result<()> {
    let stash_manager = repo.stash_manager();
    let stashes = stash_manager.list()?;

    if should_output_json(cli, Some(repo.config())) {
        let entries: Vec<StashListEntry> = stashes
            .iter()
            .map(|s| StashListEntry {
                index: s.index,
                message: s.message.clone(),
                created_at: s.created_at.to_rfc3339(),
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string(&StashListOutput { stashes: entries })?
        );
    } else if stashes.is_empty() {
        println!("No stashes.");
    } else {
        for stash in stashes {
            let msg = stash.message.as_deref().unwrap_or("WIP on main");
            println!("stash@{{{}}}: {}", stash.index, msg);
        }
    }

    Ok(())
}

fn cmd_stash_pop(cli: &Cli, repo: &Repository) -> Result<()> {
    let stash_manager = repo.stash_manager();
    let stash = stash_manager
        .pop_with(|stash| {
            apply_stash(repo, stash).map_err(|err| std::io::Error::other(err.to_string()).into())
        })?
        .ok_or_else(|| anyhow!("No stash found"))?;

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&StashOutput {
                message: "Applied and dropped stash".to_string(),
                stash_index: None,
            })?
        );
    } else {
        println!("Applied and dropped stash@{{{}}}", stash.index);
    }

    Ok(())
}

fn cmd_stash_apply(cli: &Cli, repo: &Repository) -> Result<()> {
    let stash_manager = repo.stash_manager();
    let stash = stash_manager
        .top()?
        .ok_or_else(|| anyhow!("No stash found"))?;

    apply_stash(repo, &stash)?;

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&StashOutput {
                message: format!("Applied stash@{{{}}}", stash.index),
                stash_index: Some(stash.index),
            })?
        );
    } else {
        println!("Applied stash@{{{}}}", stash.index);
    }

    Ok(())
}

fn cmd_stash_drop(cli: &Cli, repo: &Repository) -> Result<()> {
    let stash_manager = repo.stash_manager();
    let stash = stash_manager.drop()?;

    match stash {
        Some(s) => {
            if should_output_json(cli, Some(repo.config())) {
                println!(
                    "{}",
                    serde_json::to_string(&StashOutput {
                        message: format!("Dropped stash@{{{}}}", s.index),
                        stash_index: None,
                    })?
                );
            } else {
                println!("Dropped stash@{{{}}}", s.index);
            }
        }
        None => {
            return Err(anyhow!("No stash to drop"));
        }
    }

    Ok(())
}

fn cmd_stash_clear(cli: &Cli, repo: &Repository) -> Result<()> {
    let stash_manager = repo.stash_manager();
    let count = stash_manager.clear()?;

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&StashOutput {
                message: format!("Cleared {} stash(es)", count),
                stash_index: None,
            })?
        );
    } else if count == 0 {
        println!("No stashes to clear");
    } else {
        println!("Cleared {} stash(es)", count);
    }

    Ok(())
}

fn cmd_stash_show(cli: &Cli, repo: &Repository) -> Result<()> {
    let stash_manager = repo.stash_manager();
    let stash = stash_manager
        .top()?
        .ok_or_else(|| anyhow!("No stash found"))?;

    let parent_tree_hash = ContentHash::from_hex(&stash.parent_tree_hash)
        .map_err(|e| anyhow!("Invalid parent tree hash: {}", e))?;
    let _parent_tree = repo.require_tree(&parent_tree_hash)?;

    let stash_tree_hash = ContentHash::from_hex(&stash.tree_hash)
        .map_err(|e| anyhow!("Invalid stash tree hash: {}", e))?;
    let _stash_tree = repo.require_tree(&stash_tree_hash)?;

    let changes = repo.diff_trees(&parent_tree_hash, &stash_tree_hash)?;

    if should_output_json(cli, Some(repo.config())) {
        let mut modified = Vec::new();
        let mut added = Vec::new();
        let mut deleted = Vec::new();

        for change in &changes {
            match change.kind {
                DiffKind::Modified => modified.push(change.path.clone()),
                DiffKind::Added => added.push(change.path.clone()),
                DiffKind::Deleted => deleted.push(change.path.clone()),
                DiffKind::Unchanged => {}
            }
        }

        println!(
            "{}",
            serde_json::to_string(&StashShowOutput {
                modified,
                added,
                deleted,
            })?
        );
    } else if changes.is_empty() {
        println!("Empty stash");
    } else {
        for change in &changes {
            let prefix = match change.kind {
                DiffKind::Modified => "M",
                DiffKind::Added => "A",
                DiffKind::Deleted => "D",
                DiffKind::Unchanged => continue,
            };
            println!("{} {}", prefix, change.path);
        }
    }

    Ok(())
}