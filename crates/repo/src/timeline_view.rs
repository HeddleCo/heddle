// SPDX-License-Identifier: Apache-2.0
//! Rebuildable derived views over canonical agent timeline operations.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use objects::{
    error::{HeddleError, Result},
    object::{
        ChangeId, ContentHash, CursorMovedV1, NativeToolCallRefV1, TimelineBranchId,
        TimelineBranchReason, TimelineCursorMoveReason, TimelineLabel, TimelineOperationBodyV1,
        TimelineOperationEnvelope, TimelineOperationId, TimelineOperationKind, TimelineStepId,
        TimelineToolCallStatus,
    },
};

use crate::TimelineStore;

/// Native harness identity for a tool call.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimelineNativeToolKey {
    pub harness: String,
    pub session_id: Option<String>,
    pub message_id: Option<String>,
    pub tool_call_id: String,
}

impl From<&NativeToolCallRefV1> for TimelineNativeToolKey {
    fn from(value: &NativeToolCallRefV1) -> Self {
        Self {
            harness: value.harness.clone(),
            session_id: value.session_id.clone(),
            message_id: value.message_id.clone(),
            tool_call_id: value.tool_call_id.clone(),
        }
    }
}

/// Thread-local step key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimelineStepKey {
    pub thread: String,
    pub step_id: TimelineStepId,
}

impl TimelineStepKey {
    pub fn new(thread: impl Into<String>, step_id: TimelineStepId) -> Self {
        Self {
            thread: thread.into(),
            step_id,
        }
    }
}

/// Thread-local branch key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimelineBranchKey {
    pub thread: String,
    pub branch_id: TimelineBranchId,
}

impl TimelineBranchKey {
    pub fn new(thread: impl Into<String>, branch_id: TimelineBranchId) -> Self {
        Self {
            thread: thread.into(),
            branch_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct TimelineNativeIndexKey {
    thread: String,
    native: TimelineNativeToolKey,
}

/// Rebuilt current cursor status for a thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineThreadStatus {
    pub thread: String,
    pub current_branch_id: Option<TimelineBranchId>,
    pub current_step_id: Option<TimelineStepId>,
    pub current_state: Option<ChangeId>,
    pub last_operation_id: Option<TimelineOperationId>,
}

impl TimelineThreadStatus {
    fn new(thread: impl Into<String>) -> Self {
        Self {
            thread: thread.into(),
            current_branch_id: None,
            current_step_id: None,
            current_state: None,
            last_operation_id: None,
        }
    }
}

/// Rebuilt branch summary with ordered unique steps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineBranchSummary {
    pub thread: String,
    pub branch_id: TimelineBranchId,
    pub parent_branch_id: Option<TimelineBranchId>,
    pub forked_from_step_id: Option<TimelineStepId>,
    pub forked_from_state: Option<ChangeId>,
    pub reason: Option<TimelineBranchReason>,
    pub created_at_ms: Option<i64>,
    pub operation_ids: Vec<TimelineOperationId>,
    pub steps: Vec<TimelineStepId>,
}

impl TimelineBranchSummary {
    fn implicit(thread: impl Into<String>, branch_id: TimelineBranchId) -> Self {
        Self {
            thread: thread.into(),
            branch_id,
            parent_branch_id: None,
            forked_from_step_id: None,
            forked_from_state: None,
            reason: None,
            created_at_ms: None,
            operation_ids: Vec::new(),
            steps: Vec::new(),
        }
    }
}

/// Rebuilt step summary. Native payload details are limited to summaries and hashes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineStepSummary {
    pub thread: String,
    pub step_id: TimelineStepId,
    pub branch_id: TimelineBranchId,
    pub parent_step_id: Option<TimelineStepId>,
    pub native: Option<NativeToolCallRefV1>,
    pub tool_name: Option<String>,
    pub before_state: Option<ChangeId>,
    pub after_state: Option<ChangeId>,
    pub capture_state: Option<ChangeId>,
    pub capture_oplog_batch_id: Option<u64>,
    pub changed: Option<bool>,
    pub status: Option<TimelineToolCallStatus>,
    pub labels: Vec<TimelineLabel>,
    pub touched_paths: Vec<String>,
    pub payload_summary: Option<String>,
    pub payload_hash: Option<ContentHash>,
    pub operation_ids: Vec<TimelineOperationId>,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
}

impl TimelineStepSummary {
    fn new(
        thread: impl Into<String>,
        step_id: TimelineStepId,
        branch_id: TimelineBranchId,
    ) -> Self {
        Self {
            thread: thread.into(),
            step_id,
            branch_id,
            parent_step_id: None,
            native: None,
            tool_name: None,
            before_state: None,
            after_state: None,
            capture_state: None,
            capture_oplog_batch_id: None,
            changed: None,
            status: None,
            labels: Vec::new(),
            touched_paths: Vec::new(),
            payload_summary: None,
            payload_hash: None,
            operation_ids: Vec::new(),
            started_at_ms: None,
            finished_at_ms: None,
        }
    }

    fn cursor_state(&self) -> Option<ChangeId> {
        self.after_state
            .or(self.capture_state)
            .or(self.before_state)
    }
}

/// A resolved cursor target. This slice does not materialize the worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineSeekTarget {
    pub thread: String,
    pub branch_id: TimelineBranchId,
    pub step_id: Option<TimelineStepId>,
    pub state: ChangeId,
}

/// Command used to append a canonical cursor movement operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineCursorMoveRecord {
    pub thread: String,
    pub branch_id: TimelineBranchId,
    pub from_step_id: Option<TimelineStepId>,
    pub to_step_id: Option<TimelineStepId>,
    pub from_state: ChangeId,
    pub to_state: ChangeId,
    pub reason: TimelineCursorMoveReason,
    pub moved_at_ms: i64,
    pub labels: Vec<TimelineLabel>,
}

/// Deterministic derived timeline view rebuilt from canonical operation bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimelineView {
    operation_ids: Vec<TimelineOperationId>,
    threads: BTreeMap<String, TimelineThreadStatus>,
    branches: BTreeMap<TimelineBranchKey, TimelineBranchSummary>,
    steps: BTreeMap<TimelineStepKey, TimelineStepSummary>,
    native_index: BTreeMap<TimelineNativeIndexKey, TimelineStepKey>,
}

impl TimelineView {
    /// Rebuild the derived view from all canonical operations in the store.
    pub fn rebuild(store: &TimelineStore) -> Result<Self> {
        let mut records = read_timeline_operation_records(store)?;
        records.sort_by_key(|record| {
            (
                operation_timestamp(&record.envelope),
                operation_kind_order(record.envelope.kind),
                record.id.to_hex(),
            )
        });

        let mut view = Self::default();
        for record in records {
            view.operation_ids.push(record.id);
            view.apply_operation(record.id, record.envelope);
        }
        Ok(view)
    }

    /// Operation ids included in this view, in deterministic replay order.
    pub fn operation_ids(&self) -> &[TimelineOperationId] {
        &self.operation_ids
    }

    /// Number of branches known for a thread.
    pub fn branch_count(&self, thread: &str) -> usize {
        self.branches
            .keys()
            .filter(|key| key.thread == thread)
            .count()
    }

    /// Number of steps known for a thread.
    pub fn step_count(&self, thread: &str) -> usize {
        self.steps.keys().filter(|key| key.thread == thread).count()
    }

    /// Current thread cursor status.
    pub fn status(&self, thread: &str) -> Option<&TimelineThreadStatus> {
        self.threads.get(thread)
    }

    /// Branch summary for a thread-local branch id.
    pub fn branch(
        &self,
        thread: &str,
        branch_id: &TimelineBranchId,
    ) -> Option<&TimelineBranchSummary> {
        self.branches
            .get(&TimelineBranchKey::new(thread, branch_id.clone()))
    }

    /// Branch summaries for a thread in deterministic key order.
    pub fn branches_for_thread(&self, thread: &str) -> Vec<&TimelineBranchSummary> {
        self.branches
            .iter()
            .filter_map(|(key, branch)| (key.thread == thread).then_some(branch))
            .collect()
    }

    /// Step summary for a thread-local step id.
    pub fn step(&self, thread: &str, step_id: &TimelineStepId) -> Option<&TimelineStepSummary> {
        self.steps
            .get(&TimelineStepKey::new(thread, step_id.clone()))
    }

    /// Step summaries for a thread in deterministic key order.
    pub fn steps_for_thread(&self, thread: &str) -> Vec<&TimelineStepSummary> {
        self.steps
            .iter()
            .filter_map(|(key, step)| (key.thread == thread).then_some(step))
            .collect()
    }

    /// Ordered step summaries for a branch.
    pub fn list_branch_steps(
        &self,
        thread: &str,
        branch_id: &TimelineBranchId,
    ) -> Vec<&TimelineStepSummary> {
        let Some(branch) = self.branch(thread, branch_id) else {
            return Vec::new();
        };
        branch
            .steps
            .iter()
            .filter_map(|step_id| self.step(thread, step_id))
            .collect()
    }

    /// Find a thread-local step id by native tool-call identity.
    pub fn find_step_id_by_native_call(
        &self,
        thread: &str,
        native: &TimelineNativeToolKey,
    ) -> Option<&TimelineStepId> {
        self.native_index
            .get(&TimelineNativeIndexKey {
                thread: thread.to_string(),
                native: native.clone(),
            })
            .map(|key| &key.step_id)
    }

    /// Find a step summary by native tool-call identity.
    pub fn find_step_by_native_call(
        &self,
        thread: &str,
        native: &TimelineNativeToolKey,
    ) -> Option<&TimelineStepSummary> {
        let step_id = self.find_step_id_by_native_call(thread, native)?;
        self.step(thread, step_id)
    }

    /// Resolve a seek target for a specific step.
    pub fn resolve_seek_target(
        &self,
        thread: &str,
        step_id: &TimelineStepId,
    ) -> Option<TimelineSeekTarget> {
        let step = self.step(thread, step_id)?;
        Some(TimelineSeekTarget {
            thread: thread.to_string(),
            branch_id: step.branch_id.clone(),
            step_id: Some(step.step_id.clone()),
            state: step.cursor_state()?,
        })
    }

    /// Resolve a seek target by native tool-call identity.
    pub fn resolve_seek_to_native_call(
        &self,
        thread: &str,
        native: &TimelineNativeToolKey,
    ) -> Option<TimelineSeekTarget> {
        let step_id = self.find_step_id_by_native_call(thread, native)?;
        self.resolve_seek_target(thread, step_id)
    }

    /// Resolve the target for an undo from the current cursor.
    pub fn resolve_undo_target(&self, thread: &str) -> Option<TimelineSeekTarget> {
        let status = self.status(thread)?;
        let branch_id = status.current_branch_id.as_ref()?;
        let current_step_id = status.current_step_id.as_ref()?;
        let branch = self.branch(thread, branch_id)?;
        let position = branch
            .steps
            .iter()
            .position(|step_id| step_id == current_step_id)?;

        if position == 0 {
            let current_step = self.step(thread, current_step_id)?;
            let state = branch
                .forked_from_state
                .or(current_step.before_state)
                .or(status.current_state)?;
            return Some(TimelineSeekTarget {
                thread: thread.to_string(),
                branch_id: branch_id.clone(),
                step_id: branch.forked_from_step_id.clone(),
                state,
            });
        }

        let previous_step_id = branch.steps.get(position - 1)?;
        self.resolve_seek_target(thread, previous_step_id)
    }

    /// Resolve the target for a redo from the current cursor.
    pub fn resolve_redo_target(&self, thread: &str) -> Option<TimelineSeekTarget> {
        let status = self.status(thread)?;
        let branch_id = status.current_branch_id.as_ref()?;
        let branch = self.branch(thread, branch_id)?;
        let next_position = status
            .current_step_id
            .as_ref()
            .and_then(|current| branch.steps.iter().position(|step_id| step_id == current))
            .map_or(0, |position| position + 1);

        let next_step_id = branch.steps.get(next_position)?;
        self.resolve_seek_target(thread, next_step_id)
    }

    fn apply_operation(&mut self, id: TimelineOperationId, envelope: TimelineOperationEnvelope) {
        match envelope.body {
            TimelineOperationBodyV1::BranchCreated(body) => {
                self.apply_branch_created(id, body, envelope.labels);
            }
            TimelineOperationBodyV1::ToolCallStarted(body) => {
                let thread = body.thread.clone();
                let branch_id = body.branch_id.clone();
                let step_id = body.step_id.clone();
                let before_state = body.before_state;
                let native_key = TimelineNativeToolKey::from(&body.native);

                self.ensure_branch(&thread, branch_id.clone());
                self.add_branch_step(&thread, &branch_id, step_id.clone());

                let step = self.ensure_step(&thread, step_id.clone(), branch_id.clone());
                if step.parent_step_id.is_none() {
                    step.parent_step_id = body.parent_step_id;
                }
                if step.native.is_none() {
                    step.native = Some(body.native);
                }
                if step.tool_name.is_none() {
                    step.tool_name = Some(body.tool_name);
                }
                step.before_state = step.before_state.or(Some(before_state));
                merge_payload_metadata(step, body.payload);
                push_unique_operation(&mut step.operation_ids, id);
                merge_labels(&mut step.labels, envelope.labels);
                step.started_at_ms = step.started_at_ms.or(Some(body.started_at_ms));

                self.native_index.insert(
                    TimelineNativeIndexKey {
                        thread: thread.clone(),
                        native: native_key,
                    },
                    TimelineStepKey::new(thread.clone(), step_id.clone()),
                );
                self.set_thread_cursor(
                    thread,
                    Some(branch_id),
                    Some(step_id),
                    Some(before_state),
                    id,
                );
            }
            TimelineOperationBodyV1::ToolCallFinished(body) => {
                let thread = body.thread.clone();
                let branch_id = body.branch_id.clone();
                let step_id = body.step_id.clone();
                let after_state = body.after_state;
                let native_key = TimelineNativeToolKey::from(&body.native);

                self.ensure_branch(&thread, branch_id.clone());
                self.add_branch_step(&thread, &branch_id, step_id.clone());

                let step = self.ensure_step(&thread, step_id.clone(), branch_id.clone());
                if step.native.is_none() {
                    step.native = Some(body.native);
                }
                step.before_state = step.before_state.or(Some(body.before_state));
                step.after_state = Some(body.after_state);
                step.capture_state = body.capture_state;
                step.capture_oplog_batch_id = body.capture_oplog_batch_id;
                step.changed = Some(body.changed);
                step.status = Some(body.status);
                merge_payload_metadata(step, body.payload);
                merge_touched_paths(&mut step.touched_paths, body.touched_paths);
                push_unique_operation(&mut step.operation_ids, id);
                merge_labels(&mut step.labels, envelope.labels);
                step.finished_at_ms = step.finished_at_ms.or(Some(body.finished_at_ms));

                self.native_index.insert(
                    TimelineNativeIndexKey {
                        thread: thread.clone(),
                        native: native_key,
                    },
                    TimelineStepKey::new(thread.clone(), step_id.clone()),
                );
                self.set_thread_cursor(
                    thread,
                    Some(branch_id),
                    Some(step_id),
                    Some(after_state),
                    id,
                );
            }
            TimelineOperationBodyV1::CursorMoved(body) => {
                self.ensure_branch(&body.thread, body.branch_id.clone());
                self.set_thread_cursor(
                    body.thread,
                    Some(body.branch_id),
                    body.to_step_id,
                    Some(body.to_state),
                    id,
                );
            }
        }
    }

    fn apply_branch_created(
        &mut self,
        id: TimelineOperationId,
        body: objects::object::BranchCreatedV1,
        _labels: Vec<TimelineLabel>,
    ) {
        let key = TimelineBranchKey::new(body.thread.clone(), body.branch_id.clone());
        let branch = self.branches.entry(key).or_insert_with(|| {
            TimelineBranchSummary::implicit(&body.thread, body.branch_id.clone())
        });
        branch.parent_branch_id = body.parent_branch_id;
        branch.forked_from_step_id = body.from_step_id.clone();
        branch.forked_from_state = Some(body.from_state);
        branch.reason = Some(body.reason);
        branch.created_at_ms = Some(body.created_at_ms);
        push_unique_operation(&mut branch.operation_ids, id);
        self.set_thread_cursor(
            body.thread,
            Some(body.branch_id),
            body.from_step_id,
            Some(body.from_state),
            id,
        );
    }

    fn ensure_branch(&mut self, thread: &str, branch_id: TimelineBranchId) {
        let key = TimelineBranchKey::new(thread, branch_id.clone());
        self.branches
            .entry(key)
            .or_insert_with(|| TimelineBranchSummary::implicit(thread, branch_id));
    }

    fn ensure_step(
        &mut self,
        thread: &str,
        step_id: TimelineStepId,
        branch_id: TimelineBranchId,
    ) -> &mut TimelineStepSummary {
        let key = TimelineStepKey::new(thread, step_id.clone());
        self.steps
            .entry(key)
            .or_insert_with(|| TimelineStepSummary::new(thread, step_id, branch_id))
    }

    fn add_branch_step(
        &mut self,
        thread: &str,
        branch_id: &TimelineBranchId,
        step_id: TimelineStepId,
    ) {
        self.ensure_branch(thread, branch_id.clone());
        if let Some(branch) = self
            .branches
            .get_mut(&TimelineBranchKey::new(thread, branch_id.clone()))
            && !branch.steps.contains(&step_id)
        {
            branch.steps.push(step_id);
        }
    }

    fn set_thread_cursor(
        &mut self,
        thread: String,
        branch_id: Option<TimelineBranchId>,
        step_id: Option<TimelineStepId>,
        state: Option<ChangeId>,
        operation_id: TimelineOperationId,
    ) {
        let status = self
            .threads
            .entry(thread.clone())
            .or_insert_with(|| TimelineThreadStatus::new(thread));
        status.current_branch_id = branch_id;
        status.current_step_id = step_id;
        status.current_state = state;
        status.last_operation_id = Some(operation_id);
    }
}

impl TimelineStore {
    /// Append a canonical cursor movement operation. This does not reset the worktree.
    pub fn record_cursor_move(
        &self,
        movement: TimelineCursorMoveRecord,
    ) -> Result<TimelineOperationId> {
        let envelope = TimelineOperationEnvelope::new(
            TimelineOperationBodyV1::CursorMoved(CursorMovedV1 {
                thread: movement.thread,
                branch_id: movement.branch_id,
                from_step_id: movement.from_step_id,
                to_step_id: movement.to_step_id,
                from_state: movement.from_state,
                to_state: movement.to_state,
                reason: movement.reason,
                moved_at_ms: movement.moved_at_ms,
            }),
            movement.labels,
        );
        self.write_operation(&envelope)
    }
}

#[derive(Clone)]
struct TimelineOperationRecord {
    id: TimelineOperationId,
    envelope: TimelineOperationEnvelope,
}

fn read_timeline_operation_records(store: &TimelineStore) -> Result<Vec<TimelineOperationRecord>> {
    let mut paths = Vec::new();
    collect_operation_paths(&store.root().join("ops"), &mut paths)?;
    paths.sort();

    let mut records = Vec::with_capacity(paths.len());
    for path in paths {
        let bytes = fs::read(&path)?;
        let id = TimelineOperationId::for_bytes(&bytes);
        let envelope = TimelineOperationEnvelope::decode(&bytes).map_err(|err| {
            HeddleError::InvalidObject(format!(
                "decode timeline operation '{}': {err}",
                path.display()
            ))
        })?;
        records.push(TimelineOperationRecord { id, envelope });
    }
    Ok(records)
}

fn collect_operation_paths(dir: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                let file_type = entry.file_type()?;
                if file_type.is_dir() {
                    collect_operation_paths(&path, paths)?;
                } else if file_type.is_file()
                    && path.extension().is_some_and(|ext| ext == "msgpack")
                {
                    paths.push(path);
                }
            }
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn operation_timestamp(envelope: &TimelineOperationEnvelope) -> i64 {
    match &envelope.body {
        TimelineOperationBodyV1::ToolCallStarted(body) => body.started_at_ms,
        TimelineOperationBodyV1::ToolCallFinished(body) => body.finished_at_ms,
        TimelineOperationBodyV1::CursorMoved(body) => body.moved_at_ms,
        TimelineOperationBodyV1::BranchCreated(body) => body.created_at_ms,
    }
}

fn operation_kind_order(kind: TimelineOperationKind) -> u8 {
    match kind {
        TimelineOperationKind::BranchCreated => 0,
        TimelineOperationKind::ToolCallStarted => 1,
        TimelineOperationKind::ToolCallFinished => 2,
        TimelineOperationKind::CursorMoved => 3,
    }
}

fn merge_payload_metadata(
    step: &mut TimelineStepSummary,
    payload: Option<objects::object::TimelineToolPayloadMetadata>,
) {
    let Some(payload) = payload else {
        return;
    };
    if step.payload_summary.is_none() {
        step.payload_summary = payload.summary;
    }
    if step.payload_hash.is_none() {
        step.payload_hash = payload.hash;
    }
}

fn merge_labels(target: &mut Vec<TimelineLabel>, labels: Vec<TimelineLabel>) {
    for label in labels {
        if !target.contains(&label) {
            target.push(label);
        }
    }
}

fn merge_touched_paths(target: &mut Vec<String>, paths: Vec<String>) {
    for path in paths {
        if !target.contains(&path) {
            target.push(path);
        }
    }
}

fn push_unique_operation(target: &mut Vec<TimelineOperationId>, id: TimelineOperationId) {
    if !target.contains(&id) {
        target.push(id);
    }
}

#[cfg(test)]
mod tests {
    use objects::object::{
        BranchCreatedV1, NativeToolCallRefV1, TimelineToolPayloadMetadata, ToolCallFinishedV1,
        ToolCallStartedV1,
    };
    use tempfile::TempDir;

    use super::*;

    fn state(byte: u8) -> ChangeId {
        ChangeId::from_bytes([byte; 16])
    }

    fn branch(id: &str) -> TimelineBranchId {
        TimelineBranchId::new(id)
    }

    fn step(id: &str) -> TimelineStepId {
        TimelineStepId::new(id)
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

    fn temp_store() -> (TempDir, TimelineStore) {
        let temp = TempDir::new().unwrap();
        let store = TimelineStore::open(temp.path().join(".heddle")).unwrap();
        (temp, store)
    }

    fn write_main_two_steps(store: &TimelineStore) {
        store
            .write_operation(&TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::BranchCreated(BranchCreatedV1 {
                    thread: "main".to_string(),
                    branch_id: branch("tlb-main"),
                    parent_branch_id: None,
                    from_step_id: None,
                    from_state: state(0),
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
                    before_state: state(0),
                    payload: Some(TimelineToolPayloadMetadata {
                        summary: Some("created src/lib.rs".to_string()),
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
                    before_state: state(0),
                    after_state: state(1),
                    capture_state: Some(state(1)),
                    capture_oplog_batch_id: Some(7),
                    changed: true,
                    touched_paths: vec!["src/lib.rs".to_string()],
                    payload: Some(TimelineToolPayloadMetadata {
                        summary: Some("created src/lib.rs".to_string()),
                        hash: Some(ContentHash::compute_typed(
                            "timeline-tool-payload",
                            b"call-1",
                        )),
                    }),
                    finished_at_ms: 3,
                }),
                vec![TimelineLabel::RepoReversible],
            ))
            .unwrap();

        store
            .write_operation(&TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::ToolCallStarted(ToolCallStartedV1 {
                    thread: "main".to_string(),
                    step_id: step("tls-two"),
                    branch_id: branch("tlb-main"),
                    parent_step_id: Some(step("tls-one")),
                    native: native("call-2"),
                    tool_name: "edit".to_string(),
                    before_state: state(1),
                    payload: None,
                    started_at_ms: 4,
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
                    before_state: state(1),
                    after_state: state(2),
                    capture_state: Some(state(2)),
                    capture_oplog_batch_id: Some(8),
                    changed: true,
                    touched_paths: vec!["src/lib.rs".to_string(), "README.md".to_string()],
                    payload: None,
                    finished_at_ms: 5,
                }),
                vec![TimelineLabel::RepoReversible],
            ))
            .unwrap();
    }

    #[test]
    fn timeline_view_rebuilds_branch_order_status_and_native_index() {
        let (_temp, store) = temp_store();
        write_main_two_steps(&store);

        let view = TimelineView::rebuild(&store).unwrap();

        assert_eq!(view.operation_ids().len(), 5);
        let status = view.status("main").unwrap();
        assert_eq!(status.current_branch_id, Some(branch("tlb-main")));
        assert_eq!(status.current_step_id, Some(step("tls-two")));
        assert_eq!(status.current_state, Some(state(2)));

        let branch_steps = view.list_branch_steps("main", &branch("tlb-main"));
        let ids = branch_steps
            .iter()
            .map(|summary| summary.step_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["tls-one", "tls-two"]);

        let first = view
            .find_step_by_native_call("main", &native_key("call-1"))
            .unwrap();
        assert_eq!(first.step_id, step("tls-one"));
        assert_eq!(first.before_state, Some(state(0)));
        assert_eq!(first.after_state, Some(state(1)));
        assert_eq!(first.capture_state, Some(state(1)));
        assert_eq!(first.changed, Some(true));
        assert_eq!(first.status, Some(TimelineToolCallStatus::Succeeded));
        assert_eq!(first.touched_paths, vec!["src/lib.rs"]);
        assert_eq!(first.payload_summary.as_deref(), Some("created src/lib.rs"));
        assert_eq!(first.operation_ids.len(), 2);
        assert_eq!(first.labels, vec![TimelineLabel::RepoReversible]);
    }

    #[test]
    fn timeline_view_resolves_seek_undo_and_redo_targets() {
        let (_temp, store) = temp_store();
        write_main_two_steps(&store);

        let view = TimelineView::rebuild(&store).unwrap();
        assert_eq!(
            view.resolve_seek_to_native_call("main", &native_key("call-1")),
            Some(TimelineSeekTarget {
                thread: "main".to_string(),
                branch_id: branch("tlb-main"),
                step_id: Some(step("tls-one")),
                state: state(1),
            })
        );

        store
            .record_cursor_move(TimelineCursorMoveRecord {
                thread: "main".to_string(),
                branch_id: branch("tlb-main"),
                from_step_id: Some(step("tls-two")),
                to_step_id: Some(step("tls-one")),
                from_state: state(2),
                to_state: state(1),
                reason: TimelineCursorMoveReason::Undo,
                moved_at_ms: 6,
                labels: Vec::new(),
            })
            .unwrap();

        let view = TimelineView::rebuild(&store).unwrap();
        let status = view.status("main").unwrap();
        assert_eq!(status.current_step_id, Some(step("tls-one")));
        assert_eq!(status.current_state, Some(state(1)));

        assert_eq!(
            view.resolve_undo_target("main"),
            Some(TimelineSeekTarget {
                thread: "main".to_string(),
                branch_id: branch("tlb-main"),
                step_id: None,
                state: state(0),
            })
        );
        assert_eq!(
            view.resolve_redo_target("main"),
            Some(TimelineSeekTarget {
                thread: "main".to_string(),
                branch_id: branch("tlb-main"),
                step_id: Some(step("tls-two")),
                state: state(2),
            })
        );
    }

    #[test]
    fn timeline_view_keeps_fork_branch_order_separate() {
        let (_temp, store) = temp_store();
        write_main_two_steps(&store);

        store
            .write_operation(&TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::BranchCreated(BranchCreatedV1 {
                    thread: "main".to_string(),
                    branch_id: branch("tlb-child"),
                    parent_branch_id: Some(branch("tlb-main")),
                    from_step_id: Some(step("tls-one")),
                    from_state: state(1),
                    reason: TimelineBranchReason::FanOut,
                    created_at_ms: 6,
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
                    native: native("call-child"),
                    status: TimelineToolCallStatus::Succeeded,
                    before_state: state(1),
                    after_state: state(3),
                    capture_state: Some(state(3)),
                    capture_oplog_batch_id: Some(9),
                    changed: true,
                    touched_paths: vec!["src/child.rs".to_string()],
                    payload: None,
                    finished_at_ms: 7,
                }),
                vec![TimelineLabel::RepoReversible],
            ))
            .unwrap();

        let view = TimelineView::rebuild(&store).unwrap();
        let main_steps = view
            .list_branch_steps("main", &branch("tlb-main"))
            .into_iter()
            .map(|summary| summary.step_id.as_str())
            .collect::<Vec<_>>();
        let child_steps = view
            .list_branch_steps("main", &branch("tlb-child"))
            .into_iter()
            .map(|summary| summary.step_id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(main_steps, vec!["tls-one", "tls-two"]);
        assert_eq!(child_steps, vec!["tls-child"]);

        let child = view.branch("main", &branch("tlb-child")).unwrap();
        assert_eq!(child.parent_branch_id, Some(branch("tlb-main")));
        assert_eq!(child.forked_from_step_id, Some(step("tls-one")));
        assert_eq!(child.forked_from_state, Some(state(1)));

        let status = view.status("main").unwrap();
        assert_eq!(status.current_branch_id, Some(branch("tlb-child")));
        assert_eq!(status.current_step_id, Some(step("tls-child")));
        assert_eq!(status.current_state, Some(state(3)));
    }
}
