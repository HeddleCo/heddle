// SPDX-License-Identifier: Apache-2.0
use std::{fs, path::Path, time::Duration};

use objects::object::ContentHash;
use serde_json::Value;
use tempfile::TempDir;

use super::*;

const MAINTENANCE_ENVS: [(&str, &str); 1] = [("HEDDLE_FSMONITOR", "native")];

fn maintenance_inspect_json(path: &Path) -> Value {
    let output = heddle_with_env(
        &["--output", "json", "maintenance", "inspect"],
        Some(path),
        &MAINTENANCE_ENVS,
    )
    .unwrap();
    serde_json::from_str(&output).expect("maintenance inspect should return JSON")
}

fn maintenance_run(path: &Path) -> String {
    heddle_with_env(&["maintenance", "run"], Some(path), &MAINTENANCE_ENVS).unwrap()
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn find_field<'a>(value: &'a Value, candidates: &[&str]) -> Option<&'a Value> {
    let candidates: Vec<String> = candidates.iter().map(|key| normalize_key(key)).collect();
    find_field_inner(value, &candidates)
}

fn find_field_inner<'a>(value: &'a Value, candidates: &[String]) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                if candidates
                    .iter()
                    .any(|candidate| normalize_key(key) == *candidate)
                {
                    return Some(nested);
                }
                if let Some(found) = find_field_inner(nested, candidates) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items
            .iter()
            .find_map(|item| find_field_inner(item, candidates)),
        _ => None,
    }
}

fn summary_count(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::Array(items) => Some(items.len() as u64),
        Value::Object(map) => [
            "count",
            "total",
            "entries",
            "len",
            "missing",
            "missing_count",
            "ref_count",
            "refs",
        ]
        .into_iter()
        .find_map(|key| map.get(key))
        .and_then(summary_count),
        _ => None,
    }
}

fn is_present_like(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_u64().unwrap_or_default() > 0,
        Value::String(value) => !value.trim().is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => {
            for key in ["present", "exists", "enabled", "ready"] {
                if let Some(flag) = map.get(key).and_then(Value::as_bool) {
                    return flag;
                }
            }

            for key in ["path", "status", "backend"] {
                if let Some(value) = map.get(key).and_then(Value::as_str)
                    && !value.trim().is_empty()
                {
                    return true;
                }
            }

            summary_count(value).unwrap_or_default() > 0
        }
        Value::Null => false,
    }
}

fn mark_repo_with_missing_blob(path: &Path) {
    let repo = Repository::open(path).expect("repo should open");
    repo.record_missing_blob(ContentHash::compute(b"maintenance-missing-blob"))
        .expect("record missing blob marker");
}

fn known_state_sidecars(path: &Path) -> Vec<&'static str> {
    let mut present = Vec::new();
    for relative in [
        ".heddle/state/index.bin",
        ".heddle/state/fsmonitor.toml",
        ".heddle/state/monitor-native.bin",
        ".heddle/state/monitor-helper.json",
    ] {
        if path.join(relative).exists() {
            present.push(relative);
        }
    }
    present
}

#[test]
fn test_release_help_surfaces_keep_low_level_maintenance_hidden() {
    let temp = TempDir::new().unwrap();
    let top_level = heddle(&["--help"], Some(temp.path())).unwrap();
    assert!(
        !top_level.contains("  index"),
        "top-level help should hide low-level index helper: {top_level}"
    );
    assert!(
        !top_level.contains("  monitor"),
        "top-level help should hide low-level monitor helper: {top_level}"
    );
    assert!(
        !top_level.contains("  gc"),
        "top-level help should hide raw gc helper: {top_level}"
    );

    let maintenance = heddle(&["maintenance", "--help"], Some(temp.path())).unwrap();
    assert!(
        maintenance.contains("gc"),
        "maintenance help should expose garbage collection: {maintenance}"
    );
    assert!(
        !maintenance.contains("index"),
        "maintenance help should keep index as an internal helper: {maintenance}"
    );
    assert!(
        !maintenance.contains("monitor"),
        "maintenance help should keep monitor as an internal helper: {maintenance}"
    );
}

#[test]
fn test_maintenance_inspect_json_reports_repo_shape_fields() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    heddle_must_succeed(&["capture", "-m", "initial"], temp.path());
    mark_repo_with_missing_blob(temp.path());

    let inspect = maintenance_inspect_json(temp.path());
    let root = inspect
        .as_object()
        .expect("inspect output should be an object");
    assert!(!root.is_empty(), "inspect output should not be empty");

    assert!(
        find_field(&inspect, &["monitor", "change_monitor"]).is_some(),
        "inspect should expose monitor information: {inspect}"
    );
    assert!(
        find_field(&inspect, &["index", "worktree_index"]).is_some(),
        "inspect should expose worktree index information: {inspect}"
    );
    assert!(
        find_field(&inspect, &["commit_graph", "commit_graphs", "commitgraph"]).is_some(),
        "inspect should expose commit-graph information: {inspect}"
    );
    assert!(
        find_field(&inspect, &["refs", "ref_summary", "refs_summary"]).is_some(),
        "inspect should expose ref summary information: {inspect}"
    );
    assert!(
        find_field(
            &inspect,
            &[
                "missing_blobs",
                "missing_blob",
                "partial_fetch",
                "partial_fetch_missing"
            ]
        )
        .is_some(),
        "inspect should expose missing-blob or partial-fetch information: {inspect}"
    );
}

#[test]
fn test_maintenance_run_creates_or_refreshes_sidecars_in_simple_repo() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    heddle_must_succeed(&["capture", "-m", "initial"], temp.path());

    let state_dir = temp.path().join(".heddle/state");
    if state_dir.exists() {
        fs::remove_dir_all(&state_dir).unwrap();
    }

    std::thread::sleep(Duration::from_millis(25));
    let output = maintenance_run(temp.path());

    assert!(
        output.is_empty()
            || output.contains("maintenance")
            || output.contains("index")
            || output.contains("monitor"),
        "maintenance run output should be empty or mention maintenance work: {output}"
    );

    let sidecars = known_state_sidecars(temp.path());
    assert!(
        sidecars.iter().any(|path| path.ends_with("index.bin")),
        "maintenance run should create an index sidecar, found: {sidecars:?}"
    );
    assert!(
        sidecars
            .iter()
            .any(|path| path.ends_with("fsmonitor.toml") || path.ends_with("monitor-native.bin")),
        "maintenance run should create a monitor-related sidecar, found: {sidecars:?}"
    );
}

/// `heddle store warm` should: (a) succeed on a fresh repo with no
/// blobs (everything `already_loose` because there's nothing to
/// promote), (b) succeed on a packed-and-pruned repo (promote N
/// blobs), (c) be idempotent on a second invocation.
#[test]
fn test_store_warm_promotes_packed_repo_and_is_idempotent() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    // Stage a few small files and snapshot to populate blobs.
    for i in 0..3 {
        fs::write(
            temp.path().join(format!("warm-{i}.txt")),
            format!("warm-pass payload {i}"),
        )
        .unwrap();
    }
    heddle_must_succeed(&["capture", "-m", "warm-test"], temp.path());

    // Pack + prune so blobs live only in a packfile.
    heddle_must_succeed(&["gc", "--prune"], temp.path());

    // First warm: should report some `promoted` count and 0 errors.
    let first = heddle_must_succeed(&["--output", "json", "store", "warm"], temp.path());
    let first_json: Value = serde_json::from_str(&first).expect("warm output should be JSON");
    let first_promoted = first_json
        .get("promoted")
        .and_then(Value::as_u64)
        .expect("warm output should include 'promoted'");
    let first_errors = first_json
        .get("errors")
        .and_then(Value::as_u64)
        .expect("warm output should include 'errors'");
    assert_eq!(first_errors, 0, "first warm should have no errors: {first}");
    assert!(
        first_promoted > 0,
        "first warm on packed repo should promote at least one blob: {first}"
    );

    // Second warm: idempotent — every blob already loose+uncompressed.
    let second = heddle_must_succeed(&["--output", "json", "store", "warm"], temp.path());
    let second_json: Value =
        serde_json::from_str(&second).expect("second warm output should be JSON");
    let second_promoted = second_json
        .get("promoted")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let second_already = second_json
        .get("already_loose")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    assert_eq!(
        second_promoted, 0,
        "idempotent second warm must not promote: {second}"
    );
    assert!(
        second_already >= first_promoted,
        "second warm should report all blobs already_loose ({second_already} >= {first_promoted})"
    );
}

#[test]
fn test_maintenance_inspect_reflects_sidecars_after_run() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    heddle_must_succeed(&["capture", "-m", "initial"], temp.path());
    mark_repo_with_missing_blob(temp.path());

    maintenance_run(temp.path());
    let inspect = maintenance_inspect_json(temp.path());

    let index = find_field(&inspect, &["index", "worktree_index"])
        .expect("inspect should include index details after maintenance run");
    assert!(
        is_present_like(index),
        "index details should report a present or usable sidecar after run: {inspect}"
    );

    let monitor = find_field(&inspect, &["monitor", "change_monitor"])
        .expect("inspect should include monitor details after maintenance run");
    assert!(
        is_present_like(monitor),
        "monitor details should report a present or usable sidecar after run: {inspect}"
    );

    let refs = find_field(&inspect, &["refs", "ref_summary", "refs_summary"])
        .expect("inspect should include ref summary after maintenance run");
    assert!(
        summary_count(refs).unwrap_or_default() >= 1,
        "ref summary should report at least one ref in a simple repo: {inspect}"
    );

    let missing = find_field(
        &inspect,
        &[
            "missing_blobs",
            "missing_blob",
            "partial_fetch",
            "partial_fetch_missing",
        ],
    )
    .expect("inspect should include missing blob details after maintenance run");
    assert!(
        summary_count(missing).unwrap_or_default() >= 1,
        "missing blob summary should reflect the recorded partial-fetch marker: {inspect}"
    );

    assert!(
        temp.path().join(".heddle/state/index.bin").exists(),
        "maintenance run should leave an index sidecar behind"
    );
}
