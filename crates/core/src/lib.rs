// SPDX-License-Identifier: Apache-2.0
//! Embeddable Heddle facade scaffolding.

pub mod context;
pub mod fsck;
pub mod query;

pub use context::{ExecutionContext, ExecutionContextBuilder, Verbosity};
pub use fsck::{FsckError, FsckOptions, FsckReport, fsck};
pub use objects::{
    CollectingWarnings, HeddleError, NoopProgress, NoopWarnings, ProgressEvent, ProgressSink,
    TaskId, Warning, WarningSink,
};
pub use query::{QueryHit, QueryReport, QueryRequest, query};
