// SPDX-License-Identifier: Apache-2.0
//! Shared Git-overlay mutation gates for checkpoint-shaped commands.
//!
//! This module centralizes the safety checks and recovery advice that surround
//! Git-overlay checkpoint writes. It deliberately does not execute checkout,
//! index, ref, or object mutations: those stay in the Git projection engine,
//! checkpoint, repo::atomic, and refs::commit_and_publish paths.

use anyhow::{Result, anyhow};
use objects::{
    object::{Principal, ThreadName},
    worktree::WorktreeStatus,
};
use repo::{CommitGraphIndex, Repository, RepositoryCapability, thread_flag};

use super::{
    advice::RecoveryAdvice,
    thread_landing::land_local_command,
    verification_health::{
        GitOverlayMutationPreflight, RepositoryVerificationState,
        build_repository_verification_state,
        build_repository_verification_state_with_worktree_status,
        git_overlay_mutation_preflight_advice_with_worktree_status,
        plain_git_mutation_preflight_advice, raw_git_operation_mutation_advice,
        repository_verification_blocked_advice,
    },
};
use heddle_git_projection::git_core::{
    git_config_identity_with_global_fallback, principal_is_default_unknown,
};

pub(crate) type GitOverlayWorktreeStatus = repo::Result<Option<WorktreeStatus>>;

pub(crate) struct GitOverlayMutationFacts {
    worktree_status: GitOverlayWorktreeStatus,
}

pub(crate) fn gather_mutation_facts(repo: &Repository) -> GitOverlayMutationFacts {
    GitOverlayMutationFacts {
        worktree_status: repo.git_overlay_worktree_status(),
    }
}

pub(crate) fn preflight_checkpoint_repository(start: &std::path::Path, action: &str) -> Result<()> {
    if let Some(advice) = plain_git_mutation_preflight_advice(start, action)? {
        return Err(anyhow!(advice));
    }
    Ok(())
}

pub(crate) fn preflight_checkpoint(
    repo: &Repository,
    action: &str,
    facts: &GitOverlayMutationFacts,
) -> Result<()> {
    preflight_checkpoint_like_with_worktree_status(repo, action, &facts.worktree_status)
}

pub(crate) fn preflight_land_checkpoint(repo: &Repository, thread_id: &str) -> Result<()> {
    if let Some(advice) = land_checkpoint_preflight_advice(repo, thread_id) {
        return Err(anyhow!(advice));
    }
    Ok(())
}

fn preflight_checkpoint_like_with_worktree_status(
    repo: &Repository,
    action: &str,
    worktree_status: &GitOverlayWorktreeStatus,
) -> Result<()> {
    if let Some(advice) = raw_git_operation_mutation_advice(repo, action)? {
        return Err(anyhow!(advice));
    }
    preflight_checkpoint_ref_update_with_worktree_status(repo, action, worktree_status)?;
    if let Some(advice) = git_overlay_mutation_preflight_advice_with_worktree_status(
        repo,
        action,
        GitOverlayMutationPreflight::checkpoint_like(),
        worktree_status,
    )? {
        return Err(anyhow!(advice));
    }
    Ok(())
}

fn preflight_checkpoint_ref_update_with_worktree_status(
    repo: &Repository,
    action: &str,
    worktree_status: &GitOverlayWorktreeStatus,
) -> Result<()> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Ok(());
    }
    let trust = preflight_verify_with_worktree_status(repo, worktree_status);
    preflight_checkpoint_ref_update_with_trust(repo, action, trust)
}

pub(crate) fn preflight_git_checkpoint_identity_for_principal(
    repo: &Repository,
    principal: &Principal,
    action: &str,
    retry_command: &str,
) -> Result<()> {
    if !principal_is_default_unknown(principal) {
        return Ok(());
    }
    if git_config_identity_with_global_fallback(repo.root())?.is_some() {
        return Ok(());
    }
    Err(anyhow!(missing_git_checkpoint_identity_advice(
        action,
        retry_command,
    )))
}

pub(crate) fn preflight_verify(repo: &Repository) -> RepositoryVerificationState {
    build_repository_verification_state(repo)
}

pub(crate) fn preflight_verify_with_worktree_status(
    repo: &Repository,
    worktree_status: &GitOverlayWorktreeStatus,
) -> RepositoryVerificationState {
    build_repository_verification_state_with_worktree_status(repo, worktree_status)
}

pub(crate) fn post_verify(repo: &Repository) -> RepositoryVerificationState {
    build_repository_verification_state(repo)
}

fn land_checkpoint_preflight_advice(repo: &Repository, thread_id: &str) -> Option<RecoveryAdvice> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return None;
    }
    let trust = preflight_verify(repo);
    if trust.remote_drift == "remote_diverged" {
        let remote_decision = repo
            .git_remote_tracking_status()
            .ok()
            .flatten()
            .map(|remote| super::verification_health::remote_drift_decision(repo, &remote));
        let primary_command = remote_decision
            .as_ref()
            .and_then(|decision| decision.primary_action.clone())
            .unwrap_or_else(|| {
                if trust.recommended_action.trim().is_empty() {
                    "heddle pull".to_string()
                } else {
                    trust.recommended_action.clone()
                }
            });
        let recovery_commands = if trust.recovery_commands.is_empty() {
            let mut commands = remote_decision
                .map(|decision| decision.recovery_commands)
                .unwrap_or_else(|| vec![primary_command.clone()]);
            commands.push(format!("heddle sync {}", thread_flag(thread_id)));
            commands.push(land_local_command(thread_id));
            commands
        } else {
            trust.recovery_commands.clone()
        };
        return Some(RecoveryAdvice::safety_refusal(
            "land_requires_current_upstream",
            format!("Refusing to land '{thread_id}': upstream work must be integrated first"),
            format!("Run `{primary_command}`, then retry the land."),
            format!(
                "repository verification reports {}: {}",
                trust.remote_drift, trust.summary
            ),
            "land would first integrate Heddle state locally, then fail while writing the Git checkpoint because the checkout branch is behind its upstream",
            "thread refs, Heddle refs, Git refs, index, and worktree files were left unchanged",
            primary_command,
            recovery_commands,
        ));
    }
    if repo.root().join(".git/index.lock").exists() {
        return Some(RecoveryAdvice::safety_refusal(
            "land_checkpoint_preflight_blocked",
            format!("Refusing to land '{thread_id}': Git index is locked"),
            "Remove the stale Git index lock or wait for the active Git operation to finish, then retry the land.",
            ".git/index.lock exists in the parent checkout",
            "land would first integrate Heddle state locally, then fail while writing the Git checkpoint because the Git index is locked",
            "thread refs, Heddle refs, Git refs, index, and worktree files were left unchanged",
            "heddle status",
            vec!["heddle status".to_string()],
        ));
    }
    None
}

pub(crate) fn native_checkpoint_unavailable_advice(repo: &Repository) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "checkpoint_requires_git_overlay",
        "Git checkpointing is only available in Git-overlay repositories",
        "Use `heddle capture -m \"...\"` to save native Heddle work.",
        format!("repository mode is {}", repo.storage_model_label()),
        "checkpoint would need to write a Git commit, but this checkout has no Git-overlay branch/index",
        "Heddle refs and worktree files were left unchanged",
        "heddle capture -m \"...\"",
        vec![
            "heddle capture -m \"...\"".to_string(),
            "heddle status".to_string(),
        ],
    )
}

fn preflight_checkpoint_ref_update_with_trust(
    repo: &Repository,
    action: &str,
    trust: RepositoryVerificationState,
) -> Result<()> {
    if checkpoint_trust_allows_ref_update(&trust)
        || checkpoint_can_close_integrated_remote_gap(repo, &trust)
    {
        return Ok(());
    }
    Err(anyhow!(checkpoint_preflight_advice(repo, &trust, action)))
}

fn checkpoint_trust_allows_ref_update(trust: &RepositoryVerificationState) -> bool {
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

fn checkpoint_can_close_integrated_remote_gap(
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

fn checkpoint_preflight_advice(
    repo: &Repository,
    trust: &RepositoryVerificationState,
    action: &str,
) -> RecoveryAdvice {
    let primary_command = super::verification_health::remote_drift_primary_action(repo)
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

fn missing_git_checkpoint_identity_advice(action: &str, retry_command: &str) -> RecoveryAdvice {
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
            retry_command.to_string(),
        ],
    )
}
