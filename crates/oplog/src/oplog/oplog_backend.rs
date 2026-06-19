// SPDX-License-Identifier: Apache-2.0
//! Operation-log backend traits.
//!
//! [`OpLogBackend`] is the async/cloud-native contract. [`LocalOpLogBackend`]
//! is the synchronous local capability used by the file-backed CLI/repo path.

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

/// Async backend-agnostic interface for the operation log.
#[allow(async_fn_in_trait)]
pub trait OpLogBackend: Send + Sync {
    async fn record_batch_async(&self, operations: Vec<OpRecord>) -> Result<Vec<u64>> {
        self.record_batch_scoped_async(operations, None).await
    }

    async fn record_batch_scoped_async(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
    ) -> Result<Vec<u64>> {
        let _ = (operations, scope);
        Err(unsupported_local_method("record_batch_scoped_async"))
    }

    /// Async atomic dedup+append for transaction-scoped batches.
    ///
    /// The default implementation composes async read + append and is therefore
    /// non-atomic. Backends should override when their storage authority can
    /// serialize the check and append.
    async fn record_batch_scoped_if_no_transaction_async(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
        transaction_id: &TransactionId,
        recent_window: usize,
    ) -> Result<Option<Vec<u64>>> {
        let recent = self
            .recent_batches_scoped_async(recent_window, scope)
            .await?;
        if recent.iter().any(|batch| {
            batch
                .entries
                .iter()
                .any(|entry| is_transaction_commit_for(&entry.operation, transaction_id))
        }) {
            return Ok(None);
        }
        let ids = self.record_batch_scoped_async(operations, scope).await?;
        Ok(Some(ids))
    }

    async fn record_batch_exactly_once_if_unchanged_async(
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

    async fn last_async(&self) -> Result<Option<OpEntry>> {
        Err(unsupported_local_method("last_async"))
    }

    async fn recent_async(&self, count: usize) -> Result<Vec<OpEntry>> {
        let _ = count;
        Err(unsupported_local_method("recent_async"))
    }

    async fn recent_batches_async(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.recent_batches_scoped_async(count, None).await
    }

    async fn recent_batches_scoped_async(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>> {
        let _ = (count, scope);
        Err(unsupported_local_method("recent_batches_scoped_async"))
    }

    async fn undo_batches_async(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.undo_batches_scoped_async(count, None).await
    }

    async fn undo_batches_scoped_async(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>> {
        let _ = (count, scope);
        Err(unsupported_local_method("undo_batches_scoped_async"))
    }

    async fn redo_batches_async(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.redo_batches_scoped_async(count, None).await
    }

    async fn redo_batches_scoped_async(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>> {
        let _ = (count, scope);
        Err(unsupported_local_method("redo_batches_scoped_async"))
    }

    async fn mark_batch_undone_async(&self, batch: &OpBatch) -> Result<OpBatch> {
        let _ = batch;
        Err(unsupported_local_method("mark_batch_undone_async"))
    }

    async fn mark_batch_redone_async(&self, batch: &OpBatch) -> Result<OpBatch> {
        let _ = batch;
        Err(unsupported_local_method("mark_batch_redone_async"))
    }

    async fn coalesce_batches_async(
        &self,
        primary_batch_id: u64,
        secondary_batch_id: u64,
    ) -> Result<OpBatch> {
        let _ = (primary_batch_id, secondary_batch_id);
        Err(HeddleError::Config(
            "oplog backend does not support batch coalescing".to_string(),
        ))
    }
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

/// Adapter for exposing a synchronous local oplog backend through the async
/// [`OpLogBackend`] contract without creating downstream coherence conflicts.
#[derive(Debug, Clone, Copy)]
pub struct AsyncFromLocalOpLog<T> {
    inner: T,
}

impl<T> AsyncFromLocalOpLog<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: LocalOpLogBackend> OpLogBackend for AsyncFromLocalOpLog<T> {
    async fn record_batch_async(&self, operations: Vec<OpRecord>) -> Result<Vec<u64>> {
        local_record_batch_async(&self.inner, operations).await
    }

    async fn record_batch_scoped_async(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
    ) -> Result<Vec<u64>> {
        local_record_batch_scoped_async(&self.inner, operations, scope).await
    }

    async fn record_batch_scoped_if_no_transaction_async(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
        transaction_id: &TransactionId,
        recent_window: usize,
    ) -> Result<Option<Vec<u64>>> {
        local_record_batch_scoped_if_no_transaction_async(
            &self.inner,
            operations,
            scope,
            transaction_id,
            recent_window,
        )
        .await
    }

    async fn record_batch_exactly_once_if_unchanged_async(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
        transaction_id: &TransactionId,
        precondition: &IsolationPrecondition,
    ) -> Result<ConditionalCommitOutcome> {
        local_record_batch_exactly_once_if_unchanged_async(
            &self.inner,
            operations,
            scope,
            transaction_id,
            precondition,
        )
        .await
    }

    async fn last_async(&self) -> Result<Option<OpEntry>> {
        local_last_async(&self.inner).await
    }

    async fn recent_async(&self, count: usize) -> Result<Vec<OpEntry>> {
        local_recent_async(&self.inner, count).await
    }

    async fn recent_batches_async(&self, count: usize) -> Result<Vec<OpBatch>> {
        local_recent_batches_async(&self.inner, count).await
    }

    async fn recent_batches_scoped_async(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>> {
        local_recent_batches_scoped_async(&self.inner, count, scope).await
    }

    async fn undo_batches_async(&self, count: usize) -> Result<Vec<OpBatch>> {
        local_undo_batches_async(&self.inner, count).await
    }

    async fn undo_batches_scoped_async(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>> {
        local_undo_batches_scoped_async(&self.inner, count, scope).await
    }

    async fn redo_batches_async(&self, count: usize) -> Result<Vec<OpBatch>> {
        local_redo_batches_async(&self.inner, count).await
    }

    async fn redo_batches_scoped_async(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>> {
        local_redo_batches_scoped_async(&self.inner, count, scope).await
    }

    async fn mark_batch_undone_async(&self, batch: &OpBatch) -> Result<OpBatch> {
        local_mark_batch_undone_async(&self.inner, batch).await
    }

    async fn mark_batch_redone_async(&self, batch: &OpBatch) -> Result<OpBatch> {
        local_mark_batch_redone_async(&self.inner, batch).await
    }

    async fn coalesce_batches_async(
        &self,
        primary_batch_id: u64,
        secondary_batch_id: u64,
    ) -> Result<OpBatch> {
        local_coalesce_batches_async(&self.inner, primary_batch_id, secondary_batch_id).await
    }
}

/// Borrowed variant of [`AsyncFromLocalOpLog`] for tests and helpers that
/// should not take ownership of the local backend.
#[derive(Debug, Clone, Copy)]
pub struct AsyncFromLocalOpLogRef<'a, T: ?Sized> {
    inner: &'a T,
}

impl<'a, T: ?Sized> AsyncFromLocalOpLogRef<'a, T> {
    pub fn new(inner: &'a T) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &'a T {
        self.inner
    }
}

impl<T: LocalOpLogBackend + ?Sized> OpLogBackend for AsyncFromLocalOpLogRef<'_, T> {
    async fn record_batch_async(&self, operations: Vec<OpRecord>) -> Result<Vec<u64>> {
        local_record_batch_async(self.inner, operations).await
    }

    async fn record_batch_scoped_async(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
    ) -> Result<Vec<u64>> {
        local_record_batch_scoped_async(self.inner, operations, scope).await
    }

    async fn record_batch_scoped_if_no_transaction_async(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
        transaction_id: &TransactionId,
        recent_window: usize,
    ) -> Result<Option<Vec<u64>>> {
        local_record_batch_scoped_if_no_transaction_async(
            self.inner,
            operations,
            scope,
            transaction_id,
            recent_window,
        )
        .await
    }

    async fn record_batch_exactly_once_if_unchanged_async(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
        transaction_id: &TransactionId,
        precondition: &IsolationPrecondition,
    ) -> Result<ConditionalCommitOutcome> {
        local_record_batch_exactly_once_if_unchanged_async(
            self.inner,
            operations,
            scope,
            transaction_id,
            precondition,
        )
        .await
    }

    async fn last_async(&self) -> Result<Option<OpEntry>> {
        local_last_async(self.inner).await
    }

    async fn recent_async(&self, count: usize) -> Result<Vec<OpEntry>> {
        local_recent_async(self.inner, count).await
    }

    async fn recent_batches_async(&self, count: usize) -> Result<Vec<OpBatch>> {
        local_recent_batches_async(self.inner, count).await
    }

    async fn recent_batches_scoped_async(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>> {
        local_recent_batches_scoped_async(self.inner, count, scope).await
    }

    async fn undo_batches_async(&self, count: usize) -> Result<Vec<OpBatch>> {
        local_undo_batches_async(self.inner, count).await
    }

    async fn undo_batches_scoped_async(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>> {
        local_undo_batches_scoped_async(self.inner, count, scope).await
    }

    async fn redo_batches_async(&self, count: usize) -> Result<Vec<OpBatch>> {
        local_redo_batches_async(self.inner, count).await
    }

    async fn redo_batches_scoped_async(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>> {
        local_redo_batches_scoped_async(self.inner, count, scope).await
    }

    async fn mark_batch_undone_async(&self, batch: &OpBatch) -> Result<OpBatch> {
        local_mark_batch_undone_async(self.inner, batch).await
    }

    async fn mark_batch_redone_async(&self, batch: &OpBatch) -> Result<OpBatch> {
        local_mark_batch_redone_async(self.inner, batch).await
    }

    async fn coalesce_batches_async(
        &self,
        primary_batch_id: u64,
        secondary_batch_id: u64,
    ) -> Result<OpBatch> {
        local_coalesce_batches_async(self.inner, primary_batch_id, secondary_batch_id).await
    }
}

pub(crate) mod sealed {
    pub trait HeddleLocalAsyncOptIn {}
}

impl<T> OpLogBackend for T
where
    T: LocalOpLogBackend + sealed::HeddleLocalAsyncOptIn,
{
    async fn record_batch_async(&self, operations: Vec<OpRecord>) -> Result<Vec<u64>> {
        local_record_batch_async(self, operations).await
    }

    async fn record_batch_scoped_async(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
    ) -> Result<Vec<u64>> {
        local_record_batch_scoped_async(self, operations, scope).await
    }

    async fn record_batch_scoped_if_no_transaction_async(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
        transaction_id: &TransactionId,
        recent_window: usize,
    ) -> Result<Option<Vec<u64>>> {
        local_record_batch_scoped_if_no_transaction_async(
            self,
            operations,
            scope,
            transaction_id,
            recent_window,
        )
        .await
    }

    async fn record_batch_exactly_once_if_unchanged_async(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&Scope>,
        transaction_id: &TransactionId,
        precondition: &IsolationPrecondition,
    ) -> Result<ConditionalCommitOutcome> {
        local_record_batch_exactly_once_if_unchanged_async(
            self,
            operations,
            scope,
            transaction_id,
            precondition,
        )
        .await
    }

    async fn last_async(&self) -> Result<Option<OpEntry>> {
        local_last_async(self).await
    }

    async fn recent_async(&self, count: usize) -> Result<Vec<OpEntry>> {
        local_recent_async(self, count).await
    }

    async fn recent_batches_async(&self, count: usize) -> Result<Vec<OpBatch>> {
        local_recent_batches_async(self, count).await
    }

    async fn recent_batches_scoped_async(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>> {
        local_recent_batches_scoped_async(self, count, scope).await
    }

    async fn undo_batches_async(&self, count: usize) -> Result<Vec<OpBatch>> {
        local_undo_batches_async(self, count).await
    }

    async fn undo_batches_scoped_async(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>> {
        local_undo_batches_scoped_async(self, count, scope).await
    }

    async fn redo_batches_async(&self, count: usize) -> Result<Vec<OpBatch>> {
        local_redo_batches_async(self, count).await
    }

    async fn redo_batches_scoped_async(
        &self,
        count: usize,
        scope: Option<&Scope>,
    ) -> Result<Vec<OpBatch>> {
        local_redo_batches_scoped_async(self, count, scope).await
    }

    async fn mark_batch_undone_async(&self, batch: &OpBatch) -> Result<OpBatch> {
        local_mark_batch_undone_async(self, batch).await
    }

    async fn mark_batch_redone_async(&self, batch: &OpBatch) -> Result<OpBatch> {
        local_mark_batch_redone_async(self, batch).await
    }

    async fn coalesce_batches_async(
        &self,
        primary_batch_id: u64,
        secondary_batch_id: u64,
    ) -> Result<OpBatch> {
        local_coalesce_batches_async(self, primary_batch_id, secondary_batch_id).await
    }
}

async fn local_record_batch_async(
    backend: &(impl LocalOpLogBackend + ?Sized),
    operations: Vec<OpRecord>,
) -> Result<Vec<u64>> {
    backend.record_batch(operations)
}

async fn local_record_batch_scoped_async(
    backend: &(impl LocalOpLogBackend + ?Sized),
    operations: Vec<OpRecord>,
    scope: Option<&Scope>,
) -> Result<Vec<u64>> {
    backend.record_batch_scoped(operations, scope)
}

async fn local_record_batch_scoped_if_no_transaction_async(
    backend: &(impl LocalOpLogBackend + ?Sized),
    operations: Vec<OpRecord>,
    scope: Option<&Scope>,
    transaction_id: &TransactionId,
    recent_window: usize,
) -> Result<Option<Vec<u64>>> {
    backend.record_batch_scoped_if_no_transaction(operations, scope, transaction_id, recent_window)
}

async fn local_record_batch_exactly_once_if_unchanged_async(
    backend: &(impl LocalOpLogBackend + ?Sized),
    operations: Vec<OpRecord>,
    scope: Option<&Scope>,
    transaction_id: &TransactionId,
    precondition: &IsolationPrecondition,
) -> Result<ConditionalCommitOutcome> {
    backend.record_batch_exactly_once_if_unchanged(operations, scope, transaction_id, precondition)
}

async fn local_last_async(backend: &(impl LocalOpLogBackend + ?Sized)) -> Result<Option<OpEntry>> {
    backend.last()
}

async fn local_recent_async(
    backend: &(impl LocalOpLogBackend + ?Sized),
    count: usize,
) -> Result<Vec<OpEntry>> {
    backend.recent(count)
}

async fn local_recent_batches_async(
    backend: &(impl LocalOpLogBackend + ?Sized),
    count: usize,
) -> Result<Vec<OpBatch>> {
    backend.recent_batches(count)
}

async fn local_recent_batches_scoped_async(
    backend: &(impl LocalOpLogBackend + ?Sized),
    count: usize,
    scope: Option<&Scope>,
) -> Result<Vec<OpBatch>> {
    backend.recent_batches_scoped(count, scope)
}

async fn local_undo_batches_async(
    backend: &(impl LocalOpLogBackend + ?Sized),
    count: usize,
) -> Result<Vec<OpBatch>> {
    backend.undo_batches(count)
}

async fn local_undo_batches_scoped_async(
    backend: &(impl LocalOpLogBackend + ?Sized),
    count: usize,
    scope: Option<&Scope>,
) -> Result<Vec<OpBatch>> {
    backend.undo_batches_scoped(count, scope)
}

async fn local_redo_batches_async(
    backend: &(impl LocalOpLogBackend + ?Sized),
    count: usize,
) -> Result<Vec<OpBatch>> {
    backend.redo_batches(count)
}

async fn local_redo_batches_scoped_async(
    backend: &(impl LocalOpLogBackend + ?Sized),
    count: usize,
    scope: Option<&Scope>,
) -> Result<Vec<OpBatch>> {
    backend.redo_batches_scoped(count, scope)
}

async fn local_mark_batch_undone_async(
    backend: &(impl LocalOpLogBackend + ?Sized),
    batch: &OpBatch,
) -> Result<OpBatch> {
    backend.mark_batch_undone(batch)
}

async fn local_mark_batch_redone_async(
    backend: &(impl LocalOpLogBackend + ?Sized),
    batch: &OpBatch,
) -> Result<OpBatch> {
    backend.mark_batch_redone(batch)
}

async fn local_coalesce_batches_async(
    backend: &(impl LocalOpLogBackend + ?Sized),
    primary_batch_id: u64,
    secondary_batch_id: u64,
) -> Result<OpBatch> {
    backend.coalesce_batches(primary_batch_id, secondary_batch_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DualCapabilityBackend;

    impl LocalOpLogBackend for DualCapabilityBackend {}

    impl OpLogBackend for DualCapabilityBackend {}

    #[test]
    fn local_oplog_backend_can_provide_explicit_async_impl() {
        fn assert_local<T: LocalOpLogBackend>() {}
        fn assert_async<T: OpLogBackend>() {}

        assert_local::<DualCapabilityBackend>();
        assert_async::<DualCapabilityBackend>();
    }
}
