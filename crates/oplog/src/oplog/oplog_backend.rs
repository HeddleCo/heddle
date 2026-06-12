// SPDX-License-Identifier: Apache-2.0
//! Abstract backend trait for the operation log.
//!
//! The local CLI uses `OpLog` (disk-based). The server uses `PgOpLogBackend`
//! (Postgres-backed append-only table). Both implement this trait.

use std::future::Future;

use objects::{
    error::Result,
    object::{ChangeId, ContentHash, MarkerName, ThreadName, VisibilityTier},
};

use super::oplog_types::{
    ConditionalCommitOutcome, IsolationPrecondition, OpBatch, OpEntry, OpRecord,
};

/// Before/after snapshots of a per-state visibility sidecar, captured around a
/// `visibility set`/`promote` put. `prior` is the full sidecar bytes before the
/// put (undo target), `new` the bytes after (redo target); `None` on either side
/// means public-by-absence. Bundled so the record helpers stay under the
/// argument-count lint and the undo/redo contract reads as one unit.
#[derive(Debug, Clone, Default)]
pub struct VisibilitySidecarSnapshots {
    pub prior: Option<Vec<u8>>,
    pub new: Option<Vec<u8>>,
}

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
    ) -> impl Future<Output = Result<Option<Vec<u64>>>> + Send {
        async move {
            let recent = self.recent_batches_scoped(recent_window, scope).await?;
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
                head: thread.is_none().then_some(*new_state),
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
                head: *target,
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
    /// Always emits `OpRecord::ThreadCreate`. V1
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
            vec![OpRecord::ThreadCreate {
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
                OpRecord::ThreadCreate {
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

    /// Record a fork. `from` is the source state, `new_state` the fork
    /// result (correct argument order — heddle#330 r15 fixed the
    /// `cmd_fork` call site that passed these reversed). `thread`/`head`
    /// name the published ref so crash-replay can re-materialize it.
    ///
    /// `scope` MUST be the writer's `op_scope` for a HEAD-moving fork
    /// (heddle#354 r7, cid 3329765074): the read chokepoint reconciles the
    /// `Local` HEAD class scoped to `op_scope`, so an unscoped record would be
    /// invisible to the fold and a crash before HEAD-publish would strand it.
    fn record_fork(
        &self,
        from: &ChangeId,
        new_state: &ChangeId,
        thread: Option<&str>,
        head: Option<&ChangeId>,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::Fork {
                from: *from,
                new_state: *new_state,
                thread: thread.map(str::to_string),
                head: head.copied(),
            }],
            scope,
        )?;
        Ok(ids[0])
    }

    /// Record a collapse. `thread` names the published ref (`Some(name)`
    /// for a thread ref, `None` for a detached HEAD at `result`).
    ///
    /// `scope` MUST be the writer's `op_scope` for a detached (HEAD-moving)
    /// collapse (heddle#354 r7, cid 3329765074), for the same reason as
    /// [`record_fork`](Self::record_fork).
    fn record_collapse(
        &self,
        sources: &[ChangeId],
        result: &ChangeId,
        thread: Option<&str>,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::Collapse {
                sources: sources.to_vec(),
                result: *result,
                thread: thread.map(str::to_string),
            }],
            scope,
        )?;
        Ok(ids[0])
    }

    /// Record a remote-thread publish (heddle#330 r9).
    fn record_remote_thread_update(
        &self,
        remote: &str,
        thread: &str,
        state: &ChangeId,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::RemoteThreadUpdate {
                remote: remote.to_string(),
                thread: thread.to_string(),
                state: *state,
            }],
            scope,
        )?;
        Ok(ids[0])
    }

    /// Record a remote-thread deletion (heddle#330 r9).
    fn record_remote_thread_delete(
        &self,
        remote: &str,
        thread: &str,
        state: &ChangeId,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::RemoteThreadDelete {
                remote: remote.to_string(),
                thread: thread.to_string(),
                state: *state,
            }],
            scope,
        )?;
        Ok(ids[0])
    }

    /// Record an undo-recovery pointer publish (heddle#330 r9). Local
    /// (per-checkout) ref, so `scope` carries `op_scope()`.
    fn record_undo_recovery_update(&self, state: &ChangeId, scope: Option<&str>) -> Result<u64> {
        let ids =
            self.record_batch_scoped(vec![OpRecord::UndoRecoveryUpdate { state: *state }], scope)?;
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

    /// Record a state-visibility declaration (heddle#317). Audit companion to
    /// the per-state `StateVisibility` sidecar; emitted by `heddle visibility
    /// set` and by the Invariant-A capture-time binding. `sidecar` carries the
    /// full per-state sidecar bytes before/after the put (or `None` for
    /// public-by-absence) so undo/redo can restore it. `scope` carries the
    /// repo's `op_scope()` for parity with the other record helpers, even
    /// though the visibility class touches no refs.
    fn record_state_visibility_set(
        &self,
        state: &ChangeId,
        record_id: &ContentHash,
        tier: &VisibilityTier,
        sidecar: VisibilitySidecarSnapshots,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::StateVisibilitySet {
                state: *state,
                record_id: *record_id,
                tier: tier.clone(),
                prior_sidecar: sidecar.prior,
                new_sidecar: sidecar.new,
            }],
            scope,
        )?;
        Ok(ids[0])
    }

    /// Record a state-visibility promotion (heddle#317): a superseding record
    /// lifted the state to a less-restrictive `tier`. `sidecar` snapshots the
    /// whole per-state sidecar before/after the put so undo/redo can restore it.
    fn record_state_visibility_promote(
        &self,
        state: &ChangeId,
        superseded: &ContentHash,
        record_id: &ContentHash,
        tier: &VisibilityTier,
        sidecar: VisibilitySidecarSnapshots,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::StateVisibilityPromote {
                state: *state,
                superseded: *superseded,
                record_id: *record_id,
                tier: tier.clone(),
                prior_sidecar: sidecar.prior,
                new_sidecar: sidecar.new,
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
    /// Emits the canonical `OpRecord::FastForward` shape with both pre- and
    /// post-target ids.
    fn record_fast_forward(
        &self,
        source_thread: &ThreadName,
        target_thread: &ThreadName,
        pre_target_id: &ChangeId,
        post_target_id: &ChangeId,
        scope: Option<&str>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::FastForward {
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
