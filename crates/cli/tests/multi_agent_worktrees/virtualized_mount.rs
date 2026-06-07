// SPDX-License-Identifier: Apache-2.0
//! End-to-end test for `--workspace light` thread starts.
//!
//! Gated to Linux because the FUSE shell only compiles there, and
//! `#[ignore]` because the CI box has to expose `/dev/fuse` and a
//! `fusermount` binary on PATH for the kernel-side mount to
//! succeed. Run locally with:
//!
//! ```sh
//! cargo test -p cli --features mount --test multi_agent_worktrees \
//!   virtualized_mount -- --ignored
//! ```
//!
//! The test does the round-trip: init repo, snapshot a file, start
//! a virtualized thread, read the file back through the mount,
//! drop the thread, and confirm the mount point is gone (or empty
//! and unmounted).
//!
//! As of 2026-05-02 the default for `--workspace light` is
//! the long-lived `heddled` daemon. Most of the round-trip tests
//! below explicitly pin themselves to `--no-daemon` to keep their
//! assertions focused on the FUSE projection itself rather than
//! daemon lifecycle. The dedicated `default_uses_daemon_when_no_flag`
//! and `no_daemon_flag_uses_in_process` cases live below to lock in
//! the dispatch behaviour.

#![cfg(target_os = "linux")]

use std::fs;

use serde_json::Value;

use super::{heddle, setup_repo};

#[test]
#[ignore = "requires Linux + FUSE + heddle built with --features mount"]
fn virtualized_thread_round_trip() {
    let main = setup_repo("greet.txt", "hello from heddle");

    // Start a virtualized thread. The mount point lives under the
    // repo's reserved `.heddle/threads/<name>` dir (excluded from the
    // parent repo's overlay/status traversal), and `heddle --output
    // json` prints the resolved path under `thread.path`.
    let raw = heddle(
        &[
            "--output", "json",
            "start",
            "feature/mount-demo",
            "--workspace",
            "virtualized",
            // Pin to in-process so the assertions below (mount path
            // shape, file readable through FUSE, drop tears down the
            // mount synchronously) don't have to reason about the
            // daemon's spawn / shutdown timing. The daemon path is
            // exercised by `default_uses_daemon_when_no_flag` below.
            "--no-daemon",
        ],
        Some(main.path()),
    )
    .expect("virtualized start succeeded; if this errors with `requires Linux + heddle built with --features mount` the test was run without --features mount");

    let out: Value = serde_json::from_str(&raw).unwrap();
    let mount_path = out
        .get("thread")
        .and_then(|t| t.get("path"))
        .and_then(Value::as_str)
        .expect("virtualized thread output should include the mount path under thread.path")
        .to_string();
    assert!(
        mount_path.contains("/.heddle/threads/"),
        "mount path should sit under the managed .heddle/threads/ root \
         (got {mount_path:?})"
    );

    // Read the snapshotted file through the mount. The mount is
    // lazy / content-addressed so this exercises the full
    // FuseShell -> ContentAddressedMount -> object-store path.
    let observed = fs::read_to_string(format!("{mount_path}/greet.txt"))
        .expect("read through FUSE mount succeeded");
    assert_eq!(observed, "hello from heddle");

    // Drop the thread. Triggers `unmount_thread_if_mounted` ->
    // BackgroundSession drop -> kernel unmount. The execution
    // path should be removed (or at minimum no longer mounted).
    heddle(&["thread", "drop", "feature/mount-demo"], Some(main.path()))
        .expect("thread drop after virtualized mount");

    // After drop, the mount point should not exist (rmdir ran on
    // the now-empty mountpoint). On a wedged unmount the path
    // would still be there but `read_dir` would return an EBUSY
    // — either way, we shouldn't be able to read greet.txt back.
    assert!(
        fs::read_to_string(format!("{mount_path}/greet.txt")).is_err(),
        "after drop, the file should no longer be readable through the mount"
    );
}

// ---------------------------------------------------------------------------
// `--from <state-spec>` resolution for virtualized threads.
//
// `start_thread` resolves `--from` via `repo.resolve_state(spec)` and pins
// the new thread's HEAD to the resolved `base_state` *before* the FUSE mount
// spawns (see `crates/cli/src/cli/commands/thread.rs:573`). Mount creation
// reads from `repo.refs().get_thread(name)` indirectly through
// `ContentAddressedMount::new(repo, thread_id)`, so anything that pins the
// thread ref before the mount call will appear in the kernel-served tree.
//
// These tests exercise three rungs of the resolver:
//   1. another thread's HEAD ("alpha") — ref-name path
//   2. a short change ID prefix          — `resolve_short_change_id` path
//   3. `HEAD~1`                          — `parse_head_steps` path
//
// All three should serve the *resolved* state through the FUSE projection,
// not the main repo's HEAD and not S1 leaked from the original setup_repo.
// ---------------------------------------------------------------------------

/// Helper: capture a snapshot in `cwd` and return the short change_id.
fn capture_short(cwd: &std::path::Path, msg: &str) -> String {
    let out =
        heddle(&["--output", "json", "capture", "-m", msg], Some(cwd)).expect("snapshot succeeded");
    let v: Value = serde_json::from_str(&out).expect("snapshot --output json is valid JSON");
    v.get("change_id")
        .and_then(Value::as_str)
        .expect("snapshot output exposes change_id")
        .to_string()
}

/// Helper: pull `thread.path` (the mount point) out of `start --output json` output.
fn mount_path_from_start(raw: &str) -> String {
    let out: Value = serde_json::from_str(raw).expect("start --output json output");
    out.get("thread")
        .and_then(|t| t.get("path"))
        .and_then(Value::as_str)
        .expect("virtualized thread output should include thread.path")
        .to_string()
}

#[test]
#[ignore = "requires Linux + FUSE + heddle built with --features mount"]
fn virtualized_from_other_thread_head_serves_that_threads_tip() {
    // S1: setup_repo's initial snapshot. greet.txt = "S1".
    let main = setup_repo("greet.txt", "S1");

    // S2: a second snapshot in the main repo. Lives on the `main` thread,
    // but more importantly it advances HEAD so we can tell apart "from
    // alpha's tip" from "from current HEAD".
    fs::write(main.path().join("greet.txt"), "S2").unwrap();
    let _s2 = capture_short(main.path(), "S2 in main");

    // Start `alpha` (visible/materialized) anchored at S1 so its tree
    // initially diverges from main's HEAD (= S2).
    let alpha_dir = tempfile::TempDir::new().unwrap();
    heddle(
        &[
            "start",
            "alpha",
            "--workspace",
            "materialized",
            "--from",
            "HEAD~1",
            "--path",
            alpha_dir.path().to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .expect("start alpha from HEAD~1");

    // Sanity: alpha's working tree hydrates from S1.
    let alpha_initial = fs::read_to_string(alpha_dir.path().join("greet.txt")).unwrap();
    assert_eq!(alpha_initial, "S1", "alpha should hydrate from S1");

    // S3: a snapshot taken inside alpha's worktree. After this, the
    // `alpha` ref points at S3.
    fs::write(alpha_dir.path().join("greet.txt"), "S3").unwrap();
    let _s3 = capture_short(alpha_dir.path(), "S3 on alpha");

    // Now spin up beta as a virtualized mount whose --from resolves
    // through `alpha`. The expected snapshot served through the FUSE
    // mount is alpha's tip, i.e. S3.
    let raw = heddle(
        &[
            "--output",
            "json",
            "start",
            "beta",
            "--workspace",
            "virtualized",
            "--from",
            "alpha",
            // In-process mount: this test exercises --from resolution,
            // not daemon lifecycle.
            "--no-daemon",
        ],
        Some(main.path()),
    )
    .expect("start beta --from alpha succeeded");

    let mount_path = mount_path_from_start(&raw);

    let observed = fs::read_to_string(format!("{mount_path}/greet.txt"))
        .expect("read through beta's FUSE mount");
    assert_eq!(
        observed, "S3",
        "beta --from alpha must serve alpha's tip (S3), not S1 or S2"
    );

    heddle(&["thread", "drop", "beta"], Some(main.path())).expect("drop beta to release the mount");
}

#[test]
#[ignore = "requires Linux + FUSE + heddle built with --features mount"]
fn virtualized_from_short_change_id_serves_that_state() {
    // S1: setup_repo's initial snapshot. greet.txt = "S1".
    let main = setup_repo("greet.txt", "S1");
    // The short ID for S1 is on disk after init — pull it from the log.
    let log: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "log", "main", "-n", "1"],
            Some(main.path()),
        )
        .expect("log main"),
    )
    .unwrap();
    let s1_short = log["states"][0]["change_id"]
        .as_str()
        .expect("log exposes change_id")
        .to_string();

    // S2: shift HEAD past S1 so the test would notice if --from were
    // silently ignored.
    fs::write(main.path().join("greet.txt"), "S2").unwrap();
    let _s2 = capture_short(main.path(), "S2 in main");

    let raw = heddle(
        &[
            "--output",
            "json",
            "start",
            "gamma",
            "--workspace",
            "virtualized",
            "--from",
            &s1_short,
            "--no-daemon",
        ],
        Some(main.path()),
    )
    .expect("start gamma --from <S1 short> succeeded");

    let mount_path = mount_path_from_start(&raw);

    let observed = fs::read_to_string(format!("{mount_path}/greet.txt"))
        .expect("read through gamma's FUSE mount");
    assert_eq!(
        observed, "S1",
        "gamma --from <S1 short> must serve S1, not S2"
    );

    heddle(&["thread", "drop", "gamma"], Some(main.path()))
        .expect("drop gamma to release the mount");
}

#[test]
#[ignore = "requires Linux + FUSE + heddle built with --features mount"]
fn virtualized_from_head_tilde_serves_parent_state() {
    // S1: setup_repo's initial snapshot. greet.txt = "S1".
    let main = setup_repo("greet.txt", "S1");

    // S2: advance HEAD so HEAD~1 points back at S1.
    fs::write(main.path().join("greet.txt"), "S2").unwrap();
    let _s2 = capture_short(main.path(), "S2 in main");

    let raw = heddle(
        &[
            "--output",
            "json",
            "start",
            "delta",
            "--workspace",
            "virtualized",
            "--from",
            "HEAD~1",
            "--no-daemon",
        ],
        Some(main.path()),
    )
    .expect("start delta --from HEAD~1 succeeded");

    let mount_path = mount_path_from_start(&raw);

    let observed = fs::read_to_string(format!("{mount_path}/greet.txt"))
        .expect("read through delta's FUSE mount");
    assert_eq!(
        observed, "S1",
        "delta --from HEAD~1 must serve S1 (the parent of HEAD), not S2"
    );

    heddle(&["thread", "drop", "delta"], Some(main.path()))
        .expect("drop delta to release the mount");
}

// ---------------------------------------------------------------------------
// Default-flip coverage (2026-05-02).
//
// `--workspace light` defaults to the `heddled` daemon. The two tests
// below assert that dispatch:
//
//   1. `default_uses_daemon_when_no_flag` — no flag means daemon. The
//      daemon writes its endpoint at `.heddle/state/heddled.endpoint.json`
//      after a successful spawn; we observe that file as the strongest
//      filesystem-visible signal that the daemon path was taken.
//   2. `no_daemon_flag_uses_in_process` — explicit `--no-daemon` skips
//      the daemon entirely. The endpoint file must not exist after the
//      start completes.
//
// Both tests are `#[ignore]` because they need the same `/dev/fuse` +
// `fusermount` setup as `virtualized_thread_round_trip`. Run locally with
// the same `--ignored` invocation.
// ---------------------------------------------------------------------------

/// Path to the heddled endpoint file inside a repo's state dir.
/// Mirrors `repo::daemon::mount_daemon_endpoint_path` without taking
/// a dep on the crate from this integration-test file.
fn heddled_endpoint_path(repo_root: &std::path::Path) -> std::path::PathBuf {
    repo_root
        .join(".heddle")
        .join("state")
        .join("heddled.endpoint.json")
}

#[test]
#[ignore = "requires Linux + FUSE + heddle built with --features mount"]
fn default_uses_daemon_when_no_flag() {
    let main = setup_repo("greet.txt", "default-daemon");
    let endpoint = heddled_endpoint_path(main.path());
    assert!(
        !endpoint.exists(),
        "fresh repo should not have a daemon endpoint before any virtualized start"
    );

    heddle(
        &[
            "--output",
            "json",
            "start",
            "feature/default-daemon",
            "--workspace",
            "virtualized",
        ],
        Some(main.path()),
    )
    .expect("default virtualized start should spawn the daemon");

    assert!(
        endpoint.exists(),
        "default `--workspace light` (no flag) must hand the mount \
         to `heddled`, leaving the endpoint file at {} as evidence",
        endpoint.display(),
    );

    // Tear down. `thread drop` issues `unmount_via_daemon`; the
    // daemon then idles out on its own, but the endpoint file may
    // linger until the daemon's idle exit. We don't assert on that
    // — we assert on the dispatch decision, which is the contract
    // this test is locking in.
    heddle(
        &["thread", "drop", "feature/default-daemon"],
        Some(main.path()),
    )
    .expect("drop daemon-owned thread");
}

#[test]
#[ignore = "requires Linux + FUSE + heddle built with --features mount"]
fn no_daemon_flag_uses_in_process() {
    let main = setup_repo("greet.txt", "in-process-only");
    let endpoint = heddled_endpoint_path(main.path());
    assert!(
        !endpoint.exists(),
        "fresh repo should not have a daemon endpoint before any virtualized start"
    );

    heddle(
        &[
            "--output",
            "json",
            "start",
            "feature/no-daemon",
            "--workspace",
            "virtualized",
            "--no-daemon",
        ],
        Some(main.path()),
    )
    .expect("--no-daemon virtualized start should succeed in-process");

    assert!(
        !endpoint.exists(),
        "explicit `--no-daemon` must NOT spawn the daemon; \
         endpoint file at {} should not exist",
        endpoint.display(),
    );

    heddle(&["thread", "drop", "feature/no-daemon"], Some(main.path()))
        .expect("drop in-process virtualized thread");
}
