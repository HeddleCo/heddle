// SPDX-License-Identifier: Apache-2.0
//! Integration tests for `heddle watch`.
//!
//! Strategy: spawn `heddle watch` as a child process, run a few
//! capture/thread operations from the test thread, then read the
//! child's stdout. We use the `--max-iterations` hidden test flag
//! so the watcher exits cleanly after observing N modify events
//! instead of relying on signal delivery (which is flaky on macOS
//! and Linux CI containers without a controlling terminal).
//!
//! Flake mitigations:
//!   - `notify` debounce is small (50ms via `--poll-interval-ms`)
//!     so per-event latency stays predictable.
//!   - `--max-iterations` is set generously (10) — the test
//!     produces fewer events than that and relies on the bounded
//!     `wait_with_timeout` to bound total runtime.
//!   - The whole test gives the child up to ~10s to complete.

use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;

use super::heddle;

/// Spawn `heddle watch` with the given args; return `(child,
/// rx)` where `rx` yields one stdout line per receive. The reader
/// thread terminates when the child closes stdout.
fn spawn_watch(
    cwd: &std::path::Path,
    args: &[&str],
) -> (std::process::Child, mpsc::Receiver<String>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.arg("watch");
    for a in args {
        cmd.arg(a);
    }
    cmd.current_dir(cwd)
        .env("HEDDLE_CONFIG", cwd.join(".heddle-user/config.toml"))
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn heddle watch");
    let stdout = child.stdout.take().expect("stdout pipe");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (child, rx)
}

fn collect_lines_until<F>(
    rx: &mpsc::Receiver<String>,
    deadline: Instant,
    mut predicate: F,
) -> Vec<String>
where
    F: FnMut(&str) -> bool,
{
    let mut out = Vec::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(line) => {
                let matched = predicate(&line);
                out.push(line);
                if matched {
                    return out;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    out
}

#[test]
fn watch_streams_snapshot_text_mode() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    // Initial snapshot before watch starts so the repo's oplog
    // already exists; we want to test the *tail* path, not first
    // creation.
    std::fs::write(temp.path().join("seed.txt"), "seed").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let (mut child, rx) = spawn_watch(
        temp.path(),
        &["--max-iterations", "5", "--poll-interval-ms", "50"],
    );

    // Give the watcher a beat to install its notify watcher before
    // we trigger the first observed event. Without this, the
    // capture below races the watcher and the modify event lands
    // before the watcher sees the file. 250ms is enough on
    // macOS/Linux even under load.
    thread::sleep(Duration::from_millis(250));

    std::fs::write(temp.path().join("hello.txt"), "world").unwrap();
    let snap_output = heddle(&["capture", "-m", "feat: hello world"], Some(temp.path()))
        .expect("snapshot succeeds");
    let change_id_short = snap_output
        .lines()
        .find_map(|line| {
            line.split_whitespace()
                .find(|tok| tok.starts_with("hd-"))
                .map(str::to_string)
        })
        .expect("snapshot output should contain hd-… short id");

    let deadline = Instant::now() + Duration::from_secs(8);
    let lines = collect_lines_until(&rx, deadline, |line| {
        line.contains(&change_id_short) || line.contains("snapshot")
    });

    let _ = child.kill();
    let _ = child.wait();

    let combined = lines.join("\n");
    assert!(
        combined.contains("snapshot"),
        "watch output missing snapshot kind: {combined}"
    );
    assert!(
        combined.contains(&change_id_short),
        "watch output missing change_id {change_id_short}: {combined}"
    );
}

#[test]
fn watch_emits_json_per_line() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let (mut child, rx) = spawn_watch(
        temp.path(),
        &[
            "--json",
            "--max-iterations",
            "3",
            "--poll-interval-ms",
            "50",
        ],
    );
    thread::sleep(Duration::from_millis(250));

    std::fs::write(temp.path().join("a.txt"), "a").unwrap();
    heddle(&["capture", "-m", "json mode test"], Some(temp.path())).unwrap();

    let deadline = Instant::now() + Duration::from_secs(8);
    let lines = collect_lines_until(&rx, deadline, |line| {
        // First well-formed JSON line with kind=snapshot ends our
        // wait. The schema check happens after kill() below.
        line.starts_with('{') && line.contains("\"kind\":\"snapshot\"")
    });

    let _ = child.kill();
    let _ = child.wait();

    let json_line = lines
        .iter()
        .find(|line| line.starts_with('{'))
        .unwrap_or_else(|| {
            panic!(
                "no JSON line in watch output (got {} lines): {}",
                lines.len(),
                lines.join("\n")
            )
        });
    let value: Value = serde_json::from_str(json_line)
        .unwrap_or_else(|err| panic!("invalid JSON {json_line:?}: {err}"));

    // Schema: ts, kind, change_id, intent, confidence, actor, id, thread.
    assert!(value["ts"].is_string(), "ts missing");
    assert_eq!(value["kind"], "snapshot");
    assert!(value["change_id"].is_string(), "change_id missing");
    // intent may be null if config didn't propagate; just assert
    // presence of the field.
    assert!(value.get("intent").is_some(), "intent field missing");
    assert!(value["id"].is_u64(), "id missing");
}

#[test]
fn watch_filter_drops_non_matching_kinds() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    // Filter to *only* thread_create — a snapshot must NOT pass.
    let (mut child, rx) = spawn_watch(
        temp.path(),
        &[
            "--json",
            "--filter",
            "thread_create",
            "--max-iterations",
            "5",
            "--poll-interval-ms",
            "50",
        ],
    );
    thread::sleep(Duration::from_millis(250));

    std::fs::write(temp.path().join("filtered.txt"), "x").unwrap();
    heddle(&["capture", "-m", "should be filtered"], Some(temp.path())).unwrap();

    // Wait briefly for any output. Filter test passes iff *no*
    // snapshot line appears.
    let deadline = Instant::now() + Duration::from_secs(3);
    let lines = collect_lines_until(&rx, deadline, |_| false);

    let _ = child.kill();
    let _ = child.wait();

    let snapshot_lines: Vec<_> = lines
        .iter()
        .filter(|l| l.contains("\"kind\":\"snapshot\""))
        .collect();
    assert!(
        snapshot_lines.is_empty(),
        "filter should drop snapshot kind, got: {snapshot_lines:?}"
    );
}

#[test]
fn watch_rejects_unknown_filter_kind() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args(["watch", "--filter", "totally_bogus_kind"])
        .current_dir(temp.path())
        .env(
            "HEDDLE_CONFIG",
            temp.path().join(".heddle-user/config.toml"),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().expect("spawn");
    assert!(
        !output.status.success(),
        "expected nonzero exit on bogus filter, got status {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown event kind") || stderr.contains("totally_bogus_kind"),
        "stderr should explain rejection: {stderr}"
    );
}