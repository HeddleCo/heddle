// SPDX-License-Identifier: Apache-2.0
//! Terminal renderer for the generic [`Progress`](objects::Progress) substrate.
//!
//! This is the *only* place that turns a [`ProgressSnapshot`] into bytes on a
//! terminal. Domain crates drive a `Progress` handle; the CLI installs a
//! [`TerminalSink`] via the JSON-guarded factory ([`progress_for`]) so progress
//! never leaks into machine-readable stdout (#550) and never repaints when
//! output is piped away from a TTY.
//!
//! # Throttling lives here
//!
//! `Progress::inc` calls `render` on every active tick; the sink decides
//! whether to actually repaint. We coalesce to at most one redraw per
//! [`COMMIT_TICK_INTERVAL`] completed units, but always repaint on a phase
//! change so stage transitions show immediately. This mirrors the cadence of
//! the bespoke import progress line this sink replaces.

use std::io::{self, IsTerminal, Write};
use std::sync::Mutex;

use objects::{Progress, ProgressSnapshot, Sink};

use crate::cli::{Cli, should_output_json, style};
use repo::Repository;

/// Redraw the live line at most once per this many completed units, so a large
/// operation doesn't spend its time flushing the terminal. Matches the historic
/// import cadence.
pub(crate) const COMMIT_TICK_INTERVAL: usize = 64;

/// Build a [`Progress`] handle for a command, applying the single JSON guard
/// (#550) exactly once at construction: JSON output → a null handle that
/// renders nothing; otherwise → a [`TerminalSink`]. The guard is never checked
/// again per update.
pub(crate) fn progress_for(cli: &Cli, repo: &Repository) -> Progress {
    if should_output_json(cli, Some(repo.config())) {
        Progress::null()
    } else {
        Progress::with_sink(Box::new(TerminalSink::new()))
    }
}

/// A `Sink` that paints a single, self-overwriting progress line.
///
/// - On a TTY: `\r`-carriage-return redraw of one dim line, throttled to one
///   repaint per [`COMMIT_TICK_INTERVAL`] units (plus forced repaint on phase
///   change and on the first/last unit).
/// - Off a TTY (piped human output): one dim line per throttled tick, no
///   control codes.
///
/// The sink is `Sync`; its small amount of interior state (last painted phase +
/// tick bookkeeping) lives behind a `Mutex`. Renders are cheap and infrequent
/// relative to `inc`, so the lock is not on any hot path.
pub(crate) struct TerminalSink {
    state: Mutex<RenderState>,
}

#[derive(Default)]
struct RenderState {
    /// Phase last painted; a change forces a repaint regardless of throttle.
    last_phase: Option<String>,
    /// Whether anything has been painted yet (drives the leading redraw and the
    /// `finish` clear).
    painted: bool,
}

impl TerminalSink {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(RenderState::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RenderState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Decide whether this snapshot should repaint. Always repaint on a phase
    /// change or the first paint; otherwise throttle on the completed count.
    fn should_repaint(state: &RenderState, snap: &ProgressSnapshot) -> bool {
        let phase_changed = state.last_phase.as_deref() != Some(snap.phase.as_str());
        phase_changed
            || snap.done == 0
            || (snap.total != 0 && snap.done == snap.total)
            || snap.done.is_multiple_of(COMMIT_TICK_INTERVAL)
    }

    fn paint(line: &str) {
        if io::stdout().is_terminal() {
            // `\r` to column 0, `\x1b[K` clears to end of line so a shorter line
            // doesn't leave stale trailing characters.
            print!("\r{}\x1b[K", style::dim(line));
            io::stdout().flush().ok();
        } else {
            println!("{}", style::dim(line));
        }
    }
}

impl Sink for TerminalSink {
    fn render(&self, snap: ProgressSnapshot) {
        // The phase label carries the fully-formatted line the caller wants
        // painted; the counters drive throttling. An empty phase is a no-op.
        if snap.phase.is_empty() {
            return;
        }
        let mut state = self.lock();
        if !Self::should_repaint(&state, &snap) {
            return;
        }
        Self::paint(&snap.phase);
        state.last_phase = Some(snap.phase);
        state.painted = true;
    }
}

/// Erase the live progress line so a subsequent `println!` starts clean.
///
/// On a TTY the live line is drawn with `\r` and no trailing newline, so
/// without this the next line would overwrite it from column 0. Off a TTY each
/// throttled tick already printed its own newline-terminated line, so there is
/// nothing to clear. No-op for a null (inactive) handle.
///
/// Only the hosted (network) push path drives progress, so this is gated on the
/// `client` feature to stay dead-code-clean in default-feature builds.
#[cfg(feature = "client")]
pub(crate) fn clear_progress_line(progress: &Progress) {
    if !progress.is_active() {
        return;
    }
    if io::stdout().is_terminal() {
        print!("\r\x1b[K");
        io::stdout().flush().ok();
    }
}

/// Paint a terminal "done" line for a finished [`Progress`], clearing the live
/// line first on a TTY. No-op for a null (inactive) handle. Used by consumers
/// that want an explicit completion marker (e.g. import's `[done]` line).
pub(crate) fn finish_line(progress: &Progress, message: &str) {
    if !progress.is_active() {
        return;
    }
    if io::stdout().is_terminal() {
        print!("\r\x1b[K{}\n", style::accent(message));
        io::stdout().flush().ok();
    } else {
        println!("{}", style::accent(message));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repaints_on_phase_change_regardless_of_throttle() {
        let mut state = RenderState {
            last_phase: Some("old".into()),
            painted: true,
        };
        let snap = ProgressSnapshot {
            done: 7, // not a throttle boundary
            total: 100,
            phase: "new".into(),
        };
        assert!(TerminalSink::should_repaint(&state, &snap));
        // Same phase, off-boundary count -> no repaint.
        state.last_phase = Some("new".into());
        assert!(!TerminalSink::should_repaint(&state, &snap));
    }

    #[test]
    fn repaints_on_throttle_boundary_and_edges() {
        let state = RenderState {
            last_phase: Some("p".into()),
            painted: true,
        };
        for (done, total, want) in
            [(0, 100, true), (64, 100, true), (100, 100, true), (63, 100, false)]
        {
            let snap = ProgressSnapshot {
                done,
                total,
                phase: "p".into(),
            };
            assert_eq!(
                TerminalSink::should_repaint(&state, &snap),
                want,
                "done={done} total={total}"
            );
        }
    }
}
