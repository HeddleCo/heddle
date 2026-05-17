// SPDX-License-Identifier: Apache-2.0
//! Revert command - create inverse of a state's changes.

use std::fs;

use anyhow::{Result, anyhow};
use objects::object::{Attribution, FileChangeSet, Tree};
use repo::{DiffKind, Repository};
use serde::Serialize;

use super::history_target::resolve_state_id;
use crate::cli::{Cli, should_output_json, worktree_status_options};

#[derive(Serialize)]
struct RevertOutput {
    change_id: Option<String>,
    reverted_state: String,
    files_affected: Vec<String>,
    message: String,
}

pub fn cmd_revert(
    cli: &Cli,
    state_spec: String,
    message: Option<String>,
    no_commit: bool,
) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    let target_id = resolve_state_id(&repo, &state_spec)?;

    let target_state = repo
        .store()
        .get_state(&target_id)?
        .ok_or_else(|| anyhow!("State not found: {}", state_spec))?;

    let parent_tree = if let Some(parent_id) = target_state.first_parent() {
        let parent_state = repo
            .store()
            .get_state(parent_id)?
            .ok_or_else(|| anyhow!("Parent state not found"))?;
        repo.require_tree(&parent_state.tree)?
    } else {
        Tree::new()
    };

    // Reject up-front if the target tree itself is missing — without
    // this, the per-entry materialize path below would surface the
    // same corruption later with a less helpful "missing blob"-shaped
    // error. `require_tree` carries the fsck recovery hint.
    repo.require_tree(&target_state.tree)?;

    let empty_tree = Tree::new();
    let parent_hash = if target_state.first_parent().is_some() {
        parent_tree.hash()
    } else {
        empty_tree.hash()
    };

    let changes = repo.diff_trees(&parent_hash, &target_state.tree)?;

    if changes.is_empty() {
        return Err(anyhow!("No changes to revert in state {}", state_spec));
    }

    let current_state = repo.current_state()?;
    let current_tree = match current_state.as_ref() {
        Some(s) => repo.require_tree(&s.tree)?,
        None => Tree::new(),
    };

    let status = repo.compare_worktree_cached_with_options(
        &current_tree,
        &worktree_status_options(Some(repo.config())),
    )?;
    if !status.is_clean() {
        return Err(anyhow!(
            "Cannot revert: you have uncommitted changes.\n\
             Commit or stash your changes first."
        ));
    }

    let mut files_affected: Vec<String> = Vec::new();

    apply_inverse_changes(&repo, &parent_tree, &changes, &mut files_affected)?;

    if no_commit {
        if should_output_json(cli, Some(repo.config())) {
            println!(
                "{}",
                serde_json::to_string(&RevertOutput {
                    change_id: None,
                    reverted_state: target_id.short(),
                    files_affected,
                    message: "Changes applied to worktree (not committed)".to_string(),
                })?
            );
        } else {
            println!("Reverted {} (not committed)", target_id.short());
            for file in &files_affected {
                println!("  {}", file);
            }
        }
        return Ok(());
    }

    let revert_message = message.unwrap_or_else(|| format!("Revert {}", target_id.short()));

    let attribution = Attribution::human(repo.get_principal()?);
    let new_state = repo.snapshot_with_attribution(Some(revert_message), None, attribution)?;

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&RevertOutput {
                change_id: Some(new_state.change_id.short()),
                reverted_state: target_id.short(),
                files_affected,
                message: format!("Created revert state {}", new_state.change_id.short()),
            })?
        );
    } else {
        println!(
            "Reverted {} as {}",
            target_id.short(),
            new_state.change_id.short()
        );
        for file in &files_affected {
            println!("  {}", file);
        }
    }

    Ok(())
}

fn apply_inverse_changes(
    repo: &Repository,
    parent_tree: &Tree,
    changes: &FileChangeSet,
    files_affected: &mut Vec<String>,
) -> Result<()> {
    for change in changes {
        let full_path = repo.root().join(&change.path);

        match change.kind {
            DiffKind::Added => {
                if full_path.exists() {
                    if full_path.is_symlink() {
                        fs::remove_file(&full_path)?;
                    } else if full_path.is_dir() {
                        // Reverting an "Added" directory removes only its
                        // heddle-tracked descendants. Heddle-ignored
                        // siblings (`.git/`, `target/`, `node_modules/`, …)
                        // that the user materialized after the snapshot
                        // must survive — `remove_path_recursively` would
                        // silently nuke them.
                        repo.remove_tracked_descendants(&full_path)?;
                    } else {
                        fs::remove_file(&full_path)?;
                    }
                }
                files_affected.push(format!("- {}", change.path));
            }
            DiffKind::Deleted => {
                let Some(entry) = parent_tree.get(&change.path) else {
                    files_affected.push(format!("+ {}", change.path));
                    continue;
                };
                if !entry.is_blob() {
                    files_affected.push(format!("+ {}", change.path));
                    continue;
                }
                let blob = repo.require_blob(&entry.hash)?;

                if let Some(parent) = full_path.parent()
                    && !parent.exists()
                {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&full_path, blob.content())?;
                files_affected.push(format!("+ {}", change.path));
            }
            DiffKind::Modified => {
                let Some(entry) = parent_tree.get(&change.path) else {
                    files_affected.push(format!("M {}", change.path));
                    continue;
                };
                if !entry.is_blob() {
                    files_affected.push(format!("M {}", change.path));
                    continue;
                }
                let blob = repo.require_blob(&entry.hash)?;
                fs::write(&full_path, blob.content())?;
                files_affected.push(format!("M {}", change.path));
            }
            DiffKind::Unchanged => {}
        }
    }

    Ok(())
}