// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn perf_trace_jsonl_status_keeps_stdout_json_and_stderr_parseable() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

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

    let lines = stderr.lines().collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        1,
        "jsonl profiling should emit exactly one stderr line: {stderr}"
    );
    let trace: Value =
        serde_json::from_str(lines[0]).expect("profile stderr line should be parseable JSON");
    assert_eq!(trace["schema"], "heddle-cli-profile/v1");
    assert_eq!(trace["command"], "status");
    assert_eq!(trace["exit_status"], "ok");
    assert!(
        trace["phases"].as_array().is_some_and(|phases| {
            phases.iter().any(|phase| phase["name"] == "status phases")
                && phases
                    .iter()
                    .any(|phase| phase["name"] == "status worktree")
                && phases.iter().any(|phase| phase["name"] == "status render")
        }),
        "trace should include status phase records: {trace}"
    );
    assert_eq!(trace["totals"]["command_body_ms"]["unit"], "milliseconds");
    assert_eq!(trace["totals"]["total_ms"]["unit"], "milliseconds");
    assert!(
        stderr.contains("\"phases\"") && !stderr.contains("heddle profile:"),
        "jsonl mode should not emit human profile text: {stderr}"
    );
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
