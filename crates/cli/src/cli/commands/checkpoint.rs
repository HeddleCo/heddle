// SPDX-License-Identifier: Apache-2.0
//! `heddle checkpoint` — Git-facing commit boundary.
//!
//! Our git-overlay model treats captures as granular sub-commit
//! provenance and checkpoints as the Git commit equivalent that
//! syncs the current Heddle state to the Git ref through Git projection.
//!
//! Resolved against main's "A11 cheap save" variant: the lightweight
//! save semantic is already covered by `heddle capture`, so we keep
//! the Git-overlay checkpoint that the shakedown doc and the OSS
//! launch claim are built around. The function is renamed to `run`
//! to match main's `pub use checkpoint::run as cmd_checkpoint;`
//! convention.

use anyhow::{Result, anyhow};
use objects::store::ObjectStore;
use oplog::{OpLogBackend, OpRecord};
use repo::{GitCheckpointRecord, Repository, RepositoryCapability};
use serde::Serialize;
use sley::{ObjectId, Repository as SleyRepository};

use super::{
    action_line::print_next, command_catalog::ActionTemplate, git_overlay_txn,
    snapshot::ensure_current_state, verification_health::RepositoryVerificationState,
    worktree_safety::dirty_worktree_advice,
};
use crate::{
    cli::{CheckpointArgs, Cli, should_output_json, style, worktree_status_options},
    config::UserConfig,
    git_projection_engine::{GitProjection, WriteThroughOutcome},
};

#[derive(Serialize)]
struct CheckpointOutput {
    output_kind: &'static str,
    status: &'static str,
    action: &'static str,
    change_id: String,
    git_commit: String,
    summary: String,
    capability: String,
    storage_model: String,
    committed_at: String,
    next_action: Option<String>,
    next_action_template: Option<ActionTemplate>,
    recommended_action: Option<String>,
    recommended_action_template: Option<ActionTemplate>,
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
}

pub async fn run(cli: &Cli, args: &CheckpointArgs) -> Result<()> {
    let cwd;
    let start = if let Some(path) = cli.repo.as_ref() {
        path
    } else {
        cwd = std::env::current_dir()?;
        &cwd
    };
    git_overlay_txn::preflight_plain_git_mutation(start, "checkpoint")?;

    let repo = Repository::open(start)?;
    let status_options = worktree_status_options(Some(repo.config()));
    let record = if args.from_index_snapshot {
        create_git_checkpoint_from_index_snapshot(&repo, args.message.as_deref(), status_options)?
    } else {
        create_git_checkpoint(&repo, args.message.as_deref(), status_options)?
    };
    let state = repo
        .current_state()?
        .ok_or_else(|| anyhow!("no captured state found after checkpoint"))?;
    // NOTE: `build_output` recomputes the verification state from scratch — it
    // must NOT reuse the pre-checkpoint worktree status. The checkpoint just
    // advanced the Git ref, which flips the git-overlay health from
    // `needs_checkpoint` to `clean` (and remote drift from diverged to ahead);
    // the post-mutation output reflects the NEW git state, so the status walk
    // here is a different, necessary one. The redundant-walk elimination is
    // scoped to the PRE-mutation consumers inside `create_git_checkpoint_inner`.
    let output = build_output(&repo, &state.change_id.short(), &record);

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!(
            "Checkpointed {} as Git commit {}",
            output.change_id,
            &output.git_commit[..std::cmp::min(12, output.git_commit.len())]
        );
        if output.trust.verified {
            println!("Verification: {}", style::accent("clean"));
        } else if !output.trust.recommended_action.is_empty() {
            print_next(&output.trust.recommended_action);
        }
    }

    Ok(())
}

pub(crate) fn create_git_checkpoint(
    repo: &Repository,
    message: Option<&str>,
    status_options: repo::WorktreeStatusOptions,
) -> Result<GitCheckpointRecord> {
    create_git_checkpoint_inner(repo, message, status_options, true, None, None)
}

/// Variant of [`create_git_checkpoint`] that reuses an already-computed
/// git-overlay worktree status for checkpoint's two PRE-mutation preflights
/// instead of re-walking the worktree. Used by `commit`, which has already
/// computed the same pre-mutation status for its own preflights — no Git
/// mutation happens between commit's walk and checkpoint's preflights, so they
/// observe the same git state and the gating decision is byte-identical to
/// [`create_git_checkpoint`].
pub(crate) fn create_git_checkpoint_with_worktree_status(
    repo: &Repository,
    message: Option<&str>,
    status_options: repo::WorktreeStatusOptions,
    worktree_status: &git_overlay_txn::GitOverlayWorktreeStatus,
) -> Result<GitCheckpointRecord> {
    create_git_checkpoint_inner(
        repo,
        message,
        status_options,
        true,
        None,
        Some(worktree_status),
    )
}

pub(crate) fn create_git_checkpoint_from_index_snapshot(
    repo: &Repository,
    message: Option<&str>,
    status_options: repo::WorktreeStatusOptions,
) -> Result<GitCheckpointRecord> {
    create_git_checkpoint_inner(repo, message, status_options, false, None, None)
}

pub(crate) fn create_git_checkpoint_from_index_snapshot_with_worktree_status(
    repo: &Repository,
    message: Option<&str>,
    status_options: repo::WorktreeStatusOptions,
    worktree_status: &git_overlay_txn::GitOverlayWorktreeStatus,
) -> Result<GitCheckpointRecord> {
    create_git_checkpoint_inner(
        repo,
        message,
        status_options,
        false,
        None,
        Some(worktree_status),
    )
}

fn create_git_checkpoint_inner(
    repo: &Repository,
    message: Option<&str>,
    status_options: repo::WorktreeStatusOptions,
    require_clean_worktree: bool,
    git_parent_override: Option<Vec<ObjectId>>,
    precomputed_worktree_status: Option<&git_overlay_txn::GitOverlayWorktreeStatus>,
) -> Result<GitCheckpointRecord> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Err(anyhow!(
            git_overlay_txn::native_checkpoint_unavailable_advice(repo)
        ));
    }
    // Compute the git-overlay worktree status ONCE up front and thread it through
    // the two PRE-mutation consumers below: the ref-update preflight and the
    // verification preflight. Both build the repository verification state, which
    // runs `git_overlay_worktree_status` — a walk that re-reads + SHA-1s every
    // tracked file. Before this, checkpoint paid that walk twice here (plus a
    // third in `build_output`, which must stay a FRESH walk because it runs AFTER
    // the checkpoint advances the Git ref — see `run`). Threading the exact
    // `Result` keeps the clean/dirty classification byte-identical, and both
    // consumers observe the SAME pre-mutation git state, so reuse is sound.
    // A caller that has already computed this pre-mutation status (e.g. `commit`)
    // passes it in so checkpoint does not re-walk the worktree.
    match precomputed_worktree_status {
        Some(status) => {
            git_overlay_txn::preflight_checkpoint_with_worktree_status(repo, "checkpoint", status)?
        }
        None => {
            let facts = git_overlay_txn::gather_mutation_facts(repo);
            git_overlay_txn::preflight_checkpoint(repo, "checkpoint", &facts)?;
        }
    };
    let state_id = ensure_current_state(
        repo,
        &UserConfig::load_default()?,
        message
            .map(ToOwned::to_owned)
            .or_else(|| Some("Bootstrap git-overlay before checkpoint".to_string())),
    )?;
    let state = repo
        .store()
        .get_state(&state_id)?
        .ok_or_else(|| anyhow!("no captured state found after bootstrap"))?;
    if require_clean_worktree {
        let tree = repo.require_tree(&state.tree)?;
        let status = repo.compare_worktree_cached_detailed_with_options(&tree, &status_options)?;
        if !status.is_clean() {
            return Err(anyhow!(dirty_worktree_advice(
                "checkpoint",
                &status,
                "the current Heddle state was left unchanged; these paths have not been captured",
            )));
        }
    }
    if let Some(record) = repo.latest_git_checkpoint_for_change(&state.change_id)? {
        return Ok(record);
    }
    git_overlay_txn::preflight_git_checkpoint_identity_for_principal(
        repo,
        &state.attribution.principal,
        "checkpoint",
        "heddle checkpoint -m \"...\"",
    )?;

    let summary = message
        .map(ToOwned::to_owned)
        .or_else(|| state.intent.clone())
        .unwrap_or_else(|| format!("Checkpoint {}", state.change_id.short()));
    let branch = repo
        .git_overlay_current_branch()?
        .unwrap_or_else(|| "HEAD".to_string());
    let previous_git_oid = git_rev_parse_head(repo.root());
    let mut bridge = GitProjection::new(repo);
    if let Some(parents) = git_parent_override {
        bridge.set_commit_parent_override(state.change_id, parents);
    }
    let git_commit = match bridge
        .write_through_current_checkout_with_message(state.change_id, summary.clone())?
    {
        WriteThroughOutcome::Wrote(git_commit) => git_commit.to_string(),
        WriteThroughOutcome::Skipped(reason) => {
            return Err(anyhow!(
                git_overlay_txn::checkpoint_git_write_skipped_advice(reason.to_string())
            ));
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

fn git_rev_parse_head(root: &std::path::Path) -> Option<String> {
    let git = SleyRepository::discover(root).ok()?;
    git.head().ok()?.oid.map(|id| id.to_string())
}

fn build_output(
    repo: &Repository,
    change_id: &str,
    record: &GitCheckpointRecord,
) -> CheckpointOutput {
    // Fresh verification state: this runs AFTER the checkpoint advanced the Git
    // ref, so it must re-read the new git-overlay state (do NOT reuse the
    // pre-checkpoint worktree status threaded into the preflights above).
    let trust = git_overlay_txn::post_verify(repo);
    let recommended_action = action_value(&trust);
    CheckpointOutput {
        output_kind: "checkpoint",
        status: "checkpointed",
        action: "checkpoint",
        change_id: change_id.to_string(),
        git_commit: record.git_commit.clone(),
        summary: record.summary.clone(),
        capability: repo.capability_label().to_string(),
        storage_model: repo.storage_model_label().to_string(),
        committed_at: record.committed_at.clone(),
        next_action: recommended_action.clone(),
        next_action_template: trust.recommended_action_template.clone(),
        recommended_action,
        recommended_action_template: trust.recommended_action_template.clone(),
        trust,
    }
}

fn action_value(trust: &RepositoryVerificationState) -> Option<String> {
    (!trust.recommended_action.trim().is_empty()).then(|| trust.recommended_action.clone())
}
