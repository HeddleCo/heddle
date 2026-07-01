// SPDX-License-Identifier: Apache-2.0
//! Abstract backend trait for the operation log.
//!
//! The local CLI uses `OpLog` (disk-based). The server uses `PgOpLogBackend`
//! (Postgres-backed append-only table). Both implement this trait.

use std::future::Future;

use objects::error::Result;

use super::oplog_types::{
    ConditionalCommitOutcome, IsolationPrecondition, OpBatch, OpEntry, OpRecord,
    is_transaction_commit_for,
};

/// Backend-agnostic interface for the operation log.
///
/// The batch-history reads (`recent_batches*`, `undo_batches*`,
/// `redo_batches*`) and the dedup'd commit
/// (`record_batch_scoped_if_no_transaction`, which scans recent batches)
/// are `async` so the Postgres backend can `.await` `sqlx` directly
/// instead of bridging through a worker-thread runtime. They're spelled
/// as `-> impl Future + Send` rather than `async fn` so the returned
/// future carries an explicit `Send` bound (required by the hosted
/// server's Tower/tonic stack) and the trait stays clean under
/// `-D warnings` (the `async_fn_in_trait` lint). Sealed interface —
/// heddle is the sole implementer.
pub trait OpLogBackend: Send + Sync {
    /// One-shot local storage migration hook. File-backed oplogs rewrite old
    /// packed containers and record schemas to the current format; non-file
    /// backends either have their own migration system or no local file to
    /// rewrite.
    fn migrate_to_current_format(&self) -> Result<()> {
        Ok(())
    }

    /// Append a batch of operations atomically. Returns the assigned IDs.
    fn record_batch(&self, operations: Vec<OpRecord>) -> Result<Vec<u64>> {
        self.record_batch_scoped(operations, None)
    }
    fn record_batch_scoped(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&str>,
    ) -> Result<Vec<u64>>;

    /// Append many independent batches in a single write.
    ///
    /// Each element of `groups` becomes its own logical batch — its own
    /// `batch_id`, its own scope, its own undo/redo granularity — exactly as
    /// if it had been passed to [`record_batch_scoped`] on its own. The only
    /// difference is durability cost: the file-backed `OpLog` rewrites the
    /// whole log once per *call* (see TODO #423 write-amplification in
    /// `packed_oplog::append_entries`), so N separate `record_batch_scoped`
    /// calls are O(N²) total while one `record_batches_scoped` of N groups is
    /// O(N). This is the importer's path: a reflog with N entries emits N
    /// per-event batches that must stay independently undoable but must not
    /// pay N full-log rewrites.
    ///
    /// Returns one id-vector per input group, in order; empty groups yield an
    /// empty id-vector and consume no batch id. The default implementation is
    /// the naive loop (correct, but pays the per-call cost) — backends with no
    /// per-append amplification (e.g. the Postgres table) keep it; `OpLog`
    /// overrides it to coalesce into one `append_entries`.
    fn record_batches_scoped(
        &self,
        groups: Vec<(Vec<OpRecord>, Option<&str>)>,
    ) -> Result<Vec<Vec<u64>>> {
        groups
            .into_iter()
            .map(|(ops, scope)| self.record_batch_scoped(ops, scope))
            .collect()
    }

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
        scope: Option<&str>,
        transaction_id: &str,
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
        scope: Option<&str>,
    ) -> impl Future<Output = Result<Vec<OpBatch>>> + Send;

    fn undo_batches(&self, count: usize) -> impl Future<Output = Result<Vec<OpBatch>>> + Send {
        async move { self.undo_batches_scoped(count, None).await }
    }
    fn undo_batches_scoped(
        &self,
        count: usize,
        scope: Option<&str>,
    ) -> impl Future<Output = Result<Vec<OpBatch>>> + Send;
    fn redo_batches(&self, count: usize) -> impl Future<Output = Result<Vec<OpBatch>>> + Send {
        async move { self.redo_batches_scoped(count, None).await }
    }
    fn redo_batches_scoped(
        &self,
        count: usize,
        scope: Option<&str>,
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
