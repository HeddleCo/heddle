// SPDX-License-Identifier: Apache-2.0
//! Internal Git-checkpoint implementation used by thread landing.

use anyhow::{Result, anyhow};
use heddle_core::{GitScope, SavePlan, SaveVerb, execute_save};
use objects::object::Attribution;
use repo::{GitCheckpointRecord, Repository, RepositoryCapability};

use super::{
    git_overlay_txn,
    snapshot::{SnapshotAgentOverrides, build_attribution},
    worktree_safety::dirty_worktree_advice,
};
use crate::config::UserConfig;

pub(crate) fn create_git_checkpoint(
    repo: &Repository,
    message: Option<&str>,
    status_options: repo::WorktreeStatusOptions,
) -> Result<GitCheckpointRecord> {
    create_git_checkpoint_inner(repo, message, status_options, true, None)
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
        if let Some(record) = repo.latest_git_checkpoint_for_change(&existing_state.change_id)? {
            return Ok(record);
        }
        git_overlay_txn::preflight_git_checkpoint_identity_for_principal(
            repo,
            &existing_state.attribution.principal,
            "land",
            "git commit -m \"...\"",
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
