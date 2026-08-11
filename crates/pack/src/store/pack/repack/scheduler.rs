// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, atomic::AtomicU64, mpsc},
    thread,
    time::{Duration, Instant},
};

use super::{
    CancellationToken, LoadMonitor, RepackContext, RepackError, RepackHandle, RepackOperation,
    RepackPolicy, RepackReason, RepackReport, RepackResourceLimits, RepackSchedule, types::NoLoad,
};

#[derive(Default)]
struct SchedulerState {
    running: HashSet<String>,
    next_attempt: HashMap<String, Instant>,
    failure_count: HashMap<String, u32>,
}

/// Bounded background scheduler shared by native and hosted repack payloads.
#[derive(Clone)]
pub struct RepackScheduler {
    policy: RepackPolicy,
    limits: RepackResourceLimits,
    load: Arc<dyn LoadMonitor>,
    state: Arc<Mutex<SchedulerState>>,
    success_backoff: Duration,
    failure_backoff: Duration,
    maximum_failure_backoff: Duration,
}

impl RepackScheduler {
    /// Construct a scheduler with configurable triggers and resource limits.
    pub fn new(policy: RepackPolicy, limits: RepackResourceLimits) -> Self {
        Self {
            policy,
            limits,
            load: Arc::new(NoLoad),
            state: Arc::new(Mutex::new(SchedulerState::default())),
            success_backoff: Duration::from_secs(30 * 60),
            failure_backoff: Duration::from_secs(30),
            maximum_failure_backoff: Duration::from_secs(30 * 60),
        }
    }

    /// Install a foreground-load signal used by worker checkpoints.
    pub fn with_load_monitor(mut self, load: Arc<dyn LoadMonitor>) -> Self {
        self.load = load;
        self
    }

    /// Override success and exponential failure backoff windows.
    pub fn with_backoff(
        mut self,
        success: Duration,
        initial_failure: Duration,
        maximum_failure: Duration,
    ) -> Self {
        self.success_backoff = success;
        self.failure_backoff = initial_failure;
        self.maximum_failure_backoff = maximum_failure.max(initial_failure);
        self
    }

    /// Inspect thresholds and start a background operation when needed.
    pub fn schedule_if_needed(
        &self,
        operation: Arc<dyn RepackOperation>,
    ) -> Result<RepackSchedule, RepackError> {
        let inventory = operation.inspect()?;
        let Some(reason) = self.policy.evaluate(inventory) else {
            return Ok(RepackSchedule::NotNeeded(inventory));
        };
        self.start(operation, reason, false, CancellationToken::default())
    }

    /// Start an operator-requested repack, bypassing heuristics and backoff.
    pub fn repack_now(
        &self,
        operation: Arc<dyn RepackOperation>,
    ) -> Result<RepackSchedule, RepackError> {
        self.start(
            operation,
            RepackReason::Manual,
            true,
            CancellationToken::default(),
        )
    }

    /// Start a manual repack controlled by an existing cancellation token.
    pub fn repack_now_with_token(
        &self,
        operation: Arc<dyn RepackOperation>,
        cancellation: CancellationToken,
    ) -> Result<RepackSchedule, RepackError> {
        self.start(operation, RepackReason::Manual, true, cancellation)
    }

    fn start(
        &self,
        operation: Arc<dyn RepackOperation>,
        reason: RepackReason,
        bypass_backoff: bool,
        cancellation: CancellationToken,
    ) -> Result<RepackSchedule, RepackError> {
        let key = operation.key();
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.running.contains(&key)
                || state.running.len() >= self.limits.max_concurrent_operations.get()
            {
                return Ok(RepackSchedule::Busy);
            }
            if !bypass_backoff
                && let Some(next) = state.next_attempt.get(&key)
                && *next > Instant::now()
            {
                return Ok(RepackSchedule::BackingOff {
                    remaining: next.saturating_duration_since(Instant::now()),
                });
            }
            state.running.insert(key.clone());
        }

        let state = Arc::clone(&self.state);
        let load = Arc::clone(&self.load);
        let limits = self.limits;
        let success_backoff = self.success_backoff;
        let failure_backoff = self.failure_backoff;
        let maximum_failure_backoff = self.maximum_failure_backoff;
        let worker_cancellation = cancellation.clone();
        let (sender, receiver) = mpsc::channel();
        let worker_state = Arc::clone(&state);
        let worker_key = key.clone();
        let spawn = thread::Builder::new()
            .name(format!("heddle-repack-{key}"))
            .spawn(move || {
                let started = Instant::now();
                let context = RepackContext {
                    cancellation: worker_cancellation,
                    load,
                    limits,
                    started,
                    accounted_io: AtomicU64::new(0),
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    operation.run(&context)
                }))
                .map_err(|_| RepackError::WorkerPanicked)
                .and_then(|result| result)
                .map(|outcome| RepackReport {
                    reason,
                    objects_repacked: outcome.objects_repacked,
                    bytes_repacked: outcome.bytes_repacked,
                    duration: started.elapsed(),
                    bytes_reclaimed: outcome.bytes_reclaimed,
                });
                finish_attempt(
                    &worker_state,
                    &worker_key,
                    &result,
                    success_backoff,
                    failure_backoff,
                    maximum_failure_backoff,
                );
                if let Ok(report) = result {
                    tracing::info!(
                        operation = %worker_key,
                        objects_repacked = report.objects_repacked,
                        bytes_repacked = report.bytes_repacked,
                        duration_ms = report.duration.as_millis(),
                        bytes_reclaimed = report.bytes_reclaimed,
                        "background repack completed"
                    );
                }
                let _ = sender.send(result);
            });
        let worker = match spawn {
            Ok(worker) => worker,
            Err(error) => {
                state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .running
                    .remove(&key);
                return Err(RepackError::operation(error));
            }
        };

        Ok(RepackSchedule::Started(RepackHandle {
            cancellation,
            result: receiver,
            worker,
        }))
    }
}

fn finish_attempt(
    state: &Mutex<SchedulerState>,
    key: &str,
    result: &Result<RepackReport, RepackError>,
    success_backoff: Duration,
    failure_backoff: Duration,
    maximum_failure_backoff: Duration,
) {
    let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
    state.running.remove(key);
    let delay = if result.is_ok() {
        state.failure_count.remove(key);
        success_backoff
    } else {
        let failures = state.failure_count.entry(key.to_string()).or_default();
        *failures = failures.saturating_add(1);
        failure_backoff
            .checked_mul(2u32.saturating_pow(failures.saturating_sub(1)))
            .unwrap_or(maximum_failure_backoff)
            .min(maximum_failure_backoff)
    };
    state
        .next_attempt
        .insert(key.to_string(), Instant::now() + delay);
}
