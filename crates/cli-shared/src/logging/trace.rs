// SPDX-License-Identifier: Apache-2.0
//! Privacy-bounded command and phase tracing for the foreground CLI.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

use tracing::{Span, field};

pub(super) const TELEMETRY_TARGET: &str = "heddle_telemetry";

static TRACE_EXPORT_ENABLED: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "observability")]
pub(super) fn set_trace_export_enabled(enabled: bool) {
    TRACE_EXPORT_ENABLED.store(enabled, Ordering::Release);
}

/// Whether this process has an endpoint-backed trace provider.
pub fn trace_export_enabled() -> bool {
    TRACE_EXPORT_ENABLED.load(Ordering::Acquire)
}

/// One root span for a foreground CLI command.
pub struct CommandTrace {
    span: Span,
    started: Instant,
    finished: bool,
}

impl CommandTrace {
    /// Start a command span only when trace export was explicitly enabled.
    pub fn start(command: &str, started: Instant) -> Option<Self> {
        trace_export_enabled().then(|| Self {
            span: tracing::info_span!(
                target: TELEMETRY_TARGET,
                "heddle.command",
                command.name = command,
                command.status = field::Empty,
                command.exit_code = field::Empty,
                command.duration_ms = field::Empty,
            ),
            started,
            finished: false,
        })
    }

    pub fn span(&self) -> &Span {
        &self.span
    }

    pub fn finish(&mut self, exit_code: i32) {
        self.span.record(
            "command.status",
            if exit_code == 0 { "ok" } else { "error" },
        );
        self.span.record("command.exit_code", exit_code);
        self.span.record(
            "command.duration_ms",
            u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        self.finished = true;
    }
}

impl Drop for CommandTrace {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(1);
        }
    }
}

/// Record an already-measured profile phase as a child of the command span.
pub fn record_phase_span(name: &'static str, duration_ms: u64) {
    if !trace_export_enabled() {
        return;
    }
    let span = tracing::info_span!(
        target: TELEMETRY_TARGET,
        "heddle.phase",
        phase.name = name,
        phase.duration_ms = duration_ms,
    );
    let _entered = span.enter();
}
