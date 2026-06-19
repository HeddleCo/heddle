// SPDX-License-Identifier: Apache-2.0
//! Domain event constructors for operation-log records.
//!
//! `BlockingOpLogBackend` is the storage contract. This trait is the explicit domain
//! recording surface for callers that intentionally build canonical `OpRecord`
//! variants from model values.

use objects::{
    error::Result,
    object::{ChangeId, ContentHash, MarkerName, Scope, ThreadName, VisibilityTier},
};

use super::{
    oplog_backend::{BlockingOpLogBackend, OpLogBackend},
    oplog_types::{OpRecord, ThreadUpdateSnapshots},
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

#[allow(async_fn_in_trait)]
pub trait OpLogRecorder: OpLogBackend {
    async fn record_snapshot(
        &self,
        new_state: &ChangeId,
        prev_head: Option<&ChangeId>,
        thread: Option<&str>,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self
            .record_batch_scoped(
                vec![OpRecord::Snapshot {
                    new_state: *new_state,
                    prev_head: prev_head.copied(),
                    head: thread.is_none().then_some(*new_state),
                    thread: thread.map(str::to_string),
                }],
                scope,
            )
            .await?;
        Ok(ids[0])
    }

    async fn record_goto(
        &self,
        target: &ChangeId,
        prev_head: Option<&ChangeId>,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self
            .record_batch_scoped(
                vec![OpRecord::Goto {
                    target: *target,
                    prev_head: prev_head.copied(),
                    head: *target,
                }],
                scope,
            )
            .await?;
        Ok(ids[0])
    }

    async fn record_thread_create(
        &self,
        name: &ThreadName,
        state: &ChangeId,
        manager_snapshot: Option<Vec<u8>>,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self
            .record_batch_scoped(
                vec![OpRecord::ThreadCreate {
                    name: name.to_string(),
                    state: *state,
                    manager_snapshot,
                }],
                scope,
            )
            .await?;
        Ok(ids[0])
    }

    async fn record_thread_delete(
        &self,
        name: &ThreadName,
        state: &ChangeId,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self
            .record_batch_scoped(
                vec![OpRecord::ThreadDelete {
                    name: name.to_string(),
                    state: *state,
                }],
                scope,
            )
            .await?;
        Ok(ids[0])
    }

    async fn record_thread_update(
        &self,
        name: &ThreadName,
        old_state: &ChangeId,
        new_state: &ChangeId,
        manager_snapshots: Option<ThreadUpdateSnapshots>,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self
            .record_batch_scoped(
                vec![OpRecord::ThreadUpdate {
                    name: name.to_string(),
                    old_state: *old_state,
                    new_state: *new_state,
                    manager_snapshots,
                }],
                scope,
            )
            .await?;
        Ok(ids[0])
    }

    async fn record_thread_rename(
        &self,
        old_name: &ThreadName,
        new_name: &ThreadName,
        state: &ChangeId,
        scope: Option<&Scope>,
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
        .await
    }

    async fn record_fork(
        &self,
        from: &ChangeId,
        new_state: &ChangeId,
        thread: Option<&str>,
        head: Option<&ChangeId>,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self
            .record_batch_scoped(
                vec![OpRecord::Fork {
                    from: *from,
                    new_state: *new_state,
                    thread: thread.map(str::to_string),
                    head: head.copied(),
                }],
                scope,
            )
            .await?;
        Ok(ids[0])
    }

    async fn record_collapse(
        &self,
        sources: &[ChangeId],
        result: &ChangeId,
        thread: Option<&str>,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self
            .record_batch_scoped(
                vec![OpRecord::Collapse {
                    sources: sources.to_vec(),
                    result: *result,
                    thread: thread.map(str::to_string),
                    pre_thread_state: None,
                }],
                scope,
            )
            .await?;
        Ok(ids[0])
    }

    async fn record_remote_thread_update(
        &self,
        remote: &str,
        thread: &str,
        state: &ChangeId,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self
            .record_batch_scoped(
                vec![OpRecord::RemoteThreadUpdate {
                    remote: objects::object::RemoteName::new(remote),
                    thread: thread.to_string(),
                    state: *state,
                }],
                scope,
            )
            .await?;
        Ok(ids[0])
    }

    async fn record_remote_thread_delete(
        &self,
        remote: &str,
        thread: &str,
        state: &ChangeId,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self
            .record_batch_scoped(
                vec![OpRecord::RemoteThreadDelete {
                    remote: objects::object::RemoteName::new(remote),
                    thread: thread.to_string(),
                    state: *state,
                }],
                scope,
            )
            .await?;
        Ok(ids[0])
    }

    async fn record_undo_recovery_update(
        &self,
        state: &ChangeId,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self
            .record_batch_scoped(vec![OpRecord::UndoRecoveryUpdate { state: *state }], scope)
            .await?;
        Ok(ids[0])
    }

    async fn record_marker_create(&self, name: &MarkerName, state: &ChangeId) -> Result<u64> {
        let ids = self
            .record_batch(vec![OpRecord::MarkerCreate {
                name: name.to_string(),
                state: *state,
            }])
            .await?;
        Ok(ids[0])
    }

    async fn record_marker_delete(&self, name: &MarkerName, state: &ChangeId) -> Result<u64> {
        let ids = self
            .record_batch(vec![OpRecord::MarkerDelete {
                name: name.to_string(),
                state: *state,
            }])
            .await?;
        Ok(ids[0])
    }

    async fn record_redact(
        &self,
        redaction_id: &ContentHash,
        blob: &ContentHash,
        state: &ChangeId,
        path: &str,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self
            .record_batch_scoped(
                vec![OpRecord::Redact {
                    redaction_id: *redaction_id,
                    blob: *blob,
                    state: *state,
                    path: path.to_string(),
                }],
                scope,
            )
            .await?;
        Ok(ids[0])
    }

    async fn record_purge(
        &self,
        redaction_id: &ContentHash,
        blob: &ContentHash,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self
            .record_batch_scoped(
                vec![OpRecord::Purge {
                    redaction_id: *redaction_id,
                    blob: *blob,
                }],
                scope,
            )
            .await?;
        Ok(ids[0])
    }

    async fn record_state_visibility_set(
        &self,
        state: &ChangeId,
        record_id: &ContentHash,
        tier: &VisibilityTier,
        sidecar: VisibilitySidecarSnapshots,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self
            .record_batch_scoped(
                vec![OpRecord::StateVisibilitySet {
                    state: *state,
                    record_id: *record_id,
                    tier: tier.clone(),
                    prior_sidecar: sidecar.prior,
                    new_sidecar: sidecar.new,
                }],
                scope,
            )
            .await?;
        Ok(ids[0])
    }

    async fn record_state_visibility_promote(
        &self,
        state: &ChangeId,
        superseded: &ContentHash,
        record_id: &ContentHash,
        tier: &VisibilityTier,
        sidecar: VisibilitySidecarSnapshots,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self
            .record_batch_scoped(
                vec![OpRecord::StateVisibilityPromote {
                    state: *state,
                    superseded: *superseded,
                    record_id: *record_id,
                    tier: tier.clone(),
                    prior_sidecar: sidecar.prior,
                    new_sidecar: sidecar.new,
                }],
                scope,
            )
            .await?;
        Ok(ids[0])
    }

    async fn record_fast_forward(
        &self,
        source_thread: &ThreadName,
        target_thread: &ThreadName,
        pre_target_id: &ChangeId,
        post_target_id: &ChangeId,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self
            .record_batch_scoped(
                vec![OpRecord::FastForward {
                    source_thread: source_thread.to_string(),
                    target_thread: target_thread.to_string(),
                    pre_target_id: *pre_target_id,
                    post_target_id: *post_target_id,
                }],
                scope,
            )
            .await?;
        Ok(ids[0])
    }
}

impl<T: OpLogBackend + ?Sized> OpLogRecorder for T {}

pub trait BlockingOpLogRecorder: BlockingOpLogBackend {
    fn record_snapshot(
        &self,
        new_state: &ChangeId,
        prev_head: Option<&ChangeId>,
        thread: Option<&str>,
        scope: Option<&Scope>,
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
        scope: Option<&Scope>,
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
    /// Always emits `OpRecord::ThreadCreate` (the collapsed canonical
    /// shape; the V1/V2 split was removed under the no-production-oplogs
    /// premise).
    fn record_thread_create(
        &self,
        name: &ThreadName,
        state: &ChangeId,
        manager_snapshot: Option<Vec<u8>>,
        scope: Option<&Scope>,
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
        scope: Option<&Scope>,
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

    fn record_thread_update(
        &self,
        name: &ThreadName,
        old_state: &ChangeId,
        new_state: &ChangeId,
        manager_snapshots: Option<ThreadUpdateSnapshots>,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::ThreadUpdate {
                name: name.to_string(),
                old_state: *old_state,
                new_state: *new_state,
                manager_snapshots,
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
        scope: Option<&Scope>,
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

    /// Record a fork. `from` is the source state, `new_state` the fork result.
    /// `thread`/`head` name the published ref so crash-replay can
    /// re-materialize it.
    fn record_fork(
        &self,
        from: &ChangeId,
        new_state: &ChangeId,
        thread: Option<&str>,
        head: Option<&ChangeId>,
        scope: Option<&Scope>,
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

    /// Record a collapse. `thread` names the published ref (`Some(name)` for a
    /// thread ref, `None` for a detached HEAD at `result`).
    fn record_collapse(
        &self,
        sources: &[ChangeId],
        result: &ChangeId,
        thread: Option<&str>,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::Collapse {
                sources: sources.to_vec(),
                result: *result,
                thread: thread.map(str::to_string),
                pre_thread_state: None,
            }],
            scope,
        )?;
        Ok(ids[0])
    }

    fn record_remote_thread_update(
        &self,
        remote: &str,
        thread: &str,
        state: &ChangeId,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::RemoteThreadUpdate {
                remote: objects::object::RemoteName::new(remote),
                thread: thread.to_string(),
                state: *state,
            }],
            scope,
        )?;
        Ok(ids[0])
    }

    fn record_remote_thread_delete(
        &self,
        remote: &str,
        thread: &str,
        state: &ChangeId,
        scope: Option<&Scope>,
    ) -> Result<u64> {
        let ids = self.record_batch_scoped(
            vec![OpRecord::RemoteThreadDelete {
                remote: objects::object::RemoteName::new(remote),
                thread: thread.to_string(),
                state: *state,
            }],
            scope,
        )?;
        Ok(ids[0])
    }

    /// Record an undo-recovery pointer publish. Local refs pass `op_scope()`.
    fn record_undo_recovery_update(&self, state: &ChangeId, scope: Option<&Scope>) -> Result<u64> {
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

    /// Record a redaction declaration. The blob bytes stay on disk; `Purge` is
    /// the separate, irreversible step that removes them.
    fn record_redact(
        &self,
        redaction_id: &ContentHash,
        blob: &ContentHash,
        state: &ChangeId,
        path: &str,
        scope: Option<&Scope>,
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

    /// Record a purge. The underlying blob bytes were physically removed from
    /// local storage. The associated `Redaction` record stays in place.
    fn record_purge(
        &self,
        redaction_id: &ContentHash,
        blob: &ContentHash,
        scope: Option<&Scope>,
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

    /// Record a state-visibility declaration. `sidecar` carries the full
    /// per-state sidecar bytes before/after the put so undo/redo can restore it.
    fn record_state_visibility_set(
        &self,
        state: &ChangeId,
        record_id: &ContentHash,
        tier: &VisibilityTier,
        sidecar: VisibilitySidecarSnapshots,
        scope: Option<&Scope>,
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

    /// Record a state-visibility promotion. `sidecar` snapshots the whole
    /// per-state sidecar before/after the put so undo/redo can restore it.
    fn record_state_visibility_promote(
        &self,
        state: &ChangeId,
        superseded: &ContentHash,
        record_id: &ContentHash,
        tier: &VisibilityTier,
        sidecar: VisibilitySidecarSnapshots,
        scope: Option<&Scope>,
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

    /// Record a fast-forward merge. Both ends of the FF are recorded so neither
    /// inverse has to re-resolve source/target tips at apply time.
    fn record_fast_forward(
        &self,
        source_thread: &ThreadName,
        target_thread: &ThreadName,
        pre_target_id: &ChangeId,
        post_target_id: &ChangeId,
        scope: Option<&Scope>,
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

impl<T: BlockingOpLogBackend + ?Sized> BlockingOpLogRecorder for T {}
