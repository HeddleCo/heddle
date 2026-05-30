// SPDX-License-Identifier: Apache-2.0
//! Recording methods for the OpLog - one method per operation type.

use objects::{
    error::Result,
    object::{ChangeId, ContentHash},
};

use super::{oplog_core::OpLog, oplog_types::OpRecord};

impl OpLog {
    /// Record a snapshot operation.
    pub fn record_snapshot(
        &self,
        new_state: &ChangeId,
        prev_head: Option<&ChangeId>,
        thread: Option<&str>,
        scope: Option<&str>,
    ) -> Result<u64> {
        self.record_single_scoped(
            OpRecord::Snapshot {
                new_state: *new_state,
                prev_head: prev_head.copied(),
                thread: thread.map(str::to_string),
            },
            scope,
        )
    }

    /// Record a goto operation.
    pub fn record_goto(
        &self,
        target: &ChangeId,
        prev_head: Option<&ChangeId>,
        scope: Option<&str>,
    ) -> Result<u64> {
        self.record_single_scoped(
            OpRecord::Goto {
                target: *target,
                prev_head: prev_head.copied(),
            },
            scope,
        )
    }

    /// Record a thread creation. See `OpLogBackend::record_thread_create`
    /// for the contract — `manager_snapshot` carries the rmp-serde bytes
    /// of the matching `Thread` record body so redo can recreate the
    /// record after undo destroyed it (heddle#23 r2). Pass `None` for
    /// callsites that don't write a record alongside the op.
    ///
    /// Always emits `OpRecord::ThreadCreateV2`.
    pub fn record_thread_create(
        &self,
        name: &str,
        state: &ChangeId,
        manager_snapshot: Option<Vec<u8>>,
        scope: Option<&str>,
    ) -> Result<u64> {
        self.record_single_scoped(
            OpRecord::ThreadCreateV2 {
                name: name.to_string(),
                state: *state,
                manager_snapshot,
            },
            scope,
        )
    }

    /// Record a thread deletion.
    pub fn record_thread_delete(
        &self,
        name: &str,
        state: &ChangeId,
        scope: Option<&str>,
    ) -> Result<u64> {
        self.record_single_scoped(
            OpRecord::ThreadDelete {
                name: name.to_string(),
                state: *state,
            },
            scope,
        )
    }

    /// Record a thread rename as a batch of operations. See
    /// `OpLogBackend::record_thread_rename` for why the new-name arm
    /// carries `manager_snapshot: None`.
    pub fn record_thread_rename(
        &self,
        old_name: &str,
        new_name: &str,
        state: &ChangeId,
        scope: Option<&str>,
    ) -> Result<Vec<u64>> {
        self.record_batch_scoped(
            vec![
                OpRecord::ThreadCreateV2 {
                    name: new_name.to_string(),
                    state: *state,
                    manager_snapshot: None,
                },
                OpRecord::ThreadDelete {
                    name: old_name.to_string(),
                    state: *state,
                },
            ],
            scope,
        )
    }

    /// Record a fork operation. `from` is the source state, `new_state`
    /// the fork result; `thread`/`head` name the published ref so
    /// crash-replay can re-materialize it (heddle#330).
    pub fn record_fork(
        &self,
        from: &ChangeId,
        new_state: &ChangeId,
        thread: Option<&str>,
        head: Option<&ChangeId>,
    ) -> Result<u64> {
        self.record_single(OpRecord::Fork {
            from: *from,
            new_state: *new_state,
            thread: thread.map(str::to_string),
            head: head.copied(),
        })
    }

    /// Record a collapse operation. `thread` names the published ref
    /// (`Some` thread name, or `None` for a detached HEAD at `result`).
    pub fn record_collapse(
        &self,
        sources: &[ChangeId],
        result: &ChangeId,
        thread: Option<&str>,
    ) -> Result<u64> {
        self.record_single(OpRecord::Collapse {
            sources: sources.to_vec(),
            result: *result,
            thread: thread.map(str::to_string),
        })
    }

    /// Record a marker creation.
    pub fn record_marker_create(&self, name: &str, state: &ChangeId) -> Result<u64> {
        self.record_single(OpRecord::MarkerCreate {
            name: name.to_string(),
            state: *state,
        })
    }

    /// Record a marker deletion.
    pub fn record_marker_delete(&self, name: &str, state: &ChangeId) -> Result<u64> {
        self.record_single(OpRecord::MarkerDelete {
            name: name.to_string(),
            state: *state,
        })
    }

    /// Record a redaction declaration. See the trait-level doc on
    /// `OpLogBackend::record_redact` for why `scope` is required —
    /// without it the entry slips past the CLI's scoped undo filter.
    pub fn record_redact(
        &self,
        redaction_id: &ContentHash,
        blob: &ContentHash,
        state: &ChangeId,
        path: &str,
        scope: Option<&str>,
    ) -> Result<u64> {
        self.record_single_scoped(
            OpRecord::Redact {
                redaction_id: *redaction_id,
                blob: *blob,
                state: *state,
                path: path.to_string(),
            },
            scope,
        )
    }

    /// Record a purge — the underlying blob bytes were physically removed.
    /// The associated `Redaction` record stays in place; only the bytes
    /// are gone.
    pub fn record_purge(
        &self,
        redaction_id: &ContentHash,
        blob: &ContentHash,
        scope: Option<&str>,
    ) -> Result<u64> {
        self.record_single_scoped(
            OpRecord::Purge {
                redaction_id: *redaction_id,
                blob: *blob,
            },
            scope,
        )
    }

    /// Record a fast-forward merge. `pre_target_id` is the target
    /// thread's tip before the FF (undo target); `post_target_id` is
    /// the target thread's tip after the FF (redo target). Use this
    /// *instead* of `record_goto` when an FF merge moves an attached
    /// thread ref forward — recording the FF as a plain `Goto` strands
    /// the thread ref on undo (heddle#99 r1) and recording the redo
    /// target via name-resolution is non-deterministic (heddle#99 r2).
    ///
    /// Always emits the V2 variant; V1 (`OpRecord::FastForward`) is
    /// retained for read-back compatibility with records written by
    /// the heddle#99 r1 implementation but is no longer recorded.
    pub fn record_fast_forward(
        &self,
        source_thread: &str,
        target_thread: &str,
        pre_target_id: &ChangeId,
        post_target_id: &ChangeId,
        scope: Option<&str>,
    ) -> Result<u64> {
        self.record_single_scoped(
            OpRecord::FastForwardV2 {
                source_thread: source_thread.to_string(),
                target_thread: target_thread.to_string(),
                pre_target_id: *pre_target_id,
                post_target_id: *post_target_id,
            },
            scope,
        )
    }
}
