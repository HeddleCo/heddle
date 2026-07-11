// SPDX-License-Identifier: Apache-2.0
//! Stash command implementation.

use anyhow::{Result, anyhow};
use heddle_core::stash_plan::{
    StashEntryOpPlan, StashMessageMode, StashMutationReport, StashPushPlan, StashShowChangeKind,
    bucket_stash_show_changes, format_stash_list_line, plan_stash_entry_op, plan_stash_push,
    stash_list_is_empty, stash_mutation_message, stash_show_change_prefix, stash_show_is_empty,
};
use objects::{object::ContentHash, store::ObjectStore};
use repo::{DiffKind, Repository};
use serde::Serialize;

use super::{
    advice::RecoveryAdvice,
    stash_ops::{apply_stash, build_worktree_tree, restore_worktree},
};
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
    output_kind: &'static str,
    stashes: Vec<StashListEntry>,
}

#[derive(Serialize)]
struct StashShowOutput {
    output_kind: &'static str,
    modified: Vec<String>,
    added: Vec<String>,
    deleted: Vec<String>,
}

pub fn cmd_stash(cli: &Cli, command: StashCommands) -> Result<()> {
    let repo = cli.open_repo()?;

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

fn print_stash_mutation(cli: &Cli, repo: &Repository, report: &StashMutationReport) -> Result<()> {
    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{}",
            serde_json::to_string(&StashOutput {
                message: stash_mutation_message(report, StashMessageMode::Json),
                stash_index: report.json_stash_index(),
            })?
        );
    } else {
        println!("{}", stash_mutation_message(report, StashMessageMode::Text));
    }
    Ok(())
}

fn map_diff_kind(kind: DiffKind) -> StashShowChangeKind {
    match kind {
        DiffKind::Modified => StashShowChangeKind::Modified,
        DiffKind::Added => StashShowChangeKind::Added,
        DiffKind::Deleted => StashShowChangeKind::Deleted,
        DiffKind::Unchanged => StashShowChangeKind::Unchanged,
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

    if matches!(
        plan_stash_push(status.is_clean()),
        StashPushPlan::RefuseNoChanges
    ) {
        return Err(anyhow!(no_changes_to_stash_advice()));
    }

    let stash_manager = repo.stash_manager();
    stash_manager.init()?;

    let parent_tree_hash = current_tree.hash().to_string();

    let worktree_tree = build_worktree_tree(repo, &status)?;

    let tree_hash = repo.store().put_tree(&worktree_tree)?;

    let entry = stash_manager.push(tree_hash, parent_tree_hash, message)?;

    restore_worktree(repo, &current_tree, &status)?;

    print_stash_mutation(cli, repo, &StashMutationReport::stashed(entry.index))
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
            serde_json::to_string(&StashListOutput {
                output_kind: "stash_list",
                stashes: entries
            })?
        );
    } else if stash_list_is_empty(stashes.len()) {
        println!("No stashes.");
    } else {
        for stash in stashes {
            println!(
                "{}",
                format_stash_list_line(stash.index, stash.message.as_deref())
            );
        }
    }

    Ok(())
}

fn cmd_stash_pop(cli: &Cli, repo: &Repository) -> Result<()> {
    let stash_manager = repo.stash_manager();
    let stash = stash_manager.pop_with(|stash| {
        apply_stash(repo, stash).map_err(|err| std::io::Error::other(err.to_string()).into())
    })?;

    match plan_stash_entry_op(stash.is_some()) {
        StashEntryOpPlan::RefuseEmpty => Err(anyhow!(no_stash_available_advice(
            "pop stash",
            "No stash found"
        ))),
        StashEntryOpPlan::Proceed => {
            let stash = stash.expect("Proceed implies Some");
            print_stash_mutation(
                cli,
                repo,
                &StashMutationReport::applied_and_dropped(stash.index),
            )
        }
    }
}

fn cmd_stash_apply(cli: &Cli, repo: &Repository) -> Result<()> {
    let stash_manager = repo.stash_manager();
    let stash = stash_manager.top()?;

    match plan_stash_entry_op(stash.is_some()) {
        StashEntryOpPlan::RefuseEmpty => Err(anyhow!(no_stash_available_advice(
            "apply stash",
            "No stash found"
        ))),
        StashEntryOpPlan::Proceed => {
            let stash = stash.expect("Proceed implies Some");
            apply_stash(repo, &stash)?;
            print_stash_mutation(cli, repo, &StashMutationReport::applied(stash.index))
        }
    }
}

fn cmd_stash_drop(cli: &Cli, repo: &Repository) -> Result<()> {
    let stash_manager = repo.stash_manager();
    let stash = stash_manager.drop()?;

    match plan_stash_entry_op(stash.is_some()) {
        StashEntryOpPlan::RefuseEmpty => Err(anyhow!(no_stash_available_advice(
            "drop stash",
            "No stash to drop"
        ))),
        StashEntryOpPlan::Proceed => {
            let stash = stash.expect("Proceed implies Some");
            print_stash_mutation(cli, repo, &StashMutationReport::dropped(stash.index))
        }
    }
}

fn cmd_stash_clear(cli: &Cli, repo: &Repository) -> Result<()> {
    let stash_manager = repo.stash_manager();
    let count = stash_manager.clear()?;

    print_stash_mutation(cli, repo, &StashMutationReport::cleared(count))
}

fn cmd_stash_show(cli: &Cli, repo: &Repository) -> Result<()> {
    let stash_manager = repo.stash_manager();
    let stash = stash_manager.top()?;

    let stash = match plan_stash_entry_op(stash.is_some()) {
        StashEntryOpPlan::RefuseEmpty => {
            return Err(anyhow!(no_stash_available_advice(
                "show stash",
                "No stash found"
            )));
        }
        StashEntryOpPlan::Proceed => stash.expect("Proceed implies Some"),
    };

    let parent_tree_hash = ContentHash::from_hex(&stash.parent_tree_hash)
        .map_err(|e| anyhow!("Invalid parent tree hash: {}", e))?;
    let _parent_tree = repo.require_tree(&parent_tree_hash)?;

    let stash_tree_hash = ContentHash::from_hex(&stash.tree_hash)
        .map_err(|e| anyhow!("Invalid stash tree hash: {}", e))?;
    let _stash_tree = repo.require_tree(&stash_tree_hash)?;

    let changes = repo.diff_trees(&parent_tree_hash, &stash_tree_hash)?;

    if should_output_json(cli, Some(repo.config())) {
        let buckets = bucket_stash_show_changes(
            changes
                .iter()
                .map(|change| (map_diff_kind(change.kind), change.path.as_str())),
        );

        println!(
            "{}",
            serde_json::to_string(&StashShowOutput {
                output_kind: "stash_show",
                modified: buckets.modified,
                added: buckets.added,
                deleted: buckets.deleted,
            })?
        );
    } else if stash_show_is_empty(changes.len()) {
        println!("Empty stash");
    } else {
        for change in &changes {
            let Some(prefix) = stash_show_change_prefix(map_diff_kind(change.kind)) else {
                continue;
            };
            println!("{prefix} {}", change.path);
        }
    }

    Ok(())
}

fn no_changes_to_stash_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "no_changes_to_stash",
        "No changes to stash",
        "Inspect the worktree with `heddle status`; make changes before running `heddle stash push -m \"...\"`.",
        "the worktree has no modified, deleted, or untracked paths",
        "stash push would create an empty stash entry with no recoverable work",
        "repository state was left unchanged",
        "heddle status",
        vec![
            "heddle status".to_string(),
            "heddle stash push -m \"...\"".to_string(),
        ],
    )
}

fn no_stash_available_advice(action: &'static str, error: &'static str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "no_stash_available",
        error,
        "Inspect the stash stack with `heddle stash list`; create one with `heddle stash push -m \"...\"` before retrying.",
        "the stash stack is empty",
        format!("{action} would need an existing stash entry"),
        "repository state was left unchanged",
        "heddle stash list",
        vec![
            "heddle stash list".to_string(),
            "heddle stash push -m \"...\"".to_string(),
        ],
    )
}
