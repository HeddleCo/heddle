// SPDX-License-Identifier: Apache-2.0
//! Heddle core domain modules extracted from the monolith.

pub mod blame;
pub mod fault_inject;
pub mod legacy;
pub mod observe;
pub mod progress;
pub mod store;
pub mod sync;
pub mod util;
pub mod worktree;

pub use error::{HeddleError, RecoveryDetails};
pub use heddle_fs_prims::{fs_atomic, fs_clone, fs_ops, lock};
pub use heddle_object_model::{error, object};
pub use observe::{
    CollectingWarnings, NoopProgress, NoopWarnings, ProgressEvent, ProgressSink, TaskId, Warning,
    WarningSink,
};
pub use progress::{Progress, ProgressSnapshot, Sink};
pub use sync::{LockExt, RwLockExt};

#[cfg(test)]
mod object_model_store_tests;
