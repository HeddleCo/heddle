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

/// Identity priority: with no `--principal-*` flags, quickstart resolves
/// the identity already available in the user config and attributes the
/// capture to it — without writing a repo-level principal.
#[test]
fn quickstart_resolves_identity_from_user_config_without_flags() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    let out = heddle_output(
        &["init", "--quickstart", "--no-harness-install", "--yes"],
        Some(dir),
    )
    .unwrap();
    assert!(
        out.status.success(),
        "quickstart should resolve identity from user config: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let log: Value =
        serde_json::from_str(&heddle(&["log", "--output", "json"], Some(dir)).unwrap()).unwrap();
    let states = log["states"].as_array().expect("log emits a states array");
    assert_eq!(
        states[0]["principal"].as_str(),
        Some("Heddle Test <heddle@example.com>"),
        "capture is attributed to the seeded user-config identity: {log}"
    );
}

/// ESC-safety / fail-fast ordering: when no identity is resolvable and
/// there is no interactive terminal to prompt, quickstart must refuse
/// BEFORE any filesystem write — leaving no partial `.heddle/`.
#[test]
fn quickstart_identity_failfast_leaves_no_partial_heddle() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    git_hermetic(&["init"], dir);
    std::fs::write(dir.join("a.txt"), "hi").unwrap();
    git_hermetic(&["add", "."], dir);
    git_hermetic(&["commit", "-m", "initial"], dir);

    // `--yes` clears the confirmation gate so we isolate the identity
    // gate; no flags + no resolvable identity + non-interactive => bail.
    let out = heddle_output_with_env(
        &["init", "--quickstart", "--no-harness-install", "--yes"],
        Some(dir),
        &[
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("HEDDLE_PRINCIPAL_NAME", ""),
            ("HEDDLE_PRINCIPAL_EMAIL", ""),
        ],
    )
    .unwrap();

    assert!(
        !out.status.success(),
        "must refuse without a resolvable identity: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !dir.join(".heddle").exists(),
        "a declined/aborted preflight must leave NO partial .heddle/"
    );
}

/// `--quickstart-thread` names the thread the quickstart starts.
#[test]
fn quickstart_uses_custom_thread_name() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    heddle(
        &[
            "init",
            "--quickstart",
            "--quickstart-thread",
            "research",
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
    assert_eq!(
        status.get("thread").and_then(Value::as_str),
        Some("research"),
        "status surfaces the custom thread: {status}"
    );
}

/// When the directory already has capturable files, quickstart captures
/// them and does NOT write the `QUICKSTART.md` placeholder.
#[test]
fn quickstart_captures_existing_files_without_placeholder() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    std::fs::write(dir.join("notes.txt"), "my work\n").unwrap();

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
        !dir.join("QUICKSTART.md").exists(),
        "no placeholder when real files exist to capture"
    );
    let log: Value =
        serde_json::from_str(&heddle(&["log", "--output", "json"], Some(dir)).unwrap()).unwrap();
    assert_eq!(
        log["states"].as_array().map(Vec::len),
        Some(1),
        "exactly one capture: {log}"
    );
}

/// On a Git-overlay repo with history, quickstart imports the history,
/// captures, and lands a Git checkpoint commit.
#[test]
fn quickstart_checkpoints_on_git_overlay() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    git_hermetic(&["init"], dir);
    std::fs::write(dir.join("a.txt"), "hi\n").unwrap();
    git_hermetic(&["add", "."], dir);
    git_hermetic(&["commit", "-m", "initial"], dir);

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
        "quickstart should checkpoint on git-overlay: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Checkpoint:"),
        "git-overlay quickstart reports a checkpoint: {stdout}"
    );
}

/// Non-interactive `--install-harnesses` installs the named harness as a
/// post-write step (the decision is resolved up front, the install runs
/// after the repo exists).
#[test]
fn quickstart_installs_selected_harness() {
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
            "--install-harnesses",
            "claude-code",
            "--harness-install-scope",
            "repo",
            "--yes",
        ],
        Some(dir),
    )
    .unwrap();

    let settings = dir.join(".claude").join("settings.json");
    assert!(settings.is_file(), "claude-code integration was installed");
    let contents = std::fs::read_to_string(&settings).unwrap();
    assert!(
        contents.contains("integration relay claude-code"),
        "settings carry the relay hooks: {contents}"
    );
}

/// `--install-harnesses none` resolves to an empty selection and installs
/// nothing.
#[test]
fn quickstart_install_none_installs_nothing() {
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
            "--install-harnesses",
            "none",
            "--yes",
        ],
        Some(dir),
    )
    .unwrap();

    assert!(
        !dir.join(".claude").exists(),
        "selecting `none` installs no harness integration"
    );
}
