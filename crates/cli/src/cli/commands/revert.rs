// SPDX-License-Identifier: Apache-2.0
//! Revert command - create inverse of a state's changes.

use std::fs;

use anyhow::{Result, anyhow};
use heddle_core::{
    RevertMessageMode, RevertOutcome, RevertPlan, RevertSuccessFacts,
    default_revert_commit_message, no_changes_to_revert_kind, no_changes_to_revert_summary,
    plan_revert, revert_inspect_command, revert_success_message,
};
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
    let target_short = target_id.short();

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
    let target_tree = repo.require_tree(&target_state.tree)?;

    let empty_tree = Tree::new();
    let parent_hash = if target_state.first_parent().is_some() {
        parent_tree.hash()
    } else {
        empty_tree.hash()
    };

    let changes = repo.diff_trees(&parent_hash, &target_state.tree)?;

    if matches!(plan_revert(changes.len()), RevertPlan::NoChanges) {
        return Err(anyhow!(no_changes_to_revert_advice(&target_short)));
    }

    ensure_worktree_clean(&repo, "revert")?;

    let mut files_affected: Vec<String> = Vec::new();

    apply_inverse_changes(
        &repo,
        &parent_tree,
        &target_tree,
        &changes,
        &mut files_affected,
    )?;

    let json = should_output_json(cli, Some(repo.config()));

    if no_commit {
        let facts = RevertSuccessFacts {
            outcome: RevertOutcome::AppliedNotCommitted,
            state_short: &target_short,
            new_change_id_short: None,
        };
        let success_message = revert_success_message(
            &facts,
            if json {
                RevertMessageMode::Json
            } else {
                RevertMessageMode::Text
            },
        );
        if json {
            println!(
                "{}",
                serde_json::to_string(&RevertOutput {
                    output_kind: "revert",
                    change_id: None,
                    reverted_state: target_short,
                    files_affected,
                    message: success_message,
                })?
            );
        } else {
            println!("{success_message}");
            for file in &files_affected {
                println!("  {}", file);
            }
        }
        return Ok(());
    }

    let revert_message = message.unwrap_or_else(|| default_revert_commit_message(&target_short));

    let attribution = Attribution::human(repo.get_principal()?);
    let new_state = repo.snapshot_with_attribution(Some(revert_message), None, attribution)?;
    let new_short = new_state.change_id.short();
    let facts = RevertSuccessFacts {
        outcome: RevertOutcome::Committed,
        state_short: &target_short,
        new_change_id_short: Some(&new_short),
    };
    let success_message = revert_success_message(
        &facts,
        if json {
            RevertMessageMode::Json
        } else {
            RevertMessageMode::Text
        },
    );

    if json {
        println!(
            "{}",
            serde_json::to_string(&RevertOutput {
                output_kind: "revert",
                change_id: Some(new_short),
                reverted_state: target_short,
                files_affected,
                message: success_message,
            })?
        );
    } else {
        println!("{success_message}");
        for file in &files_affected {
            println!("  {}", file);
        }
    }

    Ok(())
}

fn no_changes_to_revert_advice(state: &str) -> RecoveryAdvice {
    let inspect_command = revert_inspect_command(state);
    RecoveryAdvice::safety_refusal(
        no_changes_to_revert_kind(),
        no_changes_to_revert_summary(state),
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
    target_tree: &Tree,
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
                        if let Some(source_subtree) =
                            repo.resolve_subtree(target_tree, std::path::Path::new(&change.path))?
                        {
                            // Reverting an "Added" directory removes only its
                            // heddle-tracked descendants. Heddle-ignored
                            // siblings (`.git/`, `target/`, `node_modules/`, ...)
                            // that the user materialized after the snapshot
                            // must survive.
                            repo.remove_tracked_descendants_with_source(
                                &full_path,
                                &source_subtree,
                            )?;
                        } else {
                            return Err(anyhow!(
                                "cannot safely revert added path `{}`: worktree holds a directory but the target state has no tracked subtree for that path",
                                change.path
                            ));
                        }
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
                let Some(hash) = entry.blob_hash() else {
                    files_affected.push(format!("+ {}", change.path));
                    continue;
                };
                let blob = repo.require_blob(&hash)?;

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
                let Some(hash) = entry.blob_hash() else {
                    files_affected.push(format!("M {}", change.path));
                    continue;
                };
                let blob = repo.require_blob(&hash)?;
                fs::write(&full_path, blob.content())?;
                files_affected.push(format!("M {}", change.path));
            }
            DiffKind::Unchanged => {}
        }
    }

    Ok(())
}
