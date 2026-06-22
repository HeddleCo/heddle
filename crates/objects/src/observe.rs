// SPDX-License-Identifier: Apache-2.0
//! Structured progress and warning channels for library callers.

use std::{
    borrow::Cow,
    sync::{Mutex, MutexGuard},
};

/// Stable progress task identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(pub u64);

/// Structured progress event emitted by long-running operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    Start {
        id: TaskId,
        label: Cow<'static, str>,
        total: Option<u64>,
    },
    Advance {
        id: TaskId,
        delta: u64,
    },
    Message {
        id: TaskId,
        msg: Cow<'static, str>,
    },
    Finish {
        id: TaskId,
    },
}

/// Receives structured progress events.
pub trait ProgressSink: Send + Sync {
    fn event(&self, ev: ProgressEvent);
}

/// Receives structured warnings that should not be printed by domain crates.
pub trait WarningSink: Send + Sync {
    fn warn(&self, w: Warning);
}

/// Structured warning for embedders and render layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// Stable machine-readable class, e.g. "refs_unlock_failed".
    pub kind: Cow<'static, str>,
    pub message: String,
}

/// Default progress sink for callers that do not care about progress.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopProgress;

impl ProgressSink for NoopProgress {
    fn event(&self, _ev: ProgressEvent) {}
}

/// Default warning sink for callers that do not care about warnings.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopWarnings;

impl WarningSink for NoopWarnings {
    fn warn(&self, _w: Warning) {}
}

/// Warning sink useful for embedders and tests that need to inspect warnings.
#[derive(Debug, Default)]
pub struct CollectingWarnings {
    warnings: Mutex<Vec<Warning>>,
}

impl CollectingWarnings {
    pub fn warnings(&self) -> Vec<Warning> {
        self.guard().clone()
    }

    pub fn drain(&self) -> Vec<Warning> {
        self.guard().drain(..).collect()
    }

    fn guard(&self) -> MutexGuard<'_, Vec<Warning>> {
        match self.warnings.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl WarningSink for CollectingWarnings {
    fn warn(&self, w: Warning) {
        self.guard().push(w);
    }
}
