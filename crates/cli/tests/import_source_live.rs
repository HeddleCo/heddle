// SPDX-License-Identifier: Apache-2.0
//! Live weft proof for server-side public Git import.
//!
//! The runner supplies an authenticated v2 weft, a public source URL, and a
//! checkout of that source at HEAD. The test requires durable RUNNING updates,
//! clones the resulting native spool with a second fresh HEDDLE_HOME, and
//! compares every source file byte-for-byte.

use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn required(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the live import test"))
}

fn unique_destination(suffix: &str) -> String {
    let prefix = required("HEDDLE_IMPORT_SOURCE_E2E_DESTINATION_PREFIX");
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("current time")
        .as_secs();
    format!(
        "{}/import-source-{suffix}-{}-{epoch}",
        prefix.trim_end_matches('/'),
        std::process::id()
    )
}

fn hosted_url(server: &str, destination: &str) -> String {
    let server = server.trim_end_matches('/');
    if server.starts_with("https://") {
        format!("{server}/{destination}")
    } else {
        format!("https://{server}/{destination}")
    }
}

fn source_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(|entry| entry.expect("walk source tree"))
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            let relative = entry.path().strip_prefix(root).expect("relative path");
            let first = relative.components().next()?.as_os_str();
            if first == ".git" || first == ".heddle" {
                return None;
            }
            Some((
                relative.to_path_buf(),
                fs::read(entry.path()).expect("read source file"),
            ))
        })
        .collect()
}

#[test]
#[ignore = "requires authenticated live v2 weft plus HEDDLE_IMPORT_SOURCE_E2E_* inputs"]
fn public_git_import_streams_progress_and_clones_exact_head() {
    let server = required("HEDDLE_IMPORT_SOURCE_E2E_SERVER");
    let source_url = required("HEDDLE_IMPORT_SOURCE_E2E_SOURCE_URL");
    let source_dir = PathBuf::from(required("HEDDLE_IMPORT_SOURCE_E2E_SOURCE_DIR"));
    let destination = unique_destination("positive");
    let import_home = tempfile::tempdir().expect("fresh import HEDDLE_HOME");
    let operation_id = uuid::Uuid::new_v4().to_string();
    let output = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args([
            "import",
            "url",
            &source_url,
            "--to",
            &destination,
            "--server",
            &server,
            "--output",
            "json",
            "--op-id",
            &operation_id,
        ])
        .env("HEDDLE_HOME", import_home.path())
        .output()
        .expect("run hosted source import");
    assert!(
        output.status.success(),
        "source import failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("finite import URL JSON result");
    assert!(
        record["state"] == "completed"
            && record["terminal"] == true
            && record["operation_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
            && record["results"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty())
    );

    let clone_root = tempfile::tempdir().expect("clone root");
    let clone_home = tempfile::tempdir().expect("fresh clone HEDDLE_HOME");
    let clone_dir = clone_root.path().join("round-trip");
    let clone = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args([
            "clone",
            &hosted_url(&server, &destination),
            clone_dir.to_str().expect("clone path UTF-8"),
        ])
        .env("HEDDLE_HOME", clone_home.path())
        .output()
        .expect("clone imported hosted spool");
    assert!(
        clone.status.success(),
        "hosted clone failed:\n{}",
        String::from_utf8_lossy(&clone.stderr)
    );
    assert_eq!(source_files(&source_dir), source_files(&clone_dir));
}

#[test]
#[ignore = "requires authenticated live v2 weft plus HEDDLE_IMPORT_SOURCE_E2E_* inputs"]
fn unreachable_public_git_source_is_a_failed_operation() {
    let server = required("HEDDLE_IMPORT_SOURCE_E2E_SERVER");
    let destination = unique_destination("negative");
    let home = tempfile::tempdir().expect("fresh negative HEDDLE_HOME");
    let operation_id = uuid::Uuid::new_v4().to_string();
    let output = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args([
            "import",
            "url",
            "https://github.com/octocat/heddle-import-source-missing.git",
            "--to",
            &destination,
            "--server",
            &server,
            "--op-id",
            &operation_id,
        ])
        .env("HEDDLE_HOME", home.path())
        .output()
        .expect("run failing hosted source import");
    assert!(!output.status.success(), "failed import reported success");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("hosted source import failed"), "{stderr}");
    assert!(stderr.contains("RetryImportSource"), "{stderr}");
}
