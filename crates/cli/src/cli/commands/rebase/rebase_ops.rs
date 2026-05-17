// SPDX-License-Identifier: Apache-2.0
//! Rebase operation execution — applying commits onto a new base.

use std::fs;

use anyhow::{Result, anyhow};
use objects::object::{Blob, ChangeId, ContentHash, EntryType, State};
use repo::Repository;

use super::rebase_state::{load_rebase_state, save_rebase_state};
use crate::cli::{Cli, should_output_json};

pub(super) fn replay_commits(
    repo: &Repository,
    rebase_state_path: &std::path::Path,
    cli: &Cli,
) -> Result<()> {
    replay_commits_internal(repo, rebase_state_path, Some(cli))
}

pub(super) fn replay_commits_silent(
    repo: &Repository,
    rebase_state_path: &std::path::Path,
) -> Result<()> {
    replay_commits_internal(repo, rebase_state_path, None)
}

fn replay_commits_internal(
    repo: &Repository,
    rebase_state_path: &std::path::Path,
    cli: Option<&Cli>,
) -> Result<()> {
    let mut state = load_rebase_state(rebase_state_path)?;
    resume_manual_resolution_if_present(repo, &mut state, rebase_state_path, cli)?;

    let mut current_head = if state.current_index == 0 {
        state.onto
    } else {
        repo.current_state()?
            .ok_or_else(|| anyhow!("No current state"))?
            .change_id
    };

    while state.current_index < state.commits_to_replay.len() {
        let commit_id = state.commits_to_replay[state.current_index];
        let commit_state = repo
            .store()
            .get_state(&commit_id)?
            .ok_or_else(|| anyhow!("Commit {} not found", commit_id))?;

        if let Some(cli) = cli
            && should_output_json(cli, Some(repo.config()))
        {
            println!(
                "{{\"status\": \"applying\", \"commit\": \"{}\"}}",
                commit_id.short()
            );
        } else if cli.is_some() {
            println!("Applying {}...", commit_id.short());
        }

        let result = apply_commit(repo, &commit_state, &current_head)?;

        match result {
            ApplyResult::Success(new_head) => {
                current_head = new_head;
                state.current_index += 1;
                state.pending_manual_resolution = None;
                state.pre_conflict_head = None;
                save_rebase_state(rebase_state_path, &state)?;
            }
            ApplyResult::Conflict => {
                state.pending_manual_resolution = Some(commit_id);
                state.pre_conflict_head = Some(current_head);
                save_rebase_state(rebase_state_path, &state)?;
                if let Some(cli) = cli
                    && should_output_json(cli, Some(repo.config()))
                {
                    println!(
                        "{{\"status\": \"conflict\", \"commit\": \"{}\"}}",
                        commit_id.short()
                    );
                } else if cli.is_some() {
                    println!(
                        "Conflict applying {}. Fix conflicts and run 'heddle rebase --continue'",
                        commit_id.short()
                    );
                }
                return Ok(());
            }
        }
    }

    fs::remove_file(rebase_state_path)?;

    if let Some(cli) = cli
        && should_output_json(cli, Some(repo.config()))
    {
        println!(
            "{{\"status\": \"completed\", \"onto\": \"{}\"}}",
            state.onto
        );
    } else if cli.is_some() {
        println!("Rebase completed");
    }

    Ok(())
}

fn resume_manual_resolution_if_present(
    repo: &Repository,
    state: &mut super::rebase_state::RebaseState,
    rebase_state_path: &std::path::Path,
    cli: Option<&Cli>,
) -> Result<()> {
    let Some(pending_commit) = state.pending_manual_resolution else {
        return Ok(());
    };
    let Some(pre_conflict_head) = state.pre_conflict_head else {
        return Ok(());
    };

    let current_state = repo
        .current_state()?
        .ok_or_else(|| anyhow!("No current state"))?;

    if current_state.change_id == pre_conflict_head || current_state.change_id == pending_commit {
        if let Some(cli) = cli
            && should_output_json(cli, Some(repo.config()))
        {
            println!(
                "{{\"status\": \"conflict\", \"commit\": \"{}\"}}",
                pending_commit.short()
            );
        } else if cli.is_some() {
            println!(
                "Conflict applying {}. Capture a manual resolution, then run 'heddle rebase --continue'",
                pending_commit.short()
            );
        }
        return Ok(());
    }

    state.current_index += 1;
    state.pending_manual_resolution = None;
    state.pre_conflict_head = None;
    save_rebase_state(rebase_state_path, state)?;

    if let Some(cli) = cli
        && should_output_json(cli, Some(repo.config()))
    {
        println!(
            "{{\"status\": \"manual-resolution-accepted\", \"commit\": \"{}\", \"resolved_as\": \"{}\"}}",
            pending_commit.short(),
            current_state.change_id.short()
        );
    } else if cli.is_some() {
        println!(
            "Accepted manual resolution for {} as {}",
            pending_commit.short(),
            current_state.change_id.short()
        );
    }

    Ok(())
}

enum ApplyResult {
    Success(ChangeId),
    Conflict,
}

fn apply_commit(
    repo: &Repository,
    commit_state: &objects::object::State,
    current_head: &ChangeId,
) -> Result<ApplyResult> {
    let current_tree_hash = get_tree_for_state(repo, current_head)?;
    let commit_tree_hash = commit_state.tree;

    let current_tree = repo
        .store()
        .get_tree(&current_tree_hash)?
        .ok_or_else(|| anyhow!("Current tree not found"))?;

    let commit_tree = repo
        .store()
        .get_tree(&commit_tree_hash)?
        .ok_or_else(|| anyhow!("Commit tree not found"))?;

    let parent_tree_hash = if let Some(parent_id) = commit_state.parents.first() {
        get_tree_for_state(repo, parent_id)?
    } else {
        return apply_tree_to_worktree(repo, commit_state, &commit_tree, current_head);
    };

    let parent_tree = repo
        .store()
        .get_tree(&parent_tree_hash)?
        .ok_or_else(|| anyhow!("Parent tree not found"))?;

    let changes = compute_tree_diff(&parent_tree, &commit_tree);

    let mut has_conflicts = false;
    let mut updated_entries: Vec<(String, objects::object::EntryType, ContentHash)> = Vec::new();
    let mut deleted_paths: Vec<String> = Vec::new();

    for (path, change) in changes {
        match change {
            TreeChange::Added(entry_type, hash) => {
                if let Some(existing) = find_entry_in_tree(&current_tree, &path) {
                    if existing.1 != hash {
                        has_conflicts = true;
                    }
                } else {
                    updated_entries.push((path, entry_type, hash));
                }
            }
            TreeChange::Modified(entry_type, hash) => {
                if let Some(existing) = find_entry_in_tree(&current_tree, &path)
                    && existing.1 == hash
                {
                    continue;
                }
                if let Some(parent) = find_entry_in_tree(&parent_tree, &path)
                    && let Some(existing) = find_entry_in_tree(&current_tree, &path)
                    && existing.1 != parent.1
                    && existing.1 != hash
                {
                    if let Some(merged_hash) =
                        try_auto_merge_textual_change(repo, &parent.1, &existing.1, &hash)?
                    {
                        updated_entries.push((path, EntryType::Blob, merged_hash));
                        continue;
                    }
                    if blob_contains_both(repo, &hash, &existing.1, &parent.1)? {
                        updated_entries.push((path, entry_type, hash));
                        continue;
                    }
                    has_conflicts = true;
                    continue;
                }
                updated_entries.push((path, entry_type, hash));
            }
            TreeChange::Deleted => {
                if let Some(existing) = find_entry_in_tree(&current_tree, &path) {
                    if let Some(parent) = find_entry_in_tree(&parent_tree, &path)
                        && existing.1 != parent.1
                    {
                        has_conflicts = true;
                        continue;
                    }
                    deleted_paths.push(path);
                }
            }
        }
    }

    if has_conflicts {
        return Ok(ApplyResult::Conflict);
    }

    let new_tree = apply_changes_to_tree(&current_tree, &updated_entries, &deleted_paths)?;
    let new_tree_hash = repo.store().put_tree(&new_tree)?;

    let new_state = State::new_refresh_of(
        new_tree_hash,
        vec![*current_head],
        commit_state.attribution.clone(),
        commit_state.logical_change_id(),
    )
    .with_intent(
        commit_state
            .intent
            .clone()
            .unwrap_or_else(|| "rebase".to_string()),
    )
    .with_status(commit_state.status);

    let new_state = copy_state_metadata(new_state, commit_state);

    let new_state_id = new_state.change_id;
    repo.store().put_state(&new_state)?;
    repo.fast_forward_attached(&new_state_id)?;

    Ok(ApplyResult::Success(new_state_id))
}

fn try_auto_merge_textual_change(
    repo: &Repository,
    base: &ContentHash,
    current: &ContentHash,
    incoming: &ContentHash,
) -> Result<Option<ContentHash>> {
    let Some(base_blob) = repo.store().get_blob(base)? else {
        return Ok(None);
    };
    let Some(current_blob) = repo.store().get_blob(current)? else {
        return Ok(None);
    };
    let Some(incoming_blob) = repo.store().get_blob(incoming)? else {
        return Ok(None);
    };

    let Some(merged) = auto_merge_text_lines(
        base_blob.content(),
        current_blob.content(),
        incoming_blob.content(),
    )?
    else {
        return Ok(None);
    };

    let hash = repo.store().put_blob(&Blob::new(merged))?;
    Ok(Some(hash))
}

/// Replay-time auto-merge of three line-level byte slices.
///
/// Returns `Some(bytes)` only when the merge resolves *cleanly* — any
/// conflict (including binary, delete/modify, or overlapping hunks) returns
/// `None`, leaving the rebase caller to either invoke `blob_contains_both`
/// or stop with `ApplyResult::Conflict`. With the `semantic` feature on
/// (the default), this routes through the native hunk-level merger added
/// in heddle#79, so multi-hunk-per-side edits auto-resolve when disjoint —
/// directly addressing the heddle#54 trip report's rebase failure mode.
#[cfg(feature = "semantic")]
fn auto_merge_text_lines(base: &[u8], current: &[u8], incoming: &[u8]) -> Result<Option<Vec<u8>>> {
    use semantic::merge::{MergeOutcome, text_hunk_merge};
    match text_hunk_merge(base, current, incoming) {
        MergeOutcome::Clean(bytes) => Ok(Some(bytes)),
        MergeOutcome::Conflicts { .. }
        | MergeOutcome::Binary
        | MergeOutcome::DeleteVsModify => Ok(None),
    }
}

#[cfg(not(feature = "semantic"))]
fn auto_merge_text_lines(
    _base: &[u8],
    _current: &[u8],
    _incoming: &[u8],
) -> Result<Option<Vec<u8>>> {
    // No semantic feature → no native hunk-level merge engine. Decline
    // auto-merge; caller falls back to `blob_contains_both` or marks
    // the file conflicted.
    Ok(None)
}

fn blob_contains_both(
    repo: &Repository,
    candidate: &ContentHash,
    current: &ContentHash,
    parent: &ContentHash,
) -> Result<bool> {
    let Some(candidate_blob) = repo.store().get_blob(candidate)? else {
        return Ok(false);
    };
    let Some(current_blob) = repo.store().get_blob(current)? else {
        return Ok(false);
    };
    let Some(parent_blob) = repo.store().get_blob(parent)? else {
        return Ok(false);
    };
    let candidate = candidate_blob.content();
    Ok(!current_blob.content().is_empty()
        && !parent_blob.content().is_empty()
        && contains_bytes(candidate, current_blob.content())
        && contains_bytes(candidate, parent_blob.content()))
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn apply_tree_to_worktree(
    repo: &Repository,
    commit_state: &objects::object::State,
    tree: &objects::object::Tree,
    current_head: &ChangeId,
) -> Result<ApplyResult> {
    let tree_hash = repo.store().put_tree(tree)?;
    let new_state = State::new_refresh_of(
        tree_hash,
        vec![*current_head],
        commit_state.attribution.clone(),
        commit_state.logical_change_id(),
    )
    .with_intent(
        commit_state
            .intent
            .clone()
            .unwrap_or_else(|| "rebase".to_string()),
    )
    .with_status(commit_state.status);
    let new_state = copy_state_metadata(new_state, commit_state);

    let new_state_id = new_state.change_id;
    repo.store().put_state(&new_state)?;
    repo.fast_forward_attached(&new_state_id)?;

    Ok(ApplyResult::Success(new_state_id))
}

fn copy_state_metadata(
    mut rebased_state: objects::object::State,
    source_state: &objects::object::State,
) -> objects::object::State {
    if let Some(confidence) = source_state.confidence {
        rebased_state = rebased_state.with_confidence(confidence);
    }
    if let Some(verification) = source_state.verification.clone() {
        rebased_state = rebased_state.with_verification(verification);
    }
    if let Some(provenance) = source_state.provenance {
        rebased_state = rebased_state.with_provenance(provenance);
    }
    if let Some(context) = source_state.context {
        rebased_state = rebased_state.with_context(context);
    }
    rebased_state
}

fn get_tree_for_state(repo: &Repository, state_id: &ChangeId) -> Result<ContentHash> {
    let state = repo
        .store()
        .get_state(state_id)?
        .ok_or_else(|| anyhow!("State {} not found", state_id))?;
    Ok(state.tree)
}

#[derive(Clone)]
enum TreeChange {
    Added(objects::object::EntryType, ContentHash),
    Modified(objects::object::EntryType, ContentHash),
    Deleted,
}

fn compute_tree_diff(
    old_tree: &objects::object::Tree,
    new_tree: &objects::object::Tree,
) -> Vec<(String, TreeChange)> {
    let mut changes = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in new_tree.entries() {
        seen.insert(entry.name.clone());
        let old_entry = old_tree.entries().iter().find(|e| e.name == entry.name);
        match old_entry {
            Some(old) => {
                if old.hash != entry.hash {
                    changes.push((
                        entry.name.clone(),
                        TreeChange::Modified(entry.entry_type, entry.hash),
                    ));
                }
            }
            None => {
                changes.push((
                    entry.name.clone(),
                    TreeChange::Added(entry.entry_type, entry.hash),
                ));
            }
        }
    }

    for entry in old_tree.entries() {
        if !seen.contains(&entry.name) {
            changes.push((entry.name.clone(), TreeChange::Deleted));
        }
    }

    changes
}

fn find_entry_in_tree(
    tree: &objects::object::Tree,
    name: &str,
) -> Option<(objects::object::EntryType, ContentHash)> {
    for entry in tree.entries() {
        if entry.name == name {
            return Some((entry.entry_type, entry.hash));
        }
    }
    None
}

fn apply_changes_to_tree(
    base_tree: &objects::object::Tree,
    updates: &[(String, objects::object::EntryType, ContentHash)],
    deletions: &[String],
) -> Result<objects::object::Tree> {
    let mut entries: Vec<objects::object::TreeEntry> = base_tree.entries().to_vec();

    for name in deletions {
        entries.retain(|e| &e.name != name);
    }

    for (name, entry_type, hash) in updates {
        entries.retain(|e| &e.name != name);
        let new_entry = match entry_type {
            objects::object::EntryType::Blob => {
                objects::object::TreeEntry::file(name.clone(), *hash, false)?
            }
            objects::object::EntryType::Tree => {
                objects::object::TreeEntry::directory(name.clone(), *hash)?
            }
            objects::object::EntryType::Symlink => {
                objects::object::TreeEntry::symlink(name.clone(), *hash)?
            }
        };
        entries.push(new_entry);
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(objects::object::Tree::from_entries(entries))
}