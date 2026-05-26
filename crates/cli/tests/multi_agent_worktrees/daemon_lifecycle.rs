// SPDX-License-Identifier: Apache-2.0
//! End-to-end tests for the long-lived `heddled` mount daemon
//! (`--workspace light --daemon`).
//!
//! These complement the in-process tests in [`virtualized_mount`]
//! (which exercise the `--workspace light` *without* `--daemon`
//! path). The daemon path hands the FUSE session off to a
//! `heddle daemon serve` subprocess that survives the spawning CLI's
//! exit; these tests verify the cross-CLI lifecycle, idempotency,
//! the `daemon stop`/`status` verbs, and the failure modes documented
//! in `docs/design/mount-daemon.md` § "Failure modes".
//!
//! Gated to Linux because the FUSE shell only compiles there. Each
//! test that needs a real kernel mount checks `/dev/fuse` first and
//! returns gracefully (with a `eprintln!` warning) on hosts without
//! it — same skip pattern the existing tests rely on. Marked
//! `#[ignore]` so `cargo test` on developer laptops doesn't try to
//! shell out to `fusermount` or bind to FUSE without opting in:
//!
//! ```sh
//! cargo test -p cli --features mount --test multi_agent_worktrees \
//!   daemon_lifecycle -- --ignored
//! ```
//!
//! Every test passes `--daemon` to `heddle thread start` explicitly.
//! The daemon-vs-in-process default is currently in flux (a parallel
//! agent is changing the flip), and these tests must continue to
//! pass regardless of the default.

#![cfg(target_os = "linux")]

use std::{
    fs,
    path::{Path, PathBuf},
    thread::sleep,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tempfile::TempDir;

use super::{heddle, setup_repo};

// ---------------------------------------------------------------------------
// Local helpers
//
// These are deliberately local to this file (not pushed into
// `multi_agent_worktrees.rs`) because they only matter for the daemon
// path; promoting them would noise the parent module for the other
// integration tests. If a third file needs the same helpers, hoist
// then.
// ---------------------------------------------------------------------------

/// Skip the test (warn + return) when the host doesn't expose
/// `/dev/fuse`. CI runners frequently lack it; a hard failure there
/// would be useless noise.
fn fuse_supported_or_skip(test_name: &str) -> bool {
    if std::path::Path::new("/dev/fuse").exists() {
        return true;
    }
    eprintln!(
        "[daemon_lifecycle::{test_name}] skipping: /dev/fuse not present \
         on this host"
    );
    false
}

/// Conventional endpoint file path the daemon writes on bind. Kept
/// inline (rather than imported from `repo::daemon`) so this test
/// file doesn't need to touch the dev-deps to add a `repo` import.
fn endpoint_path(repo_root: &Path) -> PathBuf {
    repo_root
        .join(".heddle")
        .join("state")
        .join("heddled.endpoint.json")
}

/// Conventional registry file path the daemon mirrors on every
/// mount/unmount transition.
fn registry_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".heddle").join("state").join("mounts.json")
}

/// Wait up to `deadline` for `predicate` to return `true`. Returns
/// `true` on success, `false` on timeout. Sleeps 50 ms between
/// checks (mirrors the daemon-client spawn-retry cadence in
/// `crates/cli/src/cli/commands/daemon/client.rs`).
fn wait_until(deadline: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if predicate() {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    predicate()
}

/// Read the daemon's PID out of the endpoint file. `None` if the
/// file is missing or unparseable. The endpoint file shape is
/// documented in `crates/repo/src/daemon/endpoint.rs::EndpointState`.
fn endpoint_pid(repo_root: &Path) -> Option<u32> {
    let raw = fs::read_to_string(endpoint_path(repo_root)).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    v.get("pid").and_then(Value::as_u64).map(|pid| pid as u32)
}

/// `kill -0 <pid>` — return whether the PID is still alive. Mirrors
/// `repo::daemon::endpoint::pid_alive` so we don't need to import
/// from the workspace tree.
fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 to libc::kill is the canonical Unix existence
    // probe; no memory effects.
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    // ESRCH (3) ⇒ no such process. Any other errno (EPERM, etc.)
    // means the process exists, we just can't signal it.
    errno != 3
}

/// Send `signal` to `pid`. Used by the daemon-killed-mid-mount
/// recovery test to simulate a hard crash. Best-effort: any error
/// is logged but the test continues so the assertion failure
/// surfaces the actual problem rather than an unrelated `kill` ENOENT.
fn kill_pid(pid: u32, signal: i32) {
    // SAFETY: libc::kill is async-signal-safe and has no memory
    // effects. The PID/signal combination is bounded by what the
    // test passes in.
    let result = unsafe { libc::kill(pid as i32, signal) };
    if result != 0 {
        let errno = std::io::Error::last_os_error();
        eprintln!("[daemon_lifecycle] kill({pid}, {signal}) failed: {errno}");
    }
}

/// Pull `thread.path` (the FUSE mount point) out of `start --output json`
/// output. Identical helper lives in `virtualized_mount.rs`; we
/// duplicate rather than hoist to keep the test files independently
/// readable.
fn mount_path_from_start(raw: &str) -> String {
    let out: Value = serde_json::from_str(raw).expect("start --output json output");
    out.get("thread")
        .and_then(|t| t.get("path"))
        .and_then(Value::as_str)
        .expect("virtualized thread output should include thread.path")
        .to_string()
}

/// Capture a snapshot in `cwd` and return its short change_id.
fn capture_short(cwd: &Path, msg: &str) -> String {
    let out =
        heddle(&["--output", "json", "capture", "-m", msg], Some(cwd)).expect("snapshot succeeded");
    let v: Value = serde_json::from_str(&out).expect("snapshot --output json is valid JSON");
    v.get("change_id")
        .and_then(Value::as_str)
        .expect("snapshot output exposes change_id")
        .to_string()
}

/// Best-effort `daemon stop` for use in test cleanup. Swallows
/// errors because the test may have already torn the daemon down.
fn try_stop_daemon(repo_root: &Path) {
    let _ = heddle(&["daemon", "stop"], Some(repo_root));
}

/// Parse the human-formatted `daemon status` output and return the
/// `mount_count` field. Status output shape is fixed by
/// `cmd_daemon_status` in `crates/cli/src/cli/commands/daemon/cmd.rs`:
///   `daemon: ok=true version=2 uptime_s=12 mount_count=1`
fn parse_mount_count(status_output: &str) -> Option<u32> {
    for token in status_output.split_whitespace() {
        if let Some(rest) = token.strip_prefix("mount_count=") {
            return rest.parse().ok();
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Scenario 1: cross-CLI-invocation mount survival.
//
// Start a thread with `--daemon`, exit the heddle process, then read
// from the mount path via a *fresh* heddle invocation. Verifies the
// daemon-owned mount outlives the spawning CLI — the entire reason
// `--daemon` exists.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires Linux + FUSE + heddle built with --features mount"]
fn daemon_mount_survives_cli_exit() {
    if !fuse_supported_or_skip("daemon_mount_survives_cli_exit") {
        return;
    }
    let main = setup_repo("greet.txt", "hello from daemon");

    let raw = heddle(
        &[
            "--output",
            "json",
            "start",
            "feature/daemon-survives",
            "--workspace",
            "virtualized",
            "--daemon",
        ],
        Some(main.path()),
    )
    .expect("--daemon start succeeded");
    let mount_path = mount_path_from_start(&raw);

    // The CLI process that ran `start` has already exited (the
    // `Command::output` in `heddle()` waits on it). A fresh
    // invocation of std::fs::read_to_string is exercising the
    // daemon-owned mount across processes.
    let observed = fs::read_to_string(format!("{mount_path}/greet.txt"))
        .expect("read through daemon-owned mount after CLI exit");
    assert_eq!(observed, "hello from daemon");

    // Drop the thread. The drop path RPCs the daemon's `unmount`
    // verb (see `cmd_thread_drop` in
    // `crates/cli/src/cli/commands/thread_cmd.rs`), so this also
    // verifies the unmount RPC works.
    heddle(
        &["thread", "drop", "feature/daemon-survives"],
        Some(main.path()),
    )
    .expect("thread drop after daemon mount");

    assert!(
        fs::read_to_string(format!("{mount_path}/greet.txt")).is_err(),
        "after drop, mount must be inaccessible"
    );

    try_stop_daemon(main.path());
}

// ---------------------------------------------------------------------------
// Scenario 2: daemon spawn-on-demand + status.
//
// `--daemon` starts the daemon if it isn't running; `daemon status`
// reports it healthy with the right mount count.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires Linux + FUSE + heddle built with --features mount"]
fn daemon_spawns_on_demand_and_status_reports_healthy() {
    if !fuse_supported_or_skip("daemon_spawns_on_demand_and_status_reports_healthy") {
        return;
    }
    let main = setup_repo("greet.txt", "alive");

    assert!(
        !endpoint_path(main.path()).exists(),
        "endpoint must not exist before any --daemon use"
    );

    heddle(
        &[
            "--output",
            "json",
            "start",
            "spawn-test",
            "--workspace",
            "virtualized",
            "--daemon",
        ],
        Some(main.path()),
    )
    .expect("--daemon start spawned the daemon");

    assert!(
        endpoint_path(main.path()).exists(),
        "endpoint file must be present after `--daemon` use"
    );

    let status = heddle(&["daemon", "status"], Some(main.path())).expect("daemon status RPC");
    assert!(
        status.contains("ok=true"),
        "daemon status should report ok=true; got: {status:?}"
    );
    assert_eq!(
        parse_mount_count(&status),
        Some(1),
        "exactly one mount expected after a single --daemon start; got: {status:?}"
    );

    // Cleanup: drop the thread to release the mount, then ask the
    // daemon to exit so it doesn't sit on a port for the next test.
    heddle(&["thread", "drop", "spawn-test"], Some(main.path())).expect("drop spawn-test thread");
    try_stop_daemon(main.path());
}

// ---------------------------------------------------------------------------
// Scenario 3: idempotent mount.
//
// Issuing `--daemon` twice for the same thread/mountpoint is a
// no-op; the daemon's `MountRegistry` returns the existing handle
// and `mount_count` stays at 1. Documented in
// `docs/design/mount-daemon.md` § "Failure modes" → "Race".
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires Linux + FUSE + heddle built with --features mount"]
fn idempotent_mount_does_not_double_register() {
    if !fuse_supported_or_skip("idempotent_mount_does_not_double_register") {
        return;
    }
    let main = setup_repo("greet.txt", "once");

    let first = heddle(
        &[
            "--output",
            "json",
            "start",
            "idem-thread",
            "--workspace",
            "virtualized",
            "--daemon",
        ],
        Some(main.path()),
    )
    .expect("first --daemon start");
    let first_path = mount_path_from_start(&first);

    // Second invocation. The CLI's `start_thread` may take a
    // different code path on a second call against an existing
    // thread (re-resolving the workspace), but the mount RPC must
    // be idempotent on the daemon side regardless.
    let second = heddle(
        &[
            "--output",
            "json",
            "start",
            "idem-thread",
            "--workspace",
            "virtualized",
            "--daemon",
        ],
        Some(main.path()),
    )
    .expect("second --daemon start must succeed");
    let second_path = mount_path_from_start(&second);
    assert_eq!(
        first_path, second_path,
        "second start must resolve to the same mount path"
    );

    let status = heddle(&["daemon", "status"], Some(main.path())).expect("daemon status RPC");
    assert_eq!(
        parse_mount_count(&status),
        Some(1),
        "mount count must remain 1 after idempotent re-mount; got: {status:?}"
    );

    // Read still works after the second call.
    let observed = fs::read_to_string(format!("{first_path}/greet.txt"))
        .expect("read after idempotent re-mount");
    assert_eq!(observed, "once");

    heddle(&["thread", "drop", "idem-thread"], Some(main.path())).expect("drop idem-thread");
    try_stop_daemon(main.path());
}

// ---------------------------------------------------------------------------
// Scenario 4: `heddle daemon stop`.
//
// With a live mount, `daemon stop` must (a) drain the mounts, (b)
// exit the daemon process, (c) remove the endpoint file, (d) remove
// `mounts.json`. Verified via a `kill -0` probe on the daemon PID
// and `metadata()` on the mount path.
//
// The teardown contract `cmd_daemon_stop` advertises (see its
// rustdoc) is:
//
//   1. Daemon's `MountRegistry::shutdown_all` drains FUSE sessions
//      and removes `mounts.json`.
//   2. Daemon's `run_mount_daemon` removes `endpoint.json`.
//   3. Daemon process exits.
//   4. CLI's `cmd_daemon_stop` polls (a) for endpoint.json gone,
//      then (b) for the recorded PID to die — and only then
//      returns success. The CLI-side `sweep_stale_mounts` is a
//      redundant safety net (idempotent `fs::remove_file`).
//
// Once `daemon stop` returns and the recorded PID is dead, every
// post-shutdown observation below is a hard assertion — there is
// no remaining cleanup actor that could re-create either state
// file.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires Linux + FUSE + heddle built with --features mount"]
fn daemon_stop_drains_mounts_and_exits() {
    if !fuse_supported_or_skip("daemon_stop_drains_mounts_and_exits") {
        return;
    }
    let main = setup_repo("greet.txt", "stoppable");

    let raw = heddle(
        &[
            "--output",
            "json",
            "start",
            "stop-test",
            "--workspace",
            "virtualized",
            "--daemon",
        ],
        Some(main.path()),
    )
    .expect("--daemon start");
    let mount_path = mount_path_from_start(&raw);
    let pid = endpoint_pid(main.path()).expect("endpoint file must record the daemon PID");
    assert!(pid_alive(pid), "daemon must be alive before stop");

    heddle(&["daemon", "stop"], Some(main.path())).expect("daemon stop RPC");

    // (a) endpoint file gone — `cmd_daemon_stop` waits up to 2 s
    // for the daemon to remove it; if it's still here, the daemon
    // didn't shut down cleanly.
    assert!(
        wait_until(Duration::from_secs(5), || {
            !endpoint_path(main.path()).exists()
        }),
        "endpoint file must be removed after daemon stop"
    );

    // (b) daemon PID actually gone. Allow up to 5 s for the OS to
    // reap. If the process is still alive past then, the shutdown
    // path is broken.
    assert!(
        wait_until(Duration::from_secs(5), || !pid_alive(pid)),
        "daemon PID {pid} must exit after `daemon stop`"
    );

    // (c) the kernel mountpoint is gone — `metadata()` against the
    // file under it must fail. ENOTCONN, ENOENT, or EIO are all
    // valid post-unmount outcomes; we just require failure.
    assert!(
        fs::metadata(format!("{mount_path}/greet.txt")).is_err(),
        "mount point must be inaccessible after daemon stop"
    );

    // (d) `mounts.json` is gone. With the recorded PID dead and
    // `endpoint.json` removed, no actor remains that could
    // re-create the file: the daemon exited *after*
    // `MountRegistry::shutdown_all` (which removes mounts.json) and
    // *after* `remove_endpoint` (see `cmd_daemon_stop`'s rustdoc
    // for the strict cleanup ordering). The CLI-side
    // `sweep_stale_mounts` is itself idempotent. Therefore this is
    // a hard assertion — flake here points at a real teardown
    // regression, not a race.
    assert!(
        !registry_path(main.path()).exists(),
        "mounts.json must be removed after daemon stop (daemon's \
         shutdown_all sequences before remove_endpoint, and the CLI \
         waits for daemon PID death before returning)"
    );
}

// ---------------------------------------------------------------------------
// Scenario 5: stale-endpoint sweep.
//
// Manually craft endpoint+registry files pointing at a dead PID and
// a phantom mount path. The next `--daemon` use must (a) detect the
// stale endpoint, (b) sweep the phantom mount path (best-effort —
// it never existed, so `fusermount -u` will fail silently), (c)
// respawn at a fresh PID, (d) successfully mount a new thread.
//
// The CLI sweep logic lives in `daemon::client::ensure_daemon_endpoint`.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires Linux + FUSE + heddle built with --features mount"]
fn stale_endpoint_is_swept_and_daemon_respawns() {
    if !fuse_supported_or_skip("stale_endpoint_is_swept_and_daemon_respawns") {
        return;
    }
    let main = setup_repo("greet.txt", "after stale sweep");

    let endpoint = endpoint_path(main.path());
    let registry = registry_path(main.path());
    fs::create_dir_all(endpoint.parent().unwrap()).unwrap();

    // Stale endpoint: protocol version 2 (current) so the staleness
    // signal is the dead PID, not a version mismatch. 0x7fff_fffe
    // is the same dead-PID sentinel used by the unit tests in
    // `daemon::client`.
    let stale_pid: u32 = 0x7fff_fffe;
    let stale_endpoint = json!({
        "version": 2,
        "host": "127.0.0.1",
        "port": 1u16,
        "pid": stale_pid,
    });
    fs::write(
        &endpoint,
        serde_json::to_vec_pretty(&stale_endpoint).unwrap(),
    )
    .unwrap();

    // Phantom mounts.json entry. The path doesn't exist, which
    // matches the "wedged kernel mount whose process died" failure
    // mode the sweep is meant to recover from. `fusermount -u`
    // against a non-existent path fails silently — that's fine,
    // the sweep is best-effort.
    let phantom_path = main.path().join("__phantom_mount__");
    let registry_payload = json!({
        "mounts": [{
            "thread_id": "ghost",
            "mount_path": phantom_path.to_str().unwrap(),
            "pid": stale_pid,
            "since_ms": 0u64,
        }]
    });
    fs::write(
        &registry,
        serde_json::to_vec_pretty(&registry_payload).unwrap(),
    )
    .unwrap();

    // Now hit the daemon path. The client must sweep, respawn, and
    // mount cleanly.
    let raw = heddle(
        &[
            "--output",
            "json",
            "start",
            "post-sweep",
            "--workspace",
            "virtualized",
            "--daemon",
        ],
        Some(main.path()),
    )
    .expect("post-sweep start must succeed after stale endpoint cleanup");
    let mount_path = mount_path_from_start(&raw);

    // (a) endpoint file now points at a *live* PID, not the stale
    // sentinel. The freshly spawned daemon owns it.
    let new_pid = endpoint_pid(main.path()).expect("respawned daemon wrote endpoint");
    assert_ne!(
        new_pid, stale_pid,
        "endpoint file must record the respawned daemon's PID, not the sentinel"
    );
    assert!(pid_alive(new_pid), "respawned daemon PID must be live");

    // (d) the new mount actually serves the right content.
    let observed = fs::read_to_string(format!("{mount_path}/greet.txt"))
        .expect("read through respawned daemon's mount");
    assert_eq!(observed, "after stale sweep");

    heddle(&["thread", "drop", "post-sweep"], Some(main.path())).expect("drop post-sweep");
    try_stop_daemon(main.path());
}

// ---------------------------------------------------------------------------
// Scenario 6: daemon dies with mount alive.
//
// SIGKILL the daemon while it owns a live FUSE session. The kernel
// mount is then "wedged" — `BackgroundSession::drop` never ran.
// Assert (a) reads against the mount fail (EIO/ENOTCONN/ESHUTDOWN),
// (b) the next CLI invocation sweeps the stale endpoint and
// respawns cleanly, (c) re-mounting the same thread works.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires Linux + FUSE + heddle built with --features mount"]
fn daemon_killed_mid_mount_recovers_on_next_invocation() {
    if !fuse_supported_or_skip("daemon_killed_mid_mount_recovers_on_next_invocation") {
        return;
    }
    let main = setup_repo("greet.txt", "before kill");

    let raw = heddle(
        &[
            "--output",
            "json",
            "start",
            "kill-test",
            "--workspace",
            "virtualized",
            "--daemon",
        ],
        Some(main.path()),
    )
    .expect("--daemon start before kill");
    let mount_path = mount_path_from_start(&raw);
    let pid = endpoint_pid(main.path()).expect("daemon endpoint must record PID after start");

    // Sanity: read works while the daemon is alive.
    assert_eq!(
        fs::read_to_string(format!("{mount_path}/greet.txt")).unwrap(),
        "before kill"
    );

    // Hard kill — no chance for `BackgroundSession::drop` to run,
    // so the kernel mount stays wedged.
    kill_pid(pid, libc::SIGKILL);
    assert!(
        wait_until(Duration::from_secs(5), || !pid_alive(pid)),
        "daemon PID must die after SIGKILL"
    );

    // (a) reads against the wedged mount must fail. The kernel
    // typically returns ENOTCONN once the userspace handler is
    // gone, but EIO and ESHUTDOWN are all valid outcomes — we just
    // require a hard error rather than silent stale reads.
    let read_err = fs::read_to_string(format!("{mount_path}/greet.txt"));
    assert!(
        read_err.is_err(),
        "read against wedged mount must error after daemon SIGKILL; got: {:?}",
        read_err
    );

    // (b)+(c) the next CLI invocation must sweep + respawn + remount
    // cleanly. We use a *different* thread to side-step any in-CLI
    // memoization of the existing thread state, then verify the
    // recovered daemon is the path serving the recovered mount.
    let raw_recovery = heddle(
        &[
            "--output",
            "json",
            "start",
            "after-kill",
            "--workspace",
            "virtualized",
            "--daemon",
        ],
        Some(main.path()),
    )
    .expect("post-kill start must respawn the daemon and mount cleanly");
    let recovery_path = mount_path_from_start(&raw_recovery);

    let recovered_pid = endpoint_pid(main.path()).expect("respawned daemon wrote endpoint");
    assert_ne!(
        recovered_pid, pid,
        "respawned daemon must have a different PID than the SIGKILLed one"
    );
    assert!(pid_alive(recovered_pid), "respawned daemon must be alive");

    let observed = fs::read_to_string(format!("{recovery_path}/greet.txt"))
        .expect("read through respawned daemon's mount for the new thread");
    assert_eq!(observed, "before kill");

    // Cleanup. Don't try to drop kill-test through the daemon —
    // the daemon never re-acquired ownership of that wedged mount.
    // The sweep already best-effort `fusermount -u`'d it.
    heddle(&["thread", "drop", "after-kill"], Some(main.path())).expect("drop after-kill");
    try_stop_daemon(main.path());
}

// ---------------------------------------------------------------------------
// Scenario 7: `--from <state-spec>` + `--daemon`.
//
// The state-resolution path (`repo.resolve_state`) runs in
// `start_thread` *before* the mount RPC is sent, so a daemon-mounted
// thread should serve the correct historic state. Mirror the
// `virtualized_from_other_thread_head_serves_that_threads_tip` test
// but with `--daemon`.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires Linux + FUSE + heddle built with --features mount"]
fn daemon_mount_with_from_serves_resolved_state() {
    if !fuse_supported_or_skip("daemon_mount_with_from_serves_resolved_state") {
        return;
    }
    // S1: setup_repo's initial snapshot (greet.txt = "S1").
    let main = setup_repo("greet.txt", "S1");

    // S2: advance HEAD so HEAD~1 points back at S1. If the mount
    // ignored `--from`, we'd see "S2".
    fs::write(main.path().join("greet.txt"), "S2").unwrap();
    let _s2 = capture_short(main.path(), "S2 in main");

    let raw = heddle(
        &[
            "--output",
            "json",
            "start",
            "from-daemon",
            "--workspace",
            "virtualized",
            "--daemon",
            "--from",
            "HEAD~1",
        ],
        Some(main.path()),
    )
    .expect("--daemon --from HEAD~1 start");
    let mount_path = mount_path_from_start(&raw);

    let observed = fs::read_to_string(format!("{mount_path}/greet.txt"))
        .expect("read through daemon-owned --from mount");
    assert_eq!(
        observed, "S1",
        "--daemon --from HEAD~1 must serve S1, not S2"
    );

    heddle(&["thread", "drop", "from-daemon"], Some(main.path())).expect("drop from-daemon");
    try_stop_daemon(main.path());
}

// ---------------------------------------------------------------------------
// Scenario 8 (defensive): `daemon status` against an absent daemon.
//
// `cmd_daemon_status` is documented as a no-op success when the
// daemon isn't running — explicitly so scripts can probe it. This
// test pins that contract so the default-flip PR doesn't silently
// regress it.
// ---------------------------------------------------------------------------

#[test]
fn daemon_status_is_noop_success_when_daemon_absent() {
    let main = TempDir::new().unwrap();
    heddle(&["init"], Some(main.path())).unwrap();

    let status = heddle(&["daemon", "status"], Some(main.path()))
        .expect("daemon status must succeed even with no daemon running");
    let status: serde_json::Value =
        serde_json::from_str(&status).expect("captured daemon status should use the JSON contract");
    assert_eq!(status["status"], "not_running");
    assert_eq!(status["running"], false);
}
