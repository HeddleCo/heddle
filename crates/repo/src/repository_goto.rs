// SPDX-License-Identifier: Apache-2.0
//! Worktree movement operations.

use objects::store::ObjectStore;
use std::time::Instant;

use objects::{lock::RepositoryLockExt, object::ChangeId};
use refs::Head;
use tracing::debug;

use super::{
    HeddleError, Repository, Result,
    repository_worktree_apply::{
        WorktreeApplyPlan, WorktreeApplyReport, WorktreeApplyStats, WorktreeApplyStrategy,
    },
};
use crate::{thread_model::ThreadFreshness, thread_storage::ThreadManager};

impl Repository {
    /// Move worktree to a different state.
    pub fn goto(&self, target: &ChangeId) -> Result<()> {
        self.goto_internal(target, true, false)
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
    pub fn fast_forward_attached(&self, target: &ChangeId) -> Result<()> {
        self.fast_forward_attached_internal(target, true)
    }

    /// Variant of [`Self::fast_forward_attached`] that performs the
    /// fast-forward without recording an `OpRecord::Goto`. The merge
    /// command uses this so it can record an `OpRecord::FastForwardV2`
    /// instead — the FF-specific variant carries both `pre_target_id`
    /// (for undo) and `post_target_id` (for deterministic redo). The
    /// generic `Goto` inverse only rewinds HEAD, which stranded the
    /// merged-into thread ref (heddle#99 r1); a name-resolved redo was
    /// also non-deterministic if the source thread moved (heddle#99 r2).
    pub fn fast_forward_attached_without_record(&self, target: &ChangeId) -> Result<()> {
        self.fast_forward_attached_internal(target, false)
    }

    fn fast_forward_attached_internal(&self, target: &ChangeId, record: bool) -> Result<()> {
        let head_before = self.refs.read_head()?;
        if record {
            self.goto(target)?;
        } else {
            self.goto_without_record(target)?;
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

    pub fn goto_verified_clean(&self, target: &ChangeId) -> Result<()> {
        self.goto_internal(target, true, true)
    }

    pub fn goto_verified_clean_without_record(&self, target: &ChangeId) -> Result<()> {
        self.goto_internal(target, false, true)
    }

    pub fn goto_without_record(&self, target: &ChangeId) -> Result<()> {
        self.goto_internal(target, false, false)
    }

    fn goto_internal(
        &self,
        target: &ChangeId,
        record: bool,
        current_worktree_verified_clean: bool,
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
        let (prev_head, current_state) = match &prev_head_ref {
            Head::Attached { thread } => match self.refs.get_thread(thread)? {
                Some(change_id) => (Some(change_id), self.store.get_state(&change_id)?),
                None => (None, None),
            },
            Head::Detached { state } => (Some(*state), self.store.get_state(state)?),
        };
        let same_state_verified_clean = current_worktree_verified_clean
            && current_state
                .as_ref()
                .is_some_and(|current_state| current_state.change_id == *target);

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
            )?;
            let apply_report = self.execute_worktree_apply(&apply_plan, tree, &self.root)?;
            (apply_plan, apply_report)
        } else {
            (
                WorktreeApplyPlan {
                    strategy: WorktreeApplyStrategy::Incremental,
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
