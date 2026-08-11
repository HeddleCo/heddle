// SPDX-License-Identifier: Apache-2.0

use std::{
    error::Error,
    fmt,
    num::{NonZeroU64, NonZeroUsize},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use super::{RepackInventory, RepackReason};

/// Cooperative cancellation shared by a caller and a repack operation.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// Request cancellation. Storage cutover may deliberately defer it.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Host-load signal consulted at cooperative checkpoints.
pub trait LoadMonitor: Send + Sync + 'static {
    /// Return `true` while foreground work should take priority.
    fn should_yield(&self) -> bool;
}

#[derive(Debug)]
pub(super) struct NoLoad;

impl LoadMonitor for NoLoad {
    fn should_yield(&self) -> bool {
        false
    }
}

/// Scheduler-wide resource limits.
#[derive(Clone, Copy, Debug)]
pub struct RepackResourceLimits {
    pub(super) max_concurrent_operations: NonZeroUsize,
    pub(super) io_bytes_per_second: Option<NonZeroU64>,
    pub(super) load_yield_interval: Duration,
}

impl Default for RepackResourceLimits {
    fn default() -> Self {
        Self {
            max_concurrent_operations: NonZeroUsize::MIN,
            io_bytes_per_second: NonZeroU64::new(64 * 1024 * 1024),
            load_yield_interval: Duration::from_millis(10),
        }
    }
}

impl RepackResourceLimits {
    /// Create limits with a bounded number of simultaneous operations.
    pub fn new(max_concurrent_operations: NonZeroUsize) -> Self {
        Self {
            max_concurrent_operations,
            ..Self::default()
        }
    }

    /// Set the aggregate per-operation I/O rate; `None` disables throttling.
    pub fn with_io_rate(mut self, bytes_per_second: Option<NonZeroU64>) -> Self {
        self.io_bytes_per_second = bytes_per_second;
        self
    }

    /// Set how often a load-blocked operation rechecks load and cancellation.
    pub fn with_load_yield_interval(mut self, interval: Duration) -> Self {
        self.load_yield_interval = interval.max(Duration::from_millis(1));
        self
    }
}

/// Error returned by scheduling or executing a repack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepackError {
    /// Cancellation was observed before the storage cutover began.
    Cancelled,
    /// The storage-specific operation failed.
    Operation(String),
    /// The worker panicked before reporting a result.
    WorkerPanicked,
}

impl RepackError {
    /// Wrap a storage-specific error without coupling the scheduler to it.
    pub fn operation(error: impl Error) -> Self {
        Self::Operation(error.to_string())
    }

    /// Wrap a storage-specific diagnostic that is not an error type.
    pub fn message(message: impl Into<String>) -> Self {
        Self::Operation(message.into())
    }
}

impl fmt::Display for RepackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("repack cancelled"),
            Self::Operation(message) => write!(formatter, "repack operation failed: {message}"),
            Self::WorkerPanicked => formatter.write_str("repack worker panicked"),
        }
    }
}

impl Error for RepackError {}

/// Storage-produced measurements, excluding scheduler-owned duration/reason.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RepackOutcome {
    /// Unique objects written to replacement storage.
    pub objects_repacked: u64,
    /// Logical object bytes read and repacked.
    pub bytes_repacked: u64,
    /// Physical bytes removed minus physical replacement bytes.
    pub bytes_reclaimed: u64,
}

/// Completed operation measurements.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepackReport {
    /// Policy or operator signal that started the work.
    pub reason: RepackReason,
    /// Unique objects written to replacement storage.
    pub objects_repacked: u64,
    /// Logical object bytes read and repacked.
    pub bytes_repacked: u64,
    /// Wall-clock scheduler duration.
    pub duration: Duration,
    /// Physical bytes reclaimed by cutover.
    pub bytes_reclaimed: u64,
}

/// Cooperative controls supplied to a storage-specific operation.
pub struct RepackContext {
    pub(super) cancellation: CancellationToken,
    pub(super) load: Arc<dyn LoadMonitor>,
    pub(super) limits: RepackResourceLimits,
    pub(super) started: Instant,
    pub(super) accounted_io: AtomicU64,
}

impl RepackContext {
    /// Check cancellation, yield to foreground load, and rate-limit I/O.
    pub fn checkpoint(&self, io_bytes: u64) -> Result<(), RepackError> {
        if self.cancellation.is_cancelled() {
            return Err(RepackError::Cancelled);
        }
        while self.load.should_yield() {
            self.sleep_cooperatively(self.limits.load_yield_interval)?;
        }
        let total = self.accounted_io.fetch_add(io_bytes, Ordering::Relaxed) + io_bytes;
        if let Some(rate) = self.limits.io_bytes_per_second {
            let target = Duration::from_secs_f64(total as f64 / rate.get() as f64);
            let elapsed = self.started.elapsed();
            if target > elapsed {
                self.sleep_cooperatively(target - elapsed)?;
            }
        }
        thread::yield_now();
        Ok(())
    }

    /// The cancellation token observed by this operation.
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation
    }

    fn sleep_cooperatively(&self, duration: Duration) -> Result<(), RepackError> {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            if self.cancellation.is_cancelled() {
                return Err(RepackError::Cancelled);
            }
            thread::sleep((deadline - Instant::now()).min(Duration::from_millis(10)));
        }
        Ok(())
    }
}

/// Storage-specific repack payload driven by the scheduler.
///
/// Implementations must stage immutable replacement data, verify all logical
/// identities, and make cutover atomic. Once cutover starts, it must finish
/// even if the context's token becomes cancelled.
pub trait RepackOperation: Send + Sync + 'static {
    /// Stable key used to prevent concurrent repacks of the same store.
    fn key(&self) -> String;
    /// Cheap facts used by automatic trigger policy.
    fn inspect(&self) -> Result<RepackInventory, RepackError>;
    /// Build, verify, and atomically publish replacement storage.
    fn run(&self, context: &RepackContext) -> Result<RepackOutcome, RepackError>;
}

/// Result of asking the scheduler to start work.
pub enum RepackSchedule {
    /// A background operation started.
    Started(RepackHandle),
    /// No automatic threshold was crossed.
    NotNeeded(RepackInventory),
    /// This store is in success/failure backoff.
    BackingOff { remaining: Duration },
    /// The per-store or global concurrency bound is occupied.
    Busy,
}

/// Handle for cancellation and result collection.
pub struct RepackHandle {
    pub(super) cancellation: CancellationToken,
    pub(super) result: mpsc::Receiver<Result<RepackReport, RepackError>>,
    pub(super) worker: thread::JoinHandle<()>,
}

impl RepackHandle {
    /// Request cancellation before cutover.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Clone the token for integration with an external shutdown signal.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Wait for the operation to finish.
    pub fn wait(self) -> Result<RepackReport, RepackError> {
        let result = self
            .result
            .recv()
            .unwrap_or(Err(RepackError::WorkerPanicked));
        if self.worker.join().is_err() {
            return Err(RepackError::WorkerPanicked);
        }
        result
    }
}
