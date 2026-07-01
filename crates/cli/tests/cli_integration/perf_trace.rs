// SPDX-License-Identifier: Apache-2.0
use super::*;

fn profile_trace_from_stderr(stderr: &str) -> Value {
    let lines = stderr.lines().collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        1,
        "jsonl profiling should emit exactly one stderr line: {stderr}"
    );
    serde_json::from_str(lines[0]).expect("profile stderr line should be parseable JSON")
}

fn profile_phase_names(trace: &Value) -> Vec<&str> {
    trace["phases"]
        .as_array()
        .expect("trace should include phases")
        .iter()
        .map(|phase| {
            phase["name"]
                .as_str()
                .expect("phase should include a string name")
        })
        .collect()
}

fn assert_profile_trace_is_sanitized(stderr: &str, temp: &TempDir) {
    let temp_path = temp.path().display().to_string();
    for sensitive in [
        temp_path.as_str(),
        "sensitive-profile-input.txt",
        "HEDDLE_PROFILE",
        "jsonl",
        "--output",
    ] {
        assert!(
            !stderr.contains(sensitive),
            "profile trace should not include sensitive input `{sensitive}`: {stderr}"
        );
    }
}

fn setup_profile_repo_with_sensitive_input() -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("sensitive-profile-input.txt"), "secret").unwrap();
    temp
}

#[test]
fn perf_trace_jsonl_status_keeps_stdout_json_and_stderr_parseable() {
    let temp = setup_profile_repo_with_sensitive_input();

    let output = heddle_output_with_env(
        &["--output", "json", "status"],
        Some(temp.path()),
        &[("HEDDLE_PROFILE", "jsonl")],
    )
    .expect("status should run");
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();

    assert!(
        output.status.success(),
        "profiled status should succeed; stdout={stdout} stderr={stderr}"
    );
    serde_json::from_str::<Value>(stdout).expect("status stdout should remain valid JSON");

    let trace = profile_trace_from_stderr(stderr);
    assert_eq!(trace["schema"], "heddle-cli-profile/v1");
    assert_eq!(trace["command"], "status");
    assert_eq!(trace["exit_status"], "ok");
    let phases = profile_phase_names(&trace);
    for expected in [
        "status repo open",
        "status current state",
        "status worktree status",
        "status thread summary",
        "status build total",
        "status render",
    ] {
        assert!(
            phases.contains(&expected),
            "trace should include status phase `{expected}`: {trace}"
        );
    }
    assert!(
        !phases.contains(&"status phases"),
        "jsonl profile should split the old grouped status phase: {trace}"
    );
    assert_eq!(trace["totals"]["command_body_ms"]["unit"], "milliseconds");
    assert_eq!(trace["totals"]["total_ms"]["unit"], "milliseconds");
    assert!(
        stderr.contains("\"phases\"") && !stderr.contains("heddle profile:"),
        "jsonl mode should not emit human profile text: {stderr}"
    );
    assert_profile_trace_is_sanitized(stderr, &temp);
}

#[test]
fn perf_trace_jsonl_thread_list_uses_named_phase_records() {
    let temp = setup_profile_repo_with_sensitive_input();

    let output = heddle_output_with_env(
        &["--output", "json", "thread", "list"],
        Some(temp.path()),
        &[("HEDDLE_PROFILE", "jsonl")],
    )
    .expect("thread list should run");
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();

    assert!(
        output.status.success(),
        "profiled thread list should succeed; stdout={stdout} stderr={stderr}"
    );
    serde_json::from_str::<Value>(stdout).expect("thread list stdout should remain valid JSON");

    let trace = profile_trace_from_stderr(stderr);
    assert_eq!(trace["schema"], "heddle-cli-profile/v1");
    assert_eq!(trace["command"], "thread list");
    assert_eq!(trace["exit_status"], "ok");
    let phases = profile_phase_names(&trace);
    for expected in [
        "thread list collect summaries",
        "thread list verification",
        "thread list command body",
    ] {
        assert!(
            phases.contains(&expected),
            "trace should include thread list phase `{expected}`: {trace}"
        );
    }
    assert!(
        !phases.contains(&"thread list phases"),
        "jsonl profile should split the old grouped thread list phase: {trace}"
    );
    assert_profile_trace_is_sanitized(stderr, &temp);
}

#[test]
fn perf_trace_jsonl_verify_uses_named_phase_records() {
    let temp = setup_profile_repo_with_sensitive_input();
    heddle(&["capture", "-m", "profile input"], Some(temp.path())).unwrap();

    let output = heddle_output_with_env(
        &["--output", "json", "verify"],
        Some(temp.path()),
        &[("HEDDLE_PROFILE", "jsonl")],
    )
    .expect("verify should run");
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();

    assert!(
        output.status.success(),
        "profiled verify should succeed; stdout={stdout} stderr={stderr}"
    );
    serde_json::from_str::<Value>(stdout).expect("verify stdout should remain valid JSON");

    let trace = profile_trace_from_stderr(stderr);
    assert_eq!(trace["schema"], "heddle-cli-profile/v1");
    assert_eq!(trace["command"], "verify");
    assert_eq!(trace["exit_status"], "ok");
    let phases = profile_phase_names(&trace);
    for expected in [
        "verify plain git probe",
        "verify repo open",
        "verify repository checks",
        "verify command body",
    ] {
        assert!(
            phases.contains(&expected),
            "trace should include verify phase `{expected}`: {trace}"
        );
    }
    assert!(
        !phases.contains(&"verify phases"),
        "jsonl profile should split the old grouped verify phase: {trace}"
    );
    assert_profile_trace_is_sanitized(stderr, &temp);
}

#[test]
fn perf_trace_human_mode_still_writes_profile_text() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output_with_env(
        &["--output", "json", "status"],
        Some(temp.path()),
        &[("HEDDLE_PROFILE", "1")],
    )
    .expect("status should run");
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();

    assert!(
        output.status.success(),
        "human-profiled status should succeed; stdout={stdout} stderr={stderr}"
    );
    serde_json::from_str::<Value>(stdout).expect("status stdout should remain valid JSON");
    assert!(
        stderr.contains("heddle profile:")
            && stderr.contains("command: status phases")
            && stderr.contains("command_body_ms:"),
        "human profile output should be preserved on stderr: {stderr}"
    );
}
