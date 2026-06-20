// SPDX-License-Identifier: Apache-2.0
//! Operation-log backend traits.
//!
//! [`LocalOpLogBackend`] is the synchronous local capability used by the
//! file-backed CLI/repo path.

use objects::{
    error::{HeddleError, Result, StorageErrorKind},
    object::{Scope, TransactionId},
};

use super::oplog_types::{
    ConditionalCommitOutcome, IsolationPrecondition, OpBatch, OpEntry, OpRecord,
    is_transaction_commit_for,
};

fn unsupported_local_method(method: &str) -> HeddleError {
    HeddleError::storage(
        StorageErrorKind::Unsupported,
        format!("{method} is only available on local oplog backends"),
    )
}

/// Local synchronous backend interface for the operation log.
pub trait LocalOpLogBackend: Send + Sync {
    /// Append a batch of operations atomically. Returns the assigned IDs.
    fn record_batch(&self, operations: Vec<OpRecord>) -> Result<Vec<u64>> {
        self.record_batch_scoped(operations, None)
    }

    fn record_batch_scoped(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
    ) -> Result<Vec<u64>> {
        let _ = (operations, scope);
        Err(unsupported_local_method("record_batch_scoped"))
    }

    /// Atomic dedup+append for transaction-scoped batches.
    ///
    /// The default implementation is non-atomic. Local backends that can
    /// serialize the scan+append at their write authority should override it.
    fn record_batch_scoped_if_no_transaction(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
        transaction_id: &TransactionId,
        recent_window: usize,
    ) -> Result<Option<Vec<u64>>> {
        let recent = self.recent_batches_scoped(recent_window, scope)?;
        if recent.iter().any(|batch| {
            batch
                .entries
                .iter()
                .any(|entry| is_transaction_commit_for(&entry.operation, transaction_id))
        }) {
            return Ok(None);
        }
        let ids = self.record_batch_scoped(operations, scope)?;
        Ok(Some(ids))
    }

    fn record_batch_exactly_once_if_unchanged(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
        transaction_id: &TransactionId,
        precondition: &IsolationPrecondition,
    ) -> Result<ConditionalCommitOutcome> {
        let _ = (operations, scope, transaction_id, precondition);
        Err(HeddleError::Config(
            "oplog backend does not support conditional transaction commits".to_string(),
        ))
    }

    fn last(&self) -> Result<Option<OpEntry>> {
        Err(unsupported_local_method("last"))
    }

    fn recent(&self, count: usize) -> Result<Vec<OpEntry>> {
        let _ = count;
        Err(unsupported_local_method("recent"))
    }

    fn recent_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.recent_batches_scoped(count, None)
    }

    fn recent_batches_scoped(&self, count: usize, scope: Option<&Scope>) -> Result<Vec<OpBatch>> {
        let _ = (count, scope);
        Err(unsupported_local_method("recent_batches_scoped"))
    }

    fn undo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.undo_batches_scoped(count, None)
    }

    fn undo_batches_scoped(&self, count: usize, scope: Option<&Scope>) -> Result<Vec<OpBatch>> {
        let _ = (count, scope);
        Err(unsupported_local_method("undo_batches_scoped"))
    }

    fn redo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.redo_batches_scoped(count, None)
    }

    fn redo_batches_scoped(&self, count: usize, scope: Option<&Scope>) -> Result<Vec<OpBatch>> {
        let _ = (count, scope);
        Err(unsupported_local_method("redo_batches_scoped"))
    }

    fn mark_batch_undone(&self, batch: &OpBatch) -> Result<OpBatch> {
        let _ = batch;
        Err(unsupported_local_method("mark_batch_undone"))
    }

    fn mark_batch_redone(&self, batch: &OpBatch) -> Result<OpBatch> {
        let _ = batch;
        Err(unsupported_local_method("mark_batch_redone"))
    }

    fn coalesce_batches(&self, primary_batch_id: u64, secondary_batch_id: u64) -> Result<OpBatch> {
        let _ = (primary_batch_id, secondary_batch_id);
        Err(HeddleError::Config(
            "oplog backend does not support batch coalescing".to_string(),
        ))
    }
}
