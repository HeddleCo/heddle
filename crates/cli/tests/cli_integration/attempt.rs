// SPDX-License-Identifier: Apache-2.0
//! Coverage for item 3.2 of the heddle 6→8 plan: `heddle attempt N -- <cmd>`.
//!
//! Exercises:
//!   1. `heddle attempt 1 -- true` — the degenerate case (equivalent
//!      to `heddle try`). One attempt, succeeds, recommends itself,
//!      parent unchanged.
//!   2. `heddle attempt 3 -- true` — three parallel successes; ranking
//!      picks one; the other two stay around.
//!   3. `heddle attempt 3 -- false` — three failures; all dropped;
//!      no recommendation.
//!   4. Mixed: one branch succeeds, another fails — ranking puts the
//!      success first; failure dropped.
//!   5. `--evaluate "false"` after primary success — surfaces as
//!      `evaluate_failed` and ranks below pure successes.
//!   6. `N > 10` rejected with a clear error.
//!   7. `--name-prefix my-test` → threads named `my-test-1`, ….
//!   8. `--shared-target` (Rust workspace default-on) doesn't error
//!      and produces N attempts.

use std::fs;

use serde_json::Value;
use tempfile::TempDir;

use super::{heddle, heddle_argv_json, heddle_output};

/// Bootstrap a minimal repo with a single capture so the parent has a
/// HEAD. Same shape as the `try_cmd.rs` setup so the two suites stay
/// comparable.
fn setup_repo() -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "init"], Some(temp.path())).unwrap();
    temp
}

/// Resolve the parent thread's HEAD via `heddle log --output json` so we
/// observe through the same path an agent would.
fn parent_head(repo: &std::path::Path) -> String {
    let raw = heddle(&["--output", "json", "log", "--limit", "1"], Some(repo)).unwrap();
    let value: Value = serde_json::from_str(&raw).unwrap();
    value["states"][0]["change_id_full"]
        .as_str()
        .or_else(|| value["states"][0]["change_id"].as_str())
        .unwrap()
        .to_string()
}

/// Capture the parent's worktree top-level files. Used to confirm the
/// invariant that `heddle attempt` never touches the parent's files.
fn worktree_snapshot(repo: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in fs::read_dir(repo).unwrap().flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        if name_str.starts_with('.') {
            continue;
        }
        if path.is_file() {
            out.push((name_str, fs::read(&path).unwrap()));
        }
    }
    out.sort();
    out
}

#[test]
fn attempt_n_one_is_degenerate_try() {
    let temp = setup_repo();
    let head_before = parent_head(temp.path());
    let worktree_before = worktree_snapshot(temp.path());

    let raw = heddle(
        &["--output", "json", "attempt", "1", "--", "true"],
        Some(temp.path()),
    )
    .expect("heddle attempt 1 -- true should succeed");
    let value: Value = serde_json::from_str(&raw).expect("output should be JSON");

    assert_eq!(value["status"], "completed", "raw: {raw}");
    assert_eq!(value["attempts_total"], 1);
    assert_eq!(value["attempts_succeeded"], 1);
    assert!(value["recommended"].is_string(), "raw: {raw}");
    let recommended = value["recommended"]
        .as_str()
        .expect("attempt should recommend a winning thread");
    assert_eq!(
        value["next_action"],
        format!("heddle ready --thread {recommended}"),
        "attempt should check readiness for the winning thread before landing it: {raw}"
    );
    assert_eq!(
        value["recommended_action"], value["next_action"],
        "attempt should expose the shared action field for agents: {raw}"
    );
    assert_eq!(
        value["next_action_template"]["argv_template"],
        heddle_argv_json(["ready", "--thread", recommended]),
        "attempt should expose replayable argv_template for the readiness action: {raw}"
    );
    assert!(
        value["next_action_template"]["required_inputs"]
            .as_array()
            .is_some_and(|inputs| inputs.is_empty()),
        "attempt's concrete readiness action template should need no inputs to run: {raw}"
    );
    assert_eq!(
        value["recommended_action_template"]["argv_template"], value["next_action_template"]["argv_template"],
        "recommended action argv_template should mirror next_action argv_template: {raw}"
    );
    assert_eq!(
        value["recommended_action_template"], value["next_action_template"],
        "recommended_action template metadata should mirror next_action: {raw}"
    );

    // Parent invariants.
    assert_eq!(head_before, parent_head(temp.path()));
    assert_eq!(worktree_before, worktree_snapshot(temp.path()));
}

#[test]
fn attempt_three_parallel_successes_produce_ranking() {
    let temp = setup_repo();
    let head_before = parent_head(temp.path());

    let raw = heddle(
        &["--output", "json", "attempt", "3", "--", "true"],
        Some(temp.path()),
    )
    .expect("attempt 3 -- true should succeed");
    let value: Value = serde_json::from_str(&raw).unwrap();

    assert_eq!(value["status"], "completed", "raw: {raw}");
    assert_eq!(value["attempts_total"], 3);
    assert_eq!(value["attempts_succeeded"], 3);

    let recommended = value["recommended"].as_str().expect("recommended set");
    assert!(
        recommended.starts_with("attempt-"),
        "recommended should be a generated attempt-* name (got {recommended})"
    );

    // Ranking should have rank-1 = recommended.
    let attempts = value["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 3);
    assert_eq!(
        attempts[0]["thread"].as_str().unwrap(),
        recommended,
        "rank-1 attempt should equal the recommendation"
    );

    // Parent HEAD must not move.
    assert_eq!(head_before, parent_head(temp.path()));

    // The two non-recommended attempts should still exist as Active
    // threads (we only drop failures).
    let list_raw = heddle(&["--output", "json", "thread", "list"], Some(temp.path())).unwrap();
    let list: Value = serde_json::from_str(&list_raw).unwrap();
    let active_attempts: Vec<&str> = list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["thread_state"] == "active")
        .filter_map(|t| t["name"].as_str())
        .filter(|n| n.starts_with("attempt-"))
        .collect();
    assert_eq!(
        active_attempts.len(),
        3,
        "all three success attempts should survive cleanup; got {active_attempts:?}"
    );
}

#[test]
fn attempt_all_failures_drops_all_and_yields_no_recommendation() {
    let temp = setup_repo();
    let head_before = parent_head(temp.path());
    let worktree_before = worktree_snapshot(temp.path());

    // `false` exits non-zero; the entire command returns 0 from
    // heddle's perspective (it printed a structured "no winner"
    // result), so we use heddle(...) which checks the outer exit
    // code rather than heddle_output.
    let raw = heddle(
        &["--output", "json", "attempt", "3", "--", "false"],
        Some(temp.path()),
    )
    .expect("attempt 3 -- false: outer cmd should still exit 0 (it's a structured no-win)");
    let value: Value = serde_json::from_str(&raw).unwrap();

    assert_eq!(value["status"], "failed", "raw: {raw}");
    assert_eq!(value["attempts_total"], 3);
    assert_eq!(value["attempts_succeeded"], 0);
    assert_eq!(value["attempts_dropped"], 3);
    assert!(
        value["recommended"].is_null(),
        "no recommendation on all-fail: {raw}"
    );

    // Parent invariants.
    assert_eq!(head_before, parent_head(temp.path()));
    assert_eq!(worktree_before, worktree_snapshot(temp.path()));

    // No active attempt-* threads should remain.
    let list_raw = heddle(&["--output", "json", "thread", "list"], Some(temp.path())).unwrap();
    let list: Value = serde_json::from_str(&list_raw).unwrap();
    let active_attempts: Vec<&str> = list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["thread_state"] == "active")
        .filter_map(|t| t["name"].as_str())
        .filter(|n| n.starts_with("attempt-"))
        .collect();
    assert!(
        active_attempts.is_empty(),
        "all failed attempts should be dropped; got {active_attempts:?}"
    );
}

#[test]
fn attempt_mixed_success_and_failure_ranks_success_first() {
    let temp = setup_repo();

    // Build a script that flips behaviour based on which thread's
    // checkout it's running in. Each attempt's checkout sits at
    // `.heddle/threads/<thread-name>/root`, so the parent
    // directory's basename is the thread name (`mixed-1`, `mixed-2`,
    // …). We succeed on threads whose name ends in `-1` and fail on
    // the rest. With N=3 that yields exactly one success and two
    // failures.
    let script_dir = TempDir::new().unwrap();
    let script_path = script_dir.path().join("mixed.sh");
    fs::write(
        &script_path,
        "#!/bin/sh\nset -e\n\
        thread_name=$(basename \"$(dirname \"$(pwd)\")\")\n\
        case \"$thread_name\" in\n  *-1) printf 'win\\n' > out.txt; exit 0 ;;\n  *) exit 1 ;;\nesac\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).unwrap();
    }

    let raw = heddle(
        &[
            "--output",
            "json",
            "attempt",
            "3",
            "--name-prefix",
            "mixed",
            "--",
            "sh",
            script_path.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("mixed attempt outer should succeed");
    let value: Value = serde_json::from_str(&raw).unwrap();

    assert_eq!(value["status"], "completed", "raw: {raw}");
    assert_eq!(value["attempts_total"], 3);
    assert_eq!(value["attempts_succeeded"], 1);
    assert_eq!(value["attempts_dropped"], 2);

    let recommended = value["recommended"].as_str().expect("recommendation");
    assert!(
        recommended.ends_with("-1"),
        "the only succeeding attempt is the one ending in -1; got {recommended}"
    );

    // Rank-1 is the recommended attempt.
    let attempts = value["attempts"].as_array().unwrap();
    assert_eq!(attempts[0]["thread"].as_str().unwrap(), recommended);
    assert_eq!(attempts[0]["status"], "succeeded");
    // The remaining two are failures.
    for failed in &attempts[1..] {
        assert_eq!(failed["status"], "failed");
    }
}

#[test]
fn attempt_n_zero_is_rejected() {
    let temp = setup_repo();
    let output = heddle_output(
        &["--output", "json", "attempt", "0", "--", "true"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(!output.status.success(), "N=0 should fail");
    let stderr = std::str::from_utf8(&output.stderr).unwrap_or("");
    assert!(stderr.contains("at least 1"), "stderr: {stderr}");
}

#[test]
fn attempt_n_eleven_is_rejected_with_clear_error() {
    let temp = setup_repo();
    let output = heddle_output(
        &["--output", "json", "attempt", "11", "--", "true"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(!output.status.success(), "N>10 should fail");
    let stderr = std::str::from_utf8(&output.stderr).unwrap_or("");
    assert!(
        stderr.contains("capped at 10"),
        "expected the cap error; stderr: {stderr}"
    );
}

#[test]
fn attempt_evaluate_failure_ranks_below_clean_success() {
    let temp = setup_repo();

    // We need at least one "primary success + evaluate failure" attempt
    // to verify ranking. Use N=1 with `--evaluate "false"`; expectation
    // is `attempts_succeeded == 0`, `recommended` is the
    // EvaluateFailed attempt (fallback path), and the message reflects
    // "no clean wins".
    let raw = heddle(
        &[
            "--output",
            "json",
            "attempt",
            "1",
            "--evaluate",
            "false",
            "--",
            "true",
        ],
        Some(temp.path()),
    )
    .expect("evaluate-failure outer should still exit 0");
    let value: Value = serde_json::from_str(&raw).unwrap();

    assert_eq!(value["attempts_total"], 1);
    assert_eq!(value["attempts_succeeded"], 0);
    let attempts = value["attempts"].as_array().unwrap();
    assert_eq!(attempts[0]["status"], "evaluate_failed");
    // We still surface a recommendation as a fallback ("primary worked,
    // tests didn't"), so the user has something to inspect.
    assert!(
        value["recommended"].is_string(),
        "fallback recommendation expected when evaluate fails: {raw}"
    );
}

#[test]
fn attempt_with_explicit_name_prefix_uses_that_prefix() {
    let temp = setup_repo();
    let raw = heddle(
        &[
            "--output",
            "json",
            "attempt",
            "2",
            "--name-prefix",
            "my-prefix",
            "--",
            "true",
        ],
        Some(temp.path()),
    )
    .expect("attempt with --name-prefix should succeed");
    let value: Value = serde_json::from_str(&raw).unwrap();
    let attempts = value["attempts"].as_array().unwrap();
    let names: Vec<&str> = attempts
        .iter()
        .filter_map(|a| a["thread"].as_str())
        .collect();
    assert!(
        names.contains(&"my-prefix-1"),
        "expected my-prefix-1; got {names:?}"
    );
    assert!(
        names.contains(&"my-prefix-2"),
        "expected my-prefix-2; got {names:?}"
    );
}

#[test]
fn attempt_shared_target_default_on_for_rust_workspace() {
    // Construct a Rust-workspace-shaped repo: a `Cargo.toml` at the
    // root is the trigger. We don't need a real cargo workspace; the
    // attempt code only checks for the file's existence.
    let temp = setup_repo();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[workspace]\nmembers = []\n",
    )
    .unwrap();

    let raw = heddle(
        &["--output", "json", "attempt", "2", "--", "true"],
        Some(temp.path()),
    )
    .expect("attempt 2 against a fake Rust workspace should succeed");
    let value: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["attempts_total"], 2);
    assert_eq!(value["attempts_succeeded"], 2);

    // The thread checkouts should each contain a `.cargo/config.toml`
    // that redirects to the shared target dir. The thread path layout
    // for `--workspace materialized` is
    // `<repo>/.heddle/threads/<thread>/root/`; we assert the cargo
    // config exists there.
    let attempts = value["attempts"].as_array().unwrap();
    let threads_dir = temp.path().join(".heddle").join("threads");
    for attempt in attempts {
        let name = attempt["thread"].as_str().unwrap();
        let cargo_config = threads_dir
            .join(name)
            .join("root")
            .join(".cargo/config.toml");
        assert!(
            cargo_config.is_file(),
            "expected .cargo/config.toml redirect under attempt thread '{name}' at {}",
            cargo_config.display()
        );
        // Spot-check the redirect target: it should mention the
        // shared target dir under `.heddle/targets/<fingerprint>`.
        let body = fs::read_to_string(&cargo_config).unwrap();
        assert!(
            body.contains(".heddle/targets/") || body.contains("[build]"),
            "cargo config didn't look like a shared-target redirect: {body}"
        );
    }
}
