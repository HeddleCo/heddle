// SPDX-License-Identifier: Apache-2.0

use std::{
    num::NonZeroUsize,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use super::*;

fn automatic_policy() -> RepackPolicy {
    RepackPolicy {
        loose_object_threshold: Some(1),
        pack_count_threshold: None,
        pack_bytes_threshold: None,
        fragmentation_threshold_bps: None,
    }
}

fn unthrottled_limits(concurrency: usize) -> RepackResourceLimits {
    RepackResourceLimits::new(NonZeroUsize::new(concurrency).unwrap()).with_io_rate(None)
}

struct BlockingOperation {
    key: String,
    started: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    runs: Arc<AtomicUsize>,
}

impl RepackOperation for BlockingOperation {
    fn key(&self) -> String {
        self.key.clone()
    }

    fn inspect(&self) -> Result<RepackInventory, RepackError> {
        Ok(RepackInventory {
            loose_objects: 1,
            ..RepackInventory::default()
        })
    }

    fn run(&self, context: &RepackContext) -> Result<RepackOutcome, RepackError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.started.send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
        context.checkpoint(0)?;
        Ok(RepackOutcome {
            objects_repacked: 7,
            bytes_repacked: 70,
            bytes_reclaimed: 30,
        })
    }
}

#[test]
fn scheduler_caps_concurrency_and_reports_metrics() {
    let scheduler = RepackScheduler::new(automatic_policy(), unthrottled_limits(1));
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let runs = Arc::new(AtomicUsize::new(0));
    let operation = Arc::new(BlockingOperation {
        key: "one".to_string(),
        started: started_tx,
        release: Mutex::new(release_rx),
        runs: Arc::clone(&runs),
    });

    let RepackSchedule::Started(handle) = scheduler.repack_now(operation.clone()).unwrap() else {
        panic!("first operation should start");
    };
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let (_other_started, other_rx) = mpsc::channel();
    let (_other_release, other_release_rx) = mpsc::channel();
    let other = Arc::new(BlockingOperation {
        key: "two".to_string(),
        started: _other_started,
        release: Mutex::new(other_release_rx),
        runs: Arc::clone(&runs),
    });
    assert!(matches!(
        scheduler.repack_now(other).unwrap(),
        RepackSchedule::Busy
    ));

    release_tx.send(()).unwrap();
    let report = handle.wait().unwrap();
    assert_eq!(report.objects_repacked, 7);
    assert_eq!(report.bytes_repacked, 70);
    assert_eq!(report.bytes_reclaimed, 30);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    drop(other_rx);
}

struct BusyLoad(AtomicBool);

impl LoadMonitor for BusyLoad {
    fn should_yield(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

struct CheckpointOperation;

impl RepackOperation for CheckpointOperation {
    fn key(&self) -> String {
        "checkpoint".to_string()
    }

    fn inspect(&self) -> Result<RepackInventory, RepackError> {
        Ok(RepackInventory::default())
    }

    fn run(&self, context: &RepackContext) -> Result<RepackOutcome, RepackError> {
        context.checkpoint(0)?;
        Ok(RepackOutcome::default())
    }
}

#[test]
fn load_yield_remains_cancellable() {
    let load = Arc::new(BusyLoad(AtomicBool::new(true)));
    let scheduler = RepackScheduler::new(automatic_policy(), unthrottled_limits(1))
        .with_load_monitor(load)
        .with_backoff(Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let RepackSchedule::Started(handle) =
        scheduler.repack_now(Arc::new(CheckpointOperation)).unwrap()
    else {
        panic!("manual operation should start");
    };
    handle.cancel();
    assert_eq!(handle.wait().unwrap_err(), RepackError::Cancelled);
}

struct FailingOperation;

impl RepackOperation for FailingOperation {
    fn key(&self) -> String {
        "failing".to_string()
    }

    fn inspect(&self) -> Result<RepackInventory, RepackError> {
        Ok(RepackInventory {
            loose_objects: 1,
            ..RepackInventory::default()
        })
    }

    fn run(&self, _context: &RepackContext) -> Result<RepackOutcome, RepackError> {
        Err(RepackError::message("injected failure"))
    }
}

#[test]
fn automatic_failure_backs_off_but_manual_request_bypasses_it() {
    let scheduler = RepackScheduler::new(automatic_policy(), unthrottled_limits(1)).with_backoff(
        Duration::from_secs(60),
        Duration::from_secs(60),
        Duration::from_secs(60),
    );
    let operation = Arc::new(FailingOperation);
    let RepackSchedule::Started(handle) = scheduler.schedule_if_needed(operation.clone()).unwrap()
    else {
        panic!("threshold should start work");
    };
    assert!(handle.wait().is_err());
    assert!(matches!(
        scheduler.schedule_if_needed(operation.clone()).unwrap(),
        RepackSchedule::BackingOff { .. }
    ));
    let RepackSchedule::Started(handle) = scheduler.repack_now(operation).unwrap() else {
        panic!("manual request should bypass backoff");
    };
    assert!(handle.wait().is_err());
}

#[test]
fn policy_covers_loose_pack_size_and_fragmentation_signals() {
    let policy = RepackPolicy {
        loose_object_threshold: Some(10),
        pack_count_threshold: Some(4),
        pack_bytes_threshold: Some(1_000),
        fragmentation_threshold_bps: Some(2_000),
    };
    assert!(matches!(
        policy.evaluate(RepackInventory {
            loose_objects: 10,
            ..RepackInventory::default()
        }),
        Some(RepackReason::LooseObjects { .. })
    ));
    assert!(matches!(
        policy.evaluate(RepackInventory {
            pack_count: 4,
            ..RepackInventory::default()
        }),
        Some(RepackReason::PackCount { .. })
    ));
    assert!(matches!(
        policy.evaluate(RepackInventory {
            pack_count: 2,
            pack_bytes: 1_000,
            ..RepackInventory::default()
        }),
        Some(RepackReason::PackBytes { .. })
    ));
    assert!(matches!(
        policy.evaluate(RepackInventory {
            duplicate_objects: 2,
            packed_objects: 10,
            ..RepackInventory::default()
        }),
        Some(RepackReason::Fragmentation { .. })
    ));
    assert_eq!(
        policy.evaluate(RepackInventory {
            pack_count: 1,
            pack_bytes: 10_000,
            ..RepackInventory::default()
        }),
        None,
        "one large consolidated pack must not trigger size-only thrash"
    );
}
