// SPDX-License-Identifier: Apache-2.0
use std::{collections::BTreeSet, path::Path};

use anyhow::Result;
use chrono::Utc;
use gix::bstr::ByteSlice;
use objects::object::ThreadName;
use repo::{
    update_thread_state_from_state, GitOverlayImportHint, GitRemoteTrackingStatus, OperationKind,
    OperationScope, Repository, RepositoryOperationStatus, ThreadFreshness,
    ThreadIntegrationPolicy, ThreadManager, ThreadState,
};
use serde::{ser::SerializeStruct, Serialize, Serializer};

use super::{
    bisect::reset_bisect_state,
    git_overlay_health::{
        action_template, repository_verification_blockers,
        repository_verification_primary_command, RepositoryVerificationState,
    },
    next_action::{effective_next_action, NextActionInput},
    rebase::{
        cmd_rebase_silent, continue_rebase_for_operator, has_persisted_rebase_state,
        OperatorContinueStatus,
    },
    resolve::abort_merge_state,
    snapshot::{create_snapshot, SnapshotAgentOverrides},
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

    pub(crate) fn block_success_claim_if_verification_blocked(
        &mut self,
        trust: &RepositoryVerificationState,
        local_context: impl Into<String>,
        policy: VerificationClaimPolicy,
    ) {
        if repository_verification_allows_success_claim(self, trust, policy) {
            return;
        }
        *self = Self::blocked_by_repository_verification(
            self.action.clone(),
            format!(
                "{} reached local checks, but repository verification is blocked: {}",
                local_context.into(),
                trust.summary
            ),
            trust,
        );
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VerificationClaimPolicy {
    allow_ship_publish_followup: bool,
    allow_matching_workflow_action: bool,
}

impl VerificationClaimPolicy {
    pub(crate) fn strict() -> Self {
        Self::default()
    }

    pub(crate) fn allow_ship_publish_followup(mut self) -> Self {
        self.allow_ship_publish_followup = true;
        self
    }

    pub(crate) fn allow_matching_workflow_action(mut self) -> Self {
        self.allow_matching_workflow_action = true;
        self
    }
}

fn repository_verification_allows_success_claim(
    output: &OperatorCommandOutput,
    trust: &RepositoryVerificationState,
    policy: VerificationClaimPolicy,
) -> bool {
    if trust.verified || matches!(output.status.as_str(), "blocked" | "failed") {
        return true;
    }
    if policy.allow_ship_publish_followup
        && output.action == "ship"
        && output.status == "shipped"
        && trust.recommended_action == "heddle push"
        && matches!(
            trust.remote_drift.as_str(),
            "remote_untracked" | "remote_ahead"
        )
    {
        return true;
    }
    if policy.allow_matching_workflow_action
        && trust.workflow_status == "ready"
        && output
            .recommended_action
            .as_deref()
            .is_some_and(|action| action == trust.recommended_action)
    {
        return true;
    }
    false
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
        let next_action_template = next_action.and_then(action_template);
        let recommended_action_template = recommended_action.and_then(action_template);

        let mut len = 8;
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
        state.serialize_field("next_action_template", &next_action_template)?;
        state.serialize_field("recommended_action", &recommended_action)?;
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
        let next_action = complete_current_thread_manual_resolution(repo)?;
        return Ok(OperatorCommandOutput {
            status: "continued".to_string(),
            action: "merge".to_string(),
            message: "Completed the in-progress Heddle merge".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: next_action.clone(),
            recommended_action: next_action,
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

fn complete_current_thread_manual_resolution(repo: &Repository) -> Result<Option<String>> {
    let Some(current_thread) = repo.current_lane()? else {
        return Ok(None);
    };
    let Some(current_state) = repo.head()? else {
        return Ok(None);
    };
    let Some(current_state_obj) = repo.store().get_state(&current_state)? else {
        return Ok(None);
    };

    let manager = ThreadManager::new(repo.heddle_dir());
    let Some(mut thread) = manager.find_by_thread(&current_thread)? else {
        return Ok(None);
    };
    let Some(target_thread) = thread.target_thread.clone() else {
        return Ok(None);
    };
    let Some(target_state) = repo.refs().get_thread(&ThreadName::new(&target_thread))? else {
        return Ok(None);
    };
    let Some(target_state_obj) = repo.store().get_state(&target_state)? else {
        return Ok(None);
    };

    thread.base_state = target_state.short();
    thread.base_root = target_state_obj.tree.short();
    update_thread_state_from_state(&mut thread, &current_state_obj);
    thread.state = ThreadState::Ready;
    thread.freshness = ThreadFreshness::Current;
    thread.integration_policy_result = ThreadIntegrationPolicy {
        status: Some("manual_resolved".to_string()),
        reason: Some("manual conflict resolution captured".to_string()),
        manual_resolution_state: Some(current_state.short()),
    };
    thread.updated_at = Utc::now();
    let thread_id = thread.id.clone();
    let target = thread.target_thread.clone();
    manager.save(&thread)?;

    let action = super::thread_landing::ship_command_for_thread(repo, &thread_id);
    Ok(Some(super::thread::contextual_thread_action(
        repo,
        &thread_id,
        target.as_deref(),
        &action,
    )))
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
    effective_next_action(NextActionInput::default(
        operation,
        remote_tracking,
        import_hint,
        fallback,
    ))
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
    use std::collections::BTreeMap;

    use super::*;
    use crate::cli::commands::git_overlay_health::{machine_contract_coverage, VerificationCheck};

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
        assert!(output
            .blockers
            .iter()
            .any(|path| path == "unresolved: conflict.txt"));
        assert!(!output
            .recommended_action
            .as_deref()
            .is_some_and(|action| action.starts_with("git ")));
    }

    #[test]
    fn verification_claim_gate_blocks_local_success_claims() {
        let trust = verification_state(false, "needs_checkpoint", "heddle checkpoint -m \"...\"");
        let mut output = OperatorCommandOutput {
            status: "synced".to_string(),
            action: "sync".to_string(),
            message: "Thread is already current".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: None,
            recommended_action: None,
        };

        output.block_success_claim_if_verification_blocked(
            &trust,
            "sync",
            VerificationClaimPolicy::strict(),
        );

        assert_eq!(output.status, "blocked");
        assert_eq!(
            output.recommended_action.as_deref(),
            Some("heddle checkpoint -m \"...\"")
        );
        assert!(output
            .message
            .contains("repository verification is blocked"));
    }

    #[test]
    fn verification_claim_gate_allows_ship_publish_followup_only_by_policy() {
        let trust = verification_state(false, "remote_ahead", "heddle push");
        let shipped = || OperatorCommandOutput {
            status: "shipped".to_string(),
            action: "ship".to_string(),
            message: "Shipped thread 'feature'".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: Some("heddle push".to_string()),
            recommended_action: Some("heddle push".to_string()),
        };

        let mut strict = shipped();
        strict.block_success_claim_if_verification_blocked(
            &trust,
            "ship",
            VerificationClaimPolicy::strict(),
        );
        assert_eq!(strict.status, "blocked");

        let mut allowed = shipped();
        allowed.block_success_claim_if_verification_blocked(
            &trust,
            "ship",
            VerificationClaimPolicy::strict().allow_ship_publish_followup(),
        );
        assert_eq!(allowed.status, "shipped");
        assert_eq!(allowed.recommended_action.as_deref(), Some("heddle push"));
    }

    fn verification_state(
        verified: bool,
        status: &str,
        recommended_action: &str,
    ) -> RepositoryVerificationState {
        let check = VerificationCheck {
            name: "Worktree".to_string(),
            status: status.to_string(),
            clean: verified,
            summary: "repository verification fixture".to_string(),
            recommended_action: (!verified).then(|| recommended_action.to_string()),
            recommended_action_template: None,
            recovery_commands: if verified {
                Vec::new()
            } else {
                vec![recommended_action.to_string()]
            },
            recovery_action_templates: Vec::new(),
            details: BTreeMap::new(),
        };
        RepositoryVerificationState {
            verified,
            status: status.to_string(),
            repository_mode: "git-overlay".to_string(),
            heddle_initialized: true,
            git_branch: Some("main".to_string()),
            heddle_thread: Some("main".to_string()),
            worktree_dirty: false,
            worktree_state: "clean".to_string(),
            import_state: "clean".to_string(),
            mapping_state: "clean".to_string(),
            remote_drift: status.to_string(),
            active_operation: None,
            default_remote: Some("origin".to_string()),
            clone_verification: "not_applicable".to_string(),
            machine_contract: "available".to_string(),
            machine_contract_coverage: machine_contract_coverage(),
            workflow_status: "clean".to_string(),
            workflow_summary: "workflow fixture".to_string(),
            summary: "repository verification fixture".to_string(),
            recommended_action: if verified {
                String::new()
            } else {
                recommended_action.to_string()
            },
            recommended_action_template: None,
            recovery_commands: if verified {
                Vec::new()
            } else {
                vec![recommended_action.to_string()]
            },
            recovery_action_templates: Vec::new(),
            checks: vec![check],
        }
    }
}
