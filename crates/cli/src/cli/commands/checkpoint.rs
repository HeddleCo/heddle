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

use anyhow::{Result, anyhow};
use objects::{object::ThreadName, store::ObjectStore};
use oplog::{OpLogBackend, OpRecord};
use repo::{CommitGraphIndex, GitCheckpointRecord, Repository, RepositoryCapability};
use serde::Serialize;
use sley::Repository as SleyRepository;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    command_catalog::ActionTemplate,
    git_overlay_health::{
        GitOverlayMutationPreflight, RepositoryVerificationState,
        build_repository_verification_state, git_overlay_mutation_preflight_advice,
        plain_git_mutation_preflight_advice, repository_verification_blocked_advice,
    },
    snapshot::ensure_current_state,
    worktree_safety::dirty_worktree_advice,
};
use heddle_core::bridge::{
    GitBridge, WriteThroughOutcome,
    git_core::{git_config_identity_with_global_fallback, principal_is_default_unknown},
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
    if let Some(advice) = plain_git_mutation_preflight_advice(start, "checkpoint")? {
        return Err(anyhow!(advice));
    }

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
    create_git_checkpoint_inner(repo, message, status_options, true)
}

pub(crate) fn create_git_checkpoint_from_index_snapshot(
    repo: &Repository,
    message: Option<&str>,
    status_options: repo::WorktreeStatusOptions,
) -> Result<GitCheckpointRecord> {
    create_git_checkpoint_inner(repo, message, status_options, false)
}

fn create_git_checkpoint_inner(
    repo: &Repository,
    message: Option<&str>,
    status_options: repo::WorktreeStatusOptions,
    require_clean_worktree: bool,
) -> Result<GitCheckpointRecord> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Err(anyhow!(native_checkpoint_unavailable_advice(repo)));
    }
    preflight_git_checkpoint_ref_update(repo, "checkpoint")?;
    if let Some(advice) = git_overlay_mutation_preflight_advice(
        repo,
        "checkpoint",
        GitOverlayMutationPreflight::checkpoint_like(),
    )? {
        return Err(anyhow!(advice));
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
    if principal_is_default_unknown(&state.attribution.principal)
        && git_config_identity_with_global_fallback(repo.root())?.is_none()
    {
        return Err(anyhow!(missing_checkpoint_identity_advice("checkpoint")));
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
    let git_commit = match bridge
        .write_through_current_checkout_with_message(state.change_id, summary.clone())?
    {
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

pub(crate) fn preflight_git_checkpoint_ref_update(repo: &Repository, action: &str) -> Result<()> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Ok(());
    }
    let trust = build_repository_verification_state(repo);
    if git_checkpoint_trust_allows_ref_update(&trust)
        || git_checkpoint_can_close_integrated_remote_gap(repo, &trust)
    {
        return Ok(());
    }
    Err(anyhow!(git_checkpoint_preflight_advice(
        repo, &trust, action
    )))
}

fn git_checkpoint_trust_allows_ref_update(trust: &RepositoryVerificationState) -> bool {
    let status_allows_checkpoint = matches!(
        trust.status.as_str(),
        "clean" | "dirty_worktree" | "needs_checkpoint" | "remote_ahead" | "remote_untracked"
    );
    let remote_allows_checkpoint = matches!(
        trust.remote_drift.as_str(),
        "clean" | "remote_ahead" | "remote_untracked"
    );
    status_allows_checkpoint && remote_allows_checkpoint
}

fn git_checkpoint_can_close_integrated_remote_gap(
    repo: &Repository,
    trust: &RepositoryVerificationState,
) -> bool {
    if trust.status != "needs_checkpoint"
        || !matches!(
            trust.remote_drift.as_str(),
            "remote_behind" | "remote_diverged"
        )
    {
        return false;
    }
    let Some(remote) = repo.git_remote_tracking_status().ok().flatten() else {
        return false;
    };
    let upstream = remote.upstream.trim();
    if upstream.is_empty() {
        return false;
    }
    let Ok(Some(upstream_state)) = repo.refs().get_thread(&ThreadName::new(upstream)) else {
        return false;
    };
    let Ok(Some(current_state)) = repo.head() else {
        return false;
    };
    let mut graph = CommitGraphIndex::new(repo);
    graph
        .is_ancestor(&upstream_state, &current_state)
        .unwrap_or(false)
}

fn git_checkpoint_preflight_advice(
    repo: &Repository,
    trust: &RepositoryVerificationState,
    action: &str,
) -> RecoveryAdvice {
    let primary_command = super::git_overlay_health::remote_drift_primary_action(repo)
        .unwrap_or_else(|| {
            if trust.recommended_action.trim().is_empty() {
                "heddle verify".to_string()
            } else {
                trust.recommended_action.clone()
            }
        });
    repository_verification_blocked_advice(
        "git_checkpoint_preflight_blocked",
        format!("Refusing to {action}: Git checkpoint preflight is blocked"),
        format!("retrying `heddle {action}`"),
        trust,
        format!(
            "repository verification status is {}; remote drift is {}: {}",
            trust.status, trust.remote_drift, trust.summary
        ),
        format!(
            "{action} would capture Heddle state before the Git checkpoint ref update is known to be safe"
        ),
        "Git refs, Heddle refs, Git checkpoint metadata, and worktree files were left unchanged",
        Some(primary_command),
    )
}

fn native_checkpoint_unavailable_advice(repo: &Repository) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "checkpoint_requires_git_overlay",
        "`heddle checkpoint` is only for Git-overlay repositories",
        "Use `heddle commit -m \"...\"` to save native Heddle work, or `heddle capture -m \"...\"` for a recoverable step without a Git checkpoint.",
        format!("repository mode is {}", repo.storage_model_label()),
        "checkpoint would need to write a Git commit, but this checkout has no Git-overlay branch/index",
        "Heddle refs and worktree files were left unchanged",
        "heddle commit -m \"...\"",
        vec![
            "heddle commit -m \"...\"".to_string(),
            "heddle capture -m \"...\"".to_string(),
            "heddle status".to_string(),
        ],
    )
}

fn missing_checkpoint_identity_advice(action: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "git_checkpoint_identity_required",
        format!("Refusing to {action}: no accountable identity is configured for the Git commit"),
        "Configure `HEDDLE_PRINCIPAL_NAME` and `HEDDLE_PRINCIPAL_EMAIL`, set .heddle principal, or configure Git user.name/user.email before retrying.",
        "Heddle would otherwise have to write Unknown <unknown@example.com> into the Git commit",
        format!("{action} would create an auditable Git checkpoint without a real author identity"),
        "Git refs, Heddle refs, Git checkpoint metadata, and worktree files were left unchanged",
        "heddle init --principal-name <name> --principal-email <email>",
        vec![
            "heddle init --principal-name <name> --principal-email <email>".to_string(),
            "heddle checkpoint -m \"...\"".to_string(),
        ],
    )
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
    let git = SleyRepository::discover(root).ok()?;
    git.head().ok()?.oid.map(|id| id.to_string())
}

fn build_output(
    repo: &Repository,
    change_id: &str,
    record: &GitCheckpointRecord,
) -> CheckpointOutput {
    let trust = build_repository_verification_state(repo);
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
