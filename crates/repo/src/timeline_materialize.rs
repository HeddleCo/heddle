// SPDX-License-Identifier: Apache-2.0
//! Timeline cursor materialization helpers.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use objects::{
    object::{
        ChangeId, TimelineBranchId, TimelineCursorMoveReason, TimelineLabel, TimelineOperationId,
    },
    store::ObjectStore,
};

use crate::{
    HeddleError, Repository, Result, TimelineCursorMoveRecord,
    TimelineMaterializationRecoveryRecord, TimelineNativeToolKey, TimelineSeekTarget,
    TimelineStepId, TimelineStore, TimelineView, WorktreeStatusDetailed,
    repository::repository_worktree_apply::WorktreeApplyDirtyBehavior,
};

/// Selects the logical timeline target a checkout should materialize.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelineSeekSelector {
    /// Seek directly to a thread-local step id.
    StepId(TimelineStepId),
    /// Seek to the step associated with a native harness tool-call id.
    NativeToolCall(TimelineNativeToolKey),
    /// Seek to the previous step from the current logical cursor.
    Undo,
    /// Seek to the next step from the current logical cursor.
    Redo,
    /// Materialize the current logical cursor without changing it.
    CurrentCursor,
}

/// Optional branch invariant supplied by an embedding harness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelineSeekBranchConstraint {
    /// The logical cursor must currently be on this branch after any pending
    /// recovery has completed.
    Current(TimelineBranchId),
    /// The resolved target must belong to this branch.
    Target(TimelineBranchId),
}

/// Materialization safety mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineMaterializeMode {
    /// Refuse when the physical checkout has uncommitted tracked/untracked work.
    FailIfDirty,
    /// Capture the current checkout before seeking. Reserved for a later slice.
    CaptureCurrentThenSeek,
}

/// Conservative boundary assessment for a materialization preview.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineMaterializationBoundaryStatus {
    /// The repo apply path only touches tracked repo paths; no ignored/outside
    /// boundary probe has been performed for this preview.
    Unknown,
}

/// Why a materialization preview cannot proceed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelineMaterializationBlocker {
    /// The requested mode is defined but not implemented by this slice.
    UnsupportedMode(TimelineMaterializeMode),
    /// The physical checkout has local changes relative to its current state.
    DirtyWorktree { paths: Vec<String> },
    /// The physical checkout has no known current state to compare against.
    CheckoutStateUnknown,
    /// The state object exists, but its tree is unavailable.
    MissingTree(ChangeId),
}

/// Preview of a timeline seek before optional physical materialization.
#[derive(Clone, Debug)]
pub struct TimelineSeekPreview {
    pub thread: String,
    pub current_branch_id: Option<crate::TimelineBranchId>,
    pub current_step_id: Option<TimelineStepId>,
    pub current_state: Option<ChangeId>,
    pub checkout_state: Option<ChangeId>,
    pub target: TimelineSeekTarget,
    pub changed_paths: Vec<String>,
    pub worktree_status: Option<WorktreeStatusDetailed>,
    pub boundary_status: TimelineMaterializationBoundaryStatus,
    pub blockers: Vec<TimelineMaterializationBlocker>,
}

impl TimelineSeekPreview {
    pub fn can_materialize(&self) -> bool {
        self.blockers.is_empty()
    }
}

/// Final status for a materialization attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelineMaterializeStatus {
    Materialized,
    AlreadyAtTarget,
    Refused,
    Unsupported,
    RecoveryBlocked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelineMaterializationRecoveryStatus {
    NoPending,
    CursorRecorded,
    AlreadyApplied,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelineMaterializationRecoveryBlocker {
    CheckoutNotAtTarget {
        checkout_state: Option<ChangeId>,
        target_state: ChangeId,
    },
}

#[derive(Clone, Debug)]
pub struct TimelineMaterializationRecoveryOutcome {
    pub record: Option<TimelineMaterializationRecoveryRecord>,
    pub cursor_operation_id: Option<TimelineOperationId>,
    pub status: TimelineMaterializationRecoveryStatus,
    pub blocker: Option<TimelineMaterializationRecoveryBlocker>,
}

impl TimelineMaterializationRecoveryOutcome {
    fn no_pending() -> Self {
        Self {
            record: None,
            cursor_operation_id: None,
            status: TimelineMaterializationRecoveryStatus::NoPending,
            blocker: None,
        }
    }
}

/// Result of a materialization attempt.
#[derive(Clone, Debug)]
pub struct TimelineMaterializeOutcome {
    pub preview: TimelineSeekPreview,
    pub cursor_operation_id: Option<TimelineOperationId>,
    pub status: TimelineMaterializeStatus,
    pub recovery: TimelineMaterializationRecoveryOutcome,
}

impl Repository {
    /// Preview a timeline seek target without mutating the checkout or timeline.
    pub fn preview_timeline_seek(
        &self,
        store: &TimelineStore,
        thread: &str,
        selector: &TimelineSeekSelector,
        mode: TimelineMaterializeMode,
    ) -> Result<TimelineSeekPreview> {
        self.preview_timeline_seek_constrained(store, thread, selector, mode, None)
    }

    /// Preview a timeline seek target with an optional branch invariant.
    pub fn preview_timeline_seek_constrained(
        &self,
        store: &TimelineStore,
        thread: &str,
        selector: &TimelineSeekSelector,
        mode: TimelineMaterializeMode,
        branch_constraint: Option<&TimelineSeekBranchConstraint>,
    ) -> Result<TimelineSeekPreview> {
        let view = TimelineView::rebuild(store)?;
        let target = resolve_timeline_selector(&view, thread, selector)?;
        let preview = self.preview_timeline_target(&view, thread, target, mode)?;
        validate_branch_constraint(&preview, branch_constraint)?;
        Ok(preview)
    }

    /// Materialize a logical timeline cursor target into the physical checkout.
    ///
    /// This writes the timeline `cursor_moved` operation only after the checkout
    /// is known to be at the target state. A per-thread recovery sidecar is
    /// staged before the checkout move and cleared after the cursor move, so a
    /// later call can finish the logical cursor update if a process crashes
    /// between those writes.
    pub fn materialize_timeline_cursor(
        &self,
        store: &TimelineStore,
        thread: &str,
        selector: &TimelineSeekSelector,
        mode: TimelineMaterializeMode,
        moved_at_ms: i64,
    ) -> Result<TimelineMaterializeOutcome> {
        self.materialize_timeline_cursor_constrained(
            store,
            thread,
            selector,
            mode,
            None,
            moved_at_ms,
        )
    }

    /// Materialize a logical timeline cursor target with an optional branch
    /// invariant checked after pending recovery has been applied.
    pub fn materialize_timeline_cursor_constrained(
        &self,
        store: &TimelineStore,
        thread: &str,
        selector: &TimelineSeekSelector,
        mode: TimelineMaterializeMode,
        branch_constraint: Option<&TimelineSeekBranchConstraint>,
        moved_at_ms: i64,
    ) -> Result<TimelineMaterializeOutcome> {
        self.materialize_timeline_cursor_constrained_with_reason(
            store,
            thread,
            selector,
            mode,
            branch_constraint,
            cursor_reason(selector),
            moved_at_ms,
        )
    }

    pub fn materialize_timeline_cursor_constrained_with_reason(
        &self,
        store: &TimelineStore,
        thread: &str,
        selector: &TimelineSeekSelector,
        mode: TimelineMaterializeMode,
        branch_constraint: Option<&TimelineSeekBranchConstraint>,
        reason: TimelineCursorMoveReason,
        moved_at_ms: i64,
    ) -> Result<TimelineMaterializeOutcome> {
        let _materialization_guard = store.lock_materialization(thread)?;
        let recovery = self.recover_pending_timeline_materialization(store, thread)?;
        let preview = self.preview_timeline_seek(store, thread, selector, mode)?;
        if recovery.status == TimelineMaterializationRecoveryStatus::Blocked {
            return Ok(TimelineMaterializeOutcome {
                preview,
                cursor_operation_id: None,
                status: TimelineMaterializeStatus::RecoveryBlocked,
                recovery,
            });
        }
        validate_branch_constraint(&preview, branch_constraint)?;
        if preview
            .blockers
            .iter()
            .any(|blocker| matches!(blocker, TimelineMaterializationBlocker::UnsupportedMode(_)))
        {
            return Ok(TimelineMaterializeOutcome {
                preview,
                cursor_operation_id: None,
                status: TimelineMaterializeStatus::Unsupported,
                recovery,
            });
        }
        if !preview.can_materialize() {
            return Ok(TimelineMaterializeOutcome {
                preview,
                cursor_operation_id: None,
                status: TimelineMaterializeStatus::Refused,
                recovery,
            });
        }

        let already_at_target = preview.checkout_state == Some(preview.target.state)
            && preview
                .worktree_status
                .as_ref()
                .is_none_or(WorktreeStatusDetailed::is_clean);

        let moved = preview.current_step_id != preview.target.step_id
            || preview.current_state != Some(preview.target.state)
            || preview.current_branch_id != Some(preview.target.branch_id.clone());
        let recovery_record = moved.then(|| {
            TimelineMaterializationRecoveryRecord::new(
                preview.thread.clone(),
                preview.target.branch_id.clone(),
                preview.current_step_id.clone(),
                preview.target.step_id.clone(),
                preview
                    .current_state
                    .or(preview.checkout_state)
                    .unwrap_or(preview.target.state),
                preview.target.state,
                reason,
                moved_at_ms,
            )
        });
        if let Some(record) = &recovery_record {
            store.stage_materialization_recovery(record)?;
        }

        if !already_at_target {
            self.goto(&preview.target.state)?;
            objects::fault_inject::maybe_panic_at(
                "timeline_materialize_after_goto_before_cursor_move",
            );
        }

        let cursor_operation_id = if moved {
            let record = recovery_record
                .as_ref()
                .expect("moved timeline materialization stages recovery");
            let id = record_cursor_move_from_recovery(store, record)?;
            objects::fault_inject::maybe_panic_at(
                "timeline_materialize_after_cursor_move_before_recovery_clear",
            );
            store.clear_materialization_recovery(&preview.thread)?;
            Some(id)
        } else {
            None
        };

        Ok(TimelineMaterializeOutcome {
            preview,
            cursor_operation_id,
            status: if already_at_target {
                TimelineMaterializeStatus::AlreadyAtTarget
            } else {
                TimelineMaterializeStatus::Materialized
            },
            recovery,
        })
    }

    pub fn recover_pending_timeline_materialization(
        &self,
        store: &TimelineStore,
        thread: &str,
    ) -> Result<TimelineMaterializationRecoveryOutcome> {
        let _materialization_guard = store.lock_materialization(thread)?;
        let Some(record) = store.read_materialization_recovery(thread)? else {
            return Ok(TimelineMaterializationRecoveryOutcome::no_pending());
        };
        let view = TimelineView::rebuild(store)?;
        if timeline_cursor_matches_recovery(&view, &record) {
            store.clear_materialization_recovery(thread)?;
            return Ok(TimelineMaterializationRecoveryOutcome {
                record: Some(record),
                cursor_operation_id: None,
                status: TimelineMaterializationRecoveryStatus::AlreadyApplied,
                blocker: None,
            });
        }

        let checkout_state = self.head()?;
        if checkout_state != Some(record.to_state) {
            return Ok(TimelineMaterializationRecoveryOutcome {
                blocker: Some(
                    TimelineMaterializationRecoveryBlocker::CheckoutNotAtTarget {
                        checkout_state,
                        target_state: record.to_state,
                    },
                ),
                record: Some(record),
                cursor_operation_id: None,
                status: TimelineMaterializationRecoveryStatus::Blocked,
            });
        }

        let id = record_cursor_move_from_recovery(store, &record)?;
        store.clear_materialization_recovery(thread)?;
        Ok(TimelineMaterializationRecoveryOutcome {
            record: Some(record),
            cursor_operation_id: Some(id),
            status: TimelineMaterializationRecoveryStatus::CursorRecorded,
            blocker: None,
        })
    }

    fn preview_timeline_target(
        &self,
        view: &TimelineView,
        thread: &str,
        target: TimelineSeekTarget,
        mode: TimelineMaterializeMode,
    ) -> Result<TimelineSeekPreview> {
        let status = view.status(thread);
        let checkout_state = self.head()?;
        let mut blockers = Vec::new();
        if mode == TimelineMaterializeMode::CaptureCurrentThenSeek {
            blockers.push(TimelineMaterializationBlocker::UnsupportedMode(mode));
        }

        let target_tree = self.tree_for_materialization_state(&target.state)?;
        let mut changed_paths = match checkout_state {
            Some(current) if current == target.state => Vec::new(),
            Some(current) => {
                let current_tree = self.tree_for_materialization_state(&current)?;
                changed_paths_from_worktree_apply_plan(self, Some(&current_tree), &target_tree)?
            }
            None => {
                blockers.push(TimelineMaterializationBlocker::CheckoutStateUnknown);
                diff_tree_paths(self, None, Some(&target_tree))?
            }
        };
        changed_paths.sort();
        changed_paths.dedup();

        let worktree_status = match checkout_state {
            Some(current) => {
                let current_tree = self.tree_for_materialization_state(&current)?;
                let status = self.compare_worktree_cached_detailed(&current_tree)?;
                if !status.is_clean() {
                    blockers.push(TimelineMaterializationBlocker::DirtyWorktree {
                        paths: dirty_status_paths(&status),
                    });
                }
                Some(status)
            }
            None => None,
        };

        Ok(TimelineSeekPreview {
            thread: thread.to_string(),
            current_branch_id: status.and_then(|status| status.current_branch_id.clone()),
            current_step_id: status.and_then(|status| status.current_step_id.clone()),
            current_state: status.and_then(|status| status.current_state),
            checkout_state,
            target,
            changed_paths,
            worktree_status,
            boundary_status: TimelineMaterializationBoundaryStatus::Unknown,
            blockers,
        })
    }

    fn tree_for_materialization_state(&self, state_id: &ChangeId) -> Result<objects::object::Tree> {
        let state = self
            .store()
            .get_state(state_id)?
            .ok_or(HeddleError::StateNotFound(*state_id))?;
        self.store()
            .get_tree(&state.tree)?
            .ok_or(TimelineMaterializationBlocker::MissingTree(*state_id))
            .map_err(|blocker| HeddleError::Config(format!("{blocker:?}")))
    }
}

fn validate_branch_constraint(
    preview: &TimelineSeekPreview,
    constraint: Option<&TimelineSeekBranchConstraint>,
) -> Result<()> {
    match constraint {
        Some(TimelineSeekBranchConstraint::Target(expected))
            if preview.target.branch_id != *expected =>
        {
            Err(HeddleError::Conflict(format!(
                "timeline target belongs to branch '{}', not requested branch '{}'",
                preview.target.branch_id, expected
            )))
        }
        Some(TimelineSeekBranchConstraint::Current(expected))
            if preview.current_branch_id.as_ref() != Some(expected) =>
        {
            let actual = preview
                .current_branch_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "unknown".to_string());
            Err(HeddleError::Conflict(format!(
                "timeline cursor is on branch '{actual}', not requested branch '{expected}'"
            )))
        }
        _ => Ok(()),
    }
}

pub(crate) fn resolve_timeline_selector(
    view: &TimelineView,
    thread: &str,
    selector: &TimelineSeekSelector,
) -> Result<TimelineSeekTarget> {
    let target = match selector {
        TimelineSeekSelector::StepId(step_id) => view.resolve_seek_target(thread, step_id),
        TimelineSeekSelector::NativeToolCall(native) => {
            view.resolve_seek_to_native_call(thread, native)
        }
        TimelineSeekSelector::Undo => view.resolve_undo_target(thread),
        TimelineSeekSelector::Redo => view.resolve_redo_target(thread),
        TimelineSeekSelector::CurrentCursor => {
            let status = view.status(thread).ok_or_else(|| {
                HeddleError::NotFound(format!("timeline status for thread '{thread}'"))
            })?;
            Some(TimelineSeekTarget {
                thread: thread.to_string(),
                branch_id: status.current_branch_id.clone().ok_or_else(|| {
                    HeddleError::NotFound(format!("timeline current branch for thread '{thread}'"))
                })?,
                step_id: status.current_step_id.clone(),
                state: status.current_state.ok_or_else(|| {
                    HeddleError::NotFound(format!("timeline current state for thread '{thread}'"))
                })?,
            })
        }
    };
    target.ok_or_else(|| {
        HeddleError::NotFound(format!(
            "timeline target for thread '{}' and selector {:?}",
            thread, selector
        ))
    })
}

fn cursor_reason(selector: &TimelineSeekSelector) -> TimelineCursorMoveReason {
    match selector {
        TimelineSeekSelector::Undo => TimelineCursorMoveReason::Undo,
        TimelineSeekSelector::Redo => TimelineCursorMoveReason::Redo,
        TimelineSeekSelector::StepId(_)
        | TimelineSeekSelector::NativeToolCall(_)
        | TimelineSeekSelector::CurrentCursor => TimelineCursorMoveReason::SeekToolCall,
    }
}

fn record_cursor_move_from_recovery(
    store: &TimelineStore,
    record: &TimelineMaterializationRecoveryRecord,
) -> Result<TimelineOperationId> {
    store.record_cursor_move(TimelineCursorMoveRecord {
        thread: record.thread.clone(),
        branch_id: record.branch_id.clone(),
        from_step_id: record.from_step_id.clone(),
        to_step_id: record.to_step_id.clone(),
        from_state: record.from_state,
        to_state: record.to_state,
        reason: record.reason.clone(),
        moved_at_ms: record.moved_at_ms,
        labels: vec![TimelineLabel::RepoReversible],
    })
}

fn timeline_cursor_matches_recovery(
    view: &TimelineView,
    record: &TimelineMaterializationRecoveryRecord,
) -> bool {
    view.status(&record.thread).is_some_and(|status| {
        status.current_branch_id.as_ref() == Some(&record.branch_id)
            && status.current_step_id == record.to_step_id
            && status.current_state == Some(record.to_state)
    })
}

fn dirty_status_paths(status: &WorktreeStatusDetailed) -> Vec<String> {
    let mut paths = BTreeSet::new();
    paths.extend(status.modified.iter().map(display_path));
    paths.extend(status.deleted.iter().map(display_path));
    paths.extend(status.untracked.flatten_paths().iter().map(display_path));
    paths.into_iter().collect()
}

fn display_path(path: &PathBuf) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn changed_paths_from_worktree_apply_plan(
    repo: &Repository,
    from_tree: Option<&objects::object::Tree>,
    to_tree: &objects::object::Tree,
) -> Result<Vec<String>> {
    let plan = repo.plan_worktree_apply(
        from_tree,
        to_tree,
        repo.root(),
        true,
        WorktreeApplyDirtyBehavior::RefuseOnDirty,
    )?;
    let removal_paths = relative_removal_paths(repo.root(), &plan.removals);
    let mut out = BTreeSet::new();

    for rel_path in &removal_paths {
        let is_directory_removal = removal_paths
            .iter()
            .any(|other| other != rel_path && other.starts_with(rel_path));
        if !is_directory_removal {
            out.insert(display_path(rel_path));
        }
    }

    for write in &plan.writes {
        out.insert(display_path(&repo_relative_path(repo.root(), write.path())));
    }

    Ok(out.into_iter().collect())
}

fn relative_removal_paths(root: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|path| repo_relative_path(root, path))
        .collect()
}

fn repo_relative_path(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

fn diff_tree_paths(
    repo: &Repository,
    from_tree: Option<&objects::object::Tree>,
    to_tree: Option<&objects::object::Tree>,
) -> Result<Vec<String>> {
    let mut out = BTreeSet::new();
    diff_tree_paths_inner(repo, Path::new(""), from_tree, to_tree, &mut out)?;
    Ok(out.into_iter().collect())
}

fn diff_tree_paths_inner(
    repo: &Repository,
    rel_path: &Path,
    from_tree: Option<&objects::object::Tree>,
    to_tree: Option<&objects::object::Tree>,
    out: &mut BTreeSet<String>,
) -> Result<()> {
    let from_entries = from_tree.map(objects::object::Tree::entries).unwrap_or(&[]);
    let to_entries = to_tree.map(objects::object::Tree::entries).unwrap_or(&[]);
    let mut from_index = 0;
    let mut to_index = 0;

    while from_index < from_entries.len() || to_index < to_entries.len() {
        match (from_entries.get(from_index), to_entries.get(to_index)) {
            (Some(from_entry), Some(to_entry)) => match from_entry.name.cmp(&to_entry.name) {
                std::cmp::Ordering::Less => {
                    collect_entry_paths(repo, &rel_path.join(&from_entry.name), from_entry, out)?;
                    from_index += 1;
                }
                std::cmp::Ordering::Greater => {
                    collect_entry_paths(repo, &rel_path.join(&to_entry.name), to_entry, out)?;
                    to_index += 1;
                }
                std::cmp::Ordering::Equal => {
                    let child_path = rel_path.join(&from_entry.name);
                    if from_entry.entry_type == objects::object::EntryType::Tree
                        && to_entry.entry_type == objects::object::EntryType::Tree
                    {
                        if from_entry.hash != to_entry.hash {
                            let from_subtree =
                                repo.store().get_tree(&from_entry.hash)?.ok_or_else(|| {
                                    HeddleError::NotFound(format!("tree {}", from_entry.hash))
                                })?;
                            let to_subtree =
                                repo.store().get_tree(&to_entry.hash)?.ok_or_else(|| {
                                    HeddleError::NotFound(format!("tree {}", to_entry.hash))
                                })?;
                            diff_tree_paths_inner(
                                repo,
                                &child_path,
                                Some(&from_subtree),
                                Some(&to_subtree),
                                out,
                            )?;
                        }
                    } else if from_entry.entry_type != to_entry.entry_type
                        || from_entry.hash != to_entry.hash
                        || from_entry.mode != to_entry.mode
                    {
                        out.insert(display_path(&child_path));
                    }
                    from_index += 1;
                    to_index += 1;
                }
            },
            (Some(from_entry), None) => {
                collect_entry_paths(repo, &rel_path.join(&from_entry.name), from_entry, out)?;
                from_index += 1;
            }
            (None, Some(to_entry)) => {
                collect_entry_paths(repo, &rel_path.join(&to_entry.name), to_entry, out)?;
                to_index += 1;
            }
            (None, None) => break,
        }
    }
    Ok(())
}

fn collect_entry_paths(
    repo: &Repository,
    rel_path: &Path,
    entry: &objects::object::TreeEntry,
    out: &mut BTreeSet<String>,
) -> Result<()> {
    if entry.entry_type == objects::object::EntryType::Tree {
        let tree = repo
            .store()
            .get_tree(&entry.hash)?
            .ok_or_else(|| HeddleError::NotFound(format!("tree {}", entry.hash)))?;
        for child in tree.entries() {
            collect_entry_paths(repo, &rel_path.join(&child.name), child, out)?;
        }
    } else {
        out.insert(display_path(&rel_path.to_path_buf()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use objects::object::{
        BranchCreatedV1, ContentHash, NativeToolCallRefV1, TimelineBranchId, TimelineBranchReason,
        TimelineOperationBodyV1, TimelineOperationEnvelope, TimelineToolCallStatus,
        TimelineToolPayloadMetadata, ToolCallFinishedV1, ToolCallStartedV1,
    };
    use tempfile::TempDir;

    use super::*;

    fn create_repo() -> (TempDir, Repository, TimelineStore) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let store = TimelineStore::open(repo.heddle_dir()).unwrap();
        (temp, repo, store)
    }

    fn step(id: &str) -> TimelineStepId {
        TimelineStepId::new(id)
    }

    fn branch(id: &str) -> TimelineBranchId {
        TimelineBranchId::new(id)
    }

    fn native(call: &str) -> NativeToolCallRefV1 {
        NativeToolCallRefV1 {
            harness: "opencode".to_string(),
            session_id: Some("session-1".to_string()),
            message_id: Some("message-1".to_string()),
            tool_call_id: call.to_string(),
        }
    }

    fn native_key(call: &str) -> TimelineNativeToolKey {
        TimelineNativeToolKey::from(&native(call))
    }

    fn write_state(repo: &Repository, root: &Path, path: &str, content: &str) -> ChangeId {
        fs::write(root.join(path), content).unwrap();
        repo.snapshot(Some(path.to_string()), None)
            .unwrap()
            .change_id
    }

    fn write_timeline(store: &TimelineStore, state0: ChangeId, state1: ChangeId, state2: ChangeId) {
        store
            .write_operation(&TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::BranchCreated(BranchCreatedV1 {
                    thread: "main".to_string(),
                    branch_id: branch("tlb-main"),
                    parent_branch_id: None,
                    from_step_id: None,
                    from_state: state0,
                    reason: TimelineBranchReason::ExplicitFork,
                    created_at_ms: 1,
                }),
                Vec::new(),
            ))
            .unwrap();

        store
            .write_operation(&TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::ToolCallStarted(ToolCallStartedV1 {
                    thread: "main".to_string(),
                    step_id: step("tls-one"),
                    branch_id: branch("tlb-main"),
                    parent_step_id: None,
                    native: native("call-1"),
                    tool_name: "shell".to_string(),
                    before_state: state0,
                    payload: Some(TimelineToolPayloadMetadata {
                        summary: Some("write first version".to_string()),
                        hash: Some(ContentHash::compute_typed(
                            "timeline-tool-payload",
                            b"call-1",
                        )),
                    }),
                    started_at_ms: 2,
                }),
                vec![TimelineLabel::RepoReversible],
            ))
            .unwrap();

        store
            .write_operation(&TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::ToolCallFinished(ToolCallFinishedV1 {
                    thread: "main".to_string(),
                    step_id: step("tls-one"),
                    branch_id: branch("tlb-main"),
                    native: native("call-1"),
                    status: TimelineToolCallStatus::Succeeded,
                    before_state: state0,
                    after_state: state1,
                    capture_state: Some(state1),
                    capture_oplog_batch_id: Some(1),
                    changed: true,
                    touched_paths: vec!["tracked.txt".to_string()],
                    payload: None,
                    finished_at_ms: 3,
                }),
                vec![TimelineLabel::RepoReversible],
            ))
            .unwrap();

        store
            .write_operation(&TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::ToolCallFinished(ToolCallFinishedV1 {
                    thread: "main".to_string(),
                    step_id: step("tls-two"),
                    branch_id: branch("tlb-main"),
                    native: native("call-2"),
                    status: TimelineToolCallStatus::Succeeded,
                    before_state: state1,
                    after_state: state2,
                    capture_state: Some(state2),
                    capture_oplog_batch_id: Some(2),
                    changed: true,
                    touched_paths: vec!["tracked.txt".to_string()],
                    payload: None,
                    finished_at_ms: 4,
                }),
                vec![TimelineLabel::RepoReversible],
            ))
            .unwrap();
    }

    fn write_timeline_with_child_branch(
        store: &TimelineStore,
        state0: ChangeId,
        state1: ChangeId,
        state2: ChangeId,
    ) {
        store
            .write_operation(&TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::BranchCreated(BranchCreatedV1 {
                    thread: "main".to_string(),
                    branch_id: branch("tlb-main"),
                    parent_branch_id: None,
                    from_step_id: None,
                    from_state: state0,
                    reason: TimelineBranchReason::ExplicitFork,
                    created_at_ms: 1,
                }),
                Vec::new(),
            ))
            .unwrap();

        store
            .write_operation(&TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::ToolCallFinished(ToolCallFinishedV1 {
                    thread: "main".to_string(),
                    step_id: step("tls-one"),
                    branch_id: branch("tlb-main"),
                    native: native("call-1"),
                    status: TimelineToolCallStatus::Succeeded,
                    before_state: state0,
                    after_state: state1,
                    capture_state: Some(state1),
                    capture_oplog_batch_id: Some(1),
                    changed: true,
                    touched_paths: vec!["tracked.txt".to_string()],
                    payload: None,
                    finished_at_ms: 2,
                }),
                vec![TimelineLabel::RepoReversible],
            ))
            .unwrap();

        store
            .write_operation(&TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::BranchCreated(BranchCreatedV1 {
                    thread: "main".to_string(),
                    branch_id: branch("tlb-child"),
                    parent_branch_id: Some(branch("tlb-main")),
                    from_step_id: Some(step("tls-one")),
                    from_state: state1,
                    reason: TimelineBranchReason::ExplicitFork,
                    created_at_ms: 3,
                }),
                Vec::new(),
            ))
            .unwrap();

        store
            .write_operation(&TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::ToolCallFinished(ToolCallFinishedV1 {
                    thread: "main".to_string(),
                    step_id: step("tls-child"),
                    branch_id: branch("tlb-child"),
                    native: native("call-2"),
                    status: TimelineToolCallStatus::Succeeded,
                    before_state: state1,
                    after_state: state2,
                    capture_state: Some(state2),
                    capture_oplog_batch_id: Some(2),
                    changed: true,
                    touched_paths: vec!["tracked.txt".to_string()],
                    payload: None,
                    finished_at_ms: 4,
                }),
                vec![TimelineLabel::RepoReversible],
            ))
            .unwrap();
    }

    #[test]
    fn preview_resolves_step_native_undo_and_redo_selectors() {
        let (temp, repo, store) = create_repo();
        let state0 = repo.head().unwrap().unwrap();
        let state1 = write_state(&repo, temp.path(), "tracked.txt", "one\n");
        let state2 = write_state(&repo, temp.path(), "tracked.txt", "two\n");
        write_timeline(&store, state0, state1, state2);

        let by_step = repo
            .preview_timeline_seek(
                &store,
                "main",
                &TimelineSeekSelector::StepId(step("tls-one")),
                TimelineMaterializeMode::FailIfDirty,
            )
            .unwrap();
        assert_eq!(by_step.target.state, state1);
        assert_eq!(by_step.changed_paths, vec!["tracked.txt"]);
        assert!(by_step.can_materialize());

        let by_native = repo
            .preview_timeline_seek(
                &store,
                "main",
                &TimelineSeekSelector::NativeToolCall(native_key("call-1")),
                TimelineMaterializeMode::FailIfDirty,
            )
            .unwrap();
        assert_eq!(by_native.target.state, state1);

        store
            .record_cursor_move(TimelineCursorMoveRecord {
                thread: "main".to_string(),
                branch_id: branch("tlb-main"),
                from_step_id: Some(step("tls-two")),
                to_step_id: Some(step("tls-one")),
                from_state: state2,
                to_state: state1,
                reason: TimelineCursorMoveReason::Undo,
                moved_at_ms: 5,
                labels: Vec::new(),
            })
            .unwrap();

        let undo = repo
            .preview_timeline_seek(
                &store,
                "main",
                &TimelineSeekSelector::Undo,
                TimelineMaterializeMode::FailIfDirty,
            )
            .unwrap();
        assert_eq!(undo.target.state, state0);

        let redo = repo
            .preview_timeline_seek(
                &store,
                "main",
                &TimelineSeekSelector::Redo,
                TimelineMaterializeMode::FailIfDirty,
            )
            .unwrap();
        assert_eq!(redo.target.state, state2);
    }

    #[test]
    fn fail_if_dirty_refuses_without_recording_cursor_move() {
        let (temp, repo, store) = create_repo();
        let state0 = repo.head().unwrap().unwrap();
        let state1 = write_state(&repo, temp.path(), "tracked.txt", "one\n");
        let state2 = write_state(&repo, temp.path(), "tracked.txt", "two\n");
        write_timeline(&store, state0, state1, state2);

        fs::write(temp.path().join("tracked.txt"), "local edit\n").unwrap();

        let before_ops = TimelineView::rebuild(&store).unwrap().operation_ids().len();
        let outcome = repo
            .materialize_timeline_cursor(
                &store,
                "main",
                &TimelineSeekSelector::StepId(step("tls-one")),
                TimelineMaterializeMode::FailIfDirty,
                10,
            )
            .unwrap();

        assert_eq!(outcome.status, TimelineMaterializeStatus::Refused);
        assert!(matches!(
            outcome.preview.blockers.as_slice(),
            [TimelineMaterializationBlocker::DirtyWorktree { .. }]
        ));
        assert_eq!(outcome.cursor_operation_id, None);
        assert_eq!(
            TimelineView::rebuild(&store).unwrap().operation_ids().len(),
            before_ops
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
            "local edit\n"
        );
    }

    #[test]
    fn materialize_success_moves_checkout_then_records_cursor_move() {
        let (temp, repo, store) = create_repo();
        let state0 = repo.head().unwrap().unwrap();
        let state1 = write_state(&repo, temp.path(), "tracked.txt", "one\n");
        let state2 = write_state(&repo, temp.path(), "tracked.txt", "two\n");
        write_timeline(&store, state0, state1, state2);

        let outcome = repo
            .materialize_timeline_cursor(
                &store,
                "main",
                &TimelineSeekSelector::StepId(step("tls-one")),
                TimelineMaterializeMode::FailIfDirty,
                10,
            )
            .unwrap();

        assert_eq!(outcome.status, TimelineMaterializeStatus::Materialized);
        assert!(outcome.cursor_operation_id.is_some());
        assert_eq!(
            outcome.recovery.status,
            TimelineMaterializationRecoveryStatus::NoPending
        );
        assert!(
            store
                .read_materialization_recovery("main")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(repo.head().unwrap(), Some(state1));

        let view = TimelineView::rebuild(&store).unwrap();
        let status = view.status("main").unwrap();
        assert_eq!(status.current_step_id, Some(step("tls-one")));
        assert_eq!(status.current_state, Some(state1));
    }

    #[test]
    fn recovery_completes_cursor_move_when_checkout_reached_target() {
        let (temp, repo, store) = create_repo();
        let state0 = repo.head().unwrap().unwrap();
        let state1 = write_state(&repo, temp.path(), "tracked.txt", "one\n");
        let state2 = write_state(&repo, temp.path(), "tracked.txt", "two\n");
        write_timeline(&store, state0, state1, state2);
        let record = TimelineMaterializationRecoveryRecord::new(
            "main",
            branch("tlb-main"),
            Some(step("tls-two")),
            Some(step("tls-one")),
            state2,
            state1,
            TimelineCursorMoveReason::SeekToolCall,
            10,
        );
        store.stage_materialization_recovery(&record).unwrap();
        repo.goto(&state1).unwrap();

        let outcome = repo
            .recover_pending_timeline_materialization(&store, "main")
            .unwrap();

        assert_eq!(
            outcome.status,
            TimelineMaterializationRecoveryStatus::CursorRecorded
        );
        assert!(outcome.cursor_operation_id.is_some());
        assert!(
            store
                .read_materialization_recovery("main")
                .unwrap()
                .is_none()
        );
        let view = TimelineView::rebuild(&store).unwrap();
        let status = view.status("main").unwrap();
        assert_eq!(status.current_step_id, Some(step("tls-one")));
        assert_eq!(status.current_state, Some(state1));
    }

    #[test]
    fn recovery_blocks_when_checkout_has_not_reached_target() {
        let (temp, repo, store) = create_repo();
        let state0 = repo.head().unwrap().unwrap();
        let state1 = write_state(&repo, temp.path(), "tracked.txt", "one\n");
        let state2 = write_state(&repo, temp.path(), "tracked.txt", "two\n");
        write_timeline(&store, state0, state1, state2);
        let record = TimelineMaterializationRecoveryRecord::new(
            "main",
            branch("tlb-main"),
            Some(step("tls-two")),
            Some(step("tls-one")),
            state2,
            state1,
            TimelineCursorMoveReason::SeekToolCall,
            10,
        );
        store.stage_materialization_recovery(&record).unwrap();

        let outcome = repo
            .materialize_timeline_cursor(
                &store,
                "main",
                &TimelineSeekSelector::StepId(step("tls-one")),
                TimelineMaterializeMode::FailIfDirty,
                11,
            )
            .unwrap();

        assert_eq!(outcome.status, TimelineMaterializeStatus::RecoveryBlocked);
        assert_eq!(
            outcome.recovery.status,
            TimelineMaterializationRecoveryStatus::Blocked
        );
        assert!(matches!(
            outcome.recovery.blocker,
            Some(
                TimelineMaterializationRecoveryBlocker::CheckoutNotAtTarget {
                    checkout_state: Some(checkout),
                    target_state
                }
            ) if checkout == state2 && target_state == state1
        ));
        assert!(
            store
                .read_materialization_recovery("main")
                .unwrap()
                .is_some()
        );
        let view = TimelineView::rebuild(&store).unwrap();
        assert_eq!(
            view.status("main").unwrap().current_step_id,
            Some(step("tls-two"))
        );
    }

    #[test]
    fn branch_constraint_is_checked_after_materialization_recovery() {
        let (temp, repo, store) = create_repo();
        let state0 = repo.head().unwrap().unwrap();
        let state1 = write_state(&repo, temp.path(), "tracked.txt", "one\n");
        let state2 = write_state(&repo, temp.path(), "tracked.txt", "two\n");
        write_timeline_with_child_branch(&store, state0, state1, state2);

        store
            .record_cursor_move(TimelineCursorMoveRecord {
                thread: "main".to_string(),
                branch_id: branch("tlb-main"),
                from_step_id: Some(step("tls-child")),
                to_step_id: Some(step("tls-one")),
                from_state: state2,
                to_state: state1,
                reason: TimelineCursorMoveReason::Undo,
                moved_at_ms: 5,
                labels: Vec::new(),
            })
            .unwrap();
        let view = TimelineView::rebuild(&store).unwrap();
        assert_eq!(
            view.status("main").unwrap().current_branch_id,
            Some(branch("tlb-main"))
        );

        let record = TimelineMaterializationRecoveryRecord::new(
            "main",
            branch("tlb-child"),
            Some(step("tls-one")),
            Some(step("tls-child")),
            state1,
            state2,
            TimelineCursorMoveReason::Redo,
            10,
        );
        store.stage_materialization_recovery(&record).unwrap();
        repo.goto(&state2).unwrap();

        let constraint = TimelineSeekBranchConstraint::Current(branch("tlb-child"));
        let outcome = repo
            .materialize_timeline_cursor_constrained(
                &store,
                "main",
                &TimelineSeekSelector::CurrentCursor,
                TimelineMaterializeMode::FailIfDirty,
                Some(&constraint),
                11,
            )
            .unwrap();

        assert_eq!(outcome.status, TimelineMaterializeStatus::AlreadyAtTarget);
        assert_eq!(
            outcome.recovery.status,
            TimelineMaterializationRecoveryStatus::CursorRecorded
        );
        assert!(outcome.recovery.cursor_operation_id.is_some());

        let view = TimelineView::rebuild(&store).unwrap();
        let status = view.status("main").unwrap();
        assert_eq!(status.current_branch_id, Some(branch("tlb-child")));
        assert_eq!(status.current_step_id, Some(step("tls-child")));
        assert_eq!(status.current_state, Some(state2));
    }

    #[test]
    fn capture_current_then_seek_is_explicitly_unsupported() {
        let (temp, repo, store) = create_repo();
        let state0 = repo.head().unwrap().unwrap();
        let state1 = write_state(&repo, temp.path(), "tracked.txt", "one\n");
        let state2 = write_state(&repo, temp.path(), "tracked.txt", "two\n");
        write_timeline(&store, state0, state1, state2);

        let outcome = repo
            .materialize_timeline_cursor(
                &store,
                "main",
                &TimelineSeekSelector::StepId(step("tls-one")),
                TimelineMaterializeMode::CaptureCurrentThenSeek,
                10,
            )
            .unwrap();

        assert_eq!(outcome.status, TimelineMaterializeStatus::Unsupported);
        assert_eq!(outcome.cursor_operation_id, None);
        assert_eq!(
            fs::read_to_string(temp.path().join("tracked.txt")).unwrap(),
            "two\n"
        );
    }
}
