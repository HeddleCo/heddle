// SPDX-License-Identifier: Apache-2.0
//! Revert command - create inverse of a state's changes.

use std::fs;

use anyhow::{Result, anyhow};
use objects::object::{Attribution, FileChangeSet, Tree};
use repo::{DiffKind, Repository};
use serde::Serialize;

use super::{
    advice::RecoveryAdvice,
    history_target::{require_resolved_state, resolve_state_id},
    worktree_safety::ensure_worktree_clean,
};
use crate::cli::{Cli, should_output_json};

#[derive(Serialize)]
struct RevertOutput {
    output_kind: &'static str,
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
    let repo = cli.open_repo()?;

    let target_id = resolve_state_id(&repo, &state_spec)?;

    let target_state = require_resolved_state(&repo, &target_id)?;

    let parent_tree = if let Some(parent_id) = target_state.first_parent() {
        let parent_state = require_resolved_state(&repo, parent_id)?;
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
        return Err(anyhow!(no_changes_to_revert_advice(&target_id.short())));
    }

    ensure_worktree_clean(&repo, "revert")?;

    let mut files_affected: Vec<String> = Vec::new();

    apply_inverse_changes(&repo, &parent_tree, &changes, &mut files_affected)?;

    if no_commit {
        if should_output_json(cli, Some(repo.config())) {
            println!(
                "{}",
                serde_json::to_string(&RevertOutput {
                    output_kind: "revert",
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
                output_kind: "revert",
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

fn no_changes_to_revert_advice(state: &str) -> RecoveryAdvice {
    let inspect_command = format!("heddle show {state}");
    RecoveryAdvice::safety_refusal(
        "no_changes_to_revert",
        format!("No changes to revert in state {state}"),
        format!(
            "Inspect the state with `{inspect_command}` and choose a state with a non-empty diff."
        ),
        "the selected state has an empty diff relative to its parent",
        "revert would not update any files or create a meaningful inverse state",
        "repository state was left unchanged",
        inspect_command.clone(),
        vec![inspect_command, "heddle log".to_string()],
    )
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
