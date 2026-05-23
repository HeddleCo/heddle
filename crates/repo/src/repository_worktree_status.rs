// SPDX-License-Identifier: Apache-2.0
//! Cached worktree comparison helpers.

use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use objects::{object::Tree, worktree::WorktreeStatus};

use super::{
    status_tracked_refresh::refresh_tracked_paths, status_untracked_scan::scan_untracked_paths,
};
use crate::{
    Repository, Result, WorktreeIndex, fsmonitor::ChangeMonitorSession,
    worktree_ignore::WorktreeIgnoreMatcher,
};

#[derive(Debug, Default)]
pub(crate) struct WorktreeCompareStats {
    pub(crate) directories_scanned: u64,
    pub(crate) directories_skipped: u64,
    pub(crate) files_hashed: u64,
    pub(crate) cache_hits: u64,
    pub(crate) monitor_changed_paths: u64,
    pub(crate) monitor_skipped_directories: u64,
    pub(crate) tracked_refresh_ms: u128,
    pub(crate) untracked_scan_ms: u128,
    pub(crate) hashing_ms: u128,
    pub(crate) directory_cache_compare_ms: u128,
}

#[derive(Debug, Clone, Default)]
pub struct UntrackedSubtree {
    pub root: PathBuf,
    pub relative_files: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct UntrackedSet {
    pub files: Vec<PathBuf>,
    pub subtrees: Vec<UntrackedSubtree>,
}

impl UntrackedSet {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.subtrees.is_empty()
    }

    pub fn flattened_path_count(&self) -> usize {
        self.files.len()
            + self
                .subtrees
                .iter()
                .map(|subtree| subtree.relative_files.len())
                .sum::<usize>()
    }

    pub fn flatten_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::with_capacity(self.flattened_path_count());
        paths.extend(self.files.iter().cloned());
        for subtree in &self.subtrees {
            for relative_file in &subtree.relative_files {
                paths.push(join_relative_path(&subtree.root, relative_file));
            }
        }
        paths
    }

    pub fn removal_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::with_capacity(self.files.len() + self.subtrees.len());
        roots.extend(self.files.iter().cloned());
        roots.extend(self.subtrees.iter().map(|subtree| subtree.root.clone()));
        roots
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorktreeStatusDetailed {
    pub modified: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    pub untracked: UntrackedSet,
}

impl WorktreeStatusDetailed {
    pub fn is_clean(&self) -> bool {
        self.modified.is_empty() && self.deleted.is_empty() && self.untracked.is_empty()
    }

    pub fn change_count(&self) -> usize {
        self.modified.len() + self.deleted.len() + self.untracked.flattened_path_count()
    }

    pub fn into_flat_status(self) -> WorktreeStatus {
        WorktreeStatus {
            modified: self.modified,
            added: self.untracked.flatten_paths(),
            deleted: self.deleted,
        }
    }
}

pub(crate) fn compare_worktree_with_index_detailed(
    repo: &Repository,
    tree: &Tree,
    ignore_matcher: &WorktreeIgnoreMatcher,
    index: &mut WorktreeIndex,
    monitor: &ChangeMonitorSession,
) -> Result<(WorktreeStatusDetailed, WorktreeCompareStats)> {
    let mut detailed = WorktreeStatusDetailed::default();
    let mut stats = WorktreeCompareStats {
        monitor_changed_paths: monitor.changed_path_count(),
        ..WorktreeCompareStats::default()
    };

    let tracked_refresh_start = Instant::now();
    let tracked_dirty_directories =
        refresh_tracked_paths(repo, tree, index, monitor, &mut detailed, &mut stats)?;
    stats.tracked_refresh_ms = tracked_refresh_start.elapsed().as_millis();

    let untracked_scan_start = Instant::now();
    scan_untracked_paths(
        repo,
        tree,
        ignore_matcher,
        &tracked_dirty_directories,
        index,
        &mut detailed.untracked,
        &mut stats,
    )?;
    stats.untracked_scan_ms = untracked_scan_start.elapsed().as_millis();

    Ok((detailed, stats))
}

fn join_relative_path(parent: &Path, name: &str) -> PathBuf {
    if parent.as_os_str().is_empty() {
        PathBuf::from(name)
    } else {
        parent.join(name)
    }
}
