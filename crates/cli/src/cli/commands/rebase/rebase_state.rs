// SPDX-License-Identifier: Apache-2.0
//! Rebase state persistence and commit-graph traversal.

use std::{fs, io::Write};

use anyhow::{Result, anyhow};
use objects::object::ChangeId;
use repo::Repository;

#[derive(Debug, Clone)]
pub(crate) struct RebaseState {
    pub(crate) onto: ChangeId,
    pub(crate) commits_to_replay: Vec<ChangeId>,
    pub(crate) current_index: usize,
    pub(crate) original_head: ChangeId,
    pub(crate) pending_manual_resolution: Option<ChangeId>,
    pub(crate) pre_conflict_head: Option<ChangeId>,
}

pub(crate) fn collect_commits_to_rebase(
    repo: &Repository,
    current_head: &ChangeId,
    onto: &ChangeId,
) -> Result<Vec<ChangeId>> {
    let mut commits = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut current = *current_head;

    while visited.insert(current) {
        if current == *onto {
            break;
        }

        if is_ancestor_of(repo, &current, onto)? {
            break;
        }

        commits.push(current);

        let state = match repo.store().get_state(&current)? {
            Some(s) => s,
            None => break,
        };

        match state.parents.first() {
            Some(parent) => current = *parent,
            None => break,
        }
    }

    commits.reverse();
    Ok(commits)
}

pub(crate) fn is_ancestor_of(
    repo: &Repository,
    potential_ancestor: &ChangeId,
    descendant: &ChangeId,
) -> Result<bool> {
    Ok(proto::is_ancestor(
        repo.store(),
        *potential_ancestor,
        *descendant,
    )?)
}

pub(crate) fn save_rebase_state(path: &std::path::Path, state: &RebaseState) -> Result<()> {
    let mut content = String::new();
    content.push_str(&format!("onto={}\n", state.onto.to_string_full()));
    content.push_str(&format!(
        "original_head={}\n",
        state.original_head.to_string_full()
    ));
    content.push_str(&format!("current_index={}\n", state.current_index));
    if let Some(commit) = state.pending_manual_resolution {
        content.push_str(&format!(
            "pending_manual_resolution={}\n",
            commit.to_string_full()
        ));
    }
    if let Some(head) = state.pre_conflict_head {
        content.push_str(&format!("pre_conflict_head={}\n", head.to_string_full()));
    }
    content.push_str("commits=");
    for (i, commit) in state.commits_to_replay.iter().enumerate() {
        if i > 0 {
            content.push(',');
        }
        content.push_str(&commit.to_string_full());
    }
    content.push('\n');

    let mut file = fs::File::create(path)?;
    file.write_all(content.as_bytes())?;

    Ok(())
}

pub(crate) fn load_rebase_state(path: &std::path::Path) -> Result<RebaseState> {
    let content = fs::read_to_string(path)?;

    let mut onto = None;
    let mut original_head = None;
    let mut current_index = 0;
    let mut commits_to_replay = Vec::new();
    let mut pending_manual_resolution = None;
    let mut pre_conflict_head = None;

    for line in content.lines() {
        if let Some(value) = line.strip_prefix("onto=") {
            onto = Some(ChangeId::parse(value)?);
        } else if let Some(value) = line.strip_prefix("original_head=") {
            original_head = Some(ChangeId::parse(value)?);
        } else if let Some(value) = line.strip_prefix("current_index=") {
            current_index = value.parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("pending_manual_resolution=") {
            pending_manual_resolution = Some(ChangeId::parse(value)?);
        } else if let Some(value) = line.strip_prefix("pre_conflict_head=") {
            pre_conflict_head = Some(ChangeId::parse(value)?);
        } else if let Some(value) = line.strip_prefix("commits=") {
            for commit_str in value.split(',') {
                if !commit_str.is_empty() {
                    commits_to_replay.push(ChangeId::parse(commit_str)?);
                }
            }
        }
    }

    Ok(RebaseState {
        onto: onto.ok_or_else(|| anyhow!("Missing 'onto' in rebase state"))?,
        original_head: original_head
            .ok_or_else(|| anyhow!("Missing 'original_head' in rebase state"))?,
        current_index,
        commits_to_replay,
        pending_manual_resolution,
        pre_conflict_head,
    })
}
