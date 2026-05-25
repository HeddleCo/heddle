// SPDX-License-Identifier: Apache-2.0
//! Resource-cleanup properties for the out-of-process FUSE worker.
//!
//! Codex PR #225 P2 review findings — the spawn path leaked the
//! child IPC fd when `Command::spawn` itself failed, and would
//! leave a started worker unreaped if the watcher-thread spawn
//! failed. These tests exercise both failure paths.
//!
//! Unlike `fuse_worker_crash.rs`, these tests do NOT need a live
//! `/dev/fuse` — they assert that the supervisor cleans up after
//! itself when the spawn fails *before* the kernel ever sees the
//! mount. They run by default in `cargo test -p heddle-mount`.

#![cfg(all(target_os = "linux", feature = "fuse"))]

use std::{fs, path::PathBuf};

use mount::worker::Supervisor;
use tempfile::TempDir;

/// Count the number of file descriptors open in `/proc/self/fd`.
/// Linux-only; the test file is already cfg-gated to linux.
fn count_open_fds() -> usize {
    fs::read_dir("/proc/self/fd")
        .expect("read /proc/self/fd")
        .count()
}

/// **Codex P2 — fd leak on spawn failure.**
///
/// `Supervisor::spawn` converts the child's IPC socket to a raw fd
/// before invoking `Command::spawn`. If `spawn()` returns an error
/// (e.g. the worker binary is missing or non-executable), the
/// pre-fix code returned early without closing that raw fd. The
/// kernel never inherited the dup (no child was forked), so the
/// parent's reference is the only thing keeping the fd alive — and
/// it's now orphaned.
///
/// Repro: spawn against a guaranteed-nonexistent binary in a loop;
/// the fd count climbs monotonically on the pre-fix code. On the
/// post-fix code, the fd count is stable.
///
/// We allow a small slack constant to absorb transient fds from
/// the libc/tempfile/`/proc` enumeration itself.
#[test]
fn child_ipc_fd_closed_on_spawn_failure() {
    let repo_dir = TempDir::new().expect("tempdir for repo");
    let mountpoint = TempDir::new().expect("mountpoint tempdir");
    // A path that cannot exist. `spawn()` will fail with ENOENT.
    let bogus_binary = PathBuf::from("/nonexistent/heddle-fuse-worker-xyzzy");

    // Warm-up: the first iteration may allocate libc / tracing
    // statics that look like an fd leak. Run once, then take the
    // baseline.
    let _ = Supervisor::spawn(
        &bogus_binary,
        repo_dir.path(),
        "main",
        mountpoint.path(),
    );

    let baseline = count_open_fds();

    const ITERS: usize = 32;
    for _ in 0..ITERS {
        let result = Supervisor::spawn(
            &bogus_binary,
            repo_dir.path(),
            "main",
            mountpoint.path(),
        );
        assert!(
            result.is_err(),
            "spawn against {bogus_binary:?} must fail (binary does not exist)"
        );
    }

    let after = count_open_fds();
    let growth = after.saturating_sub(baseline);
    assert!(
        growth <= 2,
        "fd count grew by {growth} over {ITERS} failed spawns \
         (baseline={baseline}, after={after}); each failed spawn \
         is leaking the child IPC fd"
    );
}
