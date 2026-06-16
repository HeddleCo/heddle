// SPDX-License-Identifier: Apache-2.0
//! Local daemon UDS cache mechanism tests.
//!
//! The caches in `crates/cli/src/client/local_daemon.rs` exist to make
//! a hot agent loop cheap: the first daemon detection / channel build
//! pays the filesystem + connect cost, every subsequent call in the
//! process is an O(1) cache hit.
//!
//! These tests assert that property by the *mechanism*, not the clock.
//! Earlier revisions compared wall-clock time (warm ≥5× faster than
//! cold), which flaked on shared CI runners — worse under `llvm-cov` —
//! and became merge-blocking once `Check & test` was made a required
//! check (see issue #722). Wall-clock ratios are nondeterministic; the
//! cache hit/miss is not. So instead we count the underlying expensive
//! operations via process-wide instrumentation
//! (`probe_run_count` / `channel_build_count`) and assert the warm path
//! skips them entirely. This proves the exact same O(1)/warm-skip
//! behavior, deterministically.
//!
//! Both tests run against a daemon started in-process via
//! `daemon::local_daemon::serve`.

#![cfg(unix)]

use std::{
    io::ErrorKind,
    path::PathBuf,
    time::{Duration, Instant},
};

use cli::client::local_daemon::{
    LocalDaemonStatus, channel_build_count, connect_local_daemon_channel, detect_local_daemon,
    probe, probe_run_count,
};
use daemon::local_daemon::{LocalDaemonConfig, serve};
use objects::error::HeddleError;
use repo::Repository;
use tempfile::TempDir;
use tokio::sync::oneshot;

/// Spin up a local daemon for the duration of one test. Returns the
/// repo's `.heddle/` dir, a shutdown handle, and the join handle of
/// the daemon task.
async fn spawn_local_daemon() -> Option<(
    TempDir,
    PathBuf,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
)> {
    let temp = TempDir::new().expect("temp repo dir");
    let repo = Repository::init_default(temp.path()).expect("init heddle repo");
    let heddle_dir = repo.heddle_dir().to_path_buf();
    let config = LocalDaemonConfig::from_repo(&repo);

    let (tx, rx) = oneshot::channel::<()>();
    let (startup_tx, mut startup_rx) = oneshot::channel::<HeddleError>();
    let task = tokio::spawn(async move {
        let shutdown = async move {
            let _ = rx.await;
        };
        // The serve future borrows `repo` by value; the task owns it.
        if let Err(err) = serve(repo, config, shutdown).await {
            eprintln!("serve_local test daemon exited: {err}");
            let _ = startup_tx.send(err);
        }
    });

    // Wait for the listener to actually bind. Polling is the standard
    // local-test pattern — checking that the file-stat probe reports
    // `Running` plus the socket file exists is sufficient.
    let socket = heddle_dir.join("sockets").join("grpc.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if socket.exists() && matches!(probe(&heddle_dir).status, LocalDaemonStatus::Running { .. })
        {
            break;
        }
        if Instant::now() > deadline {
            panic!(
                "local daemon did not come up within 5s; socket exists={}",
                socket.exists()
            );
        }
        tokio::select! {
            err = &mut startup_rx => match err {
                Ok(err) if is_permission_denied(&err) => {
                    eprintln!(
                        "skipping serve_local daemon cache test: local UDS daemon startup denied: {err}"
                    );
                    let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
                    return None;
                }
                Ok(err) => panic!("local daemon exited before startup: {err}"),
                Err(_) => panic!("local daemon exited before startup without an error"),
            },
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }

    Some((temp, heddle_dir, tx, task))
}

fn is_permission_denied(err: &HeddleError) -> bool {
    matches!(err, HeddleError::Io(io) if io.kind() == ErrorKind::PermissionDenied)
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(process_global)]
async fn detect_local_daemon_warm_path_skips_the_cold_probe() {
    let Some((_temp, heddle_dir, shutdown, task)) = spawn_local_daemon().await else {
        return;
    };

    // Prime the process-wide `DETECT_CACHE` so the first (cache-filling)
    // call doesn't count against the warm sample. After this, the entry
    // for `heddle_dir` is resident and every later `detect_local_daemon`
    // is a pure cache hit.
    let primed = detect_local_daemon(&heddle_dir).expect("daemon present");

    // Warm path: N `detect_local_daemon` calls. The mechanism under
    // test is the cache short-circuit — a warm hit returns the cached
    // `Option<UdsTarget>` WITHOUT running `probe` (the "cold setup
    // step": two file stats + `kill(pid, 0)` + executable-identity
    // check). We prove that by snapshotting the process-wide probe
    // counter around the loop and asserting it does not move.
    let warm_iters = 100u64;
    let probe_before_warm = probe_run_count(&heddle_dir);
    for _ in 0..warm_iters {
        let target = detect_local_daemon(&heddle_dir).expect("daemon present");
        // The cache must hand back the same target each time, not a
        // freshly-probed (and possibly divergent) one.
        assert_eq!(target, primed, "warm detect must return the cached target");
        std::hint::black_box(target);
    }
    let warm_probe_runs = probe_run_count(&heddle_dir) - probe_before_warm;
    assert_eq!(
        warm_probe_runs, 0,
        "warm detect_local_daemon must skip the cold probe entirely \
         (cache hit), but probe ran {warm_probe_runs} time(s) over \
         {warm_iters} warm calls"
    );

    // Cold path, for contrast: N fresh `probe()` calls. Each one is the
    // cold setup step the warm path skipped, so the counter must move
    // by exactly N. This nails down that the counter actually tracks
    // the work — a counter that never moves would make the warm
    // assertion vacuous.
    let cold_iters = 100u64;
    let probe_before_cold = probe_run_count(&heddle_dir);
    for _ in 0..cold_iters {
        let result = probe(&heddle_dir);
        assert!(matches!(result.status, LocalDaemonStatus::Running { .. }));
        std::hint::black_box(result);
    }
    let cold_probe_runs = probe_run_count(&heddle_dir) - probe_before_cold;
    assert_eq!(
        cold_probe_runs, cold_iters,
        "each cold probe() must run the setup step exactly once; \
         expected {cold_iters} runs, saw {cold_probe_runs}"
    );

    // Also exercise the channel path so this file isn't only testing
    // the cheap file-stat cache. This validates that the tonic UDS
    // channel + Health.Check actually round-trip against the live
    // `serve()` daemon (which doesn't install a health reporter —
    // Unimplemented is the expected status code, treated as "channel is
    // alive" by the client).
    let connected = connect_local_daemon_channel(&heddle_dir, Duration::from_millis(500)).await;
    assert!(
        connected.is_some(),
        "connect_local_daemon_channel must succeed against a live daemon"
    );

    let _ = shutdown.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(process_global)]
async fn channel_cache_returns_clones_without_rebuilding() {
    // The first `connect_local_daemon_channel` does the real work:
    // open the UDS, build the tonic channel, run the `Health.Check`
    // handshake (all of it inside `build_channel`). Every subsequent
    // call should hit `CHANNEL_CACHE` and hand back a *clone* of the
    // cached channel in O(1) — no second connect/handshake.
    //
    // We assert that by the mechanism, not the clock: `build_channel`
    // bumps a process-wide counter, so a cache hit leaves the counter
    // untouched. (The old revision compared wall-clock ratios, which
    // flaked under CI load — issue #722.)
    let Some((_temp, heddle_dir, shutdown, task)) = spawn_local_daemon().await else {
        return;
    };

    let builds_before = channel_build_count(&heddle_dir);

    let first = connect_local_daemon_channel(&heddle_dir, Duration::from_millis(500))
        .await
        .expect("first connect");
    let builds_after_first = channel_build_count(&heddle_dir);
    assert_eq!(
        builds_after_first - builds_before,
        1,
        "first connect_local_daemon_channel must build the channel exactly once"
    );

    let second = connect_local_daemon_channel(&heddle_dir, Duration::from_millis(500))
        .await
        .expect("cached connect");
    let builds_after_second = channel_build_count(&heddle_dir);
    assert_eq!(
        builds_after_second - builds_after_first,
        0,
        "second connect_local_daemon_channel must be an O(1) cache hit \
         that returns a clone WITHOUT rebuilding the channel; \
         build ran {} extra time(s)",
        builds_after_second - builds_after_first
    );

    // The cache hit must hand back the same target the first build
    // produced — a clone, not a divergent re-detection.
    assert_eq!(
        first.target, second.target,
        "cached channel must carry the same UdsTarget as the first build"
    );

    let _ = shutdown.send(());
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}
