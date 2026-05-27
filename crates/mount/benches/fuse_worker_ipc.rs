// SPDX-License-Identifier: Apache-2.0
//! Worker-supervisor control-plane RTT bench (heddle#190).
//!
//! Spawns `heddle-fuse-worker` under a real FUSE mount, then measures
//! the round-trip cost of [`mount::worker::Supervisor::status`] —
//! the parent's `Status` command → worker's `StatusOk` reply path
//! over the inherited Unix socketpair.
//!
//! The spike's budget (heddle#88 §7) is:
//!
//! * **Control-plane RTT < 1 ms** — anything in the
//!   [`mount::worker::SupervisorCommand`] family
//!   (`Stop` / `Status` / future `Capture` / `Invalidate`). This bench
//!   gate fails the run if the worst observation in the sample set
//!   exceeds 1 ms, matching the spike's "p99 ≤ 500ms" discipline for
//!   the respawn budget (worst-case-driven, not median).
//! * **No per-kernel-callback IPC.** Kernel ↔ worker traffic goes
//!   straight through `/dev/fuse`; the worker IS the handler. No
//!   IPC budget applies to per-syscall paths — that's enforced by
//!   the architecture, not by this bench.
//!
//! ## Running
//!
//! ```bash
//! cargo bench -p heddle-mount --features fuse --bench fuse_worker_ipc
//! ```
//!
//! Skips on hosts without `/dev/fuse`.

#![cfg(all(target_os = "linux", feature = "fuse"))]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use mount::worker::Supervisor;
use tempfile::TempDir;

fn worker_binary_path() -> PathBuf {
    // Build-script gymnastics avoided: `cargo bench` exposes the
    // worker as a sibling artifact at `target/<profile>/heddle-fuse-worker`.
    // We resolve by walking up from the test binary (which `cargo
    // bench` puts under `target/<profile>/deps/`). This pattern is
    // the same one the integration tests use; if the worker was
    // never built, `env::current_exe` still works but the resolved
    // path won't exist and `Supervisor::spawn` will error with a
    // clear message.
    let exe = std::env::current_exe().expect("locate bench binary");
    // `<target>/<profile>/deps/<bench>` → walk up two.
    let target_profile = exe
        .parent()
        .and_then(Path::parent)
        .expect("bench binary has grandparent");
    target_profile.join("heddle-fuse-worker")
}

fn main() {
    if !Path::new("/dev/fuse").exists() {
        eprintln!("skipping fuse_worker_ipc bench: /dev/fuse not present");
        return;
    }

    let worker_bin = worker_binary_path();
    if !worker_bin.exists() {
        // Try the alternative `cargo bench` layout (without
        // `deps/`).
        let exe = std::env::current_exe().expect("locate bench binary");
        let alt = exe.parent().map(|p| p.join("heddle-fuse-worker"));
        let Some(alt) = alt.filter(|p| p.exists()) else {
            eprintln!(
                "skipping fuse_worker_ipc bench: heddle-fuse-worker not built (tried {})",
                worker_bin.display()
            );
            return;
        };
        run_bench(&alt);
        return;
    }
    run_bench(&worker_bin);
}

fn run_bench(worker_bin: &Path) {
    // Build a tiny repo + snapshot.
    let repo_dir = TempDir::new().expect("repo tempdir");
    let crate_root = repo_dir.path();
    fs::write(crate_root.join("hello.txt"), b"world").expect("write hello.txt");

    // We need a heddle repo with at least one captured state for
    // the mount to succeed. Use the heddle binary if it's on PATH;
    // otherwise call into the library directly via repo::Repository.
    let _ = Command::new("heddle"); // hint only
    let repo = repo::Repository::init_default(crate_root).expect("init heddle repo");
    repo.snapshot(Some("bench-fixture".into()), None)
        .expect("snapshot fixture");

    let mountpoint = TempDir::new().expect("mountpoint tempdir");

    // Bring the supervisor up.
    let sup =
        Supervisor::spawn(worker_bin, crate_root, "main", mountpoint.path()).expect("spawn worker");

    // Warm-up — first roundtrip pays page-fault tax we don't want
    // in the measured sample.
    for _ in 0..16 {
        sup.status().expect("warm-up status");
    }

    // Measured sample. 256 roundtrips gives stable percentiles
    // without bloating the bench runtime.
    const SAMPLES: usize = 256;
    let mut durs: Vec<Duration> = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let t0 = Instant::now();
        sup.status().expect("status");
        durs.push(t0.elapsed());
    }
    durs.sort();

    let p50 = durs[SAMPLES / 2];
    let p99 = durs[(SAMPLES * 99) / 100];
    let max = *durs.last().unwrap();
    println!("fuse_worker_ipc: p50={:?} p99={:?} max={:?}", p50, p99, max);

    // Budget gate — spike §7. Worst-case-driven, not median.
    // 1ms is the explicit ceiling.
    let budget = Duration::from_millis(1);
    assert!(
        max <= budget,
        "control-plane RTT max {:?} exceeded budget {:?} (p50={:?} p99={:?})",
        max,
        budget,
        p50,
        p99,
    );

    sup.unmount().expect("unmount");
}
