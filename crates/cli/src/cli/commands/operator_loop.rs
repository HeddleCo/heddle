// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;
use repo::{GitOverlayImportHint, GitRemoteTrackingStatus, RepositoryOperationStatus};

use super::{
    git_overlay_health::{RepositoryTrustState, build_repository_trust_state},
    operator_core::{
        OperatorCommandOutput, abort_operator, open_operator_repo_from_path, recommend_next_action,
        run_git_control,
    },
    workflow::cmd_sync,
};
use crate::{
    bridge::{GitBridge, git_import::import_selected_refs},
    cli::{Cli, cli_args::SyncArgs, should_output_json, style},
};

pub async fn cmd_continue(cli: &Cli) -> Result<()> {
    let current_dir = std::env::current_dir()?;
    let cwd = cli.repo.as_ref().unwrap_or(&current_dir);
    let repo = open_operator_repo_from_path(cwd)?;
    let output = super::operator_core::continue_operator(&repo)?;
    emit(cli, output)
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
        if remote.behind > 0 {
            run_git_control(&repo, &["pull", "--rebase"])?;
            let mut bridge = GitBridge::new(&repo);
            import_selected_refs(
                &mut bridge,
                Some(repo.root()),
                std::slice::from_ref(&remote.branch),
            )?;
            let trust = build_repository_trust_state(&repo);
            if !trust.trusted {
                return emit(cli, sync_blocked_by_trust(trust));
            }
            return emit(
                cli,
                OperatorCommandOutput {
                    status: "synced".to_string(),
                    action: "sync".to_string(),
                    message: format!(
                        "Synced branch '{}' with upstream '{}'",
                        remote.branch, remote.upstream
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

fn sync_blocked_by_trust(trust: RepositoryTrustState) -> OperatorCommandOutput {
    let recommended_action = if trust.recommended_action.is_empty() {
        "heddle trust".to_string()
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
            "Sync changed the checkout, but trust is still blocked: {}",
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
