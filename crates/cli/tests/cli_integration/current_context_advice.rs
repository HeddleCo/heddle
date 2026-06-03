// SPDX-License-Identifier: Apache-2.0
//! JSON error envelopes for commands that need current thread/session context.

use std::{fs, str};

use repo::Repository;
use serde_json::Value;
use tempfile::TempDir;

use super::{assert_json_recovery_advice_fields, heddle, heddle_argv_json, heddle_output};

fn setup_detached_repo_without_current_thread() -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "init"], Some(temp.path())).unwrap();

    let repo = Repository::open(temp.path()).unwrap();
    let head = repo
        .head()
        .unwrap()
        .expect("repo should have a current state after capture");
    fs::write(
        temp.path().join(".heddle").join("HEAD"),
        format!("{}\n", head.to_string_full()),
    )
    .unwrap();
    drop(repo);

    temp
}

fn json_failure(args: &[&str], cwd: &std::path::Path) -> Value {
    let output = heddle_output(args, Some(cwd)).expect("invoke JSON failure");
    assert!(
        !output.status.success(),
        "command should fail for args {args:?}"
    );
    let stdout = str::from_utf8(&output.stdout).expect("stdout should be utf8");
    assert!(
        stdout.trim().is_empty(),
        "JSON failures should not emit success-shaped stdout: {stdout}"
    );
    let stderr = str::from_utf8(&output.stderr).expect("stderr should be utf8");
    let envelope: Value = serde_json::from_str(stderr.trim()).expect("stderr should be JSON");
    assert_json_recovery_advice_fields(&envelope, stderr);
    envelope
}

#[test]
fn ready_without_current_thread_uses_typed_advice() {
    let temp = setup_detached_repo_without_current_thread();

    let envelope = json_failure(&["--output", "json", "ready"], temp.path());
    assert_eq!(envelope["kind"], "no_current_thread");
    assert_eq!(envelope["primary_command"], "heddle ready --thread <name>");
    assert_eq!(envelope["primary_command_argv"], Value::Null);
    assert_action_template(
        &envelope["primary_command_template"],
        "heddle ready --thread <name>",
        heddle_argv_json(["ready", "--thread", "<thread>"]),
        &["thread"],
        true,
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("--thread")),
        "ready advice should name the explicit selector: {envelope}"
    );
}

#[test]
fn ship_without_current_thread_uses_typed_advice() {
    let temp = setup_detached_repo_without_current_thread();

    let envelope = json_failure(&["--output", "json", "land"], temp.path());
    assert_eq!(envelope["kind"], "no_current_thread");
    assert_eq!(envelope["primary_command"], "heddle land --thread <name>");
    assert_eq!(envelope["primary_command_argv"], Value::Null);
    assert_action_template(
        &envelope["primary_command_template"],
        "heddle land --thread <name>",
        heddle_argv_json(["land", "--thread", "<thread>"]),
        &["thread"],
        true,
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("--thread")),
        "land advice should name the explicit selector: {envelope}"
    );
}

#[test]
fn delegate_without_attached_parent_thread_uses_typed_advice() {
    let temp = setup_detached_repo_without_current_thread();

    let envelope = json_failure(&["--output", "json", "delegate", "probe"], temp.path());
    assert_eq!(envelope["kind"], "no_attached_parent_thread");
    assert_eq!(
        envelope["primary_command"],
        "heddle delegate --parent <THREAD> <task>"
    );
    assert_eq!(envelope["primary_command_argv"], Value::Null);
    assert_action_template(
        &envelope["primary_command_template"],
        "heddle delegate --parent <THREAD> <task>",
        heddle_argv_json(["delegate", "--parent", "<thread>", "<task>"]),
        &["thread", "task"],
        false,
    );
    assert!(
        envelope["recovery_action_templates"]
            .as_array()
            .is_some_and(|templates| templates
                .iter()
                .any(|template| template["action"] == "heddle delegate --parent <THREAD> <task>")),
        "delegate recovery commands should carry template metadata: {envelope}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("--parent")),
        "delegate advice should name the explicit parent selector: {envelope}"
    );
}

fn assert_action_template(
    template: &Value,
    action: &str,
    argv_template: Value,
    required_inputs: &[&str],
    agent_may_fill: bool,
) {
    assert_eq!(template["action"], action);
    assert_eq!(template["argv_template"], argv_template);
    assert_eq!(
        template["required_inputs"],
        serde_json::json!(required_inputs)
    );
    assert_eq!(template["agent_may_fill"], agent_may_fill);
}

#[test]
fn session_show_without_active_session_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let envelope = json_failure(&["--output", "json", "session", "show"], temp.path());
    assert_eq!(envelope["kind"], "no_current_session");
    assert_eq!(envelope["primary_command"], "heddle session start");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle session start")),
        "session advice should point at session start: {envelope}"
    );
}

#[test]
fn session_segment_without_active_session_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let envelope = json_failure(
        &[
            "--output",
            "json",
            "session",
            "segment",
            "--provider",
            "codex",
            "--model",
            "test-model",
        ],
        temp.path(),
    );
    assert_eq!(envelope["kind"], "no_current_session");
    assert_eq!(envelope["primary_command"], "heddle session start");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error == "No active session"),
        "session segment should share the concise no-session error: {envelope}"
    );
}
