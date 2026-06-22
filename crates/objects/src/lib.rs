// SPDX-License-Identifier: Apache-2.0
//! Heddle core domain modules extracted from the monolith.

pub mod delta;
pub mod error;
pub mod fault_inject;
pub mod fs_atomic;
pub mod fs_clone;
pub mod fs_ops;
pub mod lock;
pub mod object;
pub mod observe;
pub mod store;
pub mod sync;
pub mod util;
pub mod worktree;

pub use error::HeddleError;
pub use observe::{
    CollectingWarnings, NoopProgress, NoopWarnings, ProgressEvent, ProgressSink, TaskId, Warning,
    WarningSink,
};
pub use sync::{LockExt, RwLockExt};
