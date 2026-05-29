// SPDX-License-Identifier: Apache-2.0
//! Shared worktree apply planning and execution.

use objects::store::ObjectStore;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use objects::{
    fs_atomic::{enrich_fs_error, is_directory_not_empty as fs_is_directory_not_empty},
    object::{EntryType, Tree, TreeEntry},
    worktree::should_ignore as should_ignore_path,
};
use tracing::{debug, warn};

use super::{
    HeddleError, Repository, Result,
    repository_materialization::{MaterializedTree, WorktreeWriteOp},
};
use crate::{
    FsMonitorSettings, WorktreeIndex,
    fsmonitor::persist_current_monitor_cursor,
    worktree_index::{DirectoryCacheEntry, WorktreeIndexLoadStats, WorktreeIndexSaveStats},
    worktree_walk::{build_cached_entry, cache_key},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorktreeApplyStrategy {
    Incremental,
    FullRematerialize,
}

impl WorktreeApplyStrategy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Incremental => "incremental",
            Self::FullRematerialize => "full_rematerialize",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorktreeApplyFallbackReason {
    MissingCurrentTree,
    NonRootDirectory,
    DirtyWorktree,
}

impl WorktreeApplyFallbackReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::MissingCurrentTree => "missing_current_tree",
            Self::NonRootDirectory => "non_root_directory",
            Self::DirtyWorktree => "dirty_worktree",
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct WorktreeApplyStats {
    pub(crate) unchanged_count: usize,
    pub(crate) changed_count: usize,
}

#[derive(Debug)]
pub(crate) struct WorktreeApplyPlan {
    pub(crate) strategy: WorktreeApplyStrategy,
    pub(crate) removals: Vec<PathBuf>,
    pub(crate) directories: Vec<PathBuf>,
    pub(crate) writes: Vec<WorktreeWriteOp>,
    pub(crate) fallback_reason: Option<WorktreeApplyFallbackReason>,
    pub(crate) stats: WorktreeApplyStats,
}

#[derive(Debug, Default)]
pub(crate) struct WorktreeApplyReport {
    pub(crate) delete_phase_ms: u128,
    pub(crate) mkdir_phase_ms: u128,
    pub(crate) write_phase_ms: u128,
    pub(crate) index_update_ms: u128,
    pub(crate) index_snapshot_load_ms: u128,
    pub(crate) index_journal_replay_ms: u128,
    pub(crate) index_snapshot_write_ms: u128,
    pub(crate) index_journal_append_ms: u128,
    pub(crate) index_snapshot_bytes: u64,
    pub(crate) index_journal_bytes: u64,
    pub(crate) index_journal_ops: usize,
    pub(crate) index_compacted: bool,
    pub(crate) index_compact_reason: Option<&'static str>,
    pub(crate) fsmonitor_refresh_ms: u128,
    pub(crate) worker_count: usize,
}

impl WorktreeApplyPlan {
    fn incremental() -> Self {
        Self {
            strategy: WorktreeApplyStrategy::Incremental,
            removals: Vec::new(),
            directories: Vec::new(),
            writes: Vec::new(),
            fallback_reason: None,
            stats: WorktreeApplyStats::default(),
        }
    }

    fn fallback(reason: WorktreeApplyFallbackReason) -> Self {
        Self {
            strategy: WorktreeApplyStrategy::FullRematerialize,
            removals: Vec::new(),
            directories: Vec::new(),
            writes: Vec::new(),
            fallback_reason: Some(reason),
            stats: WorktreeApplyStats::default(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.removals.is_empty() && self.directories.is_empty() && self.writes.is_empty()
    }
}

impl Repository {
    pub(crate) fn plan_worktree_apply(
        &self,
        from_tree: Option<&Tree>,
        to_tree: &Tree,
        dir: &Path,
        current_worktree_verified_clean: bool,
    ) -> Result<WorktreeApplyPlan> {
        let plan_start = Instant::now();
        let plan = match from_tree {
            None => WorktreeApplyPlan::fallback(WorktreeApplyFallbackReason::MissingCurrentTree),
            Some(_) if dir != self.root() => {
                WorktreeApplyPlan::fallback(WorktreeApplyFallbackReason::NonRootDirectory)
            }
            Some(from_tree) => {
                // FOOTGUN: when the worktree is dirty we silently fall back to
                // `FullRematerialize`, which calls `clear_worktree` on the way
                // out — wiping any tracked-but-unsnapshotted edits and any
                // untracked files on tracked paths. Callers cannot tell from
                // the return value whether their tree-apply preserved or
                // destroyed uncommitted work.
                //
                // Defense-in-depth lives at the CLI layer: `goto`, `revert`,
                // `undo`, `redo`, `cherry-pick`, and `rebase` all refuse on a
                // dirty worktree (with `--force` to bypass) before reaching
                // here. The remaining direct callers of `goto_internal` are
                // either internal (`fast_forward_attached`, rebase replay,
                // operator continue) or already pass
                // `current_worktree_verified_clean=true`.
                //
                // TODO: surface the strategy choice as an explicit parameter
                // (e.g. `apply_strategy: AllowDirtyFallback | RefuseOnDirty`)
                // so library callers cannot accidentally clobber a dirty
                // worktree. That refactor is out of scope for this change.
                if !current_worktree_verified_clean && !self.worktree_is_clean_cached(from_tree)? {
                    WorktreeApplyPlan::fallback(WorktreeApplyFallbackReason::DirtyWorktree)
                } else {
                    let mut plan = WorktreeApplyPlan::incremental();
                    self.plan_tree_apply_recursive(
                        Path::new(""),
                        Some(from_tree),
                        Some(to_tree),
                        &mut plan,
                    )?;
                    plan
                }
            }
        };

        debug!(
            strategy = plan.strategy.as_str(),
            changed_count = plan.stats.changed_count,
            unchanged_count = plan.stats.unchanged_count,
            fallback_reason = plan
                .fallback_reason
                .map(WorktreeApplyFallbackReason::as_str)
                .unwrap_or("none"),
            plan_duration_ms = plan_start.elapsed().as_millis(),
            "Worktree apply plan ready"
        );

        Ok(plan)
    }

    pub(crate) fn execute_worktree_apply(
        &self,
        plan: &WorktreeApplyPlan,
        tree: &Tree,
        dir: &Path,
    ) -> Result<WorktreeApplyReport> {
        match plan.strategy {
            WorktreeApplyStrategy::Incremental => {
                self.execute_incremental_worktree_apply(plan, tree)
            }
            WorktreeApplyStrategy::FullRematerialize => {
                let delete_start = Instant::now();
                if self.worktree_requires_clear()? {
                    self.clear_worktree()?;
                }
                let delete_phase_ms = delete_start.elapsed().as_millis();

                let write_start = Instant::now();
                let materialized = self.materialize_tree_seeded(tree, dir)?;
                let write_phase_ms = write_start.elapsed().as_millis();

                let index_update_start = Instant::now();
                if let Err(error) = self
                    .refresh_worktree_performance_state_after_full_rematerialize(materialized, dir)
                {
                    self.invalidate_worktree_performance_state()?;
                    return Err(error);
                }
                let index_update_ms = index_update_start.elapsed().as_millis();

                let fsmonitor_refresh_start = Instant::now();
                self.refresh_fsmonitor_after_incremental_apply();
                let fsmonitor_refresh_ms = fsmonitor_refresh_start.elapsed().as_millis();

                Ok(WorktreeApplyReport {
                    delete_phase_ms,
                    mkdir_phase_ms: 0,
                    write_phase_ms,
                    index_update_ms,
                    fsmonitor_refresh_ms,
                    worker_count: 0,
                    ..WorktreeApplyReport::default()
                })
            }
        }
    }

    fn execute_incremental_worktree_apply(
        &self,
        plan: &WorktreeApplyPlan,
        tree: &Tree,
    ) -> Result<WorktreeApplyReport> {
        if plan.is_empty() {
            return Ok(WorktreeApplyReport::default());
        }

        let delete_start = Instant::now();
        for path in &plan.removals {
            remove_existing_path(path)?;
        }
        let delete_phase_ms = delete_start.elapsed().as_millis();

        let mkdir_start = Instant::now();
        for directory in &plan.directories {
            fs::create_dir_all(directory)
                .map_err(|e| HeddleError::Io(enrich_fs_error(directory, "creating", e)))?;
        }
        let mkdir_phase_ms = mkdir_start.elapsed().as_millis();

        let write_start = Instant::now();
        let worker_count = self.materialize_write_ops(&plan.writes)?;
        let write_phase_ms = write_start.elapsed().as_millis();

        let index_update_start = Instant::now();
        let (index_update_ms, index_load_stats, index_save_stats) =
            self.update_worktree_index_after_incremental_apply(plan, tree)?;
        let index_update_ms = index_update_start
            .elapsed()
            .as_millis()
            .max(index_update_ms);

        let fsmonitor_refresh_start = Instant::now();
        self.refresh_fsmonitor_after_incremental_apply();
        let fsmonitor_refresh_ms = fsmonitor_refresh_start.elapsed().as_millis();

        Ok(WorktreeApplyReport {
            delete_phase_ms,
            mkdir_phase_ms,
            write_phase_ms,
            index_update_ms,
            index_snapshot_load_ms: index_load_stats.snapshot_load_ms,
            index_journal_replay_ms: index_load_stats.journal_replay_ms,
            index_snapshot_write_ms: index_save_stats.snapshot_write_ms,
            index_journal_append_ms: index_save_stats.journal_append_ms,
            index_snapshot_bytes: index_save_stats
                .snapshot_bytes
                .max(index_load_stats.snapshot_bytes),
            index_journal_bytes: index_save_stats
                .journal_bytes
                .max(index_load_stats.journal_bytes),
            index_journal_ops: index_save_stats
                .journal_ops
                .max(index_load_stats.journal_ops),
            index_compacted: index_save_stats.compacted,
            index_compact_reason: index_save_stats.compact_reason,
            fsmonitor_refresh_ms,
            worker_count,
        })
    }

    fn plan_tree_apply_recursive(
        &self,
        rel_path: &Path,
        from_tree: Option<&Tree>,
        to_tree: Option<&Tree>,
        plan: &mut WorktreeApplyPlan,
    ) -> Result<()> {
        let from_entries = from_tree.map(Tree::entries).unwrap_or(&[]);
        let to_entries = to_tree.map(Tree::entries).unwrap_or(&[]);
        let mut from_index = 0;
        let mut to_index = 0;

        while from_index < from_entries.len() || to_index < to_entries.len() {
            match (from_entries.get(from_index), to_entries.get(to_index)) {
                (Some(from_entry), Some(to_entry)) => match from_entry.name.cmp(&to_entry.name) {
                    std::cmp::Ordering::Less => {
                        self.plan_remove_entry(&rel_path.join(&from_entry.name), from_entry, plan)?;
                        from_index += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        self.plan_add_entry(&rel_path.join(&to_entry.name), to_entry, plan)?;
                        to_index += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        self.plan_update_entry(
                            &rel_path.join(&from_entry.name),
                            from_entry,
                            to_entry,
                            plan,
                        )?;
                        from_index += 1;
                        to_index += 1;
                    }
                },
                (Some(from_entry), None) => {
                    self.plan_remove_entry(&rel_path.join(&from_entry.name), from_entry, plan)?;
                    from_index += 1;
                }
                (None, Some(to_entry)) => {
                    self.plan_add_entry(&rel_path.join(&to_entry.name), to_entry, plan)?;
                    to_index += 1;
                }
                (None, None) => break,
            }
        }

        Ok(())
    }

    fn plan_add_entry(
        &self,
        rel_path: &Path,
        entry: &TreeEntry,
        plan: &mut WorktreeApplyPlan,
    ) -> Result<()> {
        match entry.entry_type {
            EntryType::Blob => {
                plan.stats.changed_count += 1;
                plan.writes.push(WorktreeWriteOp::Blob {
                    path: self.root().join(rel_path),
                    hash: entry.hash,
                    executable: entry.is_executable(),
                });
            }
            EntryType::Symlink => {
                plan.stats.changed_count += 1;
                plan.writes.push(WorktreeWriteOp::Symlink {
                    path: self.root().join(rel_path),
                    hash: entry.hash,
                });
            }
            EntryType::Tree => {
                plan.directories.push(self.root().join(rel_path));
                let subtree = self
                    .store
                    .get_tree(&entry.hash)?
                    .ok_or_else(|| HeddleError::NotFound(format!("tree {}", entry.hash)))?;
                self.plan_tree_apply_recursive(rel_path, None, Some(&subtree), plan)?;
            }
        }

        Ok(())
    }

    fn plan_remove_entry(
        &self,
        rel_path: &Path,
        entry: &TreeEntry,
        plan: &mut WorktreeApplyPlan,
    ) -> Result<()> {
        match entry.entry_type {
            EntryType::Blob | EntryType::Symlink => {
                plan.stats.changed_count += 1;
                plan.removals.push(self.root().join(rel_path));
            }
            EntryType::Tree => {
                let subtree = self
                    .store
                    .get_tree(&entry.hash)?
                    .ok_or_else(|| HeddleError::NotFound(format!("tree {}", entry.hash)))?;
                self.plan_tree_apply_recursive(rel_path, Some(&subtree), None, plan)?;
                plan.removals.push(self.root().join(rel_path));
            }
        }

        Ok(())
    }

    fn plan_update_entry(
        &self,
        rel_path: &Path,
        from_entry: &TreeEntry,
        to_entry: &TreeEntry,
        plan: &mut WorktreeApplyPlan,
    ) -> Result<()> {
        if from_entry.entry_type == EntryType::Tree && to_entry.entry_type == EntryType::Tree {
            if from_entry.hash == to_entry.hash {
                plan.stats.unchanged_count += 1;
                return Ok(());
            }

            let from_subtree = self
                .store
                .get_tree(&from_entry.hash)?
                .ok_or_else(|| HeddleError::NotFound(format!("tree {}", from_entry.hash)))?;
            let to_subtree = self
                .store
                .get_tree(&to_entry.hash)?
                .ok_or_else(|| HeddleError::NotFound(format!("tree {}", to_entry.hash)))?;
            return self.plan_tree_apply_recursive(
                rel_path,
                Some(&from_subtree),
                Some(&to_subtree),
                plan,
            );
        }

        if from_entry.entry_type == EntryType::Blob && to_entry.entry_type == EntryType::Blob {
            if from_entry.hash == to_entry.hash && from_entry.mode == to_entry.mode {
                plan.stats.unchanged_count += 1;
                return Ok(());
            }

            plan.stats.changed_count += 1;
            plan.writes.push(WorktreeWriteOp::Blob {
                path: self.root().join(rel_path),
                hash: to_entry.hash,
                executable: to_entry.is_executable(),
            });
            return Ok(());
        }

        if from_entry.entry_type == EntryType::Symlink && to_entry.entry_type == EntryType::Symlink
        {
            if from_entry.hash == to_entry.hash {
                plan.stats.unchanged_count += 1;
                return Ok(());
            }

            plan.stats.changed_count += 1;
            plan.removals.push(self.root().join(rel_path));
            plan.writes.push(WorktreeWriteOp::Symlink {
                path: self.root().join(rel_path),
                hash: to_entry.hash,
            });
            return Ok(());
        }

        self.plan_remove_entry(rel_path, from_entry, plan)?;
        self.plan_add_entry(rel_path, to_entry, plan)
    }

    pub(crate) fn clear_worktree(&self) -> Result<()> {
        let patterns = self.ignore_patterns()?;
        let walker = ignore::WalkBuilder::new(&self.root)
            .hidden(false)
            .git_ignore(false)
            .follow_links(false)
            .build();

        let mut to_remove = Vec::new();

        for entry in walker {
            let entry = entry.map_err(|e| HeddleError::Io(std::io::Error::other(e.to_string())))?;
            let path = entry.path();

            if path == self.root {
                continue;
            }

            let rel_path = path.strip_prefix(&self.root).unwrap_or(path);

            if should_ignore_path(rel_path, &patterns) {
                continue;
            }

            to_remove.push(path.to_path_buf());
        }

        to_remove.sort();
        to_remove.reverse();

        for path in to_remove {
            remove_existing_path(&path)?;
        }

        Ok(())
    }

    fn worktree_requires_clear(&self) -> Result<bool> {
        let patterns = self.ignore_patterns()?;
        for entry in fs::read_dir(self.root())? {
            let entry = entry?;
            let path = entry.path();
            let rel_path = path.strip_prefix(self.root()).unwrap_or(&path);
            if should_ignore_path(rel_path, &patterns) {
                continue;
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn invalidate_worktree_performance_state(&self) -> Result<()> {
        self.remove_if_exists(&self.root.join(".heddle/state").join("index.bin"))?;
        self.remove_if_exists(&self.root.join(".heddle/state").join("index.journal"))?;
        self.remove_if_exists(&self.root.join(".heddle/state").join("fsmonitor.toml"))?;
        Ok(())
    }

    fn refresh_worktree_performance_state_after_full_rematerialize(
        &self,
        materialized: MaterializedTree,
        dir: &Path,
    ) -> Result<()> {
        if dir != self.root() {
            return self.invalidate_worktree_performance_state();
        }

        let index_path = self.root.join(".heddle/state").join("index.bin");
        let mut index = WorktreeIndex::new();
        for entry in materialized.file_entries {
            index.insert_seeded(entry.key, entry.entry);
        }
        for directory in materialized.directory_contexts {
            let metadata = fs::symlink_metadata(&directory.path)?;
            if !metadata.is_dir() {
                return Err(HeddleError::Config(format!(
                    "materialized path is not a directory: {}",
                    directory.path.display()
                )));
            }
            if let Some(directory_entry) = DirectoryCacheEntry::from_child_names(
                &metadata,
                directory.child_names.iter().map(String::as_str),
                directory.child_names.len(),
                Some(directory.tree_hash),
            ) {
                index.insert_seeded_directory(directory.key, directory_entry);
            }
        }
        index.save_snapshot_profiled(&index_path).map_err(|error| {
            HeddleError::Config(format!(
                "save worktree index after full rematerialize: {error}"
            ))
        })?;
        Ok(())
    }

    fn update_worktree_index_after_incremental_apply(
        &self,
        plan: &WorktreeApplyPlan,
        tree: &Tree,
    ) -> Result<(u128, WorktreeIndexLoadStats, WorktreeIndexSaveStats)> {
        let index_path = self.root.join(".heddle/state").join("index.bin");
        let load_start = Instant::now();
        let (mut index, load_stats) = match WorktreeIndex::load_profiled(&index_path) {
            Ok(result) => result,
            Err(error) => {
                warn!(path = %index_path.display(), %error, "Ignoring unreadable worktree index during incremental apply");
                (WorktreeIndex::new(), WorktreeIndexLoadStats::default())
            }
        };

        let mut affected_directory_keys = BTreeSet::from([String::new()]);

        for path in &plan.removals {
            let rel_path = path.strip_prefix(self.root()).unwrap_or(path);
            index.remove_path_and_descendants(&cache_key(rel_path));
            extend_ancestor_directory_keys(&mut affected_directory_keys, rel_path.parent());
        }

        for directory in &plan.directories {
            let rel_path = directory.strip_prefix(self.root()).unwrap_or(directory);
            index.remove_path_and_descendants(&cache_key(rel_path));
            extend_ancestor_directory_keys(&mut affected_directory_keys, Some(rel_path));
        }

        for write in &plan.writes {
            let rel_path = write
                .path()
                .strip_prefix(self.root())
                .unwrap_or(write.path());
            let key = cache_key(rel_path);
            index.remove_path_and_descendants(&key);
            if let Ok(metadata) = fs::symlink_metadata(write.path())
                && let Some(cached) = build_cached_entry(
                    write.hash(),
                    &metadata,
                    write.executable(),
                    write.index_kind(),
                )
            {
                index.insert(key, cached);
            }
            extend_ancestor_directory_keys(&mut affected_directory_keys, rel_path.parent());
        }

        let mut tree_lookup = DirectoryTreeHashLookup::new(self, tree);

        for dir_key in affected_directory_keys {
            refresh_directory_index_entry_from_tree(self, &mut index, &dir_key, &mut tree_lookup)?;
        }

        let save_stats = if index.is_dirty() {
            let stats = index.save_profiled(&index_path).map_err(|error| {
                HeddleError::Config(format!(
                    "save worktree index after incremental apply: {error}"
                ))
            })?;
            index.mark_clean();
            stats
        } else {
            WorktreeIndexSaveStats::default()
        };

        Ok((load_start.elapsed().as_millis(), load_stats, save_stats))
    }

    fn refresh_fsmonitor_after_incremental_apply(&self) {
        let settings = FsMonitorSettings::from(self.config.worktree.fsmonitor);
        if let Err(error) = persist_current_monitor_cursor(self.root(), settings) {
            warn!(root = %self.root().display(), %error, "Failed to refresh monitor cursor after incremental apply");
        }
    }

    fn remove_if_exists(&self, path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(HeddleError::Io(enrich_fs_error(path, "removing", error))),
        }
    }

    /// Remove only the heddle-tracked descendants beneath `path`, preserving
    /// any untracked or explicitly ignored siblings.
    ///
    /// This exists so commands that mutate the worktree at the top-level
    /// tree-entry granularity (`merge`, `cherry-pick`, `revert`) can drop a
    /// tracked directory without recursively destroying the user's local
    /// build artifacts, dependencies, or co-located Git state. The shape
    /// matches `remove_existing_path` in this module: tracked content is
    /// removed, then the directory itself is removed *if empty*; if ignored
    /// content keeps it occupied, the dir is left in place. That keeps disk
    /// in lock-step with the new tree (no stale tracked file under the dir)
    /// without nuking work the user expects to survive.
    ///
    /// Ignore-pattern based variant. Uses the *current* `.heddleignore` to
    /// decide which children to preserve. This is unsafe for the
    /// merge/cherry-pick/revert flow when a tracked path is also matched by
    /// a current ignore rule: the file would silently survive on disk after
    /// HEAD advances. Prefer
    /// [`Self::remove_tracked_descendants_with_source`] in those flows so
    /// removal is driven by the source-tree's actual tracked set.
    ///
    /// `path` must be inside the repository root. If it doesn't exist, this
    /// is a no-op. If it's a regular file or symlink, it is removed.
    pub fn remove_tracked_descendants(&self, path: &Path) -> Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(HeddleError::Io(enrich_fs_error(path, "inspecting", error))),
        };

        let file_type = metadata.file_type();
        if file_type.is_symlink() || file_type.is_file() {
            fs::remove_file(path)
                .map_err(|e| HeddleError::Io(enrich_fs_error(path, "removing", e)))?;
            return Ok(());
        }
        if !file_type.is_dir() {
            return Ok(());
        }

        let patterns = self.ignore_patterns()?;
        remove_tracked_descendants_inner_by_ignore(self.root(), path, &patterns)
    }

    /// Tree-driven variant of [`Self::remove_tracked_descendants`].
    ///
    /// Removal is driven by `source_subtree` — the subtree at `path` in the
    /// state we're transitioning AWAY from. Every blob/symlink it lists is
    /// removed; nested directory entries are recursed into using the matching
    /// child subtree. This is intentionally independent of the *current*
    /// ignore rules: a `.heddleignore` (or config-level) rule that newly
    /// matches a previously-tracked path must NOT silently preserve that
    /// path on disk. Doing so would let HEAD advance past a tree where the
    /// path is gone while the worktree still holds the stale content,
    /// hidden from `heddle status` by the same ignore rule. Tracked-tree
    /// membership is the only source of truth here.
    ///
    /// `path` must be inside the repository root. If it doesn't exist, this
    /// is a no-op. If it's a regular file or symlink, it is removed.
    pub fn remove_tracked_descendants_with_source(
        &self,
        path: &Path,
        source_subtree: &Tree,
    ) -> Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(HeddleError::Io(enrich_fs_error(path, "inspecting", error))),
        };

        let file_type = metadata.file_type();
        if file_type.is_symlink() || file_type.is_file() {
            fs::remove_file(path)
                .map_err(|e| HeddleError::Io(enrich_fs_error(path, "removing", e)))?;
            return Ok(());
        }
        if !file_type.is_dir() {
            return Ok(());
        }

        remove_tracked_descendants_inner(self, path, source_subtree)
    }

    /// Look up the subtree at `rel_path` within `root_tree`. Returns `None`
    /// if the path isn't reachable as a `Tree`-typed entry (missing entry,
    /// blob entry, or unresolved hash). Used by
    /// [`Self::remove_tracked_descendants_with_source`] callers to derive
    /// the source subtree from a top-level tree entry.
    pub fn resolve_subtree(&self, root_tree: &Tree, rel_path: &Path) -> Result<Option<Tree>> {
        // Walk component-by-component, owning the current subtree at each
        // step. Each iteration consults the most recently resolved Tree,
        // so the previous one can be dropped — we never need to borrow
        // across iterations.
        let mut components = rel_path.components();
        let first = match components.next() {
            Some(c) => c,
            None => return Ok(Some(root_tree.clone())),
        };
        let mut current = match self.descend_one(root_tree, first)? {
            Some(t) => t,
            None => return Ok(None),
        };
        for component in components {
            current = match self.descend_one(&current, component)? {
                Some(t) => t,
                None => return Ok(None),
            };
        }
        Ok(Some(current))
    }

    fn descend_one(
        &self,
        tree: &Tree,
        component: std::path::Component<'_>,
    ) -> Result<Option<Tree>> {
        let name = match component.as_os_str().to_str() {
            Some(name) => name,
            None => return Ok(None),
        };
        let Some(entry) = tree.entries().iter().find(|e| e.name == name) else {
            return Ok(None);
        };
        if entry.entry_type != EntryType::Tree {
            return Ok(None);
        }
        self.store().get_tree(&entry.hash)
    }
}

fn refresh_directory_index_entry_from_tree(
    repo: &Repository,
    index: &mut WorktreeIndex,
    dir_key: &str,
    tree_lookup: &mut DirectoryTreeHashLookup<'_>,
) -> Result<()> {
    let Some(tree) = tree_lookup.subtree_at_directory(dir_key)? else {
        index.remove_directory(dir_key);
        return Ok(());
    };

    let dir_path = if dir_key.is_empty() {
        repo.root().to_path_buf()
    } else {
        repo.root().join(dir_key)
    };
    let metadata = match fs::symlink_metadata(&dir_path) {
        Ok(metadata) if metadata.is_dir() => metadata,
        Ok(_) | Err(_) => {
            index.remove_directory(dir_key);
            return Ok(());
        }
    };
    if let Some(directory_entry) = DirectoryCacheEntry::from_child_names(
        &metadata,
        tree.entries().iter().map(|entry| entry.name.as_str()),
        tree.entries().len(),
        Some(tree.hash()),
    ) {
        index.insert_directory(dir_key.to_string(), directory_entry);
    } else {
        index.remove_directory(dir_key);
    }
    Ok(())
}

struct DirectoryTreeHashLookup<'repo> {
    repo: &'repo Repository,
    root_tree: &'repo Tree,
    subtrees: BTreeMap<String, Option<Tree>>,
}

impl<'repo> DirectoryTreeHashLookup<'repo> {
    fn new(repo: &'repo Repository, root_tree: &'repo Tree) -> Self {
        Self {
            repo,
            root_tree,
            subtrees: BTreeMap::new(),
        }
    }

    fn subtree_at_directory(&mut self, dir_key: &str) -> Result<Option<&Tree>> {
        if dir_key.is_empty() {
            return Ok(Some(self.root_tree));
        }

        if !self.subtrees.contains_key(dir_key) {
            let subtree = self.load_subtree(dir_key)?;
            self.subtrees.insert(dir_key.to_string(), subtree);
        }

        Ok(self.subtrees.get(dir_key).and_then(Option::as_ref))
    }

    fn load_subtree(&mut self, dir_key: &str) -> Result<Option<Tree>> {
        let Some((parent_key, name)) = split_parent_directory_key(dir_key) else {
            return Ok(None);
        };
        let Some(tree_hash) = ({
            let Some(parent_tree) = self.subtree_at_directory(&parent_key)? else {
                return Ok(None);
            };
            let Some(entry) = parent_tree.get(name) else {
                return Ok(None);
            };
            if entry.entry_type != EntryType::Tree {
                return Ok(None);
            }
            Some(entry.hash)
        }) else {
            return Ok(None);
        };

        self.repo.store().get_tree(&tree_hash)
    }
}

fn split_parent_directory_key(dir_key: &str) -> Option<(String, &str)> {
    if dir_key.is_empty() {
        return None;
    }
    let (parent_key, name) = match dir_key.rfind('/') {
        Some(idx) => (&dir_key[..idx], &dir_key[idx + 1..]),
        None => ("", dir_key),
    };
    if name.is_empty() {
        return None;
    }
    Some((parent_key.to_string(), name))
}

fn extend_ancestor_directory_keys(keys: &mut BTreeSet<String>, rel_path: Option<&Path>) {
    let Some(mut current) = rel_path else {
        keys.insert(String::new());
        return;
    };

    loop {
        keys.insert(cache_key(current));
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => current = parent,
            _ => {
                keys.insert(String::new());
                break;
            }
        }
    }
}

fn remove_existing_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() || file_type.is_file() {
                fs::remove_file(path)
                    .map_err(|e| HeddleError::Io(enrich_fs_error(path, "removing", e)))?;
            } else if file_type.is_dir() {
                match fs::remove_dir(path) {
                    Ok(()) => {}
                    // The directory still holds entries the apply
                    // intentionally preserved — untracked or explicitly
                    // ignored content that the planner skipped over. Leave
                    // the directory in place: its tracked children are
                    // already gone and the local children must survive the apply.
                    // Without this tolerance, an undo over a real-world
                    // worktree with a `.git` or `target` dir aborts mid-
                    // run with `os error 66` after destroying the tracked
                    // files but before HEAD advances, leaving state
                    // diverged from disk.
                    Err(error) if is_directory_not_empty(&error) => {}
                    Err(error) => {
                        return Err(HeddleError::Io(enrich_fs_error(path, "removing", error)));
                    }
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(HeddleError::Io(enrich_fs_error(path, "inspecting", error))),
    }
}

/// Legacy ignore-driven walker — backs the deprecated
/// [`Repository::remove_tracked_descendants`] entrypoint. Walks `dir`
/// recursively, removing every entry whose worktree-relative path is
/// *not* heddle-ignored. New code should use the tree-driven variant
/// (`remove_tracked_descendants_inner`) so that ignore-rule changes
/// can't silently strand previously-tracked content on disk.
fn remove_tracked_descendants_inner_by_ignore(
    root: &Path,
    dir: &Path,
    patterns: &[String],
) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(HeddleError::Io(enrich_fs_error(dir, "reading", error))),
    };

    for entry in entries {
        let entry = entry?;
        let child = entry.path();
        let rel = child.strip_prefix(root).unwrap_or(&child);

        if should_ignore_path(rel, patterns) {
            continue;
        }

        let file_type = entry.file_type()?;
        if file_type.is_symlink() || file_type.is_file() {
            fs::remove_file(&child)
                .map_err(|e| HeddleError::Io(enrich_fs_error(&child, "removing", e)))?;
        } else if file_type.is_dir() {
            remove_tracked_descendants_inner_by_ignore(root, &child, patterns)?;
        }
    }

    match fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        Err(error) if is_directory_not_empty(&error) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(HeddleError::Io(enrich_fs_error(dir, "removing", error))),
    }
}

/// Walk the entries listed in `source_subtree` and remove the matching
/// children of `dir` from disk. Anything not present in the subtree (an
/// untracked or explicitly ignored sibling, OR previously-tracked content
/// that the subtree doesn't list — there is none, by construction) is left
/// untouched. After draining tracked
/// descendants, `dir` itself is removed if it ended up empty; otherwise it
/// is left in place for the same reason `remove_existing_path` tolerates
/// `ENOTEMPTY`.
///
/// Tree-driven removal is the load-bearing invariant: it is independent of
/// the *current* `.heddleignore` patterns, so a newly-added ignore rule
/// matching previously-tracked content cannot silently preserve that
/// content on disk after a merge/cherry-pick/revert that drops it.
///
/// `repo` is used to resolve nested subtrees by hash; `dir` must be a
/// directory and the caller has already verified that.
fn remove_tracked_descendants_inner(
    repo: &Repository,
    dir: &Path,
    source_subtree: &Tree,
) -> Result<()> {
    for entry in source_subtree.entries() {
        let child = dir.join(&entry.name);
        match entry.entry_type {
            EntryType::Blob | EntryType::Symlink => match fs::symlink_metadata(&child) {
                Ok(_) => {
                    fs::remove_file(&child)
                        .map_err(|e| HeddleError::Io(enrich_fs_error(&child, "removing", e)))?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(HeddleError::Io(enrich_fs_error(
                        &child,
                        "inspecting",
                        error,
                    )));
                }
            },
            EntryType::Tree => {
                let metadata = match fs::symlink_metadata(&child) {
                    Ok(m) => m,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(HeddleError::Io(enrich_fs_error(
                            &child,
                            "inspecting",
                            error,
                        )));
                    }
                };
                if !metadata.file_type().is_dir() {
                    // Tree entry but disk holds a file/symlink: remove it
                    // — that file is not in the source-tree's blob set
                    // either, but treating it as tracked content here
                    // keeps disk in lock-step with the new tree.
                    fs::remove_file(&child)
                        .map_err(|e| HeddleError::Io(enrich_fs_error(&child, "removing", e)))?;
                    continue;
                }
                let nested = match repo.store().get_tree(&entry.hash)? {
                    Some(t) => t,
                    None => continue,
                };
                remove_tracked_descendants_inner(repo, &child, &nested)?;
            }
        }
    }

    // Drop the directory itself if its tracked content is gone. Ignored
    // children may still be present, in which case `remove_dir` returns
    // `ENOTEMPTY` and we leave the dir in place — the caller's contract is
    // "tracked content gone", not "directory gone".
    match fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        Err(error) if is_directory_not_empty(&error) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(HeddleError::Io(enrich_fs_error(dir, "removing", error))),
    }
}

/// Detects an `ENOTEMPTY`-equivalent error from `remove_dir`.
///
/// The apply planner only removes tracked descendants. When tracked content is
/// removed, the parent directory may still hold untracked or explicitly ignored
/// siblings; `remove_dir` then errors. Callers tolerate that error
/// by leaving the directory in place — the tracked children are
/// already gone and the ignored ones must survive the apply.
///
/// Shared between `remove_existing_path` (incremental + full apply)
/// and `repository_materialization::remove_materialized_leaf`
/// (symlink-write replacement). Both paths can otherwise abort
/// mid-apply after destructive writes, leaving state diverged from
/// disk.
///
/// Thin re-export over `objects::fs_atomic::is_directory_not_empty` so the
/// canonical predicate (and its raw-OS-code coverage) lives in one place
/// alongside the other fs error predicates the workspace shares.
pub(crate) fn is_directory_not_empty(error: &std::io::Error) -> bool {
    fs_is_directory_not_empty(error)
}

#[cfg(test)]
mod tests {
    use std::{fs, thread, time::Duration};

    use super::*;
    use crate::Repository;

    fn create_repo() -> (tempfile::TempDir, Repository) {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();
        (temp_dir, repo)
    }

    #[test]
    fn goto_same_tree_plan_is_incremental_and_empty() {
        let (temp_dir, repo) = create_repo();
        fs::write(temp_dir.path().join("a.txt"), "version 1").unwrap();
        let state = repo.snapshot(Some("base".to_string()), None).unwrap();
        let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

        let plan = repo
            .plan_worktree_apply(Some(&tree), &tree, temp_dir.path(), true)
            .unwrap();

        assert_eq!(plan.strategy, WorktreeApplyStrategy::Incremental);
        assert!(plan.removals.is_empty());
        assert!(plan.directories.is_empty());
        assert!(plan.writes.is_empty());
    }

    #[test]
    fn goto_small_delta_plan_only_writes_changed_paths() {
        let (temp_dir, repo) = create_repo();
        let keep = temp_dir.path().join("keep.txt");
        let flip = temp_dir.path().join("flip.txt");
        fs::write(&keep, "keep").unwrap();
        fs::write(&flip, "v1").unwrap();
        let state_one = repo.snapshot(Some("one".to_string()), None).unwrap();

        fs::write(&flip, "v2").unwrap();
        let state_two = repo.snapshot(Some("two".to_string()), None).unwrap();

        let tree_one = repo.store().get_tree(&state_one.tree).unwrap().unwrap();
        let tree_two = repo.store().get_tree(&state_two.tree).unwrap().unwrap();

        repo.goto(&state_one.change_id).unwrap();
        let keep_before = fs::metadata(&keep).unwrap().modified().unwrap();

        thread::sleep(Duration::from_millis(20));
        let plan = repo
            .plan_worktree_apply(Some(&tree_one), &tree_two, temp_dir.path(), true)
            .unwrap();
        let report = repo
            .execute_worktree_apply(&plan, &tree_two, temp_dir.path())
            .unwrap();

        assert_eq!(plan.strategy, WorktreeApplyStrategy::Incremental);
        assert_eq!(plan.writes.len(), 1);
        assert!(report.write_phase_ms < 1_000);
        assert_eq!(fs::read_to_string(&flip).unwrap(), "v2");
        assert_eq!(
            fs::metadata(&keep).unwrap().modified().unwrap(),
            keep_before
        );
    }

    #[test]
    fn full_rematerialize_reseeds_worktree_index() {
        let (temp_dir, repo) = create_repo();
        let nested_dir = temp_dir.path().join("src/bin");
        fs::create_dir_all(&nested_dir).unwrap();
        fs::write(temp_dir.path().join("README.md"), "hello\n").unwrap();
        fs::write(nested_dir.join("app.rs"), "fn main() {}\n").unwrap();
        let state = repo.snapshot(Some("seed".to_string()), None).unwrap();
        let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

        repo.clear_worktree().unwrap();

        let plan = WorktreeApplyPlan::fallback(WorktreeApplyFallbackReason::MissingCurrentTree);
        let report = repo
            .execute_worktree_apply(&plan, &tree, temp_dir.path())
            .unwrap();

        let index = WorktreeIndex::load(&temp_dir.path().join(".heddle/state/index.bin")).unwrap();
        assert!(!temp_dir.path().join(".heddle/state/index.journal").exists());

        assert!(report.index_update_ms < 1_000);
        assert!(index.get("README.md").is_some());
        assert!(index.get("src/bin/app.rs").is_some());
        assert!(repo.worktree_is_clean_cached(&tree).unwrap());
    }

    #[test]
    fn directory_tree_hash_lookup_reuses_subtree_hashes() {
        let (temp_dir, repo) = create_repo();
        let nested_dir = temp_dir.path().join("src/bin");
        fs::create_dir_all(&nested_dir).unwrap();
        fs::write(temp_dir.path().join("README.md"), "hello\n").unwrap();
        fs::write(nested_dir.join("app.rs"), "fn main() {}\n").unwrap();
        let state = repo.snapshot(Some("seed".to_string()), None).unwrap();
        let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
        let src_hash = tree.get("src").unwrap().hash;
        let src_tree = repo.store().get_tree(&src_hash).unwrap().unwrap();
        let bin_hash = src_tree.get("bin").unwrap().hash;
        let mut lookup = DirectoryTreeHashLookup::new(&repo, &tree);

        assert_eq!(
            lookup.subtree_at_directory("").unwrap().map(Tree::hash),
            Some(tree.hash())
        );
        assert_eq!(
            lookup.subtree_at_directory("src").unwrap().map(Tree::hash),
            Some(src_hash)
        );
        assert_eq!(
            lookup
                .subtree_at_directory("src/bin")
                .unwrap()
                .map(Tree::hash),
            Some(bin_hash)
        );
        assert!(lookup.subtree_at_directory("missing").unwrap().is_none());
    }

    /// Exercises the end-to-end path: a `fs::remove_dir` against a
    /// non-empty directory must produce a wrapped `HeddleError::Io`
    /// whose Display starts with "could not remove directory" and
    /// names the offending path. This is what the user-facing CLI
    /// stderr ends up showing instead of bare `os error 66`.
    ///
    /// We trip ENOTEMPTY directly through `fs::remove_dir` (the
    /// kernel surface that originally leaked) and confirm the
    /// `enrich_fs_error` wrapping naming the path.
    #[test]
    fn enriched_remove_dir_error_names_path_and_action() {
        use objects::fs_atomic::enrich_fs_error;

        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("not-empty");
        fs::create_dir(&target).unwrap();
        // Drop a file inside so `remove_dir` will surface ENOTEMPTY.
        fs::write(target.join("blocker"), b"x").unwrap();

        let raw_err = fs::remove_dir(&target).unwrap_err();
        // Sanity: this really was the kernel's directory-not-empty
        // signal, otherwise the wrapping below wouldn't be exercising
        // the right path.
        assert!(
            super::is_directory_not_empty(&raw_err),
            "expected ENOTEMPTY from remove_dir on non-empty dir, got {raw_err:?}"
        );

        let wrapped = enrich_fs_error(&target, "removing", raw_err);
        let msg = wrapped.to_string();
        assert!(
            msg.contains("could not remove directory"),
            "missing action verb: {msg}"
        );
        assert!(
            msg.contains(target.to_str().unwrap()),
            "missing path in message: {msg}"
        );
        assert!(
            msg.contains("heddle-ignored"),
            "missing heddle-ignored hint: {msg}"
        );
    }

    /// Regression: when a `.heddleignore` rule (or config rule) matches a
    /// previously-tracked file, the legacy ignore-driven removal silently
    /// preserved that file on disk after a merge/cherry-pick/revert that
    /// dropped it. HEAD then advanced past a tree where the path is gone
    /// while the worktree still held the stale content, hidden from
    /// `heddle status` by the same ignore rule. The tree-driven walker
    /// must remove tracked content regardless of current ignore rules.
    #[test]
    fn tree_driven_removal_strips_tracked_files_matched_by_new_ignore_rule() {
        let (temp_dir, repo) = create_repo();
        // Snapshot a tree that tracks `legacy.txt`.
        fs::write(temp_dir.path().join("legacy.txt"), "tracked content").unwrap();
        let state = repo.snapshot(Some("seed".to_string()), None).unwrap();
        let source_tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

        // After the snapshot, the user (or a hook) drops a `.heddleignore`
        // rule that matches the previously-tracked path. The legacy code
        // would walk the directory, see the path is now ignored, and
        // skip it — leaving stale tracked content behind.
        fs::write(temp_dir.path().join(".heddleignore"), "legacy.txt\n").unwrap();

        // Sanity: `legacy.txt` exists on disk.
        assert!(temp_dir.path().join("legacy.txt").exists());

        // Driving removal off the source tree (which lists `legacy.txt`)
        // must remove it even though the current ignore rules now match.
        repo.remove_tracked_descendants_with_source(temp_dir.path(), &source_tree)
            .unwrap();

        assert!(
            !temp_dir.path().join("legacy.txt").exists(),
            "tree-driven removal must strip tracked files even when current \
             ignore rules match — the source tree is the source of truth"
        );
        // `.heddleignore` is NOT in `source_tree` (it was created after
        // the snapshot), so the walker must leave it alone.
        assert!(
            temp_dir.path().join(".heddleignore").exists(),
            "untracked file outside the source-tree set must survive"
        );
    }

    /// The tree-driven walker must preserve untracked or explicitly ignored
    /// siblings — the original purpose of `remove_tracked_descendants` is "drop
    /// tracked content without nuking local-only build/dependency output".
    /// Tree-driven removal achieves this by *not visiting* paths absent
    /// from the source tree.
    #[test]
    fn tree_driven_removal_preserves_untracked_siblings() {
        let (temp_dir, repo) = create_repo();
        fs::create_dir_all(temp_dir.path().join("pkg")).unwrap();
        fs::write(temp_dir.path().join("pkg/keep.txt"), "tracked").unwrap();
        let state = repo.snapshot(Some("seed".to_string()), None).unwrap();
        let source_tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

        // Now create untracked content under `pkg/` — simulating a
        // `node_modules/` or `target/` directory the user materializes
        // post-snapshot.
        fs::create_dir_all(temp_dir.path().join("pkg/node_modules")).unwrap();
        fs::write(
            temp_dir.path().join("pkg/node_modules/leftover"),
            b"untracked",
        )
        .unwrap();

        // Resolve the `pkg` subtree and ask the walker to drop it.
        let pkg_subtree = repo
            .resolve_subtree(&source_tree, std::path::Path::new("pkg"))
            .unwrap()
            .expect("pkg subtree resolves");
        repo.remove_tracked_descendants_with_source(&temp_dir.path().join("pkg"), &pkg_subtree)
            .unwrap();

        assert!(
            !temp_dir.path().join("pkg/keep.txt").exists(),
            "tracked content must be removed"
        );
        assert!(
            temp_dir.path().join("pkg/node_modules/leftover").exists(),
            "untracked sibling must survive the walk"
        );
        assert!(
            temp_dir.path().join("pkg").exists(),
            "the parent dir survives because untracked content keeps it occupied"
        );
    }
}
