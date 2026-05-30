// SPDX-License-Identifier: Apache-2.0
//! `heddle init --quickstart` — first-run UX (heddle#231).

use super::*;

/// §5 sentinel from the spike (`docs/spikes/heddle-26-quickstart-ux.md`):
/// a fresh runner with no prior Heddle state reaches the success state
/// from a single, fully flag-driven command — no stdin.
#[test]
fn quickstart_sentinel_reaches_first_commit_from_one_command() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    let out = heddle_output(
        &[
            "init",
            "--quickstart",
            "--principal-name",
            "CI Sentinel",
            "--principal-email",
            "ci@example.invalid",
            "--no-harness-install",
            "--yes",
        ],
        Some(dir),
    )
    .unwrap();
    assert!(
        out.status.success(),
        "quickstart should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // init landed
    assert!(dir.join(".heddle").is_dir(), "init created .heddle");

    // a thread exists
    let status = status_json(dir);
    assert_eq!(
        status.get("thread").and_then(Value::as_str),
        Some("quickstart"),
        "status surfaces the quickstart thread: {status}"
    );

    // at least one user-visible capture whose message matches `quickstart`
    let log: Value =
        serde_json::from_str(&heddle(&["log", "--output", "json"], Some(dir)).unwrap()).unwrap();
    let states = log["states"].as_array().expect("log emits a states array");
    assert!(!states.is_empty(), "at least one capture: {log}");
    let intent = states[0]["intent"].as_str().unwrap_or_default();
    assert!(
        intent.contains("quickstart"),
        "first capture intent mentions quickstart: {intent:?}"
    );
    // identity from the flags must win over any ambient config
    assert_eq!(
        states[0]["principal"].as_str(),
        Some("CI Sentinel <ci@example.invalid>"),
        "capture is attributed to the flag identity: {log}"
    );
}

/// On an empty native directory, no capturable files exist yet, so
/// quickstart writes a root-level `QUICKSTART.md` pointer and captures
/// it (a `.heddle/`-nested placeholder would be dropped by the default
/// ignore list).
#[test]
fn quickstart_writes_placeholder_when_directory_is_empty() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    heddle(
        &[
            "init",
            "--quickstart",
            "--principal-name",
            "CI Sentinel",
            "--principal-email",
            "ci@example.invalid",
            "--no-harness-install",
            "--yes",
        ],
        Some(dir),
    )
    .unwrap();

    assert!(
        dir.join("QUICKSTART.md").is_file(),
        "quickstart wrote the root-level placeholder"
    );
    let log: Value =
        serde_json::from_str(&heddle(&["log", "--output", "json"], Some(dir)).unwrap()).unwrap();
    assert_eq!(
        log["states"].as_array().map(Vec::len),
        Some(1),
        "exactly one capture: {log}"
    );
}

/// `heddle status` on a freshly-`init`'d native repo whose log is empty
/// surfaces `heddle init --quickstart` in `recommended_action`.
#[test]
fn status_recommends_quickstart_on_empty_native_repo() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    heddle(&["init", "--no-harness-install"], Some(dir)).unwrap();

    let status = status_json(dir);
    assert_eq!(
        status["recommended_action"].as_str(),
        Some("heddle init --quickstart"),
        "fresh native repo recommends quickstart: {status}"
    );

    // Once quickstart has produced a capture, the log is no longer empty
    // and the recommendation steps aside.
    heddle(
        &[
            "init",
            "--quickstart",
            "--principal-name",
            "CI Sentinel",
            "--principal-email",
            "ci@example.invalid",
            "--no-harness-install",
            "--yes",
        ],
        Some(dir),
    )
    .unwrap();
    let status = status_json(dir);
    assert_ne!(
        status["recommended_action"].as_str(),
        Some("heddle init --quickstart"),
        "after quickstart the recommendation no longer fires: {status}"
    );
}

/// The confirmation gate refuses to act non-interactively on a directory
/// that already holds Heddle data unless `--yes` is passed.
#[test]
fn quickstart_confirmation_gate_blocks_existing_repo_without_yes() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    heddle(
        &[
            "init",
            "--quickstart",
            "--principal-name",
            "CI Sentinel",
            "--principal-email",
            "ci@example.invalid",
            "--no-harness-install",
            "--yes",
        ],
        Some(dir),
    )
    .unwrap();

    // Re-running without `--yes` in a non-interactive context must refuse.
    let out = heddle_output(
        &[
            "init",
            "--quickstart",
            "--principal-name",
            "CI Sentinel",
            "--principal-email",
            "ci@example.invalid",
            "--no-harness-install",
        ],
        Some(dir),
    )
    .unwrap();
    assert!(
        !out.status.success(),
        "must refuse without --yes on an existing repo"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("--yes"),
        "the refusal points at --yes: {combined}"
    );
}
