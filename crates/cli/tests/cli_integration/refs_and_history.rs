// SPDX-License-Identifier: Apache-2.0
use objects::object::ThreadName;

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

    assert!(heddle(&["thread", "marker", "create", "v1.0.0"], Some(temp.path())).is_ok());

    let output = heddle(&["thread", "marker", "list"], Some(temp.path())).unwrap();
    assert!(output.contains("v1.0.0"), "Should list marker: {}", output);
    assert!(heddle(&["thread", "marker", "show", "v1.0.0"], Some(temp.path())).is_ok());
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

    heddle(
        &["thread", "marker", "create", "failed-test-1"],
        Some(temp.path()),
    )
    .unwrap();
    heddle(
        &["thread", "marker", "create", "failed-test-2"],
        Some(temp.path()),
    )
    .unwrap();
    heddle(&["thread", "marker", "create", "keepme"], Some(temp.path())).unwrap();

    let json = heddle(
        &[
            "--output", "json", "thread", "marker", "list", "--filter", "failed-",
        ],
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
    let json_all = heddle(
        &["--output", "json", "thread", "marker", "list"],
        Some(temp.path()),
    )
    .unwrap();
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
    heddle(&["thread", "marker", "create", "alpha"], Some(temp.path())).unwrap();

    let json = heddle(
        &[
            "--output", "json", "thread", "marker", "list", "--filter", "nope-",
        ],
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
fn test_cli_marker_delete_single_name_form() {
    // The single-positional form is the canonical one-marker delete path.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Marked"], Some(temp.path())).unwrap();
    heddle(&["thread", "marker", "create", "v1"], Some(temp.path())).unwrap();

    let output = heddle(&["thread", "marker", "delete", "v1"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Deleted marker 'v1'"),
        "Single delete output: {}",
        output
    );

    // Deleting a non-existent name should error.
    let err = heddle(
        &["thread", "marker", "delete", "does-not-exist"],
        Some(temp.path()),
    );
    assert!(err.is_err(), "Deleting unknown marker should error");
}

#[test]
fn test_cli_marker_delete_prefix_matches_multiple() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Marked"], Some(temp.path())).unwrap();

    // Three failing-test markers and one keeper.
    heddle(
        &["thread", "marker", "create", "failed-test-1"],
        Some(temp.path()),
    )
    .unwrap();
    heddle(
        &["thread", "marker", "create", "failed-test-2"],
        Some(temp.path()),
    )
    .unwrap();
    heddle(
        &["thread", "marker", "create", "failed-test-3"],
        Some(temp.path()),
    )
    .unwrap();
    heddle(&["thread", "marker", "create", "keepme"], Some(temp.path())).unwrap();

    let output = heddle(
        &["thread", "marker", "delete", "--prefix", "failed-"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        output.contains("Deleted 3 markers"),
        "Bulk delete output: {}",
        output
    );

    // Confirm only the keeper remains.
    let listing = heddle(&["thread", "marker", "list"], Some(temp.path())).unwrap();
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
    heddle(&["thread", "marker", "create", "alpha"], Some(temp.path())).unwrap();

    let output = heddle(
        &["thread", "marker", "delete", "--prefix", "nope-"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        output.contains("No markers matched prefix 'nope-'"),
        "Expected no-match message, got: {}",
        output
    );

    // alpha must still be present.
    let listing = heddle(&["thread", "marker", "list"], Some(temp.path())).unwrap();
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
        &[
            "thread",
            "marker",
            "delete",
            "some-name",
            "--prefix",
            "failed-",
        ],
        Some(temp.path()),
    );
    assert!(result.is_err(), "Clap should reject NAME + --prefix");
}

#[test]
fn test_cli_marker_delete_requires_arg() {
    // Bare `marker delete` with neither NAME nor --prefix must error.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let result = heddle(&["thread", "marker", "delete"], Some(temp.path()));
    assert!(result.is_err(), "Empty marker delete should error");
}

#[test]
fn test_cli_marker_delete_prefix_empty_rejected() {
    // An empty --prefix would match every marker; refuse it explicitly.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Marked"], Some(temp.path())).unwrap();
    heddle(
        &["thread", "marker", "create", "important"],
        Some(temp.path()),
    )
    .unwrap();

    let output = heddle_output(
        &[
            "--output", "json", "thread", "marker", "delete", "--prefix", "",
        ],
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
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Refusing to delete markers")),
        "empty prefix refusal should use full typed advice: {stderr}"
    );

    // Sanity-check: marker still exists.
    let listing = heddle(&["thread", "marker", "list"], Some(temp.path())).unwrap();
    assert!(
        listing.contains("important"),
        "important should remain: {}",
        listing
    );
}

#[test]
fn test_cli_start_creates_exploration_thread() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "main content").unwrap();
    heddle(&["capture", "-m", "Main state"], Some(temp.path())).unwrap();

    let output = heddle(
        &["start", "experiment", "--workspace", "solid"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        output.contains("experiment"),
        "Should show thread created: {}",
        output
    );
}

#[test]
fn test_cli_start_bootstraps_current_state_with_user_config() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).expect("init repo");

    let before = Repository::open(temp.path()).expect("open repo");
    before
        .refs()
        .delete_thread(&ThreadName::new("main"))
        .expect("clear current thread ref");
    assert!(
        before.current_state().unwrap().is_none(),
        "fresh repo should have no current state after clearing main"
    );

    let config_path = temp.path().join("start-config.toml");
    std::fs::write(
        &config_path,
        "[principal]\nname = \"Fork Tester\"\nemail = \"fork@example.com\"\n",
    )
    .unwrap();
    let config = config_path.to_string_lossy().to_string();

    let output = heddle_output_with_env(
        &["thread", "create", "bootstrap-start"],
        Some(temp.path()),
        &[("HEDDLE_CONFIG", &config)],
    )
    .expect("invoke start");
    assert!(
        output.status.success(),
        "thread create should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let repo = Repository::open(temp.path()).expect("reopen repo");
    let bootstrapped = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("fork should bootstrap main before creating fork");
    let forked = repo
        .refs()
        .get_thread(&ThreadName::new("bootstrap-start"))
        .unwrap()
        .expect("fork should create named thread");
    assert_eq!(
        forked, bootstrapped,
        "thread create should create the named thread at the bootstrapped current state"
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
            "thread",
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

    let output = heddle(&["diff", "HEAD~1", "HEAD"], Some(temp.path())).unwrap();
    assert!(
        output.contains("file.txt"),
        "diff should show differences: {}",
        output
    );
}

#[test]
fn test_cli_help_shows_thread_surface() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    // The first screen is the everyday task map. The complete command tree is
    // explicit progressive disclosure through `help --all`.
    let everyday = heddle(&["help"], Some(temp.path())).unwrap();
    assert!(everyday.contains("\n  ready"));
    assert!(!everyday.contains("\n  thread"));
    assert!(!everyday.contains("\n  workspace"));
    assert!(!everyday.contains("\n  worktree"));
    assert!(!everyday.contains("\n  lane"));

    let all = heddle(&["help", "--all"], Some(temp.path())).unwrap();
    assert!(all.contains("\n  thread"));

    let thread_help = heddle(&["thread", "--help"], Some(temp.path())).unwrap();
    assert!(thread_help.contains("review") || thread_help.contains("Usage:"));
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
        help_init.contains("Usage: heddle init")
            || help_init.contains("Initialize Heddle in a directory"),
        "`heddle help init` should render clap's per-verb help: {help_init}"
    );

    // Truly unknown names still print the missing-topic fallback, but now
    // distinguish command paths from topic pages and point back to curated help.
    let help_garbage = heddle(&["help", "definitely-not-a-thing"], Some(temp.path())).unwrap();
    assert!(
        help_garbage.contains("no topic or command 'definitely-not-a-thing'")
            && help_garbage.contains("heddle help model")
            && help_garbage.contains("heddle help"),
        "unknown name should print the missing-topic recovery message: {help_garbage}"
    );
}

#[test]
fn test_cli_show_accepts_short_state_id() {
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

#[test]
fn test_cli_show_and_log_read_committed_attribution_without_timeline() {
    use objects::object::{
        Attribution, AttributionBasis, AttributionClaim, AttributionEvidenceV1, AttributionSource,
        Principal,
    };
    let temp = TempDir::new().expect("repo");
    let repository = init_test_repository(temp.path()).expect("init");
    std::fs::write(temp.path().join("file.txt"), "content").expect("write");
    let mut evidence = AttributionEvidenceV1 {
        harness: Some(AttributionClaim::new(
            "codex",
            AttributionBasis::Observed,
            AttributionSource::Process,
        )),
        ..Default::default()
    };
    for (reported_model, label) in [
        (None, "codex (model unknown)"),
        (Some("actual-model"), "codex (actual-model)"),
    ] {
        evidence.response.model = reported_model.map(|model| {
            AttributionClaim::new(
                model,
                AttributionBasis::ResponseReported,
                AttributionSource::Response,
            )
        });
        let capture = repository
            .snapshot_with_attribution_evidence_profiled(
                Some("committed attribution".into()),
                None,
                Attribution::human(Principal::new("Ada", "ada@example.test")),
                Some(evidence.clone()),
                None,
                false,
            )
            .expect("capture");
        let state_id = capture.state.id().to_string_full();
        let show: Value = serde_json::from_str(
            &heddle(&["show", &state_id, "--output", "json"], Some(temp.path())).expect("show"),
        )
        .expect("show json");
        assert_eq!(show["state_id_full"], state_id);
        assert_eq!(show["is_agent_authored"], true);
        assert!(
            show["agent"].is_null(),
            "partial model must not manufacture a provider"
        );
        assert_eq!(
            show["attribution_evidence"],
            serde_json::to_value(&evidence).expect("evidence")
        );
        let human = heddle(&["show", &state_id, "--output", "text"], Some(temp.path()))
            .expect("human show");
        assert!(human.contains(label), "{human}");
        let log: Value = serde_json::from_str(
            &heddle(&["log", "--output", "json"], Some(temp.path())).expect("log"),
        )
        .expect("log json");
        let entry = log["states"]
            .as_array()
            .expect("states")
            .iter()
            .find(|entry| entry["state_id"] == capture.state.id().short())
            .expect("capture in log");
        assert_eq!(entry["is_agent_authored"], true);
        assert_eq!(entry["agent"], label);
        assert_eq!(entry["attribution_evidence"], show["attribution_evidence"]);
        let human_log =
            heddle(&["log", "-v", "--output", "text"], Some(temp.path())).expect("human log");
        assert!(human_log.contains(label), "{human_log}");
    }
}

#[test]
fn test_cli_show_rejects_missing_committed_attribution_evidence() {
    use objects::object::{Attribution, ContentHash, Principal, State, Tree};
    let temp = TempDir::new().expect("repo");
    let repository = init_test_repository(temp.path()).expect("init");
    let state = State::new(
        Tree::new().hash(),
        vec![],
        Attribution::human(Principal::new("Ada", "ada@example.test")),
    )
    .with_attribution_evidence(ContentHash::compute(b"missing evidence"));
    repository.store().put_state(&state).expect("state");
    let result = heddle(
        &["show", &state.id().to_string_full(), "--output", "json"],
        Some(temp.path()),
    );
    assert!(
        result.is_err(),
        "missing required evidence must not become agent=null success"
    );
    assert!(
        result
            .expect_err("missing")
            .contains("attribution evidence")
    );
}
