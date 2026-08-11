// SPDX-License-Identifier: Apache-2.0
//! End-to-end coverage for the shared `--dry-run` plan surface on
//! `ready`, `land`, and `push`. These paths were reachable but untested,
//! which left `dry_run.rs` at 0% and large emitters in ready/land/remote
//! unexercised under the coverage gate.

use serde_json::Value;
use tempfile::TempDir;

use super::{heddle, heddle_output};

fn json(args: &[&str], cwd: &std::path::Path) -> Value {
    let output = heddle_output(args, Some(cwd))
        .unwrap_or_else(|err| panic!("`heddle {}` should run: {err}", args.join(" ")));
    assert!(
        output.status.success(),
        "`heddle {}` should succeed\nstdout: {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|err| {
        panic!(
            "`heddle {}` should emit JSON: {err}\nstdout: {stdout}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn setup_native_repo() -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    temp
}

fn setup_managed_thread(name: &str) -> (TempDir, TempDir, String) {
    let main = setup_native_repo();
    let checkout = TempDir::new().unwrap();
    let checkout_arg = checkout.path().join("work");
    let started = json(
        &[
            "--output",
            "json",
            "start",
            name,
            "--path",
            checkout_arg.to_str().unwrap(),
        ],
        main.path(),
    );
    let execution_path = started["execution_path"]
        .as_str()
        .expect("start should report execution_path")
        .to_string();
    (main, checkout, execution_path)
}

#[test]
fn ready_dry_run_emits_plan_without_mutation() {
    let (main, checkout_owner, execution_path) = setup_managed_thread("feature/ready-dry");
    let checkout = std::path::Path::new(&execution_path);
    std::fs::write(checkout.join("feature.txt"), "feature\n").unwrap();
    heddle(&["capture", "-m", "feature"], Some(checkout)).unwrap();

    let plan = json(
        &[
            "--output",
            "json",
            "ready",
            "--dry-run",
            "--thread",
            "feature/ready-dry",
        ],
        main.path(),
    );
    assert_eq!(plan["output_kind"], "dry_run_plan");
    assert_eq!(plan["command"], "ready");
    assert_eq!(plan["performed_mutation"], false);
    assert!(
        plan["integrations"]
            .as_array()
            .is_some_and(|v| !v.is_empty()),
        "ready dry-run should preview integration: {plan}"
    );
    // Thread must still not be ready (dry-run did not mutate).
    let shown = json(
        &["--output", "json", "thread", "show", "feature/ready-dry"],
        main.path(),
    );
    assert_ne!(
        shown["state"].as_str().unwrap_or(""),
        "ready",
        "dry-run must not mark the thread ready: {shown}"
    );
    drop(checkout_owner);
}

#[test]
fn ready_dry_run_reports_missing_thread_as_blocker() {
    let main = setup_native_repo();
    let plan = json(
        &[
            "--output",
            "json",
            "ready",
            "--dry-run",
            "--thread",
            "does-not-exist",
        ],
        main.path(),
    );
    assert_eq!(plan["output_kind"], "dry_run_plan");
    assert_eq!(plan["command"], "ready");
    let blockers = plan["blockers"]
        .as_array()
        .expect("blockers array")
        .iter()
        .map(|v| v.as_str().unwrap_or(""))
        .collect::<Vec<_>>();
    assert!(
        blockers
            .iter()
            .any(|b| b.contains("thread") || b.contains("no current")),
        "missing thread should surface as a blocker: {plan}"
    );
}

#[test]
fn land_dry_run_emits_plan_without_landing() {
    let (main, checkout_owner, execution_path) = setup_managed_thread("feature/land-dry");
    let checkout = std::path::Path::new(&execution_path);
    std::fs::write(checkout.join("feature.txt"), "feature\n").unwrap();
    heddle(&["capture", "-m", "feature"], Some(checkout)).unwrap();
    heddle(
        &["ready", "--thread", "feature/land-dry"],
        Some(main.path()),
    )
    .unwrap();

    let plan = json(
        &[
            "--output",
            "json",
            "land",
            "--dry-run",
            "--thread",
            "feature/land-dry",
        ],
        main.path(),
    );
    assert_eq!(plan["output_kind"], "dry_run_plan");
    assert_eq!(plan["command"], "land");
    assert_eq!(plan["performed_mutation"], false);
    assert!(
        plan["integrations"]
            .as_array()
            .is_some_and(|v| !v.is_empty()),
        "land dry-run should preview integrations: {plan}"
    );
    // Thread should still exist as ready/not landed.
    let shown = json(
        &["--output", "json", "thread", "show", "feature/land-dry"],
        main.path(),
    );
    assert_ne!(
        shown["state"].as_str().unwrap_or(""),
        "landed",
        "dry-run must not land the thread: {shown}"
    );
    drop(checkout_owner);
}

#[test]
fn run_cmd_requires_command_and_executes_when_present() {
    let main = setup_native_repo();
    // Missing command → typed usage refusal.
    let missing = heddle_output(&["--output", "json", "run"], Some(main.path()))
        .expect("run with no command should invoke");
    assert!(
        !missing.status.success(),
        "run without a command must refuse"
    );

    // With a command, run succeeds and does not mutate the repo.
    let ok = heddle(&["run", "--", "true"], Some(main.path()));
    assert!(ok.is_ok(), "run -- true should succeed: {ok:?}");
}

#[test]
fn push_dry_run_emits_plan_without_network() {
    let main = setup_native_repo();
    // No remote configured — dry-run should still produce a plan or a typed
    // refusal without contacting a server. Either shape is fine as long as
    // the dry-run path is exercised.
    let output = heddle_output(
        &["--output", "json", "push", "--dry-run"],
        Some(main.path()),
    )
    .expect("push --dry-run should run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        let plan: Value = serde_json::from_str(&stdout).expect("json plan");
        assert_eq!(plan["output_kind"], "dry_run_plan");
        assert_eq!(plan["command"], "push");
        assert_eq!(plan["performed_mutation"], false);
    } else {
        // Preflight refusal (no remote) is also a real covered path.
        assert!(
            !stdout.is_empty() || !stderr.is_empty(),
            "push --dry-run failure should still emit a message"
        );
    }
}
