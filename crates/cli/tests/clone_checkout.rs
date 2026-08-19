// SPDX-License-Identifier: Apache-2.0
//! Clone walks for HeddleCo/heddle#1216:
//! 1. A fresh native clone must check out the remote default thread.
//! 2. Clone copies saved history only — source dirt is not contagious.

use std::{
    path::Path,
    process::{Command, Output},
};

use serde_json::Value;
use tempfile::TempDir;

fn heddle_output(args: &[&str], cwd: &Path) -> Output {
    let config = cwd.join("heddle-config.toml");
    if !config.exists() {
        std::fs::write(
            &config,
            "[principal]\nname = \"Heddle Test\"\nemail = \"heddle@example.com\"\n",
        )
        .expect("write Heddle config");
    }
    Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(args)
        .current_dir(cwd)
        .env("HEDDLE_CONFIG", &config)
        .env("HOME", cwd)
        .env("HEDDLE_FSMONITOR", "off")
        .env("NO_COLOR", "1")
        .output()
        .unwrap_or_else(|err| panic!("invoke heddle {}: {err}", args.join(" ")))
}

fn heddle(args: &[&str], cwd: &Path) -> String {
    let output = heddle_output(args, cwd);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "heddle {} failed (exit {:?})\nstdout: {stdout}\nstderr: {stderr}",
        args.join(" "),
        output.status.code()
    );
    stdout
}

fn json_cmd(args: &[&str], cwd: &Path) -> Value {
    let stdout = heddle(args, cwd);
    serde_json::from_str(&stdout).unwrap_or_else(|err| {
        panic!(
            "heddle {} stdout is not JSON: {err}\n{stdout}",
            args.join(" ")
        )
    })
}

fn seed_native_source(root: &Path, captured: &str, dirty: Option<&str>) {
    heddle(&["init"], root);
    std::fs::write(root.join("README.md"), captured).expect("write captured file");
    heddle(&["capture", "-m", "seed"], root);
    if let Some(dirty) = dirty {
        std::fs::write(root.join("NOTES.md"), dirty).expect("write uncommitted dirt");
    }
}

fn assert_checked_out_thread(clone: &Path, expected: &str) {
    let status_text = heddle(&["status"], clone);
    assert!(
        !status_text.contains("detached HEAD"),
        "status text must not report detached HEAD after clone:\n{status_text}"
    );
    assert!(
        status_text.contains(expected),
        "status text must name thread {expected}:\n{status_text}"
    );

    let status = json_cmd(&["--output", "json", "status"], clone);
    assert_eq!(
        status["thread"], expected,
        "status JSON thread must be {expected}: {status}"
    );
    assert_eq!(
        status["thread"].as_str(),
        Some(expected),
        "fresh clone must be attached, not null/detached: {status}"
    );

    let list = json_cmd(&["--output", "json", "thread", "list"], clone);
    assert_eq!(
        list["current"], expected,
        "thread list current must be {expected}: {list}"
    );
    let threads = list["threads"]
        .as_array()
        .expect("thread list threads array");
    let current = threads
        .iter()
        .find(|thread| thread["name"] == expected)
        .unwrap_or_else(|| panic!("thread list must include {expected}: {list}"));
    assert_eq!(
        current["is_current"], true,
        "{expected} must be the active checkout: {list}"
    );

    let list_text = heddle(&["thread", "list"], clone);
    assert!(
        list_text.contains("Current"),
        "thread list text must have a Current section:\n{list_text}"
    );
    assert!(
        !(list_text.contains("Other threads")
            && !list_text.contains("Current")
            && list_text.contains(expected)),
        "default thread must not be listed only under Other threads:\n{list_text}"
    );

    let head = std::fs::read_to_string(clone.join(".heddle").join("HEAD"))
        .expect("read cloned Heddle HEAD");
    assert_eq!(
        head.trim(),
        format!("ref: {expected}"),
        "Heddle HEAD must attach to {expected}, got {head:?}"
    );
}

#[test]
fn clone_native_checks_out_default_thread() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.path().join("source");
    let clone = temp.path().join("clone");
    std::fs::create_dir_all(&source).expect("source dir");
    seed_native_source(&source, "captured\n", None);

    let clone_text = heddle(
        &[
            "clone",
            source.to_str().expect("source utf8"),
            clone.to_str().expect("clone utf8"),
        ],
        temp.path(),
    );
    assert!(
        clone_text.contains("current thread") && clone_text.contains("main"),
        "clone text must name the checked-out thread:\n{clone_text}"
    );

    let clone_json = json_cmd(
        &[
            "--output",
            "json",
            "clone",
            source.to_str().expect("source utf8"),
            temp.path().join("clone-json").to_str().expect("utf8"),
        ],
        temp.path(),
    );
    assert_eq!(clone_json["output_kind"], "clone");
    assert_eq!(clone_json["success"], true);
    assert_eq!(clone_json["branch"], "main");

    assert_checked_out_thread(&clone, "main");
    assert_eq!(
        std::fs::read_to_string(clone.join("README.md")).expect("cloned README"),
        "captured\n"
    );
}

#[test]
fn clone_native_honors_thread_flag() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.path().join("source");
    let clone = temp.path().join("clone");
    std::fs::create_dir_all(&source).expect("source dir");
    seed_native_source(&source, "captured\n", None);
    heddle(&["thread", "create", "feature"], &source);

    let output = heddle_output(
        &[
            "clone",
            "--thread",
            "feature",
            source.to_str().expect("source utf8"),
            clone.to_str().expect("clone utf8"),
        ],
        temp.path(),
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "clone --thread feature must succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_checked_out_thread(&clone, "feature");
}

#[test]
fn clone_native_refuses_unknown_thread_before_destination() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.path().join("source");
    let clone = temp.path().join("clone");
    std::fs::create_dir_all(&source).expect("source dir");
    seed_native_source(&source, "captured\n", None);

    let output = heddle_output(
        &[
            "--output",
            "json",
            "clone",
            "--thread",
            "missing",
            source.to_str().expect("source utf8"),
            clone.to_str().expect("clone utf8"),
        ],
        temp.path(),
    );
    assert!(
        !output.status.success(),
        "unknown --thread must fail closed"
    );
    assert!(
        !clone.exists(),
        "unknown --thread must not create the destination"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value = serde_json::from_str(&stderr).expect("JSON refusal");
    assert_eq!(envelope["kind"], "clone_remote_thread_not_found");
}

#[test]
fn clone_native_does_not_copy_source_dirt() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.path().join("source");
    let clone = temp.path().join("clone");
    std::fs::create_dir_all(&source).expect("source dir");
    seed_native_source(
        &source,
        "captured\n",
        Some("uncommitted field-study notes\n"),
    );

    let output = heddle_output(
        &[
            "clone",
            source.to_str().expect("source utf8"),
            clone.to_str().expect("clone utf8"),
        ],
        temp.path(),
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "dirty-source clone must still succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        clone.join("README.md").is_file(),
        "saved history must be materialized"
    );
    assert!(
        !clone.join("NOTES.md").exists(),
        "uncommitted NOTES.md must not be contagious"
    );

    let status = json_cmd(&["--output", "json", "status"], &clone);
    assert_eq!(
        status["thread"], "main",
        "dirty-source clone must still attach: {status}"
    );
    let changed = status["changed_paths"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        changed.is_empty(),
        "clone status must be clean; got changed_paths {changed:?} in {status}"
    );
    let added = status["changes"]["added"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let modified = status["changes"]["modified"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        added.is_empty() && modified.is_empty(),
        "clone must not report uncaptured source dirt: {status}"
    );
}
