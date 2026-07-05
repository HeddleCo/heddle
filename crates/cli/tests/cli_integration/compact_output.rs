// SPDX-License-Identifier: Apache-2.0
//! `--output json-compact` projects the decision surface only.
//!
//! heddle#470: `--output json` is the full machine contract — great for
//! machines, but extremely noisy for an agent driving Heddle, where the
//! actionable surface is just `output_kind`, `status`/`coordination_status`,
//! `blockers`, `next_action`, `changed_paths`/`changed_path_count`, and
//! `conflicts`/`conflict_count`. `--output json-compact` emits ONLY those
//! fields; the full `--output json` contract is unchanged.

use std::str;

use serde_json::Value;
use tempfile::TempDir;

use super::{heddle, heddle_output};

/// Every key the compact decision surface is allowed to emit. Any key
/// outside this set leaking into a compact payload is a regression —
/// the whole point of the mode is that the surface stays small as the
/// full envelope grows.
const COMPACT_ALLOWED_KEYS: &[&str] = &[
    "output_kind",
    "status",
    "coordination_status",
    "blockers",
    "next_action",
    "next_action_template",
    "changed_paths",
    "changed_path_count",
    "conflicts",
    "conflict_count",
];

fn assert_only_compact_keys(value: &Value, context: &str) {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("{context}: compact payload must be a JSON object: {value}"));
    for key in obj.keys() {
        assert!(
            COMPACT_ALLOWED_KEYS.contains(&key.as_str()),
            "{context}: compact payload leaked non-decision-surface key `{key}`: {value}"
        );
    }
}

fn compact_json(args: &[&str], temp: &TempDir) -> Value {
    let mut argv: Vec<&str> = vec!["--output", "json-compact"];
    argv.extend(args.iter().copied());
    let out =
        heddle_output(&argv, Some(temp.path())).unwrap_or_else(|err| panic!("spawn failed: {err}"));
    let stdout = str::from_utf8(&out.stdout).expect("stdout utf8");
    let line = stdout.lines().next().unwrap_or_else(|| {
        panic!(
            "heddle {argv:?} produced no stdout; stderr={}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    serde_json::from_str(line)
        .unwrap_or_else(|err| panic!("heddle {argv:?} stdout not JSON: {err}\n  line: {line}"))
}

fn compact_json_output(args: &[&str], temp: &TempDir) -> Value {
    let out =
        heddle_output(args, Some(temp.path())).unwrap_or_else(|err| panic!("spawn failed: {err}"));
    assert!(
        out.status.success(),
        "heddle {args:?} should succeed; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = str::from_utf8(&out.stdout).expect("stdout utf8");
    let line = stdout
        .lines()
        .next()
        .unwrap_or_else(|| panic!("heddle {args:?} produced no stdout"));
    serde_json::from_str(line)
        .unwrap_or_else(|err| panic!("heddle {args:?} stdout not JSON: {err}\n  line: {line}"))
}

fn assert_compact_op_id_error_envelope(args: &[&str], temp: &TempDir, context: &str) -> Value {
    let out =
        heddle_output(args, Some(temp.path())).unwrap_or_else(|err| panic!("spawn failed: {err}"));
    assert!(
        !out.status.success(),
        "{context}: heddle {args:?} should fail; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "{context}: failed compact op-id command should not emit stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = str::from_utf8(&out.stderr).expect("stderr utf8");
    let envelope: Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|err| panic!("{context}: stderr not JSON: {err}\n  stderr: {stderr}"));
    // `kind` (not the dropped `code` duplicate) is the envelope's
    // discriminator (HeddleCo/heddle#647).
    for key in ["kind", "error", "exit_code", "hint"] {
        assert!(
            envelope.get(key).is_some(),
            "{context}: compact op-id failure stripped `{key}` from error envelope: {envelope}"
        );
    }
    envelope
}

fn full_json(args: &[&str], temp: &TempDir) -> Value {
    let mut argv: Vec<&str> = vec!["--output", "json"];
    argv.extend(args.iter().copied());
    let stdout = heddle(&argv, Some(temp.path()))
        .unwrap_or_else(|err| panic!("heddle {argv:?} failed: {err}"));
    let line = stdout.lines().next().expect("full json stdout");
    serde_json::from_str(line).expect("full json parses")
}

#[test]
fn capture_op_id_compact_failure_preserves_error_envelope() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    let op_id = "550e8400-e29b-41d4-a716-446655440471";
    let args = ["--output", "json-compact", "--op-id", op_id, "capture"];

    let first = assert_compact_op_id_error_envelope(&args, &temp, "capture op-id failure");
    let replayed =
        assert_compact_op_id_error_envelope(&args, &temp, "capture op-id failure replay");
    assert_eq!(
        replayed, first,
        "compact op-id replay should preserve the cached error envelope verbatim"
    );
}

#[test]
fn status_compact_emits_only_decision_surface() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");

    let compact = compact_json(&["status"], &temp);
    assert_eq!(
        compact["output_kind"].as_str(),
        Some("status"),
        "compact status must carry output_kind: {compact}"
    );
    assert!(
        compact.get("coordination_status").is_some(),
        "compact status must carry coordination_status: {compact}"
    );
    assert!(
        compact.get("changed_path_count").is_some(),
        "compact status must carry changed_path_count: {compact}"
    );
    assert!(
        compact.get("changed_paths").is_some(),
        "compact status must carry changed_paths: {compact}"
    );
    assert_only_compact_keys(&compact, "status");

    // Contrast: the full contract still carries verification metadata the
    // compact projection drops.
    let full = full_json(&["status"], &temp);
    assert!(
        full.get("git_overlay_health").is_none() && full.get("verification").is_some(),
        "full status must expose verification without the legacy git_overlay_health alias: {full}"
    );
    assert!(
        compact.get("git_overlay_health").is_none() && compact.get("verification").is_none(),
        "compact status must drop git_overlay_health/verification: {compact}"
    );
}

#[test]
fn status_compact_keeps_uncaptured_changed_path_count_consistent() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    std::fs::write(temp.path().join("work.txt"), "pending\n").unwrap();

    let compact = compact_json(&["status"], &temp);
    let changed_paths = compact["changed_paths"]
        .as_array()
        .unwrap_or_else(|| panic!("compact status must carry changed_paths: {compact}"));
    assert_eq!(
        compact["changed_path_count"].as_u64(),
        Some(changed_paths.len() as u64),
        "compact status count must match changed_paths in a dirty uncaptured repo: {compact}"
    );
    assert_eq!(
        changed_paths.as_slice(),
        [Value::String("work.txt".to_string())],
        "dirty uncaptured repo should report the pending worktree path: {compact}"
    );
    assert_only_compact_keys(&compact, "dirty uncaptured status");
}

#[test]
fn capture_op_id_compact_replay_emits_only_decision_surface() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    std::fs::write(temp.path().join("work.txt"), "pending\n").unwrap();
    let op_id = "550e8400-e29b-41d4-a716-446655440470";
    let args = [
        "--output",
        "json-compact",
        "--op-id",
        op_id,
        "capture",
        "-m",
        "compact op-id capture",
    ];

    let first = compact_json_output(&args, &temp);
    assert_only_compact_keys(&first, "capture op-id executed");
    assert!(
        first.get("operation_record").is_none()
            && first.get("op_id").is_none()
            && first.get("idempotency_status").is_none()
            && first.get("replayed").is_none(),
        "compact executed op-id output must not leak idempotency fields: {first}"
    );

    let replayed = compact_json_output(&args, &temp);
    assert_only_compact_keys(&replayed, "capture op-id replayed");
    assert_eq!(
        replayed, first,
        "compact op-id replay should return the cached compact payload without wrapper decoration"
    );
    assert!(
        replayed.get("operation_record").is_none()
            && replayed.get("op_id").is_none()
            && replayed.get("idempotency_status").is_none()
            && replayed.get("replayed").is_none(),
        "compact replayed op-id output must not leak idempotency fields: {replayed}"
    );
}

#[test]
fn continue_compact_drops_operator_metadata() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");

    let compact = compact_json(&["continue"], &temp);
    assert_eq!(compact["output_kind"].as_str(), Some("continue"));
    assert_eq!(compact["status"].as_str(), Some("noop"));
    assert_only_compact_keys(&compact, "continue");

    // The full operator envelope carries `message`, `action`, and
    // `recommended_action`; compact keeps only `next_action`.
    let full = full_json(&["continue"], &temp);
    assert!(full.get("message").is_some() && full.get("action").is_some());
    assert!(
        compact.get("message").is_none()
            && compact.get("action").is_none()
            && compact.get("recommended_action").is_none(),
        "compact continue must drop message/action/recommended_action: {compact}"
    );
}

#[test]
fn json_compact_is_a_valid_output_value() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    let out =
        heddle_output(&["--output", "json-compact", "status"], Some(temp.path())).expect("spawn");
    assert!(
        out.status.success(),
        "--output json-compact must parse and run: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn json_compact_rejects_commands_without_projection() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");

    let out =
        heddle_output(&["--output", "json-compact", "help"], Some(temp.path())).expect("spawn");
    assert!(
        !out.status.success(),
        "compact-less command must reject json-compact"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("json-compact is not supported"),
        "rejection should explain unsupported compact mode: {stderr}"
    );
}
