// SPDX-License-Identifier: Apache-2.0
//! First-class timeline navigation actions.

use objects::object::{
    BranchCreatedV1, TimelineBranchId, TimelineBranchReason, TimelineCursorMoveReason,
    TimelineLabel, TimelineOperationBodyV1, TimelineOperationEnvelope, TimelineOperationId,
};

use crate::{
    HeddleError, Repository, Result, TimelineCursorMoveRecord,
    TimelineMaterializationRecoveryOutcome, TimelineMaterializeMode, TimelineMaterializeOutcome,
    TimelineNavigationSnapshot, TimelineSeekBranchConstraint, TimelineSeekSelector, TimelineStepId,
    TimelineStore, TimelineView, timeline_materialize::resolve_timeline_selector,
};

#[derive(Clone, Debug)]
pub struct TimelineForkOutcome {
    pub navigation: TimelineNavigationSnapshot,
    pub operation_id: TimelineOperationId,
    pub branch_id: TimelineBranchId,
    pub parent_branch_id: TimelineBranchId,
    pub from_step_id: Option<TimelineStepId>,
}

#[derive(Clone, Debug)]
pub struct TimelineResetOutcome {
    pub navigation: TimelineNavigationSnapshot,
    pub cursor_operation_id: Option<TimelineOperationId>,
    pub materialization: Option<TimelineMaterializeOutcome>,
}

#[derive(Clone, Debug)]
pub struct TimelineRecoverOutcome {
    pub navigation: TimelineNavigationSnapshot,
    pub recovery: TimelineMaterializationRecoveryOutcome,
}

impl Repository {
    #[allow(clippy::too_many_arguments)]
    pub fn fork_timeline_from_selector(
        &self,
        store: &TimelineStore,
        thread: &str,
        selector: &TimelineSeekSelector,
        branch_constraint: Option<&TimelineSeekBranchConstraint>,
        branch_id: Option<TimelineBranchId>,
        reason: TimelineBranchReason,
        created_at_ms: i64,
    ) -> Result<TimelineForkOutcome> {
        let _record_guard = store.lock_recording(thread)?;
        let view = TimelineView::rebuild(store)?;
        let target = resolve_timeline_selector(&view, thread, selector)?;
        if let Some(constraint) = branch_constraint {
            validate_reset_branch_constraint(&view, thread, &target.branch_id, constraint)?;
        }
        let branch_id = branch_id.unwrap_or_else(TimelineBranchId::generate);
        if view.branch(thread, &branch_id).is_some() {
            return Err(HeddleError::Conflict(format!(
                "timeline branch '{}' already exists",
                branch_id
            )));
        }

        let operation_id = store.write_operation(&TimelineOperationEnvelope::new(
            TimelineOperationBodyV1::BranchCreated(BranchCreatedV1 {
                thread: thread.to_string(),
                branch_id: branch_id.clone(),
                parent_branch_id: Some(target.branch_id.clone()),
                from_step_id: target.step_id.clone(),
                from_state: target.state,
                reason,
                created_at_ms,
            }),
            Vec::new(),
        ))?;

        Ok(TimelineForkOutcome {
            navigation: self.timeline_navigation_snapshot(store, thread)?,
            operation_id,
            branch_id,
            parent_branch_id: target.branch_id,
            from_step_id: target.step_id,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn reset_timeline_cursor(
        &self,
        store: &TimelineStore,
        thread: &str,
        selector: &TimelineSeekSelector,
        mode: TimelineMaterializeMode,
        branch_constraint: Option<&TimelineSeekBranchConstraint>,
        materialize_checkout: bool,
        moved_at_ms: i64,
    ) -> Result<TimelineResetOutcome> {
        let _record_guard = store.lock_recording(thread)?;
        if materialize_checkout {
            let materialization = self.materialize_timeline_cursor_constrained_with_reason(
                store,
                thread,
                selector,
                mode,
                branch_constraint,
                TimelineCursorMoveReason::Reset,
                moved_at_ms,
            )?;
            let cursor_operation_id = materialization.cursor_operation_id;
            return Ok(TimelineResetOutcome {
                navigation: self.timeline_navigation_snapshot(store, thread)?,
                cursor_operation_id,
                materialization: Some(materialization),
            });
        }

        let view = TimelineView::rebuild(store)?;
        let target = resolve_timeline_selector(&view, thread, selector)?;
        if let Some(constraint) = branch_constraint {
            validate_reset_branch_constraint(&view, thread, &target.branch_id, constraint)?;
        }
        let status = view.status(thread);
        let cursor_operation_id = store.record_cursor_move(TimelineCursorMoveRecord {
            thread: thread.to_string(),
            branch_id: target.branch_id,
            from_step_id: status.and_then(|status| status.current_step_id.clone()),
            to_step_id: target.step_id,
            from_state: status
                .and_then(|status| status.current_state)
                .unwrap_or(target.state),
            to_state: target.state,
            reason: TimelineCursorMoveReason::Reset,
            moved_at_ms,
            labels: vec![TimelineLabel::RepoReversible],
        })?;

        Ok(TimelineResetOutcome {
            navigation: self.timeline_navigation_snapshot(store, thread)?,
            cursor_operation_id: Some(cursor_operation_id),
            materialization: None,
        })
    }

    pub fn recover_timeline_materialization_action(
        &self,
        store: &TimelineStore,
        thread: &str,
    ) -> Result<TimelineRecoverOutcome> {
        let recovery = self.recover_pending_timeline_materialization(store, thread)?;
        Ok(TimelineRecoverOutcome {
            navigation: self.timeline_navigation_snapshot(store, thread)?,
            recovery,
        })
    }
}

fn validate_reset_branch_constraint(
    view: &TimelineView,
    thread: &str,
    target_branch_id: &TimelineBranchId,
    constraint: &TimelineSeekBranchConstraint,
) -> Result<()> {
    match constraint {
        TimelineSeekBranchConstraint::Target(expected) if expected != target_branch_id => {
            Err(HeddleError::Conflict(format!(
                "timeline target belongs to branch '{}', not requested branch '{}'",
                target_branch_id, expected
            )))
        }
        TimelineSeekBranchConstraint::Current(expected)
            if view
                .status(thread)
                .and_then(|status| status.current_branch_id.as_ref())
                != Some(expected) =>
        {
            let actual = view
                .status(thread)
                .and_then(|status| status.current_branch_id.as_ref())
                .map(ToString::to_string)
                .unwrap_or_else(|| "unknown".to_string());
            Err(HeddleError::Conflict(format!(
                "timeline cursor is on branch '{actual}', not requested branch '{expected}'"
            )))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use objects::object::{
        NativeToolCallRefV1, TimelineOperationBodyV1, TimelineToolCallStatus, ToolCallFinishedV1,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::{TimelineNativeToolKey, TimelineStepId};

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

    fn write_state(
        repo: &Repository,
        root: &Path,
        path: &str,
        content: &str,
    ) -> objects::object::ChangeId {
        fs::write(root.join(path), content).unwrap();
        repo.snapshot(Some(path.to_string()), None)
            .unwrap()
            .change_id
    }

    fn write_step(
        store: &TimelineStore,
        step_id: &str,
        call_id: &str,
        before: objects::object::ChangeId,
        after: objects::object::ChangeId,
    ) {
        store
            .write_operation(&TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::ToolCallFinished(ToolCallFinishedV1 {
                    thread: "main".to_string(),
                    step_id: step(step_id),
                    branch_id: branch("tlb-main"),
                    native: native(call_id),
                    status: TimelineToolCallStatus::Succeeded,
                    before_state: before,
                    after_state: after,
                    capture_state: Some(after),
                    capture_oplog_batch_id: None,
                    changed: true,
                    touched_paths: vec!["tracked.txt".to_string()],
                    payload: None,
                    finished_at_ms: 1,
                }),
                vec![TimelineLabel::RepoReversible],
            ))
            .unwrap();
    }

    #[test]
    fn fork_from_native_selector_returns_updated_navigation() {
        let (temp, repo, store) = create_repo();
        let state0 = repo.head().unwrap().unwrap();
        let state1 = write_state(&repo, temp.path(), "tracked.txt", "one\n");
        write_step(&store, "tls-one", "call-1", state0, state1);

        let outcome = repo
            .fork_timeline_from_selector(
                &store,
                "main",
                &TimelineSeekSelector::NativeToolCall(TimelineNativeToolKey {
                    harness: "opencode".to_string(),
                    session_id: Some("session-1".to_string()),
                    message_id: Some("message-1".to_string()),
                    tool_call_id: "call-1".to_string(),
                }),
                None,
                Some(branch("tlb-child")),
                TimelineBranchReason::ExplicitFork,
                2,
            )
            .unwrap();

        assert_eq!(outcome.branch_id, branch("tlb-child"));
        assert_eq!(outcome.parent_branch_id, branch("tlb-main"));
        assert_eq!(outcome.from_step_id, Some(step("tls-one")));
        assert!(
            outcome
                .navigation
                .branches
                .iter()
                .any(|summary| summary.branch_id == branch("tlb-child"))
        );
    }

    #[test]
    fn reset_without_materialization_moves_logical_cursor() {
        let (temp, repo, store) = create_repo();
        let state0 = repo.head().unwrap().unwrap();
        let state1 = write_state(&repo, temp.path(), "tracked.txt", "one\n");
        let state2 = write_state(&repo, temp.path(), "tracked.txt", "two\n");
        write_step(&store, "tls-one", "call-1", state0, state1);
        write_step(&store, "tls-two", "call-2", state1, state2);

        let outcome = repo
            .reset_timeline_cursor(
                &store,
                "main",
                &TimelineSeekSelector::StepId(step("tls-one")),
                TimelineMaterializeMode::FailIfDirty,
                None,
                false,
                3,
            )
            .unwrap();

        assert!(outcome.cursor_operation_id.is_some());
        assert!(outcome.materialization.is_none());
        assert_eq!(outcome.navigation.cursor.step_id, Some(step("tls-one")));
        assert_eq!(repo.head().unwrap(), Some(state2));
    }
}
