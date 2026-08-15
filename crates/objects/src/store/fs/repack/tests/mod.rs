// SPDX-License-Identifier: Apache-2.0

mod concurrency;
mod frame_reader;
mod hot_tier;
mod integrity;

use std::{
    num::NonZeroUsize,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
};

use tempfile::TempDir;

use super::super::FsStore;
use crate::store::{
    RepackLoadMonitor, RepackPolicy, RepackResourceLimits, RepackSchedule, RepackScheduler,
};

pub(super) fn create_store() -> (TempDir, FsStore) {
    let temp = TempDir::new().unwrap();
    let store = FsStore::new(temp.path().join(".heddle"));
    store.init().unwrap();
    (temp, store)
}

fn scheduler(load: Option<Arc<dyn RepackLoadMonitor>>) -> RepackScheduler {
    let limits = RepackResourceLimits::new(NonZeroUsize::MIN).with_io_rate(None);
    let scheduler = RepackScheduler::new(RepackPolicy::default(), limits);
    load.map_or(scheduler.clone(), |load| scheduler.with_load_monitor(load))
}

fn direct_pack_names(root: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(root.join("packs"))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("pack" | "idx")
            )
            .then(|| path.file_name().unwrap().to_string_lossy().into_owned())
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn started_handle(schedule: RepackSchedule) -> crate::store::RepackHandle {
    let RepackSchedule::Started(handle) = schedule else {
        panic!("repack should start");
    };
    handle
}

struct GateLoad {
    threshold: usize,
    calls: AtomicUsize,
    blocked: AtomicBool,
    notified: AtomicBool,
    paused: Mutex<Option<mpsc::Sender<()>>>,
}

impl GateLoad {
    fn new(threshold: usize) -> (Arc<Self>, mpsc::Receiver<()>) {
        let (sender, receiver) = mpsc::channel();
        (
            Arc::new(Self {
                threshold,
                calls: AtomicUsize::new(0),
                blocked: AtomicBool::new(true),
                notified: AtomicBool::new(false),
                paused: Mutex::new(Some(sender)),
            }),
            receiver,
        )
    }

    fn release(&self) {
        self.blocked.store(false, Ordering::Release);
    }
}

impl RepackLoadMonitor for GateLoad {
    fn should_yield(&self) -> bool {
        let call = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
        if call < self.threshold || !self.blocked.load(Ordering::Acquire) {
            return false;
        }
        if !self.notified.swap(true, Ordering::AcqRel)
            && let Some(sender) = self.paused.lock().unwrap().take()
        {
            let _ = sender.send(());
        }
        true
    }
}
