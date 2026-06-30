// SPDX-License-Identifier: Apache-2.0
use super::*;

const PAYLOAD_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn timeline_json(args: &[&str], cwd: &std::path::Path) -> Value {
    let mut argv = vec!["--output", "json", "timeline"];
    argv.extend(args.iter().copied());
    let output = heddle_output(&argv, Some(cwd)).expect("invoke timeline command");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "timeline {args:?} should succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    serde_json::from_str(&stdout).expect("timeline JSON should parse")
}

#[test]
fn timeline_recording_status_roundtrip_keeps_payload_scrubbed() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let initial = timeline_json(&["status"], temp.path());
    assert_eq!(initial["output_kind"], "timeline_status");
    assert_eq!(initial["status"], "ok");
    assert_eq!(initial["thread"], "main");
    assert_eq!(initial["step_count"], 0);
    assert!(initial["current_step"].is_null());

    let started = timeline_json(
        &[
            "record-start",
            "--tool-call",
            "call-privacy-roundtrip",
            "--tool-name",
            "read",
            "--summary",
            "summary only",
            "--payload-hash",
            PAYLOAD_HASH,
        ],
        temp.path(),
    );
    assert_eq!(started["output_kind"], "timeline_record_start");
    assert_eq!(started["action"], "record-start");
    assert_eq!(started["payload_summary"], "summary only");
    assert_eq!(started["payload_hash"], PAYLOAD_HASH);
    let step_id = started["step_id"]
        .as_str()
        .expect("record-start returns step id")
        .to_string();

    std::fs::write(temp.path().join("raw-secret-filename.txt"), "changed\n").unwrap();
    heddle(&["capture", "-m", "timeline fixture"], Some(temp.path())).unwrap();

    let finished = timeline_json(
        &[
            "record-finish",
            "--tool-call",
            "call-privacy-roundtrip",
            "--status",
            "succeeded",
            "--summary",
            "finish summary",
            "--payload-hash",
            PAYLOAD_HASH,
        ],
        temp.path(),
    );
    assert_eq!(finished["output_kind"], "timeline_record_finish");
    assert_eq!(finished["action"], "record-finish");
    assert_eq!(finished["step_id"], step_id);
    assert_eq!(finished["changed"], true);
    assert_eq!(finished["tool_status"], "succeeded");

    let status = timeline_json(&["status"], temp.path());
    assert_eq!(status["output_kind"], "timeline_status");
    assert_eq!(status["cursor_step_id"], step_id);
    assert_eq!(status["step_count"], 1);
    assert_eq!(status["current_step"]["payload_summary"], "summary only");
    assert_eq!(status["current_step"]["payload_hash"], PAYLOAD_HASH);
    assert_eq!(status["current_step"]["tool_status"], "succeeded");
    assert_eq!(status["current_step"]["changed"], true);
    let status_text = status.to_string();
    assert!(
        !status_text.contains("raw-secret-filename.txt"),
        "timeline status must not leak filenames or raw payload details: {status_text}"
    );
}

#[test]
fn timeline_record_finish_uses_started_branch_after_cursor_moves() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let started = timeline_json(
        &[
            "record-start",
            "--tool-call",
            "call-branch-pinning",
            "--tool-name",
            "read",
            "--branch",
            "tlb-original",
        ],
        temp.path(),
    );
    assert_eq!(started["branch_id"], "tlb-original");

    let forked = timeline_json(
        &["fork", "--current", "--branch", "tlb-after-start"],
        temp.path(),
    );
    assert_eq!(forked["output_kind"], "timeline_action");
    assert_eq!(forked["cursor_branch_id"], "tlb-after-start");

    let finished = timeline_json(
        &[
            "record-finish",
            "--tool-call",
            "call-branch-pinning",
            "--status",
            "succeeded",
        ],
        temp.path(),
    );
    assert_eq!(finished["output_kind"], "timeline_record_finish");
    assert_eq!(finished["branch_id"], "tlb-original");
}
