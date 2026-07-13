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
use heddle_core::{GitScope, SavePlan, SaveVerb, execute_save};
use objects::object::Attribution;
use repo::{GitCheckpointRecord, Repository, RepositoryCapability};
use serde::Serialize;

use super::{
    action_line::print_next,
    command_catalog::ActionTemplate,
    git_overlay_txn,
    snapshot::{SnapshotAgentOverrides, build_attribution},
    verification_health::RepositoryVerificationState,
    worktree_safety::dirty_worktree_advice,
};
use crate::{
    cli::{CheckpointArgs, Cli, should_output_json, style, worktree_status_options},
    config::UserConfig,
};

#[derive(Serialize)]
struct CheckpointOutput {
    output_kind: &'static str,
    status: &'static str,
    action: &'static str,
    state_id: String,
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
    let output = build_output(&repo, &state.state_id.short(), &record);

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!(
            "Checkpointed {} as Git commit {}",
            output.state_id,
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
    create_git_checkpoint_inner(repo, message, status_options, true, None)
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
    create_git_checkpoint_inner(repo, message, status_options, true, Some(worktree_status))
}

pub(crate) fn create_git_checkpoint_from_index_snapshot(
    repo: &Repository,
    message: Option<&str>,
    status_options: repo::WorktreeStatusOptions,
) -> Result<GitCheckpointRecord> {
    create_git_checkpoint_inner(repo, message, status_options, false, None)
}

/// Index-snapshot checkpoint helper retained for callers that still compose
/// capture and checkpoint as separate steps; commit uses `execute_save` now.
#[allow(dead_code)]
pub(crate) fn create_git_checkpoint_from_index_snapshot_with_worktree_status(
    repo: &Repository,
    message: Option<&str>,
    status_options: repo::WorktreeStatusOptions,
    worktree_status: &git_overlay_txn::GitOverlayWorktreeStatus,
) -> Result<GitCheckpointRecord> {
    create_git_checkpoint_inner(repo, message, status_options, false, Some(worktree_status))
}

fn create_git_checkpoint_inner(
    repo: &Repository,
    message: Option<&str>,
    status_options: repo::WorktreeStatusOptions,
    require_clean_worktree: bool,
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

    let user_config = UserConfig::load_default()?;

    // Fast path for an already-captured state: reuse an existing checkpoint
    // record and gate identity on the state's STORED principal, in main's
    // order — record-reuse first (a no-op checkpoint must not fail identity),
    // and never against an `unknown@example.com` fallback (which would let a
    // misconfigured identity slip a Git commit through). Bootstrap + new-state
    // creation fall through to `execute_save` below.
    if let Some(existing_state) = repo.current_state()? {
        if require_clean_worktree {
            let tree = repo.require_tree(&existing_state.tree)?;
            let status =
                repo.compare_worktree_cached_detailed_with_options(&tree, &status_options)?;
            if !status.is_clean() {
                return Err(anyhow!(dirty_worktree_advice(
                    "checkpoint",
                    &status,
                    "the current Heddle state was left unchanged; these paths have not been captured",
                )));
            }
        }
        if let Some(record) = repo.latest_git_checkpoint_for_change(&existing_state.state_id)? {
            return Ok(record);
        }
        git_overlay_txn::preflight_git_checkpoint_identity_for_principal(
            repo,
            &existing_state.attribution.principal,
            "checkpoint",
            "heddle checkpoint -m \"...\"",
        )?;
    }

    // Attribution for the `execute_save` plan. When bootstrapping a missing
    // Heddle state, resolve full attribution so the capture inherits
    // agent/principal rules; when reusing HEAD the plan reuses the current
    // state and this attribution is only a fallback identity for the commit.
    let attribution = if repo.current_state()?.is_some() {
        let principal = super::snapshot::resolve_principal(repo, &user_config)
            .unwrap_or_else(|_| objects::object::Principal::new("Unknown", "unknown@example.com"));
        Attribution::human(principal)
    } else {
        build_attribution(
            repo,
            &user_config,
            &SnapshotAgentOverrides {
                provider: None,
                model: None,
                session: None,
                segment: None,
                policy: None,
                no_policy: false,
                no_agent: false,
            },
        )?
    };

    let plan = SavePlan {
        verb: SaveVerb::Checkpoint,
        intent: message
            .map(ToOwned::to_owned)
            .or_else(|| Some("Bootstrap git-overlay before checkpoint".to_string())),
        confidence: None,
        attribution,
        git_scope: if require_clean_worktree {
            GitScope::WorktreeAll
        } else {
            GitScope::Staged
        },
        supplied_tree: None,
        reuse_current_state: true,
        require_clean_worktree,
        worktree_status_options: status_options,
        run_hooks: true,
        commit_safe_post_verify: false,
        coalesce_snapshot_and_checkpoint: false,
        precomputed_worktree_status: precomputed_worktree_status.map(|status| match status {
            Ok(Some(s)) => Ok(Some(objects::worktree::WorktreeStatus {
                modified: s.modified.clone(),
                added: s.added.clone(),
                deleted: s.deleted.clone(),
            })),
            Ok(None) => Ok(None),
            Err(err) => Err(objects::HeddleError::Config(err.to_string())),
        }),
    };

    let report = execute_save(repo, plan).map_err(|err| {
        // Preserve dirty-worktree RecoveryAdvice wording when the shared
        // primitive refuses a dirty checkpoint.
        if err
            .chain()
            .any(|cause| {
                cause
                    .downcast_ref::<objects::HeddleError>()
                    .is_some_and(|he| {
                        matches!(
                            he,
                            objects::HeddleError::Recovery(details) if details.kind == "dirty_worktree"
                        )
                    })
            })
        {
            // Recompute dirty status for the richer CLI advice path.
            if let Ok(Some(state)) = repo.current_state()
                && let Ok(tree) = repo.require_tree(&state.tree)
                && let Ok(status) =
                    repo.compare_worktree_cached_detailed_with_options(&tree, &status_options)
                && !status.is_clean()
            {
                return anyhow!(dirty_worktree_advice(
                    "checkpoint",
                    &status,
                    "the current Heddle state was left unchanged; these paths have not been captured",
                ));
            }
        }
        err
    })?;

    report
        .git_checkpoint
        .ok_or_else(|| anyhow!("checkpoint completed without a Git checkpoint record"))
}

fn build_output(
    repo: &Repository,
    state_id: &str,
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
        state_id: state_id.to_string(),
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
