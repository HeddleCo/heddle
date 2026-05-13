// SPDX-License-Identifier: Apache-2.0
use std::path::{Path, PathBuf};

use anyhow::Result;
use objects::object::ChangeId;
use repo::Repository;

pub(crate) fn prepare_worktree_target(repo: &Repository, path: &Path) -> Result<PathBuf> {
    let requested = absolute_path(path)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&requested)
        && metadata.file_type().is_symlink()
    {
        return Err(anyhow::anyhow!(
            "worktree target '{}' cannot be a symlink",
            requested.display()
        ));
    }
    let resolved = canonicalize_existing_ancestor(&requested)?;
    validate_worktree_target(repo, &resolved)?;
    std::fs::create_dir_all(&resolved).map_err(|error| {
        anyhow::anyhow!(
            "Could not prepare heavy thread workspace '{}': {}.\n\
             This checkout may have uncaptured work or a protected parent directory. \
             Use `heddle capture`, `heddle start --workspace heavy <name>`, or choose an empty writable path with `--path`.",
            requested.display(),
            error
        )
    })?;
    validate_worktree_target(repo, &requested)?;
    Ok(requested)
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
            .ok_or_else(|| anyhow::anyhow!("invalid worktree path '{}'", path.display()))?;
    }

    let mut resolved = ancestor.canonicalize()?;
    let remainder = path
        .strip_prefix(ancestor)
        .map_err(|_| anyhow::anyhow!("invalid worktree path '{}'", path.display()))?;

    for component in remainder.components() {
        match component {
            std::path::Component::Normal(part) => resolved.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::Prefix(_)
            | std::path::Component::RootDir => {
                return Err(anyhow::anyhow!("unsafe worktree path '{}'", path.display()));
            }
        }
    }

    Ok(resolved)
}

fn validate_worktree_target(repo: &Repository, path: &Path) -> Result<()> {
    if path == repo.heddle_dir() || path.starts_with(repo.heddle_dir()) {
        return Err(anyhow::anyhow!(
            "worktree target '{}' cannot point into .heddle storage",
            path.display()
        ));
    }

    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(anyhow::anyhow!(
                "worktree target '{}' cannot be a symlink",
                path.display()
            ));
        }

        if !metadata.is_dir() {
            return Err(anyhow::anyhow!(
                "worktree target '{}' must be a directory",
                path.display()
            ));
        }

        if std::fs::read_dir(path)?.next().transpose()?.is_some() {
            return Err(anyhow::anyhow!(
                "worktree target '{}' is not empty.\n\
                 Use an empty path, capture current work with `heddle capture`, or let Heddle pick a managed heavy checkout with `heddle start --workspace heavy <name>`.",
                path.display()
            ));
        }
    }

    Ok(())
}

pub(crate) fn write_isolated_checkout(
    repo: &Repository,
    abs_path: &Path,
    base_state: &ChangeId,
    thread: Option<&str>,
) -> Result<()> {
    let heddle_dir = abs_path.join(".heddle");
    if heddle_dir.exists() {
        return Err(anyhow::anyhow!(
            "'{}' already has a .heddle directory",
            abs_path.display()
        ));
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