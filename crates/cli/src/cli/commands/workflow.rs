// SPDX-License-Identifier: Apache-2.0
use anyhow::{Context, Result, anyhow};
use objects::object::ChangeId;
use oplog::{OpBatch, OpRecord};
use repo::{Repository, Thread, ThreadIntegrationPolicy};
use serde::Serialize;

use super::{
    advice::RecoveryAdvice,
    checkpoint::create_git_checkpoint,
    git_overlay_health::{RepositoryVerificationState, build_repository_verification_state},
    merge::{build_thread_preview_report, merge_thread_into_current},
    operator_core::{OperatorCommandOutput, exit_if_blocked_operator_status},
    operator_loop::primary_next_action,
    ready_cmd::worktree_dirty,
    snapshot::{SnapshotAgentOverrides, create_snapshot},
    thread::start_thread,
    thread_cmd::{
        current_thread, load_thread, refresh_thread, thread_manager, thread_not_found_advice,
    },
};
use crate::{
    cli::{
        Cli, ThreadStartArgs, WorkspaceModeArg,
        cli_args::{DelegateArgs, ShipArgs, SyncArgs},
        should_output_json, style, worktree_status_options,
    },
    config::UserConfig,
};

#[derive(Serialize)]
struct SyncOutput {
    #[serde(flatten)]
    operator: OperatorCommandOutput,
    #[serde(rename = "verification")]
    trust: RepositoryVerificationState,
    thread: String,
    current_state: Option<String>,
    chosen_path: String,
}

#[derive(Serialize)]
struct ShipOutput {
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
            Some("heddle ship"),
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
    } else if stale_report.conflict_count > 0 || !stale_blockers.is_empty() {
        let recommended_action = primary_next_action(
            operation.as_ref(),
            remote_tracking.as_ref(),
            import_hint.as_ref(),
            Some(&stale_report.recommended_action),
        );
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
                message: format!("Thread '{}' needs manual refresh", thread.id),
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
    } else {
        let refreshed = refresh_thread(&repo, &thread.id, cli)?;
        update_integration_policy(&repo, &refreshed.id, "current", "thread refreshed cleanly")?;
        let recommended_action = primary_next_action(
            operation.as_ref(),
            remote_tracking.as_ref(),
            import_hint.as_ref(),
            Some("heddle ship"),
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
    };
    block_operator_claim_if_trust_blocked(&mut output.operator, &output.trust);

    emit(cli, &output)
}

pub async fn cmd_ship(cli: &Cli, args: ShipArgs) -> Result<()> {
    // Open at CWD only to discover the active thread, then re-open at
    // its metadata-recorded worktree. This makes `heddle ship` work
    // from anywhere — operators don't need to `cd` into a lightweight
    // thread directory before shipping. The capture/merge below run
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
        "ship",
        "heddle ship --thread <name>",
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
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "thread_worktree_missing",
            format!("Thread '{}' worktree is missing", thread.id),
            format!(
                "Materialize the thread again with `heddle start {} --path <path>` or merge it by ref with `heddle merge {} --preview`.",
                thread.id, thread.id
            ),
            format!(
                "recorded execution path does not exist: {}",
                thread.execution_path.display()
            ),
            "ship would need to inspect that checkout for unsaved work before merging",
            "repository state, refs, metadata, and worktree files were left unchanged",
            format!("heddle merge {} --preview", thread.id),
            vec![format!("heddle merge {} --preview", thread.id)],
        )));
    };
    if args.push && args.no_push {
        return Err(anyhow!(RecoveryAdvice::ship_push_option_conflict(
            &thread.id
        )));
    }
    if let Some(remote) = args.remote.as_deref()
        && !args.push
    {
        return Err(anyhow!(RecoveryAdvice::ship_remote_requires_push(
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
                return Err(anyhow!(RecoveryAdvice::ship_push_remote_missing(
                    &thread.id
                )));
            }
        }
    } else {
        None
    };
    if let Some(advice) = ship_checkpoint_preflight_advice(&repo, &thread.id) {
        return Err(anyhow!(advice));
    }

    let mut captured = false;
    if let Some(thread_repo) = thread_repo.as_ref() {
        let status_options = worktree_status_options(Some(thread_repo.config()));
        if worktree_dirty(thread_repo, &status_options)? {
            let capture_message = args
                .message
                .clone()
                .or_else(|| Some(format!("Ship {}", thread.id)));
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
        "ship",
        "heddle ship --thread <name>",
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
                    .unwrap_or_else(|| "refresh requires manual resolution".to_string()),
            )?;
            return write_ship_output(
                cli,
                &ShipOutput {
                    operator: OperatorCommandOutput {
                        status: "blocked".to_string(),
                        action: "ship".to_string(),
                        message: format!(
                            "Thread '{}' must be refreshed manually",
                            refreshed_thread.id
                        ),
                        blockers: ship_blockers_for_preview(&preview, &stale_blockers),
                        warnings: Vec::new(),
                        next_action: Some(preview.recommended_action.clone()),
                        recommended_action: Some(preview.recommended_action),
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
                    performed_steps: ship_performed_steps(captured, false, false, false, false),
                    skipped_steps: ship_skipped_steps(captured, false, false, false, false),
                },
            );
        }

        refreshed_thread = refresh_thread(&repo, &refreshed_thread.id, cli)?;
        synced = true;
    }

    let mut merge_thread = resolve_thread(
        &repo,
        Some(&refreshed_thread.id),
        "ship",
        "heddle ship --thread <name>",
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
                ship_checkpoint_message(&repo, &merge_thread, args.message.as_deref());
            let record = create_git_checkpoint(
                &repo,
                Some(&checkpoint_message),
                worktree_status_options(Some(repo.config())),
            )
            .map_err(|error| {
                anyhow!(RecoveryAdvice::ship_checkpoint_partial_failure(
                    &merge_thread.id,
                    error,
                    ship_performed_steps(captured, synced, true, false, false),
                ))
            })?;
            checkpointed = true;
            git_commit = Some(record.git_commit);
        }
        coalesce_ship_integration_and_checkpoint(
            &repo,
            Some(&merge_state),
            git_commit.as_deref(),
        )
        .context(
            "ship completed but failed to record manual integration and Git checkpoint as one undo batch",
        )?;
        let mut pushed = false;
        let mut pushed_remote = None;
        if should_push {
            let remote_name = push_after_ship(
                cli,
                &repo,
                planned_push_remote.clone(),
                Some(merge_state.clone()),
                captured,
                synced,
                true,
                checkpointed,
                git_commit.as_deref(),
            )
            .await
            .map_err(|error| {
                anyhow!(RecoveryAdvice::ship_push_partial_failure(
                    &merge_thread.id,
                    error,
                    ship_performed_steps(captured, synced, true, checkpointed, false),
                    git_commit.as_deref(),
                    planned_push_remote.as_deref(),
                ))
            })?;
            pushed = true;
            pushed_remote = Some(remote_name);
        }
        clear_manual_resolution_state(&repo, &merge_thread.id)?;
        let trust = build_repository_verification_state(&repo);
        let post_ship_action = integrated_ship_next_action(true, pushed, &trust);
        let mut operator = OperatorCommandOutput {
            status: "shipped".to_string(),
            action: "ship".to_string(),
            message: format!(
                "Shipped thread '{}' from a manually resolved integration state",
                merge_thread.id
            ),
            blockers: Vec::new(),
            warnings: Vec::new(),
            next_action: post_ship_action.clone(),
            recommended_action: post_ship_action,
        };
        block_operator_claim_if_trust_blocked(&mut operator, &trust);
        return write_ship_output(
            cli,
            &ShipOutput {
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
                performed_steps: ship_performed_steps(captured, synced, true, checkpointed, pushed),
                skipped_steps: ship_skipped_steps(captured, synced, true, checkpointed, pushed),
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
        let recommended_action = integration_blocker_recommended_action(&integration_blockers)
            .unwrap_or_else(|| preview.recommended_action.clone());
        update_integration_policy(&repo, &merge_thread.id, "blocked", &reason)?;
        return write_ship_output(
            cli,
            &ShipOutput {
                operator: OperatorCommandOutput {
                    status: "blocked".to_string(),
                    action: "ship".to_string(),
                    message: format!("Thread '{}' is not eligible for auto-ship", merge_thread.id),
                    blockers: ship_blockers_for_preview(&preview, &integration_blockers),
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
                performed_steps: ship_performed_steps(captured, synced, false, false, false),
                skipped_steps: ship_skipped_steps(captured, synced, false, false, false),
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
            ship_checkpoint_message(&repo, &merge_thread, args.message.as_deref());
        let record = create_git_checkpoint(
            &repo,
            Some(&checkpoint_message),
            worktree_status_options(Some(repo.config())),
        )
        .map_err(|error| {
            anyhow!(RecoveryAdvice::ship_checkpoint_partial_failure(
                &merge_thread.id,
                error,
                ship_performed_steps(captured, synced, integrated, false, false),
            ))
        })?;
        checkpointed = true;
        git_commit = Some(record.git_commit);
    }
    coalesce_ship_integration_and_checkpoint(
        &repo,
        merge_output.merge_state.as_deref(),
        git_commit.as_deref(),
    )
    .context("ship completed but failed to record merge and Git checkpoint as one undo batch")?;

    let mut pushed = false;
    let mut pushed_remote = None;
    if integrated && should_push {
        let remote_name = push_after_ship(
            cli,
            &repo,
            planned_push_remote.clone(),
            merge_output.merge_state.clone(),
            captured,
            synced,
            integrated,
            checkpointed,
            git_commit.as_deref(),
        )
        .await
        .map_err(|error| {
            anyhow!(RecoveryAdvice::ship_push_partial_failure(
                &merge_thread.id,
                error,
                ship_performed_steps(captured, synced, integrated, checkpointed, false),
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
    let integrated_next_action = integrated_ship_next_action(integrated, pushed, &trust);
    let mut operator = OperatorCommandOutput {
        status: if integrated { "shipped" } else { "blocked" }.to_string(),
        action: "ship".to_string(),
        message: if integrated {
            format!("Shipped thread '{}'", merge_thread.id)
        } else {
            format!("Thread '{}' could not be shipped cleanly", merge_thread.id)
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
    block_operator_claim_if_trust_blocked(&mut operator, &trust);

    write_ship_output(
        cli,
        &ShipOutput {
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
            performed_steps: ship_performed_steps(
                captured,
                synced,
                integrated,
                checkpointed,
                pushed,
            ),
            skipped_steps: ship_skipped_steps(captured, synced, integrated, checkpointed, pushed),
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

async fn push_after_ship(
    cli: &Cli,
    repo: &Repository,
    remote: Option<String>,
    state: Option<String>,
    _captured: bool,
    _synced: bool,
    _integrated: bool,
    _checkpointed: bool,
    _git_commit: Option<&str>,
) -> Result<String> {
    if repo.capability() == repo::RepositoryCapability::GitOverlay && !repo.hosted_enabled() {
        let (remote_name, _, _, _, _) =
            super::remote::push_git_overlay_refs(repo, remote.as_deref(), false)?;
        Ok(remote_name)
    } else {
        let pushed_remote = remote
            .clone()
            .or(super::remote::resolved_default_remote_name(repo)?)
            .unwrap_or_else(|| "default".to_string());
        super::remote::cmd_push(cli, remote, None, state, false, false).await?;
        Ok(pushed_remote)
    }
}

fn ship_performed_steps(
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
    .filter_map(|(done, step)| done.then(|| step.to_string()))
    .collect()
}

fn ship_skipped_steps(
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
    .filter_map(|(skipped, step)| skipped.then(|| step.to_string()))
    .collect()
}

fn integrated_ship_next_action(
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

fn block_operator_claim_if_trust_blocked(
    operator: &mut OperatorCommandOutput,
    trust: &RepositoryVerificationState,
) {
    if trust.verified || operator.status == "blocked" || operator.status == "failed" {
        return;
    }
    if operator.action == "ship"
        && operator.status == "shipped"
        && trust.recommended_action == "heddle push"
        && matches!(
            trust.remote_drift.as_str(),
            "remote_untracked" | "remote_ahead"
        )
    {
        return;
    }

    let blocked = OperatorCommandOutput::blocked_by_repository_verification(
        operator.action.clone(),
        format!(
            "{} reached its local state checks, but repository verification is blocked: {}",
            operator.action, trust.summary
        ),
        trust,
    );
    *operator = blocked;
}

fn ship_checkpoint_preflight_advice(repo: &Repository, thread_id: &str) -> Option<RecoveryAdvice> {
    if repo.capability() != repo::RepositoryCapability::GitOverlay {
        return None;
    }
    let trust = build_repository_verification_state(repo);
    if matches!(
        trust.remote_drift.as_str(),
        "remote_behind" | "remote_diverged"
    ) {
        let primary_command = if trust.recommended_action.trim().is_empty() {
            "heddle pull".to_string()
        } else {
            trust.recommended_action.clone()
        };
        let recovery_commands = if trust.recovery_commands.is_empty() {
            vec![
                primary_command.clone(),
                format!("heddle merge {thread_id} --preview"),
                format!("heddle ship --thread {thread_id} --no-push"),
            ]
        } else {
            trust.recovery_commands.clone()
        };
        return Some(RecoveryAdvice::safety_refusal(
            "ship_requires_current_upstream",
            format!("Refusing to ship '{thread_id}': upstream work must be integrated first"),
            format!("Run `{primary_command}`, then preview and retry the ship."),
            format!(
                "repository verification reports {}: {}",
                trust.remote_drift, trust.summary
            ),
            "ship would first land Heddle state locally, then fail while writing the Git checkpoint because the checkout branch is behind its upstream",
            "thread refs, Heddle refs, Git refs, index, and worktree files were left unchanged",
            primary_command,
            recovery_commands,
        ));
    }
    if repo.root().join(".git/index.lock").exists() {
        return Some(RecoveryAdvice::safety_refusal(
            "ship_checkpoint_preflight_blocked",
            format!("Refusing to ship '{thread_id}': Git index is locked"),
            "Remove the stale Git index lock or wait for the active Git operation to finish, then retry the ship.",
            ".git/index.lock exists in the parent checkout",
            "ship would first land Heddle state locally, then fail while writing the Git checkpoint because the Git index is locked",
            "thread refs, Heddle refs, Git refs, index, and worktree files were left unchanged",
            "heddle status",
            vec!["heddle status".to_string()],
        ));
    }
    None
}

fn ship_checkpoint_message(repo: &Repository, thread: &Thread, explicit: Option<&str>) -> String {
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
    format!("Ship {}", thread.id)
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

    emit(
        cli,
        &DelegateOutput {
            parent_thread: parent.id,
            delegated,
            message: "Delegated child threads created".to_string(),
        },
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
        format!("auto-ship blocked: {reason}")
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

fn coalesce_ship_integration_and_checkpoint(
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

    let integration_batch = find_recent_ship_integration_batch(repo, merge_state)?;
    let checkpoint_batch = find_recent_ship_git_checkpoint_batch(repo, git_commit)?;
    repo.oplog()
        .coalesce_batches(integration_batch.id, checkpoint_batch.id)?;
    Ok(())
}

fn find_recent_ship_integration_batch(repo: &Repository, merge_state: &str) -> Result<OpBatch> {
    repo.oplog()
        .recent_batches_scoped(12, Some(&repo.op_scope()))?
        .into_iter()
        .find(|batch| {
            batch
                .entries
                .iter()
                .any(|entry| op_targets_merge_state(&entry.operation, merge_state))
        })
        .ok_or_else(|| anyhow!("ship merge succeeded but its oplog batch was not found"))
}

fn find_recent_ship_git_checkpoint_batch(repo: &Repository, git_commit: &str) -> Result<OpBatch> {
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
        .ok_or_else(|| anyhow!("ship Git checkpoint succeeded but its oplog batch was not found"))
}

fn op_targets_merge_state(op: &OpRecord, merge_state: &str) -> bool {
    match op {
        OpRecord::Snapshot { new_state, .. } => change_id_matches_display(new_state, merge_state),
        OpRecord::Checkpoint { state, .. } => change_id_matches_display(state, merge_state),
        OpRecord::Goto { target, .. } => change_id_matches_display(target, merge_state),
        OpRecord::FastForwardV2 { post_target_id, .. } => {
            change_id_matches_display(post_target_id, merge_state)
        }
        OpRecord::FastForward { .. } => false,
        _ => false,
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
    let target = repo.refs().get_thread(&thread.thread)?.ok_or_else(|| {
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

const AUTO_SHIP_CONFIDENCE_THRESHOLD: f32 = 0.75;
const AUTO_SHIP_CONFIDENCE_RECOVERY_ACTION: &str =
    "heddle capture -m \"...\" --confidence <confidence>";

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
    blockers.extend(auto_ship_policy_blockers(repo, thread));
    blockers
}

pub(crate) fn auto_ship_policy_blockers(repo: &Repository, thread: &Thread) -> Vec<String> {
    let mut blockers = Vec::new();
    let agent_authored = thread_is_agent_authored(repo, thread);
    if agent_authored {
        if let Some(confidence) = thread.confidence_summary.value
            && confidence < AUTO_SHIP_CONFIDENCE_THRESHOLD
        {
            blockers.push(format!(
                "confidence {:.2} is below the auto-ship threshold of 0.75",
                confidence
            ));
        }
    }
    if matches!(thread.verification_summary.tests_passed, Some(false)) {
        blockers.push("verification summary reports failing tests".to_string());
    }
    blockers
}

pub(crate) fn integration_blocker_recommended_action(blockers: &[String]) -> Option<String> {
    blockers
        .iter()
        .any(|blocker| {
            blocker.starts_with("confidence ")
                || blocker == "verification summary reports failing tests"
        })
        .then(|| AUTO_SHIP_CONFIDENCE_RECOVERY_ACTION.to_string())
}

fn ship_blockers_for_preview(
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
        .get_thread(&thread.thread)
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
        .or_else(|| repo.refs().get_thread(&thread.thread).ok().flatten());
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

fn emit<T: Serialize>(cli: &Cli, output: &T) -> Result<()> {
    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(output)?);
    } else {
        println!("{}", serde_json::to_string_pretty(output)?);
    }
    Ok(())
}

fn write_ship_output(cli: &Cli, output: &ShipOutput) -> Result<()> {
    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(output)?);
    } else {
        let marker = match output.operator.status.as_str() {
            "shipped" => style::ok_marker(),
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
                            .map(|step| ship_text_step(step))
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
                            .map(|step| ship_text_step(step))
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
            println!("Next: {}", style::bold(next));
        }
    }
    exit_if_blocked_operator_status(&output.operator.status);
    Ok(())
}

fn ship_text_step(step: &str) -> String {
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
