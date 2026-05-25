// SPDX-License-Identifier: Apache-2.0
//! Crash-isolation properties for the out-of-process FUSE worker.
//!
//! Each test spawns the real `heddle-fuse-worker` binary via
//! `mount::worker::Supervisor::spawn`, exercises a failure mode,
//! and asserts the parent process observes it cleanly. These are
//! the two **red-commits** the heddle#190 DoD requires (see the
//! issue body's "Red-commit" rows in the acceptance criteria).
//!
//! The second test (`sigkill_worker_auto_unmounts`) lands in a
//! follow-up red commit so each crash-isolation property is its
//! own discrete signal in the PR's history.
//!
//! ## Why `#[ignore]`
//!
//! Same calculus as `fuse_mount.rs`: FUSE on the runner is
//! required. Opt-in via `--ignored`.

#![cfg(all(target_os = "linux", feature = "fuse"))]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Once,
    thread,
    time::{Duration, Instant},
};

use mount::worker::{Supervisor, PANIC_ON_INIT_ENV, STOP_GRACE_ENV};
use repo::Repository;
use tempfile::TempDir;

/// Path to the `heddle-fuse-worker` artifact `cargo` built for the
/// integration test. `CARGO_BIN_EXE_heddle-fuse-worker` is set by
/// the cargo test harness whenever a `[[bin]]` lives in the same
/// crate as the integration test.
fn worker_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heddle-fuse-worker"))
}

fn build_fixture() -> (TempDir, Repository) {
    let repo_dir = TempDir::new().expect("tempdir for repo");
    let repo = Repository::init_default(repo_dir.path()).expect("init_default");
    fs::write(repo_dir.path().join("hello.txt"), b"world").expect("write hello.txt");
    repo.snapshot(Some("worker-crash-fixture".into()), None)
        .expect("snapshot fixture");
    (repo_dir, repo)
}

static SHRINK_STOP_GRACE: Once = Once::new();

/// The integration tests want fast failure paths. Shrink the
/// supervisor's grace window down to 200ms so a hung-worker SIGTERM
/// fallback doesn't drag the test out.
fn shrink_stop_grace() {
    SHRINK_STOP_GRACE.call_once(|| {
        // SAFETY: env::set_var is safe in single-threaded test
        // setup; the `Once` makes sure we only run once across the
        // process. Touched before any worker spawn so the spawn
        // sees the shrunken value.
        unsafe {
            std::env::set_var(STOP_GRACE_ENV, "200");
        }
    });
}

fn skip_if_no_fuse() -> bool {
    if !Path::new("/dev/fuse").exists() {
        eprintln!("skipping: /dev/fuse not present on this host");
        return true;
    }
    false
}

/// Wait up to `dur` for `target` to (dis)appear.
fn wait_for_path(target: &Path, expect_present: bool, dur: Duration) {
    let deadline = Instant::now() + dur;
    while target.exists() != expect_present && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
}

/// **Red-commit 1 — panic isolation.**
///
/// Asks the worker to panic before `MountReady` fires. The
/// supervisor's [`Supervisor::spawn`] must surface this as an
/// `Err` (instead of, say, hanging waiting for a handshake that
/// never arrives or — worse — panicking the test process). After
/// the failed spawn the test process must still be healthy enough
/// to allocate, fork, and run a fresh supervisor.
///
/// On the **pre-impl tree** the worker binary does NOT honor
/// [`PANIC_ON_INIT_ENV`], so the env var is silently ignored, the
/// worker mounts normally, the spawn returns `Ok`, and the test
/// fails on the `panic!("expected to fail")` branch. The impl
/// commit (single SHA below the second red) wires the env var into
/// `run_worker` — once it does, the worker panics before
/// `MountReady`, the supervisor observes the IPC EOF, surfaces the
/// `Err`, and the test passes.
#[test]
#[ignore = "requires FUSE + heddle-fuse-worker binary on host; opt-in via --ignored"]
fn panic_kills_only_worker_not_parent() {
    if skip_if_no_fuse() {
        return;
    }
    shrink_stop_grace();

    let (repo_dir, _repo) = build_fixture();
    let mountpoint = TempDir::new().expect("mountpoint tempdir");

    let bin = worker_binary();

    // Ask the worker to panic via env var. We set + unset around
    // the spawn so any subsequent (clean) spawn isn't poisoned.
    // SAFETY: same single-threaded reasoning as `shrink_stop_grace`.
    unsafe {
        std::env::set_var(PANIC_ON_INIT_ENV, "1");
    }
    let spawn_result =
        Supervisor::spawn(&bin, repo_dir.path(), "main", mountpoint.path());
    unsafe {
        std::env::remove_var(PANIC_ON_INIT_ENV);
    }

    // The panic happens before MountReady; the supervisor must
    // surface it as `Err` (it observes EOF on the IPC socket when
    // the worker's process unwinds + exits).
    let err = match spawn_result {
        Err(e) => e,
        Ok(sup) => {
            // Defensive: if the spawn somehow succeeded, drop it
            // and panic — the test is supposed to assert the
            // crash propagates as an error.
            let _ = sup.unmount();
            panic!(
                "expected Supervisor::spawn to fail when {PANIC_ON_INIT_ENV} is set, but it succeeded"
            );
        }
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("worker") || msg.contains("handshake") || msg.contains("MountReady"),
        "expected worker-crash error message, got: {msg}"
    );

    // Parent must be healthy enough to spin up a NEW worker.
    // (Pre-impl tree: if the panic ran in-process this assertion
    // is unreachable because the test process aborted.)
    let mountpoint2 = TempDir::new().expect("mountpoint2 tempdir");
    let sup = Supervisor::spawn(&bin, repo_dir.path(), "main", mountpoint2.path())
        .expect("post-crash supervisor spawn (parent must still be healthy)");
    // Sanity: the new mount serves the fixture file.
    let target = mountpoint2.path().join("hello.txt");
    wait_for_path(&target, true, Duration::from_secs(5));
    let read = fs::read_to_string(&target).expect("read post-crash mount");
    assert_eq!(read, "world");

    sup.unmount().expect("clean unmount");
}

/// **Red-commit 2 — SIGKILL → kernel auto-unmount.**
///
/// Spawn a healthy worker, SIGKILL it, then assert:
///   (a) the supervisor's `is_alive()` flips to false within a
///       bounded window (the watcher thread observed the child
///       exit),
///   (b) the kernel auto-unmounts: the fixture file is no longer
///       visible under the mountpoint after the SIGKILL.
///
/// On the **pre-impl tree** the watcher does not flip
/// `Liveness::Exited` (it reaps the child but doesn't store), so
/// `wait_for_exit` times out and assertion (a) fails. The green
/// commit wires the `liveness.store(Exited, ...)` in `watch_child`
/// and the test passes.
#[test]
#[ignore = "requires FUSE + heddle-fuse-worker binary on host; opt-in via --ignored"]
fn sigkill_worker_auto_unmounts() {
    if skip_if_no_fuse() {
        return;
    }
    shrink_stop_grace();

    let (repo_dir, _repo) = build_fixture();
    let mountpoint = TempDir::new().expect("mountpoint tempdir");

    let bin = worker_binary();
    let sup = Supervisor::spawn(&bin, repo_dir.path(), "main", mountpoint.path())
        .expect("spawn worker");

    // Pre-condition: mountpoint serves the fixture file.
    let target = mountpoint.path().join("hello.txt");
    wait_for_path(&target, true, Duration::from_secs(5));
    assert!(target.exists(), "fixture file must be visible before SIGKILL");

    // SIGKILL the worker.
    let pid = sup.pid();
    let kill_status = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status()
        .expect("invoke kill -KILL");
    assert!(kill_status.success(), "kill -KILL exited non-zero");

    // (a) supervisor's watcher must flip is_alive() to false
    //     within the grace window.
    let killed = sup.wait_for_exit(Duration::from_secs(5));
    assert!(
        killed,
        "supervisor never observed worker exit after SIGKILL (is_alive still true)"
    );
    assert!(!sup.is_alive(), "is_alive must be false after watcher reaps");

    // (b) kernel must auto-unmount. The fixture file should no
    //     longer be visible (either the path resolves to the empty
    //     backing dir, or the dentry has gone stale).
    wait_for_path(&target, false, Duration::from_secs(5));
    assert!(
        !target.exists(),
        "kernel did not auto-unmount after SIGKILL: {} still visible",
        target.display(),
    );

    // The supervisor's unmount() call should be idempotent and
    // succeed even though the worker is already gone.
    sup.unmount().expect("idempotent unmount after SIGKILL");
}
