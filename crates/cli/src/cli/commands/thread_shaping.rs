// SPDX-License-Identifier: Apache-2.0
//! Heddle-native thread shaping helpers.

use std::{fs, path::Path};

use anyhow::{Result, anyhow};
use objects::{
    fs_ops::remove_path_recursively,
    object::{ChangeId, ThreadName},
    store::LocalObjectStore,
};
use repo::{GitOverlayImportHint, GitRemoteTrackingStatus, Repository, RepositoryOperationStatus};
use serde::Serialize;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    git_overlay_health::{RepositoryVerificationState, build_repository_verification_state},
    merge::merge_thread_into_current,
    next_action::{NextActionValidationContext, write_command_json},
    operator_core::{OperatorAction, OperatorCommandOutput},
    operator_loop::primary_next_action,
    ready_cmd::worktree_dirty,
    snapshot::{SnapshotAgentOverrides, create_snapshot},
    thread_cmd::{
        capture_thread_update_before, current_thread_ref_state, load_thread, refresh_thread,
        refresh_thread_freshness, save_thread_update_with_oplog, thread_not_found_advice,
    },
    thread_landing::{land_command_for_thread, land_command_with_push_target},
};
use crate::{
    cli::{
        Cli, output_is_compact, render::shell_quote, should_output_json, style,
        worktree_status_options,
    },
    config::UserConfig,
};

#[derive(Debug, Serialize)]
pub struct ThreadMoveOutput {
    pub from_thread: String,
    pub to_thread: String,
    pub moved_paths: Vec<String>,
    pub source_change_id: Option<String>,
    pub target_change_id: String,
    pub message: String,
}

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
    let current = super::thread_cmd::current_thread(&repo)?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::no_current_thread(
            "capture --split",
            None,
            "heddle thread switch <name>",
        ))
    })?;
    let target = load_thread(&repo, &into)?;
    let moved_paths = collect_worktree_split_paths(&repo, &prefixes)?;
    if moved_paths.is_empty() {
        return Err(anyhow!(no_paths_matched_advice(
            "capture split",
            "No dirty paths matched the requested split prefixes",
            "the worktree has no dirty paths under the requested prefixes",
            "capture --split would not move any work into the target thread",
            "heddle status",
        )));
    }

    let target_repo = Repository::open(&target.execution_path)?;
    apply_selected_worktree_paths(&repo, &target_repo, &moved_paths)?;
    let user_config = UserConfig::load_default().unwrap_or_default();
    let target_snapshot = create_snapshot(
        &target_repo,
        &user_config,
        Some(intent.unwrap_or_else(|| format!("Split paths from {}", current.id))),
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

    restore_paths_from_state(&repo, repo.head()?, &moved_paths)?;

    let output = ThreadMoveOutput {
        from_thread: current.id,
        to_thread: target.id,
        moved_paths,
        source_change_id: None,
        target_change_id: target_snapshot.change_id,
        message: "Split selected paths into target thread".to_string(),
    };
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
    let source = load_thread(&repo, &from)?;
    let target = load_thread(&repo, &to)?;
    let source_repo = Repository::open(&source.execution_path)?;
    let target_repo = Repository::open(&target.execution_path)?;

    let source_current = resolve_required_state(
        &source_repo,
        source.current_state.as_deref(),
        "source thread has no current state",
    )?;
    let source_base = resolve_required_state(
        &source_repo,
        Some(&source.base_state),
        "source thread has no base state",
    )?;
    let moved_paths =
        collect_state_move_paths(&source_repo, &source_base, &source_current, &prefixes)?;
    if moved_paths.is_empty() {
        return Err(anyhow!(no_paths_matched_advice(
            "thread move",
            "No captured paths matched the requested prefixes",
            "the source thread has no captured paths under the requested prefixes",
            "thread move would not move any captured files into the target thread",
            "heddle thread show",
        )));
    }

    apply_selected_state_paths(&source_repo, &source_current, &target_repo, &moved_paths)?;
    let user_config = UserConfig::load_default().unwrap_or_default();
    let target_snapshot = create_snapshot(
        &target_repo,
        &user_config,
        Some(
            message
                .clone()
                .unwrap_or_else(|| format!("Move paths from {}", source.id)),
        ),
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

    restore_paths_from_state(&source_repo, Some(source_base), &moved_paths)?;
    let source_snapshot = create_snapshot(
        &source_repo,
        &user_config,
        Some(message.unwrap_or_else(|| format!("Move paths to {}", target.id))),
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

    let output = ThreadMoveOutput {
        from_thread: source.id,
        to_thread: target.id,
        moved_paths,
        source_change_id: Some(source_snapshot.change_id),
        target_change_id: target_snapshot.change_id,
        message: "Moved selected paths between threads".to_string(),
    };
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
    let user_config = UserConfig::load_default().unwrap_or_default();
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
            recommended_action = "heddle rebase --continue".to_string();
        } else {
            blockers.push(
                "refresh has a rebase in progress; capture a manual resolution in the thread checkout, then run `heddle rebase --continue`".to_string(),
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
    let import_hint = repo.git_overlay_import_hint()?;
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

fn is_manual_review_blocker(blocker: &str) -> bool {
    blocker.starts_with("Heavy-impact change:")
}

fn thread_resolve_next_action(
    blockers: &[String],
    operation: Option<&RepositoryOperationStatus>,
    remote_tracking: Option<&GitRemoteTrackingStatus>,
    import_hint: Option<&GitOverlayImportHint>,
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
            "refresh has a rebase in progress; capture a manual resolution in the thread checkout, then run `heddle rebase --continue`".to_string(),
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
    let land_command = land_command_with_push_target(thread_id, trust.default_remote.is_some());
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

fn no_paths_matched_advice(
    action: &'static str,
    error: &'static str,
    unsafe_condition: &'static str,
    would_change: &'static str,
    primary_command: &'static str,
) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "no_paths_matched",
        error,
        format!(
            "Inspect available paths with `{primary_command}`, then retry `{action}` with a matching prefix."
        ),
        unsafe_condition,
        would_change,
        "repository state was left unchanged",
        primary_command,
        vec![primary_command.to_string()],
    )
}

fn resolve_required_state(
    repo: &Repository,
    spec: Option<&str>,
    message: &str,
) -> Result<ChangeId> {
    let spec = spec.ok_or_else(|| anyhow!(message.to_string()))?;
    repo.resolve_state(spec)?
        .ok_or_else(|| anyhow!(message.to_string()))
}

fn collect_worktree_split_paths(repo: &Repository, prefixes: &[String]) -> Result<Vec<String>> {
    let baseline = match repo.current_state()? {
        Some(state) => repo.require_tree(&state.tree)?,
        None => objects::object::Tree::new(),
    };
    let status = repo.compare_worktree_cached_with_options(
        &baseline,
        &worktree_status_options(Some(repo.config())),
    )?;
    let mut paths = status
        .modified
        .iter()
        .chain(status.added.iter())
        .chain(status.deleted.iter())
        .map(|path| path.to_string_lossy().to_string())
        .filter(|path| matches_prefix(path, prefixes))
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn collect_state_move_paths(
    repo: &Repository,
    base: &ChangeId,
    current: &ChangeId,
    prefixes: &[String],
) -> Result<Vec<String>> {
    let base_tree = repo
        .store()
        .get_state(base)?
        .ok_or_else(|| anyhow!("Base state not found"))?
        .tree;
    let current_tree = repo
        .store()
        .get_state(current)?
        .ok_or_else(|| anyhow!("Current state not found"))?
        .tree;
    let mut paths = repo
        .diff_trees(&base_tree, &current_tree)?
        .into_iter()
        .map(|change| change.path)
        .filter(|path| matches_prefix(path, prefixes))
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn apply_selected_worktree_paths(
    source_repo: &Repository,
    target_repo: &Repository,
    paths: &[String],
) -> Result<()> {
    for path in paths {
        let source_path = source_repo.root().join(path);
        let target_path = target_repo.root().join(path);
        if source_path.exists() {
            copy_path(&source_path, &target_path)?;
        } else if target_path.exists() {
            remove_path_recursively(&target_path)?;
        }
    }
    Ok(())
}

fn apply_selected_state_paths(
    source_repo: &Repository,
    state_id: &ChangeId,
    target_repo: &Repository,
    paths: &[String],
) -> Result<()> {
    let state = source_repo
        .store()
        .get_state(state_id)?
        .ok_or_else(|| anyhow!("State '{}' not found", state_id.short()))?;
    let tree = source_repo.require_tree(&state.tree)?;
    for path in paths {
        restore_one_path(target_repo, Some(&tree), path)?;
    }
    Ok(())
}

fn restore_paths_from_state(
    repo: &Repository,
    baseline: Option<ChangeId>,
    paths: &[String],
) -> Result<()> {
    let tree = if let Some(state_id) = baseline {
        let state = repo
            .store()
            .get_state(&state_id)?
            .ok_or_else(|| anyhow!("Baseline state '{}' not found", state_id.short()))?;
        Some(repo.require_tree(&state.tree)?)
    } else {
        None
    };
    for path in paths {
        restore_one_path(repo, tree.as_ref(), path)?;
    }
    Ok(())
}

fn restore_one_path(
    repo: &Repository,
    baseline_tree: Option<&objects::object::Tree>,
    path: &str,
) -> Result<()> {
    let target_path = repo.root().join(path);
    if let Some(tree) = baseline_tree
        && let Some(entry) = tree.get(path)
    {
        let blob = repo.require_blob(&entry.hash)?;
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target_path, blob.content())?;
        return Ok(());
    }

    if target_path.exists() {
        remove_path_recursively(&target_path)?;
    }
    Ok(())
}

fn copy_path(from: &Path, to: &Path) -> Result<()> {
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_path(&entry.path(), &to.join(entry.file_name()))?;
        }
        return Ok(());
    }

    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(from, to)?;
    Ok(())
}

fn matches_prefix(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| {
        let prefix = prefix.trim_matches('/');
        path == prefix || path.starts_with(&format!("{prefix}/"))
    })
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
    use crate::cli::commands::git_overlay_health::VerificationCheck;

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
            recommended_action: (!verified).then(|| "heddle adopt --ref main".to_string()),
            recommended_action_template: None,
            recovery_commands: if verified {
                Vec::new()
            } else {
                vec!["heddle adopt --ref main".to_string()]
            },
            recovery_action_templates: Vec::new(),
            details: std::collections::BTreeMap::new(),
        };
        let machine_contract_coverage =
            crate::cli::commands::git_overlay_health::machine_contract_coverage();
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
            machine_contract: crate::cli::commands::git_overlay_health::machine_contract_status(
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
            Some("heddle land --thread feature/clean --no-push")
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
            Some("heddle adopt --ref main")
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
    fn empty_path_movement_refusals_use_typed_advice() {
        let split = no_paths_matched_advice(
            "capture split",
            "No dirty paths matched the requested split prefixes",
            "the worktree has no dirty paths under the requested prefixes",
            "capture --split would not move any work into the target thread",
            "heddle status",
        );
        assert_eq!(split.kind, "no_paths_matched");
        assert_eq!(split.primary_command, "heddle status");
        assert!(
            split
                .to_string()
                .contains("Preserved: repository state was left unchanged"),
            "display should keep the uniform advice surface: {split}"
        );

        let move_paths = no_paths_matched_advice(
            "thread move",
            "No captured paths matched the requested prefixes",
            "the source thread has no captured paths under the requested prefixes",
            "thread move would not move any captured files into the target thread",
            "heddle thread show",
        );
        assert_eq!(move_paths.kind, "no_paths_matched");
        assert_eq!(move_paths.primary_command, "heddle thread show");
        assert!(
            move_paths.primary_hint().contains("heddle thread show"),
            "hint should name the inspection command: {}",
            move_paths.primary_hint()
        );
    }
}
