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

    /// Record a thread creation.
    pub fn record_thread_create(
        &self,
        name: &str,
        state: &ChangeId,
        scope: Option<&str>,
    ) -> Result<u64> {
        self.record_single_scoped(
            OpRecord::ThreadCreate {
                name: name.to_string(),
                state: *state,
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

    /// Record a thread rename as a batch of operations.
    pub fn record_thread_rename(
        &self,
        old_name: &str,
        new_name: &str,
        state: &ChangeId,
        scope: Option<&str>,
    ) -> Result<Vec<u64>> {
        self.record_batch_scoped(
            vec![
                OpRecord::ThreadCreate {
                    name: new_name.to_string(),
                    state: *state,
                },
                OpRecord::ThreadDelete {
                    name: old_name.to_string(),
                    state: *state,
                },
            ],
            scope,
        )
    }

    /// Record a fork operation.
    pub fn record_fork(&self, from: &ChangeId, new_state: &ChangeId) -> Result<u64> {
        self.record_single(OpRecord::Fork {
            from: *from,
            new_state: *new_state,
        })
    }

    /// Record a collapse operation.
    pub fn record_collapse(&self, sources: &[ChangeId], result: &ChangeId) -> Result<u64> {
        self.record_single(OpRecord::Collapse {
            sources: sources.to_vec(),
            result: *result,
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
}