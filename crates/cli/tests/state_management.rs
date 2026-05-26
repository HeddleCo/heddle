// SPDX-License-Identifier: Apache-2.0
//! Integration tests for state-management commands: clean, revert, stash, merge.

use std::{
    fs,
    process::{Command, Output},
    str,
};

use serde_json::Value;
use tempfile::TempDir;

#[path = "state_management/clean.rs"]
mod clean;
#[path = "state_management/merge.rs"]
mod merge;
#[path = "state_management/merge_store_integrity.rs"]
mod merge_store_integrity;
#[path = "state_management/missing_tree_integrity.rs"]
mod missing_tree_integrity;
#[path = "state_management/revert.rs"]
mod revert;
#[path = "state_management/stash.rs"]
mod stash;

fn translate_legacy_args(args: &[&str]) -> Vec<String> {
    let mut prefix = Vec::new();
    let mut i = 0;
    while i < args.len() && args[i].starts_with("--") {
        prefix.push(args[i].to_string());
        i += 1;
    }
    let rest = &args[i..];
    let translated = match rest {
        ["thread", "delete", name] => vec![
            "thread".into(),
            "drop".into(),
            (*name).into(),
            "--delete-thread".into(),
        ],
        _ => rest.iter().map(|arg| (*arg).to_string()).collect(),
    };
    prefix.extend(translated);
    prefix
}

fn heddle(args: &[&str], cwd: Option<&std::path::Path>) -> Result<String, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args(translate_legacy_args(args));
    cmd.env("HEDDLE_PRINCIPAL_NAME", "Heddle Test")
        .env("HEDDLE_PRINCIPAL_EMAIL", "test@heddle.dev");

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let output = cmd.output().map_err(|e| e.to_string())?;
    let stdout = str::from_utf8(&output.stdout).unwrap_or("").to_string();
    let stderr = str::from_utf8(&output.stderr).unwrap_or("").to_string();

    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "Exit code: {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            stdout,
            stderr
        ))
    }
}

fn heddle_output(args: &[&str], cwd: Option<&std::path::Path>) -> Result<Output, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args(translate_legacy_args(args));

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    cmd.output().map_err(|e| e.to_string())
}

fn status_json(path: &std::path::Path) -> Value {
    let output = heddle(&["status", "--output", "json"], Some(path)).unwrap();
    serde_json::from_str(&output).expect("status output should be JSON")
}

pub(crate) fn assert_json_recovery_advice_fields(envelope: &Value, context: &str) {
    for field in [
        "unsafe_condition",
        "would_change",
        "preserved",
        "primary_command",
        "recovery_commands",
        "hint",
    ] {
        assert!(
            envelope[field]
                .as_str()
                .is_some_and(|value| !value.trim().is_empty())
                || envelope[field]
                    .as_array()
                    .is_some_and(|value| !value.is_empty()),
            "JSON recovery advice should expose `{field}` through structured fields: {context}"
        );
    }
    assert!(
        envelope["error"].as_str().is_some_and(|error| {
            !error.contains("Unsafe:")
                && !error.contains("Would change:")
                && !error.contains("Preserved:")
                && !error.contains("Primary recovery:")
                && !error.contains("Other recovery:")
        }),
        "JSON `error` should stay concise; recovery detail belongs in structured fields: {context}"
    );
    assert!(
        envelope.get("primary_command_argv").is_some_and(|argv| {
            argv.is_null() || argv.as_array().is_some_and(|parts| !parts.is_empty())
        }),
        "JSON recovery advice should expose `primary_command_argv` as argv array or null: {context}"
    );
    assert!(
        envelope
            .get("primary_command_template")
            .is_some_and(|template| template.is_null() || template.is_object()),
        "JSON recovery advice should expose `primary_command_template` as object or null: {context}"
    );
    assert!(
        envelope["recovery_command_argv"]
            .as_array()
            .is_some_and(|commands| commands.iter().all(|command| command.is_array())),
        "JSON recovery advice should expose `recovery_command_argv` as an array of argv arrays: {context}"
    );
    assert!(
        envelope["recovery_action_templates"]
            .as_array()
            .is_some_and(|templates| templates.iter().all(|template| template.is_object())),
        "JSON recovery advice should expose `recovery_action_templates` as an array of template objects: {context}"
    );
}

fn setup_repo_with_file(temp: &TempDir, filename: &str, content: &str) {
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join(filename), content).unwrap();
    heddle(&["capture", "-m", "initial"], Some(temp.path())).unwrap();
}

fn assert_file_exists(path: impl AsRef<std::path::Path>, msg: &str) {
    let path = path.as_ref();
    assert!(path.exists(), "{}: {:?}", msg, path);
}

fn assert_file_not_exists(path: impl AsRef<std::path::Path>, msg: &str) {
    let path = path.as_ref();
    assert!(!path.exists(), "{}: {:?}", msg, path);
}

fn current_head_json(path: &std::path::Path) -> Value {
    serde_json::from_str(&heddle(&["--output", "json", "show", "HEAD"], Some(path)).unwrap())
        .expect("show HEAD should return JSON")
}

#[test]
fn capture_without_message_refuses_and_preserves_head() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "initial");
    let before = current_head_json(temp.path());

    fs::write(temp.path().join("file.txt"), "changed").unwrap();
    let output = heddle_output(&["--output", "text", "capture"], Some(temp.path())).unwrap();

    assert!(!output.status.success(), "capture without -m must fail");
    assert!(
        str::from_utf8(&output.stderr)
            .unwrap_or("")
            .contains("Next: heddle capture -m \"...\""),
        "text refusal should include the direct next command: {}",
        str::from_utf8(&output.stderr).unwrap_or("")
    );
    assert_eq!(current_head_json(temp.path()), before);
}

#[test]
fn capture_without_message_json_refusal_is_structured_and_preserves_head() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "initial");
    let before = current_head_json(temp.path());

    fs::write(temp.path().join("file.txt"), "changed").unwrap();
    let output = heddle_output(&["--output", "json", "capture"], Some(temp.path())).unwrap();

    assert!(!output.status.success(), "capture without -m must fail");
    assert!(
        output.stdout.is_empty(),
        "failed JSON command should not emit stdout"
    );
    let envelope: Value =
        serde_json::from_slice(&output.stderr).expect("stderr should be a JSON envelope");
    assert_eq!(envelope["kind"], "missing_capture_intent");
    assert_eq!(envelope["primary_command"], "heddle capture -m \"...\"");
    assert_json_recovery_advice_fields(&envelope, "capture without message");
    assert_eq!(current_head_json(temp.path()), before);
}

#[test]
fn commit_without_message_refuses_and_preserves_head() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "initial");
    let before = current_head_json(temp.path());

    fs::write(temp.path().join("file.txt"), "changed").unwrap();
    let output = heddle_output(&["--output", "text", "commit"], Some(temp.path())).unwrap();

    assert!(!output.status.success(), "commit without -m must fail");
    assert!(
        str::from_utf8(&output.stderr)
            .unwrap_or("")
            .contains("Next: heddle commit -m \"...\""),
        "text refusal should include the direct next command: {}",
        str::from_utf8(&output.stderr).unwrap_or("")
    );
    assert_eq!(current_head_json(temp.path()), before);
}
