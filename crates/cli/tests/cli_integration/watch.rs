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
    io::{BufRead, BufReader, Read},
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
) -> (
    std::process::Child,
    mpsc::Receiver<String>,
    mpsc::Receiver<String>,
) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.arg("watch");
    for a in args {
        cmd.arg(a);
    }
    cmd.current_dir(cwd)
        .env("HEDDLE_CONFIG", cwd.join(".heddle-user/config.toml"))
        .env("HOME", cwd.join(".heddle-test-home"))
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn heddle watch");
    let stdout = child.stdout.take().expect("stdout pipe");
    let stderr = child.stderr.take().expect("stderr pipe");
    (child, spawn_line_reader(stdout), spawn_line_reader(stderr))
}

fn spawn_line_reader<R>(stream: R) -> mpsc::Receiver<String>
where
    R: Read + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
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

fn coverage_instrumented() -> bool {
    std::env::var_os("LLVM_PROFILE_FILE").is_some() || std::env::var_os("CARGO_LLVM_COV").is_some()
}

fn watch_startup_delay() -> Duration {
    if coverage_instrumented() {
        Duration::from_secs(2)
    } else {
        Duration::from_millis(250)
    }
}

fn watch_event_deadline() -> Duration {
    if coverage_instrumented() {
        Duration::from_secs(6)
    } else {
        Duration::from_secs(3)
    }
}

fn watch_retry_delay() -> Duration {
    if coverage_instrumented() {
        Duration::from_millis(750)
    } else {
        Duration::from_millis(100)
    }
}

fn watch_trigger_attempts() -> usize {
    if coverage_instrumented() { 5 } else { 2 }
}

fn stop_watch(child: &mut std::process::Child, stderr_rx: &mpsc::Receiver<String>) -> String {
    let _ = child.kill();
    let _ = child.wait();
    collect_lines_until(
        stderr_rx,
        Instant::now() + Duration::from_millis(250),
        |_| false,
    )
    .join("\n")
}

fn snapshot_change_id(output: &str) -> String {
    output
        .lines()
        .find_map(|line| {
            line.split_whitespace()
                .find(|tok| tok.starts_with("hd-"))
                .map(str::to_string)
        })
        .expect("snapshot output should contain hd-... short id")
}

fn has_line<F>(lines: &[String], mut predicate: F) -> bool
where
    F: FnMut(&str) -> bool,
{
    lines.iter().any(|line| predicate(line))
}

fn text_watch_observed(lines: &[String], expected_change_ids: &[String]) -> bool {
    has_line(lines, |line| {
        line.contains("snapshot")
            && expected_change_ids
                .iter()
                .any(|change_id| line.contains(change_id))
    })
}

fn json_watch_observed(lines: &[String]) -> bool {
    has_line(lines, |line| {
        line.starts_with('{') && line.contains("\"kind\":\"snapshot\"")
    })
}

fn missing_watch_context(lines: &[String], stderr: &str) -> String {
    format!(
        "stdout ({} lines): {}\nstderr: {}",
        lines.len(),
        lines.join("\n"),
        stderr
    )
}

fn watch_attempt_deadline() -> Instant {
    Instant::now() + watch_event_deadline()
}

fn sleep_before_watch_trigger(attempt: usize) {
    thread::sleep(if attempt == 0 {
        watch_startup_delay()
    } else {
        watch_retry_delay()
    });
}

fn watch_max_iterations() -> &'static str {
    if coverage_instrumented() { "50" } else { "10" }
}

fn watch_filter_deadline() -> Duration {
    if coverage_instrumented() {
        Duration::from_secs(8)
    } else {
        Duration::from_secs(3)
    }
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

    let (mut child, rx, stderr_rx) = spawn_watch(
        temp.path(),
        &[
            "--max-iterations",
            watch_max_iterations(),
            "--poll-interval-ms",
            "50",
        ],
    );

    let mut lines = Vec::new();
    let mut expected_change_ids = Vec::new();
    for attempt in 0..watch_trigger_attempts() {
        sleep_before_watch_trigger(attempt);
        std::fs::write(
            temp.path().join(format!("hello-{attempt}.txt")),
            format!("world {attempt}"),
        )
        .unwrap();
        let snap_output = heddle(
            &["capture", "-m", &format!("feat: hello world {attempt}")],
            Some(temp.path()),
        )
        .expect("snapshot succeeds");
        expected_change_ids.push(snapshot_change_id(&snap_output));
        lines.extend(collect_lines_until(&rx, watch_attempt_deadline(), |line| {
            line.contains("snapshot")
        }));
        if text_watch_observed(&lines, &expected_change_ids) {
            break;
        }
    }

    let stderr = stop_watch(&mut child, &stderr_rx);
    let combined = lines.join("\n");
    let context = missing_watch_context(&lines, &stderr);
    assert!(
        combined.contains("snapshot"),
        "watch output missing snapshot kind: {context}"
    );
    assert!(
        expected_change_ids
            .iter()
            .any(|change_id| combined.contains(change_id)),
        "watch output missing expected change_id {:?}: {context}",
        expected_change_ids
    );
}

#[test]
fn watch_emits_json_per_line() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("seed.txt"), "seed").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let (mut child, rx, stderr_rx) = spawn_watch(
        temp.path(),
        &[
            "--output",
            "json",
            "--max-iterations",
            watch_max_iterations(),
            "--poll-interval-ms",
            "50",
        ],
    );

    let mut lines = Vec::new();
    for attempt in 0..watch_trigger_attempts() {
        sleep_before_watch_trigger(attempt);
        std::fs::write(temp.path().join(format!("a-{attempt}.txt")), "a").unwrap();
        heddle(
            &["capture", "-m", &format!("json mode test {attempt}")],
            Some(temp.path()),
        )
        .unwrap();
        lines.extend(collect_lines_until(&rx, watch_attempt_deadline(), |line| {
            // First well-formed JSON line with kind=snapshot ends this
            // attempt. The schema check happens after kill() below.
            line.starts_with('{') && line.contains("\"kind\":\"snapshot\"")
        }));
        if json_watch_observed(&lines) {
            break;
        }
    }

    let stderr = stop_watch(&mut child, &stderr_rx);

    let json_line = lines
        .iter()
        .find(|line| line.starts_with('{'))
        .unwrap_or_else(|| {
            panic!(
                "no JSON line in watch output: {}",
                missing_watch_context(&lines, &stderr)
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
    let (mut child, rx, stderr_rx) = spawn_watch(
        temp.path(),
        &[
            "--output",
            "json",
            "--filter",
            "thread_create",
            "--max-iterations",
            watch_max_iterations(),
            "--poll-interval-ms",
            "50",
        ],
    );
    thread::sleep(watch_startup_delay());

    std::fs::write(temp.path().join("filtered.txt"), "x").unwrap();
    heddle(&["capture", "-m", "should be filtered"], Some(temp.path())).unwrap();

    // Wait briefly for any output. Filter test passes iff *no*
    // snapshot line appears.
    let deadline = Instant::now() + watch_filter_deadline();
    let lines = collect_lines_until(&rx, deadline, |_| false);

    let stderr = stop_watch(&mut child, &stderr_rx);

    let snapshot_lines: Vec<_> = lines
        .iter()
        .filter(|l| l.contains("\"kind\":\"snapshot\""))
        .collect();
    assert!(
        snapshot_lines.is_empty(),
        "filter should drop snapshot kind, got: {snapshot_lines:?}; stderr: {stderr}"
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
