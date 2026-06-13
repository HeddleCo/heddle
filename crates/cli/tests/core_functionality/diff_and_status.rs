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

    let result = heddle(
        &["diff", "HEAD~2", "HEAD", "--output", "json"],
        Some(temp.path()),
    )
    .unwrap();

    // Pre-fix this assertion was just `is_ok()`, so the test passed even
    // when state-to-state diffs fell through to the
    // "Binary file or unable to diff" catch-all (which they did, on
    // every plain text file). Lock in real line content instead.
    assert!(
        !result.contains("Binary file or unable to diff"),
        "state-to-state diff must NOT render the binary fallback for plain text. Output:\n{result}"
    );

    // Test harness defaults to `--output json` output, so parse and inspect
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
fn test_diff_worktree_json_groups_changes_by_category() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("modified.txt"), "original\n").unwrap();
    std::fs::write(temp.path().join("delete.txt"), "remove me\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());

    std::fs::write(temp.path().join("modified.txt"), "changed\n").unwrap();
    std::fs::remove_file(temp.path().join("delete.txt")).unwrap();
    std::fs::write(temp.path().join("added.txt"), "brand new\n").unwrap();

    let json = heddle(&["diff", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json)
        .unwrap_or_else(|_| panic!("worktree diff should emit JSON. Got: {json}"));

    // A consumer must derive add/modify/delete from `diff` alone: `changes`
    // is a category object, not a flat array.
    let changes = parsed["changes"].as_object().unwrap_or_else(|| {
        panic!("worktree diff `changes` should be a category object, not a flat array. Got: {json}")
    });
    assert!(
        changes.contains_key("modified")
            && changes.contains_key("added")
            && changes.contains_key("deleted"),
        "changes must mirror the status command's {{modified,added,deleted}} shape. Got: {json}"
    );

    let paths = |key: &str| -> Vec<String> {
        changes[key]
            .as_array()
            .unwrap_or_else(|| panic!("changes.{key} should be an array. Got: {json}"))
            .iter()
            .map(|entry| entry["path"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    assert_eq!(
        paths("modified"),
        vec!["modified.txt".to_string()],
        "{json}"
    );
    assert_eq!(paths("added"), vec!["added.txt".to_string()], "{json}");
    assert_eq!(paths("deleted"), vec!["delete.txt".to_string()], "{json}");
}

#[test]
fn test_diff_worktree_changes_shape_matches_status() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "original\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "changed\n").unwrap();

    let diff_json = heddle(&["diff", "--output", "json"], Some(temp.path())).unwrap();
    let status_json = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let diff: serde_json::Value = serde_json::from_str(&diff_json).unwrap();
    let status: serde_json::Value = serde_json::from_str(&status_json).unwrap();

    let mut diff_keys: Vec<String> = diff["changes"]
        .as_object()
        .unwrap_or_else(|| panic!("diff changes should be an object. Got: {diff_json}"))
        .keys()
        .cloned()
        .collect();
    let mut status_keys: Vec<String> = status["changes"]
        .as_object()
        .unwrap_or_else(|| panic!("status changes should be an object. Got: {status_json}"))
        .keys()
        .cloned()
        .collect();
    diff_keys.sort();
    status_keys.sort();
    assert_eq!(
        diff_keys, status_keys,
        "diff and status `changes` must share the same category field names. \
         diff={diff_json}\nstatus={status_json}"
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
fn test_diff_stat_detects_clear_rename_with_small_edit() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("src/renamed.txt"), "base\nmain\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());

    std::fs::rename(
        temp.path().join("src/renamed.txt"),
        temp.path().join("src/final-name.txt"),
    )
    .unwrap();
    std::fs::write(
        temp.path().join("src/final-name.txt"),
        "base\nmain\nhuman complex first\n",
    )
    .unwrap();

    let json = heddle(&["diff", "--stat", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json)
        .unwrap_or_else(|_| panic!("diff --stat should emit JSON in tests. Got: {json}"));
    // Worktree-mode diff groups `changes` into {modified, added, deleted};
    // a collapsed rename buckets under `modified` (it is an existing file
    // whose identity changed, with `old_path`/`kind` carrying the detail).
    let changes = parsed["changes"]
        .as_object()
        .unwrap_or_else(|| panic!("worktree changes should be a category object. Got: {json}"));
    let modified = changes["modified"]
        .as_array()
        .unwrap_or_else(|| panic!("changes.modified should be an array. Got: {json}"));
    assert_eq!(
        modified.len(),
        1,
        "rename should collapse add/delete into one modified-bucket entry: {json}"
    );
    let entry = &modified[0];
    assert_eq!(entry["kind"], "renamed", "expected renamed row: {json}");
    assert_eq!(entry["old_path"], "src/renamed.txt");
    assert_eq!(entry["path"], "src/final-name.txt");
    assert!(
        changes["added"].as_array().is_some_and(|a| a.is_empty())
            && changes["deleted"].as_array().is_some_and(|a| a.is_empty()),
        "collapsed rename must not leave stray add/delete entries: {json}"
    );
    assert_eq!(parsed["changed_path_count"], 1);
    assert_eq!(parsed["stats"]["files_changed"], 1);
    assert_eq!(parsed["stats"]["renames"], 1);
    assert!(
        entry.get("lines").is_none(),
        "diff --stat JSON should be a stat payload, not a hunk payload: {json}"
    );

    let text = heddle(&["diff", "--stat", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        text.contains("src/renamed.txt -> src/final-name.txt | renamed"),
        "human stat should show the rename path pair. Got:\n{text}"
    );
    assert!(
        text.contains("1 renames"),
        "summary should count rename. Got:\n{text}"
    );
    assert!(
        !text.contains("0 renames"),
        "summary must not contradict the rename row. Got:\n{text}"
    );
}

#[test]
fn test_diff_stat_counts_inserted_lines_in_modified_file() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("notes.txt"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());

    std::fs::write(temp.path().join("notes.txt"), "base\nadded\n").unwrap();

    let text = heddle(&["diff", "--stat", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        text.contains("1 files changed, 1 additions, 0 modifications"),
        "one inserted line in an existing file should count as an addition. Got:\n{text}"
    );
    assert!(
        !text.contains("0 additions, 1 modifications"),
        "stat summary should not classify a pure inserted line as only a modification. Got:\n{text}"
    );
}

#[test]
#[cfg(unix)]
fn test_diff_retargeted_symlink_renders_old_and_new_targets() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("old.txt"), "old\n").unwrap();
    std::fs::write(temp.path().join("new.txt"), "new\n").unwrap();
    std::os::unix::fs::symlink("old.txt", temp.path().join("link")).unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());

    std::fs::remove_file(temp.path().join("link")).unwrap();
    std::os::unix::fs::symlink("new.txt", temp.path().join("link")).unwrap();

    let text = heddle(&["diff", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        text.contains("--- a/link") && text.contains("+++ b/link"),
        "retargeted symlink should render as a modification to link. Got:\n{text}"
    );
    assert!(
        text.contains("-old.txt") && text.contains("+new.txt"),
        "retargeted symlink should show old target -> new target, not an add-only hunk. Got:\n{text}"
    );
}

#[test]
fn test_status_text_in_isolated_checkout_does_not_leak_raw_git_stderr() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());

    let checkout = temp.path().with_extension("isolated-status");
    let checkout_arg = checkout.to_str().expect("checkout path should be utf8");
    heddle_must_succeed(
        &["start", "feature/status-text", "--path", checkout_arg],
        temp.path(),
    );

    let mut command = Command::new(env!("CARGO_BIN_EXE_heddle"));
    let output = command
        .args(["status", "--output", "text"])
        .current_dir(&checkout)
        .output()
        .expect("status should run in isolated checkout");
    let stdout = str::from_utf8(&output.stdout).unwrap_or("");
    let stderr = str::from_utf8(&output.stderr).unwrap_or("");
    assert!(
        output.status.success(),
        "isolated status should succeed. stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("fatal: not a git repository")
            && !stdout.contains("fatal: not a git repository"),
        "isolated status should not leak raw Git stderr. stdout:\n{stdout}\nstderr:\n{stderr}"
    );
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
    // The native fsmonitor helper spawns asynchronously; under CI load the
    // original 500ms window (10 × 50ms) was too tight and flaked. Allow ~6s.
    for _ in 0..60 {
        let output = heddle_with_env(
            &["maintenance", "monitor", "--output", "json"],
            Some(temp.path()),
            &envs,
        )
        .unwrap();
        let monitor: Value = serde_json::from_str(&output).unwrap();
        if monitor["backend"] == "native-helper" {
            assert_eq!(monitor["status"], "usable");
            helper_ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    assert!(
        helper_ready,
        "native helper did not come up after repeated status runs"
    );
}
