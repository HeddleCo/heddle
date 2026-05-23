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

use std::process::Command;

use anyhow::{Result, anyhow, bail};
use oplog::OpRecord;
use repo::{GitCheckpointRecord, Repository, RepositoryCapability};
use serde::Serialize;

use super::{
    advice::RecoveryAdvice, snapshot::ensure_current_state, worktree_safety::dirty_worktree_advice,
};
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
    let status = repo.compare_worktree_cached_detailed_with_options(&tree, &status_options)?;
    if !status.is_clean() {
        return Err(anyhow!(dirty_worktree_advice(
            "checkpoint",
            &status,
            "the current Heddle state was left unchanged; these paths have not been captured",
        )));
    }

    let summary = message
        .map(ToOwned::to_owned)
        .or_else(|| state.intent.clone())
        .unwrap_or_else(|| format!("Checkpoint {}", state.change_id.short()));
    let branch = repo
        .git_overlay_current_branch()?
        .unwrap_or_else(|| "HEAD".to_string());
    let previous_git_oid = git_rev_parse_head(repo.root());
    let mut bridge = GitBridge::new(repo);
    let git_commit = match bridge.write_through_current_checkout()? {
        WriteThroughOutcome::Wrote(git_commit) => git_commit.to_string(),
        WriteThroughOutcome::Skipped(reason) => {
            return Err(anyhow!(checkpoint_git_write_skipped_advice(
                reason.to_string()
            )));
        }
    };
    let record = repo.record_git_checkpoint(&state.change_id, git_commit.clone(), summary)?;
    repo.oplog().record_batch_scoped(
        vec![OpRecord::GitCheckpoint {
            branch,
            state: state.change_id,
            previous_git_oid,
            new_git_oid: git_commit,
        }],
        Some(&repo.op_scope()),
    )?;
    Ok(record)
}

fn checkpoint_git_write_skipped_advice(reason: String) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "checkpoint_git_write_skipped",
        format!("Checkpoint could not update the Git checkout: {reason}"),
        "Resolve the Git checkout issue, then retry with `heddle checkpoint -m \"...\"`.",
        reason,
        "checkpoint would need to write the current Heddle state into the Git branch and index",
        "the current Heddle state was preserved; no Git checkpoint record was written",
        "heddle checkpoint -m \"...\"",
        vec!["heddle checkpoint -m \"...\"".to_string()],
    )
}

fn git_rev_parse_head(root: &std::path::Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!oid.is_empty()).then_some(oid)
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
