// SPDX-License-Identifier: Apache-2.0
//! Thread capture-split and thread-move repository operations.

use std::{error::Error, fmt, fs, path::Path};

use anyhow::{Result, anyhow};
use chrono::Utc;
use objects::{
    fs_ops::remove_path_recursively,
    object::{StateId, ThreadName},
    store::ObjectStore,
};
use refs::Head;
use repo::{
    Repository, Thread, ThreadFreshness, ThreadManager, ThreadMode, ThreadState,
    WorktreeStatusOptions,
};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ThreadMoveOutput {
    pub from_thread: String,
    pub to_thread: String,
    pub moved_paths: Vec<String>,
    pub source_state_id: Option<String>,
    pub target_state_id: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct CaptureSplitOptions {
    pub into: String,
    pub prefixes: Vec<String>,
    pub intent: Option<String>,
    pub worktree_status_options: WorktreeStatusOptions,
}

#[derive(Debug, Clone)]
pub struct ThreadMoveOptions {
    pub from: String,
    pub to: String,
    pub prefixes: Vec<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoPathsMatchedDetails {
    pub action: &'static str,
    pub error: &'static str,
    pub unsafe_condition: &'static str,
    pub would_change: &'static str,
    pub primary_command: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadShapingError {
    NoCurrentThread,
    NoPathsMatched(NoPathsMatchedDetails),
    ThreadNotFound {
        thread_id: String,
        action: &'static str,
    },
    ImportedGitRefNotManaged {
        thread_id: String,
    },
}

impl fmt::Display for ThreadShapingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCurrentThread => write!(f, "No current thread"),
            Self::NoPathsMatched(details) => write!(f, "{}", details.error),
            Self::ThreadNotFound { thread_id, .. } => {
                write!(f, "Thread '{thread_id}' not found")
            }
            Self::ImportedGitRefNotManaged { thread_id } => write!(
                f,
                "'{thread_id}' is an imported Git ref, not a managed Heddle thread"
            ),
        }
    }
}

impl Error for ThreadShapingError {}

pub fn capture_split(
    repo: &Repository,
    opts: CaptureSplitOptions,
    snapshot: impl Fn(&Repository, Option<String>) -> Result<String>,
) -> Result<ThreadMoveOutput> {
    let current = current_thread(repo)?.ok_or(ThreadShapingError::NoCurrentThread)?;
    let target = load_thread(repo, &opts.into, "load thread")?;
    let moved_paths =
        collect_worktree_split_paths(repo, &opts.prefixes, &opts.worktree_status_options)?;
    if moved_paths.is_empty() {
        return Err(ThreadShapingError::NoPathsMatched(no_paths_matched_details(
            "capture split",
            "No dirty paths matched the requested split prefixes",
            "the worktree has no dirty paths under the requested prefixes",
            "capture --split would not move any work into the target thread",
            "heddle status",
        ))
        .into());
    }

    let target_repo = Repository::open(&target.execution_path)?;
    apply_selected_worktree_paths(repo, &target_repo, &moved_paths)?;
    let target_snapshot = snapshot(
        &target_repo,
        Some(
            opts.intent
                .unwrap_or_else(|| format!("Split paths from {}", current.id)),
        ),
    )?;

    restore_paths_from_state(repo, repo.head()?, &moved_paths)?;

    Ok(ThreadMoveOutput {
        from_thread: current.id,
        to_thread: target.id,
        moved_paths,
        source_state_id: None,
        target_state_id: target_snapshot,
        message: "Split selected paths into target thread".to_string(),
    })
}

pub fn thread_move(
    repo: &Repository,
    opts: ThreadMoveOptions,
    snapshot: impl Fn(&Repository, Option<String>) -> Result<String>,
) -> Result<ThreadMoveOutput> {
    let source = load_thread(repo, &opts.from, "load thread")?;
    let target = load_thread(repo, &opts.to, "load thread")?;
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
        collect_state_move_paths(&source_repo, &source_base, &source_current, &opts.prefixes)?;
    if moved_paths.is_empty() {
        return Err(ThreadShapingError::NoPathsMatched(no_paths_matched_details(
            "thread move",
            "No captured paths matched the requested prefixes",
            "the source thread has no captured paths under the requested prefixes",
            "thread move would not move any captured files into the target thread",
            "heddle thread show",
        ))
        .into());
    }

    apply_selected_state_paths(&source_repo, &source_current, &target_repo, &moved_paths)?;
    let target_snapshot = snapshot(
        &target_repo,
        Some(
            opts.message
                .clone()
                .unwrap_or_else(|| format!("Move paths from {}", source.id)),
        ),
    )?;

    restore_paths_from_state(&source_repo, Some(source_base), &moved_paths)?;
    let source_snapshot = snapshot(
        &source_repo,
        Some(
            opts.message
                .unwrap_or_else(|| format!("Move paths to {}", target.id)),
        ),
    )?;

    Ok(ThreadMoveOutput {
        from_thread: source.id,
        to_thread: target.id,
        moved_paths,
        source_state_id: Some(source_snapshot),
        target_state_id: target_snapshot,
        message: "Moved selected paths between threads".to_string(),
    })
}

fn thread_manager(repo: &Repository) -> ThreadManager {
    ThreadManager::new(repo.heddle_dir())
}

fn current_thread(repo: &Repository) -> Result<Option<Thread>> {
    if let Some(thread) = thread_manager(repo).find_by_execution_root(repo.root())? {
        return Ok(Some(thread));
    }

    let Head::Attached { thread } = repo.head_ref()? else {
        return Ok(None);
    };
    let current_state = repo.refs().get_thread(&thread)?.map(|id| id.short());
    let base_root = current_state
        .as_deref()
        .and_then(|state| repo.resolve_state(state).ok().flatten())
        .and_then(|id| repo.store().get_state(&id).ok().flatten())
        .map(|state| state.tree.short())
        .unwrap_or_default();

    let thread_str = thread.to_string();
    Ok(Some(Thread {
        id: thread_str.clone(),
        thread: thread_str,
        target_thread: None,
        parent_thread: None,
        mode: ThreadMode::Materialized,
        state: ThreadState::Active,
        base_state: current_state.clone().unwrap_or_default(),
        base_root,
        current_state,
        merged_state: None,
        task: None,
        execution_path: repo.root().to_path_buf(),
        materialized_path: None,
        changed_paths: Vec::new(),
        impact_categories: Vec::new(),
        heavy_impact_paths: Vec::new(),
        promotion_suggested: false,
        freshness: ThreadFreshness::Unknown,
        verification_summary: Default::default(),
        confidence_summary: Default::default(),
        integration_policy_result: Default::default(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        ephemeral: None,
        auto: false,
        shared_target_dir: None,
    }))
}

fn load_thread(repo: &Repository, thread_id: &str, action: &'static str) -> Result<Thread> {
    match thread_manager(repo).load(thread_id)? {
        Some(thread) => Ok(thread),
        None if repo
            .refs()
            .get_thread(&ThreadName::new(thread_id))?
            .is_some() =>
        {
            Err(ThreadShapingError::ImportedGitRefNotManaged {
                thread_id: thread_id.to_string(),
            }
            .into())
        }
        None => Err(ThreadShapingError::ThreadNotFound {
            thread_id: thread_id.to_string(),
            action,
        }
        .into()),
    }
}

fn no_paths_matched_details(
    action: &'static str,
    error: &'static str,
    unsafe_condition: &'static str,
    would_change: &'static str,
    primary_command: &'static str,
) -> NoPathsMatchedDetails {
    NoPathsMatchedDetails {
        action,
        error,
        unsafe_condition,
        would_change,
        primary_command,
    }
}

fn resolve_required_state(repo: &Repository, spec: Option<&str>, message: &str) -> Result<StateId> {
    let spec = spec.ok_or_else(|| anyhow!(message.to_string()))?;
    repo.resolve_state(spec)?
        .ok_or_else(|| anyhow!(message.to_string()))
}

fn collect_worktree_split_paths(
    repo: &Repository,
    prefixes: &[String],
    worktree_status_options: &WorktreeStatusOptions,
) -> Result<Vec<String>> {
    let baseline = match repo.current_state()? {
        Some(state) => repo.require_tree(&state.tree)?,
        None => objects::object::Tree::new(),
    };
    let status = repo.compare_worktree_cached_with_options(&baseline, worktree_status_options)?;
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
    base: &StateId,
    current: &StateId,
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
    state_id: &StateId,
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
    baseline: Option<StateId>,
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
        let Some(hash) = entry.leaf_content_hash() else {
            return Ok(());
        };
        let blob = repo.require_blob(&hash)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_path_movement_refusals_use_typed_error_details() {
        let split = no_paths_matched_details(
            "capture split",
            "No dirty paths matched the requested split prefixes",
            "the worktree has no dirty paths under the requested prefixes",
            "capture --split would not move any work into the target thread",
            "heddle status",
        );
        assert_eq!(split.action, "capture split");
        assert_eq!(split.primary_command, "heddle status");
        assert_eq!(
            split.error,
            "No dirty paths matched the requested split prefixes"
        );

        let move_paths = no_paths_matched_details(
            "thread move",
            "No captured paths matched the requested prefixes",
            "the source thread has no captured paths under the requested prefixes",
            "thread move would not move any captured files into the target thread",
            "heddle thread show",
        );
        assert_eq!(move_paths.action, "thread move");
        assert_eq!(move_paths.primary_command, "heddle thread show");
        assert_eq!(
            move_paths.error,
            "No captured paths matched the requested prefixes"
        );
    }

    #[test]
    fn matches_prefix_respects_directory_boundaries() {
        let prefixes = vec!["auth".to_string()];
        assert!(matches_prefix("auth", &prefixes));
        assert!(matches_prefix("auth/login.rs", &prefixes));
        assert!(!matches_prefix("authz.rs", &prefixes));
    }
}
