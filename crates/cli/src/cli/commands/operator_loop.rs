// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;
use repo::{GitOverlayImportHint, GitRemoteTrackingStatus, RepositoryOperationStatus};

use super::{
    action_line::print_next_step,
    git_overlay_health::{RepositoryVerificationState, build_repository_verification_state},
    next_action::{
        NextActionInput, NextActionValidationContext, effective_next_action, write_command_json,
    },
    operator_core::{
        ABORT_OPERATOR_EMISSION, CONTINUE_OPERATOR_EMISSION, OperatorAction, OperatorCommandOutput,
        OperatorEmission, SYNC_OPERATOR_EMISSION, abort_operator, exit_if_blocked_operator_status,
        open_operator_repo_from_path, recommend_next_action,
    },
    remote::resolve_default_remote_name,
    workflow::cmd_sync,
    worktree_safety::ensure_worktree_clean,
};
use crate::{
    bridge::GitBridge,
    cli::{Cli, cli_args::SyncArgs, output_is_compact, should_output_json, style},
};

pub async fn cmd_continue(cli: &Cli) -> Result<()> {
    let current_dir = std::env::current_dir()?;
    let cwd = cli.repo.as_ref().unwrap_or(&current_dir);
    let repo = open_operator_repo_from_path(cwd)?;
    let output = super::operator_core::continue_operator(&repo)?;
    let status = output.status.clone();
    emit(cli, &repo, output, CONTINUE_OPERATOR_EMISSION)?;
    exit_if_blocked_operator_status(&status);
    Ok(())
}

pub fn cmd_abort(cli: &Cli) -> Result<()> {
    let current_dir = std::env::current_dir()?;
    let cwd = cli.repo.as_ref().unwrap_or(&current_dir);
    let repo = open_operator_repo_from_path(cwd)?;
    let output = abort_operator(&repo)?;
    emit(cli, &repo, output, ABORT_OPERATOR_EMISSION)
}

pub async fn cmd_sync_smart(cli: &Cli, args: SyncArgs) -> Result<()> {
    let current_dir = std::env::current_dir()?;
    let cwd = cli.repo.as_ref().unwrap_or(&current_dir);
    let repo = open_operator_repo_from_path(cwd)?;
    if repo.operation_status()?.is_some() || repo.merge_state_manager().is_merge_in_progress() {
        return emit(
            cli,
            &repo,
            OperatorCommandOutput {
                status: "blocked".to_string(),
                action: OperatorAction::Sync,
                message: "Finish the in-progress operation before syncing".to_string(),
                blockers: Vec::new(),
                warnings: Vec::new(),
                next_action: Some("heddle continue".to_string()),
                recommended_action: Some("heddle continue".to_string()),
            },
            SYNC_OPERATOR_EMISSION,
        );
    }

    if let Some(remote) = repo.git_remote_tracking_status()? {
        let remote_decision = super::git_overlay_health::remote_drift_decision(&repo, &remote);
        if remote_decision.status == "remote_diverged" {
            let recommended_action = remote_decision
                .primary_action
                .clone()
                .unwrap_or_else(|| "heddle verify".to_string());
            return emit(
                cli,
                &repo,
                OperatorCommandOutput {
                    status: "blocked".to_string(),
                    action: OperatorAction::Sync,
                    message: remote.message,
                    blockers: vec![
                        "Git branch and upstream both contain commits the other side lacks"
                            .to_string(),
                    ],
                    warnings: Vec::new(),
                    next_action: Some(recommended_action.clone()),
                    recommended_action: Some(recommended_action),
                },
                SYNC_OPERATOR_EMISSION,
            );
        }
        if remote_decision.status == "remote_behind" {
            ensure_worktree_clean(&repo, "sync")?;
            let remote_name = resolve_default_remote_name(&repo, None)?;
            let mut bridge = GitBridge::new(&repo);
            let outcome = bridge.pull(&remote_name)?;
            let verification = build_repository_verification_state(&repo);
            if !verification.verified {
                return emit(
                    cli,
                    &repo,
                    sync_blocked_by_trust(verification),
                    SYNC_OPERATOR_EMISSION,
                );
            }
            return emit(
                cli,
                &repo,
                OperatorCommandOutput {
                    status: "synced".to_string(),
                    action: OperatorAction::Sync,
                    message: format!(
                        "Synced branch '{}' with remote '{}' ({} commit(s) seen, {} state(s) imported)",
                        remote.branch, remote_name, outcome.commits_seen, outcome.states_created,
                    ),
                    blockers: Vec::new(),
                    warnings: Vec::new(),
                    next_action: None,
                    recommended_action: None,
                },
                SYNC_OPERATOR_EMISSION,
            );
        }
        if remote_decision.status == "remote_ahead" {
            return emit(
                cli,
                &repo,
                OperatorCommandOutput {
                    status: "ahead".to_string(),
                    action: OperatorAction::Sync,
                    message: remote.message,
                    blockers: Vec::new(),
                    warnings: Vec::new(),
                    next_action: Some("heddle push".to_string()),
                    recommended_action: Some("heddle push".to_string()),
                },
                SYNC_OPERATOR_EMISSION,
            );
        }
    }

    cmd_sync(cli, args).await
}

fn sync_blocked_by_trust(trust: RepositoryVerificationState) -> OperatorCommandOutput {
    OperatorCommandOutput::blocked_by_repository_verification(
        OperatorAction::Sync,
        format!(
            "Sync changed the checkout, but repository verification is still blocked: {}",
            trust.summary
        ),
        &trust,
    )
}

fn emit(
    cli: &Cli,
    repo: &repo::Repository,
    output: OperatorCommandOutput,
    emission: OperatorEmission,
) -> Result<()> {
    if should_output_json(cli, None) {
        let envelope = output.envelope_for_command(emission.output_kind);
        write_command_json(
            &envelope,
            output_is_compact(cli),
            NextActionValidationContext::new(emission.command, repo.capability()),
        )?;
    } else {
        let message = match output.status.as_str() {
            "blocked" => style::warn(&output.message),
            "aborted" => style::warn(&output.message),
            "continued" | "completed" | "synced" => style::accent(&output.message),
            _ => output.message.clone(),
        };
        println!("{}", message);
        if !output.blockers.is_empty() {
            println!("{}", style::warn("Blocked by"));
            for blocker in &output.blockers {
                println!("  - {}", style::warn(blocker));
            }
        }
        if let Some(next) = output.recommended_action.or(output.next_action) {
            print_next_step(&next);
        }
    }
    Ok(())
}

pub(crate) fn primary_next_action(
    operation: Option<&RepositoryOperationStatus>,
    remote_tracking: Option<&GitRemoteTrackingStatus>,
    import_hint: Option<&GitOverlayImportHint>,
    fallback: Option<&str>,
) -> String {
    recommend_next_action(operation, remote_tracking, import_hint, fallback)
}

pub(crate) fn primary_next_action_with_verification(
    operation: Option<&RepositoryOperationStatus>,
    remote_tracking: Option<&GitRemoteTrackingStatus>,
    import_hint: Option<&GitOverlayImportHint>,
    fallback: Option<&str>,
    trust: &RepositoryVerificationState,
) -> String {
    let fallback = non_empty_action(fallback)
        .or_else(|| non_empty_action(Some(trust.recommended_action.as_str())));
    effective_next_action(
        NextActionInput::default(operation, remote_tracking, import_hint, fallback)
            .with_verification(trust),
    )
}

fn non_empty_action(action: Option<&str>) -> Option<&str> {
    action.filter(|action| !action.trim().is_empty())
}
