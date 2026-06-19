// SPDX-License-Identifier: Apache-2.0
//! Abstract backend trait for the operation log.
//!
//! The local CLI uses `OpLog` (disk-based). Hosted/server backends live
//! outside this crate and implement the async [`OpLogBackend`] trait.

use std::future::Future;

use objects::{
    error::Result,
    object::{Scope, TransactionId},
};

use super::oplog_types::{
    ConditionalCommitOutcome, IsolationPrecondition, OpBatch, OpEntry, OpRecord,
    is_transaction_commit_for,
};

/// Async backend-agnostic interface for the operation log.
#[allow(async_fn_in_trait)]
pub trait OpLogBackend {
    /// Append a batch of operations atomically. Returns the assigned IDs.
    async fn record_batch(&self, operations: Vec<OpRecord>) -> Result<Vec<u64>> {
        self.record_batch_scoped(operations, None).await
    }

    async fn record_batch_scoped(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
    ) -> Result<Vec<u64>>;

    /// Atomic dedup+append for transaction-scoped batches.
    async fn record_batch_scoped_if_no_transaction(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
        transaction_id: &TransactionId,
        recent_window: usize,
    ) -> Result<Option<Vec<u64>>> {
        let recent = self.recent_batches_scoped(recent_window, scope).await?;
        if recent.iter().any(|batch| {
            batch
                .entries
                .iter()
                .any(|entry| is_transaction_commit_for(&entry.operation, transaction_id))
        }) {
            return Ok(None);
        }
        let ids = self.record_batch_scoped(operations, scope).await?;
        Ok(Some(ids))
    }

    /// Exact-once transaction append guarded by a per-key isolation
    /// precondition.
    async fn record_batch_exactly_once_if_unchanged(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
        transaction_id: &TransactionId,
        precondition: &IsolationPrecondition,
    ) -> Result<ConditionalCommitOutcome> {
        let _ = (operations, scope, transaction_id, precondition);
        Err(objects::error::HeddleError::Config(
            "oplog backend does not support conditional transaction commits".to_string(),
        ))
    }

    async fn last(&self) -> Result<Option<OpEntry>>;
    async fn recent(&self, count: usize) -> Result<Vec<OpEntry>>;
    async fn recent_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.recent_batches_scoped(count, None).await
    }
    async fn recent_batches_scoped(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>>;

    async fn undo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.undo_batches_scoped(count, None).await
    }
    async fn undo_batches_scoped(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>>;

    async fn redo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.redo_batches_scoped(count, None).await
    }
    async fn redo_batches_scoped(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>>;

    async fn mark_batch_undone(&self, batch: &OpBatch) -> Result<OpBatch>;
    async fn mark_batch_redone(&self, batch: &OpBatch) -> Result<OpBatch>;

    /// Coalesce two existing batches into one logical undo/redo unit.
    async fn coalesce_batches(
        &self,
        primary_batch_id: u64,
        secondary_batch_id: u64,
    ) -> Result<OpBatch> {
        let _ = (primary_batch_id, secondary_batch_id);
        Err(objects::error::HeddleError::Config(
            "oplog backend does not support batch coalescing".to_string(),
        ))
    }
}

impl<T> OpLogBackend for T
where
    T: BlockingOpLogBackend + Send + Sync,
{
    async fn record_batch(&self, operations: Vec<OpRecord>) -> Result<Vec<u64>> {
        BlockingOpLogBackend::record_batch(self, operations)
    }

    async fn record_batch_scoped(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
    ) -> Result<Vec<u64>> {
        BlockingOpLogBackend::record_batch_scoped(self, operations, scope)
    }

    async fn record_batch_scoped_if_no_transaction(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
        transaction_id: &TransactionId,
        recent_window: usize,
    ) -> Result<Option<Vec<u64>>> {
        BlockingOpLogBackend::record_batch_scoped_if_no_transaction(
            self,
            operations,
            scope,
            transaction_id,
            recent_window,
        )
        .await
    }

    async fn record_batch_exactly_once_if_unchanged(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
        transaction_id: &TransactionId,
        precondition: &IsolationPrecondition,
    ) -> Result<ConditionalCommitOutcome> {
        BlockingOpLogBackend::record_batch_exactly_once_if_unchanged(
            self,
            operations,
            scope,
            transaction_id,
            precondition,
        )
    }

    async fn last(&self) -> Result<Option<OpEntry>> {
        BlockingOpLogBackend::last(self)
    }

    async fn recent(&self, count: usize) -> Result<Vec<OpEntry>> {
        BlockingOpLogBackend::recent(self, count)
    }

    async fn recent_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        BlockingOpLogBackend::recent_batches(self, count).await
    }

    async fn recent_batches_scoped(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>> {
        BlockingOpLogBackend::recent_batches_scoped(self, count, scope).await
    }

    async fn undo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        BlockingOpLogBackend::undo_batches(self, count).await
    }

    async fn undo_batches_scoped(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>> {
        BlockingOpLogBackend::undo_batches_scoped(self, count, scope).await
    }

    async fn redo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        BlockingOpLogBackend::redo_batches(self, count).await
    }

    async fn redo_batches_scoped(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>> {
        BlockingOpLogBackend::redo_batches_scoped(self, count, scope).await
    }

    async fn mark_batch_undone(&self, batch: &OpBatch) -> Result<OpBatch> {
        BlockingOpLogBackend::mark_batch_undone(self, batch)
    }

    async fn mark_batch_redone(&self, batch: &OpBatch) -> Result<OpBatch> {
        BlockingOpLogBackend::mark_batch_redone(self, batch)
    }

    async fn coalesce_batches(
        &self,
        primary_batch_id: u64,
        secondary_batch_id: u64,
    ) -> Result<OpBatch> {
        BlockingOpLogBackend::coalesce_batches(self, primary_batch_id, secondary_batch_id)
    }
}

/// Blocking local backend interface for the file-backed operation log.
///
/// Existing local code remains explicit about using the blocking filesystem
/// backend, while hosted implementations can implement [`OpLogBackend`]
/// directly.
///
/// Backend-agnostic interface for the operation log.
///
/// The batch-history reads (`recent_batches*`, `undo_batches*`,
/// `redo_batches*`) and the dedup'd commit
/// (`record_batch_scoped_if_no_transaction`, which scans recent batches)
/// are spelled as `-> impl Future + Send` rather than `async fn` so the
/// returned future carries an explicit `Send` bound and the trait stays
/// clean under `-D warnings` (the `async_fn_in_trait` lint).
pub trait BlockingOpLogBackend: Send + Sync {
    /// Append a batch of operations atomically. Returns the assigned IDs.
    fn record_batch(&self, operations: Vec<OpRecord>) -> Result<Vec<u64>> {
        self.record_batch_scoped(operations, None)
    }
    fn record_batch_scoped(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
    ) -> Result<Vec<u64>>;

    /// Atomic dedup+append for transaction-scoped batches: scan the
    /// most recent `recent_window` batches under the same write lock
    /// used by [`BlockingOpLogBackend::record_batch_scoped`] for an
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
        scope: Option<&Scope>,
        transaction_id: &TransactionId,
        recent_window: usize,
    ) -> impl Future<Output = Result<Option<Vec<u64>>>> + Send {
        async move {
            let recent = self.recent_batches_scoped(recent_window, scope).await?;
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
    }

    /// Exact-once transaction append guarded by a per-key isolation
    /// precondition. Implementations that support local/hosted AtomicMutation
    /// commits must override this so dedup, conflict detection, and append are
    /// serialized at the backend's write authority.
    fn record_batch_exactly_once_if_unchanged(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
        transaction_id: &TransactionId,
        precondition: &IsolationPrecondition,
    ) -> Result<ConditionalCommitOutcome> {
        let _ = (operations, scope, transaction_id, precondition);
        Err(objects::error::HeddleError::Config(
            "oplog backend does not support conditional transaction commits".to_string(),
        ))
    }

    fn last(&self) -> Result<Option<OpEntry>>;
    fn recent(&self, count: usize) -> Result<Vec<OpEntry>>;
    fn recent_batches(&self, count: usize) -> impl Future<Output = Result<Vec<OpBatch>>> + Send {
        async move { self.recent_batches_scoped(count, None).await }
    }
    fn recent_batches_scoped(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> impl Future<Output = Result<Vec<OpBatch>>> + Send;

    fn undo_batches(&self, count: usize) -> impl Future<Output = Result<Vec<OpBatch>>> + Send {
        async move { self.undo_batches_scoped(count, None).await }
    }
    fn undo_batches_scoped(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> impl Future<Output = Result<Vec<OpBatch>>> + Send;
    fn redo_batches(&self, count: usize) -> impl Future<Output = Result<Vec<OpBatch>>> + Send {
        async move { self.redo_batches_scoped(count, None).await }
    }
    fn redo_batches_scoped(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> impl Future<Output = Result<Vec<OpBatch>>> + Send;
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
}
