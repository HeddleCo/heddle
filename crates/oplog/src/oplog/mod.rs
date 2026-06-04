// SPDX-License-Identifier: Apache-2.0
//! Operation log for undo/redo functionality.

mod op_record_codec;
mod oplog_backend;
mod oplog_core;
mod oplog_types;
mod packed_oplog;

#[cfg(feature = "postgres")]
mod pg_oplog;

#[cfg(test)]
mod oplog_tests;

pub use oplog_backend::OpLogBackend;
pub use oplog_core::OpLog;
pub use oplog_types::{
    ConditionalCommitOutcome, IsolationKey, IsolationPrecondition, OpBatch, OpEntry, OpRecord,
    RedactionUndoClass, isolation_keys_for_record,
};
#[cfg(feature = "postgres")]
pub use pg_oplog::PgOpLogBackend;
