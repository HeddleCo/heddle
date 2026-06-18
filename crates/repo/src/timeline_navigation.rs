// SPDX-License-Identifier: Apache-2.0
//! Render-ready timeline navigation snapshots.

use std::collections::BTreeSet;

use objects::object::{
    ChangeId, ContentHash, NativeToolCallRefV1, TimelineBranchId, TimelineBranchReason,
    TimelineCursorMoveReason, TimelineLabel, TimelineOperationId, TimelineStepId,
    TimelineToolCallStatus,
};

use crate::{
    Repository, Result, TimelineMaterializationRecoveryRecord, TimelineStore, TimelineView,
};

#[derive(Clone, Debug)]
pub struct TimelineNavigationSnapshot {
    pub thread: String,
    pub cursor: TimelineNavigationCursor,
    pub branches: Vec<TimelineNavigationBranch>,
    pub steps: Vec<TimelineNavigationStep>,
    pub active_branch_path: Vec<TimelineBranchId>,
    pub actions: TimelineNavigationActionAvailability,
    pub recovery: Option<TimelineNavigationRecovery>,
}

#[derive(Clone, Debug)]
pub struct TimelineNavigationCursor {
    pub branch_id: Option<TimelineBranchId>,
    pub step_id: Option<TimelineStepId>,
    pub state: Option<ChangeId>,
}

#[derive(Clone, Debug)]
pub struct TimelineNavigationBranch {
    pub branch_id: TimelineBranchId,
    pub parent_branch_id: Option<TimelineBranchId>,
    pub forked_from_step_id: Option<TimelineStepId>,
    pub forked_from_state: Option<ChangeId>,
    pub reason: Option<TimelineBranchReason>,
    pub created_at_ms: Option<i64>,
    pub operation_ids: Vec<TimelineOperationId>,
    pub step_ids: Vec<TimelineStepId>,
    pub is_active: bool,
    pub is_on_active_path: bool,
}

#[derive(Clone, Debug)]
pub struct TimelineNavigationStep {
    pub thread: String,
    pub step_id: TimelineStepId,
    pub branch_id: TimelineBranchId,
    pub parent_step_id: Option<TimelineStepId>,
    pub native: Option<NativeToolCallRefV1>,
    pub tool_name: Option<String>,
    pub status: Option<TimelineToolCallStatus>,
    pub changed: Option<bool>,
    pub touched_paths: Vec<String>,
    pub before_state: Option<ChangeId>,
    pub after_state: Option<ChangeId>,
    pub capture_state: Option<ChangeId>,
    pub capture_oplog_batch_id: Option<u64>,
    pub labels: Vec<TimelineLabel>,
    pub payload_summary: Option<String>,
    pub payload_hash: Option<ContentHash>,
    pub operation_ids: Vec<TimelineOperationId>,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub cursor_state: Option<ChangeId>,
    pub is_current: bool,
    pub is_on_active_branch_path: bool,
    pub can_seek: bool,
    pub can_fork: bool,
    pub can_reset: bool,
    pub can_materialize: bool,
    pub has_boundary_warning: bool,
}

#[derive(Clone, Debug)]
pub struct TimelineNavigationActionAvailability {
    pub can_undo: bool,
    pub can_redo: bool,
}

#[derive(Clone, Debug)]
pub struct TimelineNavigationRecovery {
    pub status: TimelineNavigationRecoveryStatus,
    pub thread: String,
    pub branch_id: TimelineBranchId,
    pub from_step_id: Option<TimelineStepId>,
    pub to_step_id: Option<TimelineStepId>,
    pub from_state: ChangeId,
    pub to_state: ChangeId,
    pub reason: TimelineCursorMoveReason,
    pub moved_at_ms: i64,
    pub checkout_state: Option<ChangeId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineNavigationRecoveryStatus {
    PendingCursorRecord,
    Blocked,
    AlreadyApplied,
}

impl Repository {
    pub fn timeline_navigation_snapshot(
        &self,
        store: &TimelineStore,
        thread: &str,
    ) -> Result<TimelineNavigationSnapshot> {
        let view = TimelineView::rebuild(store)?;
        let status = view.status(thread);
        let active_branch_path = active_branch_path(
            &view,
            thread,
            status.and_then(|s| s.current_branch_id.as_ref()),
        );
        let active_branch_set = active_branch_path.iter().cloned().collect::<BTreeSet<_>>();
        let active_branch_id = status.and_then(|status| status.current_branch_id.as_ref());
        let current_step_id = status.and_then(|status| status.current_step_id.as_ref());

        let branches = view
            .branches_for_thread(thread)
            .into_iter()
            .map(|branch| TimelineNavigationBranch {
                branch_id: branch.branch_id.clone(),
                parent_branch_id: branch.parent_branch_id.clone(),
                forked_from_step_id: branch.forked_from_step_id.clone(),
                forked_from_state: branch.forked_from_state,
                reason: branch.reason.clone(),
                created_at_ms: branch.created_at_ms,
                operation_ids: branch.operation_ids.clone(),
                step_ids: branch.steps.clone(),
                is_active: active_branch_id == Some(&branch.branch_id),
                is_on_active_path: active_branch_set.contains(&branch.branch_id),
            })
            .collect();

        let steps = view
            .steps_for_thread(thread)
            .into_iter()
            .map(|step| {
                let cursor_state = step
                    .after_state
                    .or(step.capture_state)
                    .or(step.before_state);
                let can_target = cursor_state.is_some();
                TimelineNavigationStep {
                    thread: step.thread.clone(),
                    step_id: step.step_id.clone(),
                    branch_id: step.branch_id.clone(),
                    parent_step_id: step.parent_step_id.clone(),
                    native: step.native.clone(),
                    tool_name: step.tool_name.clone(),
                    status: step.status.clone(),
                    changed: step.changed,
                    touched_paths: step.touched_paths.clone(),
                    before_state: step.before_state,
                    after_state: step.after_state,
                    capture_state: step.capture_state,
                    capture_oplog_batch_id: step.capture_oplog_batch_id,
                    labels: step.labels.clone(),
                    payload_summary: step.payload_summary.clone(),
                    payload_hash: step.payload_hash,
                    operation_ids: step.operation_ids.clone(),
                    started_at_ms: step.started_at_ms,
                    finished_at_ms: step.finished_at_ms,
                    cursor_state,
                    is_current: current_step_id == Some(&step.step_id),
                    is_on_active_branch_path: active_branch_set.contains(&step.branch_id),
                    can_seek: can_target,
                    can_fork: can_target,
                    can_reset: can_target,
                    can_materialize: can_target,
                    has_boundary_warning: step.labels.iter().any(label_has_boundary_warning),
                }
            })
            .collect();

        let recovery = match store.read_materialization_recovery(thread)? {
            Some(record) => Some(self.navigation_recovery_status(&view, &record)?),
            None => None,
        };

        Ok(TimelineNavigationSnapshot {
            thread: thread.to_string(),
            cursor: TimelineNavigationCursor {
                branch_id: status.and_then(|status| status.current_branch_id.clone()),
                step_id: status.and_then(|status| status.current_step_id.clone()),
                state: status.and_then(|status| status.current_state),
            },
            branches,
            steps,
            active_branch_path,
            actions: TimelineNavigationActionAvailability {
                can_undo: view.resolve_undo_target(thread).is_some(),
                can_redo: view.resolve_redo_target(thread).is_some(),
            },
            recovery,
        })
    }

    fn navigation_recovery_status(
        &self,
        view: &TimelineView,
        record: &TimelineMaterializationRecoveryRecord,
    ) -> Result<TimelineNavigationRecovery> {
        let checkout_state = self.head()?;
        let status = if timeline_cursor_matches_recovery(view, record) {
            TimelineNavigationRecoveryStatus::AlreadyApplied
        } else if checkout_state == Some(record.to_state) {
            TimelineNavigationRecoveryStatus::PendingCursorRecord
        } else {
            TimelineNavigationRecoveryStatus::Blocked
        };

        Ok(TimelineNavigationRecovery {
            status,
            thread: record.thread.clone(),
            branch_id: record.branch_id.clone(),
            from_step_id: record.from_step_id.clone(),
            to_step_id: record.to_step_id.clone(),
            from_state: record.from_state,
            to_state: record.to_state,
            reason: record.reason.clone(),
            moved_at_ms: record.moved_at_ms,
            checkout_state,
        })
    }
}

fn active_branch_path(
    view: &TimelineView,
    thread: &str,
    current_branch_id: Option<&TimelineBranchId>,
) -> Vec<TimelineBranchId> {
    let mut path = Vec::new();
    let mut seen = BTreeSet::new();
    let mut next = current_branch_id.cloned();

    while let Some(branch_id) = next {
        if !seen.insert(branch_id.clone()) {
            break;
        }
        let parent = view
            .branch(thread, &branch_id)
            .and_then(|branch| branch.parent_branch_id.clone());
        path.push(branch_id);
        next = parent;
    }

    path.reverse();
    path
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

fn label_has_boundary_warning(label: &TimelineLabel) -> bool {
    matches!(
        label,
        TimelineLabel::IgnoredPathTouched
            | TimelineLabel::OutsideRepoTouched
            | TimelineLabel::PurgeBoundary
            | TimelineLabel::CaptureFailed
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use objects::object::{
        BranchCreatedV1, ContentHash, NativeToolCallRefV1, TimelineBranchReason,
        TimelineOperationBodyV1, TimelineOperationEnvelope, TimelineToolPayloadMetadata,
        ToolCallFinishedV1,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::{TimelineCursorMoveRecord, TimelineLabel, TimelineMaterializeMode};

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

    fn write_state(repo: &Repository, root: &Path, path: &str, content: &str) -> ChangeId {
        fs::write(root.join(path), content).unwrap();
        repo.snapshot(Some(path.to_string()), None)
            .unwrap()
            .change_id
    }

    fn write_finished_step(
        store: &TimelineStore,
        step_id: &str,
        branch_id: &str,
        native_id: &str,
        before_state: ChangeId,
        after_state: ChangeId,
        finished_at_ms: i64,
    ) {
        store
            .write_operation(&TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::ToolCallFinished(ToolCallFinishedV1 {
                    thread: "main".to_string(),
                    step_id: step(step_id),
                    branch_id: branch(branch_id),
                    native: native(native_id),
                    status: objects::object::TimelineToolCallStatus::Succeeded,
                    before_state,
                    after_state,
                    capture_state: Some(after_state),
                    capture_oplog_batch_id: Some(finished_at_ms as u64),
                    changed: true,
                    touched_paths: vec!["tracked.txt".to_string()],
                    payload: Some(TimelineToolPayloadMetadata {
                        summary: Some(format!("finished {native_id}")),
                        hash: Some(ContentHash::compute_typed(
                            "timeline-tool-payload",
                            native_id.as_bytes(),
                        )),
                    }),
                    finished_at_ms,
                }),
                vec![TimelineLabel::RepoReversible],
            ))
            .unwrap();
    }

    #[test]
    fn navigation_snapshot_marks_cursor_actions_and_active_path() {
        let (temp, repo, store) = create_repo();
        let state0 = repo.head().unwrap().unwrap();
        let state1 = write_state(&repo, temp.path(), "tracked.txt", "one\n");
        let state2 = write_state(&repo, temp.path(), "tracked.txt", "two\n");

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
        write_finished_step(&store, "tls-one", "tlb-main", "call-1", state0, state1, 2);
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
        write_finished_step(
            &store,
            "tls-child",
            "tlb-child",
            "call-2",
            state1,
            state2,
            4,
        );

        let snapshot = repo.timeline_navigation_snapshot(&store, "main").unwrap();

        assert_eq!(snapshot.cursor.branch_id, Some(branch("tlb-child")));
        assert_eq!(snapshot.cursor.step_id, Some(step("tls-child")));
        assert!(snapshot.actions.can_undo);
        assert!(!snapshot.actions.can_redo);
        assert_eq!(
            snapshot.active_branch_path,
            vec![branch("tlb-main"), branch("tlb-child")]
        );
        assert_eq!(snapshot.branches.len(), 2);
        assert!(snapshot.branches.iter().any(|branch| branch.is_active));
        let current = snapshot
            .steps
            .iter()
            .find(|step| step.is_current)
            .expect("current step");
        assert_eq!(current.step_id, step("tls-child"));
        assert_eq!(
            current
                .native
                .as_ref()
                .map(|native| native.tool_call_id.as_str()),
            Some("call-2")
        );
    }

    #[test]
    fn navigation_snapshot_surfaces_pending_recovery() {
        let (temp, repo, store) = create_repo();
        let state0 = repo.head().unwrap().unwrap();
        let state1 = write_state(&repo, temp.path(), "tracked.txt", "one\n");
        let state2 = write_state(&repo, temp.path(), "tracked.txt", "two\n");
        write_finished_step(&store, "tls-one", "tlb-main", "call-1", state0, state1, 1);
        write_finished_step(&store, "tls-two", "tlb-main", "call-2", state1, state2, 2);

        store
            .record_cursor_move(TimelineCursorMoveRecord {
                thread: "main".to_string(),
                branch_id: branch("tlb-main"),
                from_step_id: Some(step("tls-two")),
                to_step_id: Some(step("tls-one")),
                from_state: state2,
                to_state: state1,
                reason: TimelineCursorMoveReason::Undo,
                moved_at_ms: 3,
                labels: Vec::new(),
            })
            .unwrap();
        store
            .stage_materialization_recovery(&TimelineMaterializationRecoveryRecord::new(
                "main",
                branch("tlb-main"),
                Some(step("tls-one")),
                Some(step("tls-two")),
                state1,
                state2,
                TimelineCursorMoveReason::Redo,
                4,
            ))
            .unwrap();
        repo.goto(&state2).unwrap();

        let snapshot = repo.timeline_navigation_snapshot(&store, "main").unwrap();
        let recovery = snapshot.recovery.expect("pending recovery");

        assert_eq!(
            recovery.status,
            TimelineNavigationRecoveryStatus::PendingCursorRecord
        );
        assert_eq!(recovery.to_step_id, Some(step("tls-two")));
        assert_eq!(recovery.checkout_state, Some(state2));

        let outcome = repo
            .materialize_timeline_cursor(
                &store,
                "main",
                &crate::TimelineSeekSelector::CurrentCursor,
                TimelineMaterializeMode::FailIfDirty,
                5,
            )
            .unwrap();
        assert_eq!(
            outcome.recovery.status,
            crate::TimelineMaterializationRecoveryStatus::CursorRecorded
        );
    }

    #[test]
    fn navigation_boundary_warning_ignores_external_unknown_only() {
        let (temp, repo, store) = create_repo();
        let state0 = repo.head().unwrap().unwrap();
        let state1 = write_state(&repo, temp.path(), "tracked.txt", "one\n");

        store
            .write_operation(&TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::ToolCallFinished(ToolCallFinishedV1 {
                    thread: "main".to_string(),
                    step_id: step("tls-external"),
                    branch_id: branch("tlb-main"),
                    native: native("call-external"),
                    status: objects::object::TimelineToolCallStatus::Succeeded,
                    before_state: state0,
                    after_state: state1,
                    capture_state: Some(state1),
                    capture_oplog_batch_id: None,
                    changed: true,
                    touched_paths: Vec::new(),
                    payload: None,
                    finished_at_ms: 1,
                }),
                vec![
                    TimelineLabel::RepoReversible,
                    TimelineLabel::ExternalSideEffectsUnknown,
                ],
            ))
            .unwrap();
        store
            .write_operation(&TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::ToolCallFinished(ToolCallFinishedV1 {
                    thread: "main".to_string(),
                    step_id: step("tls-ignored"),
                    branch_id: branch("tlb-main"),
                    native: native("call-ignored"),
                    status: objects::object::TimelineToolCallStatus::Succeeded,
                    before_state: state1,
                    after_state: state1,
                    capture_state: Some(state1),
                    capture_oplog_batch_id: None,
                    changed: true,
                    touched_paths: vec!["ignored.log".to_string()],
                    payload: None,
                    finished_at_ms: 2,
                }),
                vec![
                    TimelineLabel::RepoReversible,
                    TimelineLabel::IgnoredPathTouched,
                ],
            ))
            .unwrap();

        let snapshot = repo.timeline_navigation_snapshot(&store, "main").unwrap();
        let external_id = step("tls-external");
        let ignored_id = step("tls-ignored");
        let external = snapshot
            .steps
            .iter()
            .find(|candidate| candidate.step_id == external_id)
            .expect("external step");
        let ignored = snapshot
            .steps
            .iter()
            .find(|candidate| candidate.step_id == ignored_id)
            .expect("ignored step");

        assert!(!external.has_boundary_warning);
        assert!(ignored.has_boundary_warning);
    }
}
