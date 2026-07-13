// SPDX-License-Identifier: Apache-2.0
//! Terminal renderer for the generic [`Progress`](objects::Progress) substrate.
//!
//! This is the *only* place that turns a [`ProgressSnapshot`] into bytes on a
//! terminal. Domain crates drive a `Progress` handle; the CLI installs a
//! [`TerminalSink`] via the JSON-guarded factory ([`progress_for`]) so progress
//! is written to stderr and never leaks into result-only stdout (#550).
//!
//! # Throttling lives here
//!
//! `Progress::inc` calls `render` on every active tick; the sink decides
//! whether to actually repaint. We coalesce to at most one redraw per
//! [`COMMIT_TICK_INTERVAL`] completed units, but always repaint on a phase
//! change so stage transitions show immediately. This mirrors the cadence of
//! the bespoke import progress line this sink replaces.

use std::{
    io::{self, IsTerminal, Write},
    sync::Mutex,
};

use objects::{Progress, ProgressSnapshot, Sink};
use repo::Repository;

use crate::cli::{Cli, should_output_json, style};

/// Redraw the live line at most once per this many completed units, so a large
/// operation doesn't spend its time flushing the terminal. Matches the historic
/// import cadence.
pub(crate) const COMMIT_TICK_INTERVAL: usize = 64;

pub(crate) fn format_transfer_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else if bytes < GIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    }
}

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

    /// Format the line to paint: the phase label, plus a live `(done/total,
    /// pct%)` suffix when the snapshot carries a real count.
    ///
    /// A consumer that pre-formats its own counts into the phase string (the
    /// import flow) never advances `done`, so `done == 0` and no suffix is
    /// appended — its lines paint verbatim. A count-driven seam (tree
    /// materialization) leaves the phase a bare label and increments `done`, so
    /// the suffix supplies the live count. This is the single rule that lets one
    /// renderer serve both styles without double-counting.
    fn format_line(snap: &ProgressSnapshot) -> String {
        if snap.done > 0 && snap.total > 0 {
            let pct = snap.done.saturating_mul(100) / snap.total;
            format!("{} ({}/{}, {}%)", snap.phase, snap.done, snap.total, pct)
        } else {
            snap.phase.clone()
        }
    }

    fn paint(line: &str) {
        if io::stderr().is_terminal() {
            // `\r` to column 0, `\x1b[K` clears to end of line so a shorter line
            // doesn't leave stale trailing characters.
            eprint!("\r{}\x1b[K", style::dim(line));
            io::stderr().flush().ok();
        } else {
            eprintln!("{}", style::dim(line));
        }
    }
}

impl Sink for TerminalSink {
    fn render(&self, snap: ProgressSnapshot) {
        // The phase label is the human line; the counters drive throttling and
        // (for count-driven seams) the live `(done/total)` suffix. An empty
        // phase is a no-op.
        if snap.phase.is_empty() {
            return;
        }
        let mut state = self.lock();
        if !Self::should_repaint(&state, &snap) {
            return;
        }
        Self::paint(&Self::format_line(&snap));
        state.last_phase = Some(snap.phase);
        state.painted = true;
    }
}

/// Clear an active TTY progress line before rendering command output.
#[cfg(feature = "client")]
pub(crate) fn clear_line(progress: &Progress) {
    if !progress.is_active() {
        return;
    }
    if io::stderr().is_terminal() {
        eprint!("\r\x1b[K");
        io::stderr().flush().ok();
    }
}

/// Paint a terminal "done" line for a finished [`Progress`], clearing the live
/// line first on a TTY. No-op for a null (inactive) handle. Used by consumers
/// that want an explicit completion marker (e.g. import's `[done]` line).
pub(crate) fn finish_line(progress: &Progress, message: &str) {
    if !progress.is_active() {
        return;
    }
    if io::stderr().is_terminal() {
        eprintln!("\r\x1b[K{}", style::accent(message));
        io::stderr().flush().ok();
    } else {
        eprintln!("{}", style::accent(message));
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
    fn count_driven_snapshot_gets_a_live_suffix() {
        // A materialize-style seam leaves the phase a bare label and advances
        // `done`; the renderer supplies the live `(done/total, pct%)` suffix.
        let snap = ProgressSnapshot {
            done: 32,
            total: 128,
            phase: "checking out files".into(),
        };
        assert_eq!(
            TerminalSink::format_line(&snap),
            "checking out files (32/128, 25%)"
        );
    }

    #[test]
    fn preformatted_line_without_a_count_paints_verbatim() {
        // The import flow pre-formats its own counts into the phase and never
        // advances `done` (stays 0), so no suffix is appended and the line is
        // painted exactly as given — no double-counting.
        let snap = ProgressSnapshot {
            done: 0,
            total: 3,
            phase: "[2/3] importing commits... 64/128 inspected (50%)".into(),
        };
        assert_eq!(
            TerminalSink::format_line(&snap),
            "[2/3] importing commits... 64/128 inspected (50%)"
        );
        // A count with an unknown total (total == 0) also gets no suffix.
        let counting = ProgressSnapshot {
            done: 500,
            total: 0,
            phase: "scanning".into(),
        };
        assert_eq!(TerminalSink::format_line(&counting), "scanning");
    }

    #[test]
    fn repaints_on_throttle_boundary_and_edges() {
        let state = RenderState {
            last_phase: Some("p".into()),
            painted: true,
        };
        for (done, total, want) in [
            (0, 100, true),
            (64, 100, true),
            (100, 100, true),
            (63, 100, false),
        ] {
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
