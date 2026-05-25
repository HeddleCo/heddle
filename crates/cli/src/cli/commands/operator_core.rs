// SPDX-License-Identifier: Apache-2.0
use std::{collections::BTreeSet, path::Path};

use anyhow::Result;
use gix::bstr::ByteSlice;
use repo::{
    GitOverlayImportHint, GitRemoteTrackingStatus, OperationKind, OperationScope, Repository,
    RepositoryOperationStatus,
};
use serde::{Serialize, Serializer, ser::SerializeStruct};

use super::{
    bisect::reset_bisect_state,
    git_overlay_health::{
        RepositoryVerificationState, action_argv, action_template,
        import_hint_includes_active_branch, repository_verification_blockers,
        repository_verification_primary_command,
    },
    rebase::{
        OperatorContinueStatus, cmd_rebase_silent, continue_rebase_for_operator,
        has_persisted_rebase_state,
    },
    resolve::abort_merge_state,
    snapshot::{SnapshotAgentOverrides, create_snapshot},
};
use crate::config::UserConfig;

#[derive(Debug, Clone, Default)]
pub(crate) struct OperatorCommandOutput {
    pub status: String,
    pub action: String,
    pub message: String,
    /// Reasons the operation could not advance state. Only populated
    /// when `status == "blocked"` or `status == "failed"`. When the
    /// operation succeeded with caveats, use `warnings` instead.
    pub blockers: Vec<String>,
    /// Non-blocking nudges surfaced when the operation actually
    /// advanced state but the caller may still want a follow-up
    /// (e.g. a heavy-impact change worth reviewing for broader impact).
    /// Always omitted when empty.
    pub warnings: Vec<String>,
    pub next_action: Option<String>,
    pub recommended_action: Option<String>,
}

impl OperatorCommandOutput {
    pub(crate) fn blocked_by_repository_verification(
        action: impl Into<String>,
        message: impl Into<String>,
        trust: &RepositoryVerificationState,
    ) -> Self {
        let recommended_action = repository_verification_primary_command(trust);
        Self {
            status: "blocked".to_string(),
            action: action.into(),
            message: message.into(),
            blockers: repository_verification_blockers(trust),
            warnings: Vec::new(),
            next_action: Some(recommended_action.clone()),
            recommended_action: Some(recommended_action),
        }
    }
}

pub(crate) fn blocked_operator_exit_code(status: &str) -> Option<i32> {
    matches!(status, "blocked" | "failed").then_some(1)
}

pub(crate) fn exit_if_blocked_operator_status(status: &str) {
    if let Some(code) = blocked_operator_exit_code(status) {
        std::process::exit(code);
    }
}

impl Serialize for OperatorCommandOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let next_action = normalized_action(self.next_action.as_deref());
        let recommended_action = normalized_action(self.recommended_action.as_deref());
        let next_action_argv = next_action.and_then(action_argv);
        let next_action_template = next_action.and_then(action_template);
        let recommended_action_argv = recommended_action.and_then(action_argv);
        let recommended_action_template = recommended_action.and_then(action_template);

        let mut len = 10;
        if !self.blockers.is_empty() {
            len += 1;
        }
        if !self.warnings.is_empty() {
            len += 1;
        }

        let mut state = serializer.serialize_struct("OperatorCommandOutput", len)?;
        state.serialize_field("output_kind", &self.action)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("action", &self.action)?;
        state.serialize_field("message", &self.message)?;
        if !self.blockers.is_empty() {
            state.serialize_field("blockers", &self.blockers)?;
        }
        if !self.warnings.is_empty() {
            state.serialize_field("warnings", &self.warnings)?;
        }
        state.serialize_field("next_action", &next_action)?;
        state.serialize_field("next_action_argv", &next_action_argv)?;
        state.serialize_field("next_action_template", &next_action_template)?;
        state.serialize_field("recommended_action", &recommended_action)?;
        state.serialize_field("recommended_action_argv", &recommended_action_argv)?;
        state.serialize_field("recommended_action_template", &recommended_action_template)?;
        state.end()
    }
}

fn normalized_action(action: Option<&str>) -> Option<&str> {
    action.filter(|action| !action.trim().is_empty())
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
            Ok(raw_git_operation_handoff("continue", operation, unresolved))
        }
        (OperationScope::Git, OperationKind::Merge) => {
            let unresolved = git_unmerged_paths(repo)?;
            Ok(raw_git_operation_handoff("continue", operation, unresolved))
        }
        (OperationScope::Git, OperationKind::CherryPick) => {
            let unresolved = git_unmerged_paths(repo)?;
            Ok(raw_git_operation_handoff("continue", operation, unresolved))
        }
        (OperationScope::Git, OperationKind::Revert) => {
            let unresolved = git_unmerged_paths(repo)?;
            Ok(raw_git_operation_handoff("continue", operation, unresolved))
        }
        (OperationScope::Git, OperationKind::Bisect) => {
            Ok(raw_git_operation_handoff("continue", operation, Vec::new()))
        }
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
        (OperationScope::Git, _) => {
            let unresolved = git_unmerged_paths(repo).unwrap_or_default();
            return Ok(raw_git_operation_handoff("abort", operation, unresolved));
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

fn raw_git_operation_handoff(
    attempted_action: &str,
    operation: &RepositoryOperationStatus,
    unresolved: Vec<String>,
) -> OperatorCommandOutput {
    let primary = raw_git_preservation_command();
    let mut blockers = vec![format!(
        "externally-started Git {} is {}",
        operation.kind, operation.state
    )];
    blockers.extend(unresolved.iter().map(|path| format!("unresolved: {path}")));
    let unresolved_summary = if unresolved.is_empty() {
        String::new()
    } else {
        format!(" Unresolved paths: {}.", unresolved.join(", "))
    };
    let recovery_text = raw_git_operation_recovery_text(&operation.kind, &primary);
    OperatorCommandOutput {
        status: "blocked".to_string(),
        action: operation.kind.to_string(),
        message: format!(
            "Cannot {attempted_action} the active raw Git {} inside Heddle's no-git runtime. Heddle did not start this Git sequencer operation, so it left Git metadata, refs, index, and worktree files unchanged.{unresolved_summary} {recovery_text}",
            operation.kind
        ),
        blockers,
        warnings: Vec::new(),
        next_action: Some(primary.clone()),
        recommended_action: Some(primary),
    }
}

fn raw_git_preservation_command() -> String {
    "heddle bridge git status".to_string()
}

fn raw_git_operation_recovery_text(kind: &OperationKind, primary_command: &str) -> String {
    format!(
        "Inspect it with `{primary_command}`. Heddle did not start this raw Git {kind}, so finish or abort it with the Git-compatible tool that started it, then run `heddle verify` for the exact adoption command."
    )
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
            return if remote_tracking.ahead > 0 {
                if remote_tracking.upstream.is_empty() {
                    "heddle fetch".to_string()
                } else {
                    format!(
                        "heddle bridge git import --ref {}",
                        remote_tracking.upstream
                    )
                }
            } else {
                "heddle pull".to_string()
            };
        }
        return "heddle push".to_string();
    }
    let fallback = fallback.unwrap_or_default();
    if !fallback.is_empty() {
        return fallback.to_string();
    }
    if let Some(hint) = import_hint {
        if import_hint_includes_active_branch(hint) {
            return hint.recommended_command.clone();
        }
    }
    String::new()
}

fn git_unmerged_paths(repo: &Repository) -> Result<Vec<String>> {
    let git = match gix::discover(repo.root()) {
        Ok(git) => git,
        Err(_) => return Ok(Vec::new()),
    };
    let index = match git.index_or_empty() {
        Ok(index) => index,
        Err(_) => return Ok(Vec::new()),
    };
    let mut paths = BTreeSet::new();
    for (_, path) in index.entries_with_paths_by_filter_map(|path, entry| {
        (entry.stage_raw() != 0).then(|| path.to_str_lossy().into_owned())
    }) {
        paths.insert(path);
    }
    Ok(paths.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_git_operation_handoff_recommends_heddle_preservation_not_git_cli() {
        let operation = RepositoryOperationStatus {
            scope: OperationScope::Git,
            kind: OperationKind::Merge,
            in_progress: true,
            state: "in-progress".to_string(),
            message: "Git merge is in progress".to_string(),
            next_action: raw_git_preservation_command(),
        };
        let output =
            raw_git_operation_handoff("continue", &operation, vec!["conflict.txt".to_string()]);
        assert_eq!(output.status, "blocked");
        assert_eq!(
            output.recommended_action.as_deref(),
            Some("heddle bridge git status")
        );
        assert!(output.message.contains("no-git runtime"));
        assert!(output.message.contains("conflict.txt"));
        assert!(
            output
                .blockers
                .iter()
                .any(|path| path == "unresolved: conflict.txt")
        );
        assert!(
            !output
                .recommended_action
                .as_deref()
                .is_some_and(|action| action.starts_with("git "))
        );
    }
}
