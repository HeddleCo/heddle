// SPDX-License-Identifier: Apache-2.0
//! Sync→async runtime bridge for Heddle.
//!
//! [`RuntimeBridge`] is a worker thread that owns a private current-thread
//! Tokio runtime. Synchronous code hands futures to the worker over an
//! mpsc channel and blocks on a reply channel; the worker drives the
//! future inside its own runtime. The caller's runtime (if any) is never
//! re-entered.
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
//! ## Shutdown
//!
//! Dropping the bridge drops the `Sender`; the worker's `Receiver::recv`
//! then returns `Err`, the loop breaks, and the runtime is dropped on
//! the worker thread. The `JoinHandle` is retained on the bridge so the
//! thread isn't reaped before its in-flight requests drain.

use std::{future::Future, io, pin::Pin, sync::mpsc, thread};

/// Worker thread + private current-thread Tokio runtime used to drive
/// async work off the caller's runtime.
///
/// See the crate-level docs for the design rationale. Construct with
/// [`RuntimeBridge::new`]; submit work with [`RuntimeBridge::block_on`].
pub struct RuntimeBridge {
    tx: mpsc::Sender<BridgedTask>,
    _worker: thread::JoinHandle<()>,
}

type BridgedTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

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
        let (tx, rx) = mpsc::channel::<BridgedTask>();
        let worker = thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                // current_thread is sufficient: every request is serialized
                // through the mpsc channel anyway, and an idle bridge costs
                // exactly one thread + one runtime instead of one per
                // worker-pool slot.
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build heddle-runtime-bridge worker runtime");
                runtime.block_on(async move {
                    // Blocking `rx.recv()` parks the runtime between
                    // requests. Acceptable: nothing else lives on this
                    // runtime, so there are no other tasks to starve.
                    while let Ok(task) = rx.recv() {
                        task.await;
                    }
                });
            })?;
        Ok(Self {
            tx,
            _worker: worker,
        })
    }

    /// Run `future` on the worker's runtime and block the caller until it
    /// completes, returning the future's output.
    ///
    /// `future` must be `Send + 'static`; compose it from owned data
    /// (`Arc` clones, owned `String` keys, serialized bodies), not borrows
    /// of `&self`.
    ///
    /// ## Panics
    ///
    /// Panics if the worker thread has terminated unexpectedly (either by
    /// panic inside a previous task or by the runtime build failing). In
    /// well-formed usage the worker outlives every `block_on` call because
    /// the `RuntimeBridge` owns its `JoinHandle`.
    pub fn block_on<F, T>(&self, future: F) -> T
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
        self.tx
            .send(task)
            .expect("heddle-runtime-bridge worker thread terminated unexpectedly");
        reply_rx
            .recv()
            .expect("heddle-runtime-bridge worker dropped reply channel without sending")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn block_on_from_non_tokio_thread() {
        let bridge = RuntimeBridge::new().expect("spawn bridge");
        let value = bridge.block_on(async { 1 + 2 });
        assert_eq!(value, 3);
    }

    /// The whole point of this crate: a current-thread Tokio runtime can
    /// drive sync work through the bridge without the
    /// `tokio::task::block_in_place` panic that
    /// `Handle::current().block_on(...)` would trigger.
    #[tokio::test(flavor = "current_thread")]
    async fn block_on_from_current_thread_runtime() {
        let bridge = RuntimeBridge::new().expect("spawn bridge");
        let value = bridge.block_on(async { "ok".to_string() });
        assert_eq!(value, "ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_on_from_multi_thread_runtime() {
        let bridge = RuntimeBridge::new().expect("spawn bridge");
        let value = bridge.block_on(async { 42_u64 });
        assert_eq!(value, 42);
    }

    /// Multiple sequential calls share one worker thread.
    #[test]
    fn block_on_sequential_calls() {
        let bridge = Arc::new(RuntimeBridge::new().expect("spawn bridge"));
        for i in 0..5 {
            let got: u32 = bridge.block_on(async move { i * 2 });
            assert_eq!(got, i * 2);
        }
    }

    /// Dropping the bridge shuts down the worker cleanly: the worker's
    /// `recv` returns `Err`, the loop exits, the runtime drops. We can't
    /// observe the drop directly here, but if it deadlocked the test would
    /// hang.
    #[test]
    fn drop_shuts_down_worker() {
        let bridge = RuntimeBridge::new().expect("spawn bridge");
        let _ = bridge.block_on(async { 1 });
        drop(bridge);
    }
}
