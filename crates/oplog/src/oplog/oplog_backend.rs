// SPDX-License-Identifier: Apache-2.0
//! Abstract backend trait for the operation log.
//!
//! The local CLI uses `OpLog` (disk-based). The server uses `PgOpLogBackend`
//! (Postgres-backed append-only table). Both implement this trait.

use objects::{
    error::Result,
    object::{ChangeId, ContentHash},
};

use super::oplog_types::{OpBatch, OpEntry, OpRecord};

/// Backend-agnostic interface for the operation log.
pub trait OpLogBackend: Send + Sync {
    /// Append a batch of operations atomically. Returns the assigned IDs.
    fn record_batch(&self, operations: Vec<OpRecord>) -> Result<Vec<u64>>;
    fn record_batch_scoped(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&str>,
    ) -> Result<Vec<u64>>;

    fn last(&self) -> Result<Option<OpEntry>>;
    fn recent(&self, count: usize) -> Result<Vec<OpEntry>>;
    fn recent_batches(&self, count: usize) -> Result<Vec<OpBatch>>;
    fn recent_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>>;

    fn undo_batches(&self, count: usize) -> Result<Vec<OpBatch>>;
    fn undo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>>;
    fn redo_batches(&self, count: usize) -> Result<Vec<OpBatch>>;
    fn redo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>>;
    fn mark_batch_undone(&self, batch: &OpBatch) -> Result<OpBatch>;
    fn mark_batch_redone(&self, batch: &OpBatch) -> Result<OpBatch>;

    fn record_snapshot(
        &self,
        new_state: &ChangeId,
        prev_head: Option<&ChangeId>,
        thread: Option<&str>,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::Snapshot {
                new_state: *new_state,
                prev_head: prev_head.copied(),
                thread: thread.map(str::to_string),
            }],
            scope,
        )?;
        Ok(ids[0])
    }

    fn record_goto(
        &self,
        target: &ChangeId,
        prev_head: Option<&ChangeId>,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::Goto {
                target: *target,
                prev_head: prev_head.copied(),
            }],
            scope,
        )?;
        Ok(ids[0])
    }

    fn record_thread_create(
        &self,
        name: &str,
        state: &ChangeId,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::ThreadCreate {
                name: name.to_string(),
                state: *state,
            }],
            scope,
        )?;
        Ok(ids[0])
    }

    fn record_thread_delete(
        &self,
        name: &str,
        state: &ChangeId,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::ThreadDelete {
                name: name.to_string(),
                state: *state,
            }],
            scope,
        )?;
        Ok(ids[0])
    }

    fn record_thread_rename(
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

    fn record_fork(&self, from: &ChangeId, new_state: &ChangeId) -> Result<u64> {
        let ids = self.record_batch(vec![OpRecord::Fork {
            from: *from,
            new_state: *new_state,
        }])?;
        Ok(ids[0])
    }

    fn record_collapse(&self, sources: &[ChangeId], result: &ChangeId) -> Result<u64> {
        let ids = self.record_batch(vec![OpRecord::Collapse {
            sources: sources.to_vec(),
            result: *result,
        }])?;
        Ok(ids[0])
    }

    fn record_marker_create(&self, name: &str, state: &ChangeId) -> Result<u64> {
        let ids = self.record_batch(vec![OpRecord::MarkerCreate {
            name: name.to_string(),
            state: *state,
        }])?;
        Ok(ids[0])
    }

    fn record_marker_delete(&self, name: &str, state: &ChangeId) -> Result<u64> {
        let ids = self.record_batch(vec![OpRecord::MarkerDelete {
            name: name.to_string(),
            state: *state,
        }])?;
        Ok(ids[0])
    }

    /// Record a redaction declaration. The blob bytes stay on disk —
    /// `Purge` is the separate, irreversible step that removes them.
    ///
    /// `scope` carries the repo's `op_scope()` so the CLI's scoped
    /// undo can reach this batch. Without it the entry is recorded
    /// with `scope: None` and `undo_batches_scoped` (the only path
    /// `heddle undo` consults) silently filters it out — the silent
    /// no-op fixed under heddle#98.
    fn record_redact(
        &self,
        redaction_id: &ContentHash,
        blob: &ContentHash,
        state: &ChangeId,
        path: &str,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::Redact {
                redaction_id: *redaction_id,
                blob: *blob,
                state: *state,
                path: path.to_string(),
            }],
            scope,
        )?;
        Ok(ids[0])
    }

    /// Record a purge — the underlying blob bytes were physically
    /// removed from local storage. The associated `Redaction` record
    /// stays in place.
    ///
    /// Scoped for the same reason as `record_redact`: the CLI's
    /// `heddle undo` only sees scoped batches, and the Purge inverse
    /// (refusal) needs to see the entry to surface the irreversibility
    /// message instead of silently skipping it.
    fn record_purge(
        &self,
        redaction_id: &ContentHash,
        blob: &ContentHash,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::Purge {
                redaction_id: *redaction_id,
                blob: *blob,
            }],
            scope,
        )?;
        Ok(ids[0])
    }

    /// Record a fast-forward merge. `pre_target_id` captures the target
    /// thread's tip before the FF so undo can restore both HEAD and the
    /// target thread ref. Recording an FF merge as a plain `Goto`
    /// stranded the target thread ref at the FF target on undo — the
    /// bug heddle#99 closes.
    fn record_fast_forward(
        &self,
        source_thread: &str,
        target_thread: &str,
        pre_target_id: &ChangeId,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::FastForward {
                source_thread: source_thread.to_string(),
                target_thread: target_thread.to_string(),
                pre_target_id: *pre_target_id,
            }],
            scope,
        )?;
        Ok(ids[0])
    }
}
