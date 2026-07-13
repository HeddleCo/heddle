// SPDX-License-Identifier: Apache-2.0
//! Worktree movement operations.

use std::time::Instant;

use objects::{lock::RepositoryLockExt, object::StateId, store::ObjectStore};
use oplog::OpLogRecorder;
use refs::Head;
use tracing::debug;

use super::{
    HeddleError, Repository, Result,
    repository_worktree_apply::{
        WorktreeApplyDirtyBehavior, WorktreeApplyPlan, WorktreeApplyReport, WorktreeApplyStats,
        WorktreeApplyStrategy,
    },
};
use crate::{thread_model::ThreadFreshness, thread_storage::ThreadManager};

#[derive(Debug, Clone, Copy)]
enum WorktreeBaseline {
    Head,
    Materialized(Option<StateId>),
}

impl Repository {
    /// Move worktree to a different state.
    pub fn goto(&self, target: &StateId) -> Result<()> {
        self.goto_internal(
            target,
            true,
            false,
            WorktreeApplyDirtyBehavior::RefuseOnDirty,
            WorktreeBaseline::Head,
        )
    }

    /// Move worktree to a different state, discarding unsnapped local edits.
    pub fn goto_discard_local(&self, target: &StateId) -> Result<()> {
        self.goto_internal(
            target,
            true,
            false,
            WorktreeApplyDirtyBehavior::DiscardLocalChanges,
            WorktreeBaseline::Head,
        )
    }

    /// Move worktree to `target` using the state that the worktree currently
    /// represents as the dirty-check and incremental-apply baseline.
    ///
    /// This is for callers that must publish or import refs before they can
    /// materialize the checkout. In those flows HEAD may already resolve to
    /// `target`, even though the files on disk still represent `materialized`.
    pub fn goto_from_materialized_state(
        &self,
        target: &StateId,
        materialized: Option<&StateId>,
    ) -> Result<()> {
        self.goto_internal(
            target,
            true,
            false,
            WorktreeApplyDirtyBehavior::RefuseOnDirty,
            WorktreeBaseline::Materialized(materialized.copied()),
        )
    }

    /// Fast-forward the current checkout to `target`.
    ///
    /// If HEAD was attached to a thread, advance that thread's ref so the
    /// thread now points at `target` and re-attach HEAD. If HEAD was detached,
    /// advance to `target` while remaining detached.
    ///
    /// Use this anywhere you'd previously call [`Repository::goto`] from a
    /// context where HEAD was potentially attached (merge/rebase fast-forward,
    /// pull/fetch, etc). The low-level `goto` unconditionally writes
    /// `Head::Detached`, which silently strands the attached thread at its
    /// pre-op state — this helper preserves attached-HEAD semantics so the
    /// thread's ref and metadata advance with the worktree.
    pub fn fast_forward_attached(&self, target: &StateId) -> Result<()> {
        self.fast_forward_attached_internal(
            target,
            true,
            WorktreeApplyDirtyBehavior::RefuseOnDirty,
            WorktreeBaseline::Head,
        )
    }

    pub fn fast_forward_attached_discard_local(&self, target: &StateId) -> Result<()> {
        self.fast_forward_attached_internal(
            target,
            true,
            WorktreeApplyDirtyBehavior::DiscardLocalChanges,
            WorktreeBaseline::Head,
        )
    }

    pub fn fast_forward_attached_from_materialized_state(
        &self,
        target: &StateId,
        materialized: Option<&StateId>,
    ) -> Result<()> {
        self.fast_forward_attached_internal(
            target,
            true,
            WorktreeApplyDirtyBehavior::RefuseOnDirty,
            WorktreeBaseline::Materialized(materialized.copied()),
        )
    }

    /// Variant of [`Self::fast_forward_attached`] that performs the
    /// fast-forward without recording an `OpRecord::Goto`. The merge
    /// command uses this so it can record an `OpRecord::FastForward`
    /// instead — the FF-specific variant carries both `pre_target_id`
    /// (for undo) and `post_target_id` (for deterministic redo). The
    /// generic `Goto` inverse only rewinds HEAD, which stranded the
    /// merged-into thread ref (heddle#99 r1); a name-resolved redo was
    /// also non-deterministic if the source thread moved (heddle#99 r2).
    pub fn fast_forward_attached_without_record(&self, target: &StateId) -> Result<()> {
        self.fast_forward_attached_internal(
            target,
            false,
            WorktreeApplyDirtyBehavior::RefuseOnDirty,
            WorktreeBaseline::Head,
        )
    }

    pub fn fast_forward_attached_without_record_discard_local(
        &self,
        target: &StateId,
    ) -> Result<()> {
        self.fast_forward_attached_internal(
            target,
            false,
            WorktreeApplyDirtyBehavior::DiscardLocalChanges,
            WorktreeBaseline::Head,
        )
    }

    pub fn fast_forward_attached_from_materialized_state_without_record(
        &self,
        target: &StateId,
        materialized: Option<&StateId>,
    ) -> Result<()> {
        self.fast_forward_attached_internal(
            target,
            false,
            WorktreeApplyDirtyBehavior::RefuseOnDirty,
            WorktreeBaseline::Materialized(materialized.copied()),
        )
    }

    fn fast_forward_attached_internal(
        &self,
        target: &StateId,
        record: bool,
        dirty_behavior: WorktreeApplyDirtyBehavior,
        baseline: WorktreeBaseline,
    ) -> Result<()> {
        let head_before = self.refs.read_head()?;
        if record {
            self.goto_internal(target, true, false, dirty_behavior, baseline)?;
        } else {
            self.goto_internal(target, false, false, dirty_behavior, baseline)?;
        }
        if let Head::Attached {
            thread: current_thread,
        } = &head_before
        {
            self.refs.set_thread(current_thread, target)?;
            self.refs.write_head(&Head::Attached {
                thread: current_thread.clone(),
            })?;
            let thread_manager = ThreadManager::new(self.heddle_dir());
            if let Some(mut current_meta) = thread_manager.find_by_thread(current_thread)? {
                current_meta.current_state = Some(target.short());
                current_meta.updated_at = chrono::Utc::now();
                current_meta.freshness = ThreadFreshness::Current;
                thread_manager.save(&current_meta)?;
            }
        }
        Ok(())
    }

    pub fn goto_verified_clean(&self, target: &StateId) -> Result<()> {
        self.goto_internal(
            target,
            true,
            true,
            WorktreeApplyDirtyBehavior::RefuseOnDirty,
            WorktreeBaseline::Head,
        )
    }

    pub fn goto_verified_clean_without_record(&self, target: &StateId) -> Result<()> {
        self.goto_internal(
            target,
            false,
            true,
            WorktreeApplyDirtyBehavior::RefuseOnDirty,
            WorktreeBaseline::Head,
        )
    }

    pub fn goto_without_record(&self, target: &StateId) -> Result<()> {
        self.goto_internal(
            target,
            false,
            false,
            WorktreeApplyDirtyBehavior::RefuseOnDirty,
            WorktreeBaseline::Head,
        )
    }

    pub fn goto_without_record_discard_local(&self, target: &StateId) -> Result<()> {
        self.goto_internal(
            target,
            false,
            false,
            WorktreeApplyDirtyBehavior::DiscardLocalChanges,
            WorktreeBaseline::Head,
        )
    }

    fn goto_internal(
        &self,
        target: &StateId,
        record: bool,
        current_worktree_verified_clean: bool,
        dirty_behavior: WorktreeApplyDirtyBehavior,
        baseline: WorktreeBaseline,
    ) -> Result<()> {
        let total_start = Instant::now();
        let _lock = self
            .locker()
            .write()
            .map_err(|e| HeddleError::Io(std::io::Error::other(e.to_string())))?;
        let load_start = Instant::now();
        let state = self
            .store
            .get_state(target)?
            .ok_or(HeddleError::StateNotFound(*target))?;

        let prev_head_ref = self.refs.read_head()?;
        let (prev_head, head_state) = match &prev_head_ref {
            Head::Attached { thread } => match self.refs.get_thread(thread)? {
                Some(change_id) => (Some(change_id), self.store.get_state(&change_id)?),
                None => (None, None),
            },
            Head::Detached { state } => (Some(*state), self.store.get_state(state)?),
        };
        let current_state = match baseline {
            WorktreeBaseline::Head => head_state,
            WorktreeBaseline::Materialized(Some(change_id)) => self.store.get_state(&change_id)?,
            WorktreeBaseline::Materialized(None) => None,
        };
        let same_state_verified_clean = current_worktree_verified_clean
            && current_state
                .as_ref()
                .is_some_and(|current_state| current_state.id() == *target);

        let tree = if same_state_verified_clean {
            None
        } else {
            Some(
                self.store
                    .get_tree(&state.tree)?
                    .ok_or_else(|| HeddleError::NotFound(format!("tree {}", state.tree)))?,
            )
        };
        let load_duration_ms = load_start.elapsed().as_millis();

        let current_tree = match &current_state {
            Some(current_state) => self.store.get_tree(&current_state.tree)?,
            None => None,
        };

        let (apply_plan, apply_report) = if let Some(tree) = tree.as_ref() {
            let apply_plan = self.plan_worktree_apply(
                current_tree.as_ref(),
                tree,
                &self.root,
                current_worktree_verified_clean,
                dirty_behavior,
            )?;
            let apply_report = self.execute_worktree_apply(&apply_plan, tree, &self.root)?;
            (apply_plan, apply_report)
        } else {
            (
                WorktreeApplyPlan {
                    strategy: WorktreeApplyStrategy::Incremental,
                    dirty_behavior: WorktreeApplyDirtyBehavior::RefuseOnDirty,
                    removals: Vec::new(),
                    directories: Vec::new(),
                    writes: Vec::new(),
                    fallback_reason: None,
                    stats: WorktreeApplyStats::default(),
                },
                WorktreeApplyReport::default(),
            )
        };

        if record {
            self.oplog
                .record_goto(target, prev_head.as_ref(), Some(&self.op_scope()))?;
            objects::fault_inject::maybe_panic_at("goto_after_oplog_commit_before_ref_publish");
        }
        self.refs.write_head(&Head::Detached { state: *target })?;

        debug!(
            load_duration_ms,
            apply_strategy = apply_plan.strategy.as_str(),
            apply_fallback_reason = apply_plan
                .fallback_reason
                .map(|reason| reason.as_str())
                .unwrap_or("none"),
            changed_count = apply_plan.stats.changed_count,
            unchanged_count = apply_plan.stats.unchanged_count,
            delete_phase_ms = apply_report.delete_phase_ms,
            mkdir_phase_ms = apply_report.mkdir_phase_ms,
            write_phase_ms = apply_report.write_phase_ms,
            index_update_ms = apply_report.index_update_ms,
            index_snapshot_load_ms = apply_report.index_snapshot_load_ms,
            index_journal_replay_ms = apply_report.index_journal_replay_ms,
            index_snapshot_write_ms = apply_report.index_snapshot_write_ms,
            index_journal_append_ms = apply_report.index_journal_append_ms,
            index_snapshot_bytes = apply_report.index_snapshot_bytes,
            index_journal_bytes = apply_report.index_journal_bytes,
            index_journal_ops = apply_report.index_journal_ops,
            index_compacted = apply_report.index_compacted,
            index_compact_reason = apply_report.index_compact_reason.unwrap_or("none"),
            fsmonitor_refresh_ms = apply_report.fsmonitor_refresh_ms,
            workers = apply_report.worker_count,
            total_duration_ms = total_start.elapsed().as_millis(),
            "Goto complete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use objects::object::ThreadName;
    use refs::Head;
    use tempfile::TempDir;

    use super::*;

    fn create_repo() -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        (temp, repo)
    }

    fn write_snapshot(
        repo: &Repository,
        root: &std::path::Path,
        path: &str,
        content: &str,
    ) -> StateId {
        fs::write(root.join(path), content).unwrap();
        repo.snapshot(Some(path.to_string()), None).unwrap().id()
    }

    #[test]
    fn goto_from_unknown_materialized_state_full_rematerializes_clean_checkout() {
        let (temp, repo) = create_repo();
        let target = write_snapshot(&repo, temp.path(), "target.txt", "target\n");
        repo.clear_worktree().unwrap();

        repo.goto_from_materialized_state(&target, None).unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("target.txt")).unwrap(),
            "target\n"
        );
        assert!(matches!(
            repo.refs().read_head().unwrap(),
            Head::Detached { state } if state == target
        ));
    }

    #[test]
    fn goto_from_materialized_pre_target_uses_disk_baseline_when_head_already_moved() {
        let (temp, repo) = create_repo();
        let file = temp.path().join("tracked.txt");
        let pre_target = write_snapshot(&repo, temp.path(), "tracked.txt", "before\n");
        fs::write(&file, "after\n").unwrap();
        fs::write(temp.path().join("added.txt"), "added\n").unwrap();
        let target = repo
            .snapshot(Some("target".to_string()), None)
            .unwrap()
            .id();

        repo.goto(&pre_target).unwrap();
        repo.refs()
            .write_head(&Head::Detached { state: target })
            .unwrap();

        repo.goto_from_materialized_state(&target, Some(&pre_target))
            .unwrap();

        assert_eq!(fs::read_to_string(&file).unwrap(), "after\n");
        assert_eq!(
            fs::read_to_string(temp.path().join("added.txt")).unwrap(),
            "added\n"
        );
        assert!(matches!(
            repo.refs().read_head().unwrap(),
            Head::Detached { state } if state == target
        ));
    }

    #[test]
    fn discard_local_goto_variants_overwrite_unsnapped_edits() {
        let (temp, repo) = create_repo();
        let tracked = temp.path().join("tracked.txt");
        let base = write_snapshot(&repo, temp.path(), "tracked.txt", "base\n");
        fs::write(&tracked, "target\n").unwrap();
        let target = repo
            .snapshot(Some("target".to_string()), None)
            .unwrap()
            .id();

        repo.goto(&base).unwrap();
        fs::write(&tracked, "local edit\n").unwrap();
        fs::write(temp.path().join("local.txt"), "local only\n").unwrap();

        repo.goto_without_record_discard_local(&target).unwrap();

        assert_eq!(fs::read_to_string(&tracked).unwrap(), "target\n");
        assert!(!temp.path().join("local.txt").exists());
    }

    #[test]
    fn attached_fast_forward_from_materialized_state_advances_thread_without_detaching() {
        let (temp, repo) = create_repo();
        let base = write_snapshot(&repo, temp.path(), "base.txt", "base\n");
        fs::write(temp.path().join("next.txt"), "next\n").unwrap();
        let target = repo
            .snapshot(Some("target".to_string()), None)
            .unwrap()
            .id();

        repo.goto(&base).unwrap();
        let thread = ThreadName::new("main");
        repo.refs().set_thread(&thread, &target).unwrap();
        repo.refs()
            .write_head(&Head::Attached {
                thread: thread.clone(),
            })
            .unwrap();

        repo.fast_forward_attached_from_materialized_state(&target, Some(&base))
            .unwrap();

        assert_eq!(repo.refs().get_thread(&thread).unwrap(), Some(target));
        assert!(matches!(
            repo.refs().read_head().unwrap(),
            Head::Attached { thread: current } if current == thread
        ));
        assert_eq!(
            fs::read_to_string(temp.path().join("next.txt")).unwrap(),
            "next\n"
        );
    }
}
