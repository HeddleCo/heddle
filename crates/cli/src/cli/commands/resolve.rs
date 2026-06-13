// SPDX-License-Identifier: Apache-2.0
//! Resolve command implementation.

use objects::store::ObjectStore;
use std::fs;

use anyhow::{Context, Result, anyhow};
use repo::{MergeState, Repository};
use serde::Serialize;

use super::advice::RecoveryAdvice;
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

#[allow(clippy::too_many_arguments)]
pub fn cmd_resolve(
    cli: &Cli,
    path: Option<String>,
    all: bool,
    list: bool,
    ours: bool,
    theirs: bool,
    force: bool,
    abort: bool,
) -> Result<()> {
    let repo = cli.open_repo()?;
    let merge_manager = repo.merge_state_manager();

    if abort {
        return cmd_resolve_abort(&repo, &merge_manager, cli);
    }

    if list {
        return cmd_resolve_list(&repo, &merge_manager, cli);
    }

    if all {
        return cmd_resolve_all(&repo, &merge_manager, cli, ours, theirs, force);
    }

    let Some(path) = path else {
        return Err(anyhow!(
            "Specify a file to resolve, or use --all, --list, or --abort"
        ));
    };

    cmd_resolve_file(&repo, &merge_manager, cli, &path, ours, theirs, force)
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
    let merge_state = load_merge_state_or_advice(merge_manager, "abort merge")?;
    // The 3-way merge that preceded this abort wrote a partial tree
    // (conflict markers) but did not move HEAD or the target thread
    // ref — both stay at `ours` throughout the conflicted-merge
    // window. The FF here is therefore a worktree reset to `ours`,
    // not a thread advance, so the recorded `FastForward`'s
    // `pre_target_id` and `post_target_id` are equal. Migrated as
    // part of the heddle#110 Rule-7 sweep for uniformity with the
    // other `fast_forward_attached` callers: a future merge variant
    // that *does* move HEAD before aborting (e.g. a partial-apply
    // shape) would then get correct undo semantics for free without
    // a second migration.
    super::ff_record::record_ff_advance_discard_local(repo, "<abort>", &merge_state.ours)?;
    merge_manager.abort()?;
    Ok(())
}

fn cmd_resolve_list(
    repo: &Repository,
    merge_manager: &repo::MergeStateManager,
    cli: &Cli,
) -> Result<()> {
    let merge_state = load_merge_state_or_advice(merge_manager, "list merge conflicts")?;
    let unresolved = unresolved_paths(&merge_state);

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
    force: bool,
) -> Result<()> {
    let merge_state = load_merge_state_or_advice(merge_manager, "resolve merge conflicts")?;
    let unresolved = unresolved_paths(&merge_state);

    if unresolved.is_empty() {
        return Err(anyhow!(no_conflicts_to_resolve_advice()));
    }

    for path in &unresolved {
        resolve_file_with_version(repo, &merge_state, path, ours, theirs)?;
        ensure_resolved_file_has_no_conflict_markers(repo, path, ours || theirs, force)?;
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
    force: bool,
) -> Result<()> {
    let merge_state = load_merge_state_or_advice(merge_manager, "resolve merge conflict")?;
    if !merge_state
        .conflicts
        .iter()
        .any(|conflict| conflict == path)
    {
        return Err(anyhow!(path_not_in_active_merge_advice(path)));
    }
    resolve_file_with_version(repo, &merge_state, path, ours, theirs)?;
    ensure_resolved_file_has_no_conflict_markers(repo, path, ours || theirs, force)?;
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

fn ensure_resolved_file_has_no_conflict_markers(
    repo: &Repository,
    path: &str,
    selected_side: bool,
    force: bool,
) -> Result<()> {
    if selected_side || force {
        return Ok(());
    }
    let full_path = repo.root().join(path);
    let content = fs::read(&full_path)
        .with_context(|| format!("read resolved conflict candidate {}", full_path.display()))?;
    if contains_conflict_markers(&content) {
        return Err(anyhow!(conflict_markers_still_present_advice(path)));
    }
    Ok(())
}

fn contains_conflict_markers(content: &[u8]) -> bool {
    content.split(|byte| *byte == b'\n').any(|line| {
        line.starts_with(b"<<<<<<<") || line.starts_with(b"=======") || line.starts_with(b">>>>>>>")
    })
}

fn resolve_file_with_version(
    repo: &Repository,
    merge_state: &MergeState,
    path: &str,
    ours: bool,
    theirs: bool,
) -> Result<()> {
    if !ours && !theirs {
        return Ok(());
    }

    let full_path = repo.root().join(path);

    if ours {
        let our_state = repo
            .store()
            .get_state(&merge_state.ours)?
            .ok_or_else(|| anyhow!("Our state not found"))?;
        let our_tree = repo.require_tree(&our_state.tree)?;

        if let Some(entry) = our_tree.get(path) {
            let blob = repo.require_blob(&entry.hash)?;
            fs::write(&full_path, blob.content())?;
        }
    } else if theirs {
        let their_state = repo
            .store()
            .get_state(&merge_state.theirs)?
            .ok_or_else(|| anyhow!("Their state not found"))?;
        let their_tree = repo.require_tree(&their_state.tree)?;

        if let Some(entry) = their_tree.get(path) {
            let blob = repo.require_blob(&entry.hash)?;
            fs::write(&full_path, blob.content())?;
        }
    }

    Ok(())
}

fn load_merge_state_or_advice(
    merge_manager: &repo::MergeStateManager,
    action: &'static str,
) -> Result<MergeState> {
    merge_manager
        .load()?
        .ok_or_else(|| anyhow!(no_merge_in_progress_advice(action)))
}

fn unresolved_paths(merge_state: &MergeState) -> Vec<String> {
    merge_state
        .conflicts
        .iter()
        .filter(|conflict| !merge_state.resolved.contains(conflict))
        .cloned()
        .collect()
}

fn no_merge_in_progress_advice(action: &'static str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "no_merge_in_progress",
        "No merge in progress",
        "Inspect the current operation state with `heddle status`.",
        "the repository has no persisted Heddle merge state",
        format!("{action} would need to read or update conflict state for an active merge"),
        "repository state was left unchanged",
        "heddle status",
        vec!["heddle status".to_string()],
    )
}

fn no_conflicts_to_resolve_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "no_conflicts_to_resolve",
        "No conflicts to resolve",
        "Inspect the current conflict set with `heddle resolve --list`.",
        "the active merge has no unresolved conflict paths",
        "resolve --all would not update any files or merge state",
        "repository state was left unchanged",
        "heddle resolve --list",
        vec!["heddle resolve --list".to_string()],
    )
}

fn path_not_in_active_merge_advice(path: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "conflict_path_not_found",
        format!("No active merge conflict is registered for {path}"),
        "Inspect unresolved conflicts with `heddle resolve --list`.",
        format!("{path} is not in the active merge conflict set"),
        "marking an unregistered path resolved would make the merge state disagree with the worktree",
        "repository state was left unchanged",
        "heddle resolve --list",
        vec!["heddle resolve --list".to_string()],
    )
}

fn conflict_markers_still_present_advice(path: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "conflict_markers_still_present",
        format!("Refusing to mark {path} resolved while conflict markers remain"),
        format!(
            "Edit {path} to remove `<<<<<<<`, `=======`, and `>>>>>>>`, then rerun `heddle resolve {path}`. Use `--ours`, `--theirs`, or `--force` only when intentional."
        ),
        format!("{path} still contains conflict marker lines"),
        "continuing the merge would capture unresolved marker text as the resolved file content",
        "the merge state, refs, objects, and worktree files were left unchanged",
        "heddle resolve --list".to_string(),
        vec![
            "heddle resolve --list".to_string(),
            format!("heddle resolve {path}"),
            format!("heddle resolve {path} --force"),
        ],
    )
}
