// SPDX-License-Identifier: Apache-2.0
//! Heddle-native thread shaping helpers.

use std::{fs, path::Path};

use anyhow::{Result, anyhow};
use objects::{fs_ops::remove_path_recursively, object::ChangeId};
use repo::Repository;
use serde::Serialize;

use super::{
    merge::merge_thread_into_current,
    operator_core::OperatorCommandOutput,
    operator_loop::primary_next_action,
    ready_cmd::worktree_dirty,
    snapshot::{SnapshotAgentOverrides, create_snapshot},
    thread_cmd::{load_thread, refresh_thread, refresh_thread_freshness},
};
use crate::{
    cli::{Cli, should_output_json, style, worktree_status_options},
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
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let current = super::thread_cmd::current_thread(&repo)?.ok_or_else(|| {
        anyhow!("No current thread; `heddle capture --split` requires an active thread checkout")
    })?;
    let target = load_thread(&repo, &into)?;
    let moved_paths = collect_worktree_split_paths(&repo, &prefixes)?;
    if moved_paths.is_empty() {
        return Err(anyhow!(
            "No dirty paths matched the requested split prefixes"
        ));
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
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
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
        return Err(anyhow!("No captured paths matched the requested prefixes"));
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
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let child = load_thread(&repo, &thread)?;
    let parent_id = into
        .or(child.parent_thread.clone())
        .ok_or_else(|| anyhow!("Thread '{}' has no recorded parent; pass --into", child.id))?;
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
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let mut thread = load_thread(&repo, &thread_id)?;
    refresh_thread_freshness(&repo, &mut thread)?;
    let source_root = if thread.execution_path.as_os_str().is_empty() {
        repo.root().to_path_buf()
    } else {
        thread.execution_path.clone()
    };
    let source_repo = Repository::open(&source_root)?;

    if thread.freshness == repo::ThreadFreshness::Stale
        && refresh_thread(&repo, &thread_id, cli).is_ok()
    {
        let manager = super::thread_cmd::thread_manager(&repo);
        let mut refreshed_thread = manager
            .load(&thread_id)?
            .ok_or_else(|| anyhow!("Thread '{}' not found after refresh", thread_id))?;
        let resolved_state = repo
            .refs()
            .get_thread(&refreshed_thread.thread)?
            .map(|id| id.short());
        refreshed_thread.integration_policy_result.status = Some("manual_resolved".to_string());
        refreshed_thread.integration_policy_result.reason =
            Some("manual integration resolution captured".to_string());
        refreshed_thread
            .integration_policy_result
            .manual_resolution_state = resolved_state;
        manager.save(&refreshed_thread)?;
        return emit_thread_resolve(
            cli,
            &ThreadResolveOutput {
                operator: OperatorCommandOutput {
                    status: "synced".to_string(),
                    action: "resolve".to_string(),
                    message: "Thread refreshed cleanly".to_string(),
                    blockers: Vec::new(),
                    warnings: Vec::new(),
                    next_action: Some(format!("heddle ship --thread {}", thread.id)),
                    recommended_action: Some(format!("heddle ship --thread {}", thread.id)),
                },
                thread: thread_id,
            },
        );
    }

    let summary = super::thread::find_thread_summary(&repo, &thread.id)?
        .ok_or_else(|| anyhow!("Thread '{}' not found", thread.id))?;
    let rebase_state_path = source_repo.heddle_dir().join("REBASE_STATE");
    let mut blockers = if rebase_state_path.exists() {
        Vec::new()
    } else {
        summary.blockers.clone()
    };
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
            recommended_action = format!("heddle merge {}", thread.id);
        }
    }
    if blockers.is_empty() {
        let manager = super::thread_cmd::thread_manager(&repo);
        thread.integration_policy_result.status = Some("manual_resolved".to_string());
        thread.integration_policy_result.reason =
            Some("manual integration resolution captured".to_string());
        thread.integration_policy_result.manual_resolution_state =
            repo.refs().get_thread(&thread.thread)?.map(|id| id.short());
        manager.save(&thread)?;
    }
    let recommended_action = if blockers.is_empty() {
        if rebase_state_path.exists() {
            recommended_action
        } else {
            format!("heddle ship --thread {}", summary.name)
        }
    } else {
        recommended_action
    };
    let operation = repo.operation_status()?;
    let remote_tracking = repo.git_remote_tracking_status()?;
    let import_hint = repo.git_overlay_import_hint()?;
    let recommended_action = primary_next_action(
        operation.as_ref(),
        remote_tracking.as_ref(),
        import_hint.as_ref(),
        Some(&recommended_action),
    );
    emit_thread_resolve(
        cli,
        &ThreadResolveOutput {
            operator: OperatorCommandOutput {
                status: if blockers.is_empty() {
                    "completed".to_string()
                } else {
                    "blocked".to_string()
                },
                action: "resolve".to_string(),
                message: "Thread requires a manual follow-up".to_string(),
                blockers: blockers.clone(),
                warnings: Vec::new(),
                next_action: Some(recommended_action.clone()),
                recommended_action: Some(recommended_action),
            },
            thread: summary.name.clone(),
        },
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
    let baseline = repo
        .current_state()?
        .and_then(|state| repo.store().get_tree(&state.tree).transpose())
        .transpose()?
        .unwrap_or_default();
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
    let tree = source_repo
        .store()
        .get_tree(&state.tree)?
        .unwrap_or_default();
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
        Some(repo.store().get_tree(&state.tree)?.unwrap_or_default())
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

fn emit_thread_resolve(cli: &Cli, output: &ThreadResolveOutput) -> Result<()> {
    if should_output_json(cli, None) {
        println!("{}", serde_json::to_string(output)?);
    } else {
        println!("{}", output.operator.message);
        println!("Thread: {}", style::bold(&output.thread));
        if !output.operator.blockers.is_empty() {
            println!("{}", style::warn("Blockers:"));
            for blocker in &output.operator.blockers {
                println!("  - {}", style::warn(blocker));
            }
        }
        if let Some(next) = output
            .operator
            .recommended_action
            .as_ref()
            .or(output.operator.next_action.as_ref())
        {
            println!("Next: {}", style::bold(next));
        }
    }
    Ok(())
}