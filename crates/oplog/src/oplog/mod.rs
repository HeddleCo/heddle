// SPDX-License-Identifier: Apache-2.0
//! Operation log for undo/redo functionality.

mod oplog_backend;
mod oplog_core;
mod oplog_recorder;
mod oplog_types;
mod packed_oplog;

#[cfg(feature = "postgres")]
mod pg_oplog;

#[cfg(test)]
mod oplog_tests;

pub use oplog_backend::OpLogBackend;
pub use oplog_core::OpLog;
pub use oplog_recorder::{OpLogRecorder, VisibilitySidecarSnapshots};
pub use oplog_types::{
    ConditionalCommitOutcome, IsolationKey, IsolationPrecondition, OpBatch, OpEntry, OpRecord,
    RedactionUndoClass, ThreadUpdateSnapshots, is_transaction_commit, is_transaction_commit_for,
    isolation_keys_for_record,
};
pub use packed_oplog::OplogRecoveryReport;
#[cfg(feature = "postgres")]
pub use pg_oplog::PgOpLogBackend;

#[cfg(test)]
pub(crate) fn fresh_state_id() -> objects::object::StateId {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&NEXT.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    objects::object::StateId::from_bytes(bytes)
}
