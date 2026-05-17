// SPDX-License-Identifier: Apache-2.0
//! Types for the operation log.

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
    Fork { from: ChangeId, new_state: ChangeId },
    /// Collapse operation.
    Collapse {
        sources: Vec<ChangeId>,
        result: ChangeId,
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
    /// Fast-forward merge: `target_thread` advanced from `pre_target_id`
    /// to `source_thread`'s tip without writing a synthetic merge state.
    /// `source_thread` is untouched.
    ///
    /// Distinct from `Goto` so undo can restore both HEAD *and* the
    /// target thread ref. The `Goto` inverse only rewinds HEAD, which
    /// stranded the merged-into thread ref at the FF target — the bug
    /// closed by heddle#99 and previously pinned by
    /// `test_undo_ff_merge_restores_head_but_strands_thread_ref`.
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
    pub actor: Principal,
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
