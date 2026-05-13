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
    /// `Redaction` object instead. Reversible via `heddle undo` (the
    /// reversal stops materialize from substituting the stub but doesn't
    /// touch any later Purge — Purge is irreversible).
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