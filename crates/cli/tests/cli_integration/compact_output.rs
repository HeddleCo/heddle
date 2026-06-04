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
    let line = stdout
        .lines()
        .next()
        .unwrap_or_else(|| panic!("heddle {argv:?} produced no stdout; stderr={}", String::from_utf8_lossy(&out.stderr)));
    serde_json::from_str(line)
        .unwrap_or_else(|err| panic!("heddle {argv:?} stdout not JSON: {err}\n  line: {line}"))
}

fn full_json(args: &[&str], temp: &TempDir) -> Value {
    let mut argv: Vec<&str> = vec!["--output", "json"];
    argv.extend(args.iter().copied());
    let stdout = heddle(&argv, Some(temp.path())).unwrap_or_else(|err| panic!("heddle {argv:?} failed: {err}"));
    let line = stdout.lines().next().expect("full json stdout");
    serde_json::from_str(line).expect("full json parses")
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
    assert_only_compact_keys(&compact, "status");

    // Contrast: the full contract still carries the noisy metadata the
    // compact projection drops.
    let full = full_json(&["status"], &temp);
    assert!(
        full.get("git_overlay_health").is_some() && full.get("verification").is_some(),
        "full status must keep the metadata compact drops: {full}"
    );
    assert!(
        compact.get("git_overlay_health").is_none() && compact.get("verification").is_none(),
        "compact status must drop git_overlay_health/verification: {compact}"
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
        compact.get("message").is_none() && compact.get("action").is_none()
            && compact.get("recommended_action").is_none(),
        "compact continue must drop message/action/recommended_action: {compact}"
    );
}

#[test]
fn json_compact_is_a_valid_output_value() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init");
    let out = heddle_output(&["--output", "json-compact", "status"], Some(temp.path()))
        .expect("spawn");
    assert!(
        out.status.success(),
        "--output json-compact must parse and run: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}
