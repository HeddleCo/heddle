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
    fn record_redact(
        &self,
        redaction_id: &ContentHash,
        blob: &ContentHash,
        state: &ChangeId,
        path: &str,
    ) -> Result<u64> {
        let ids = self.record_batch(vec![OpRecord::Redact {
            redaction_id: *redaction_id,
            blob: *blob,
            state: *state,
            path: path.to_string(),
        }])?;
        Ok(ids[0])
    }

    /// Record a purge — the underlying blob bytes were physically
    /// removed from local storage. The associated `Redaction` record
    /// stays in place.
    fn record_purge(&self, redaction_id: &ContentHash, blob: &ContentHash) -> Result<u64> {
        let ids = self.record_batch(vec![OpRecord::Purge {
            redaction_id: *redaction_id,
            blob: *blob,
        }])?;
        Ok(ids[0])
    }
}