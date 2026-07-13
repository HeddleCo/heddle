// SPDX-License-Identifier: Apache-2.0
//! Heddle-native thread shaping helpers.

use std::path::Path;

use anyhow::{Result, anyhow};
use heddle_core::{
    CaptureSplitOptions, ThreadMoveOptions, ThreadShapingError, capture_split,
    is_manual_review_blocker, thread_move,
};
use objects::object::ThreadName;
use repo::{GitImportGuidance, GitRemoteTrackingStatus, Repository, RepositoryOperationStatus};
use serde::Serialize;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    merge::merge_thread_into_current,
    next_action::{NextActionValidationContext, write_command_json},
    operator_core::{OperatorAction, OperatorCommandOutput},
    operator_loop::primary_next_action,
    ready_cmd::worktree_dirty,
    snapshot::{SnapshotAgentOverrides, create_snapshot, ensure_current_state},
    thread_cmd::{
        capture_thread_update_before, current_thread_ref_state, load_thread, refresh_thread,
        refresh_thread_freshness, save_thread_update_with_oplog, thread_not_found_advice,
    },
    thread_landing::{land_command_for_thread, land_local_command},
    verification_health::{RepositoryVerificationState, build_repository_verification_state},
};
use crate::{
    cli::{
        Cli, output_is_compact, render::shell_quote, should_output_json, style,
        worktree_status_options,
    },
    config::UserConfig,
};

#[derive(Debug, Serialize)]
pub struct ThreadResolveOutput {
    #[serde(flatten)]
    pub operator: OperatorCommandOutput,
    pub thread: String,
}

impl super::compact::CompactProjection for ThreadResolveOutput {
    fn compact(&self) -> super::compact::CompactOutput {
        <OperatorCommandOutput as super::compact::CompactProjection>::compact(&self.operator)
    }
}

#[derive(Debug, Serialize)]
pub struct ThreadAbsorbOutput {
    pub thread: String,
    pub into: String,
    pub preview_only: bool,
    pub conflicts: Vec<String>,
    pub merge_state: Option<String>,
    pub message: String,
}

pub fn cmd_capture_split(
    cli: &Cli,
    into: String,
    prefixes: Vec<String>,
    intent: Option<String>,
) -> Result<()> {
    let repo = cli.open_repo()?;
    let user_config = UserConfig::load_default()?;
    let output = capture_split(
        &repo,
        CaptureSplitOptions {
            into,
            prefixes,
            intent,
            worktree_status_options: worktree_status_options(Some(repo.config())),
        },
        |target_repo, snapshot_intent| {
            Ok(create_snapshot(
                target_repo,
                &user_config,
                snapshot_intent,
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
            )?
            .change_id)
        },
    )
    .map_err(map_thread_shaping_anyhow_error)?;
    emit(cli, &output)
}

pub fn cmd_thread_move(
    cli: &Cli,
    from: String,
    to: String,
    prefixes: Vec<String>,
    message: Option<String>,
) -> Result<()> {
    let repo = cli.open_repo()?;
    let user_config = UserConfig::load_default()?;
    let output = thread_move(
        &repo,
        ThreadMoveOptions {
            from,
            to,
            prefixes,
            message,
        },
        |target_repo, snapshot_intent| {
            Ok(create_snapshot(
                target_repo,
                &user_config,
                snapshot_intent,
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
            )?
            .change_id)
        },
    )
    .map_err(map_thread_shaping_anyhow_error)?;
    emit(cli, &output)
}

pub fn cmd_thread_absorb(
    cli: &Cli,
    thread: String,
    into: Option<String>,
    message: Option<String>,
    preview: bool,
) -> Result<()> {
    let repo = cli.open_repo()?;
    let child = load_thread(&repo, &thread)?;
    let parent_id = into
        .or(child.parent_thread.clone())
        .ok_or_else(|| anyhow!(RecoveryAdvice::thread_absorb_parent_required(&child.id)))?;
    let parent = load_thread(&repo, &parent_id)?;
    let parent_repo = Repository::open(&parent.execution_path)?;
    let user_config = UserConfig::load_default()?;
    let status_options = worktree_status_options(Some(parent_repo.config()));
    if worktree_dirty(&parent_repo, &status_options)? {
        create_snapshot(
            &parent_repo,
            &user_config,
            Some(format!("Prepare absorb of {}", child.id)),
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
    }
    // Bootstrap missing current state (freshly-adopted git-overlay parent) so
    // the core merge facade has a base to absorb into instead of hard-erroring.
    let _ = ensure_current_state(
        &parent_repo,
        &user_config,
        Some(format!(
            "Bootstrap git-overlay before absorbing {}",
            child.thread
        )),
    )?;
    let output = merge_thread_into_current(
        &parent_repo,
        &child.thread,
        message,
        false,
        preview,
        false,
        false,
        false,
    )?;
    emit(
        cli,
        &ThreadAbsorbOutput {
            thread: child.id,
            into: parent_id,
            preview_only: output.preview_only,
            conflicts: output.conflicts,
            merge_state: output.merge_state,
            message: output.operator.message,
        },
    )
}

pub fn cmd_thread_resolve(cli: &Cli, thread_id: String) -> Result<()> {
    let repo = cli.open_repo()?;
    let mut thread = load_thread(&repo, &thread_id)?;
    refresh_thread_freshness(&repo, &mut thread)?;
    let source_root = if thread.execution_path.as_os_str().is_empty() {
        repo.root().to_path_buf()
    } else {
        thread.execution_path.clone()
    };
    let source_repo = Repository::open(&source_root)?;
    let rebase_state_path = source_repo.heddle_dir().join("REBASE_STATE");

    if thread.freshness == repo::ThreadFreshness::Stale {
        match refresh_thread(&repo, &thread_id, cli) {
            Ok(_) => {
                let manager = super::thread_cmd::thread_manager(&repo);
                let mut refreshed_thread = manager.load(&thread_id)?.ok_or_else(|| {
                    anyhow!(thread_not_found_advice(&thread_id, "resolve thread"))
                })?;
                let before_update =
                    capture_thread_update_before(&repo, &manager, &refreshed_thread)?;
                let resolved_state = repo
                    .refs()
                    .get_thread(&ThreadName::new(&refreshed_thread.thread))?
                    .map(|id| id.short());
                let new_state = current_thread_ref_state(&repo, &refreshed_thread)?;
                refreshed_thread.integration_policy_result.status =
                    Some("manual_resolved".to_string());
                refreshed_thread.integration_policy_result.reason =
                    Some("manual integration resolution captured".to_string());
                refreshed_thread
                    .integration_policy_result
                    .manual_resolution_state = resolved_state;
                // The stale thread refreshed cleanly (no conflicts surfaced
                // for the user to resolve), so the land message must not
                // claim a manual resolution.
                refreshed_thread
                    .integration_policy_result
                    .conflicts_resolved_manually = false;
                save_thread_update_with_oplog(
                    &repo,
                    &manager,
                    &refreshed_thread,
                    before_update,
                    new_state,
                )?;
                let operator = if rebase_state_path.exists() {
                    thread_resolve_rebase_followup_operator(
                        &source_repo,
                        &rebase_state_path,
                        &thread.id,
                    )?
                } else {
                    let trust = build_repository_verification_state(&repo);
                    thread_resolve_refresh_operator(&thread.id, &trust)
                };
                return emit_thread_resolve(
                    cli,
                    &repo,
                    &ThreadResolveOutput {
                        operator,
                        thread: thread_id,
                    },
                );
            }
            Err(err) => {
                if rebase_state_path.exists() {
                    let operator = thread_resolve_rebase_followup_operator(
                        &source_repo,
                        &rebase_state_path,
                        &thread.id,
                    )?;
                    return emit_thread_resolve(
                        cli,
                        &repo,
                        &ThreadResolveOutput {
                            operator,
                            thread: thread_id,
                        },
                    );
                }
                if let Some(operator) =
                    thread_resolve_conflict_recovery_operator(&source_repo, &thread.id)?
                {
                    return emit_thread_resolve(
                        cli,
                        &repo,
                        &ThreadResolveOutput {
                            operator,
                            thread: thread_id,
                        },
                    );
                }
                return Err(err);
            }
        }
    }

    let summary = super::thread::find_thread_summary(&repo, &thread.id)?
        .ok_or_else(|| anyhow!(thread_not_found_advice(&thread.id, "resolve thread")))?;
    let mut blockers = if rebase_state_path.exists() {
        Vec::new()
    } else {
        summary.blockers.clone()
    };
    let mut warnings = Vec::new();
    if !blockers.is_empty()
        && blockers
            .iter()
            .all(|blocker| is_manual_review_blocker(blocker))
    {
        warnings = blockers.clone();
        blockers.clear();
    }
    let mut recommended_action = summary.recommended_action.clone();
    if blockers.is_empty() && rebase_state_path.exists() {
        let rebase_state = super::rebase::load_persisted_rebase_state(&rebase_state_path)?;
        let current_state = source_repo
            .current_state()?
            .ok_or_else(|| anyhow!("Thread '{}' has no current state", thread.id))?;
        if rebase_state
            .pre_conflict_head
            .is_some_and(|head| head != current_state.change_id)
        {
            recommended_action = "heddle continue".to_string();
        } else {
            blockers.push(
                "refresh has a replay in progress; capture the manual resolution in the thread checkout, then run `heddle continue`".to_string(),
            );
        }
    }
    if blockers.is_empty()
        && !rebase_state_path.exists()
        && thread
            .integration_policy_result
            .manual_resolution_state
            .is_none()
    {
        // Bootstrap missing current state (freshly-adopted git-overlay repo)
        // so the conflict-preview merge has a base instead of hard-erroring.
        let _ = ensure_current_state(
            &repo,
            &UserConfig::load_default().unwrap_or_default(),
            Some(format!(
                "Bootstrap git-overlay before resolving {}",
                thread.id
            )),
        )?;
        let preview =
            merge_thread_into_current(&repo, &thread.id, None, false, true, false, false, false)?;
        if preview.conflict_count > 0 {
            blockers.push(format!(
                "Thread '{}' still has merge conflicts: {}",
                thread.id,
                preview.conflicts.join(", ")
            ));
            recommended_action = "heddle resolve --list".to_string();
        }
    }
    if blockers.is_empty() {
        let manager = super::thread_cmd::thread_manager(&repo);
        let before_update = capture_thread_update_before(&repo, &manager, &thread)?;
        let thread_state = before_update.state;
        thread.integration_policy_result.status = Some("manual_resolved".to_string());
        thread.integration_policy_result.reason =
            Some("manual integration resolution captured".to_string());
        thread.integration_policy_result.manual_resolution_state = repo
            .refs()
            .get_thread(&ThreadName::new(&thread.thread))?
            .map(|id| id.short());
        // Reached only after the conflict preview above came back clean
        // because the operator had captured a resolution in their checkout —
        // this is the genuine `heddle resolve` manual-resolution path.
        thread.integration_policy_result.conflicts_resolved_manually = true;
        save_thread_update_with_oplog(&repo, &manager, &thread, before_update, thread_state)?;
    }
    let recommended_action = if blockers.is_empty() {
        if rebase_state_path.exists() {
            recommended_action
        } else {
            land_command_for_thread(&repo, &summary.name)
        }
    } else {
        recommended_action
    };
    let operation = repo.operation_status()?;
    let remote_tracking = repo.git_remote_tracking_status()?;
    let import_hint = repo.git_import_guidance()?;
    let recommended_action = thread_resolve_next_action(
        &blockers,
        operation.as_ref(),
        remote_tracking.as_ref(),
        import_hint.as_ref(),
        &recommended_action,
    );
    emit_thread_resolve(
        cli,
        &repo,
        &ThreadResolveOutput {
            operator: OperatorCommandOutput {
                status: if blockers.is_empty() {
                    "completed".to_string()
                } else {
                    "blocked".to_string()
                },
                action: OperatorAction::ThreadResolve,
                message: if blockers.is_empty() {
                    if warnings.is_empty() {
                        "Thread manual resolution recorded".to_string()
                    } else {
                        "Thread manual review recorded".to_string()
                    }
                } else {
                    "Thread requires a manual follow-up".to_string()
                },
                blockers: blockers.clone(),
                warnings,
                next_action: recommended_action.clone(),
                recommended_action,
            },
            thread: summary.name.clone(),
        },
    )
}

fn thread_resolve_next_action(
    blockers: &[String],
    operation: Option<&RepositoryOperationStatus>,
    remote_tracking: Option<&GitRemoteTrackingStatus>,
    import_hint: Option<&GitImportGuidance>,
    local_action: &str,
) -> Option<String> {
    let action = if blockers.is_empty() {
        primary_next_action(operation, remote_tracking, import_hint, Some(local_action))
    } else if let Some(operation) = operation {
        operation.next_action.clone()
    } else {
        local_action.to_string()
    };
    (!action.trim().is_empty()).then_some(action)
}

fn thread_resolve_rebase_followup_operator(
    source_repo: &Repository,
    rebase_state_path: &Path,
    thread_id: &str,
) -> Result<OperatorCommandOutput> {
    let rebase_state = super::rebase::load_persisted_rebase_state(rebase_state_path)?;
    let current_state = source_repo
        .current_state()?
        .ok_or_else(|| anyhow!("Thread '{}' has no current state", thread_id))?;
    let next_action = "heddle continue".to_string();
    let mut blockers = Vec::new();
    if rebase_state
        .pre_conflict_head
        .is_none_or(|head| head == current_state.change_id)
    {
        blockers.push(
            "refresh has a replay in progress; capture the manual resolution in the thread checkout, then run `heddle continue`".to_string(),
        );
    }

    Ok(OperatorCommandOutput {
        status: if blockers.is_empty() {
            "completed".to_string()
        } else {
            "blocked".to_string()
        },
        action: OperatorAction::ThreadResolve,
        message: if blockers.is_empty() {
            "Thread manual resolution recorded; continue the rebase".to_string()
        } else {
            "Thread still requires a manual rebase resolution".to_string()
        },
        blockers,
        warnings: Vec::new(),
        next_action: Some(next_action.clone()),
        recommended_action: Some(next_action),
    })
}

fn thread_resolve_conflict_recovery_operator(
    source_repo: &Repository,
    thread_id: &str,
) -> Result<Option<OperatorCommandOutput>> {
    if !source_repo.merge_state_manager().is_merge_in_progress() {
        return Ok(None);
    }
    let unresolved = source_repo.merge_state_manager().unresolved()?;
    let repo_arg = shell_quote(&source_repo.root().display().to_string());
    let conflict_list_command = format!("heddle --repo {repo_arg} resolve --list");
    let recommended_action = unresolved
        .first()
        .map(|path| format!("heddle --repo {repo_arg} resolve {}", shell_quote(path)))
        .unwrap_or_else(|| format!("heddle --repo {repo_arg} continue"));
    let blockers = if unresolved.is_empty() {
        Vec::new()
    } else {
        unresolved
            .iter()
            .map(|path| format!("Resolve conflict marker path: {path}"))
            .collect()
    };
    Ok(Some(OperatorCommandOutput {
        status: "blocked".to_string(),
        action: OperatorAction::ThreadResolve,
        message: format!(
            "Thread '{thread_id}' has conflict markers in its checkout; resolve them there, then continue"
        ),
        blockers,
        warnings: Vec::new(),
        next_action: Some(conflict_list_command),
        recommended_action: Some(recommended_action),
    }))
}

fn thread_resolve_refresh_operator(
    thread_id: &str,
    trust: &RepositoryVerificationState,
) -> OperatorCommandOutput {
    let land_command = land_local_command(thread_id);
    if trust.verified {
        return OperatorCommandOutput {
            status: "synced".to_string(),
            action: OperatorAction::ThreadResolve,
            message: "Thread refreshed cleanly".to_string(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: Some(land_command.clone()),
            recommended_action: Some(land_command),
        };
    }

    OperatorCommandOutput::blocked_by_repository_verification(
        OperatorAction::ThreadResolve,
        format!(
            "Thread refreshed cleanly, but repository verification is blocked: {}",
            trust.summary
        ),
        trust,
    )
}

fn map_thread_shaping_anyhow_error(err: anyhow::Error) -> anyhow::Error {
    match err.downcast::<ThreadShapingError>() {
        Ok(shaping_err) => map_thread_shaping_error(shaping_err),
        Err(err) => err,
    }
}

fn map_thread_shaping_error(err: ThreadShapingError) -> anyhow::Error {
    match err {
        ThreadShapingError::NoCurrentThread => anyhow!(RecoveryAdvice::no_current_thread(
            "capture --split",
            None,
            "heddle thread switch <name>",
        )),
        ThreadShapingError::NoPathsMatched(details) => anyhow!(RecoveryAdvice::safety_refusal(
            "no_paths_matched",
            details.error,
            format!(
                "Inspect available paths with `{}`, then retry `{}` with a matching prefix.",
                details.primary_command, details.action
            ),
            details.unsafe_condition,
            details.would_change,
            "repository state was left unchanged",
            details.primary_command,
            vec![details.primary_command.to_string()],
        )),
        ThreadShapingError::ThreadNotFound { thread_id, action } => {
            anyhow!(super::thread_cmd::thread_not_found_advice(
                &thread_id, action
            ))
        }
        ThreadShapingError::ImportedGitRefNotManaged { thread_id } => {
            let reconcile_preview =
                heddle_core::status::next_action::canonical_git_repair_ref_preview_command(
                    None, &thread_id,
                );
            anyhow!(RecoveryAdvice::safety_refusal(
                "imported_git_ref_not_managed_thread",
                format!("'{thread_id}' is an imported Git ref, not a managed Heddle thread"),
                format!(
                    "Preview Git/Heddle reconciliation with `{reconcile_preview}`. Use managed threads for `ready` and `land`."
                ),
                format!(
                    "thread ref '{thread_id}' exists, but no managed thread metadata exists for it"
                ),
                "ready/land require managed thread metadata and explicit integration authority; treating an imported Git ref as landable would be ambiguous",
                "thread refs, Git refs, checkout files, and thread metadata were left unchanged",
                reconcile_preview.clone(),
                vec![reconcile_preview, "heddle thread list".to_string()],
            ))
        }
    }
}

fn emit<T: Serialize>(cli: &Cli, output: &T) -> Result<()> {
    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(output)?);
    } else {
        println!("{}", serde_json::to_string_pretty(output)?);
    }
    Ok(())
}

fn emit_thread_resolve(cli: &Cli, repo: &Repository, output: &ThreadResolveOutput) -> Result<()> {
    if should_output_json(cli, None) {
        write_command_json(
            output,
            output_is_compact(cli),
            NextActionValidationContext::new(&["thread", "resolve"], repo.capability()),
        )?;
    } else {
        println!("{}", output.operator.message);
        println!("Thread: {}", style::bold(&output.thread));
        if !output.operator.blockers.is_empty() {
            println!("{}", style::warn("Blockers:"));
            for blocker in &output.operator.blockers {
                println!("  - {}", style::warn(blocker));
            }
        }
        if !output.operator.warnings.is_empty() {
            println!("{}", style::warn("Reviewed:"));
            for warning in &output.operator.warnings {
                println!("  - {}", style::warn(warning));
            }
        }
        if let Some(next) = output
            .operator
            .recommended_action
            .as_ref()
            .or(output.operator.next_action.as_ref())
        {
            print_next(next);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands::verification_health::VerificationCheck;

    fn trust_state(verified: bool) -> RepositoryVerificationState {
        let check = VerificationCheck {
            name: "Mapping".to_string(),
            status: if verified { "clean" } else { "needs_import" }.to_string(),
            clean: verified,
            summary: if verified {
                "Git/Heddle mapping is clean"
            } else {
                "active Git branch has not been imported"
            }
            .to_string(),
            recommended_action: (!verified).then(|| "heddle import git --ref main".to_string()),
            recommended_action_template: None,
            recovery_commands: if verified {
                Vec::new()
            } else {
                vec!["heddle import git --ref main".to_string()]
            },
            recovery_action_templates: Vec::new(),
            details: std::collections::BTreeMap::new(),
        };
        let machine_contract_coverage =
            crate::cli::commands::verification_health::machine_contract_coverage();
        RepositoryVerificationState {
            verified,
            status: if verified { "clean" } else { "needs_import" }.to_string(),
            repository_mode: "git-overlay".to_string(),
            heddle_initialized: true,
            git_branch: Some("main".to_string()),
            heddle_thread: Some("main".to_string()),
            worktree_dirty: false,
            worktree_state: "clean".to_string(),
            import_state: check.status.clone(),
            mapping_state: check.status.clone(),
            remote_drift: "clean".to_string(),
            active_operation: None,
            default_remote: None,
            clone_verification: "not_applicable".to_string(),
            machine_contract: crate::cli::commands::verification_health::machine_contract_status(
                &machine_contract_coverage,
            )
            .to_string(),
            machine_contract_coverage,
            summary: check.summary.clone(),
            workflow_status: "clean".to_string(),
            workflow_summary: "no workflow attention needed".to_string(),
            recommended_action: check.recommended_action.clone().unwrap_or_default(),
            recommended_action_template: check.recommended_action_template.clone(),
            recovery_commands: check.recovery_commands.clone(),
            recovery_action_templates: check.recovery_action_templates.clone(),
            checks: vec![check],
        }
    }

    #[test]
    fn thread_resolve_reports_synced_only_when_repository_verification_is_clean() {
        let clean = thread_resolve_refresh_operator("feature/clean", &trust_state(true));
        assert_eq!(clean.status, "synced");
        assert_eq!(
            clean.recommended_action.as_deref(),
            Some("heddle land --thread feature/clean")
        );

        let blocked = thread_resolve_refresh_operator("feature/blocked", &trust_state(false));
        assert_eq!(blocked.status, "blocked");
        assert!(
            blocked
                .message
                .contains("repository verification is blocked"),
            "blocked message should name verification, got: {}",
            blocked.message
        );
        assert_eq!(
            blocked.recommended_action.as_deref(),
            Some("heddle import git --ref main")
        );
        assert!(
            blocked
                .blockers
                .iter()
                .any(|blocker| blocker.contains("active Git branch has not been imported")),
            "verification blocker should be surfaced: {:?}",
            blocked.blockers
        );
    }

    #[test]
    fn thread_resolve_blockers_keep_local_recovery_ahead_of_remote_push() {
        let blockers = vec!["Thread still has merge conflicts".to_string()];
        let remote = GitRemoteTrackingStatus {
            branch: "main".to_string(),
            upstream: "origin/main".to_string(),
            ahead: 1,
            behind: 0,
            local_oid: None,
            upstream_oid: None,
            upstream_is_undone_checkpoint: false,
            message: "branch is ahead".to_string(),
            next_action: "heddle push".to_string(),
        };

        let action = thread_resolve_next_action(
            &blockers,
            None,
            Some(&remote),
            None,
            "heddle resolve --list",
        );

        assert_eq!(action.as_deref(), Some("heddle resolve --list"));
    }

    #[test]
    fn thread_resolve_clean_state_can_surface_remote_push() {
        let remote = GitRemoteTrackingStatus {
            branch: "main".to_string(),
            upstream: "origin/main".to_string(),
            ahead: 1,
            behind: 0,
            local_oid: None,
            upstream_oid: None,
            upstream_is_undone_checkpoint: false,
            message: "branch is ahead".to_string(),
            next_action: "heddle push".to_string(),
        };

        let action =
            thread_resolve_next_action(&[], None, Some(&remote), None, "heddle land --thread x");

        assert_eq!(action.as_deref(), Some("heddle push"));
    }

    #[test]
    fn empty_path_movement_refusals_map_to_typed_advice() {
        let split = map_thread_shaping_error(ThreadShapingError::NoPathsMatched(
            heddle_core::NoPathsMatchedDetails {
                action: "capture split",
                error: "No dirty paths matched the requested split prefixes",
                unsafe_condition: "the worktree has no dirty paths under the requested prefixes",
                would_change: "capture --split would not move any work into the target thread",
                primary_command: "heddle status",
            },
        ));
        let advice = split
            .downcast_ref::<RecoveryAdvice>()
            .expect("mapped error should carry RecoveryAdvice");
        assert_eq!(advice.kind, "no_paths_matched");
        assert_eq!(advice.primary_command, "heddle status");
        assert!(
            advice
                .to_string()
                .contains("Preserved: repository state was left unchanged"),
            "display should keep the uniform advice surface: {advice}"
        );

        let move_paths = map_thread_shaping_error(ThreadShapingError::NoPathsMatched(
            heddle_core::NoPathsMatchedDetails {
                action: "thread move",
                error: "No captured paths matched the requested prefixes",
                unsafe_condition: "the source thread has no captured paths under the requested prefixes",
                would_change: "thread move would not move any captured files into the target thread",
                primary_command: "heddle thread show",
            },
        ));
        let advice = move_paths
            .downcast_ref::<RecoveryAdvice>()
            .expect("mapped error should carry RecoveryAdvice");
        assert_eq!(advice.kind, "no_paths_matched");
        assert_eq!(advice.primary_command, "heddle thread show");
        assert!(
            advice.primary_hint().contains("heddle thread show"),
            "hint should name the inspection command: {}",
            advice.primary_hint()
        );
    }
}
