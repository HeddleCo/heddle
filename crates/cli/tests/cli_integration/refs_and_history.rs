// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_cli_track_operations() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "On main"], Some(temp.path())).unwrap();

    assert!(heddle(&["thread", "create", "feature/test"], Some(temp.path())).is_ok());

    let output = heddle(&["thread", "list"], Some(temp.path())).unwrap();
    assert!(
        output.contains("feature/test"),
        "Should list new thread: {}",
        output
    );

    assert!(heddle(&["thread", "switch", "feature/test"], Some(temp.path())).is_ok());
}

#[test]
fn test_cli_track_rename() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();
    heddle(&["thread", "create", "old-name"], Some(temp.path())).unwrap();

    assert!(
        heddle(
            &["thread", "rename", "old-name", "new-name"],
            Some(temp.path()),
        )
        .is_ok()
    );

    assert!(
        heddle(&["thread", "list"], Some(temp.path()))
            .unwrap()
            .contains("new-name")
    );
}

#[test]
fn test_cli_marker_operations() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Marked state"], Some(temp.path())).unwrap();

    assert!(heddle(&["marker", "create", "v1.0.0"], Some(temp.path())).is_ok());

    let output = heddle(&["marker", "list"], Some(temp.path())).unwrap();
    assert!(output.contains("v1.0.0"), "Should list marker: {}", output);
    assert!(heddle(&["marker", "show", "v1.0.0"], Some(temp.path())).is_ok());
}

#[test]
fn test_cli_marker_list_filter_prefix_match() {
    // `marker list --filter <prefix>` should narrow the result to
    // markers whose name starts with the given prefix. This is the
    // symmetric LIST counterpart to `marker delete --prefix`.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Marked"], Some(temp.path())).unwrap();

    heddle(&["marker", "create", "failed-test-1"], Some(temp.path())).unwrap();
    heddle(&["marker", "create", "failed-test-2"], Some(temp.path())).unwrap();
    heddle(&["marker", "create", "keepme"], Some(temp.path())).unwrap();

    let json = heddle(
        &["--json", "marker", "list", "--filter", "failed-"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    let markers = parsed["markers"].as_array().expect("markers array");
    assert_eq!(
        markers.len(),
        2,
        "filter 'failed-' should match exactly 2 markers, got: {}",
        json
    );
    for m in markers {
        let name = m["name"].as_str().unwrap();
        assert!(
            name.starts_with("failed-"),
            "filtered marker should start with 'failed-': {}",
            name
        );
    }

    // Unfiltered listing should still return all three.
    let json_all = heddle(&["--json", "marker", "list"], Some(temp.path())).unwrap();
    let parsed_all: Value = serde_json::from_str(&json_all).unwrap();
    assert_eq!(parsed_all["markers"].as_array().unwrap().len(), 3);
}

#[test]
fn test_cli_marker_list_filter_no_match_is_empty_array() {
    // A filter that matches nothing must return an empty array, not
    // an error. This is the difference between "find and delete" (an
    // error if zero matches feels wrong even there — the prefix form
    // returns count: 0) and "find" (always succeeds).
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Marked"], Some(temp.path())).unwrap();
    heddle(&["marker", "create", "alpha"], Some(temp.path())).unwrap();

    let json = heddle(
        &["--json", "marker", "list", "--filter", "nope-"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        parsed["markers"].as_array().unwrap().len(),
        0,
        "non-matching filter should produce empty array: {}",
        json
    );
}

#[test]
fn test_cli_marker_delete_single_back_compat() {
    // The single-positional form must keep working byte-for-byte.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Marked"], Some(temp.path())).unwrap();
    heddle(&["marker", "create", "v1"], Some(temp.path())).unwrap();

    let output = heddle(&["marker", "delete", "v1"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Deleted marker 'v1'"),
        "Single delete output: {}",
        output
    );

    // Deleting a non-existent name should error.
    let err = heddle(&["marker", "delete", "does-not-exist"], Some(temp.path()));
    assert!(err.is_err(), "Deleting unknown marker should error");
}

#[test]
fn test_cli_marker_delete_prefix_matches_multiple() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Marked"], Some(temp.path())).unwrap();

    // Three failing-test markers and one keeper.
    heddle(&["marker", "create", "failed-test-1"], Some(temp.path())).unwrap();
    heddle(&["marker", "create", "failed-test-2"], Some(temp.path())).unwrap();
    heddle(&["marker", "create", "failed-test-3"], Some(temp.path())).unwrap();
    heddle(&["marker", "create", "keepme"], Some(temp.path())).unwrap();

    let output = heddle(
        &["marker", "delete", "--prefix", "failed-"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        output.contains("Deleted 3 markers"),
        "Bulk delete output: {}",
        output
    );

    // Confirm only the keeper remains.
    let listing = heddle(&["marker", "list"], Some(temp.path())).unwrap();
    assert!(
        listing.contains("keepme"),
        "keepme should remain: {}",
        listing
    );
    assert!(
        !listing.contains("failed-"),
        "failed- markers should be gone: {}",
        listing
    );
}

#[test]
fn test_cli_marker_delete_prefix_no_match_is_noop() {
    // Deleting with a prefix that matches nothing is a no-op success
    // (count: 0). Distinct from the single-name form, which errors.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Marked"], Some(temp.path())).unwrap();
    heddle(&["marker", "create", "alpha"], Some(temp.path())).unwrap();

    let output = heddle(
        &["marker", "delete", "--prefix", "nope-"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        output.contains("No markers matched prefix 'nope-'"),
        "Expected no-match message, got: {}",
        output
    );

    // alpha must still be present.
    let listing = heddle(&["marker", "list"], Some(temp.path())).unwrap();
    assert!(
        listing.contains("alpha"),
        "alpha should remain: {}",
        listing
    );
}

#[test]
fn test_cli_marker_delete_prefix_and_name_conflict() {
    // Clap should reject combining --prefix with a positional NAME.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Marked"], Some(temp.path())).unwrap();

    let result = heddle(
        &["marker", "delete", "some-name", "--prefix", "failed-"],
        Some(temp.path()),
    );
    assert!(result.is_err(), "Clap should reject NAME + --prefix");
}

#[test]
fn test_cli_marker_delete_requires_arg() {
    // Bare `marker delete` with neither NAME nor --prefix must error.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let result = heddle(&["marker", "delete"], Some(temp.path()));
    assert!(result.is_err(), "Empty marker delete should error");
}

#[test]
fn test_cli_marker_delete_prefix_empty_rejected() {
    // An empty --prefix would match every marker; refuse it explicitly.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Marked"], Some(temp.path())).unwrap();
    heddle(&["marker", "create", "important"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "marker", "delete", "--prefix", ""],
        Some(temp.path()),
    )
    .expect("marker delete should run");
    assert!(
        !output.status.success(),
        "Empty --prefix should be rejected"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode marker refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr)
        .unwrap_or_else(|err| panic!("stderr should be JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "marker_delete_empty_prefix");
    assert!(
        envelope["error"].as_str().is_some_and(|error| error
            .contains("Refusing to delete markers")
            && error.contains("Unsafe:")
            && error.contains("Would change:")
            && error.contains("Preserved:")
            && error.contains("Primary recovery:")),
        "empty prefix refusal should use full typed advice: {stderr}"
    );

    // Sanity-check: marker still exists.
    let listing = heddle(&["marker", "list"], Some(temp.path())).unwrap();
    assert!(
        listing.contains("important"),
        "important should remain: {}",
        listing
    );
}

#[test]
fn test_cli_fork_creates_exploration_branch() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "main content").unwrap();
    heddle(&["capture", "-m", "Main state"], Some(temp.path())).unwrap();

    let output = heddle(&["fork", "--name", "experiment"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Created fork") || output.contains("experiment"),
        "Should show fork created: {}",
        output
    );
}

#[test]
fn test_cli_collapse_squashes_states() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for i in 1..=3 {
        std::fs::write(
            temp.path().join(format!("file{}.txt", i)),
            format!("content {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("State {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    let mut state_ids = state_chain_ids(temp.path(), 3);
    state_ids.reverse();
    let output = heddle(
        &[
            "collapse",
            &state_ids[0],
            &state_ids[1],
            &state_ids[2],
            "--into",
            "Collapsed work",
        ],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        output.contains("Collapsed 3 states into"),
        "Collapse should report success: {}",
        output
    );
}

#[test]
fn test_cli_compare_shows_differences() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("file.txt"), "version1").unwrap();
    heddle(&["capture", "-m", "State A"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "version2").unwrap();
    heddle(&["capture", "-m", "State B"], Some(temp.path())).unwrap();

    let output = heddle(&["compare", "HEAD~1", "HEAD"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Comparing") || output.contains("file.txt"),
        "Compare should show differences: {}",
        output
    );
}

#[test]
fn test_cli_help_shows_thread_surface() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    // `thread`, `workspace`, and `ready` are part of the core-loop
    // everyday surface. The thread-surface terminology guarantee is
    // still load-bearing: neither tier should resurrect the legacy
    // `worktree`/`lane` verbs.
    let everyday = heddle(&["help"], Some(temp.path())).unwrap();
    assert!(everyday.contains("\n  thread"));
    assert!(everyday.contains("\n  workspace"));
    assert!(everyday.contains("\n  ready"));
    assert!(!everyday.contains("\n  worktree"));
    assert!(!everyday.contains("\n  lane"));

    let advanced = heddle(&["help", "advanced"], Some(temp.path())).unwrap();
    assert!(advanced.contains("review"));
    assert!(!advanced.contains("\n  worktree"));
    assert!(!advanced.contains("\n  lane"));
}

#[test]
fn test_cli_help_verb_falls_through_to_clap() {
    // Contract on `Commands::Help`: `heddle help <verb>` falls through
    // to that verb's clap-derived help. Regression test against the
    // earlier behaviour where any non-topic name printed "no topic".
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let help_init = heddle(&["help", "init"], Some(temp.path())).unwrap();
    assert!(
        !help_init.contains("no topic"),
        "`heddle help init` printed the missing-topic fallback instead \
         of clap's per-verb help: {help_init}"
    );
    assert!(
        help_init.contains("Usage: init") || help_init.contains("Initialize a new Heddle"),
        "`heddle help init` should render clap's per-verb help: {help_init}"
    );

    // Truly unknown names still print the missing-topic fallback, but now
    // distinguish command paths from topic pages and point back to curated help.
    let help_garbage = heddle(&["help", "definitely-not-a-thing"], Some(temp.path())).unwrap();
    assert!(
        help_garbage.contains("no topic or command 'definitely-not-a-thing'")
            && help_garbage.contains("heddle help advanced")
            && help_garbage.contains("heddle help"),
        "unknown name should print the missing-topic recovery message: {help_garbage}"
    );
}

#[test]
fn test_cli_show_accepts_short_change_id() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "State 1"], Some(temp.path())).unwrap();

    let log_output = heddle(&["log", "--oneline", "--output", "text"], Some(temp.path())).unwrap();
    let short_id = log_output
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .expect("log should include change id");

    let output = heddle(&["show", short_id], Some(temp.path())).unwrap();
    assert!(
        output.contains(short_id) || output.contains("State:"),
        "Show should display state details: {}",
        output
    );
}
