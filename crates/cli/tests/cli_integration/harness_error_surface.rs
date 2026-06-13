// SPDX-License-Identifier: Apache-2.0

use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

use tempfile::TempDir;

use super::heddle;

fn run_relay(cwd: &Path, config_path: &Path, payload: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(["integration", "relay", "codex", "agent_done"])
        .current_dir(cwd)
        .env("HEDDLE_CONFIG", config_path)
        .env_remove("NO_COLOR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn heddle relay");
    child
        .stdin
        .take()
        .expect("relay stdin")
        .write_all(payload.as_bytes())
        .expect("write relay payload");
    child.wait_with_output().expect("wait for heddle relay")
}

#[test]
fn harness_relay_warns_on_malformed_user_config_and_continues() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let config = temp.path().join("bad-config.toml");
    std::fs::write(&config, "[harness\ntransport = \"direct\"\n").unwrap();

    let output = run_relay(temp.path(), &config, r#"{"message":"hello"}"#);

    assert!(
        output.status.success(),
        "relay should continue with defaults; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("warning: failed to load user config"),
        "malformed config warning should be visible: {stderr}"
    );
    assert!(
        stderr.contains(&config.display().to_string()),
        "warning should name the config path: {stderr}"
    );
}

#[test]
fn harness_relay_missing_user_config_defaults_without_warning() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let missing = temp.path().join("missing-config.toml");

    let output = run_relay(temp.path(), &missing, r#"{"message":"hello"}"#);

    assert!(
        output.status.success(),
        "relay should continue with defaults; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("failed to load user config"),
        "missing config should not warn: {stderr}"
    );
}

#[test]
fn harness_relay_valid_user_config_loads_without_warning() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let config = temp.path().join("config.toml");
    std::fs::write(&config, "[harness]\ntransport = \"spool\"\n").unwrap();

    let output = run_relay(temp.path(), &config, r#"{"message":"hello"}"#);

    assert!(
        output.status.success(),
        "relay should load valid config; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("failed to load user config"),
        "valid config should not warn: {stderr}"
    );
}

#[test]
fn harness_relay_warns_on_invalid_payload_json_and_continues() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let config = temp.path().join("config.toml");
    std::fs::write(&config, "[harness]\ntransport = \"spool\"\n").unwrap();

    let output = run_relay(temp.path(), &config, "{not-json");

    assert!(
        output.status.success(),
        "relay should continue with null payload; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("warning: failed to parse harness relay payload as JSON"),
        "invalid JSON warning should be visible: {stderr}"
    );
}
