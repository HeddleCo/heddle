// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;
use repo::{GitOverlayImportHint, GitRemoteTrackingStatus, RepositoryOperationStatus};

use super::{
    git_overlay_health::{RepositoryVerificationState, build_repository_verification_state},
    operator_core::{
        OperatorCommandOutput, abort_operator, exit_if_blocked_operator_status,
        open_operator_repo_from_path, recommend_next_action,
    },
    remote::resolve_default_remote_name,
    workflow::cmd_sync,
    worktree_safety::ensure_worktree_clean,
};
use crate::{
    bridge::GitBridge,
    cli::{Cli, cli_args::SyncArgs, should_output_json, style},
};

pub async fn cmd_continue(cli: &Cli) -> Result<()> {
    let current_dir = std::env::current_dir()?;
    let cwd = cli.repo.as_ref().unwrap_or(&current_dir);
    let repo = open_operator_repo_from_path(cwd)?;
    let output = super::operator_core::continue_operator(&repo)?;
    let status = output.status.clone();
    emit(cli, output)?;
    exit_if_blocked_operator_status(&status);
    Ok(())
}

pub fn cmd_abort(cli: &Cli) -> Result<()> {
    let current_dir = std::env::current_dir()?;
    let cwd = cli.repo.as_ref().unwrap_or(&current_dir);
    let repo = open_operator_repo_from_path(cwd)?;
    let output = abort_operator(&repo)?;
    emit(cli, output)
}

pub async fn cmd_sync_smart(cli: &Cli, args: SyncArgs) -> Result<()> {
    let current_dir = std::env::current_dir()?;
    let cwd = cli.repo.as_ref().unwrap_or(&current_dir);
    let repo = open_operator_repo_from_path(cwd)?;
    if repo.operation_status()?.is_some() || repo.merge_state_manager().is_merge_in_progress() {
        return emit(
            cli,
            OperatorCommandOutput {
                status: "blocked".to_string(),
                action: "sync".to_string(),
                message: "Finish the in-progress operation before syncing".to_string(),
                blockers: Vec::new(),
                warnings: Vec::new(),
                next_action: Some("heddle continue".to_string()),
                recommended_action: Some("heddle continue".to_string()),
            },
        );
    }

    if let Some(remote) = repo.git_remote_tracking_status()? {
        if remote.ahead > 0 && remote.behind > 0 {
            let recommended_action =
                super::git_overlay_health::remote_drift_recovery_commands(&repo, &remote)
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| "heddle verify".to_string());
            return emit(
                cli,
                OperatorCommandOutput {
                    status: "blocked".to_string(),
                    action: "sync".to_string(),
                    message: remote.message,
                    blockers: vec![
                        "Git branch and upstream both contain commits the other side lacks"
                            .to_string(),
                    ],
                    warnings: Vec::new(),
                    next_action: Some(recommended_action.clone()),
                    recommended_action: Some(recommended_action),
                },
            );
        }
        if remote.behind > 0 {
            ensure_worktree_clean(&repo, "sync")?;
            let remote_name = resolve_default_remote_name(&repo, None)?;
            let mut bridge = GitBridge::new(&repo);
            let outcome = bridge.pull(&remote_name)?;
            let verification = build_repository_verification_state(&repo);
            if !verification.verified {
                return emit(cli, sync_blocked_by_trust(verification));
            }
            return emit(
                cli,
                OperatorCommandOutput {
                    status: "synced".to_string(),
                    action: "sync".to_string(),
                    message: format!(
                        "Synced branch '{}' with remote '{}' ({} commit(s) seen, {} state(s) imported)",
                        remote.branch, remote_name, outcome.commits_seen, outcome.states_created,
                    ),
                    blockers: Vec::new(),
                    warnings: Vec::new(),
                    next_action: None,
                    recommended_action: None,
                },
            );
        }
        if remote.ahead > 0 {
            return emit(
                cli,
                OperatorCommandOutput {
                    status: "ahead".to_string(),
                    action: "sync".to_string(),
                    message: remote.message,
                    blockers: Vec::new(),
                    warnings: Vec::new(),
                    next_action: Some("heddle push".to_string()),
                    recommended_action: Some("heddle push".to_string()),
                },
            );
        }
    }

    cmd_sync(cli, args).await
}

fn sync_blocked_by_trust(trust: RepositoryVerificationState) -> OperatorCommandOutput {
    let recommended_action = if trust.recommended_action.is_empty() {
        "heddle verify".to_string()
    } else {
        trust.recommended_action.clone()
    };
    let blockers = trust
        .checks
        .iter()
        .filter(|check| !check.clean)
        .map(|check| format!("{}: {}", check.name, check.summary))
        .collect::<Vec<_>>();
    OperatorCommandOutput {
        status: "blocked".to_string(),
        action: "sync".to_string(),
        message: format!(
            "Sync changed the checkout, but repository verification is still blocked: {}",
            trust.summary
        ),
        blockers,
        warnings: Vec::new(),
        next_action: Some(recommended_action.clone()),
        recommended_action: Some(recommended_action),
    }
}

fn emit(cli: &Cli, output: OperatorCommandOutput) -> Result<()> {
    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(&output)?);
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
            println!("Next step: {}", style::bold(&next));
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
    if !trust.verified {
        return trust.recommended_action.clone();
    }
    let fallback = non_empty_action(fallback)
        .or_else(|| non_empty_action(Some(trust.recommended_action.as_str())));
    recommend_next_action(operation, remote_tracking, import_hint, fallback)
}

fn non_empty_action(action: Option<&str>) -> Option<&str> {
    action.filter(|action| !action.trim().is_empty())
}
