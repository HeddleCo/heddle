// SPDX-License-Identifier: Apache-2.0
//! Sync→async runtime bridge for Heddle.
//!
//! [`RuntimeBridge`] is a worker thread that owns a private current-thread
//! Tokio runtime. Synchronous code hands futures to the worker over an
//! async mpsc channel; the worker `tokio::spawn`s each future on its own
//! runtime and the caller blocks on a per-request reply channel until the
//! task completes. The caller's runtime (if any) is never re-entered.
//!
//! ## Why
//!
//! Heddle has several `ObjectStore` / `OpLogBackend` / `RefBackend`
//! implementations whose trait surface is synchronous but whose underlying
//! I/O is async (`aws-sdk-s3`, `sqlx`, etc.). A naive bridge —
//! `Handle::current().block_on(...)` or
//! `tokio::task::block_in_place(|| Handle::current().block_on(...))` —
//! breaks in caller-flavor-dependent ways:
//!
//! * `Handle::current().block_on(...)` panics with "Cannot start a runtime
//!   from within a runtime" when the caller is already on a Tokio runtime.
//! * `block_in_place(...)` panics with "can call blocking only when running
//!   on the multi-threaded runtime" when the caller is on a current-thread
//!   runtime (e.g. `#[tokio::test(flavor = "current_thread")]`).
//! * Neither works at all when the caller is on a non-Tokio thread.
//!
//! Routing through this bridge sidesteps all three: the future runs on the
//! bridge's private runtime regardless of who calls it, and the caller's
//! thread simply blocks on a reply channel.
//!
//! ## Concurrency
//!
//! The worker dispatches each request via [`tokio::spawn`] rather than
//! awaiting it inline, so concurrent callers sharing one bridge can
//! progress in parallel on the worker's runtime. This preserves the
//! connection-level parallelism of pools like `sqlx::PgPool` instead of
//! head-of-line blocking every caller behind the slowest in-flight query.
//!
//! ## Error recovery
//!
//! [`RuntimeBridge::block_on`] returns [`Result<T, BridgeError>`] so a dead
//! worker surfaces as a recoverable error in the caller's `Result`-typed
//! API rather than escalating into a process-level panic. A bridged task
//! that panics aborts only that task: its reply channel is dropped and the
//! waiting caller observes [`BridgeError::ResponseLost`]; the worker keeps
//! serving other requests.
//!
//! ## Shutdown
//!
//! Dropping the bridge drops the `Sender`; the worker's `Receiver::recv`
//! then returns `None`, the loop breaks, and the runtime is dropped on
//! the worker thread. The `JoinHandle` is retained on the bridge so the
//! thread isn't reaped before its in-flight requests drain.

use std::{fmt, future::Future, io, pin::Pin, sync::mpsc, thread};

use tokio::sync::mpsc as tokio_mpsc;

/// Worker thread + private current-thread Tokio runtime used to drive
/// async work off the caller's runtime.
///
/// See the crate-level docs for the design rationale. Construct with
/// [`RuntimeBridge::new`]; submit work with [`RuntimeBridge::block_on`].
pub struct RuntimeBridge {
    tx: tokio_mpsc::UnboundedSender<BridgedTask>,
    _worker: thread::JoinHandle<()>,
}

type BridgedTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Errors returned by [`RuntimeBridge::block_on`] when the bridge cannot
/// complete a request. Both variants signal an unrecoverable problem with
/// the worker for this one call; the bridge as a whole remains usable for
/// subsequent calls only when [`BridgeError::ResponseLost`] is returned
/// (the worker is still alive, just this one task died).
#[derive(Debug)]
pub enum BridgeError {
    /// The worker thread is gone — sending the task failed because the
    /// receiver side of the dispatch channel was dropped. In practice this
    /// means the worker thread panicked while building or driving its
    /// runtime; every later call will return the same error.
    WorkerDead,
    /// The task was accepted but no reply ever arrived. The future panicked
    /// mid-poll (so its reply sender was dropped without sending) or the
    /// worker's runtime was shut down underneath an in-flight task. Other
    /// in-flight requests on the same bridge are unaffected.
    ResponseLost,
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkerDead => {
                f.write_str("runtime bridge: worker thread terminated; dispatch channel closed")
            }
            Self::ResponseLost => f.write_str(
                "runtime bridge: task accepted but no reply received (task panicked \
                 or runtime shut down mid-flight)",
            ),
        }
    }
}

impl std::error::Error for BridgeError {}

impl RuntimeBridge {
    /// Spawn a worker thread with a private current-thread Tokio runtime.
    ///
    /// Returns the worker's `io::Error` if the OS refuses the thread spawn.
    /// The thread name is `heddle-runtime-bridge` so it's identifiable in
    /// stack traces and process listings; pick a more specific wrapper at
    /// the call site if you need per-consumer naming.
    pub fn new() -> io::Result<Self> {
        Self::with_thread_name("heddle-runtime-bridge")
    }

    /// Same as [`RuntimeBridge::new`] but uses a custom worker thread name.
    pub fn with_thread_name(thread_name: impl Into<String>) -> io::Result<Self> {
        // tokio mpsc lets the worker `await` on `recv` so the executor
        // stays scheduling spawned tasks between dispatches. A std mpsc
        // would block the runtime thread on `recv`, defeating the spawn
        // concurrency fix.
        let (tx, mut rx) = tokio_mpsc::unbounded_channel::<BridgedTask>();
        let worker = thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build heddle-runtime-bridge worker runtime");
                runtime.block_on(async move {
                    // `tokio::spawn` runs each task concurrently on the
                    // current-thread runtime; the loop returns to `recv`
                    // immediately so subsequent callers don't queue behind
                    // the slowest in-flight task. Task `JoinHandle`s are
                    // dropped (tasks are detached); each task signals its
                    // caller through its private reply channel.
                    while let Some(task) = rx.recv().await {
                        tokio::spawn(task);
                    }
                });
            })?;
        Ok(Self {
            tx,
            _worker: worker,
        })
    }

    /// Run `future` on the worker's runtime and block the caller until it
    /// completes, returning the future's output or a [`BridgeError`] when
    /// the worker cannot deliver a reply.
    ///
    /// `future` must be `Send + 'static`; compose it from owned data
    /// (`Arc` clones, owned `String` keys, serialized bodies), not borrows
    /// of `&self`.
    ///
    /// ## Errors
    ///
    /// - [`BridgeError::WorkerDead`] when the worker thread has terminated
    ///   (its receive channel was dropped). Subsequent calls will keep
    ///   returning this error; the bridge cannot recover.
    /// - [`BridgeError::ResponseLost`] when the worker accepted the task
    ///   but no reply arrived — the future panicked, or the worker's
    ///   runtime was dropped while the task was in flight. The bridge
    ///   itself remains usable; other in-flight callers are unaffected.
    pub fn block_on<F, T>(&self, future: F) -> Result<T, BridgeError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        // Capacity 1: each call sends exactly one value and then drops.
        let (reply_tx, reply_rx) = mpsc::sync_channel::<T>(1);
        let task: BridgedTask = Box::pin(async move {
            let value = future.await;
            // The receiver is held on the caller's stack until `recv`
            // returns, so `send` is infallible in practice.
            let _ = reply_tx.send(value);
        });
        self.tx.send(task).map_err(|_| BridgeError::WorkerDead)?;
        reply_rx.recv().map_err(|_| BridgeError::ResponseLost)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use super::*;

    #[test]
    fn block_on_from_non_tokio_thread() {
        let bridge = RuntimeBridge::new().expect("spawn bridge");
        let value = bridge.block_on(async { 1 + 2 }).expect("ok");
        assert_eq!(value, 3);
    }

    /// The whole point of this crate: a current-thread Tokio runtime can
    /// drive sync work through the bridge without the
    /// `tokio::task::block_in_place` panic that
    /// `Handle::current().block_on(...)` would trigger.
    #[tokio::test(flavor = "current_thread")]
    async fn block_on_from_current_thread_runtime() {
        let bridge = RuntimeBridge::new().expect("spawn bridge");
        let value = bridge.block_on(async { "ok".to_string() }).expect("ok");
        assert_eq!(value, "ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_on_from_multi_thread_runtime() {
        let bridge = RuntimeBridge::new().expect("spawn bridge");
        let value = bridge.block_on(async { 42_u64 }).expect("ok");
        assert_eq!(value, 42);
    }

    /// Multiple sequential calls share one worker thread.
    #[test]
    fn block_on_sequential_calls() {
        let bridge = Arc::new(RuntimeBridge::new().expect("spawn bridge"));
        for i in 0..5 {
            let got: u32 = bridge.block_on(async move { i * 2 }).expect("ok");
            assert_eq!(got, i * 2);
        }
    }

    /// Dropping the bridge shuts down the worker cleanly: the worker's
    /// `recv` returns `None`, the loop exits, the runtime drops. We can't
    /// observe the drop directly here, but if it deadlocked the test would
    /// hang.
    #[test]
    fn drop_shuts_down_worker() {
        let bridge = RuntimeBridge::new().expect("spawn bridge");
        let _ = bridge.block_on(async { 1 });
        drop(bridge);
    }

    /// Codex P2 #1 regression guard: concurrent callers on one bridge must
    /// progress in parallel rather than head-of-line block on a single
    /// `task.await`.
    ///
    /// Each of N caller threads calls `block_on` with a future that sleeps
    /// for `delay`. Total wall time is measured by an external clock. If
    /// the worker serialized requests (the pre-fix behaviour), total time
    /// would be ≥ `N * delay`. With concurrent dispatch, every call sleeps
    /// in parallel, so the worst case is ~`delay` plus a small scheduling
    /// margin. The assertion bounds at `N * delay / 2`: tight enough to
    /// fail the serial implementation, loose enough to survive slow CI.
    #[test]
    fn concurrent_callers_run_in_parallel() {
        const N: usize = 4;
        const DELAY: Duration = Duration::from_millis(250);

        let bridge = Arc::new(RuntimeBridge::new().expect("spawn bridge"));
        let barrier = Arc::new(Barrier::new(N));
        let started = Instant::now();

        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let bridge = Arc::clone(&bridge);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                // Sync the dispatch instant across threads so the bridge
                // sees all N requests at roughly the same time.
                barrier.wait();
                bridge
                    .block_on(async move {
                        tokio::time::sleep(DELAY).await;
                    })
                    .expect("ok")
            }));
        }
        for h in handles {
            h.join().expect("worker thread joined");
        }
        let elapsed = started.elapsed();
        let serial_floor = DELAY * (N as u32);
        assert!(
            elapsed < serial_floor / 2,
            "concurrent dispatch regressed to serial behaviour: \
             elapsed {elapsed:?} >= serial_floor/2 ({:?}); \
             N={N}, per-call delay={DELAY:?}",
            serial_floor / 2,
        );
    }

    /// Codex P2 #2 regression guard: a bridged task that panics must
    /// surface as `BridgeError::ResponseLost` to its caller, and the
    /// bridge must remain usable for subsequent calls. The pre-fix
    /// `.expect(...)` path would have escalated into a process-level
    /// panic in the caller's thread on the *next* call.
    #[test]
    fn panicking_task_returns_response_lost_and_bridge_stays_alive() {
        let bridge = RuntimeBridge::new().expect("spawn bridge");
        let result: Result<(), _> = bridge.block_on(async {
            panic!("simulated task panic");
        });
        assert!(
            matches!(result, Err(BridgeError::ResponseLost)),
            "panicking task must return ResponseLost; got {result:?}",
        );
        // The bridge must still be usable — the worker thread is alive,
        // only the panicking task died.
        let next: u64 = bridge.block_on(async { 7 }).expect("ok");
        assert_eq!(next, 7, "bridge must keep serving after a task panic");
    }

    /// Sanity check the `tokio::spawn` path inside the worker: when the
    /// bridged future spawns its own tasks they must complete normally
    /// (i.e. the runtime is still running the executor, not parked on a
    /// blocking recv).
    #[test]
    fn worker_runtime_runs_inner_spawns() {
        let bridge = RuntimeBridge::new().expect("spawn bridge");
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_task = Arc::clone(&counter);
        bridge
            .block_on(async move {
                let handles: Vec<_> = (0..8)
                    .map(|_| {
                        let c = Arc::clone(&counter_for_task);
                        tokio::spawn(async move {
                            c.fetch_add(1, Ordering::SeqCst);
                        })
                    })
                    .collect();
                for h in handles {
                    h.await.expect("inner spawn joined");
                }
            })
            .expect("ok");
        assert_eq!(counter.load(Ordering::SeqCst), 8);
    }
}
