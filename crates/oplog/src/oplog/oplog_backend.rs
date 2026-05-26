// SPDX-License-Identifier: Apache-2.0
//! Abstract backend trait for the operation log.
//!
//! The local CLI uses `OpLog` (disk-based). The server uses `PgOpLogBackend`
//! (Postgres-backed append-only table). Both implement this trait.

use objects::{
    error::Result,
    object::{ChangeId, ContentHash, MarkerName, ThreadName},
};

use super::oplog_types::{OpBatch, OpEntry, OpRecord};

/// Backend-agnostic interface for the operation log.
pub trait OpLogBackend: Send + Sync {
    /// Append a batch of operations atomically. Returns the assigned IDs.
    fn record_batch(&self, operations: Vec<OpRecord>) -> Result<Vec<u64>> {
        self.record_batch_scoped(operations, None)
    }
    fn record_batch_scoped(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&str>,
    ) -> Result<Vec<u64>>;

    /// Atomic dedup+append for transaction-scoped batches: scan the
    /// most recent `recent_window` batches under the same write lock
    /// used by [`OpLogBackend::record_batch_scoped`] for an
    /// `OpRecord::TransactionCommit { transaction_id: id, .. }` marker.
    /// On a hit, return `Ok(None)` (batch was already committed by a
    /// prior call). Otherwise append `operations` and return
    /// `Ok(Some(ids))`.
    ///
    /// The default implementation is non-atomic — it calls the two
    /// existing methods in sequence and is therefore subject to a
    /// check/append race under concurrent writers. The local
    /// file-backed `OpLog` overrides it to hold the write lock across
    /// the scan and the append (heddle#198 r4 / Codex PR #218 P2).
    fn record_batch_scoped_if_no_transaction(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&str>,
        transaction_id: &str,
        recent_window: usize,
    ) -> Result<Option<Vec<u64>>> {
        let recent = self.recent_batches_scoped(recent_window, scope)?;
        if recent.iter().any(|batch| {
            batch.entries.iter().any(|entry| {
                matches!(
                    &entry.operation,
                    OpRecord::TransactionCommit { transaction_id: id, .. }
                        if id == transaction_id
                )
            })
        }) {
            return Ok(None);
        }
        let ids = self.record_batch_scoped(operations, scope)?;
        Ok(Some(ids))
    }

    fn last(&self) -> Result<Option<OpEntry>>;
    fn recent(&self, count: usize) -> Result<Vec<OpEntry>>;
    fn recent_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.recent_batches_scoped(count, None)
    }
    fn recent_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>>;

    fn undo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.undo_batches_scoped(count, None)
    }
    fn undo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>>;
    fn redo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.redo_batches_scoped(count, None)
    }
    fn redo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>>;
    fn mark_batch_undone(&self, batch: &OpBatch) -> Result<OpBatch>;
    fn mark_batch_redone(&self, batch: &OpBatch) -> Result<OpBatch>;

    /// Coalesce two existing batches into one logical undo/redo unit.
    ///
    /// Implementations should preserve entry IDs and chronological entry
    /// order, rewriting only batch metadata. Backends that cannot rewrite
    /// local batch metadata may keep the default fail-closed behavior.
    fn coalesce_batches(&self, primary_batch_id: u64, secondary_batch_id: u64) -> Result<OpBatch> {
        let _ = (primary_batch_id, secondary_batch_id);
        Err(objects::error::HeddleError::Config(
            "oplog backend does not support batch coalescing".to_string(),
        ))
    }

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

    /// Record a thread creation.
    ///
    /// `manager_snapshot` is opaque rmp-serde bytes of the matching
    /// `Thread` record body (see `repo::ThreadManager::snapshot_thread_record`)
    /// so redo can recreate the record after undo destroyed it. Pass
    /// `None` for callsites that don't write a `ThreadManager` record
    /// alongside the create (rename batch's new-name arm, ingest,
    /// harness/agent stubs that write the record later or not at all).
    ///
    /// Always emits `OpRecord::ThreadCreateV2`. V1
    /// (`OpRecord::ThreadCreate`) is retained as read-back-only for
    /// legacy oplog entries written before heddle#23 r2.
    fn record_thread_create(
        &self,
        name: &ThreadName,
        state: &ChangeId,
        manager_snapshot: Option<Vec<u8>>,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::ThreadCreateV2 {
                name: name.to_string(),
                state: *state,
                manager_snapshot,
            }],
            scope,
        )?;
        Ok(ids[0])
    }

    fn record_thread_delete(
        &self,
        name: &ThreadName,
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
        old_name: &ThreadName,
        new_name: &ThreadName,
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

    fn record_marker_create(&self, name: &MarkerName, state: &ChangeId) -> Result<u64> {
        let ids = self.record_batch(vec![OpRecord::MarkerCreate {
            name: name.to_string(),
            state: *state,
        }])?;
        Ok(ids[0])
    }

    fn record_marker_delete(&self, name: &MarkerName, state: &ChangeId) -> Result<u64> {
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

    /// Record a fast-forward merge. `pre_target_id` is the target's tip
    /// before the FF (undo target); `post_target_id` is the target's tip
    /// after the FF (redo target). Both ends of the FF are recorded so
    /// neither inverse has to re-resolve `source_thread → tip` at apply
    /// time — closes heddle#99 r1 (stranded ref on undo) and r2 (redo
    /// non-determinism).
    ///
    /// Always emits the V2 variant. V1 (`OpRecord::FastForward`) is
    /// retained as read-back-only.
    fn record_fast_forward(
        &self,
        source_thread: &ThreadName,
        target_thread: &ThreadName,
        pre_target_id: &ChangeId,
        post_target_id: &ChangeId,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::FastForwardV2 {
                source_thread: source_thread.to_string(),
                target_thread: target_thread.to_string(),
                pre_target_id: *pre_target_id,
                post_target_id: *post_target_id,
            }],
            scope,
        )?;
        Ok(ids[0])
    }
}
