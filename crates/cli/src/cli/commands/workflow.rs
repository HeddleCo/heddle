// SPDX-License-Identifier: Apache-2.0
use objects::store::ObjectStore;
use anyhow::{Context, Result, anyhow};
use objects::object::{ChangeId, ThreadName};
use oplog::{OpBatch, OpRecord};
use repo::{Repository, Thread, ThreadIntegrationPolicy, thread_flag};
use serde::Serialize;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    checkpoint::create_git_checkpoint,
    git_overlay_health::{RepositoryVerificationState, build_repository_verification_state},
    merge::{build_thread_preview_report, merge_thread_into_current},
    operator_core::{
        OperatorCommandOutput, VerificationClaimPolicy, exit_if_blocked_operator_status,
    },
    operator_loop::primary_next_action,
    ready_cmd::worktree_dirty,
    snapshot::{SnapshotAgentOverrides, create_snapshot},
    thread::start_thread,
    thread_cmd::{
        current_thread, load_thread, refresh_thread, thread_manager, thread_not_found_advice,
    },
    next_action::{NextActionValidationContext, write_validated_json_stdout},
    thread_landing::{land_local_command, switch_thread_command},
};
use crate::{
    cli::{
        Cli, ThreadStartArgs, WorkspaceModeArg,
        cli_args::{DelegateArgs, LandArgs, SyncArgs},
        should_output_json, style, worktree_status_options,
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
    pushed: bool,
    pushed_remote: Option<String>,
    performed_steps: Vec<String>,
    skipped_steps: Vec<String>,
    merge_state: Option<String>,
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
    chosen_path: String,
}

#[derive(Serialize)]
struct DelegatedThreadOutput {
    name: String,
    task: String,
    path: Option<String>,
    execution_path: Option<String>,
}

#[derive(Serialize)]
struct DelegateOutput {
    parent_thread: String,
    delegated: Vec<DelegatedThreadOutput>,
    message: String,
}

pub async fn cmd_sync(cli: &Cli, args: SyncArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
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
    let import_hint = repo.git_overlay_import_hint()?;
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
                action: "sync".to_string(),
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
                action: "sync".to_string(),
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
                        action: "sync".to_string(),
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
                        action: "sync".to_string(),
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

    emit_with_next_action_validation(cli, &repo, &output, &["sync"])
}

pub async fn cmd_land(cli: &Cli, args: LandArgs) -> Result<()> {
    // Open at CWD only to discover the active thread, then re-open at
    // its metadata-recorded worktree. This makes `heddle land` work
    // from anywhere — operators don't need to `cd` into a lightweight
    // thread directory before landing. The capture/merge below run
    // against `repo`, so they all see the same checkout. See
    // `Repository::active_worktree_path`.
    let cwd_repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let target_path = cwd_repo.active_worktree_path()?;
    let repo = if target_path == *cwd_repo.root() {
        cwd_repo
    } else {
        Repository::open(&target_path)?
    };
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
        // operator is stuck. `heddle switch` rebuilds the dedicated worktree at
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
    if args.push && args.no_push {
        return Err(anyhow!(RecoveryAdvice::land_push_option_conflict(
            &thread.id
        )));
    }
    if let Some(remote) = args.remote.as_deref()
        && !args.push
    {
        return Err(anyhow!(RecoveryAdvice::land_remote_requires_push(
            &thread.id, remote,
        )));
    }
    let should_push = args.push;
    let planned_push_remote = if should_push {
        match args
            .remote
            .clone()
            .or(super::remote::resolved_default_remote_name(&repo)?)
        {
            Some(remote) => Some(remote),
            None => {
                return Err(anyhow!(RecoveryAdvice::land_push_remote_missing(
                    &thread.id
                )));
            }
        }
    } else {
        None
    };
    if let Some(advice) = land_checkpoint_preflight_advice(&repo, &thread.id) {
        return Err(anyhow!(advice));
    }

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

    let mut synced = false;
    let mut refreshed_thread = resolve_thread(
        &repo,
        Some(&thread.id),
        "land",
        "heddle land --thread <name>",
    )?;
    if refreshed_thread.freshness == repo::ThreadFreshness::Stale {
        let preview = build_thread_preview_report(&repo, &mut refreshed_thread, true)?;
        let stale_blockers = non_staleness_blockers(&preview.blockers);
        if preview.conflict_count > 0 || !stale_blockers.is_empty() {
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
                        action: "land".to_string(),
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
                    pushed: false,
                    pushed_remote: None,
                    merge_state: None,
                    trust: build_repository_verification_state(&repo),
                    chosen_path: "blocked".to_string(),
                    performed_steps: land_performed_steps(captured, false, false, false, false),
                    skipped_steps: land_skipped_steps(captured, false, false, false, false),
                },
            );
        }

        refreshed_thread = refresh_thread(&repo, &refreshed_thread.id, cli)?;
        synced = true;
    }

    let mut merge_thread = resolve_thread(
        &repo,
        Some(&refreshed_thread.id),
        "land",
        "heddle land --thread <name>",
    )?;
    let preview = build_thread_preview_report(&repo, &mut merge_thread, true)?;
    let integration_blockers = integration_blockers(&repo, &merge_thread, &preview);
    let manual_resolution_current = manual_resolution_current(&repo, &merge_thread);
    if manual_resolution_current {
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
            let checkpoint_message =
                land_checkpoint_message(&repo, &merge_thread, args.message.as_deref());
            let record = create_git_checkpoint(
                &repo,
                Some(&checkpoint_message),
                worktree_status_options(Some(repo.config())),
            )
            .map_err(|error| {
                anyhow!(RecoveryAdvice::land_checkpoint_partial_failure(
                    &merge_thread.id,
                    error,
                    land_performed_steps(captured, synced, true, false, false),
                ))
            })?;
            checkpointed = true;
            git_commit = Some(record.git_commit);
        }
        coalesce_land_integration_and_checkpoint(
            &repo,
            Some(&merge_state),
            git_commit.as_deref(),
        )
        .context(
            "land completed but failed to record manual integration and Git checkpoint as one undo batch",
        )?;
        let mut pushed = false;
        let mut pushed_remote = None;
        if should_push {
            let remote_name = push_after_land(
                cli,
                &repo,
                planned_push_remote.clone(),
                Some(merge_state.clone()),
            )
            .await
            .map_err(|error| {
                anyhow!(RecoveryAdvice::land_push_partial_failure(
                    &merge_thread.id,
                    error,
                    land_performed_steps(captured, synced, true, checkpointed, false),
                    git_commit.as_deref(),
                    planned_push_remote.as_deref(),
                ))
            })?;
            pushed = true;
            pushed_remote = Some(remote_name);
        }
        clear_manual_resolution_state(&repo, &merge_thread.id)?;
        let trust = build_repository_verification_state(&repo);
        let post_land_action = integrated_land_next_action(true, pushed, &trust);
        let mut operator = OperatorCommandOutput {
            status: "landed".to_string(),
            action: "land".to_string(),
            message: format!(
                "Landed thread '{}' from a manually resolved integration state",
                merge_thread.id
            ),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: post_land_action.clone(),
            recommended_action: post_land_action,
        };
        operator.block_success_claim_if_verification_blocked(
            &trust,
            "land",
            VerificationClaimPolicy::strict().allow_land_publish_followup(),
        );
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
                pushed,
                pushed_remote,
                merge_state: Some(merge_state),
                trust,
                performed_steps: land_performed_steps(captured, synced, true, checkpointed, pushed),
                skipped_steps: land_skipped_steps(captured, synced, true, checkpointed, pushed),
                chosen_path: if checkpointed {
                    if pushed {
                        "capture_sync_manual_resolution_checkpoint_push".to_string()
                    } else {
                        "capture_sync_manual_resolution_checkpoint".to_string()
                    }
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
        // Never fall back to `preview.recommended_action` here: this is the
        // pre-merge bail, so no merge state is materialized (a `resolve`
        // breadcrumb would die with `no_merge_in_progress`) and the preview's
        // own land recommendation would self-loop this very command. Drive the
        // operator through `sync`, which materializes and resolves a genuine
        // conflict before a land retry. (heddle#464 close-the-class.)
        let recovery_scope = recovery_scope_checkout(&merge_thread, repo.root());
        let recommended_action =
            integration_blocker_recommended_action(&integration_blockers, recovery_scope.as_deref())
                .unwrap_or_else(|| format!("heddle sync {}", thread_flag(&merge_thread.id)));
        update_integration_policy(&repo, &merge_thread.id, "blocked", &reason)?;
        return write_land_output(
            cli,
            &repo,
            &LandOutput {
                operator: OperatorCommandOutput {
                    status: "blocked".to_string(),
                    action: "land".to_string(),
                    message: format!("Thread '{}' is not eligible for auto-land", merge_thread.id),
                    blockers: land_blockers_for_preview(&preview, &integration_blockers),
                    warnings: Vec::new(),
                    next_action: Some(recommended_action.clone()),
                    recommended_action: Some(recommended_action),
                },
                thread: merge_thread.id.clone(),
                captured,
                checkpointed: false,
                git_commit: None,
                synced,
                integrated: false,
                pushed: false,
                pushed_remote: None,
                merge_state: None,
                trust: build_repository_verification_state(&repo),
                chosen_path: "blocked".to_string(),
                performed_steps: land_performed_steps(captured, synced, false, false, false),
                skipped_steps: land_skipped_steps(captured, synced, false, false, false),
            },
        );
    }

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
        let checkpoint_message =
            land_checkpoint_message(&repo, &merge_thread, args.message.as_deref());
        let record = create_git_checkpoint(
            &repo,
            Some(&checkpoint_message),
            worktree_status_options(Some(repo.config())),
        )
        .map_err(|error| {
            anyhow!(RecoveryAdvice::land_checkpoint_partial_failure(
                &merge_thread.id,
                error,
                land_performed_steps(captured, synced, integrated, false, false),
            ))
        })?;
        checkpointed = true;
        git_commit = Some(record.git_commit);
    }
    coalesce_land_integration_and_checkpoint(
        &repo,
        merge_output.merge_state.as_deref(),
        git_commit.as_deref(),
    )
    .context("land completed but failed to record merge and Git checkpoint as one undo batch")?;

    let mut pushed = false;
    let mut pushed_remote = None;
    if integrated && should_push {
        let remote_name = push_after_land(
            cli,
            &repo,
            planned_push_remote.clone(),
            merge_output.merge_state.clone(),
        )
        .await
        .map_err(|error| {
            anyhow!(RecoveryAdvice::land_push_partial_failure(
                &merge_thread.id,
                error,
                land_performed_steps(captured, synced, integrated, checkpointed, false),
                git_commit.as_deref(),
                planned_push_remote.as_deref(),
            ))
        })?;
        pushed = true;
        pushed_remote = Some(remote_name);
    }

    if integrated {
        clear_manual_resolution_state(&repo, &merge_thread.id)?;
    }

    let trust = build_repository_verification_state(&repo);
    let integrated_next_action = integrated_land_next_action(integrated, pushed, &trust);
    let mut operator = OperatorCommandOutput {
        status: if integrated { "landed" } else { "blocked" }.to_string(),
        action: "land".to_string(),
        message: if integrated {
            format!("Landed thread '{}'", merge_thread.id)
        } else {
            format!("Thread '{}' could not be landed cleanly", merge_thread.id)
        },
        blockers: merge_output.operator.blockers.clone(),
        warnings: Vec::new(),
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
            pushed,
            pushed_remote,
            merge_state: merge_output.merge_state.clone(),
            trust,
            performed_steps: land_performed_steps(
                captured,
                synced,
                integrated,
                checkpointed,
                pushed,
            ),
            skipped_steps: land_skipped_steps(captured, synced, integrated, checkpointed, pushed),
            chosen_path: if integrated {
                if pushed {
                    "capture_sync_merge_checkpoint_push"
                } else if checkpointed {
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

async fn push_after_land(
    cli: &Cli,
    repo: &Repository,
    remote: Option<String>,
    state: Option<String>,
) -> Result<String> {
    if repo.capability() == repo::RepositoryCapability::GitOverlay && !repo.hosted_enabled() {
        let (remote_name, _, _, _, _) =
            super::remote::push_git_overlay_refs(repo, remote.as_deref(), false, false)?;
        Ok(remote_name)
    } else {
        let pushed_remote = remote
            .clone()
            .or(super::remote::resolved_default_remote_name(repo)?)
            .unwrap_or_else(|| "default".to_string());
        super::remote::cmd_push(cli, remote, None, state, false, false, None).await?;
        Ok(pushed_remote)
    }
}

fn land_performed_steps(
    captured: bool,
    synced: bool,
    integrated: bool,
    checkpointed: bool,
    pushed: bool,
) -> Vec<String> {
    [
        (captured, "capture"),
        (synced, "sync"),
        (integrated, "merge"),
        (checkpointed, "checkpoint"),
        (pushed, "push"),
    ]
    .into_iter()
    .filter(|&(done, _step)| done).map(|(_done, step)| step.to_string())
    .collect()
}

fn land_skipped_steps(
    captured: bool,
    synced: bool,
    integrated: bool,
    checkpointed: bool,
    pushed: bool,
) -> Vec<String> {
    [
        (!captured, "capture(no changes)"),
        (!synced, "sync(current)"),
        (!integrated, "merge(blocked)"),
        (!checkpointed && integrated, "checkpoint(not needed)"),
        (!checkpointed && !integrated, "checkpoint(not reached)"),
        (!pushed && integrated, "push(not requested)"),
        (!pushed && !integrated, "push(not reached)"),
    ]
    .into_iter()
    .filter(|&(skipped, _step)| skipped).map(|(_skipped, step)| step.to_string())
    .collect()
}

fn integrated_land_next_action(
    integrated: bool,
    pushed: bool,
    trust: &RepositoryVerificationState,
) -> Option<String> {
    if !integrated {
        return None;
    }
    if !pushed && trust.recommended_action == "heddle push" {
        Some(trust.recommended_action.clone())
    } else {
        Some("heddle thread cleanup --merged --dry-run".to_string())
    }
}

fn land_checkpoint_preflight_advice(repo: &Repository, thread_id: &str) -> Option<RecoveryAdvice> {
    if repo.capability() != repo::RepositoryCapability::GitOverlay {
        return None;
    }
    let trust = build_repository_verification_state(repo);
    if matches!(
        trust.remote_drift.as_str(),
        "remote_behind" | "remote_diverged"
    ) {
        let remote_decision = repo
            .git_remote_tracking_status()
            .ok()
            .flatten()
            .map(|remote| super::git_overlay_health::remote_drift_decision(repo, &remote));
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

fn land_checkpoint_message(repo: &Repository, thread: &Thread, explicit: Option<&str>) -> String {
    if let Some(message) = explicit.filter(|message| !message.trim().is_empty()) {
        return message.to_string();
    }
    if let Some(intent) = thread
        .current_state
        .as_deref()
        .and_then(|state| repo.resolve_state(state).ok().flatten())
        .and_then(|state_id| repo.store().get_state(&state_id).ok().flatten())
        .and_then(|state| state.intent)
        .filter(|intent| !intent.trim().is_empty())
    {
        return intent;
    }
    if let Some(task) = thread
        .task
        .as_deref()
        .filter(|task| !task.trim().is_empty())
    {
        return task.to_string();
    }
    format!("Land {}", thread.id)
}

pub fn cmd_delegate(cli: &Cli, args: DelegateArgs) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    warn_if_path_prefix_inside_repo(&repo, args.path_prefix.as_deref());
    let parent = resolve_parent_thread(&repo, args.parent.as_deref())?;

    // Warm the canonical loose-uncompressed store for the parent
    // state once, before we materialize it into N child worktrees.
    // The first child would otherwise pay
    // `decompress + atomic write` per blob (lazy promotion inside
    // `materialize_blob`), and only worktrees 2..N would hardlink.
    // A single warm pass amortizes promotion cost across all N
    // children in the common N-agents-on-the-same-parent case.
    //
    // Failures are non-fatal: the lazy path inside
    // `materialize_blob` will still promote on demand, and an empty
    // or partially-warm store just means the first materialize pays
    // promotion cost for any blobs we missed.
    if args.tasks.len() > 1 {
        let parent_state_spec = parent
            .current_state
            .clone()
            .unwrap_or_else(|| parent.base_state.clone());
        match repo
            .resolve_state(&parent_state_spec)
            .ok()
            .and_then(|opt| opt)
        {
            Some(parent_state_id) => match repo.warm_canonical_store_for_state(&parent_state_id) {
                Ok(stats) => {
                    tracing::debug!(
                        promoted = stats.promoted,
                        already_loose = stats.already_loose,
                        errors = stats.errors,
                        "Warmed canonical store before delegate fan-out"
                    );
                }
                Err(err) => {
                    tracing::debug!(
                        ?err,
                        "Warm canonical store failed; falling back to lazy promotion in materialize"
                    );
                }
            },
            None => {
                tracing::debug!(
                    parent_state = %parent_state_spec,
                    "Could not resolve parent state for warm pass; falling back to lazy promotion"
                );
            }
        }
    }

    // Pre-flight: when two specs collapse to the same slug (e.g.
    // racing three agents on a "modulo" task with all three entries
    // labelled "modulo:..."), `start_thread` would refuse the duplicate
    // thread name halfway through and leave a partial workspace
    // behind. Disambiguate by suffixing the slug with the provider
    // when collisions exist. Pure heads-up logic — no behavior change
    // for the single-agent-per-task case.
    let mut slug_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for spec in &args.tasks {
        *slug_counts.entry(slugify(&spec.task)).or_insert(0) += 1;
    }

    let delegated = args
        .tasks
        .iter()
        .map(|spec| {
            let base_slug = slugify(&spec.task);
            let slug = if slug_counts.get(&base_slug).copied().unwrap_or(0) > 1 {
                match spec.provider.as_deref() {
                    Some(provider) => format!("{base_slug}-{}", slugify(provider)),
                    None => base_slug.clone(),
                }
            } else {
                base_slug
            };
            let name = format!("{}/{}", parent.id, slug);
            let path = args.path_prefix.as_ref().map(|prefix| prefix.join(&slug));

            // Per-spec agent override wins; fall back to the
            // command-wide default (`--agent-provider`/`--agent-model`).
            let agent_provider = spec
                .provider
                .clone()
                .or_else(|| args.agent_provider.clone());
            let agent_model = spec.model.clone().or_else(|| args.agent_model.clone());

            let output = start_thread(
                &repo,
                ThreadStartArgs {
                    name: name.clone(),
                    from: Some(
                        parent
                            .current_state
                            .clone()
                            .unwrap_or(parent.base_state.clone()),
                    ),
                    path,
                    workspace: args.workspace.unwrap_or(WorkspaceModeArg::Auto),
                    agent_provider,
                    agent_model,
                    task: Some(spec.task.clone()),
                    parent_thread: Some(parent.id.clone()),
                    automated: true,
                    print_cd_path: false,
                    // Delegated children inherit the in-process mount path
                    // explicitly: spawning a `heddled` daemon as a side
                    // effect of `heddle delegate` would surprise the
                    // caller (delegate is mostly used with materialized /
                    // lightweight workspaces anyway). If a future caller
                    // passes `--workspace virtualized` through delegate
                    // and wants daemon ownership, they can spawn the
                    // daemon explicitly first.
                    daemon: false,
                    no_daemon: true,
                    // Delegated children inherit the parent's
                    // implicit per-checkout target/. If a delegate
                    // user wants the shared-target arrangement they
                    // can opt in by re-running `heddle start
                    // --shared-target` against the spawned thread —
                    // delegate is a thin orchestration verb and
                    // shouldn't make filesystem-layout decisions for
                    // the user.
                    shared_target: false,
                    // Delegated children don't auto-hydrate: delegate is a
                    // thin orchestration verb and shouldn't symlink deps
                    // into a spawned checkout without an explicit ask.
                    hydrate: false,
                },
            )?;
            Ok(DelegatedThreadOutput {
                name,
                task: spec.task.clone(),
                path: output.path,
                execution_path: output.execution_path,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    emit_with_next_action_validation(
        cli,
        &repo,
        &DelegateOutput {
            parent_thread: parent.id,
            delegated,
            message: "Delegated child threads created".to_string(),
        },
        &["delegate"],
    )
}

/// Print a one-line warning when the operator passes
/// `--path-prefix <path>` and `<path>` (after resolving against CWD)
/// is a strict descendant of the repo root. The new
/// nested-thread-worktree exclusion in `repo` makes this layout safe,
/// but the conventional shape is a sibling directory; flagging the
/// unconventional choice keeps the demo geometry honest.
fn warn_if_path_prefix_inside_repo(repo: &Repository, path_prefix: Option<&std::path::Path>) {
    let Some(prefix) = path_prefix else {
        return;
    };
    let resolved = if prefix.is_absolute() {
        prefix.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(prefix),
            Err(_) => return,
        }
    };
    let canonical_prefix = resolved.canonicalize().unwrap_or(resolved);
    let canonical_root = repo
        .root()
        .canonicalize()
        .unwrap_or_else(|_| repo.root().to_path_buf());
    if canonical_prefix == canonical_root {
        return;
    }
    if !canonical_prefix.starts_with(&canonical_root) {
        return;
    }
    eprintln!(
        "warn: agent worktree at {} is nested inside repo root {}; \
         the parent thread's scans will exclude it, but a sibling path is more conventional.",
        canonical_prefix.display(),
        canonical_root.display(),
    );
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

fn resolve_parent_thread(repo: &Repository, thread: Option<&str>) -> Result<Thread> {
    resolve_thread(
        repo,
        thread,
        "delegate",
        "heddle delegate --parent <THREAD> <task>",
    )
    .or_else(|_| {
        let head = repo.head_ref()?;
        match head {
            refs::Head::Attached { thread } => load_thread(repo, &thread),
            refs::Head::Detached { .. } => {
                Err(anyhow!(RecoveryAdvice::no_attached_parent_thread()))
            }
        }
    })
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
    thread.integration_policy_result = ThreadIntegrationPolicy {
        status: Some(next_status.to_string()),
        reason: Some(next_reason),
        manual_resolution_state: thread.integration_policy_result.manual_resolution_state,
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
    Ok(manager.save(&thread)?)
}

fn coalesce_land_integration_and_checkpoint(
    repo: &Repository,
    merge_state: Option<&str>,
    git_commit: Option<&str>,
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
    Ok(())
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
    match op {
        OpRecord::Snapshot { new_state, .. } => change_id_matches_display(new_state, merge_state),
        OpRecord::Checkpoint { state, .. } => change_id_matches_display(state, merge_state),
        OpRecord::Goto { target, .. } => change_id_matches_display(target, merge_state),
        OpRecord::FastForwardV2 { post_target_id, .. } => {
            change_id_matches_display(post_target_id, merge_state)
        }
        // These records don't advance HEAD/thread to a merge target the land
        // flow tracks (legacy V1 `FastForward` re-resolves at apply time and
        // carries no post-target id). Enumerated explicitly (no wildcard) so a
        // new state-advancing variant must be considered as a possible merge
        // target here (heddle#354 r9).
        OpRecord::FastForward { .. }
        | OpRecord::ThreadCreate { .. }
        | OpRecord::ThreadCreateV2 { .. }
        | OpRecord::ThreadDelete { .. }
        | OpRecord::ThreadUpdate { .. }
        | OpRecord::Fork { .. }
        | OpRecord::Collapse { .. }
        | OpRecord::MarkerCreate { .. }
        | OpRecord::MarkerDelete { .. }
        | OpRecord::TransactionAbort { .. }
        | OpRecord::EphemeralThreadCollapse { .. }
        | OpRecord::ConflictResolved { .. }
        | OpRecord::TransactionCommit { .. }
        | OpRecord::Redact { .. }
        | OpRecord::Purge { .. }
        | OpRecord::GitCheckpoint { .. }
        | OpRecord::RemoteThreadUpdate { .. }
        | OpRecord::RemoteThreadDelete { .. }
        | OpRecord::UndoRecoveryUpdate { .. } => false,
    }
}

fn change_id_matches_display(id: &ChangeId, display: &str) -> bool {
    id.short() == display || id.to_string_full() == display
}

fn adopt_manual_resolution(repo: &Repository, thread_id: &str) -> Result<String> {
    let manager = thread_manager(repo);
    let mut thread = manager.load(thread_id)?.ok_or_else(|| {
        anyhow!(thread_not_found_advice(
            thread_id,
            "adopt manual resolution"
        ))
    })?;
    let target = repo.refs().get_thread(&ThreadName::new(&thread.thread))?.ok_or_else(|| {
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

const AUTO_LAND_CONFIDENCE_THRESHOLD: f32 = 0.75;
const AUTO_LAND_CONFIDENCE_RECOVERY_ACTION: &str =
    "heddle commit -m \"...\" --confidence <confidence>";

pub(crate) fn integration_blockers(
    repo: &Repository,
    thread: &Thread,
    preview: &super::merge::ThreadPreviewReport,
) -> Vec<String> {
    let manual_resolution_current = manual_resolution_current(repo, thread);
    let mut blockers = if manual_resolution_current {
        Vec::new()
    } else {
        non_staleness_blockers(&preview.blockers)
    };
    blockers.extend(auto_land_policy_blockers(repo, thread));
    blockers
}

pub(crate) fn auto_land_policy_blockers(repo: &Repository, thread: &Thread) -> Vec<String> {
    let mut blockers = Vec::new();
    let agent_authored = thread_is_agent_authored(repo, thread);
    if agent_authored
        && let Some(confidence) = thread.confidence_summary.value
            && confidence < AUTO_LAND_CONFIDENCE_THRESHOLD
        {
            blockers.push(format!(
                "confidence {:.2} is below the auto-land threshold of 0.75",
                confidence
            ));
        }
    if matches!(thread.verification_summary.tests_passed, Some(false)) {
        blockers.push("verification summary reports failing tests".to_string());
    }
    blockers
}

pub(crate) fn integration_blocker_recommended_action(
    blockers: &[String],
    scope_to_checkout: Option<&std::path::Path>,
) -> Option<String> {
    blockers
        .iter()
        .any(|blocker| {
            blocker.starts_with("confidence ")
                || blocker == "verification summary reports failing tests"
        })
        .then(|| auto_land_confidence_recovery_action(scope_to_checkout))
}

/// The `confidence`/`verification` policy blocker is cleared by re-capturing the
/// thread's state with a fresh confidence. That capture must land in the
/// *thread's* checkout, not whatever checkout `ready`/`land` was invoked from —
/// running an unscoped `heddle commit` from the parent of an isolated
/// agent-authored thread commits the parent and never updates the blocked
/// thread. When the thread's checkout differs from the current one, scope the
/// recovery with the global `--repo` flag so the capture targets the thread.
/// (heddle#464.)
fn auto_land_confidence_recovery_action(scope_to_checkout: Option<&std::path::Path>) -> String {
    match scope_to_checkout {
        Some(path) => format!(
            "heddle --repo {} {}",
            crate::cli::render::shell_quote(&path.display().to_string()),
            AUTO_LAND_CONFIDENCE_RECOVERY_ACTION
                .strip_prefix("heddle ")
                .expect("recovery action is a heddle command"),
        ),
        None => AUTO_LAND_CONFIDENCE_RECOVERY_ACTION.to_string(),
    }
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
    let execution_path = &thread.execution_path;
    if execution_path.as_os_str().is_empty() {
        return None;
    }
    let canonical = |path: &std::path::Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    (canonical(execution_path) != canonical(current_checkout)).then(|| execution_path.clone())
}

fn land_blockers_for_preview(
    preview: &super::merge::ThreadPreviewReport,
    blockers: &[String],
) -> Vec<String> {
    let mut out = blockers.to_vec();
    if preview.conflict_count > 0 {
        out.push(format!(
            "{} path conflict(s) need manual resolution",
            preview.conflict_count
        ));
        out.extend(
            preview
                .conflicts
                .iter()
                .map(|path| format!("conflict: {path}")),
        );
    }
    out.sort();
    out.dedup();
    out
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
        .or_else(|| repo.refs().get_thread(&ThreadName::new(&thread.thread)).ok().flatten());
    current_state
        .and_then(|id| repo.store().get_state(&id).ok().flatten())
        .map(|state| state.attribution.agent.is_some())
        .unwrap_or(false)
}

fn non_staleness_blockers(blockers: &[String]) -> Vec<String> {
    blockers
        .iter()
        .filter(|blocker| !blocker.contains(" is stale against "))
        .cloned()
        .collect()
}

fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars().flat_map(|c| c.to_lowercase()) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn emit_with_next_action_validation<T: Serialize>(
    cli: &Cli,
    repo: &Repository,
    output: &T,
    emitting_command: &[&str],
) -> Result<()> {
    if should_output_json(cli, None) {
        write_validated_json_stdout(
            output,
            NextActionValidationContext::new(emitting_command, repo.capability()),
        )
    } else {
        println!("{}", serde_json::to_string_pretty(output)?);
        Ok(())
    }
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
    if should_output_json(cli, None) {
        write_validated_json_stdout(
            output,
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
            let push_status = if output.pushed {
                output
                    .pushed_remote
                    .as_deref()
                    .map(|remote| format!("pushed to {remote}"))
                    .unwrap_or_else(|| "pushed".to_string())
            } else {
                "not pushed".to_string()
            };
            println!("  {}", style::field("push", &push_status));
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
        for blocker in &output.operator.blockers {
            println!("  blocker: {}", style::warn(blocker));
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
    exit_if_blocked_operator_status(&output.operator.status);
    Ok(())
}

fn land_text_step(step: &str) -> String {
    match step {
        "capture" => "saved".to_string(),
        "sync" => "refreshed".to_string(),
        "merge" => "merged".to_string(),
        "checkpoint" => "committed".to_string(),
        "push" => "pushed".to_string(),
        "capture(no changes)" => "no unsaved changes".to_string(),
        "sync(current)" => "already refreshed".to_string(),
        "merge(blocked)" => "merge blocked".to_string(),
        "checkpoint(not needed)" => "no Git commit needed".to_string(),
        "checkpoint(not reached)" => "Git commit not reached".to_string(),
        "push(not requested)" => "push not requested".to_string(),
        "push(not reached)" => "push not reached".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands::command_catalog::validate_recommended_action;
    use std::path::{Path, PathBuf};

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

    // heddle#464 bug 2: the confidence/verification policy-blocker recovery used
    // to be a bare `heddle commit ... --confidence`, which commits the CURRENT
    // checkout. Run from the parent of an isolated thread, that never updates the
    // blocked thread's state. Scope it to the thread's checkout via `--repo`.
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
            "heddle --repo /work/threads/agent-thread commit -m \"...\" --confidence <confidence>"
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
            "heddle --repo /work/threads/agent-thread commit -m \"...\" --confidence <confidence>"
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
