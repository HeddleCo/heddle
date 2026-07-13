// SPDX-License-Identifier: Apache-2.0
//! Types for the operation log.

use std::{collections::BTreeSet, sync::Arc};

use chrono::{DateTime, Utc};
use objects::object::{ContentHash, OperationId, Principal, StateId, VisibilityTier};
use serde::{Deserialize, Serialize};

/// Logical key used by conditional transaction commits to detect intervening
/// same-thread changes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum IsolationKey {
    Thread(String),
    LocalHead {
        scope: String,
    },
    /// Per-state visibility key (heddle#317). Every `StateVisibilitySet` /
    /// `StateVisibilityPromote` record on a state contributes this key, so a
    /// visibility mutation on state `S` conflicts with an in-flight undo/redo of
    /// a visibility batch that also touched `S`. Without it a visibility record
    /// would contribute no isolation key, and an undo could restore an older
    /// `prior_sidecar` over a concurrently-committed newer visibility record,
    /// silently discarding it. Keyed by the state's `StateId`, so visibility
    /// mutations on *different* states never spuriously conflict.
    StateVisibility(StateId),
}

/// The oplog generation and logical keys a transaction observed before apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationPrecondition {
    pub since_head_id: u64,
    pub keys: BTreeSet<IsolationKey>,
}

/// Result of an exact-once append guarded by an isolation precondition.
#[derive(Debug, Clone)]
pub enum ConditionalCommitOutcome {
    Committed(Vec<u64>),
    AlreadyCommitted(Vec<OpRecord>),
    IsolationConflict {
        key: IsolationKey,
        since_head_id: u64,
        conflicting_entry_id: u64,
    },
}

/// Record of an operation that can be undone.
///
/// Variants must be appended at the tail. rmp-serde encodes enum variants by
/// discriminant index, so reordering or inserting in the middle would break
/// every pre-existing on-disk oplog entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OpRecord {
    /// Snapshot operation.
    Snapshot {
        new_state: StateId,
        prev_head: Option<StateId>,
        /// Detached HEAD published by this snapshot, if any. Attached
        /// snapshots publish their `thread` ref instead; detached snapshots
        /// publish `HEAD = Detached(head)`.
        head: Option<StateId>,
        thread: Option<String>,
    },
    /// Goto operation.
    Goto {
        target: StateId,
        prev_head: Option<StateId>,
        /// HEAD published by this goto. This intentionally duplicates `target`
        /// for the current detached-only command shape so crash replay folds the
        /// published ref state directly instead of inferring it from intent.
        head: StateId,
    },
    /// Thread creation.
    ///
    /// `manager_snapshot` is opaque rmp-serde bytes of the `Thread`
    /// record body. Opaque to keep the `oplog` crate independent of
    /// `repo`-level types; the `repo` crate owns the encoding via
    /// `ThreadManager::snapshot_thread_record` /
    /// `ThreadManager::decode_thread_record_snapshot`. `None` for
    /// callsites that don't write a ThreadManager record alongside the
    /// op (rename batch's new-name arm, ingest, harness/agent stubs).
    ThreadCreate {
        name: String,
        state: StateId,
        /// rmp-serde-encoded `Thread` record body, or `None` when no
        /// record was written by the forward path.
        manager_snapshot: Option<Vec<u8>>,
    },
    /// Thread deletion.
    ThreadDelete { name: String, state: StateId },
    /// Thread update.
    ThreadUpdate {
        name: String,
        old_state: StateId,
        new_state: StateId,
        /// rmp-serde-encoded `Thread` record bodies around the update.
        ///
        /// This is intentionally one sparse tail field rather than two
        /// independently-skipped fields: rmp-serde encodes enum variant fields
        /// positionally, so skipping only the old slot would shift the new
        /// snapshot into the old position for older readers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        manager_snapshots: Option<ThreadUpdateSnapshots>,
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
        from: StateId,
        new_state: StateId,
        #[serde(default)]
        thread: Option<String>,
        #[serde(default)]
        head: Option<StateId>,
    },
    /// Collapse operation. `thread` names the published ref: `Some(name)`
    /// when the collapse published a thread ref, `None` when it published
    /// a detached HEAD at `result` (heddle#330 write chokepoint — the
    /// published-ref discriminant replay needs to materialize the ref).
    Collapse {
        sources: Vec<StateId>,
        result: StateId,
        #[serde(default)]
        thread: Option<String>,
        #[serde(default)]
        pre_thread_state: Option<StateId>,
    },
    /// Marker creation.
    MarkerCreate { name: String, state: StateId },
    /// Marker deletion.
    MarkerDelete { name: String, state: StateId },
    // --- Agent-first tail variants below; new variants append here. ---
    /// Cheap addressable save intended for agent-style frequent saves.
    /// Distinct from `Snapshot` so `heddle log --no-checkpoints` (the human
    /// default) can filter them without losing the ability to `goto` them.
    Checkpoint {
        parent: Option<StateId>,
        state: StateId,
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
        final_state: StateId,
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
        state: StateId,
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
    /// to `post_target_id` (the source's tip at the time of the FF)
    /// without writing a synthetic merge state. `source_thread` is
    /// untouched throughout.
    ///
    /// Distinct from `Goto` so undo can restore both HEAD *and* the
    /// target thread ref. The `Goto` inverse only rewinds HEAD, which
    /// stranded the merged-into thread ref at the FF target — the bug
    /// closed by heddle#99 r1.
    ///
    /// `post_target_id` makes redo deterministic: it replays the recorded
    /// operation byte-for-byte instead of re-resolving `source_thread → tip`
    /// at apply time. `source_thread` is kept for forensic context only —
    /// neither inverse reads it.
    FastForward {
        /// The thread that was merged in. Forensic-only — neither
        /// undo nor redo reads it.
        source_thread: String,
        /// The thread that fast-forwarded. Undo restores this ref to
        /// `pre_target_id`; redo advances it to `post_target_id`.
        target_thread: String,
        /// `target_thread`'s tip before the FF. Undo target.
        pre_target_id: StateId,
        /// `target_thread`'s tip after the FF (the source's tip at
        /// recording time). Redo target — recorded so replay is
        /// deterministic regardless of what `source_thread` does
        /// later.
        post_target_id: StateId,
    },
    /// Git-overlay checkpoint written to the real Git checkout.
    GitCheckpoint {
        branch: String,
        state: StateId,
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
        state: StateId,
    },
    /// A remote-thread ref was deleted (heddle#330 r9). Folded like a
    /// `MarkerDelete`: drops the name from the reconciled remote-thread
    /// set. `state` is the value at delete time (forensic context).
    RemoteThreadDelete {
        remote: String,
        thread: String,
        state: StateId,
    },
    /// The heddle-internal pre-undo recovery pointer was set (heddle#330
    /// r9). A single rolling ORIG_HEAD-style pointer with no delete path,
    /// so one update variant suffices. Local (per-checkout) ref —
    /// reconciles within its own `op_scope`.
    UndoRecoveryUpdate { state: StateId },
    /// A visibility tier was declared on a state (heddle#317). Audit-trail
    /// companion to the per-state `StateVisibility` sidecar record: the
    /// sidecar is the authoritative effective tier; this oplog entry records
    /// who bound it and when. Emitted both by `heddle visibility set` and by
    /// the Invariant-A capture-time binding (spike #266 §5.4).
    ///
    /// Reversible: `prior_sidecar`/`new_sidecar` carry the FULL per-state
    /// visibility sidecar bytes (or `None` for public-by-absence) immediately
    /// before and after the put. Undo restores `prior_sidecar`, redo restores
    /// `new_sidecar` — both absolute write-or-remove, mirroring the redaction
    /// sidecar capture-restore. Without the before-image the undo path could
    /// only no-op, leaving the oplog and the sidecar divergent (PR #529 P1).
    StateVisibilitySet {
        state: StateId,
        /// Content id of the persisted `StateVisibility` record.
        record_id: ContentHash,
        /// The tier declared.
        tier: VisibilityTier,
        /// Full sidecar bytes before the put (`None` = public-by-absence).
        /// Undo target.
        #[serde(default)]
        prior_sidecar: Option<Vec<u8>>,
        /// Full sidecar bytes after the put (`None` = public-by-absence).
        /// Redo target.
        #[serde(default)]
        new_sidecar: Option<Vec<u8>>,
    },
    /// A state's visibility was promoted to a less-restrictive tier by
    /// appending a superseding `StateVisibility` record (heddle#317).
    /// Reversible the same way as [`StateVisibilitySet`](Self::StateVisibilitySet):
    /// `prior_sidecar`/`new_sidecar` snapshot the whole sidecar around the put.
    StateVisibilityPromote {
        state: StateId,
        /// The prior record this promotion supersedes.
        superseded: ContentHash,
        /// Content id of the new, superseding record.
        record_id: ContentHash,
        /// The tier promoted to.
        tier: VisibilityTier,
        /// Full sidecar bytes before the put (`None` = public-by-absence).
        /// Undo target.
        #[serde(default)]
        prior_sidecar: Option<Vec<u8>>,
        /// Full sidecar bytes after the put (`None` = public-by-absence).
        /// Redo target.
        #[serde(default)]
        new_sidecar: Option<Vec<u8>>,
    },
}

/// True when `record` is the atomic transaction commit marker.
///
/// A `TransactionCommit` marker carries no user-facing operation: it is the
/// atomic commit sentinel, not a forward op. Undo/redo eligibility scans ignore
/// it so a record-less transaction is never itself selected as an undoable or
/// redoable unit. A batch with at least one non-marker entry still qualifies on
/// that entry.
pub fn is_transaction_commit(record: &OpRecord) -> bool {
    matches!(record, OpRecord::TransactionCommit { .. })
}

/// True when `record` is the atomic transaction commit marker for
/// `transaction_id`.
pub fn is_transaction_commit_for(record: &OpRecord, transaction_id: &str) -> bool {
    matches!(
        record,
        OpRecord::TransactionCommit {
            transaction_id: id,
            ..
        } if id == transaction_id
    )
}

/// Optional ThreadManager record snapshots captured around a [`ThreadUpdate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadUpdateSnapshots {
    /// rmp-serde-encoded `Thread` record body before the update, or `None`
    /// when no record existed before the forward path.
    pub old: Option<Vec<u8>>,
    /// rmp-serde-encoded `Thread` record body after the update, or `None`
    /// when the forward path only moved the ref.
    pub new: Option<Vec<u8>>,
    /// Complete same-thread record set before the update. Empty for records
    /// written before duplicate-record convergence needed full-set restore.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub old_records: Vec<Vec<u8>>,
    /// Complete same-thread record set after the update. Empty for records
    /// written before duplicate-record convergence needed full-set restore.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub new_records: Vec<Vec<u8>>,
    /// Whether the thread ref was absent before the update. This lets undo of
    /// a metadata-only repair remove a recreated ref instead of setting it to a
    /// fallback state that was only used to identify the record.
    #[serde(default, skip_serializing_if = "is_false")]
    pub old_ref_absent: bool,
}

impl ThreadUpdateSnapshots {
    pub fn from_parts(old: Option<Vec<u8>>, new: Option<Vec<u8>>) -> Option<Self> {
        if old.is_none() && new.is_none() {
            None
        } else {
            Some(Self {
                old,
                new,
                old_records: Vec::new(),
                new_records: Vec::new(),
                old_ref_absent: false,
            })
        }
    }

    pub fn from_record_sets(
        old: Option<Vec<u8>>,
        new: Option<Vec<u8>>,
        old_records: Vec<Vec<u8>>,
        new_records: Vec<Vec<u8>>,
        old_ref_absent: bool,
    ) -> Option<Self> {
        if old.is_none()
            && new.is_none()
            && old_records.is_empty()
            && new_records.is_empty()
            && !old_ref_absent
        {
            None
        } else {
            Some(Self {
                old,
                new,
                old_records,
                new_records,
                old_ref_absent,
            })
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The logical isolation keys touched by one committed record.
///
/// This is intentionally explicit and shared by the backends and tests. Records
/// that do not publish or read a thread/head key return an empty set.
pub fn isolation_keys_for_record(record: &OpRecord, scope: Option<&str>) -> BTreeSet<IsolationKey> {
    let mut keys = BTreeSet::new();
    match record {
        OpRecord::Snapshot {
            thread: Some(thread),
            ..
        }
        | OpRecord::Checkpoint {
            thread: Some(thread),
            ..
        }
        | OpRecord::ThreadCreate { name: thread, .. }
        | OpRecord::ThreadDelete { name: thread, .. }
        | OpRecord::ThreadUpdate { name: thread, .. }
        | OpRecord::EphemeralThreadCollapse { thread, .. }
        | OpRecord::GitCheckpoint { branch: thread, .. }
        | OpRecord::RemoteThreadUpdate { thread, .. }
        | OpRecord::RemoteThreadDelete { thread, .. } => {
            keys.insert(IsolationKey::Thread(thread.clone()));
        }
        OpRecord::Snapshot { thread: None, .. }
        | OpRecord::Checkpoint { thread: None, .. }
        | OpRecord::Goto { .. }
        | OpRecord::UndoRecoveryUpdate { .. } => {
            if let Some(scope) = scope {
                keys.insert(IsolationKey::LocalHead {
                    scope: scope.to_string(),
                });
            }
        }
        OpRecord::FastForward {
            source_thread,
            target_thread,
            ..
        } => {
            keys.insert(IsolationKey::Thread(source_thread.clone()));
            keys.insert(IsolationKey::Thread(target_thread.clone()));
        }
        OpRecord::Fork {
            thread: Some(thread),
            ..
        }
        | OpRecord::Collapse {
            thread: Some(thread),
            ..
        } => {
            keys.insert(IsolationKey::Thread(thread.clone()));
        }
        OpRecord::Fork {
            thread: None, head, ..
        } => {
            if head.is_some()
                && let Some(scope) = scope
            {
                keys.insert(IsolationKey::LocalHead {
                    scope: scope.to_string(),
                });
            }
        }
        OpRecord::Collapse { thread: None, .. } => {
            if let Some(scope) = scope {
                keys.insert(IsolationKey::LocalHead {
                    scope: scope.to_string(),
                });
            }
        }
        // Visibility mutations contribute a per-state key (heddle#317 inv 3) so
        // an undo/redo of a visibility batch on `state` conflicts with any
        // concurrent visibility mutation on the SAME state, and never silently
        // overwrites a newer record with a stale `prior_sidecar`.
        OpRecord::StateVisibilitySet { state, .. }
        | OpRecord::StateVisibilityPromote { state, .. } => {
            keys.insert(IsolationKey::StateVisibility(*state));
        }
        OpRecord::MarkerCreate { .. }
        | OpRecord::MarkerDelete { .. }
        | OpRecord::TransactionAbort { .. }
        | OpRecord::ConflictResolved { .. }
        | OpRecord::TransactionCommit { .. }
        | OpRecord::Redact { .. }
        | OpRecord::Purge { .. } => {}
    }
    keys
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
                format!(
                    "update remote thread {}/{} -> {}",
                    remote,
                    thread,
                    state.short()
                )
            }
            OpRecord::RemoteThreadDelete { remote, thread, .. } => {
                format!("delete remote thread {}/{}", remote, thread)
            }
            OpRecord::UndoRecoveryUpdate { state } => {
                format!("set undo-recovery -> {}", state.short())
            }
            OpRecord::StateVisibilitySet { state, tier, .. } => {
                format!("set visibility {} -> {}", state.short(), tier.as_str())
            }
            OpRecord::StateVisibilityPromote { state, tier, .. } => {
                format!("promote visibility {} -> {}", state.short(), tier.as_str())
            }
        }
    }
}

/// Single source of truth for the oplog verb vocabulary (heddle#354 r9).
///
/// Every emitted "kind" string — the daemon op-log index verbs, the
/// `heddle watch --filter` keywords, the `heddle log` verb filter — derives
/// from this one catalog, so the verb set can never drift away from the
/// `OpRecord` variant set. The generated [`OpRecord::verb`] /
/// [`OpRecord::is_checkpoint_verb`] are **exhaustive** matches: adding an
/// `OpRecord` variant is a COMPILE error until it is named here, and the
/// derived [`OpRecord::verbs`] / [`OP_VERB_CATALOG`] pick it up automatically.
/// This closes the "a new variant is added but not propagated to every
/// consumer" class for the verb-vocabulary consumers — they no longer keep
/// hand-maintained string lists that silently fall out of sync.
macro_rules! op_verb_catalog {
    ( $( $variant:ident => ($verb:literal, checkpoint = $ckpt:literal) ),+ $(,)? ) => {
        impl OpRecord {
            /// The stable snake-case verb for this record's variant. Exhaustive
            /// match — a new `OpRecord` variant fails to compile until it has a
            /// verb here. Verbs are shared across variants that fold to one
            /// concept.
            pub fn verb(&self) -> &'static str {
                match self {
                    $( OpRecord::$variant { .. } => $verb, )+
                }
            }

            /// True iff this is the agent-style [`OpRecord::Checkpoint`] that
            /// `heddle log --no-checkpoints` (the human default) and the daemon
            /// op-log query hide. New variants surface by default — only the
            /// catalog entries flagged `checkpoint = true` are hidden.
            pub fn is_checkpoint_verb(&self) -> bool {
                match self {
                    $( OpRecord::$variant { .. } => $ckpt, )+
                }
            }
        }

        /// Every `(verb, is_checkpoint)` pair the vocabulary contains, in
        /// variant-declaration order. Generated from the same catalog as
        /// [`OpRecord::verb`], so it tracks the variant set with no drift. A
        /// verb shared by several variants appears once per variant; dedup at
        /// the use site (see [`OpRecord::verbs`]).
        pub const OP_VERB_CATALOG: &[(&str, bool)] = &[ $( ($verb, $ckpt) ),+ ];
    };
}

op_verb_catalog! {
    Snapshot => ("snapshot", checkpoint = false),
    Goto => ("goto", checkpoint = false),
    ThreadCreate => ("thread_create", checkpoint = false),
    ThreadDelete => ("thread_delete", checkpoint = false),
    ThreadUpdate => ("thread_update", checkpoint = false),
    Fork => ("fork", checkpoint = false),
    Collapse => ("collapse", checkpoint = false),
    MarkerCreate => ("marker_create", checkpoint = false),
    MarkerDelete => ("marker_delete", checkpoint = false),
    Checkpoint => ("checkpoint", checkpoint = true),
    TransactionAbort => ("transaction_abort", checkpoint = false),
    EphemeralThreadCollapse => ("ephemeral_thread_collapse", checkpoint = false),
    ConflictResolved => ("conflict_resolved", checkpoint = false),
    TransactionCommit => ("transaction_commit", checkpoint = false),
    Redact => ("redact", checkpoint = false),
    Purge => ("purge", checkpoint = false),
    FastForward => ("fast_forward", checkpoint = false),
    GitCheckpoint => ("git_checkpoint", checkpoint = false),
    RemoteThreadUpdate => ("remote_thread_update", checkpoint = false),
    RemoteThreadDelete => ("remote_thread_delete", checkpoint = false),
    UndoRecoveryUpdate => ("undo_recovery_update", checkpoint = false),
    StateVisibilitySet => ("state_visibility_set", checkpoint = false),
    StateVisibilityPromote => ("state_visibility_promote", checkpoint = false),
}

impl OpRecord {
    /// The deduped verb vocabulary. With `include_checkpoints == false` the
    /// agent `checkpoint` verb is dropped (the `heddle log` human default and
    /// the daemon op-log query's default filter); every other verb — including
    /// any future variant — is surfaced. Derived from [`OP_VERB_CATALOG`], so a
    /// new variant joins the vocabulary the moment it has a catalog entry, with
    /// no hand-maintained list to forget. Order follows variant declaration.
    pub fn verbs(include_checkpoints: bool) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for &(verb, is_checkpoint) in OP_VERB_CATALOG {
            if (include_checkpoints || !is_checkpoint) && !out.contains(&verb) {
                out.push(verb);
            }
        }
        out
    }
}

/// How a record participates in undo's redaction-safety preflight.
///
/// Returned by [`OpRecord::redaction_undo_class`] so the CLI preflight reads
/// the classification off the variant instead of re-deriving it from an
/// exhaustive match (heddle#500). The borrowed fields are exactly what the
/// CLI needs to build its refusal messages and `redaction_is_purged` lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionUndoClass<'a> {
    /// `Purge` — irreversible; undo of the whole chain is refused.
    Purge { redaction_id: &'a ContentHash },
    /// `Redact` — reversible but gated: undo re-exposes hidden content, so it
    /// runs only with `--allow-redact-undo`, and is refused outright if the
    /// blob bytes have since been purged.
    Redact {
        blob: &'a ContentHash,
        state: &'a StateId,
        path: &'a str,
    },
    /// Every other record is irrelevant to redaction-undo safety.
    Other,
}

/// Per-variant undo/redo semantics, classified beside `OpRecord` so adding a
/// variant updates these rules in one place rather than editing CLI safety
/// matches (heddle#500, architecture-deepening C3). Each match enumerates
/// every variant with no wildcard, so the compiler forces a new variant to
/// declare its undo/redo semantics here.
impl OpRecord {
    /// State IDs the *undo* inverse must load from the object store. Variants
    /// whose undo is a no-op, only mutates sidecars/Git OIDs, or is
    /// irreversible return an empty list — they can't trip a missing-state
    /// reachability check. Enumerated explicitly (no wildcard) so a new
    /// state-carrying variant must declare what its undo needs to load
    /// (heddle#354 r9).
    pub fn states_required_for_undo(&self) -> Vec<StateId> {
        match self {
            OpRecord::Snapshot {
                prev_head: Some(prev),
                ..
            } => vec![*prev],
            OpRecord::Goto {
                prev_head: Some(prev),
                ..
            } => vec![*prev],
            OpRecord::ThreadDelete { state, .. } => vec![*state],
            OpRecord::ThreadUpdate { old_state, .. } => vec![*old_state],
            OpRecord::MarkerDelete { state, .. } => vec![*state],
            OpRecord::FastForward { pre_target_id, .. } => vec![*pre_target_id],
            OpRecord::Snapshot {
                prev_head: None, ..
            }
            | OpRecord::Goto {
                prev_head: None, ..
            }
            | OpRecord::ThreadCreate { .. }
            | OpRecord::Fork { .. }
            | OpRecord::Collapse { .. }
            | OpRecord::MarkerCreate { .. }
            | OpRecord::Checkpoint { .. }
            | OpRecord::TransactionAbort { .. }
            | OpRecord::EphemeralThreadCollapse { .. }
            | OpRecord::ConflictResolved { .. }
            | OpRecord::TransactionCommit { .. }
            | OpRecord::Redact { .. }
            | OpRecord::Purge { .. }
            | OpRecord::GitCheckpoint { .. }
            | OpRecord::RemoteThreadUpdate { .. }
            | OpRecord::RemoteThreadDelete { .. }
            | OpRecord::UndoRecoveryUpdate { .. }
            | OpRecord::StateVisibilitySet { .. }
            | OpRecord::StateVisibilityPromote { .. } => Vec::new(),
        }
    }

    /// State IDs the *redo* replay must load from the object store. Variants
    /// whose redo is a no-op, deletes a ref, or touches only sidecars/Git OIDs
    /// return an empty list. Enumerated explicitly so a new state-carrying
    /// variant must declare its redo target (heddle#354 r9).
    pub fn states_required_for_redo(&self) -> Vec<StateId> {
        match self {
            OpRecord::Snapshot { new_state, .. } => vec![*new_state],
            OpRecord::Goto { target, .. } => vec![*target],
            OpRecord::ThreadCreate { state, .. } => vec![*state],
            OpRecord::ThreadUpdate { new_state, .. } => vec![*new_state],
            OpRecord::MarkerCreate { state, .. } => vec![*state],
            OpRecord::FastForward { post_target_id, .. } => vec![*post_target_id],
            OpRecord::ThreadDelete { .. }
            | OpRecord::MarkerDelete { .. }
            | OpRecord::Fork { .. }
            | OpRecord::Collapse { .. }
            | OpRecord::Checkpoint { .. }
            | OpRecord::TransactionAbort { .. }
            | OpRecord::EphemeralThreadCollapse { .. }
            | OpRecord::ConflictResolved { .. }
            | OpRecord::TransactionCommit { .. }
            | OpRecord::Redact { .. }
            | OpRecord::Purge { .. }
            | OpRecord::GitCheckpoint { .. }
            | OpRecord::RemoteThreadUpdate { .. }
            | OpRecord::RemoteThreadDelete { .. }
            | OpRecord::UndoRecoveryUpdate { .. }
            | OpRecord::StateVisibilitySet { .. }
            | OpRecord::StateVisibilityPromote { .. } => Vec::new(),
        }
    }

    /// Label of the operation kind when this record has *no* faithful redo
    /// path, else `None`. `Redact`/`Purge` can't be replayed — the OpRecord
    /// doesn't preserve the full `Redaction` (reason, redactor, signature) and
    /// `Purge` is irreversible. Enumerated explicitly so a future variant
    /// without a redo path must be classified here (heddle#354 r9).
    pub fn redo_unsupported_label(&self) -> Option<&'static str> {
        match self {
            OpRecord::Redact { .. } => Some("Redact"),
            OpRecord::Purge { .. } => Some("Purge"),
            OpRecord::Snapshot { .. }
            | OpRecord::Goto { .. }
            | OpRecord::ThreadCreate { .. }
            | OpRecord::ThreadDelete { .. }
            | OpRecord::ThreadUpdate { .. }
            | OpRecord::Fork { .. }
            | OpRecord::Collapse { .. }
            | OpRecord::MarkerCreate { .. }
            | OpRecord::MarkerDelete { .. }
            | OpRecord::Checkpoint { .. }
            | OpRecord::TransactionAbort { .. }
            | OpRecord::EphemeralThreadCollapse { .. }
            | OpRecord::ConflictResolved { .. }
            | OpRecord::TransactionCommit { .. }
            | OpRecord::FastForward { .. }
            | OpRecord::GitCheckpoint { .. }
            | OpRecord::RemoteThreadUpdate { .. }
            | OpRecord::RemoteThreadDelete { .. }
            | OpRecord::UndoRecoveryUpdate { .. }
            | OpRecord::StateVisibilitySet { .. }
            | OpRecord::StateVisibilityPromote { .. } => None,
        }
    }

    /// This record's role in undo's redaction-safety preflight. Enumerated
    /// explicitly so a future redaction-adjacent variant must be classified
    /// here (heddle#354 r9).
    pub fn redaction_undo_class(&self) -> RedactionUndoClass<'_> {
        match self {
            OpRecord::Purge { redaction_id, .. } => RedactionUndoClass::Purge { redaction_id },
            OpRecord::Redact {
                blob, state, path, ..
            } => RedactionUndoClass::Redact { blob, state, path },
            OpRecord::Snapshot { .. }
            | OpRecord::Goto { .. }
            | OpRecord::ThreadCreate { .. }
            | OpRecord::ThreadDelete { .. }
            | OpRecord::ThreadUpdate { .. }
            | OpRecord::Fork { .. }
            | OpRecord::Collapse { .. }
            | OpRecord::MarkerCreate { .. }
            | OpRecord::MarkerDelete { .. }
            | OpRecord::Checkpoint { .. }
            | OpRecord::TransactionAbort { .. }
            | OpRecord::EphemeralThreadCollapse { .. }
            | OpRecord::ConflictResolved { .. }
            | OpRecord::TransactionCommit { .. }
            | OpRecord::FastForward { .. }
            | OpRecord::GitCheckpoint { .. }
            | OpRecord::RemoteThreadUpdate { .. }
            | OpRecord::RemoteThreadDelete { .. }
            | OpRecord::UndoRecoveryUpdate { .. }
            | OpRecord::StateVisibilitySet { .. }
            | OpRecord::StateVisibilityPromote { .. } => RedactionUndoClass::Other,
        }
    }

    /// The thread name if undoing this record carries the worktree-orphan
    /// hazard — i.e. a thread-create whose inverse removes the ref, leaving
    /// any materialized worktree orphaned. `None` for every other
    /// record. Enumerated explicitly so a future worktree-creating variant
    /// must be classified here (heddle#354 r9).
    pub fn thread_worktree_undo_hazard_name(&self) -> Option<&str> {
        match self {
            OpRecord::ThreadCreate { name, .. } => Some(name),
            OpRecord::Snapshot { .. }
            | OpRecord::Goto { .. }
            | OpRecord::ThreadDelete { .. }
            | OpRecord::ThreadUpdate { .. }
            | OpRecord::Fork { .. }
            | OpRecord::Collapse { .. }
            | OpRecord::MarkerCreate { .. }
            | OpRecord::MarkerDelete { .. }
            | OpRecord::Checkpoint { .. }
            | OpRecord::TransactionAbort { .. }
            | OpRecord::EphemeralThreadCollapse { .. }
            | OpRecord::ConflictResolved { .. }
            | OpRecord::TransactionCommit { .. }
            | OpRecord::Redact { .. }
            | OpRecord::Purge { .. }
            | OpRecord::FastForward { .. }
            | OpRecord::GitCheckpoint { .. }
            | OpRecord::RemoteThreadUpdate { .. }
            | OpRecord::RemoteThreadDelete { .. }
            | OpRecord::UndoRecoveryUpdate { .. }
            | OpRecord::StateVisibilitySet { .. }
            | OpRecord::StateVisibilityPromote { .. } => None,
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

impl OpBatch {
    /// True iff every entry is a [`OpRecord::TransactionCommit`] marker — the
    /// commit sentinel of an atomic transaction that staged only direct,
    /// already-durable effects and so contributed no domain record of its own
    /// (e.g. `undo`/`redo`, which navigate existing states and append no new
    /// record). Such a batch carries nothing to undo, redo, or surface in
    /// operation history; the undo/redo eligibility scans and the `undo --list`
    /// view filter it out so a record-less transaction's sentinel never
    /// pollutes the user-facing log.
    pub fn is_transaction_marker_only(&self) -> bool {
        !self.entries.is_empty()
            && self
                .entries
                .iter()
                .all(|entry| is_transaction_commit(&entry.operation))
    }
}

#[cfg(test)]
mod verb_catalog_tests {
    use objects::object::{ContentHash, StateId, VisibilityTier};

    use super::*;

    fn cid() -> StateId {
        StateId::from_bytes([7; 32])
    }

    fn hash() -> ContentHash {
        ContentHash::from_bytes([3; 32])
    }

    /// One representative of every `OpRecord` variant. The match is exhaustive,
    /// so adding a variant forces a new arm here — and the assertions below then
    /// prove the new variant has a non-empty verb that is present in the
    /// catalog, i.e. the vocabulary cannot silently drop it.
    fn one_per_variant() -> Vec<OpRecord> {
        let sample = OpRecord::Snapshot {
            new_state: cid(),
            prev_head: None,
            head: Some(cid()),
            thread: None,
        };
        // Exhaustiveness anchor: this match has no wildcard, so a new variant
        // breaks the build until it is added to `all` below too.
        match &sample {
            OpRecord::Snapshot { .. }
            | OpRecord::Goto { .. }
            | OpRecord::ThreadCreate { .. }
            | OpRecord::ThreadDelete { .. }
            | OpRecord::ThreadUpdate { .. }
            | OpRecord::Fork { .. }
            | OpRecord::Collapse { .. }
            | OpRecord::MarkerCreate { .. }
            | OpRecord::MarkerDelete { .. }
            | OpRecord::Checkpoint { .. }
            | OpRecord::TransactionAbort { .. }
            | OpRecord::EphemeralThreadCollapse { .. }
            | OpRecord::ConflictResolved { .. }
            | OpRecord::TransactionCommit { .. }
            | OpRecord::Redact { .. }
            | OpRecord::Purge { .. }
            | OpRecord::FastForward { .. }
            | OpRecord::GitCheckpoint { .. }
            | OpRecord::RemoteThreadUpdate { .. }
            | OpRecord::RemoteThreadDelete { .. }
            | OpRecord::UndoRecoveryUpdate { .. }
            | OpRecord::StateVisibilitySet { .. }
            | OpRecord::StateVisibilityPromote { .. } => {}
        }
        vec![
            sample,
            OpRecord::Goto {
                target: cid(),
                prev_head: None,
                head: cid(),
            },
            OpRecord::ThreadCreate {
                name: "t".into(),
                state: cid(),
                manager_snapshot: None,
            },
            OpRecord::ThreadDelete {
                name: "t".into(),
                state: cid(),
            },
            OpRecord::ThreadUpdate {
                name: "t".into(),
                old_state: cid(),
                new_state: cid(),
                manager_snapshots: None,
            },
            OpRecord::Fork {
                from: cid(),
                new_state: cid(),
                thread: None,
                head: None,
            },
            OpRecord::Collapse {
                sources: vec![cid()],
                result: cid(),
                thread: None,
                pre_thread_state: None,
            },
            OpRecord::MarkerCreate {
                name: "m".into(),
                state: cid(),
            },
            OpRecord::MarkerDelete {
                name: "m".into(),
                state: cid(),
            },
            OpRecord::Checkpoint {
                parent: None,
                state: cid(),
                thread: None,
            },
            OpRecord::TransactionAbort {
                transaction_id: "tx".into(),
                reason: "r".into(),
            },
            OpRecord::EphemeralThreadCollapse {
                thread: "t".into(),
                final_state: cid(),
            },
            OpRecord::ConflictResolved {
                conflict_id: "c".into(),
                resolution: "r".into(),
            },
            OpRecord::TransactionCommit {
                transaction_id: "tx".into(),
                op_count: 1,
            },
            OpRecord::Redact {
                redaction_id: hash(),
                blob: hash(),
                state: cid(),
                path: "p".into(),
            },
            OpRecord::Purge {
                redaction_id: hash(),
                blob: hash(),
            },
            OpRecord::FastForward {
                source_thread: "s".into(),
                target_thread: "t".into(),
                pre_target_id: cid(),
                post_target_id: cid(),
            },
            OpRecord::ThreadCreate {
                name: "t".into(),
                state: cid(),
                manager_snapshot: None,
            },
            OpRecord::GitCheckpoint {
                branch: "main".into(),
                state: cid(),
                previous_git_oid: None,
                new_git_oid: "oid".into(),
            },
            OpRecord::RemoteThreadUpdate {
                remote: "origin".into(),
                thread: "t".into(),
                state: cid(),
            },
            OpRecord::RemoteThreadDelete {
                remote: "origin".into(),
                thread: "t".into(),
                state: cid(),
            },
            OpRecord::UndoRecoveryUpdate { state: cid() },
            OpRecord::StateVisibilitySet {
                state: cid(),
                record_id: hash(),
                tier: VisibilityTier::Internal,
                prior_sidecar: None,
                new_sidecar: Some(vec![1, 2, 3]),
            },
            OpRecord::StateVisibilityPromote {
                state: cid(),
                superseded: hash(),
                record_id: hash(),
                tier: VisibilityTier::Public,
                prior_sidecar: Some(vec![4, 5]),
                new_sidecar: None,
            },
        ]
    }

    #[test]
    fn every_variant_has_a_catalog_verb() {
        for op in one_per_variant() {
            let verb = op.verb();
            assert!(!verb.is_empty(), "empty verb for {op:?}");
            assert!(
                OP_VERB_CATALOG.iter().any(|(v, _)| *v == verb),
                "verb {verb:?} for {op:?} missing from OP_VERB_CATALOG"
            );
        }
    }

    #[test]
    fn only_checkpoint_is_checkpoint_verb() {
        for op in one_per_variant() {
            let expected = matches!(op, OpRecord::Checkpoint { .. });
            assert_eq!(
                op.is_checkpoint_verb(),
                expected,
                "checkpoint flag for {op:?}"
            );
        }
    }

    #[test]
    fn verbs_excludes_checkpoint_by_default_and_dedups() {
        let with = OpRecord::verbs(true);
        let without = OpRecord::verbs(false);
        assert!(with.contains(&"checkpoint"));
        assert!(!without.contains(&"checkpoint"));
        // git_checkpoint is NOT the agent checkpoint — it must survive the
        // no-checkpoints default.
        assert!(without.contains(&"git_checkpoint"));
        // Verbs shared by multiple variants appear once.
        assert_eq!(with.iter().filter(|v| **v == "thread_create").count(), 1);
        assert_eq!(with.iter().filter(|v| **v == "fast_forward").count(), 1);
        // Newer tail variants are surfaced by default (the drift the catalog closes).
        for v in [
            "transaction_commit",
            "redact",
            "purge",
            "fast_forward",
            "remote_thread_update",
            "remote_thread_delete",
            "undo_recovery_update",
        ] {
            assert!(without.contains(&v), "{v} missing from default verb set");
        }
    }
}
