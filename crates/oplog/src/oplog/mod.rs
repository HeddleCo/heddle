// SPDX-License-Identifier: Apache-2.0
//! Operation log for undo/redo functionality.

mod oplog_backend;
mod oplog_core;
mod oplog_records;
mod oplog_types;
mod packed_oplog;

#[cfg(feature = "postgres")]
mod pg_oplog;

#[cfg(test)]
mod oplog_tests;

pub use oplog_backend::OpLogBackend;
pub use oplog_core::OpLog;
pub use oplog_types::{OpBatch, OpEntry, OpRecord};
#[cfg(feature = "postgres")]
pub use pg_oplog::PgOpLogBackend;
