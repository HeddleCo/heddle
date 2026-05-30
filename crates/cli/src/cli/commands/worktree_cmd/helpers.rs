// SPDX-License-Identifier: Apache-2.0
use objects::store::ObjectStore;
use std::path::{Path, PathBuf};

use anyhow::Result;
use objects::object::ChangeId;
use repo::Repository;

use super::super::advice::RecoveryAdvice;

/// The prepared `--path` target plus whether THIS invocation created the
/// target directory. A compensating rollback must only undo what this
/// invocation created: a directory we created is removed entirely, but a
/// pre-existing empty directory the user supplied is preserved (only the
/// contents we materialized inside it are cleared) — never destroy user
/// state we merely wrote into.
pub(crate) struct PreparedWorktreeTarget {
    pub path: PathBuf,
    pub target_dir_created: bool,
}

pub(crate) fn prepare_worktree_target(
    repo: &Repository,
    path: &Path,
) -> Result<PreparedWorktreeTarget> {
    let requested = absolute_path(path)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&requested)
        && metadata.file_type().is_symlink()
    {
        return Err(anyhow::anyhow!(worktree_target_symlink_advice(&requested)));
    }
    let resolved = canonicalize_existing_ancestor(&requested)?;
    validate_worktree_target(repo, &resolved)?;
    // Capture pre-existence BEFORE we create the dir: this is the only
    // point where "the user gave us an existing empty dir" vs "we made
    // it" is still distinguishable.
    let target_dir_created = !resolved.exists();
    std::fs::create_dir_all(&resolved).map_err(|error| {
        anyhow::anyhow!(worktree_target_prepare_failed_advice(&requested, error))
    })?;
    validate_worktree_target(repo, &resolved)?;
    Ok(PreparedWorktreeTarget {
        path: resolved,
        target_dir_created,
    })
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn canonicalize_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut ancestor = path;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| anyhow::anyhow!(worktree_target_invalid_path_advice(path)))?;
    }

    let mut resolved = ancestor.canonicalize()?;
    let remainder = path
        .strip_prefix(ancestor)
        .map_err(|_| anyhow::anyhow!(worktree_target_invalid_path_advice(path)))?;

    for component in remainder.components() {
        match component {
            std::path::Component::Normal(part) => resolved.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::Prefix(_)
            | std::path::Component::RootDir => {
                return Err(anyhow::anyhow!(worktree_target_unsafe_path_advice(path)));
            }
        }
    }

    Ok(resolved)
}

fn validate_worktree_target(repo: &Repository, path: &Path) -> Result<()> {
    if path == repo.heddle_dir() || path.starts_with(repo.heddle_dir()) {
        return Err(anyhow::anyhow!(worktree_target_storage_advice(path)));
    }

    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(anyhow::anyhow!(worktree_target_symlink_advice(path)));
        }

        if !metadata.is_dir() {
            return Err(anyhow::anyhow!(worktree_target_not_directory_advice(path)));
        }

        if std::fs::read_dir(path)?.next().transpose()?.is_some() {
            return Err(anyhow::anyhow!(worktree_target_not_empty_advice(path)));
        }
    }

    Ok(())
}

fn worktree_target_symlink_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_symlink",
        format!("worktree target '{}' cannot be a symlink", path.display()),
        "Choose an empty real directory for `--path`, or let Heddle create a managed materialized checkout.",
        format!(
            "target path '{}' resolves through a symlink",
            path.display()
        ),
        "writing an isolated checkout through a symlink could target a different location than the caller sees",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --workspace materialized",
        vec![
            "heddle start <name> --workspace materialized".to_string(),
            "heddle start <name> --path <empty-path>".to_string(),
        ],
    )
}

fn worktree_target_prepare_failed_advice(path: &Path, error: std::io::Error) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_prepare_failed",
        format!(
            "Could not prepare isolated thread workspace '{}': {error}",
            path.display()
        ),
        "Choose an empty writable path with `--path`, or let Heddle create a managed materialized checkout.",
        format!("target path '{}' could not be created", path.display()),
        "continuing would leave the isolated checkout only partially prepared",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --workspace materialized",
        vec![
            "heddle start <name> --workspace materialized".to_string(),
            "heddle start <name> --path <empty-path>".to_string(),
        ],
    )
}

fn worktree_target_invalid_path_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_invalid_path",
        format!("invalid worktree path '{}'", path.display()),
        "Choose an empty writable path for `--path`, or let Heddle create a managed materialized checkout.",
        format!("target path '{}' has no usable ancestor", path.display()),
        "continuing would make checkout placement ambiguous",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --workspace materialized",
        vec![
            "heddle start <name> --workspace materialized".to_string(),
            "heddle start <name> --path <empty-path>".to_string(),
        ],
    )
}

fn worktree_target_unsafe_path_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_unsafe_path",
        format!("unsafe worktree path '{}'", path.display()),
        "Choose a normal empty path for `--path`; avoid parent-directory traversal.",
        format!(
            "target path '{}' contains an unsafe component",
            path.display()
        ),
        "continuing could write outside the intended checkout location",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --path <empty-path>",
        vec!["heddle start <name> --path <empty-path>".to_string()],
    )
}

fn worktree_target_storage_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_in_heddle_storage",
        format!(
            "worktree target '{}' cannot point into .heddle storage",
            path.display()
        ),
        "Choose a checkout path outside `.heddle`, preferably a sibling directory.",
        format!(
            "target path '{}' is inside repository metadata storage",
            path.display()
        ),
        "writing a checkout there could corrupt Heddle repository metadata",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --path ../<name>",
        vec!["heddle start <name> --path ../<name>".to_string()],
    )
}

fn worktree_target_not_directory_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_not_directory",
        format!("worktree target '{}' must be a directory", path.display()),
        "Choose an empty directory path for `--path`, or let Heddle create a managed materialized checkout.",
        format!(
            "target path '{}' exists but is not a directory",
            path.display()
        ),
        "continuing would overwrite a non-directory path",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --workspace materialized",
        vec![
            "heddle start <name> --workspace materialized".to_string(),
            "heddle start <name> --path <empty-path>".to_string(),
        ],
    )
}

fn worktree_target_not_empty_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_not_empty",
        format!("worktree target '{}' is not empty", path.display()),
        "Use an empty path, capture current work with `heddle capture`, or let Heddle create a managed materialized checkout.",
        format!("target path '{}' already contains files", path.display()),
        "writing an isolated checkout there could overwrite or mix with existing work",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --workspace materialized",
        vec![
            "heddle start <name> --workspace materialized".to_string(),
            "heddle start <name> --path <empty-path>".to_string(),
            "heddle capture -m \"...\"".to_string(),
        ],
    )
}

pub(crate) fn write_isolated_checkout(
    repo: &Repository,
    abs_path: &Path,
    base_state: &ChangeId,
    thread: Option<&str>,
) -> Result<()> {
    let heddle_dir = abs_path.join(".heddle");
    if heddle_dir.exists() {
        return Err(anyhow::anyhow!(worktree_target_existing_heddle_advice(
            abs_path
        )));
    }
    let shared_galeed_dir = repo.heddle_dir();
    std::fs::create_dir_all(&heddle_dir)?;
    {
        use std::io::Write as _;
        let mut pointer_file = std::fs::File::create(heddle_dir.join("objectstore"))?;
        pointer_file
            .write_all(format!("objectstore: {}\n", shared_galeed_dir.display()).as_bytes())?;
        pointer_file.sync_all()?;
    }
    std::fs::create_dir_all(heddle_dir.join("state"))?;

    let checkout_head = heddle_dir.join("HEAD");
    let head_content = match thread {
        Some(thread) => format!("ref: {}\n", thread),
        None => format!("{}\n", base_state.to_string_full()),
    };
    {
        use std::io::Write as _;
        let mut head_file = std::fs::File::create(&checkout_head)?;
        head_file.write_all(head_content.as_bytes())?;
        head_file.sync_all()?;
    }

    let state = repo
        .store()
        .get_state(base_state)?
        .ok_or_else(|| anyhow::anyhow!("State not found in object store"))?;
    let tree = repo
        .store()
        .get_tree(&state.tree)?
        .ok_or_else(|| anyhow::anyhow!("Tree not found in object store"))?;
    repo.materialize_tree(&tree, abs_path)?;
    Ok(())
}

fn worktree_target_existing_heddle_advice(path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "worktree_target_existing_heddle",
        format!("'{}' already has a .heddle directory", path.display()),
        "Choose a path that is not already a Heddle checkout.",
        format!(
            "target path '{}' already contains Heddle checkout metadata",
            path.display()
        ),
        "reusing that path could attach the new thread to the wrong checkout metadata",
        "no thread, checkout, repository object, ref, or worktree file was changed",
        "heddle start <name> --path <empty-path>",
        vec!["heddle start <name> --path <empty-path>".to_string()],
    )
}
