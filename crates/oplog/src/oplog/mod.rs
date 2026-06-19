// SPDX-License-Identifier: Apache-2.0
//! Operation log for undo/redo functionality.

mod op_record_codec;
mod oplog_backend;
mod oplog_core;
mod oplog_recorder;
mod oplog_types;
mod packed_oplog;

#[cfg(test)]
mod oplog_tests;

pub use oplog_backend::{BlockingOpLogBackend, OpLogBackend};
pub use oplog_core::OpLog;
pub use oplog_recorder::{BlockingOpLogRecorder, OpLogRecorder, VisibilitySidecarSnapshots};
pub use oplog_types::{
    ConditionalCommitOutcome, IsolationKey, IsolationPrecondition, OpBatch, OpEntry, OpRecord,
    RedactionUndoClass, ThreadUpdateSnapshots, is_transaction_commit, is_transaction_commit_for,
    isolation_keys_for_record,
};
pub use packed_oplog::OplogRecoveryReport;
