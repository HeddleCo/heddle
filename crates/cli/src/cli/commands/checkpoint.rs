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
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Err(anyhow!(
            git_overlay_txn::native_checkpoint_unavailable_advice(repo)
        ));
    }
    let facts = git_overlay_txn::gather_mutation_facts(repo);
    git_overlay_txn::preflight_checkpoint(repo, "land", &facts)?;

    let user_config = UserConfig::load_default()?;

    // Fast path for an already-captured state: reuse an existing checkpoint
    // record and gate identity on the state's STORED principal, in main's
    // order — record-reuse first (a no-op checkpoint must not fail identity),
    // and never against an `unknown@example.com` fallback (which would let a
    // misconfigured identity slip a Git commit through). Bootstrap + new-state
    // creation fall through to `execute_save` below.
    if let Some(existing_state) = repo.current_state()? {
        let tree = repo.require_tree(&existing_state.tree)?;
        let status = repo.compare_worktree_cached_detailed_with_options(&tree, &status_options)?;
        if !status.is_clean() {
            return Err(anyhow!(dirty_worktree_advice(
                "land",
                &status,
                "the current Heddle state was left unchanged; these paths have not been captured",
            )));
        }
        if let Some(record) = repo.latest_git_checkpoint_for_state(&existing_state.state_id)? {
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
        git_scope: GitScope::WorktreeAll,
        supplied_tree: None,
        reuse_current_state: true,
        require_clean_worktree: true,
        worktree_status_options: status_options,
        run_hooks: true,
        commit_safe_post_verify: false,
        coalesce_snapshot_and_checkpoint: false,
        precomputed_worktree_status: None,
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
                    "land",
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
