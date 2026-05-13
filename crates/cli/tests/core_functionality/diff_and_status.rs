// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_diff_between_arbitrary_states() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "version1\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "State A"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "version2\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "State B"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "version3\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "State C"], temp.path());

    let result = heddle(&["diff", "HEAD~2", "HEAD"], Some(temp.path())).unwrap();

    // Pre-fix this assertion was just `is_ok()`, so the test passed even
    // when state-to-state diffs fell through to the
    // "Binary file or unable to diff" catch-all (which they did, on
    // every plain text file). Lock in real line content instead.
    assert!(
        !result.contains("Binary file or unable to diff"),
        "state-to-state diff must NOT render the binary fallback for plain text. Output:\n{result}"
    );

    // Test harness defaults to `--json` output, so parse and inspect
    // the line list rather than grepping the text format.
    let parsed: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|_| panic!("diff output should be JSON. Got: {result}"));
    let lines = parsed["changes"][0]["lines"]
        .as_array()
        .unwrap_or_else(|| panic!("changes[0].lines should be an array. Got: {result}"));
    assert!(
        !lines.is_empty(),
        "state-to-state diff must populate `lines` for changed text (was the bug \
         pre-Phase-D). Got: {result}"
    );
    let has_minus_v1 = lines
        .iter()
        .any(|l| l["prefix"] == "-" && l["content"] == "version1");
    let has_plus_v3 = lines
        .iter()
        .any(|l| l["prefix"] == "+" && l["content"] == "version3");
    assert!(
        has_minus_v1 && has_plus_v3,
        "state-to-state diff must contain '-version1' and '+version3' lines. Got: {lines:#?}"
    );
}

#[test]
fn test_diff_with_deletions() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("keep.txt"), "kept").unwrap();
    std::fs::write(temp.path().join("delete.txt"), "deleted").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());
    std::fs::remove_file(temp.path().join("delete.txt")).unwrap();
    let result = heddle(&["diff"], Some(temp.path())).unwrap();
    assert!(result.contains("delete") || result.contains("removed") || result.contains("-"));
}

#[test]
fn test_status_all_change_types() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("unchanged.txt"), "same").unwrap();
    std::fs::write(temp.path().join("modified.txt"), "original").unwrap();
    std::fs::write(temp.path().join("will_delete.txt"), "delete me").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());
    std::fs::write(temp.path().join("modified.txt"), "changed").unwrap();
    std::fs::remove_file(temp.path().join("will_delete.txt")).unwrap();
    std::fs::write(temp.path().join("new_file.txt"), "new").unwrap();
    let result = heddle(&["status"], Some(temp.path())).unwrap();
    assert!(result.contains("modified") || result.contains("modified.txt"));
    assert!(
        result.contains("new")
            || result.contains("new_file")
            || result.contains("added")
            || result.contains("untracked")
    );
    assert!(
        result.contains("delete") || result.contains("will_delete") || result.contains("removed")
    );
}

#[test]
fn test_status_clean_after_snapshot() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Clean state"], temp.path());
    let result = heddle(&["status"], Some(temp.path())).unwrap();
    assert!(
        result.contains("clean") || result.contains("no changes") || !result.contains("file.txt")
    );
}

#[test]
fn test_native_status_warms_helper_for_second_run() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Clean state"], temp.path());

    let envs = [("HEDDLE_FSMONITOR", "native")];

    let first = heddle_with_env(&["status", "--short"], Some(temp.path()), &envs).unwrap();
    assert!(!first.contains("file.txt"));

    let second = heddle_with_env(&["status", "--short"], Some(temp.path()), &envs).unwrap();
    assert!(!second.contains("file.txt"));

    let mut helper_ready = false;
    for _ in 0..10 {
        let output = heddle_with_env(&["monitor"], Some(temp.path()), &envs).unwrap();
        let monitor: Value = serde_json::from_str(&output).unwrap();
        if monitor["backend"] == "native-helper" {
            assert_eq!(monitor["status"], "usable");
            helper_ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    assert!(
        helper_ready,
        "native helper did not come up after repeated status runs"
    );
}