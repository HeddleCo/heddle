// SPDX-License-Identifier: Apache-2.0
//! Generic, near-zero-cost progress substrate.
//!
//! A [`Progress`] handle is a cheap-to-clone (`Arc`-bump) counter that any
//! long-running operation can drive without knowing how — or whether — the
//! progress is rendered. The rendering half lives behind the [`Sink`] trait,
//! which higher layers (the CLI) implement to paint a terminal line. Domain
//! crates only ever see the handle.
//!
//! # Zero-overhead null path
//!
//! The overwhelmingly common case is "no one is watching" (piped output,
//! `--output json`, embedded library use). For that case a handle is built
//! with [`Progress::null`], whose sink slot is `None`. Every hot-path call
//! ([`Progress::inc`]) then costs exactly one relaxed atomic add plus a single
//! predicted-not-taken branch on the sink slot — no snapshot allocation, no
//! syscall, and no virtual `Sink::render` dispatch. The vtable is only touched
//! when a sink is actually installed via [`Progress::with_sink`].
//!
//! Throttling (redraw at most every N ticks) is deliberately *not* done here:
//! [`Progress::inc`] always calls `render` when active, and the [`Sink`]
//! decides whether to actually repaint. Keeping the decision in the renderer
//! keeps `inc` branch-predictable and lets each sink pick its own cadence.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

/// A point-in-time view of a [`Progress`] handle, handed to [`Sink::render`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressSnapshot {
    /// Units completed so far.
    pub done: usize,
    /// Total units of work, or `0` when the total is not yet known.
    pub total: usize,
    /// Current human phase label (e.g. `"importing commits"`).
    pub phase: String,
}

/// Renders progress snapshots. Implemented by the presentation layer (the CLI's
/// `TerminalSink`), never by domain crates.
///
/// A `Sink` is called on every active [`Progress::inc`], so implementations are
/// responsible for their own throttling — cheap sinks may repaint every call,
/// terminal sinks should coalesce redraws.
pub trait Sink: Send + Sync {
    /// Present the given snapshot. Called on the thread that drove the update;
    /// may be called concurrently from multiple threads, so implementations
    /// must be `Sync` and manage their own interior state.
    fn render(&self, snap: ProgressSnapshot);
}

struct ProgressInner {
    done: AtomicUsize,
    total: AtomicUsize,
    /// Phase changes are rare (once per operation stage), so a `Mutex` here
    /// keeps the hot [`Progress::inc`] path lock-free while still letting
    /// [`Progress::set_phase`] mutate the label.
    phase: Mutex<String>,
    /// `None` is the null path: no boxed sink, no snapshot, no vtable dispatch.
    sink: Option<Box<dyn Sink>>,
}

/// A cheap-to-clone progress handle.
///
/// Cloning bumps an `Arc` refcount; all clones share the same counters and
/// sink, so a handle can be threaded through `thread::scope` closures or
/// stored alongside other shared state. `Send + Sync`.
#[derive(Clone)]
pub struct Progress(Arc<ProgressInner>);

impl Progress {
    /// A handle that renders nothing. The hot path is a relaxed add plus a
    /// predicted-not-taken branch — see the module docs.
    pub fn null() -> Self {
        Progress(Arc::new(ProgressInner {
            done: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
            phase: Mutex::new(String::new()),
            sink: None,
        }))
    }

    /// A handle backed by a real [`Sink`]. Every active `inc` will call
    /// `sink.render`; the sink is responsible for throttling.
    pub fn with_sink(sink: Box<dyn Sink>) -> Self {
        Progress(Arc::new(ProgressInner {
            done: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
            phase: Mutex::new(String::new()),
            sink: Some(sink),
        }))
    }

    /// Whether this handle renders. `false` for [`Progress::null`].
    #[inline]
    pub fn is_active(&self) -> bool {
        self.0.sink.is_some()
    }

    /// Set the total unit count. Cheap; does not trigger a render on its own.
    pub fn set_total(&self, total: usize) {
        self.0.total.store(total, Ordering::Relaxed);
    }

    /// The current completed count.
    #[inline]
    pub fn done(&self) -> usize {
        self.0.done.load(Ordering::Relaxed)
    }

    /// The current total (`0` if unknown).
    #[inline]
    pub fn total(&self) -> usize {
        self.0.total.load(Ordering::Relaxed)
    }

    /// Set the human phase label. Rare relative to `inc`; when active it also
    /// triggers a render so the label change is painted immediately (a
    /// terminal sink typically forces a repaint on phase change).
    pub fn set_phase(&self, label: impl Into<String>) {
        let label = label.into();
        {
            let mut guard = self.lock_phase();
            *guard = label;
        }
        if self.0.sink.is_some() {
            self.render_current();
        }
    }

    /// The current phase label.
    pub fn phase(&self) -> String {
        self.lock_phase().clone()
    }

    /// Advance the completed count by `n` and, if active, render.
    ///
    /// Hot path: `done.fetch_add(n, Relaxed)` then one branch on the optional
    /// sink. When inactive nothing else happens — no snapshot, no vtable call.
    #[inline]
    pub fn inc(&self, n: usize) {
        self.0.done.fetch_add(n, Ordering::Relaxed);
        if self.0.sink.is_some() {
            self.render_current();
        }
    }

    /// Snapshot the current state and hand it to the sink. Cold relative to the
    /// `active` check in `inc`, so it stays out-of-line.
    fn render_current(&self) {
        if let Some(sink) = &self.0.sink {
            let snap = ProgressSnapshot {
                done: self.0.done.load(Ordering::Relaxed),
                total: self.0.total.load(Ordering::Relaxed),
                phase: self.lock_phase().clone(),
            };
            sink.render(snap);
        }
    }

    /// Take a current snapshot without rendering. Useful for a sink that wants
    /// to force a final repaint (e.g. a "done" line).
    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            done: self.0.done.load(Ordering::Relaxed),
            total: self.0.total.load(Ordering::Relaxed),
            phase: self.lock_phase().clone(),
        }
    }

    fn lock_phase(&self) -> std::sync::MutexGuard<'_, String> {
        self.0.phase.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl std::fmt::Debug for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Progress")
            .field("done", &self.done())
            .field("total", &self.total())
            .field("active", &self.is_active())
            .finish_non_exhaustive()
    }
}

impl Default for Progress {
    fn default() -> Self {
        Self::null()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Adapter so a test can keep an `Arc<T>` to inspect its sink while the
    /// [`Progress`] handle owns the `Box<dyn Sink>`. Renders forward to the
    /// shared `T`.
    struct Shared<T: Sink>(Arc<T>);
    impl<T: Sink> Sink for Shared<T> {
        fn render(&self, snap: ProgressSnapshot) {
            self.0.render(snap);
        }
    }
    fn progress_over<T: Sink + 'static>(sink: &Arc<T>) -> Progress {
        Progress::with_sink(Box::new(Shared(Arc::clone(sink))))
    }

    /// Sink that records every snapshot it is handed.
    #[derive(Default)]
    struct CapturingSink {
        renders: Mutex<Vec<ProgressSnapshot>>,
        calls: AtomicUsize,
    }
    impl CapturingSink {
        fn snapshots(&self) -> Vec<ProgressSnapshot> {
            self.renders.lock().unwrap().clone()
        }
        fn call_count(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }
    impl Sink for CapturingSink {
        fn render(&self, snap: ProgressSnapshot) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.renders.lock().unwrap().push(snap);
        }
    }

    #[test]
    fn null_path_renders_nothing_under_a_tight_loop() {
        // `null()` installs no sink. A tight `inc` loop must only advance the
        // counter and hit the predicted-not-taken sink branch — no render, no
        // panic. This is the smoke test for the null hot path; the real
        // guarantee is the single `self.0.sink.is_some()` gate in `inc`.
        let p = Progress::null();
        assert!(!p.is_active());
        for _ in 0..1_000_000 {
            p.inc(1);
        }
        assert_eq!(p.done(), 1_000_000);
        assert_eq!(p.total(), 0);
    }

    #[test]
    fn inactive_handle_never_dispatches_render() {
        // A null handle stays silent even through set_phase/set_total, which on
        // an active handle would render. The absence of a sink is the sole gate.
        let p = Progress::null();
        p.set_total(50);
        p.set_phase("noise");
        p.inc(10);
        assert!(!p.is_active());
        assert_eq!(p.done(), 10);
        assert_eq!(p.total(), 50);
        assert_eq!(p.phase(), "noise");
    }

    #[test]
    fn inc_and_set_total_track_counters() {
        let sink = Arc::new(CapturingSink::default());
        let p = progress_over(&sink);
        p.set_total(128);
        p.inc(1);
        p.inc(3);
        assert_eq!(p.done(), 4);
        assert_eq!(p.total(), 128);
        // Each active inc rendered exactly once (set_total does not render).
        assert_eq!(sink.call_count(), 2);
        let snaps = sink.snapshots();
        assert_eq!(snaps.last().unwrap().done, 4);
        assert_eq!(snaps.last().unwrap().total, 128);
    }

    #[test]
    fn set_phase_is_captured_and_renders() {
        let sink = Arc::new(CapturingSink::default());
        let p = progress_over(&sink);
        p.set_phase("scanning refs");
        p.inc(1);
        p.set_phase("writing refs");
        assert_eq!(p.phase(), "writing refs");
        let snaps = sink.snapshots();
        // set_phase -> render, inc -> render, set_phase -> render
        assert_eq!(snaps.len(), 3);
        assert_eq!(snaps[0].phase, "scanning refs");
        assert_eq!(snaps[1].phase, "scanning refs");
        assert_eq!(snaps[2].phase, "writing refs");
    }

    #[test]
    fn a_throttling_sink_sees_every_call_and_decides_itself() {
        // The substrate hands the sink every active tick; throttling is the
        // sink's job (COMMIT_TICK_INTERVAL lives in the renderer, not in `inc`).
        const INTERVAL: usize = 64;
        #[derive(Default)]
        struct ThrottlingSink {
            seen: AtomicUsize,
            painted: AtomicUsize,
        }
        impl Sink for ThrottlingSink {
            fn render(&self, snap: ProgressSnapshot) {
                self.seen.fetch_add(1, Ordering::Relaxed);
                if snap.done.is_multiple_of(INTERVAL) {
                    self.painted.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let sink = Arc::new(ThrottlingSink::default());
        let p = progress_over(&sink);
        for _ in 0..256 {
            p.inc(1);
        }
        // Offered every active tick...
        assert_eq!(sink.seen.load(Ordering::Relaxed), 256);
        // ...but only "painted" on the throttle boundary (64,128,192,256).
        assert_eq!(sink.painted.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn clone_shares_counters() {
        let sink = Arc::new(CapturingSink::default());
        let p = progress_over(&sink);
        let q = p.clone();
        p.inc(2);
        q.inc(3);
        assert_eq!(p.done(), 5);
        assert_eq!(q.done(), 5);
    }
}
