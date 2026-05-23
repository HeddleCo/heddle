// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use objects::object::{ContentHash, EntryType, Tree, TreeEntry};

use crate::{
    Repository, Result, WorktreeIndex,
    fsmonitor::ChangeMonitorSession,
    repository::repository_worktree_status::{WorktreeCompareStats, WorktreeStatusDetailed},
    worktree_index::IndexEntryKind,
    worktree_walk::{build_cached_entry, read_file_hash},
};

struct TrackedRefreshContext<'a> {
    repo: &'a Repository,
    index: &'a mut WorktreeIndex,
    monitor: &'a ChangeMonitorSession,
    status: &'a mut WorktreeStatusDetailed,
    stats: &'a mut WorktreeCompareStats,
    dirty_directories: &'a mut BTreeSet<String>,
}

pub(crate) fn refresh_tracked_paths(
    repo: &Repository,
    tree: &Tree,
    index: &mut WorktreeIndex,
    monitor: &ChangeMonitorSession,
    status: &mut WorktreeStatusDetailed,
    stats: &mut WorktreeCompareStats,
) -> Result<BTreeSet<String>> {
    let mut dirty_directories = BTreeSet::new();
    let mut ctx = TrackedRefreshContext {
        repo,
        index,
        monitor,
        status,
        stats,
        dirty_directories: &mut dirty_directories,
    };
    refresh_tracked_directory(&mut ctx, Path::new(""), "", tree).map(|_| dirty_directories)
}

fn refresh_tracked_directory(
    ctx: &mut TrackedRefreshContext<'_>,
    rel_path: &Path,
    dir_key: &str,
    tree: &Tree,
) -> Result<bool> {
    let dir_path = if dir_key.is_empty() {
        ctx.repo.root().to_path_buf()
    } else {
        ctx.repo.root().join(rel_path)
    };
    let _metadata = match fs::symlink_metadata(&dir_path) {
        Ok(metadata) if metadata.is_dir() => metadata,
        Ok(_) | Err(_) => {
            if !rel_path.as_os_str().is_empty() {
                ctx.status.modified.push(rel_path.to_path_buf());
                mark_ancestor_directories_dirty(rel_path.parent(), ctx.dirty_directories);
            }
            ctx.index.remove_path_and_descendants(dir_key);
            return Ok(false);
        }
    };

    ctx.stats.directories_scanned += 1;
    if ctx
        .monitor
        .can_skip_directory(rel_path, Some(tree), ctx.index)
    {
        ctx.stats.monitor_skipped_directories += 1;
        ctx.stats.directories_skipped += 1;
        return Ok(true);
    }

    // Directory mtimes and child-name lists do not change when an existing file's
    // contents are edited in place, so they are not sufficient to skip tracked
    // subtree refresh safely. Only fsmonitor-backed skip decisions are sound here.

    let mut subtree_clean = true;
    for entry in tree.entries() {
        let child_rel_path = join_relative_path(rel_path, &entry.name);
        let child_key = child_key(dir_key, &entry.name);
        let child_clean = match entry.entry_type {
            EntryType::Blob => refresh_tracked_file(ctx, &child_rel_path, &child_key, entry)?,
            EntryType::Symlink => refresh_tracked_symlink(ctx, &child_rel_path, &child_key, entry)?,
            EntryType::Tree => {
                let subtree = ctx.repo.store().get_tree(&entry.hash)?.ok_or_else(|| {
                    objects::error::HeddleError::NotFound(format!("tree {}", entry.hash))
                })?;
                refresh_tracked_directory(ctx, &child_rel_path, &child_key, &subtree)?
            }
        };
        subtree_clean &= child_clean;
    }
    Ok(subtree_clean)
}

fn refresh_tracked_file(
    ctx: &mut TrackedRefreshContext<'_>,
    rel_path: &Path,
    key: &str,
    tree_entry: &TreeEntry,
) -> Result<bool> {
    let path = ctx.repo.root().join(rel_path);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(_) => {
            ctx.index.remove_path_and_descendants(key);
            ctx.status.deleted.push(rel_path.to_path_buf());
            mark_ancestor_directories_dirty(rel_path.parent(), ctx.dirty_directories);
            return Ok(false);
        }
    };

    if !metadata.is_file() {
        ctx.index.remove_path_and_descendants(key);
        ctx.status.modified.push(rel_path.to_path_buf());
        mark_ancestor_directories_dirty(rel_path.parent(), ctx.dirty_directories);
        return Ok(false);
    }

    let hash = if ctx.index.is_fresh(key, &metadata) {
        ctx.stats.cache_hits += 1;
        ctx.index.get(key).map(|cached| cached.hash).map_or_else(
            || {
                compute_and_cache_file(
                    &path,
                    key,
                    &metadata,
                    tree_entry.is_executable(),
                    ctx.index,
                    ctx.stats,
                )
            },
            Ok,
        )?
    } else {
        compute_and_cache_file(
            &path,
            key,
            &metadata,
            tree_entry.is_executable(),
            ctx.index,
            ctx.stats,
        )?
    };

    if tree_entry.hash == hash && tree_entry.is_executable() == is_executable(&metadata) {
        Ok(true)
    } else {
        ctx.status.modified.push(rel_path.to_path_buf());
        mark_ancestor_directories_dirty(rel_path.parent(), ctx.dirty_directories);
        Ok(false)
    }
}

fn refresh_tracked_symlink(
    ctx: &mut TrackedRefreshContext<'_>,
    rel_path: &Path,
    key: &str,
    tree_entry: &TreeEntry,
) -> Result<bool> {
    let path = ctx.repo.root().join(rel_path);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(_) => {
            ctx.index.remove_path_and_descendants(key);
            ctx.status.deleted.push(rel_path.to_path_buf());
            mark_ancestor_directories_dirty(rel_path.parent(), ctx.dirty_directories);
            return Ok(false);
        }
    };

    if !metadata.file_type().is_symlink() {
        ctx.index.remove_path_and_descendants(key);
        ctx.status.modified.push(rel_path.to_path_buf());
        mark_ancestor_directories_dirty(rel_path.parent(), ctx.dirty_directories);
        return Ok(false);
    }

    let hash = if ctx.index.is_fresh(key, &metadata) {
        ctx.stats.cache_hits += 1;
        ctx.index.get(key).map(|cached| cached.hash).map_or_else(
            || compute_and_cache_symlink(&path, key, &metadata, ctx.index),
            Ok,
        )?
    } else {
        compute_and_cache_symlink(&path, key, &metadata, ctx.index)?
    };

    if tree_entry.hash == hash {
        Ok(true)
    } else {
        ctx.status.modified.push(rel_path.to_path_buf());
        mark_ancestor_directories_dirty(rel_path.parent(), ctx.dirty_directories);
        Ok(false)
    }
}

fn compute_and_cache_file(
    path: &Path,
    key: &str,
    metadata: &fs::Metadata,
    executable: bool,
    index: &mut WorktreeIndex,
    stats: &mut WorktreeCompareStats,
) -> Result<ContentHash> {
    stats.files_hashed += 1;
    let hash_start = Instant::now();
    let hash = read_file_hash(path, metadata.len())?;
    stats.hashing_ms += hash_start.elapsed().as_millis();
    if let Some(cached) = build_cached_entry(hash, metadata, executable, IndexEntryKind::File) {
        index.insert(key.to_string(), cached);
    }
    Ok(hash)
}

fn compute_and_cache_symlink(
    path: &Path,
    key: &str,
    metadata: &fs::Metadata,
    index: &mut WorktreeIndex,
) -> Result<ContentHash> {
    let target = fs::read_link(path)?;
    let hash = ContentHash::compute_typed("blob", target.to_string_lossy().as_bytes());
    if let Some(cached) = build_cached_entry(hash, metadata, false, IndexEntryKind::Symlink) {
        index.insert(key.to_string(), cached);
    }
    Ok(hash)
}

fn child_key(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn join_relative_path(parent: &Path, name: &str) -> PathBuf {
    if parent.as_os_str().is_empty() {
        PathBuf::from(name)
    } else {
        parent.join(name)
    }
}

fn is_executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

fn mark_ancestor_directories_dirty(
    rel_path: Option<&Path>,
    dirty_directories: &mut BTreeSet<String>,
) {
    let Some(mut current) = rel_path else {
        dirty_directories.insert(String::new());
        return;
    };

    loop {
        dirty_directories.insert(current.to_string_lossy().replace('\\', "/"));
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => current = parent,
            _ => {
                dirty_directories.insert(String::new());
                break;
            }
        }
    }
}
