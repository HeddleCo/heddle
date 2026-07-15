// SPDX-License-Identifier: Apache-2.0
use std::{cell::RefCell, collections::HashSet, fs, path::PathBuf};

use anyhow::{Context, Result, anyhow};
use heddle_core::{
    AutoLandPolicyInput, auto_land_policy_blockers as core_auto_land_policy_blockers,
    integrated_land_next_action as core_integrated_land_next_action,
    integration_blocker_recommended_action as core_integration_blocker_recommended_action,
    integration_blockers as core_integration_blockers,
    land_blockers_for_preview as core_land_blockers_for_preview,
    land_checkpoint_message as core_land_checkpoint_message,
    land_performed_steps as core_land_performed_steps,
    land_skipped_steps as core_land_skipped_steps, land_text_step as core_land_text_step,
    land_warnings_for_preview as core_land_warnings_for_preview,
    non_staleness_blockers as core_non_staleness_blockers,
    op_targets_merge_state as core_op_targets_merge_state,
    recovery_scope_checkout as core_recovery_scope_checkout,
    should_squash_land as core_should_squash_land,
};
use heddle_git_projection::GitProjection;
use objects::{
    object::{State, StateId, ThreadName},
    store::ObjectStore,
};
use oplog::{OpBatch, OpLogBackend, OpRecord};
use repo::{Repository, Thread, ThreadIntegrationPolicy, thread_flag};
use serde::{Deserialize, Serialize};

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    checkpoint::{GitCheckpointRequest, create_git_checkpoint},
    collapse::{CollapsePublishedRef, collapse_resolved_states},
    git_overlay_txn,
    merge::{build_thread_preview_report, merge_thread_into_current},
    next_action::{NextActionValidationContext, write_command_json},
    operator_core::{
        OperatorAction, OperatorCommandOutput, VerificationClaimPolicy,
        fail_if_blocked_operator_status,
    },
    operator_loop::primary_next_action,
    ready_cmd::worktree_dirty,
    snapshot::{SnapshotAgentOverrides, create_snapshot, ensure_current_state},
    thread_cmd::{
        current_thread, load_thread, refresh_thread, refresh_thread_freshness, thread_manager,
        thread_not_found_advice,
    },
    thread_landing::{land_local_command, switch_thread_command},
    undo::undo_batches_quiet,
    verification_health::{
        RepositoryVerificationState, build_repository_verification_state, remote_drift_decision,
    },
    worktree_safety::ensure_worktree_clean,
};
use crate::{
    cli::{
        Cli,
        cli_args::{LandArgs, SyncArgs},
        output_is_compact, should_output_json, style, worktree_status_options,
    },
    config::UserConfig,
};

#[derive(Serialize)]
struct SyncOutput {
    #[serde(flatten)]
    operator: OperatorCommandOutput,
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
    thread: String,
    current_state: Option<String>,
    chosen_path: String,
}

#[derive(Debug, Clone, Serialize)]
struct SiblingRestackFailure {
    thread: String,
    message: String,
}

#[derive(Debug, Default)]
struct SiblingRestackReport {
    restacked: Vec<String>,
    failed: Vec<SiblingRestackFailure>,
}

#[derive(Serialize)]
struct LandOutput {
    #[serde(flatten)]
    operator: OperatorCommandOutput,
    thread: String,
    captured: bool,
    checkpointed: bool,
    git_commit: Option<String>,
    synced: bool,
    integrated: bool,
    performed_steps: Vec<String>,
    skipped_steps: Vec<String>,
    merge_state: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    siblings_restacked: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    siblings_restack_failed: Vec<SiblingRestackFailure>,
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
    chosen_path: String,
}

#[derive(Debug, Clone, Serialize)]
struct MultiLandPeerResult {
    thread: String,
    status: String,
    message: String,
    captured: bool,
    checkpointed: bool,
    git_commit: Option<String>,
    integrated: bool,
    synced: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    siblings_restacked: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct MultiLandOutput {
    output_kind: &'static str,
    status: String,
    action: &'static str,
    message: String,
    threads: Vec<String>,
    landed: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stopped_at: Option<String>,
    peers: Vec<MultiLandPeerResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "verification")]
    trust: Option<RepositoryVerificationState>,
}

impl super::compact::CompactProjection for MultiLandOutput {
    fn compact(&self) -> super::compact::CompactOutput {
        let mut out = super::compact::CompactOutput::new(self.output_kind);
        out.status = Some(self.status.clone());
        out.next_action = self.recommended_action.clone();
        if let Some(stop) = &self.stopped_at {
            out.blockers = vec![format!("stopped at {stop}")];
        }
        out
    }
}

thread_local! {
    static MULTI_LAND_COLLECTOR: RefCell<Option<Vec<MultiLandPeerResult>>> =
        const { RefCell::new(None) };
}

pub async fn cmd_sync(cli: &Cli, args: SyncArgs) -> Result<()> {
    let repo = cli.open_repo()?;
    let mut thread = resolve_thread(
        &repo,
        args.thread.as_deref(),
        "sync",
        "heddle sync --thread <name>",
    )?;

    let stale_report = build_thread_preview_report(&repo, &mut thread, true)?;
    let stale_blockers = non_staleness_blockers(&stale_report.blockers);
    let operation = repo.operation_status()?;
    let remote_tracking = repo.git_remote_tracking_status()?;
    let import_hint = repo.git_import_guidance()?;
    let mut output = if thread.freshness == repo::ThreadFreshness::Current {
        let recommended_action = primary_next_action(
            operation.as_ref(),
            remote_tracking.as_ref(),
            import_hint.as_ref(),
            Some(&land_local_command(&thread.id)),
        );
        let trust = build_repository_verification_state(&repo);
        SyncOutput {
            operator: OperatorCommandOutput {
                status: "current".to_string(),
                action: OperatorAction::Sync,
                message: format!("Thread '{}' is already current", thread.id),
                blockers: vec![],
                warnings: Vec::new(),
                next_action: Some(recommended_action.clone()),
                recommended_action: Some(recommended_action),
            },
            trust,
            thread: thread.id.clone(),
            current_state: thread.current_state.clone(),
            chosen_path: "no_op".to_string(),
        }
    } else if stale_report.conflict_count == 0 && !stale_blockers.is_empty() {
        // Genuine non-conflict blockers (e.g. failing verification) cannot be
        // auto-refreshed away. Surface the blocker without a refresh. (The
        // conflict case carries a "path conflict(s)" blocker too, but it is
        // routed to the refresh attempt below so the breadcrumb materializes.)
        let recommended_action = if stale_report.recommended_action.trim().is_empty()
            || stale_report.recommended_action.starts_with("heddle sync")
        {
            String::new()
        } else {
            primary_next_action(
                operation.as_ref(),
                remote_tracking.as_ref(),
                import_hint.as_ref(),
                Some(&stale_report.recommended_action),
            )
        };
        update_integration_policy(
            &repo,
            &thread.id,
            "blocked",
            stale_blockers
                .first()
                .cloned()
                .unwrap_or_else(|| "refresh requires manual resolution".to_string()),
        )?;
        let trust = build_repository_verification_state(&repo);
        SyncOutput {
            operator: OperatorCommandOutput {
                status: "blocked".to_string(),
                action: OperatorAction::Sync,
                message: format!("Thread '{}' needs manual sync", thread.id),
                blockers: stale_report.blockers.clone(),
                warnings: Vec::new(),
                next_action: non_empty_next_action(&recommended_action),
                recommended_action: non_empty_next_action(&recommended_action),
            },
            trust,
            thread: thread.id.clone(),
            current_state: thread.current_state.clone(),
            chosen_path: "blocked".to_string(),
        }
    } else {
        // Either a clean stale thread or one whose replay genuinely conflicts
        // (conflict_count > 0). Attempt the refresh in both cases.
        // `refresh_thread` persists the merge state + worktree conflict
        // markers in the thread's checkout *before* returning the conflict
        // advice, so the `resolve` breadcrumb below points at real state.
        // heddle#464 r2: the old conflict branch returned here early and
        // emitted `heddle resolve --list` with no merge in progress — a dead
        // breadcrumb that failed with `no_merge_in_progress`. A previewed
        // conflict that the 3-way merge resolves cleanly also completes here
        // and recommends `land`.
        match refresh_thread(&repo, &thread.id, cli) {
            Ok(refreshed) => {
                update_integration_policy(
                    &repo,
                    &refreshed.id,
                    "current",
                    "thread refreshed cleanly",
                )?;
                let recommended_action = primary_next_action(
                    operation.as_ref(),
                    remote_tracking.as_ref(),
                    import_hint.as_ref(),
                    Some(&land_local_command(&refreshed.id)),
                );
                let trust = build_repository_verification_state(&repo);
                SyncOutput {
                    operator: OperatorCommandOutput {
                        status: "refreshed".to_string(),
                        action: OperatorAction::Sync,
                        message: format!("Refreshed thread '{}'", refreshed.id),
                        blockers: vec![],
                        warnings: Vec::new(),
                        next_action: Some(recommended_action.clone()),
                        recommended_action: Some(recommended_action),
                    },
                    trust,
                    thread: refreshed.id.clone(),
                    current_state: refreshed.current_state.clone(),
                    chosen_path: "refresh".to_string(),
                }
            }
            Err(error) => {
                // refresh_thread materializes the conflict before returning;
                // only then is `resolve` a live breadcrumb. If no merge was
                // materialized the failure is genuine — propagate it.
                if !sync_conflict_merge_in_progress(&repo, &thread) {
                    return Err(error);
                }
                update_integration_policy(
                    &repo,
                    &thread.id,
                    "blocked",
                    "refresh produced conflicts requiring manual resolution",
                )?;
                let recommended_action = scoped_resolve_list_command(&thread);
                let trust = build_repository_verification_state(&repo);
                SyncOutput {
                    operator: OperatorCommandOutput {
                        status: "blocked".to_string(),
                        action: OperatorAction::Sync,
                        message: format!("Thread '{}' has merge conflicts to resolve", thread.id),
                        blockers: stale_report.blockers.clone(),
                        warnings: Vec::new(),
                        next_action: Some(recommended_action.clone()),
                        recommended_action: Some(recommended_action),
                    },
                    trust,
                    thread: thread.id.clone(),
                    current_state: thread.current_state.clone(),
                    chosen_path: "blocked".to_string(),
                }
            }
        }
    };
    output.operator.block_success_claim_if_verification_blocked(
        &output.trust,
        "sync",
        VerificationClaimPolicy::strict(),
    );

    write_sync_output(cli, &repo, &output)
}

pub async fn cmd_land(cli: &Cli, args: LandArgs) -> Result<()> {
    if !args.threads.is_empty() {
        return cmd_land_many(cli, args).await;
    }

    // Open at CWD only to discover the active thread, then re-open at
    // its metadata-recorded worktree. This makes `heddle land` work
    // from anywhere — operators don't need to `cd` into a lightweight
    // thread directory before landing. The capture/merge below run
    // against `repo`, so they all see the same checkout. See
    // `Repository::active_worktree_path`.
    let cwd_repo = cli.open_repo()?;
    let target_path = cwd_repo.active_worktree_path()?;
    let repo = if target_path == *cwd_repo.root() {
        cwd_repo
    } else {
        Repository::open(&target_path)?
    };
    recover_incomplete_land_if_present(&repo)?;
    let user_config = UserConfig::load_default().unwrap_or_default();
    let thread = resolve_thread(
        &repo,
        args.thread.as_deref(),
        "land",
        "heddle land --thread <name>",
    )?;
    let thread_repo = if thread.execution_path.as_os_str().is_empty() {
        None
    } else if thread.execution_path.exists() {
        Some(Repository::open(&thread.execution_path).with_context(|| {
            format!(
                "opening thread '{}' worktree at {}",
                thread.id,
                thread.execution_path.display()
            )
        })?)
    } else {
        let land_command = land_local_command(&thread.id);
        // `heddle start` would refuse here — the thread still holds an active
        // reservation, so it returns `active_reservation_advice` and the
        // operator is stuck. `heddle thread switch` rebuilds the dedicated worktree at
        // the recorded `execution_path` from the thread's current state (see
        // `cmd_thread_switch`), which is exactly the path this `land` reads, so
        // the rebuild clears the blocker and the follow-up `land` succeeds.
        let switch_command = switch_thread_command(&thread.id);
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "thread_worktree_missing",
            format!("Thread '{}' worktree is missing", thread.id),
            format!(
                "Rebuild the thread's checkout with `{switch_command}` (it re-materializes the recorded worktree from the thread's current state), then retry `{land_command}`.",
            ),
            format!(
                "recorded execution path does not exist: {}",
                thread.execution_path.display()
            ),
            "land would need to inspect that checkout for unsaved work before merging",
            "repository state, refs, metadata, and worktree files were left unchanged",
            switch_command.clone(),
            vec![switch_command, land_command],
        )));
    };
    let remote_synced = sync_remote_before_land_if_needed(&repo, &thread.id)?;
    git_overlay_txn::preflight_land_checkpoint(&repo, &thread.id)?;

    let mut captured = false;
    if let Some(thread_repo) = thread_repo.as_ref() {
        let status_options = worktree_status_options(Some(thread_repo.config()));
        if worktree_dirty(thread_repo, &status_options)? {
            let capture_message = args
                .message
                .clone()
                .or_else(|| Some(format!("Land {}", thread.id)));
            create_snapshot(
                thread_repo,
                &user_config,
                capture_message,
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
            captured = true;
        }
    }

    let mut synced = remote_synced;
    let mut refreshed_thread = resolve_thread(
        &repo,
        Some(&thread.id),
        "land",
        "heddle land --thread <name>",
    )?;
    refresh_thread_freshness(&repo, &mut refreshed_thread)?;
    if refreshed_thread.freshness == repo::ThreadFreshness::Stale {
        let preview = build_thread_preview_report(&repo, &mut refreshed_thread, true)?;
        let stale_blockers = non_staleness_blockers(&preview.blockers);
        if preview.conflict_count == 0 && !stale_blockers.is_empty() {
            update_integration_policy(
                &repo,
                &refreshed_thread.id,
                "blocked",
                stale_blockers
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "sync requires manual resolution".to_string()),
            )?;
            return write_land_output(
                cli,
                &repo,
                &LandOutput {
                    operator: OperatorCommandOutput {
                        status: "blocked".to_string(),
                        action: OperatorAction::Land,
                        message: format!(
                            "Thread '{}' must be synced manually",
                            refreshed_thread.id
                        ),
                        blockers: land_blockers_for_preview(&preview, &stale_blockers),
                        warnings: Vec::new(),
                        next_action: Some(format!(
                            "heddle sync {}",
                            thread_flag(&refreshed_thread.id)
                        )),
                        recommended_action: Some(format!(
                            "heddle sync {}",
                            thread_flag(&refreshed_thread.id)
                        )),
                    },
                    thread: refreshed_thread.id.clone(),
                    captured,
                    checkpointed: false,
                    git_commit: None,
                    synced: false,
                    integrated: false,
                    merge_state: None,
                    siblings_restacked: Vec::new(),
                    siblings_restack_failed: Vec::new(),
                    trust: build_repository_verification_state(&repo),
                    chosen_path: "blocked".to_string(),
                    performed_steps: land_performed_steps(captured, false, false, false),
                    skipped_steps: land_skipped_steps(captured, false, false, false),
                },
            );
        }

        match refresh_thread(&repo, &refreshed_thread.id, cli) {
            Ok(refreshed) => {
                update_integration_policy(
                    &repo,
                    &refreshed.id,
                    "current",
                    "thread synced during land",
                )?;
                refreshed_thread = refreshed;
                synced = true;
            }
            Err(error) => {
                if !sync_conflict_merge_in_progress(&repo, &refreshed_thread) {
                    return Err(error);
                }
                update_integration_policy(
                    &repo,
                    &refreshed_thread.id,
                    "blocked",
                    "land sync produced conflicts requiring manual resolution",
                )?;
                let recommended_action = scoped_resolve_list_command(&refreshed_thread);
                return write_land_output(
                    cli,
                    &repo,
                    &LandOutput {
                        operator: OperatorCommandOutput {
                            status: "blocked".to_string(),
                            action: OperatorAction::Land,
                            message: format!(
                                "Thread '{}' has merge conflicts to resolve",
                                refreshed_thread.id
                            ),
                            blockers: land_blockers_for_preview(&preview, &stale_blockers),
                            warnings: Vec::new(),
                            next_action: Some(recommended_action.clone()),
                            recommended_action: Some(recommended_action),
                        },
                        thread: refreshed_thread.id.clone(),
                        captured,
                        checkpointed: false,
                        git_commit: None,
                        synced: false,
                        integrated: false,
                        merge_state: None,
                        siblings_restacked: Vec::new(),
                        siblings_restack_failed: Vec::new(),
                        trust: build_repository_verification_state(&repo),
                        chosen_path: "blocked".to_string(),
                        performed_steps: land_performed_steps(captured, synced, false, false),
                        skipped_steps: land_skipped_steps(captured, synced, false, false),
                    },
                );
            }
        }
    }

    let mut merge_thread = resolve_thread(
        &repo,
        Some(&refreshed_thread.id),
        "land",
        "heddle land --thread <name>",
    )?;
    let preview = build_thread_preview_report(&repo, &mut merge_thread, true)?;
    let preview_warnings = land_warnings_for_preview(&preview);
    let integration_blockers = integration_blockers(&repo, &merge_thread, &preview);
    let manual_resolution_current = manual_resolution_current(&repo, &merge_thread);
    let squash_land = should_squash_land(&args, &user_config);
    if manual_resolution_current {
        let land_collapse_state = if squash_land
            && repo.capability() == repo::RepositoryCapability::GitOverlay
        {
            collapse_thread_for_land(&repo, &user_config, &merge_thread, args.message.as_deref())?
        } else {
            None
        };
        if land_collapse_state.is_some() {
            merge_thread = resolve_thread(
                &repo,
                Some(&merge_thread.id),
                "land",
                "heddle land --thread <name>",
            )?;
        }
        let merge_state = adopt_manual_resolution(&repo, &merge_thread.id)?;
        let mut checkpointed = false;
        let mut git_commit = None;
        update_integration_policy(
            &repo,
            &merge_thread.id,
            "auto_integrated",
            "accepted manually resolved integration state",
        )?;
        if repo.capability() == repo::RepositoryCapability::GitOverlay {
            if let Err(error) = write_incomplete_land_marker(
                &repo,
                &merge_thread.id,
                Some(&merge_state),
                land_collapse_state.as_ref(),
            ) {
                return Err(land_checkpoint_failure_after_heddle(
                    &repo,
                    &merge_thread.id,
                    error,
                    Some(&merge_state),
                    land_collapse_state.as_ref(),
                    land_performed_steps(captured, synced, true, false),
                ));
            }
            let checkpoint_message = land_checkpoint_message(
                &repo,
                &merge_thread,
                args.message.as_deref(),
                land_collapse_state.is_some(),
            );
            let checkpoint = create_git_checkpoint(
                &repo,
                GitCheckpointRequest {
                    action: "land",
                    message: Some(&checkpoint_message),
                    retry_command: "heddle land --thread <name>",
                    linearize_git_parent: multi_land_has_checkpointed_peer(),
                },
                worktree_status_options(Some(repo.config())),
            );
            let record = finish_land_git_checkpoint(
                &repo,
                &merge_thread.id,
                Some(&merge_state),
                land_collapse_state.as_ref(),
                land_performed_steps(captured, synced, true, false),
                checkpoint,
            )?;
            checkpointed = true;
            git_commit = Some(record.git_commit);
        }
        coalesce_land_integration_and_checkpoint(
            &repo,
            Some(&merge_state),
            git_commit.as_deref(),
            land_collapse_state.as_ref(),
        )
        .context(
            "land completed but failed to record manual integration and Git checkpoint as one undo batch",
        )?;
        let resolved_manually = merge_thread
            .integration_policy_result
            .conflicts_resolved_manually;
        clear_manual_resolution_state(&repo, &merge_thread.id)?;
        let trust = git_overlay_txn::post_verify(&repo);
        let post_land_action = integrated_land_next_action(true, &trust);
        let mut operator = OperatorCommandOutput {
            status: "landed".to_string(),
            action: OperatorAction::Land,
            // Both genuine manual resolutions and fully-automatic conflict-free
            // integrations are adopted through this `manual_resolution_state`
            // branch (e.g. two threads forked from the same base that touch
            // disjoint files merge cleanly via a 3-way merge, yet still record a
            // resolution state to mark the thread land-ready). Only the former
            // should be reported as "manually resolved" — claiming a manual
            // resolution for an auto-clean merge is misleading.
            message: if resolved_manually {
                format!(
                    "Landed thread '{}' from a manually resolved integration state",
                    merge_thread.id
                )
            } else {
                format!(
                    "Landed thread '{}' via an automatic integration merge",
                    merge_thread.id
                )
            },
            blockers: Vec::new(),
            warnings: preview_warnings.clone(),
            next_action: post_land_action.clone(),
            recommended_action: post_land_action,
        };
        operator.block_success_claim_if_verification_blocked(
            &trust,
            "land",
            VerificationClaimPolicy::strict().allow_land_publish_followup(),
        );
        let sibling_restack =
            apply_sibling_restack_after_land(&repo, &merge_thread, cli, true, &mut operator);
        return write_land_output(
            cli,
            &repo,
            &LandOutput {
                operator,
                thread: merge_thread.id.clone(),
                captured,
                checkpointed,
                git_commit,
                synced,
                integrated: true,
                merge_state: Some(merge_state),
                siblings_restacked: sibling_restack.restacked,
                siblings_restack_failed: sibling_restack.failed,
                trust,
                performed_steps: land_performed_steps(captured, synced, true, checkpointed),
                skipped_steps: land_skipped_steps(captured, synced, true, checkpointed),
                chosen_path: if checkpointed {
                    "capture_sync_manual_resolution_checkpoint".to_string()
                } else {
                    "capture_sync_manual_resolution".to_string()
                },
            },
        );
    }
    if preview.conflict_count > 0 || !integration_blockers.is_empty() {
        let reason = integration_blockers
            .first()
            .cloned()
            .unwrap_or_else(|| "integration requires manual review".to_string());
        let recovery_scope = recovery_scope_checkout(&merge_thread, repo.root());
        let policy_recovery_action = integration_blocker_recommended_action(
            &integration_blockers,
            recovery_scope.as_deref(),
        );
        if preview.conflict_count > 0
            && policy_recovery_action.is_none()
            && materialize_land_conflict_for_thread(&repo, &merge_thread)?
        {
            update_integration_policy(&repo, &merge_thread.id, "blocked", &reason)?;
            let recommended_action = scoped_resolve_list_command(&merge_thread);
            return write_land_output(
                cli,
                &repo,
                &LandOutput {
                    operator: OperatorCommandOutput {
                        status: "blocked".to_string(),
                        action: OperatorAction::Land,
                        message: format!(
                            "Thread '{}' has merge conflicts to resolve",
                            merge_thread.id
                        ),
                        blockers: land_blockers_for_preview(&preview, &integration_blockers),
                        warnings: preview_warnings.clone(),
                        next_action: Some(recommended_action.clone()),
                        recommended_action: Some(recommended_action),
                    },
                    thread: merge_thread.id.clone(),
                    captured,
                    checkpointed: false,
                    git_commit: None,
                    synced: false,
                    integrated: false,
                    merge_state: None,
                    siblings_restacked: Vec::new(),
                    siblings_restack_failed: Vec::new(),
                    trust: build_repository_verification_state(&repo),
                    chosen_path: "blocked".to_string(),
                    performed_steps: land_performed_steps(captured, synced, false, false),
                    skipped_steps: land_skipped_steps(captured, synced, false, false),
                },
            );
        }
        // Never fall back to `preview.recommended_action` here: this is the
        // pre-merge bail, so a preview-originated `resolve` breadcrumb could
        // die with `no_merge_in_progress`, and the preview's own land
        // recommendation would self-loop this very command. When materializing
        // was not possible, drive the operator through the explicit sync path.
        let recommended_action = policy_recovery_action
            .unwrap_or_else(|| format!("heddle sync {}", thread_flag(&merge_thread.id)));
        update_integration_policy(&repo, &merge_thread.id, "blocked", &reason)?;
        return write_land_output(
            cli,
            &repo,
            &LandOutput {
                operator: OperatorCommandOutput {
                    status: "blocked".to_string(),
                    action: OperatorAction::Land,
                    message: format!("Thread '{}' is not eligible for auto-land", merge_thread.id),
                    blockers: land_blockers_for_preview(&preview, &integration_blockers),
                    warnings: preview_warnings.clone(),
                    next_action: Some(recommended_action.clone()),
                    recommended_action: Some(recommended_action),
                },
                thread: merge_thread.id.clone(),
                captured,
                checkpointed: false,
                git_commit: None,
                synced,
                integrated: false,
                merge_state: None,
                siblings_restacked: Vec::new(),
                siblings_restack_failed: Vec::new(),
                trust: build_repository_verification_state(&repo),
                chosen_path: "blocked".to_string(),
                performed_steps: land_performed_steps(captured, synced, false, false),
                skipped_steps: land_skipped_steps(captured, synced, false, false),
            },
        );
    }

    let land_collapse_state =
        if squash_land && repo.capability() == repo::RepositoryCapability::GitOverlay {
            collapse_thread_for_land(&repo, &user_config, &merge_thread, args.message.as_deref())?
        } else {
            None
        };
    if land_collapse_state.is_some() {
        merge_thread = resolve_thread(
            &repo,
            Some(&merge_thread.id),
            "land",
            "heddle land --thread <name>",
        )?;
    }

    // Bootstrap missing current state (freshly-adopted git-overlay repos) so
    // the core merge facade has a base to merge into instead of hard-erroring.
    let _ = ensure_current_state(
        &repo,
        &user_config,
        Some(format!(
            "Bootstrap git-overlay before landing {}",
            merge_thread.id
        )),
    )?;
    let merge_output = merge_thread_into_current(
        &repo,
        &merge_thread.id,
        None,
        false,
        false,
        false,
        false,
        false,
    )?;
    let integrated = merge_output.conflicts.is_empty() && merge_output.merge_state.is_some();
    let mut checkpointed = false;
    let mut git_commit = None;
    update_integration_policy(
        &repo,
        &merge_thread.id,
        if integrated {
            "auto_integrated"
        } else {
            "blocked"
        },
        if integrated {
            "clean integration path"
        } else {
            "merge produced conflicts"
        },
    )?;

    if integrated && repo.capability() == repo::RepositoryCapability::GitOverlay {
        if let Err(error) = write_incomplete_land_marker(
            &repo,
            &merge_thread.id,
            merge_output.merge_state.as_deref(),
            land_collapse_state.as_ref(),
        ) {
            return Err(land_checkpoint_failure_after_heddle(
                &repo,
                &merge_thread.id,
                error,
                merge_output.merge_state.as_deref(),
                land_collapse_state.as_ref(),
                land_performed_steps(captured, synced, integrated, false),
            ));
        }
        let checkpoint_message = land_checkpoint_message(
            &repo,
            &merge_thread,
            args.message.as_deref(),
            land_collapse_state.is_some(),
        );
        let checkpoint = create_git_checkpoint(
            &repo,
            GitCheckpointRequest {
                action: "land",
                message: Some(&checkpoint_message),
                retry_command: "heddle land --thread <name>",
                linearize_git_parent: multi_land_has_checkpointed_peer(),
            },
            worktree_status_options(Some(repo.config())),
        );
        let record = finish_land_git_checkpoint(
            &repo,
            &merge_thread.id,
            merge_output.merge_state.as_deref(),
            land_collapse_state.as_ref(),
            land_performed_steps(captured, synced, integrated, false),
            checkpoint,
        )?;
        checkpointed = true;
        git_commit = Some(record.git_commit);
    }
    coalesce_land_integration_and_checkpoint(
        &repo,
        merge_output.merge_state.as_deref(),
        git_commit.as_deref(),
        land_collapse_state.as_ref(),
    )
    .context("land completed but failed to record merge and Git checkpoint as one undo batch")?;

    if integrated {
        clear_manual_resolution_state(&repo, &merge_thread.id)?;
    }

    let trust = git_overlay_txn::post_verify(&repo);
    let integrated_next_action = integrated_land_next_action(integrated, &trust);
    let mut operator = OperatorCommandOutput {
        status: if integrated { "landed" } else { "blocked" }.to_string(),
        action: OperatorAction::Land,
        message: if integrated {
            format!("Landed thread '{}'", merge_thread.id)
        } else {
            format!("Thread '{}' could not be landed cleanly", merge_thread.id)
        },
        blockers: merge_output.operator.blockers.clone(),
        warnings: preview_warnings,
        next_action: if integrated {
            integrated_next_action.clone()
        } else {
            merge_output.operator.recommended_action.clone()
        },
        recommended_action: if integrated {
            integrated_next_action
        } else {
            merge_output.operator.recommended_action.clone()
        },
    };
    operator.block_success_claim_if_verification_blocked(
        &trust,
        "land",
        VerificationClaimPolicy::strict().allow_land_publish_followup(),
    );
    let sibling_restack =
        apply_sibling_restack_after_land(&repo, &merge_thread, cli, integrated, &mut operator);

    write_land_output(
        cli,
        &repo,
        &LandOutput {
            operator,
            thread: merge_thread.id.clone(),
            captured,
            checkpointed,
            git_commit,
            synced,
            integrated,
            merge_state: merge_output.merge_state.clone(),
            siblings_restacked: sibling_restack.restacked,
            siblings_restack_failed: sibling_restack.failed,
            trust,
            performed_steps: land_performed_steps(captured, synced, integrated, checkpointed),
            skipped_steps: land_skipped_steps(captured, synced, integrated, checkpointed),
            chosen_path: if integrated {
                if checkpointed {
                    "capture_sync_merge_checkpoint"
                } else {
                    "capture_sync_merge"
                }
                .to_string()
            } else {
                "blocked".to_string()
            },
        },
    )
}

fn should_squash_land(args: &LandArgs, user_config: &UserConfig) -> bool {
    core_should_squash_land(args.no_squash, user_config.land.squash)
}

fn sync_remote_before_land_if_needed(repo: &Repository, thread_id: &str) -> Result<bool> {
    let Some(remote) = repo.git_remote_tracking_status()? else {
        return Ok(false);
    };
    let remote_decision = remote_drift_decision(repo, &remote);
    if remote_decision.status != "remote_behind" {
        return Ok(false);
    }

    ensure_worktree_clean(repo, "land")?;
    let remote_name = super::remote::resolve_default_remote_name(repo, None)?;
    let mut bridge = GitProjection::new(repo);
    bridge.pull(&remote_name)?;

    let trust = git_overlay_txn::post_verify(repo);
    if !trust.verified {
        let primary_command = if trust.recommended_action.trim().is_empty() {
            "heddle status".to_string()
        } else {
            trust.recommended_action.clone()
        };
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "land_remote_sync_blocked",
            format!(
                "Synced remote state before landing '{thread_id}', but repository verification is still blocked"
            ),
            format!("Run `{primary_command}`, then retry the land."),
            format!(
                "repository verification reports {}: {}",
                trust.status, trust.summary
            ),
            "land must not continue into integration while repository verification is blocked",
            "remote state was imported; thread refs and worktree changes from the land were left unchanged",
            primary_command.clone(),
            vec![primary_command],
        )));
    }

    Ok(true)
}

fn restack_sibling_threads_after_land(
    repo: &Repository,
    landed: &Thread,
    cli: &Cli,
) -> SiblingRestackReport {
    let Some(target) = landed.target_thread.as_deref() else {
        return SiblingRestackReport::default();
    };

    let Ok(threads) = thread_manager(repo).list() else {
        return SiblingRestackReport::default();
    };
    let mut candidates: Vec<Thread> = threads
        .into_iter()
        .filter(|thread| {
            thread.id != landed.id
                && thread.thread != landed.thread
                && thread.target_thread.as_deref() == Some(target)
                && matches!(
                    thread.state,
                    repo::ThreadState::Draft
                        | repo::ThreadState::Active
                        | repo::ThreadState::Ready
                        | repo::ThreadState::Blocked
                )
        })
        .collect();
    candidates.sort_by(|a, b| a.id.cmp(&b.id));

    let mut report = SiblingRestackReport::default();
    for mut sibling in candidates {
        if let Err(error) = refresh_thread_freshness(repo, &mut sibling) {
            report.failed.push(SiblingRestackFailure {
                thread: sibling.id.clone(),
                message: format!("could not evaluate freshness: {error}"),
            });
            continue;
        }
        if sibling.freshness != repo::ThreadFreshness::Stale {
            continue;
        }
        match refresh_thread(repo, &sibling.id, cli) {
            Ok(_) => report.restacked.push(sibling.id),
            Err(error) => report.failed.push(SiblingRestackFailure {
                thread: sibling.id.clone(),
                message: sibling_restack_error_message(&error),
            }),
        }
    }
    report
}

fn sibling_restack_error_message(error: &anyhow::Error) -> String {
    if let Some(advice) = error.downcast_ref::<RecoveryAdvice>()
        && !advice.error.is_empty()
    {
        return advice.error.clone();
    }
    format!("{error:#}")
        .lines()
        .next()
        .unwrap_or("refresh failed")
        .to_string()
}

fn apply_sibling_restack_after_land(
    repo: &Repository,
    landed: &Thread,
    cli: &Cli,
    integrated: bool,
    operator: &mut OperatorCommandOutput,
) -> SiblingRestackReport {
    if !integrated {
        return SiblingRestackReport::default();
    }
    let report = restack_sibling_threads_after_land(repo, landed, cli);
    for failure in &report.failed {
        operator.warnings.push(format!(
            "sibling '{}' could not be restacked after land: {}",
            failure.thread, failure.message
        ));
    }
    report
}

fn materialize_land_conflict_for_thread(repo: &Repository, thread: &Thread) -> Result<bool> {
    let Some(target_thread) = thread.target_thread.as_deref() else {
        return Ok(false);
    };

    if thread.execution_path.as_os_str().is_empty() {
        if repo.current_lane()?.as_deref() != Some(thread.thread.as_str()) {
            return Ok(false);
        }
        return materialize_land_conflict_in_repo(repo, target_thread);
    }

    if !thread.execution_path.exists() {
        return Ok(false);
    }
    let thread_repo = Repository::open(&thread.execution_path).with_context(|| {
        format!(
            "opening thread '{}' worktree at {} to materialize land conflict",
            thread.id,
            thread.execution_path.display()
        )
    })?;
    materialize_land_conflict_in_repo(&thread_repo, target_thread)
}

fn materialize_land_conflict_in_repo(repo: &Repository, target_thread: &str) -> Result<bool> {
    // Bootstrap missing current state so freshly-adopted overlay repos have a
    // base for the conflict-materializing merge instead of hard-erroring.
    let _ = ensure_current_state(
        repo,
        &UserConfig::load_default().unwrap_or_default(),
        Some(format!(
            "Bootstrap git-overlay before materializing land conflict for {}",
            target_thread
        )),
    )?;
    let output =
        merge_thread_into_current(repo, target_thread, None, false, false, false, false, false)?;
    Ok(!output.conflicts.is_empty() && repo.merge_state_manager().is_merge_in_progress())
}

fn collapse_thread_for_land(
    repo: &Repository,
    user_config: &UserConfig,
    thread: &Thread,
    message: Option<&str>,
) -> Result<Option<StateId>> {
    let sources = thread_source_states(repo, thread)?;
    if sources.len() <= 1 {
        return Ok(None);
    }
    let intent = message
        .filter(|message| !message.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("Land {}", thread.id));
    let result = collapse_resolved_states(
        repo,
        user_config,
        &sources,
        intent,
        None,
        CollapsePublishedRef::Thread(ThreadName::new(&thread.id)),
    )?;
    Ok(Some(result.state_id))
}

fn thread_source_states(repo: &Repository, thread: &Thread) -> Result<Vec<State>> {
    let Some(tip) = repo.refs().get_thread(&ThreadName::new(&thread.id))? else {
        return Ok(Vec::new());
    };
    let base = repo.resolve_state(&thread.base_state)?;
    let base_reachable = match base {
        Some(base) => reachable_state_set(repo, base)?,
        None => HashSet::new(),
    };
    let mut visited = HashSet::new();
    let mut ordered = Vec::new();
    collect_thread_sources(repo, tip, &base_reachable, &mut visited, &mut ordered)?;
    Ok(ordered)
}

fn collect_thread_sources(
    repo: &Repository,
    state_id: StateId,
    excluded: &HashSet<StateId>,
    visited: &mut HashSet<StateId>,
    ordered: &mut Vec<State>,
) -> Result<()> {
    if excluded.contains(&state_id) || !visited.insert(state_id) {
        return Ok(());
    }
    let Some(state) = repo.store().get_state(&state_id)? else {
        return Ok(());
    };
    for parent in &state.parents {
        collect_thread_sources(repo, *parent, excluded, visited, ordered)?;
    }
    ordered.push(state);
    Ok(())
}

fn reachable_state_set(repo: &Repository, root: StateId) -> Result<HashSet<StateId>> {
    let mut reachable = HashSet::new();
    let mut stack = vec![root];
    while let Some(state_id) = stack.pop() {
        if !reachable.insert(state_id) {
            continue;
        }
        if let Some(state) = repo.store().get_state(&state_id)? {
            stack.extend(state.parents.iter().copied());
        }
    }
    Ok(reachable)
}

fn land_performed_steps(
    captured: bool,
    synced: bool,
    integrated: bool,
    checkpointed: bool,
) -> Vec<String> {
    core_land_performed_steps(captured, synced, integrated, checkpointed)
}

fn land_skipped_steps(
    captured: bool,
    synced: bool,
    integrated: bool,
    checkpointed: bool,
) -> Vec<String> {
    core_land_skipped_steps(captured, synced, integrated, checkpointed)
}

fn integrated_land_next_action(
    integrated: bool,
    trust: &RepositoryVerificationState,
) -> Option<String> {
    core_integrated_land_next_action(integrated, &trust.recommended_action)
}

fn land_checkpoint_message(
    repo: &Repository,
    thread: &Thread,
    explicit: Option<&str>,
    prefer_land_subject: bool,
) -> String {
    let intent = thread
        .current_state
        .as_deref()
        .and_then(|state| repo.resolve_state(state).ok().flatten())
        .and_then(|state_id| repo.store().get_state(&state_id).ok().flatten())
        .and_then(|state| state.intent);
    core_land_checkpoint_message(
        explicit,
        prefer_land_subject,
        &thread.id,
        intent.as_deref(),
        thread.task.as_deref(),
    )
}

fn resolve_thread(
    repo: &Repository,
    thread: Option<&str>,
    command: &'static str,
    primary_command: impl Into<String>,
) -> Result<Thread> {
    match thread {
        Some(thread) => load_thread(repo, thread),
        None => current_thread(repo)?.ok_or_else(|| {
            anyhow!(RecoveryAdvice::no_current_thread(
                command,
                Some("--thread"),
                primary_command,
            ))
        }),
    }
}

fn update_integration_policy(
    repo: &Repository,
    thread_id: &str,
    status: &str,
    reason: impl Into<String>,
) -> Result<()> {
    let manager = thread_manager(repo);
    let mut thread = manager.load(thread_id)?.ok_or_else(|| {
        anyhow!(thread_not_found_advice(
            thread_id,
            "update integration policy"
        ))
    })?;
    let prior_status = thread.integration_policy_result.status.clone();
    let reason = reason.into();
    let keep_previewed = status == "blocked" && prior_status.as_deref() == Some("previewed");
    let next_status = if keep_previewed { "previewed" } else { status };
    let next_reason = if keep_previewed {
        format!("auto-land blocked: {reason}")
    } else {
        reason
    };
    if status == "blocked" {
        thread.state = repo::ThreadState::Blocked;
    }
    thread.integration_policy_result = ThreadIntegrationPolicy {
        status: Some(next_status.to_string()),
        reason: Some(next_reason),
        manual_resolution_state: thread.integration_policy_result.manual_resolution_state,
        conflicts_resolved_manually: thread.integration_policy_result.conflicts_resolved_manually,
    };
    manager.save(&thread)?;
    Ok(())
}

fn clear_manual_resolution_state(repo: &Repository, thread_id: &str) -> Result<()> {
    let manager = thread_manager(repo);
    let mut thread = manager.load(thread_id)?.ok_or_else(|| {
        anyhow!(thread_not_found_advice(
            thread_id,
            "clear manual resolution"
        ))
    })?;
    thread.integration_policy_result.manual_resolution_state = None;
    thread.integration_policy_result.conflicts_resolved_manually = false;
    Ok(manager.save(&thread)?)
}

fn coalesce_land_integration_and_checkpoint(
    repo: &Repository,
    merge_state: Option<&str>,
    git_commit: Option<&str>,
    collapse_state: Option<&StateId>,
) -> Result<()> {
    let Some(merge_state) = merge_state else {
        return Ok(());
    };
    let Some(git_commit) = git_commit else {
        return Ok(());
    };

    let integration_batch = find_recent_land_integration_batch(repo, merge_state)?;
    let checkpoint_batch = find_recent_land_git_checkpoint_batch(repo, git_commit)?;
    repo.oplog()
        .coalesce_batches(integration_batch.id, checkpoint_batch.id)?;
    if let Some(collapse_state) = collapse_state {
        let collapse_batch = find_recent_land_collapse_batch(repo, collapse_state)?;
        repo.oplog()
            .coalesce_batches(integration_batch.id, collapse_batch.id)?;
    }
    Ok(())
}

fn land_checkpoint_failure_after_heddle(
    repo: &Repository,
    thread_id: &str,
    checkpoint_error: anyhow::Error,
    merge_state: Option<&str>,
    collapse_state: Option<&StateId>,
    performed_steps: Vec<String>,
) -> anyhow::Error {
    if merge_state.is_none() && collapse_state.is_none() {
        return anyhow!(RecoveryAdvice::land_checkpoint_partial_failure(
            thread_id,
            &checkpoint_error,
            performed_steps,
        ));
    }
    match auto_undo_land_integration(repo, thread_id, merge_state, collapse_state) {
        Ok(()) => match clear_incomplete_land_marker(repo) {
            Ok(()) => anyhow!(RecoveryAdvice::land_checkpoint_rolled_back(
                thread_id,
                &checkpoint_error,
                performed_steps,
            )),
            Err(cleanup_error) => anyhow!(
                RecoveryAdvice::land_checkpoint_rollback_marker_cleanup_failed(
                    thread_id,
                    &checkpoint_error,
                    cleanup_error,
                    performed_steps,
                )
            ),
        },
        Err(undo_error) => anyhow!(RecoveryAdvice::land_checkpoint_partial_failure_undo_failed(
            thread_id,
            &checkpoint_error,
            undo_error,
            performed_steps,
        )),
    }
}

fn finish_land_git_checkpoint(
    repo: &Repository,
    thread_id: &str,
    merge_state: Option<&str>,
    collapse_state: Option<&StateId>,
    performed_steps: Vec<String>,
    checkpoint: Result<repo::GitCheckpointRecord>,
) -> Result<repo::GitCheckpointRecord> {
    let record = match checkpoint {
        Ok(record) => record,
        Err(checkpoint_error) => {
            let recovered = match merge_state {
                Some(state) => match repo.resolve_state(state) {
                    Ok(Some(state_id)) => {
                        heddle_core::recover_published_git_checkpoint(repo, &state_id)
                    }
                    Ok(None) => Ok(None),
                    Err(error) => Err(error.into()),
                },
                None => Ok(None),
            };
            match recovered {
                Ok(Some(record)) => record,
                Ok(None) => {
                    return Err(land_checkpoint_failure_after_heddle(
                        repo,
                        thread_id,
                        checkpoint_error,
                        merge_state,
                        collapse_state,
                        performed_steps,
                    ));
                }
                Err(recovery_error) => {
                    return Err(anyhow!(RecoveryAdvice::land_checkpoint_recovery_required(
                        thread_id,
                        checkpoint_error,
                        recovery_error,
                        performed_steps,
                    )));
                }
            }
        }
    };
    clear_incomplete_land_marker(repo)?;
    Ok(record)
}

fn auto_undo_land_integration(
    repo: &Repository,
    thread_id: &str,
    merge_state: Option<&str>,
    collapse_state: Option<&StateId>,
) -> Result<()> {
    let mut batches = Vec::new();
    if let Some(merge_state) = merge_state
        && let Ok(batch) = find_recent_land_integration_batch(repo, merge_state)
    {
        batches.push(batch);
    }
    if let Some(collapse_state) = collapse_state
        && let Ok(batch) = find_recent_land_collapse_batch(repo, collapse_state)
        && batches.iter().all(|existing| existing.id != batch.id)
    {
        batches.push(batch);
    }
    if batches.is_empty() {
        return Err(anyhow!(
            "no land integration oplog batch was found to roll back"
        ));
    }
    undo_batches_quiet(repo, batches)?;
    let _ = update_integration_policy(
        repo,
        thread_id,
        "blocked",
        "land rolled back after Git checkpoint failure",
    );
    Ok(())
}

fn find_recent_land_collapse_batch(repo: &Repository, collapse_state: &StateId) -> Result<OpBatch> {
    repo.oplog()
        .recent_batches_scoped(12, Some(&repo.op_scope()))?
        .into_iter()
        .find(|batch| {
            batch.entries.iter().any(|entry| {
                matches!(
                    &entry.operation,
                    OpRecord::Collapse { result, .. } if result == collapse_state
                )
            })
        })
        .ok_or_else(|| anyhow!("land squash succeeded but its oplog batch was not found"))
}

fn find_recent_land_integration_batch(repo: &Repository, merge_state: &str) -> Result<OpBatch> {
    repo.oplog()
        .recent_batches_scoped(12, Some(&repo.op_scope()))?
        .into_iter()
        .find(|batch| {
            batch
                .entries
                .iter()
                .any(|entry| op_targets_merge_state(&entry.operation, merge_state))
        })
        .ok_or_else(|| anyhow!("land merge succeeded but its oplog batch was not found"))
}

fn find_recent_land_git_checkpoint_batch(repo: &Repository, git_commit: &str) -> Result<OpBatch> {
    repo.oplog()
        .recent_batches_scoped(12, Some(&repo.op_scope()))?
        .into_iter()
        .find(|batch| {
            batch.entries.iter().any(|entry| {
                matches!(
                    &entry.operation,
                    OpRecord::GitCheckpoint { new_git_oid, .. } if new_git_oid == git_commit
                )
            })
        })
        .ok_or_else(|| anyhow!("land Git checkpoint succeeded but its oplog batch was not found"))
}

fn op_targets_merge_state(op: &OpRecord, merge_state: &str) -> bool {
    core_op_targets_merge_state(op, merge_state)
}

fn adopt_manual_resolution(repo: &Repository, thread_id: &str) -> Result<String> {
    let manager = thread_manager(repo);
    let mut thread = manager.load(thread_id)?.ok_or_else(|| {
        anyhow!(thread_not_found_advice(
            thread_id,
            "adopt manual resolution"
        ))
    })?;
    let target = repo
        .refs()
        .get_thread(&ThreadName::new(&thread.thread))?
        .ok_or_else(|| {
            anyhow!(
                "Thread '{}' has no current state to integrate",
                thread.thread
            )
        })?;
    super::ff_record::record_ff_advance(repo, &thread.thread, &target)?;
    thread.state = repo::ThreadState::Merged;
    thread.merged_state = Some(target.short());
    thread.current_state = Some(target.short());
    thread.updated_at = chrono::Utc::now();
    thread.freshness = repo::ThreadFreshness::Current;
    manager.save(&thread)?;
    Ok(target.short())
}

pub(crate) fn integration_blockers(
    repo: &Repository,
    thread: &Thread,
    preview: &super::merge::ThreadPreviewReport,
) -> Vec<String> {
    core_integration_blockers(
        manual_resolution_current(repo, thread),
        &preview.blockers,
        auto_land_policy_input(repo, thread),
    )
}

pub(crate) fn auto_land_policy_blockers(repo: &Repository, thread: &Thread) -> Vec<String> {
    core_auto_land_policy_blockers(auto_land_policy_input(repo, thread))
}

fn auto_land_policy_input(repo: &Repository, thread: &Thread) -> AutoLandPolicyInput {
    AutoLandPolicyInput {
        agent_authored: thread_is_agent_authored(repo, thread),
        confidence: thread.confidence_summary.value,
        tests_passed: thread.verification_summary.tests_passed,
    }
}

pub(crate) fn integration_blocker_recommended_action(
    blockers: &[String],
    scope_to_checkout: Option<&std::path::Path>,
) -> Option<String> {
    core_integration_blocker_recommended_action(blockers, scope_to_checkout)
}

/// Returns the thread's recorded checkout iff it is a real, distinct path from
/// `current_checkout` — i.e. when a recovery breadcrumb that mutates thread
/// state must be re-scoped away from the current checkout. Canonicalizes both
/// sides (falling back to the raw path) so a symlinked worktree doesn't read as
/// "different" and over-scope the in-thread case.
pub(crate) fn recovery_scope_checkout(
    thread: &Thread,
    current_checkout: &std::path::Path,
) -> Option<std::path::PathBuf> {
    core_recovery_scope_checkout(&thread.execution_path, current_checkout)
}

fn land_blockers_for_preview(
    preview: &super::merge::ThreadPreviewReport,
    blockers: &[String],
) -> Vec<String> {
    core_land_blockers_for_preview(preview, blockers)
}

fn land_warnings_for_preview(preview: &super::merge::ThreadPreviewReport) -> Vec<String> {
    core_land_warnings_for_preview(preview)
}

fn manual_resolution_current(repo: &Repository, thread: &Thread) -> bool {
    let thread_tip = repo
        .refs()
        .get_thread(&ThreadName::new(&thread.thread))
        .ok()
        .flatten()
        .map(|id| id.short());
    thread
        .integration_policy_result
        .manual_resolution_state
        .as_deref()
        .zip(thread_tip.as_deref())
        .is_some_and(|(resolved, current)| resolved == current)
        && thread.freshness == repo::ThreadFreshness::Current
}

fn thread_is_agent_authored(repo: &Repository, thread: &Thread) -> bool {
    let current_state = thread
        .current_state
        .as_deref()
        .and_then(|state| repo.resolve_state(state).ok().flatten())
        .or_else(|| {
            repo.refs()
                .get_thread(&ThreadName::new(&thread.thread))
                .ok()
                .flatten()
        });
    current_state
        .and_then(|id| repo.store().get_state(&id).ok().flatten())
        .map(|state| state.attribution.agent.is_some())
        .unwrap_or(false)
}

pub(crate) fn non_staleness_blockers(blockers: &[String]) -> Vec<String> {
    core_non_staleness_blockers(blockers)
}

impl super::compact::CompactProjection for SyncOutput {
    fn compact(&self) -> super::compact::CompactOutput {
        <OperatorCommandOutput as super::compact::CompactProjection>::compact(&self.operator)
    }
}

impl super::compact::CompactProjection for LandOutput {
    fn compact(&self) -> super::compact::CompactOutput {
        <OperatorCommandOutput as super::compact::CompactProjection>::compact(&self.operator)
    }
}

fn write_sync_output(cli: &Cli, repo: &Repository, output: &SyncOutput) -> Result<()> {
    if should_output_json(cli, None) {
        write_command_json(
            output,
            output_is_compact(cli),
            NextActionValidationContext::new(&["sync"], repo.capability()),
        )?;
    } else {
        println!("{}", serde_json::to_string_pretty(output)?);
    }
    Ok(())
}

fn non_empty_next_action(action: &str) -> Option<String> {
    (!action.trim().is_empty()).then(|| action.to_string())
}

/// True when `refresh_thread` materialized a conflicted merge for `thread`
/// (so `heddle resolve` has state to read). The merge state lives in the
/// thread's own checkout, which may differ from the repo `sync` ran against.
fn sync_conflict_merge_in_progress(repo: &Repository, thread: &Thread) -> bool {
    if thread.execution_path.as_os_str().is_empty() {
        repo.merge_state_manager().is_merge_in_progress()
    } else if thread.execution_path.exists() {
        Repository::open(&thread.execution_path)
            .map(|worktree| worktree.merge_state_manager().is_merge_in_progress())
            .unwrap_or(false)
    } else {
        false
    }
}

/// `heddle resolve --list` scoped to wherever the conflict was materialized.
/// When the thread has its own checkout the breadcrumb must carry `--repo` so
/// it reads the merge state in that checkout rather than the repo `sync` ran
/// against (where no merge is in progress).
fn scoped_resolve_list_command(thread: &Thread) -> String {
    if thread.execution_path.as_os_str().is_empty() {
        super::command_catalog::heddle_action(["resolve", "--list"])
    } else {
        super::command_catalog::heddle_action(vec![
            "--repo".to_string(),
            thread.execution_path.display().to_string(),
            "resolve".to_string(),
            "--list".to_string(),
        ])
    }
}

fn write_land_output(cli: &Cli, repo: &Repository, output: &LandOutput) -> Result<()> {
    if MULTI_LAND_COLLECTOR.with(|collector| collector.borrow().is_some()) {
        MULTI_LAND_COLLECTOR.with(|collector| {
            if let Some(peers) = collector.borrow_mut().as_mut() {
                peers.push(MultiLandPeerResult {
                    thread: output.thread.clone(),
                    status: output.operator.status.clone(),
                    message: output.operator.message.clone(),
                    captured: output.captured,
                    checkpointed: output.checkpointed,
                    git_commit: output.git_commit.clone(),
                    integrated: output.integrated,
                    synced: output.synced,
                    siblings_restacked: output.siblings_restacked.clone(),
                    blockers: output.operator.blockers.clone(),
                    warnings: output.operator.warnings.clone(),
                });
            }
        });
        return fail_if_blocked_operator_status(&output.operator.status);
    }
    if should_output_json(cli, None) {
        write_command_json(
            output,
            output_is_compact(cli),
            NextActionValidationContext::new(&["land"], repo.capability()),
        )?;
    } else {
        let marker = match output.operator.status.as_str() {
            "landed" => style::ok_marker(),
            "blocked" => style::warn_marker(),
            _ => style::working_marker(),
        };
        println!("{marker} {}", output.operator.message);
        println!("  {}", style::field("thread", &style::bold(&output.thread)));
        if output.integrated {
            println!("  {}", style::field("landed", "on parent"));
        } else {
            if !output.performed_steps.is_empty() {
                println!(
                    "  {}",
                    style::field(
                        "completed",
                        &output
                            .performed_steps
                            .iter()
                            .map(|step| land_text_step(step))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                );
            }
            if !output.skipped_steps.is_empty() {
                println!(
                    "  {}",
                    style::field(
                        "up to date",
                        &output
                            .skipped_steps
                            .iter()
                            .map(|step| land_text_step(step))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                );
            }
        }
        if output.captured {
            println!("  {}", style::field("captured", "yes"));
        }
        if output.synced {
            println!("  {}", style::field("refreshed", "yes"));
        }
        if output.checkpointed {
            println!("  {}", style::field("saved", "local Git commit recorded"));
        }
        if !output.siblings_restacked.is_empty() {
            println!(
                "  {}",
                style::field("siblings restacked", &output.siblings_restacked.join(", "))
            );
        }
        for blocker in &output.operator.blockers {
            println!("  blocker: {}", style::warn(blocker));
        }
        for warning in &output.operator.warnings {
            println!("  warning: {}", style::warn(warning));
        }
        println!(
            "Workspace: {}",
            if output.trust.verified {
                style::accent("verified")
            } else {
                style::warn(&output.trust.status)
            }
        );
        if let Some(next) = output
            .operator
            .recommended_action
            .as_ref()
            .or(output.operator.next_action.as_ref())
        {
            print_next(next);
        }
    }
    fail_if_blocked_operator_status(&output.operator.status)
}

fn land_text_step(step: &str) -> String {
    core_land_text_step(step)
}

const INCOMPLETE_LAND_MARKER: &str = "incomplete-land.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IncompleteLandMarker {
    thread_id: String,
    merge_state: Option<String>,
    collapse_state: Option<String>,
}

fn incomplete_land_marker_path(repo: &Repository) -> PathBuf {
    repo.heddle_dir().join(INCOMPLETE_LAND_MARKER)
}

fn write_incomplete_land_marker(
    repo: &Repository,
    thread_id: &str,
    merge_state: Option<&str>,
    collapse_state: Option<&StateId>,
) -> Result<()> {
    let marker = IncompleteLandMarker {
        thread_id: thread_id.to_string(),
        merge_state: merge_state.map(str::to_string),
        collapse_state: collapse_state.map(StateId::to_string_full),
    };
    let path = incomplete_land_marker_path(repo);
    let body = serde_json::to_vec_pretty(&marker).context("serialize incomplete-land marker")?;
    objects::fs_atomic::write_file_atomic(&path, &body)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn clear_incomplete_land_marker(repo: &Repository) -> Result<()> {
    let path = incomplete_land_marker_path(repo);
    match fs::remove_file(&path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                objects::fs_atomic::sync_directory(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn load_incomplete_land_marker(repo: &Repository) -> Result<Option<IncompleteLandMarker>> {
    let path = incomplete_land_marker_path(repo);
    match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .with_context(|| format!("parse incomplete-land marker at {}", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

pub(crate) fn recover_incomplete_land_if_present(repo: &Repository) -> Result<()> {
    let Some(marker) = load_incomplete_land_marker(repo)? else {
        return Ok(());
    };

    // A crash may happen after the Git checkpoint is durably recorded but
    // before the marker is removed. In that case the dual write completed and
    // recovery must only discard the stale journal.
    if let Some(merge_state) = marker.merge_state.as_deref()
        && let Some(state_id) = repo.resolve_state(merge_state)?
        && (repo.latest_git_checkpoint_for_state(&state_id)?.is_some()
            || heddle_core::recover_published_git_checkpoint(repo, &state_id)?.is_some())
    {
        clear_incomplete_land_marker(repo)?;
        return Ok(());
    }

    let collapse = marker
        .collapse_state
        .as_deref()
        .and_then(|state| StateId::parse(state).ok());
    match auto_undo_land_integration(
        repo,
        &marker.thread_id,
        marker.merge_state.as_deref(),
        collapse.as_ref(),
    ) {
        Ok(()) => {
            clear_incomplete_land_marker(repo)?;
            eprintln!(
                "note: recovered incomplete land of '{}': rolled back Heddle integration left without a Git checkpoint",
                marker.thread_id
            );
            Ok(())
        }
        Err(error) => Err(anyhow!(
            "incomplete land of '{}' needs recovery but auto-undo failed: {error:#}. Inspect `.heddle/{INCOMPLETE_LAND_MARKER}` and run `heddle undo` if the tip is still advanced.",
            marker.thread_id
        )),
    }
}

async fn cmd_land_many(cli: &Cli, args: LandArgs) -> Result<()> {
    let mut ordered = Vec::new();
    if let Some(primary) = args.thread.clone() {
        ordered.push(primary);
    }
    for thread in &args.threads {
        let thread = thread.trim();
        if !thread.is_empty() && !ordered.iter().any(|existing| existing == thread) {
            ordered.push(thread.to_string());
        }
    }
    if ordered.is_empty() {
        return Err(anyhow!("--threads requires at least one thread name"));
    }

    MULTI_LAND_COLLECTOR.with(|collector| {
        *collector.borrow_mut() = Some(Vec::with_capacity(ordered.len()));
    });

    let mut landed = Vec::new();
    let mut stopped_at = None;
    for thread_name in &ordered {
        let mut one = args.clone();
        one.thread = Some(thread_name.clone());
        one.threads.clear();
        let result = Box::pin(cmd_land(cli, one)).await;
        let recorded_peer = MULTI_LAND_COLLECTOR.with(|collector| {
            collector
                .borrow()
                .as_ref()
                .and_then(|peers| peers.last())
                .is_some_and(|peer| peer.thread == *thread_name)
        });
        if !recorded_peer && let Err(error) = &result {
            MULTI_LAND_COLLECTOR.with(|collector| {
                if let Some(peers) = collector.borrow_mut().as_mut() {
                    peers.push(multi_land_error_peer(thread_name, error));
                }
            });
        }
        let peer_integrated = MULTI_LAND_COLLECTOR.with(|collector| {
            collector
                .borrow()
                .as_ref()
                .and_then(|peers| peers.last())
                .is_some_and(|peer| peer.thread == *thread_name && peer.integrated)
        });
        if result.is_ok() || peer_integrated {
            landed.push(thread_name.clone());
        }
        if result.is_err() {
            stopped_at = Some(thread_name.clone());
            break;
        }
    }

    let peers = MULTI_LAND_COLLECTOR
        .with(|collector| collector.borrow_mut().take())
        .unwrap_or_default();
    let repo = cli.open_repo().ok();
    write_multi_land_output(
        cli,
        repo.as_ref(),
        &ordered,
        &landed,
        stopped_at.as_deref(),
        peers,
    )
}

fn multi_land_has_checkpointed_peer() -> bool {
    MULTI_LAND_COLLECTOR.with(|collector| {
        collector
            .borrow()
            .as_ref()
            .is_some_and(|peers| peers.iter().any(|peer| peer.checkpointed))
    })
}

fn multi_land_error_peer(thread: &str, error: &anyhow::Error) -> MultiLandPeerResult {
    let (message, blockers, warnings) = match error.downcast_ref::<RecoveryAdvice>() {
        Some(advice) => (
            advice.error.clone(),
            vec![advice.unsafe_condition.clone()],
            vec![advice.hint.clone()],
        ),
        None => (format!("{error:#}"), vec![format!("{error:#}")], Vec::new()),
    };
    MultiLandPeerResult {
        thread: thread.to_string(),
        status: "blocked".to_string(),
        message,
        captured: false,
        checkpointed: false,
        git_commit: None,
        integrated: false,
        synced: false,
        siblings_restacked: Vec::new(),
        blockers,
        warnings,
    }
}

fn write_multi_land_output(
    cli: &Cli,
    repo: Option<&Repository>,
    ordered: &[String],
    landed: &[String],
    stopped_at: Option<&str>,
    peers: Vec<MultiLandPeerResult>,
) -> Result<()> {
    let all_ok = stopped_at.is_none() && landed.len() == ordered.len();
    let status = if all_ok {
        "landed"
    } else if landed.is_empty() {
        "blocked"
    } else {
        "partial"
    };
    let message = if all_ok {
        format!("Landed {} thread(s): {}", landed.len(), landed.join(", "))
    } else if let Some(stop) = stopped_at {
        format!(
            "Landed {} of {} thread(s); stopped at '{stop}'",
            landed.len(),
            ordered.len()
        )
    } else {
        format!("Landed {} of {} thread(s)", landed.len(), ordered.len())
    };
    let git_head = repo.and_then(|repo| {
        sley::Repository::discover(repo.root())
            .ok()
            .and_then(|git| git.head().ok())
            .and_then(|head| head.oid.map(|oid| oid.to_string()))
    });
    let recommended_action = stopped_at.map(|_| "heddle status".to_string());
    let output = MultiLandOutput {
        output_kind: "land_batch",
        status: status.to_string(),
        action: "land",
        message,
        threads: ordered.to_vec(),
        landed: landed.to_vec(),
        stopped_at: stopped_at.map(str::to_string),
        peers,
        git_head,
        recommended_action: recommended_action.clone(),
        trust: repo.map(build_repository_verification_state),
    };

    if should_output_json(cli, None) {
        write_command_json(
            &output,
            output_is_compact(cli),
            NextActionValidationContext::without_repo(&["land"]),
        )?;
    } else {
        let marker = if all_ok {
            style::ok_marker()
        } else {
            style::warn_marker()
        };
        println!("{marker} {}", output.message);
        if let Some(head) = &output.git_head {
            println!(
                "  {}",
                style::field("git head", &head[..head.len().min(12)])
            );
        }
        for peer in &output.peers {
            let peer_marker = if peer.status == "landed" {
                style::ok_marker()
            } else {
                style::warn_marker()
            };
            println!(
                "  {peer_marker} {} — {}",
                style::bold(&peer.thread),
                peer.status
            );
            for blocker in &peer.blockers {
                println!("      blocker: {}", style::warn(blocker));
            }
        }
        if let Some(recommended_action) = recommended_action.as_deref() {
            print_next(recommended_action);
        }
    }
    if all_ok {
        Ok(())
    } else {
        Err(anyhow!(crate::exit::OutcomeExit::data_err()))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use heddle_core::AUTO_LAND_CONFIDENCE_RECOVERY_ACTION;

    use super::*;
    use crate::cli::commands::command_catalog::validate_recommended_action;

    fn thread_with_execution_path(execution_path: PathBuf) -> Thread {
        Thread {
            id: "agent-thread".to_string(),
            thread: "agent-thread".to_string(),
            target_thread: None,
            parent_thread: None,
            mode: repo::ThreadMode::Solid,
            state: repo::ThreadState::Active,
            base_state: "base".to_string(),
            base_root: "root".to_string(),
            current_state: Some("base".to_string()),
            merged_state: None,
            task: None,
            execution_path,
            materialized_path: None,
            changed_paths: vec![],
            impact_categories: vec![],
            heavy_impact_paths: vec![],
            promotion_suggested: false,
            freshness: repo::ThreadFreshness::Current,
            verification_summary: repo::ThreadVerificationSummary::default(),
            confidence_summary: repo::ThreadConfidenceSummary::default(),
            integration_policy_result: ThreadIntegrationPolicy::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            ephemeral: None,
            auto: false,
            shared_target_dir: None,
        }
    }

    #[test]
    fn confidence_blocker_recovery_scopes_to_thread_checkout() {
        let blockers = vec!["confidence 0.40 is below the auto-land threshold of 0.75".to_string()];
        let action = integration_blocker_recommended_action(
            &blockers,
            Some(Path::new("/work/threads/agent-thread")),
        )
        .expect("a confidence blocker must yield a recovery action");
        assert_eq!(
            action,
            "heddle --repo /work/threads/agent-thread capture -m \"...\" --confidence <confidence>"
        );
        validate_recommended_action(&action)
            .unwrap_or_else(|e| panic!("scoped recovery must validate: {e}"));
    }

    #[test]
    fn verification_blocker_recovery_scopes_to_thread_checkout() {
        let blockers = vec!["verification summary reports failing tests".to_string()];
        let action = integration_blocker_recommended_action(
            &blockers,
            Some(Path::new("/work/threads/agent-thread")),
        )
        .expect("a verification blocker must yield a recovery action");
        assert_eq!(
            action,
            "heddle --repo /work/threads/agent-thread capture -m \"...\" --confidence <confidence>"
        );
        validate_recommended_action(&action)
            .unwrap_or_else(|e| panic!("scoped recovery must validate: {e}"));
    }

    // The in-thread case (recovery run from inside the thread's own checkout)
    // must stay unscoped — a `--repo` pointing back at the current checkout is
    // noise, and the bare command already targets the right state.
    #[test]
    fn confidence_blocker_recovery_stays_unscoped_in_thread() {
        let blockers = vec!["confidence 0.40 is below the auto-land threshold of 0.75".to_string()];
        let action = integration_blocker_recommended_action(&blockers, None)
            .expect("a confidence blocker must yield a recovery action");
        assert_eq!(action, AUTO_LAND_CONFIDENCE_RECOVERY_ACTION);
        validate_recommended_action(&action)
            .unwrap_or_else(|e| panic!("unscoped recovery must validate: {e}"));
    }

    #[test]
    fn non_policy_blockers_yield_no_recovery_action() {
        let blockers = vec!["3 path conflict(s) need manual resolution".to_string()];
        assert!(integration_blocker_recommended_action(&blockers, None).is_none());
    }

    #[test]
    fn heavy_impact_review_is_advisory_for_land() {
        let blockers = vec![
            "Thread 'agent-thread' is stale against 'main'".to_string(),
            "Heavy-impact change: crates/wire/src/lib.rs — review broader impact before merging"
                .to_string(),
            "confidence 0.40 is below the auto-land threshold of 0.75".to_string(),
        ];

        assert_eq!(
            non_staleness_blockers(&blockers),
            vec!["confidence 0.40 is below the auto-land threshold of 0.75".to_string()]
        );
    }

    #[test]
    fn land_warnings_surface_heavy_impact_review() {
        let preview = crate::cli::commands::merge::ThreadPreviewReport {
            thread: "agent-thread".to_string(),
            thread_mode: "solid".to_string(),
            thread_state: "active".to_string(),
            freshness: "current".to_string(),
            task: None,
            changed_paths: vec!["crates/wire/src/lib.rs".to_string()],
            changed_path_count: 1,
            impact_categories: vec![],
            heavy_impact_paths: vec!["crates/wire/src/lib.rs".to_string()],
            merge_relation: "would_merge".to_string(),
            conflicts: vec![],
            conflict_count: 0,
            blockers: vec![
                "Heavy-impact change: crates/wire/src/lib.rs — review broader impact before merging"
                    .to_string(),
            ],
            recommended_action: "heddle land --thread agent-thread".to_string(),
            recommended_action_template: None,
            thread_health: "review".to_string(),
        };

        assert_eq!(
            land_warnings_for_preview(&preview),
            vec![
                "Heavy-impact change: crates/wire/src/lib.rs — review broader impact before merging"
                    .to_string()
            ]
        );
    }

    // `recovery_scope_checkout` is the gate that decides whether to scope: an
    // isolated thread (execution_path differs from the current checkout) scopes;
    // the in-thread case (paths equal) and a worktree-less thread (empty path)
    // do not.
    #[test]
    fn recovery_scope_checkout_distinguishes_isolated_from_in_thread() {
        let isolated = thread_with_execution_path(PathBuf::from("/work/threads/agent-thread"));
        assert_eq!(
            recovery_scope_checkout(&isolated, Path::new("/work/parent")),
            Some(PathBuf::from("/work/threads/agent-thread")),
        );

        let in_thread = thread_with_execution_path(PathBuf::from("/work/threads/agent-thread"));
        assert_eq!(
            recovery_scope_checkout(&in_thread, Path::new("/work/threads/agent-thread")),
            None,
        );

        let no_worktree = thread_with_execution_path(PathBuf::new());
        assert_eq!(
            recovery_scope_checkout(&no_worktree, Path::new("/work/parent")),
            None,
        );
    }
}
