// SPDX-License-Identifier: Apache-2.0
//! `heddle <command> --schema` runtime schema introspection.
//!
//! `--schema` is a global short-circuit (like `--help`): it prints the JSON
//! Schema for the resolved command's `--output json` payload and exits 0
//! without running the command. It resolves to the deepest selected verb,
//! including flag-differentiated payloads (`land --threads`). A command with
//! no `--output json` payload fails with a clear typed error rather than a
//! panic.

use serde_json::Value;
use tempfile::TempDir;

use super::{heddle_output, heddle_schema};

/// The spawned `--schema` output for `args` must be the exact registry schema
/// for `verb`, and the process must exit 0.
fn assert_schema_matches(args: &[&str], verb: &str) {
    let temp = TempDir::new().expect("tempdir");
    let output = heddle_output(args, Some(temp.path())).expect("spawn heddle --schema");
    assert!(
        output.status.success(),
        "`heddle {}` must exit 0; stderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let printed: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
        panic!(
            "`heddle {}` stdout is not JSON: {err}: {stdout}",
            args.join(" ")
        )
    });
    assert_eq!(
        printed,
        heddle_schema(verb),
        "`heddle {}` must print the registry schema for `{verb}`",
        args.join(" "),
    );
}

#[test]
fn schema_flag_prints_registry_schema_for_several_verbs() {
    assert_schema_matches(&["init", "--schema"], "init");
    assert_schema_matches(&["land", "--schema"], "land");
    assert_schema_matches(&["log", "--schema"], "log");
    assert_schema_matches(&["status", "--schema"], "status");
    assert_schema_matches(&["push", "--schema"], "push");
}

#[test]
fn schema_flag_resolves_flag_differentiated_verbs() {
    // A folded flag selects a distinct `--output json` payload, so `--schema`
    // must print that variant's schema, not the base verb's.
    assert_schema_matches(
        &["land", "--threads", "alpha,beta", "--schema"],
        "land --threads",
    );
    assert_schema_matches(&["log", "--reflog", "--schema"], "log --reflog");
    assert_schema_matches(&["log", "--timeline", "--schema"], "log --timeline");
    assert_schema_matches(&["undo", "--list", "--schema"], "undo --list");

    // The base verb and its flagged variant register different schemas.
    assert_ne!(
        heddle_schema("land"),
        heddle_schema("land --threads"),
        "the folded `land --threads` payload should differ from base `land`",
    );
}

#[test]
fn schema_flag_resolves_nested_subcommands() {
    assert_schema_matches(&["thread", "show", "--schema"], "thread show");
    assert_schema_matches(&["discuss", "show", "--schema"], "discuss show");
    assert_schema_matches(&["remote", "list", "--schema"], "remote list");
}

#[test]
fn schema_flag_errors_cleanly_for_non_json_command() {
    // `completions` emits shell text, not `--output json`; `--schema` must
    // fail with a clear typed error (not a panic) and a non-zero exit.
    let temp = TempDir::new().expect("tempdir");
    let output =
        heddle_output(&["completions", "bash", "--schema"], Some(temp.path())).expect("spawn");
    assert!(
        !output.status.success(),
        "`heddle completions bash --schema` must exit non-zero",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no JSON schema for"),
        "error must name the missing schema; stderr: {stderr}",
    );
    assert!(
        !stderr.to_lowercase().contains("panic"),
        "error must be a typed failure, not a panic; stderr: {stderr}",
    );
}

#[test]
fn schema_flag_does_not_execute_the_command() {
    // `heddle init --schema` in an empty dir must print the init schema and
    // NOT initialize a repository — the short-circuit fires before the body.
    let temp = TempDir::new().expect("tempdir");
    let output = heddle_output(&["init", "--schema"], Some(temp.path())).expect("spawn");
    assert!(output.status.success(), "init --schema must exit 0");

    let printed: Value = serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim())
        .expect("init --schema stdout is JSON");
    assert_eq!(printed, heddle_schema("init"));

    assert!(
        !temp.path().join(".heddle").exists(),
        "`--schema` must not create a repository (no side effects)",
    );
}
