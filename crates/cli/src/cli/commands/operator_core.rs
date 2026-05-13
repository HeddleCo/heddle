// SPDX-License-Identifier: Apache-2.0
use std::{path::Path, process::Command};

use anyhow::{Result, anyhow};
use repo::{
    GitOverlayImportHint, GitRemoteTrackingStatus, OperationKind, OperationScope, Repository,
    RepositoryOperationStatus,
};
use serde::Serialize;

use super::{
    bisect::reset_bisect_state,
    rebase::{
        OperatorContinueStatus, cmd_rebase_silent, continue_rebase_for_operator,
        has_persisted_rebase_state,
    },
    resolve::abort_merge_state,
    snapshot::{SnapshotAgentOverrides, create_snapshot},
};
use crate::config::UserConfig;

#[derive(Debug, Clone, Serialize, Default)]
pub(crate) struct OperatorCommandOutput {
    pub status: String,
    pub action: String,
    pub message: String,
    /// Reasons the operation could not advance state. Only populated
    /// when `status == "blocked"` or `status == "failed"`. When the
    /// operation succeeded with caveats, use `warnings` instead.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    /// Non-blocking nudges surfaced when the operation actually
    /// advanced state but the caller may still want a follow-up
    /// (e.g. a heavy-impact change worth reviewing for broader impact).
    /// Always omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended_action: Option<String>,
}

pub(crate) fn open_operator_repo_from_path(path: &Path) -> Result<Repository> {
    let cwd_repo = Repository::open(path)?;
    let target_path = cwd_repo.active_worktree_path()?;
    if target_path == *cwd_repo.root() {
        Ok(cwd_repo)
    } else {
        Ok(Repository::open(&target_path)?)
    }
}

pub(crate) fn continue_operator(repo: &Repository) -> Result<OperatorCommandOutput> {
    if repo.merge_state_manager().is_merge_in_progress() {
        let unresolved = repo.merge_state_manager().unresolved()?;
        if !unresolved.is_empty() {
            let recommended_action = format!("heddle resolve {}", unresolved[0]);
            return Ok(OperatorCommandOutput {
                status: "blocked".to_string(),
                action: "continue".to_string(),
                message: format!(
                    "Merge still has unresolved conflicts: {}. After removing conflict markers, mark each file resolved with `heddle resolve <path>`.",
                    unresolved.join(", ")
                ),
                blockers: unresolved,
                warnings: Vec::new(),
                next_action: Some("heddle resolve --list".to_string()),
                recommended_action: Some(recommended_action),
            });
        }

        create_snapshot(
            repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some("Continue merge".to_string()),
            None,
            SnapshotAgentOverrides {
                provider: None,
                model: None,
                session: None,
                segment: None,
                policy: None,
                no_policy: false,
                no_agent: false,
            },
        )?;
        return Ok(OperatorCommandOutput {
            status: "continued".to_string(),
            action: "merge".to_string(),
            message: "Completed the in-progress Heddle merge".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: None,
            recommended_action: None,
        });
    }

    if let Some(operation) = repo.operation_status()? {
        return continue_from_operation(repo, &operation);
    }

    Ok(OperatorCommandOutput {
        status: "noop".to_string(),
        action: "continue".to_string(),
        message: "No in-progress operation needs continuing".to_string(),
        blockers: Vec::new(),
        warnings: Vec::new(),
        next_action: None,
        recommended_action: None,
    })
}

pub(crate) fn abort_operator(repo: &Repository) -> Result<OperatorCommandOutput> {
    if repo.merge_state_manager().is_merge_in_progress() {
        abort_merge_state(repo, &repo.merge_state_manager())?;
        return Ok(OperatorCommandOutput {
            status: "aborted".to_string(),
            action: "merge".to_string(),
            message: "Aborted the in-progress Heddle merge".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: None,
            recommended_action: None,
        });
    }

    if has_persisted_rebase_state(repo) {
        cmd_rebase_silent(repo, None, true, false)?;
        return Ok(OperatorCommandOutput {
            status: "aborted".to_string(),
            action: "rebase".to_string(),
            message: "Aborted the in-progress Heddle rebase".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: None,
            recommended_action: None,
        });
    }

    if let Some(operation) = repo.operation_status()? {
        return abort_from_operation(repo, &operation);
    }

    Ok(OperatorCommandOutput {
        status: "noop".to_string(),
        action: "abort".to_string(),
        message: "No in-progress operation can be aborted".to_string(),
        blockers: Vec::new(),
        warnings: Vec::new(),
        next_action: None,
        recommended_action: None,
    })
}

fn continue_from_operation(
    repo: &Repository,
    operation: &RepositoryOperationStatus,
) -> Result<OperatorCommandOutput> {
    match (&operation.scope, &operation.kind) {
        (OperationScope::Heddle, OperationKind::Rebase) => {
            Ok(match continue_rebase_for_operator(repo)? {
                OperatorContinueStatus::Blocked => OperatorCommandOutput {
                    status: "blocked".to_string(),
                    action: "rebase".to_string(),
                    message:
                        "Rebase still needs a captured manual resolution before it can continue"
                            .to_string(),
                    blockers: Vec::new(),
                    warnings: Vec::new(),
                    next_action: Some("heddle capture -m \"Manual resolution\"".to_string()),
                    recommended_action: Some("heddle capture -m \"Manual resolution\"".to_string()),
                },
                OperatorContinueStatus::Continued => OperatorCommandOutput {
                    status: "continued".to_string(),
                    action: "rebase".to_string(),
                    message: "Continued the in-progress Heddle rebase".to_string(),
                    blockers: Vec::new(),
                    warnings: Vec::new(),
                    next_action: None,
                    recommended_action: None,
                },
                OperatorContinueStatus::Completed => OperatorCommandOutput {
                    status: "completed".to_string(),
                    action: "rebase".to_string(),
                    message: "Completed the in-progress Heddle rebase".to_string(),
                    blockers: Vec::new(),
                    warnings: Vec::new(),
                    next_action: None,
                    recommended_action: None,
                },
            })
        }
        (OperationScope::Heddle, OperationKind::Bisect) => Ok(OperatorCommandOutput {
            status: "blocked".to_string(),
            action: "bisect".to_string(),
            message: "Bisect needs a good/bad decision before it can continue".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: Some(
                "heddle bisect good <state> or heddle bisect bad <state>".to_string(),
            ),
            recommended_action: Some(
                "heddle bisect good <state> or heddle bisect bad <state>".to_string(),
            ),
        }),
        (OperationScope::Git, OperationKind::Rebase) => {
            let unresolved = git_unmerged_paths(repo)?;
            if !unresolved.is_empty() {
                return Ok(git_conflict_blocked_action(
                    "rebase",
                    "Git rebase still has unresolved conflicts",
                    unresolved,
                ));
            }
            run_git_control(repo, &["rebase", "--continue"])?;
            Ok(simple_action(
                "continued",
                "rebase",
                "Continued the in-progress Git rebase",
            ))
        }
        (OperationScope::Git, OperationKind::Merge) => {
            let unresolved = git_unmerged_paths(repo)?;
            if !unresolved.is_empty() {
                return Ok(git_conflict_blocked_action(
                    "merge",
                    "Git merge still has unresolved conflicts",
                    unresolved,
                ));
            }
            run_git_control(repo, &["merge", "--continue"])?;
            Ok(simple_action(
                "continued",
                "merge",
                "Continued the in-progress Git merge",
            ))
        }
        (OperationScope::Git, OperationKind::CherryPick) => {
            continue_git_cherry_pick(repo)?;
            Ok(simple_action(
                "continued",
                "cherry-pick",
                "Continued the in-progress Git cherry-pick",
            ))
        }
        (OperationScope::Git, OperationKind::Revert) => {
            continue_git_revert(repo)?;
            Ok(simple_action(
                "continued",
                "revert",
                "Continued the in-progress Git revert",
            ))
        }
        (OperationScope::Git, OperationKind::Bisect) => Ok(OperatorCommandOutput {
            status: "blocked".to_string(),
            action: "bisect".to_string(),
            message: "Git bisect needs a good/bad decision before it can continue".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: Some("git bisect good or git bisect bad".to_string()),
            recommended_action: Some("git bisect good or git bisect bad".to_string()),
        }),
        (OperationScope::Heddle, OperationKind::Merge) => unreachable!(),
        _ => Ok(OperatorCommandOutput {
            status: "noop".to_string(),
            action: "continue".to_string(),
            message: "No in-progress operation needs continuing".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: None,
            recommended_action: None,
        }),
    }
}

fn abort_from_operation(
    repo: &Repository,
    operation: &RepositoryOperationStatus,
) -> Result<OperatorCommandOutput> {
    match (&operation.scope, &operation.kind) {
        (OperationScope::Heddle, OperationKind::Rebase) => {
            cmd_rebase_silent(repo, None, true, false)?;
        }
        (OperationScope::Heddle, OperationKind::Bisect) => {
            reset_bisect_state(repo)?;
        }
        (OperationScope::Git, OperationKind::Rebase) => {
            run_git_control(repo, &["rebase", "--abort"])?
        }
        (OperationScope::Git, OperationKind::Merge) => {
            run_git_control(repo, &["merge", "--abort"])?
        }
        (OperationScope::Git, OperationKind::CherryPick) => {
            run_git_control(repo, &["cherry-pick", "--abort"])?
        }
        (OperationScope::Git, OperationKind::Revert) => {
            run_git_control(repo, &["revert", "--abort"])?
        }
        (OperationScope::Git, OperationKind::Bisect) => {
            run_git_control(repo, &["bisect", "reset"])?
        }
        _ => {}
    }

    Ok(OperatorCommandOutput {
        status: "aborted".to_string(),
        action: operation.kind.to_string(),
        message: format!(
            "Aborted the in-progress {} {}",
            operation.scope, operation.kind
        ),
        blockers: Vec::new(),
        warnings: Vec::new(),
        next_action: None,
        recommended_action: None,
    })
}

fn simple_action(status: &str, action: &str, message: &str) -> OperatorCommandOutput {
    OperatorCommandOutput {
        status: status.to_string(),
        action: action.to_string(),
        message: message.to_string(),
        blockers: Vec::new(),
        warnings: Vec::new(),
        next_action: None,
        recommended_action: None,
    }
}

fn git_conflict_blocked_action(
    action: &str,
    message: &str,
    unresolved: Vec<String>,
) -> OperatorCommandOutput {
    OperatorCommandOutput {
        status: "blocked".to_string(),
        action: action.to_string(),
        message: format!("{message}: {}", unresolved.join(", ")),
        blockers: unresolved,
        warnings: Vec::new(),
        next_action: Some(
            "Resolve the files, stage them with `git add <files>`, then run `heddle continue`"
                .to_string(),
        ),
        recommended_action: Some("git add <files> && heddle continue".to_string()),
    }
}

pub(crate) fn recommend_next_action(
    operation: Option<&RepositoryOperationStatus>,
    remote_tracking: Option<&GitRemoteTrackingStatus>,
    import_hint: Option<&GitOverlayImportHint>,
    fallback: Option<&str>,
) -> String {
    if let Some(operation) = operation {
        return operation.next_action.clone();
    }
    if let Some(remote_tracking) = remote_tracking {
        if remote_tracking.behind > 0 {
            return "heddle sync".to_string();
        }
        return remote_tracking.next_action.clone();
    }
    let fallback = fallback.unwrap_or_default();
    if !fallback.is_empty() {
        return fallback.to_string();
    }
    if let Some(hint) = import_hint {
        return hint.recommended_command.clone();
    }
    String::new()
}

pub(crate) fn run_git_control(repo: &Repository, args: &[&str]) -> Result<()> {
    let (success, stderr) = run_git_control_attempt(repo, args)?;
    if success {
        return Ok(());
    }

    Err(anyhow!(
        "git {} failed at '{}': {}",
        args.join(" "),
        repo.root().display(),
        stderr
    ))
}

fn git_unmerged_paths(repo: &Repository) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo.root())
        .args(["diff", "--name-only", "--diff-filter=U"])
        .output()
        .map_err(|error| anyhow!("failed to inspect Git conflicts: {error}"))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect())
}

fn run_git_control_attempt(repo: &Repository, args: &[&str]) -> Result<(bool, String)> {
    let output = Command::new("git")
        .env("GIT_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .arg("-C")
        .arg(repo.root())
        .args(args)
        .output()
        .map_err(|error| anyhow!("failed to run git {}: {}", args.join(" "), error))?;

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Ok((output.status.success(), stderr))
}

fn continue_git_cherry_pick(repo: &Repository) -> Result<()> {
    let (success, stderr) = run_git_control_attempt(repo, &["cherry-pick", "--continue"])?;
    if success {
        return Ok(());
    }
    if stderr.contains("previous cherry-pick is now empty") {
        return run_git_control(repo, &["cherry-pick", "--skip"]);
    }
    Err(anyhow!(
        "git cherry-pick --continue failed at '{}': {}",
        repo.root().display(),
        stderr
    ))
}

fn continue_git_revert(repo: &Repository) -> Result<()> {
    let (success, stderr) = run_git_control_attempt(repo, &["revert", "--continue"])?;
    if success {
        return Ok(());
    }
    if stderr.contains("previous cherry-pick is now empty")
        || stderr.contains("previous revert is now empty")
    {
        return run_git_control(repo, &["revert", "--skip"]);
    }
    Err(anyhow!(
        "git revert --continue failed at '{}': {}",
        repo.root().display(),
        stderr
    ))
}