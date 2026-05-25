// SPDX-License-Identifier: Apache-2.0
//! `heddle doctor docs` integration tests.
//!
//! Exercise the verb end-to-end against tiny markdown fixtures with
//! known drift, asserting the JSON shape lines up with the public
//! contract documented in `crates/cli/src/cli/commands/doctor_docs.rs`.

use std::fs;

use serde_json::Value;
use tempfile::TempDir;

use super::*;

fn write_file(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).expect("write fixture markdown");
    path
}

fn doctor_docs_json_failure(output: &std::process::Output) -> Value {
    assert!(
        !output.status.success(),
        "expected non-zero exit on docs drift"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode docs drift should emit exactly one JSON envelope on stderr, not a stdout report: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value = serde_json::from_str(&stderr)
        .unwrap_or_else(|err| panic!("parse doctor docs JSON failure envelope: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "machine_contract_drift");
    assert_eq!(envelope["output_kind"], "doctor_docs");
    assert_eq!(envelope["status"], "drift");
    assert_eq!(envelope["verified"], false);
    envelope
}

#[test]
fn flags_invalid_workspace_value() {
    let temp = TempDir::new().expect("tempdir");
    let md = write_file(
        temp.path(),
        "drift.md",
        "Run `heddle start probe --workspace ephemeral` to see drift.\n",
    );

    let output = heddle_output(
        &[
            "doctor",
            "docs",
            "--path",
            md.to_str().unwrap(),
            "--output",
            "json",
        ],
        Some(temp.path()),
    )
    .expect("invoke doctor docs");

    let report = doctor_docs_json_failure(&output);
    assert_eq!(report["output_kind"], "doctor_docs");
    assert_eq!(report["status"], "drift");
    assert_eq!(report["verified"], false);
    assert_eq!(
        report["recommended_action_argv"],
        heddle_argv_json(["doctor", "docs", "--all", "--output", "json"])
    );
    assert_eq!(report["files_scanned"], 1);
    let issues = report["issues"].as_array().expect("issues array");
    assert!(!issues.is_empty(), "expected at least one issue");
    let kinds: Vec<&str> = issues.iter().filter_map(|i| i["kind"].as_str()).collect();
    assert!(
        kinds.contains(&"invalid_flag_value"),
        "expected invalid_flag_value, got: {:?}",
        kinds
    );
}

#[test]
fn flags_unknown_verb_and_subverb() {
    let temp = TempDir::new().expect("tempdir");
    let md = write_file(
        temp.path(),
        "drift.md",
        "First, `heddle frobnicate --foo` is bogus.\n\
         Second, `heddle thread bogus-action` is also bogus.\n",
    );

    let output = heddle_output(
        &[
            "doctor",
            "docs",
            "--path",
            md.to_str().unwrap(),
            "--output",
            "json",
        ],
        Some(temp.path()),
    )
    .expect("invoke doctor docs");
    let report = doctor_docs_json_failure(&output);
    let issues = report["issues"].as_array().expect("issues array");
    let kinds: Vec<&str> = issues.iter().filter_map(|i| i["kind"].as_str()).collect();
    assert!(
        kinds.contains(&"unknown_verb"),
        "expected unknown_verb, got: {:?}",
        kinds
    );
    assert!(
        kinds.contains(&"unknown_subverb"),
        "expected unknown_subverb, got: {:?}",
        kinds
    );
}

#[test]
fn clean_markdown_exits_zero() {
    let temp = TempDir::new().expect("tempdir");
    let md = write_file(
        temp.path(),
        "ok.md",
        "Use `heddle start probe --workspace materialized --path ./checkout` for isolation.\n\
         For status, run `heddle status --output json`.\n\
         Clean up with `heddle thread drop probe --delete-thread`.\n",
    );

    let output = heddle_output(
        &[
            "doctor",
            "docs",
            "--path",
            md.to_str().unwrap(),
            "--output",
            "json",
        ],
        Some(temp.path()),
    )
    .expect("invoke doctor docs");
    assert!(
        output.status.success(),
        "expected zero exit on clean markdown; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: Value = serde_json::from_str(&stdout).expect("parse JSON report");
    assert_eq!(report["output_kind"], "doctor_docs");
    assert_eq!(report["status"], "clean");
    assert_eq!(report["verified"], true);
    assert_eq!(report["recommended_action"], serde_json::Value::Null);
    assert_eq!(report["files_scanned"], 1);
    assert_eq!(
        report["issues"].as_array().expect("issues").len(),
        0,
        "unexpected issues: {}",
        stdout
    );
}

#[test]
fn accepts_catalog_global_options_and_non_finite_scope_values() {
    let temp = TempDir::new().expect("tempdir");
    let md = write_file(
        temp.path(),
        "ok.md",
        "Inspect `heddle status --output json --repo .`.\n\
         Add context with `heddle context set --path src/lib.rs --scope symbol:foo --kind rationale -m note`.\n\
         Install with `heddle integration install codex --harness-install-scope repo`.\n",
    );

    let output = heddle_output(
        &[
            "doctor",
            "docs",
            "--path",
            md.to_str().unwrap(),
            "--output",
            "json",
        ],
        Some(temp.path()),
    )
    .expect("invoke doctor docs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "expected zero exit on catalog-backed globals and non-finite scope; stdout={} stderr={}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_str(&stdout).expect("parse JSON report");
    assert_eq!(report["issues"].as_array().expect("issues").len(), 0);
}

#[test]
fn flags_invalid_catalog_finite_value() {
    let temp = TempDir::new().expect("tempdir");
    let md = write_file(
        temp.path(),
        "drift.md",
        "Use `heddle context set --path src/lib.rs --kind warning -m note`.\n",
    );

    let output = heddle_output(
        &[
            "doctor",
            "docs",
            "--path",
            md.to_str().unwrap(),
            "--output",
            "json",
        ],
        Some(temp.path()),
    )
    .expect("invoke doctor docs");
    let report = doctor_docs_json_failure(&output);
    let issues = report["issues"].as_array().expect("issues array");
    assert!(
        issues.iter().any(
            |issue| issue["kind"] == Value::String("invalid_flag_value".to_string())
                && issue["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.contains("--kind warning"))
        ),
        "expected invalid finite --kind value; got: {report}"
    );
}

#[test]
fn human_output_renders_when_no_json() {
    let temp = TempDir::new().expect("tempdir");
    let md = write_file(
        temp.path(),
        "drift.md",
        "Run `heddle start probe --workspace ephemeral`.\n",
    );

    let output = heddle_output(
        &[
            "doctor",
            "docs",
            "--path",
            md.to_str().unwrap(),
            "--output",
            "text",
        ],
        Some(temp.path()),
    )
    .expect("invoke doctor docs");
    assert!(!output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("doctor docs:") && stdout.contains("--workspace ephemeral"),
        "expected human-readable summary; got: {}",
        stdout
    );
}

/// Regression: `--all` previously shelled out to `git ls-files` to
/// enumerate markdown, so it failed in native-heddle repos (no
/// `.git/`) and on hosts without git installed. The native walk
/// must enumerate `.md` files rooted at the repo root and skip the
/// usual ignored prefixes (`.heddle/`, `target/`, `node_modules/`,
/// etc.) without touching `git` at all.
///
/// We construct a tempdir that has NO `.git/` and NO `.heddle/` as
/// the repo root marker — `--repo` lets us pass it explicitly so
/// `find_repo_root` is bypassed. The test asserts `--all` exits
/// cleanly (no drift in the synthetic markdown) and that
/// `files_scanned` reflects the markdown actually present.
#[test]
fn all_enumerates_markdown_without_git() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    // Two markdown files at different depths, plus one file inside
    // an ignored prefix that must NOT be scanned.
    write_file(
        root,
        "README.md",
        "Run `heddle start probe --workspace materialized --path foo`.\n",
    );
    fs::create_dir_all(root.join("docs")).unwrap();
    write_file(
        &root.join("docs"),
        "guide.md",
        "Use `heddle context set --path X --scope file --kind rationale -m \"y\"`.\n",
    );
    fs::create_dir_all(root.join("target/doc")).unwrap();
    write_file(
        &root.join("target/doc"),
        "vendored.md",
        "This file lives under target/ and must be skipped.\n",
    );
    // Confirm there's no .git/ — that's the whole point of this test.
    assert!(!root.join(".git").exists());
    assert!(!root.join(".heddle").exists());

    let output = heddle_output(
        &[
            "--repo",
            root.to_str().unwrap(),
            "doctor",
            "docs",
            "--all",
            "--output",
            "json",
        ],
        Some(root),
    )
    .expect("invoke doctor docs --all");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "expected zero exit (no drift); stdout={} stderr={}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_str(&stdout).expect("parse JSON report");
    assert_eq!(
        report["files_scanned"], 2,
        "expected README.md + docs/guide.md (target/ skipped); got: {stdout}"
    );
    assert_eq!(
        report["issues"].as_array().expect("issues").len(),
        0,
        "synthetic markdown is clean; got: {stdout}"
    );
}

/// Regression: an unreadable `--path` (typo, missing file, permission
/// error) used to be silently skipped — `eprintln` to stderr, no
/// addition to the issue list. The command then exited 0 because
/// `report.issues` was empty, so a CI invocation against a renamed
/// or vanished file passed without scanning anything.
///
/// The fix surfaces unreadable files as `kind: "unreadable"` issues.
/// The existing exit-non-zero-on-issues path then turns the noise
/// into a real failure signal.
#[test]
fn flags_unreadable_path_as_hard_failure() {
    let temp = TempDir::new().expect("tempdir");
    let missing = temp.path().join("does-not-exist.md");
    assert!(!missing.exists());

    let output = heddle_output(
        &[
            "doctor",
            "docs",
            "--path",
            missing.to_str().unwrap(),
            "--output",
            "json",
        ],
        Some(temp.path()),
    )
    .expect("invoke doctor docs");

    assert!(
        !output.status.success(),
        "expected non-zero exit on unreadable --path; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report = doctor_docs_json_failure(&output);
    let issues = report["issues"].as_array().expect("issues array");
    assert_eq!(
        issues.len(),
        1,
        "unreadable path should produce exactly one issue; got: {report}"
    );
    assert_eq!(
        issues[0]["kind"],
        Value::String("unreadable".to_string()),
        "issue kind should be 'unreadable'; got: {report}"
    );
    let detail = issues[0]["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("does-not-exist.md"),
        "detail should name the missing file; got: {detail}"
    );
}

#[test]
fn skips_feature_gated_presence_verb() {
    // `presence` lives behind the `client` feature; default
    // `cargo install --path crates/cli` builds don't see it. The
    // checker should NOT false-positive on docs that mention it.
    let temp = TempDir::new().expect("tempdir");
    let md = write_file(
        temp.path(),
        "presence.md",
        "Run `heddle presence publish --session abc-123` (client feature only).\n",
    );

    let output = heddle_output(
        &[
            "doctor",
            "docs",
            "--path",
            md.to_str().unwrap(),
            "--output",
            "json",
        ],
        Some(temp.path()),
    )
    .expect("invoke doctor docs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: Value = serde_json::from_str(&stdout).expect("parse JSON report");
    assert_eq!(
        report["issues"].as_array().expect("issues").len(),
        0,
        "presence verb should be allowlisted as feature-gated; got: {}",
        stdout
    );
    assert!(
        output.status.success(),
        "expected clean exit when only finding feature-gated verbs"
    );
}
