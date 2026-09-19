// SPDX-License-Identifier: Apache-2.0
//! Public repository-import surface and output contracts.

use std::{fs, path::Path, process::Command};

fn run(cwd: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_heddle"))
        .current_dir(cwd)
        .env("HEDDLE_HOME", home)
        .args(args)
        .output()
        .expect("run heddle")
}

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn import_local_json_is_one_final_document_and_creates_native_history() {
    let temp = tempfile::tempdir().expect("temp root");
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    fs::create_dir_all(&repo).expect("repo dir");
    fs::create_dir_all(&home).expect("home dir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Import Contract"]);
    git(&repo, &["config", "user.email", "import@example.test"]);
    fs::write(repo.join("hello.txt"), b"hello\n").expect("fixture file");
    git(&repo, &["add", "hello.txt"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let output = run(&repo, &home, &["import", "local", "--output", "json"]);
    assert!(
        output.status.success(),
        "import local failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let documents = serde_json::Deserializer::from_slice(&output.stdout)
        .into_iter::<serde_json::Value>()
        .collect::<Result<Vec<_>, _>>()
        .expect("valid JSON documents");
    assert_eq!(documents.len(), 1, "finite command must emit one document");
    let result = &documents[0];
    assert_eq!(result["output_kind"], "import_local");
    assert_eq!(result["status"], "completed");
    assert_eq!(result["commits_imported"], 1);
    assert_eq!(result["verification"]["repository_mode"], "native-heddle");
    assert!(repo.join(".heddle").is_dir());
}

#[test]
fn import_url_parse_error_points_to_nearest_help() {
    let temp = tempfile::tempdir().expect("temp root");
    let output = run(
        temp.path(),
        temp.path(),
        &["--output", "json", "import", "url"],
    );
    assert_eq!(output.status.code(), Some(64));
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("JSON parse-error envelope");
    assert_eq!(error["primary_command"], "heddle import url --help");
    assert_eq!(error["recovery_commands"][0], "heddle import url --help");
}

#[test]
fn malformed_line_parse_error_points_to_nearest_help() {
    let temp = tempfile::tempdir().expect("temp root");
    let output = run(
        temp.path(),
        temp.path(),
        &["--output", "json", "context", "set", "--line", "nope"],
    );
    assert_eq!(output.status.code(), Some(64));
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("JSON parse-error envelope");
    assert_eq!(error["primary_command"], "heddle context set --help");
    assert_eq!(error["recovery_commands"][0], "heddle context set --help");
}

#[test]
fn retired_import_surfaces_are_not_registered() {
    let temp = tempfile::tempdir().expect("temp root");
    for args in [
        vec!["adopt", "--help"],
        vec!["remote", "import-source", "--help"],
    ] {
        let output = run(temp.path(), temp.path(), &args);
        assert_eq!(output.status.code(), Some(64), "retired surface {args:?}");
    }
}
