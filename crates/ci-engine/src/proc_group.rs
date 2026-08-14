// SPDX-License-Identifier: Apache-2.0
//! Shared registry for hard teardown of live check process groups.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

#[derive(Default, Debug)]
struct Inner {
    live: BTreeSet<i32>,
    torn_down: bool,
}

/// Concurrency-safe set of process-group ids.
#[derive(Clone, Default, Debug)]
pub struct ProcGroupRegistry {
    inner: Arc<Mutex<Inner>>,
}

impl ProcGroupRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a group unless teardown already happened.
    #[must_use]
    pub fn register_active(&self, process_group: i32) -> bool {
        let mut inner = self.inner.lock().expect("process-group registry poisoned");
        if inner.torn_down {
            false
        } else {
            inner.live.insert(process_group);
            true
        }
    }

    /// Remove a reaped group.
    pub fn unregister(&self, process_group: i32) {
        self.inner
            .lock()
            .expect("process-group registry poisoned")
            .live
            .remove(&process_group);
    }

    /// Number of active groups.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("process-group registry poisoned")
            .live
            .len()
    }

    /// Whether no group is active.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Kill one Unix process group best-effort.
    #[cfg(unix)]
    pub fn kill_group(process_group: i32) {
        // SAFETY: negative pid targets the group created by `process_group(0)`.
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    }

    /// No process groups are created by this engine on non-Unix platforms.
    #[cfg(not(unix))]
    pub fn kill_group(_process_group: i32) {}

    /// Kill all active groups and latch teardown.
    #[cfg(unix)]
    pub fn kill_all(&self) -> usize {
        let mut inner = self.inner.lock().expect("process-group registry poisoned");
        let count = inner.live.len();
        for process_group in &inner.live {
            // SAFETY: each id is registered only after spawning a group leader.
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
        }
        inner.live.clear();
        inner.torn_down = true;
        count
    }

    /// Latch teardown on non-Unix platforms.
    #[cfg(not(unix))]
    pub fn kill_all(&self) -> usize {
        let mut inner = self.inner.lock().expect("process-group registry poisoned");
        inner.live.clear();
        inner.torn_down = true;
        0
    }
}
