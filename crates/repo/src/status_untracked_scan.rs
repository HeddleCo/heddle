// SPDX-License-Identifier: Apache-2.0
use objects::store::ObjectStore;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use objects::object::{EntryType, Tree};

use crate::{
    Repository, Result, WorktreeIndex,
    repository::repository_worktree_status::{
        UntrackedSet, UntrackedSubtree, WorktreeCompareStats,
    },
    worktree_ignore::WorktreeIgnoreMatcher,
    worktree_index::UntrackedDirectoryCacheEntry,
    worktree_walk::{cache_key, list_directory},
};

struct UntrackedScanContext<'a> {
    repo: &'a Repository,
    ignore_matcher: &'a WorktreeIgnoreMatcher,
    tracked_dirty_directories: &'a BTreeSet<String>,
    index: &'a mut WorktreeIndex,
    untracked: &'a mut UntrackedSet,
    stats: &'a mut WorktreeCompareStats,
}

pub(crate) fn scan_untracked_paths(
    repo: &Repository,
    tree: &Tree,
    ignore_matcher: &WorktreeIgnoreMatcher,
    tracked_dirty_directories: &BTreeSet<String>,
    index: &mut WorktreeIndex,
    untracked: &mut UntrackedSet,
    stats: &mut WorktreeCompareStats,
) -> Result<()> {
    let mut ctx = UntrackedScanContext {
        repo,
        ignore_matcher,
        tracked_dirty_directories,
        index,
        untracked,
        stats,
    };
    scan_directory(&mut ctx, Path::new(""), repo.root(), "", Some(tree)).map(|_| ())
}

fn scan_directory(
    ctx: &mut UntrackedScanContext<'_>,
    rel_path: &Path,
    dir: &Path,
    dir_key: &str,
    tree: Option<&Tree>,
) -> Result<bool> {
    if tree.is_none() {
        let relative_files = scan_pure_untracked_subtree(ctx, rel_path, dir, dir_key)?;
        push_untracked_subtree(rel_path, relative_files, ctx.untracked);
        return Ok(true);
    }

    ctx.index.remove_untracked_directory(dir_key);

    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.is_dir() => metadata,
        Ok(_) | Err(_) => {
            ctx.index.remove_directory(dir_key);
            return Ok(false);
        }
    };
    let entries = list_directory(dir, false)?;
    ctx.stats.directories_scanned += 1;

    let tree = tree.expect("tracked-tree scan requires a tree");
    let cache_compare_start = Instant::now();
    if let Some(cached) = ctx.index.get_directory(dir_key)
        && !ctx.tracked_dirty_directories.contains(dir_key)
        && cached.clean_tree_hash.as_ref() == Some(&tree.hash())
        && cached.child_names_match(
            entries.iter().map(|entry| entry.name.as_str()),
            entries.len(),
        )
    {
        ctx.stats.directory_cache_compare_ms += cache_compare_start.elapsed().as_millis();
        ctx.stats.directories_skipped += 1;
        ctx.stats.cache_hits += 1;
        return Ok(false);
    }
    ctx.stats.directory_cache_compare_ms += cache_compare_start.elapsed().as_millis();

    let tree_entries = tree.entries();
    let mut next_tree_entry = 0usize;
    let mut subtree_has_untracked = false;

    for entry in &entries {
        if ctx
            .ignore_matcher
            .should_prune_directory_child(rel_path, &entry.name)
        {
            continue;
        }
        if ctx.ignore_matcher.should_prune_absolute_path(&entry.path) {
            continue;
        }

        while next_tree_entry < tree_entries.len()
            && tree_entries[next_tree_entry].name.as_str() < entry.name.as_str()
        {
            next_tree_entry += 1;
        }

        let tree_entry = tree_entries
            .get(next_tree_entry)
            .filter(|tree_entry| tree_entry.name == entry.name);
        if tree_entry.is_some() {
            next_tree_entry += 1;
        }

        let child_rel_path = join_relative_path(rel_path, &entry.name);
        let child_key = cache_key(&child_rel_path);

        match entry.kind {
            crate::worktree_walk::ListedDirEntryKind::Directory => {
                ctx.index.remove(&child_key);
                let child_tree = match tree_entry {
                    Some(tree_entry) if tree_entry.entry_type == EntryType::Tree => Some(
                        ctx.repo
                            .store()
                            .get_tree(&tree_entry.hash)?
                            .ok_or_else(|| {
                                objects::error::HeddleError::NotFound(format!(
                                    "tree {}",
                                    tree_entry.hash
                                ))
                            })?,
                    ),
                    _ => None,
                };
                if child_tree.is_some() {
                    ctx.index.remove_untracked_directory_descendants(&child_key);
                }
                if child_tree.is_none() {
                    ctx.index.remove_path_and_descendants(&child_key);
                    subtree_has_untracked = true;
                }
                subtree_has_untracked |= scan_directory(
                    ctx,
                    &child_rel_path,
                    &entry.path,
                    &child_key,
                    child_tree.as_ref(),
                )?;
            }
            crate::worktree_walk::ListedDirEntryKind::File { .. }
            | crate::worktree_walk::ListedDirEntryKind::Symlink => {
                if tree_entry.is_none() {
                    ctx.index.remove_path_and_descendants(&child_key);
                    ctx.untracked.files.push(child_rel_path);
                    subtree_has_untracked = true;
                }
            }
            crate::worktree_walk::ListedDirEntryKind::Other => {}
        }
    }

    let clean_tree_hash =
        if !subtree_has_untracked && !ctx.tracked_dirty_directories.contains(dir_key) {
            Some(tree.hash())
        } else {
            None
        };

    if let Some(directory_entry) = crate::DirectoryCacheEntry::from_child_names(
        &metadata,
        entries.iter().map(|entry| entry.name.as_str()),
        entries.len(),
        clean_tree_hash,
    ) {
        ctx.index
            .insert_directory(dir_key.to_string(), directory_entry);
    } else {
        ctx.index.remove_directory(dir_key);
    }

    Ok(subtree_has_untracked)
}

fn scan_pure_untracked_subtree(
    ctx: &mut UntrackedScanContext<'_>,
    rel_path: &Path,
    dir: &Path,
    dir_key: &str,
) -> Result<Vec<String>> {
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.is_dir() => metadata,
        Ok(_) | Err(_) => {
            ctx.index.remove_directory(dir_key);
            ctx.index.remove_untracked_directory(dir_key);
            return Ok(Vec::new());
        }
    };
    ctx.index.remove_directory(dir_key);
    let entries = list_directory(dir, false)?;
    ctx.stats.directories_scanned += 1;

    if let Some(existing_summary) = ctx.index.get_untracked_directory(dir_key)
        && existing_summary.matches_current_directory(
            &metadata,
            entries.iter().map(|entry| entry.name.as_str()),
            entries.len(),
            ctx.ignore_matcher.fingerprint(),
        )
    {
        ctx.stats.directories_skipped += 1;
        ctx.stats.cache_hits += 1;
        return Ok(existing_summary.added_paths.clone());
    }

    ctx.index.remove_untracked_directory_descendants(dir_key);

    let mut added_paths = Vec::new();
    for entry in &entries {
        if ctx
            .ignore_matcher
            .should_prune_directory_child(rel_path, &entry.name)
        {
            continue;
        }
        if ctx.ignore_matcher.should_prune_absolute_path(&entry.path) {
            continue;
        }

        let child_rel_path = join_relative_path(rel_path, &entry.name);
        match entry.kind {
            crate::worktree_walk::ListedDirEntryKind::Directory => {
                let child_added_paths = scan_pure_untracked_subtree(
                    ctx,
                    &child_rel_path,
                    &entry.path,
                    &cache_key(&child_rel_path),
                )?;
                added_paths.extend(
                    child_added_paths
                        .into_iter()
                        .map(|path| join_relative_string(&entry.name, &path)),
                );
            }
            crate::worktree_walk::ListedDirEntryKind::File { .. }
            | crate::worktree_walk::ListedDirEntryKind::Symlink => {
                added_paths.push(entry.name.clone());
            }
            crate::worktree_walk::ListedDirEntryKind::Other => {}
        }
    }

    if let Some(summary) = UntrackedDirectoryCacheEntry::from_relative_added_paths(
        &metadata,
        entries.iter().map(|entry| entry.name.as_str()),
        entries.len(),
        ctx.ignore_matcher.fingerprint(),
        added_paths.clone(),
    ) {
        ctx.index
            .insert_untracked_directory(dir_key.to_string(), summary);
    } else {
        ctx.index.remove_untracked_directory(dir_key);
    }

    Ok(added_paths)
}

fn join_relative_path(parent: &Path, name: &str) -> PathBuf {
    if parent.as_os_str().is_empty() {
        PathBuf::from(name)
    } else {
        parent.join(name)
    }
}

fn push_untracked_subtree(root: &Path, relative_files: Vec<String>, untracked: &mut UntrackedSet) {
    if relative_files.is_empty() {
        return;
    }

    if root.as_os_str().is_empty() {
        untracked
            .files
            .extend(relative_files.into_iter().map(PathBuf::from));
    } else {
        untracked.subtrees.push(UntrackedSubtree {
            root: root.to_path_buf(),
            relative_files,
        });
    }
}

fn join_relative_string(parent: &str, child: &str) -> String {
    if child.is_empty() {
        parent.to_string()
    } else {
        format!("{parent}/{child}")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::join_relative_string;
    use crate::{
        worktree_ignore::WorktreeIgnoreMatcher, worktree_index::UntrackedDirectoryCacheEntry,
        worktree_walk::list_directory,
    };

    #[test]
    fn pure_untracked_summary_rejects_child_list_changes() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path().join("untracked");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("a.txt"), "a").unwrap();
        fs::write(dir.join("b.txt"), "b").unwrap();

        let matcher = WorktreeIgnoreMatcher::new(&[]);
        let metadata = fs::symlink_metadata(&dir).unwrap();
        let entries = list_directory(&dir, false).unwrap();
        let summary = UntrackedDirectoryCacheEntry::from_relative_added_paths(
            &metadata,
            entries.iter().map(|entry| entry.name.as_str()),
            entries.len(),
            matcher.fingerprint(),
            vec!["a.txt".to_string(), "b.txt".to_string()],
        )
        .unwrap();

        fs::write(dir.join("c.txt"), "c").unwrap();
        let updated_metadata = fs::symlink_metadata(&dir).unwrap();
        let updated_entries = list_directory(&dir, false).unwrap();

        assert!(!summary.matches_current_directory(
            &updated_metadata,
            updated_entries.iter().map(|entry| entry.name.as_str()),
            updated_entries.len(),
            matcher.fingerprint()
        ));
    }

    #[test]
    fn pure_untracked_summary_rejects_ignore_rule_changes() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path().join("untracked");
        fs::create_dir(&dir).unwrap();
        fs::create_dir(dir.join("build")).unwrap();
        fs::write(dir.join("visible.txt"), "visible").unwrap();

        let metadata = fs::symlink_metadata(&dir).unwrap();
        let entries = list_directory(&dir, false).unwrap();
        let cached_matcher = WorktreeIgnoreMatcher::new(&["build/".to_string()]);
        let summary = UntrackedDirectoryCacheEntry::from_relative_added_paths(
            &metadata,
            entries.iter().map(|entry| entry.name.as_str()),
            entries.len(),
            cached_matcher.fingerprint(),
            vec!["visible.txt".to_string()],
        )
        .unwrap();

        let current_matcher = WorktreeIgnoreMatcher::new(&[]);
        assert!(!summary.matches_current_directory(
            &metadata,
            entries.iter().map(|entry| entry.name.as_str()),
            entries.len(),
            current_matcher.fingerprint()
        ));
    }

    #[test]
    fn join_relative_string_builds_nested_relative_paths() {
        assert_eq!(
            join_relative_string("dir", "nested/file.txt"),
            "dir/nested/file.txt"
        );
    }
}
