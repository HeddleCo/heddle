// SPDX-License-Identifier: Apache-2.0
//! Embeddable Heddle facade scaffolding.

pub mod context;
pub mod status;

pub use context::{ExecutionContext, ExecutionContextBuilder, Verbosity};
pub use objects::{
    CollectingWarnings, HeddleError, NoopProgress, NoopWarnings, ProgressEvent, ProgressSink,
    TaskId, Warning, WarningSink,
};
pub use status::StatusOptions;
