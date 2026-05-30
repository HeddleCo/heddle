// SPDX-License-Identifier: Apache-2.0
//! Types for the operation log.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use objects::object::{ChangeId, ContentHash, OperationId, Principal};
use serde::{Deserialize, Serialize};

/// Record of an operation that can be undone.
///
/// Variants must be appended at the tail. rmp-serde encodes enum variants by
/// discriminant index, so reordering or inserting in the middle would break
/// every pre-existing on-disk oplog entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OpRecord {
    /// Snapshot operation.
    Snapshot {
        new_state: ChangeId,
        prev_head: Option<ChangeId>,
        thread: Option<String>,
    },
    /// Goto operation.
    Goto {
        target: ChangeId,
        prev_head: Option<ChangeId>,
    },
    /// Thread creation.
    ThreadCreate { name: String, state: ChangeId },
    /// Thread deletion.
    ThreadDelete { name: String, state: ChangeId },
    /// Thread update.
    ThreadUpdate {
        name: String,
        old_state: ChangeId,
        new_state: ChangeId,
    },
    /// Fork operation.
    ///
    /// `from` is the source state forked from; `new_state` is the fork
    /// result. `thread`/`head` name the ref the fork *published* so a
    /// crash-replay (oplog committed, ref not yet materialized) can
    /// re-derive which ref to publish (heddle#330 write chokepoint):
    /// `thread = Some(name)` when the fork attached HEAD to a new thread,
    /// `head = Some(state)` when it detached HEAD at the fork result.
    /// These published-ref fields — not the `from`/`new_state` positional
    /// pair — are the authoritative replay/materialization target.
    Fork {
        from: ChangeId,
        new_state: ChangeId,
        #[serde(default)]
        thread: Option<String>,
        #[serde(default)]
        head: Option<ChangeId>,
    },
    /// Collapse operation. `thread` names the published ref: `Some(name)`
    /// when the collapse published a thread ref, `None` when it published
    /// a detached HEAD at `result` (heddle#330 write chokepoint — the
    /// published-ref discriminant replay needs to materialize the ref).
    Collapse {
        sources: Vec<ChangeId>,
        result: ChangeId,
        #[serde(default)]
        thread: Option<String>,
    },
    /// Marker creation.
    MarkerCreate { name: String, state: ChangeId },
    /// Marker deletion.
    MarkerDelete { name: String, state: ChangeId },
    // --- Agent-first tail variants below; new variants append here. ---
    /// Cheap addressable save intended for agent-style frequent saves.
    /// Distinct from `Snapshot` so `heddle log --no-checkpoints` (the human
    /// default) can filter them without losing the ability to `goto` them.
    Checkpoint {
        parent: Option<ChangeId>,
        state: ChangeId,
        thread: Option<String>,
    },
    /// Recorded when a transaction is aborted. The buffered ops the
    /// transaction would have applied are listed for forensic replay; no
    /// state was actually committed.
    TransactionAbort {
        transaction_id: String,
        reason: String,
    },
    /// Recorded when an ephemeral thread's TTL elapses and it auto-collapses.
    /// The states behind the thread remain addressable; only the thread
    /// pointer is retired.
    EphemeralThreadCollapse {
        thread: String,
        final_state: ChangeId,
    },
    /// Recorded when a structured conflict is resolved through
    /// `ConflictService::Resolve` (or its CLI front-end). Carries the
    /// addressable conflict id rather than the path so agents can correlate
    /// across calls.
    ConflictResolved {
        conflict_id: String,
        resolution: String,
    },
    /// Recorded when a transaction is successfully committed. The number
    /// of buffered ops at commit time is captured so the audit trail
    /// shows how much work was folded in (real per-op replay is the
    /// next follow-on; today it is the count, not the records).
    TransactionCommit {
        transaction_id: String,
        op_count: u32,
    },
    /// A redaction was declared on a blob in a specific state. The blob
    /// bytes are still on disk; readers see the stub from the
    /// `Redaction` object instead.
    ///
    /// Reversible via `heddle undo --allow-redact-undo`. The inverse
    /// removes the specific `Redaction` record from the per-blob
    /// sidecar so subsequent materializes restore the original bytes.
    /// The opt-in flag exists because the inverse re-exposes
    /// previously-hidden content; a casual `heddle undo` chain refuses
    /// rather than silently unwind the stub-substitution.
    ///
    /// Refused regardless of the flag when the underlying bytes have
    /// since been purged: the `Redaction` record is then load-bearing
    /// audit trail for "these bytes were physically destroyed", and
    /// removing it would lie about local storage. `Purge` itself is
    /// irreversible.
    ///
    /// **Redo is not supported.** The OpRecord doesn't preserve the
    /// full `Redaction` (reason, redactor, signature, …), so `heddle
    /// redo` of an undone Redact refuses with a clear message rather
    /// than silently no-op. Re-run `heddle redact apply` to recreate.
    Redact {
        /// Content hash of the encoded `Redaction` object.
        redaction_id: ContentHash,
        /// Blob the redaction targets.
        blob: ContentHash,
        /// State that surfaces the redacted file.
        state: ChangeId,
        /// Path within the state's tree.
        path: String,
    },
    /// The underlying blob bytes referenced by an earlier redaction were
    /// physically removed from local storage. The Redaction record is
    /// preserved; only the bytes are gone. Non-reversible by design —
    /// `heddle undo` on a Purge fails with a clear message.
    Purge {
        /// Content hash of the `Redaction` whose bytes were purged.
        redaction_id: ContentHash,
        /// Blob hash whose bytes were physically removed.
        blob: ContentHash,
    },
    /// Fast-forward merge — **legacy V1, read-only.** Predates the
    /// heddle#99 r2 redo-determinism fix and is no longer emitted; new
    /// recordings use [`FastForwardV2`].
    ///
    /// Lacks `post_target_id`, so a redo of this variant has to
    /// re-resolve `source_thread → tip` at apply time — non-deterministic
    /// if the source thread advanced or was deleted between undo and
    /// redo. The undo direction is fine (uses `pre_target_id` directly).
    /// Records of this shape age out as the live oplog window slides
    /// forward.
    FastForward {
        /// The thread that was merged in. Its ref never moves during
        /// an FF merge — recorded only for forensic context.
        source_thread: String,
        /// The thread that fast-forwarded. Undo restores this ref to
        /// `pre_target_id`.
        target_thread: String,
        /// `target_thread`'s tip before the FF. Undo restores both
        /// HEAD and the target thread ref to this state.
        pre_target_id: ChangeId,
    },
    /// Fast-forward merge: `target_thread` advanced from `pre_target_id`
    /// to `post_target_id` (the source's tip at the time of the FF)
    /// without writing a synthetic merge state. `source_thread` is
    /// untouched throughout.
    ///
    /// Distinct from `Goto` so undo can restore both HEAD *and* the
    /// target thread ref. The `Goto` inverse only rewinds HEAD, which
    /// stranded the merged-into thread ref at the FF target — the bug
    /// closed by heddle#99 r1.
    ///
    /// V2 (heddle#99 r2) adds `post_target_id` so redo replays the
    /// recorded operation byte-for-byte instead of re-resolving
    /// `source_thread → tip` at apply time. The r1 redo was
    /// non-deterministic: if the source thread had advanced after
    /// undo, redo silently pulled in commits that were never part of
    /// the original merge; if the source thread had been deleted,
    /// redo errored even though the merged state is recoverable from
    /// the recorded SHA. `source_thread` is kept for forensic context
    /// only — neither inverse reads it.
    FastForwardV2 {
        /// The thread that was merged in. Forensic-only — neither
        /// undo nor redo reads it.
        source_thread: String,
        /// The thread that fast-forwarded. Undo restores this ref to
        /// `pre_target_id`; redo advances it to `post_target_id`.
        target_thread: String,
        /// `target_thread`'s tip before the FF. Undo target.
        pre_target_id: ChangeId,
        /// `target_thread`'s tip after the FF (the source's tip at
        /// recording time). Redo target — recorded so replay is
        /// deterministic regardless of what `source_thread` does
        /// later.
        post_target_id: ChangeId,
    },
    /// Thread creation — V2 with a ThreadManager record snapshot so redo
    /// can recreate the record body after undo destroyed it.
    ///
    /// V1 (`ThreadCreate`) carried only `(name, state)`. The undo inverse
    /// added under heddle#23 r1 also deletes the matching ThreadManager
    /// record so refs and record-store state stay in lockstep (contract
    /// rule 4). That left redo broken: `apply_redo_entry` could only
    /// restore the ref via `set_thread` — the record body (mode,
    /// execution_path, materialized_path, base_state, base_root, …) was
    /// gone and not reconstructible from V1's `(name, state)`. Record-
    /// backed commands (`thread cd`, delegate, integration policy) then
    /// silently degraded after an undo→redo round-trip. heddle#23 r2
    /// Codex P1 (PR #112, thread 3254698975).
    ///
    /// Same hazard shape as heddle#99 r2 (FastForward → FastForwardV2
    /// with `post_target_id`): undo destroys state that redo cannot
    /// reconstruct from the OpRecord alone. The fix is the same — record
    /// what redo needs.
    ///
    /// `manager_snapshot` is opaque rmp-serde bytes of the `Thread`
    /// record body. Opaque to keep the `oplog` crate independent of
    /// `repo`-level types; the `repo` crate owns the encoding via
    /// `ThreadManager::snapshot_thread_record` /
    /// `ThreadManager::restore_thread_record_from_snapshot`. `None` for
    /// callsites that don't write a ThreadManager record alongside the
    /// op (rename batch's new-name arm, ingest, harness/agent stubs).
    ///
    /// V1 records remain readable: `apply_undo_entry` keeps its V1 arm,
    /// and `apply_redo_entry` falls back to ref-only restore with a
    /// stderr warning so legacy oplog entries don't error — they
    /// degrade gracefully as the live window slides forward.
    ThreadCreateV2 {
        name: String,
        state: ChangeId,
        /// rmp-serde-encoded `Thread` record body, or `None` when no
        /// record was written by the forward path.
        manager_snapshot: Option<Vec<u8>>,
    },
    /// Git-overlay checkpoint written to the real Git checkout.
    GitCheckpoint {
        branch: String,
        state: ChangeId,
        previous_git_oid: Option<String>,
        new_git_oid: String,
    },
    /// A remote-thread ref was published (heddle#330 r9). Before this
    /// variant `set_remote_thread` wrote the ref directly with no
    /// committed record, so reconciliation of the remote-thread class
    /// folded an empty tail. Recording the publish makes that
    /// reconciliation non-vacuous and lets crash-replay re-materialize
    /// the ref from its newest in-scope record.
    RemoteThreadUpdate {
        remote: String,
        thread: String,
        state: ChangeId,
    },
    /// A remote-thread ref was deleted (heddle#330 r9). Folded like a
    /// `MarkerDelete`: drops the name from the reconciled remote-thread
    /// set. `state` is the value at delete time (forensic context).
    RemoteThreadDelete {
        remote: String,
        thread: String,
        state: ChangeId,
    },
    /// The heddle-internal pre-undo recovery pointer was set (heddle#330
    /// r9). A single rolling ORIG_HEAD-style pointer with no delete path,
    /// so one update variant suffices. Local (per-checkout) ref —
    /// reconciles within its own `op_scope`.
    UndoRecoveryUpdate { state: ChangeId },
}

impl OpRecord {
    /// Get a short description of the operation.
    pub fn description(&self) -> String {
        match self {
            OpRecord::Snapshot {
                new_state, thread, ..
            } => match thread {
                Some(thread) => format!("snapshot {} on {}", new_state.short(), thread),
                None => format!("snapshot {}", new_state.short()),
            },
            OpRecord::Goto { target, .. } => {
                format!("goto {}", target.short())
            }
            OpRecord::ThreadCreate { name, .. } => {
                format!("create thread {}", name)
            }
            OpRecord::ThreadDelete { name, .. } => {
                format!("delete thread {}", name)
            }
            OpRecord::ThreadUpdate {
                name, new_state, ..
            } => {
                format!("update thread {} -> {}", name, new_state.short())
            }
            OpRecord::Fork { new_state, .. } => {
                format!("fork -> {}", new_state.short())
            }
            OpRecord::Collapse { result, .. } => {
                format!("collapse -> {}", result.short())
            }
            OpRecord::MarkerCreate { name, .. } => {
                format!("create marker {}", name)
            }
            OpRecord::MarkerDelete { name, .. } => {
                format!("delete marker {}", name)
            }
            OpRecord::Checkpoint { state, thread, .. } => match thread {
                Some(thread) => format!("checkpoint {} on {}", state.short(), thread),
                None => format!("checkpoint {}", state.short()),
            },
            OpRecord::TransactionAbort { transaction_id, .. } => {
                format!("transaction abort {}", transaction_id)
            }
            OpRecord::EphemeralThreadCollapse { thread, .. } => {
                format!("collapse ephemeral thread {}", thread)
            }
            OpRecord::ConflictResolved { conflict_id, .. } => {
                format!("resolve conflict {}", conflict_id)
            }
            OpRecord::TransactionCommit {
                transaction_id,
                op_count,
            } => {
                format!("transaction commit {} ({} ops)", transaction_id, op_count)
            }
            OpRecord::Redact {
                redaction_id,
                blob,
                state,
                path,
            } => {
                format!(
                    "redact {} on {} (blob {}, redaction {})",
                    path,
                    state.short(),
                    blob.short(),
                    redaction_id.short()
                )
            }
            OpRecord::Purge { redaction_id, blob } => {
                format!(
                    "purge blob {} (redaction {})",
                    blob.short(),
                    redaction_id.short()
                )
            }
            OpRecord::FastForward {
                source_thread,
                target_thread,
                pre_target_id,
            } => {
                format!(
                    "fast-forward {} into {} (was at {})",
                    source_thread,
                    target_thread,
                    pre_target_id.short()
                )
            }
            OpRecord::FastForwardV2 {
                source_thread,
                target_thread,
                pre_target_id,
                post_target_id,
            } => {
                format!(
                    "fast-forward {} into {} ({} -> {})",
                    source_thread,
                    target_thread,
                    pre_target_id.short(),
                    post_target_id.short()
                )
            }
            OpRecord::ThreadCreateV2 { name, .. } => {
                format!("create thread {}", name)
            }
            OpRecord::GitCheckpoint {
                branch,
                previous_git_oid,
                new_git_oid,
                ..
            } => {
                format!(
                    "git checkpoint {} ({} -> {})",
                    branch,
                    previous_git_oid.as_deref().unwrap_or("(none)"),
                    new_git_oid
                )
            }
            OpRecord::RemoteThreadUpdate {
                remote,
                thread,
                state,
            } => {
                format!("update remote thread {}/{} -> {}", remote, thread, state.short())
            }
            OpRecord::RemoteThreadDelete { remote, thread, .. } => {
                format!("delete remote thread {}/{}", remote, thread)
            }
            OpRecord::UndoRecoveryUpdate { state } => {
                format!("set undo-recovery -> {}", state.short())
            }
        }
    }
}

/// Entry in the operation log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpEntry {
    /// Sequential ID.
    pub id: u64,
    /// Timestamp.
    pub timestamp: DateTime<Utc>,
    /// The operation.
    pub operation: OpRecord,
    /// Whether this operation has been undone.
    pub undone: bool,
    /// Batch identifier (same for grouped operations).
    #[serde(default)]
    pub batch_id: u64,
    /// Index within the batch.
    #[serde(default)]
    pub batch_index: u32,
    /// Checkout/lane scope that recorded this operation.
    #[serde(default)]
    pub scope: Option<String>,
    /// Principal that performed this operation. Required; every callsite
    /// that records an `OpEntry` must supply a real actor (typically the
    /// repository's configured principal).
    pub actor: Arc<Principal>,
    /// Client-supplied operation id, when available. The dedup store uses
    /// this to make repeated calls with the same id idempotent. `None`
    /// when the caller didn't supply one (no dedup applied).
    #[serde(default)]
    pub operation_id: Option<OperationId>,
}

/// Group of operations recorded together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpBatch {
    /// Batch identifier.
    pub id: u64,
    /// Operations in the batch.
    pub entries: Vec<OpEntry>,
}
