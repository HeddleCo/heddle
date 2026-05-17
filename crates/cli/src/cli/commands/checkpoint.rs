// SPDX-License-Identifier: Apache-2.0
//! `heddle checkpoint` — Git-facing commit boundary.
//!
//! Our git-overlay model treats captures as granular sub-commit
//! provenance and checkpoints as the Git commit equivalent that
//! syncs the current Heddle state to the Git ref via the bridge.
//!
//! Resolved against main's "A11 cheap save" variant: the lightweight
//! save semantic is already covered by `heddle capture`, so we keep
//! the Git-overlay checkpoint that the shakedown doc and the OSS
//! launch claim are built around. The function is renamed to `run`
//! to match main's `pub use checkpoint::run as cmd_checkpoint;`
//! convention.

use anyhow::{Result, anyhow, bail};
use repo::{GitCheckpointRecord, Repository, RepositoryCapability};
use serde::Serialize;

use super::snapshot::ensure_current_state;
use crate::{
    bridge::{GitBridge, WriteThroughOutcome},
    cli::{CheckpointArgs, Cli, should_output_json, worktree_status_options},
    config::UserConfig,
};

#[derive(Serialize)]
struct CheckpointOutput {
    change_id: String,
    git_commit: String,
    summary: String,
    capability: String,
    storage_model: String,
    committed_at: String,
}

pub async fn run(cli: &Cli, args: &CheckpointArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let record = create_git_checkpoint(
        &repo,
        args.message.as_deref(),
        worktree_status_options(Some(repo.config())),
    )?;
    let state = repo
        .current_state()?
        .ok_or_else(|| anyhow!("no captured state found after checkpoint"))?;
    let output = build_output(&repo, &state.change_id.short(), &record);

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!(
            "Checkpointed {} as Git commit {}",
            output.change_id,
            &output.git_commit[..std::cmp::min(12, output.git_commit.len())]
        );
        println!("Storage: {}", output.storage_model);
    }

    Ok(())
}

pub(crate) fn create_git_checkpoint(
    repo: &Repository,
    message: Option<&str>,
    status_options: repo::WorktreeStatusOptions,
) -> Result<GitCheckpointRecord> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        bail!(
            "`heddle checkpoint` is available in Git-backed repositories. This repository is using {} storage.",
            repo.storage_model_label()
        );
    }

    let state_id = ensure_current_state(
        repo,
        &UserConfig::load_default().unwrap_or_default(),
        message
            .map(ToOwned::to_owned)
            .or_else(|| Some("Bootstrap git-overlay before checkpoint".to_string())),
    )?;
    let state = repo
        .store()
        .get_state(&state_id)?
        .ok_or_else(|| anyhow!("no captured state found after bootstrap"))?;
    let tree = repo.require_tree(&state.tree)?;
    let status = repo.compare_worktree_cached_with_options(&tree, &status_options)?;
    if !status.modified.is_empty() || !status.added.is_empty() || !status.deleted.is_empty() {
        bail!(
            "worktree has changes not yet captured. Run `heddle capture -m \"...\"` before checkpointing."
        );
    }

    let summary = message
        .map(ToOwned::to_owned)
        .or_else(|| state.intent.clone())
        .unwrap_or_else(|| format!("Checkpoint {}", state.change_id.short()));
    let mut bridge = GitBridge::new(repo);
    let git_commit = match bridge.write_through_current_checkout()? {
        WriteThroughOutcome::Wrote(git_commit) => git_commit.to_string(),
        WriteThroughOutcome::Skipped(reason) => {
            bail!("checkpoint could not update the Git checkout: {reason}")
        }
    };
    Ok(repo.record_git_checkpoint(&state.change_id, git_commit, summary)?)
}

fn build_output(
    repo: &Repository,
    change_id: &str,
    record: &GitCheckpointRecord,
) -> CheckpointOutput {
    CheckpointOutput {
        change_id: change_id.to_string(),
        git_commit: record.git_commit.clone(),
        summary: record.summary.clone(),
        capability: repo.capability_label().to_string(),
        storage_model: repo.storage_model_label().to_string(),
        committed_at: record.committed_at.clone(),
    }
}