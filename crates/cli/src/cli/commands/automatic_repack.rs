// SPDX-License-Identifier: Apache-2.0
//! Post-capture automatic storage maintenance.

use std::{
    ffi::OsStr,
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::Result;
use objects::store::{
    FsRepackOperation, FsStore, RepackPolicy, RepackResourceLimits, RepackSchedule,
    RepackScheduler, SnapshotPackFold,
};
use repo::{Repository, RepositoryCapability};

const AUTOMATIC_REPACK_WORKER_ARG: &str = "--internal-automatic-repack";
const AUTOMATIC_REPACK_SUCCESS_FILE: &str = ".automatic-repack-success";
const AUTOMATIC_REPACK_SUCCESS_BACKOFF: Duration = Duration::from_secs(30 * 60);
const CAPTURE_QUIET_PERIOD: Duration = Duration::from_millis(500);
const MAX_CAPTURE_QUIET_WAIT: Duration = Duration::from_secs(30);
static CAPTURED_REPOSITORIES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

pub(crate) fn note_committed_capture(repo: &Repository) {
    if repo.capability() != RepositoryCapability::NativeHeddle {
        return;
    }
    let mut roots = CAPTURED_REPOSITORIES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !roots.iter().any(|root| root == repo.heddle_dir()) {
        roots.push(repo.heddle_dir().to_path_buf());
    }
}

pub fn automatic_repack_worker_root() -> Option<PathBuf> {
    let mut args = std::env::args_os();
    let _program = args.next()?;
    if args.next()? != OsStr::new(AUTOMATIC_REPACK_WORKER_ARG) {
        return None;
    }
    let root = PathBuf::from(args.next()?);
    args.next().is_none().then_some(root)
}

pub fn spawn_pending_automatic_repack_workers() {
    let roots = {
        let mut pending = CAPTURED_REPOSITORIES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *pending)
    };
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            tracing::warn!(%error, "could not locate automatic repack worker executable");
            return;
        }
    };
    for root in roots {
        if let Err(error) = Command::new(&executable)
            .arg(AUTOMATIC_REPACK_WORKER_ARG)
            .arg(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            tracing::warn!(repo = %root.display(), %error, "could not start automatic repack worker");
        }
    }
}

pub fn run_automatic_repack_worker(root: PathBuf) -> Result<()> {
    let store = FsStore::new(root);
    let Some(_automatic_lock) = store.try_lock_automatic_repack()? else {
        return Ok(());
    };
    wait_for_capture_quiet_period(store.root().join("packs"));
    match store.fold_snapshot_packs_if_needed()? {
        SnapshotPackFold::Busy => return Ok(()),
        SnapshotPackFold::Folded {
            source_packs,
            objects,
            pack_count,
        } => tracing::info!(
            source_packs,
            objects,
            pack_count,
            "folded incremental snapshot packs"
        ),
        SnapshotPackFold::NotNeeded { .. } => {}
    }

    let success_marker = store
        .root()
        .join("packs")
        .join(AUTOMATIC_REPACK_SUCCESS_FILE);
    if success_marker
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|elapsed| elapsed < AUTOMATIC_REPACK_SUCCESS_BACKOFF)
    {
        return Ok(());
    }

    let scheduler = RepackScheduler::new(RepackPolicy::default(), RepackResourceLimits::default());
    let operation = Arc::new(FsRepackOperation::new(store));
    if let RepackSchedule::Started(handle) = scheduler.schedule_if_needed(operation)? {
        handle.wait()?;
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(success_marker)?
            .sync_all()?;
    }
    Ok(())
}

fn wait_for_capture_quiet_period(packs: PathBuf) {
    let started = Instant::now();
    let mut stable_since = Instant::now();
    let mut observed = pack_dir_modified(&packs);
    while stable_since.elapsed() < CAPTURE_QUIET_PERIOD
        && started.elapsed() < MAX_CAPTURE_QUIET_WAIT
    {
        thread::sleep(Duration::from_millis(50));
        let current = pack_dir_modified(&packs);
        if current != observed {
            observed = current;
            stable_since = Instant::now();
        }
    }
}

fn pack_dir_modified(packs: &Path) -> Option<SystemTime> {
    packs
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
}
