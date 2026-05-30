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
        Some("heddle init --quickstart --yes"),
        "fresh native repo recommends the runnable quickstart command: {status}"
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

/// On an unborn Git overlay (a fresh `git init` with NO commits),
/// quickstart must produce exactly ONE capture + ONE checkpoint — the
/// promised single initial commit. Previously it fabricated a "Bootstrap
/// before quickstart" snapshot before `QUICKSTART.md` was written,
/// leaving an extra empty parent commit (Codex r3 cid 3328971408).
#[test]
fn quickstart_unborn_git_yields_single_capture_and_checkpoint() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    git_hermetic(&["init"], dir); // unborn: no commits yet

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
        "quickstart should succeed on an unborn Git repo: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Exactly one Heddle state: the bug left a "Bootstrap before
    // quickstart" parent in front of the real capture.
    let log: Value =
        serde_json::from_str(&heddle(&["log", "--output", "json"], Some(dir)).unwrap()).unwrap();
    let states = log["states"].as_array().expect("log emits a states array");
    assert_eq!(
        states.len(),
        1,
        "exactly one capture, no extra bootstrap parent: {log}"
    );
    let intent = states[0]["intent"].as_str().unwrap_or_default();
    assert!(
        intent.contains("quickstart"),
        "the single state is the quickstart capture, not a bootstrap: {intent:?}"
    );

    // And exactly one Git checkpoint commit (no extra bootstrap parent in
    // the exported history).
    let grepo = gix::open(dir).expect("open git repo");
    let tip = grepo
        .head_id()
        .expect("HEAD resolves to a checkpoint commit");
    let commit_count = grepo
        .rev_walk([tip.detach()])
        .all()
        .expect("rev-walk checkpoint history")
        .count();
    assert_eq!(
        commit_count, 1,
        "exactly one checkpoint commit, no extra bootstrap parent"
    );
}

/// The quickstart identity preflight must honor a repo-level
/// `.heddle/config.toml` `[principal]`: it outranks user and Git config
/// in `resolve_principal`, so a repo whose ONLY resolvable identity is
/// its repo-level principal must NOT be refused before it is opened
/// (Codex r3 cid 3328971410).
#[test]
fn quickstart_preflight_honors_repo_level_principal() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    // A Git repo so the test harness skips seeding a user-config identity;
    // combined with the cleared env below, the repo-level principal we pin
    // is the ONLY identity available.
    git_hermetic(&["init"], dir);

    let isolate: &[(&str, &str)] = &[
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("HEDDLE_PRINCIPAL_NAME", ""),
        ("HEDDLE_PRINCIPAL_EMAIL", ""),
    ];

    let init =
        heddle_output_with_env(&["init", "--no-harness-install"], Some(dir), isolate).unwrap();
    assert!(
        init.status.success(),
        "plain init should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr),
    );

    // Pin a repo-level principal as the sole identity, merged into the
    // config `init` already wrote (which carries the required
    // `[repository]` section).
    let cfg_path = dir.join(".heddle").join("config.toml");
    let mut cfg = repo::RepoConfig::load(&cfg_path).unwrap_or_default();
    cfg.set_principal("Repo Local", "repo@example.invalid");
    cfg.save(&cfg_path).unwrap();

    let out = heddle_output_with_env(
        &["init", "--quickstart", "--no-harness-install", "--yes"],
        Some(dir),
        isolate,
    )
    .unwrap();
    assert!(
        out.status.success(),
        "repo-level principal must satisfy the quickstart preflight: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let log: Value =
        serde_json::from_str(&heddle(&["log", "--output", "json"], Some(dir)).unwrap()).unwrap();
    let states = log["states"].as_array().expect("log emits a states array");
    assert_eq!(
        states[0]["principal"].as_str(),
        Some("Repo Local <repo@example.invalid>"),
        "capture is attributed to the repo-level principal: {log}"
    );
}

/// Codex r4 (cid 3329024451): Git's sentinel-default identity
/// (`user.name=Unknown` / `user.email=unknown@example.com`) is what
/// `resolve_principal`/`build_attribution` treat as UNCONFIGURED and
/// reject. The preflight must filter it the SAME way — a repo whose only
/// Git identity is the sentinel must fail BEFORE any `.heddle` write, not
/// pass the preflight and then have the capture refuse mid-write.
#[test]
fn quickstart_rejects_git_sentinel_identity_before_writes() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    git_hermetic(&["init"], dir);
    // Persist the sentinel "unconfigured" identity into the repo's local
    // Git config (the `-c` flags `git_hermetic` passes do NOT persist).
    git_hermetic(&["config", "user.name", "Unknown"], dir);
    git_hermetic(&["config", "user.email", "unknown@example.com"], dir);

    let out = heddle_output_with_env(
        &[
            "init",
            "--quickstart",
            "--no-harness-install",
            "--yes",
            "--output",
            "json",
        ],
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

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "sentinel-only Git identity must not satisfy the preflight: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        stderr.contains("quickstart_identity_required"),
        "must fail with the quickstart_identity_required advice: stderr={stderr}"
    );
    assert!(
        !dir.join(".heddle").exists(),
        "fail-before-writes: a sentinel-only identity must leave NO partial .heddle/"
    );
    assert!(
        !dir.join("QUICKSTART.md").exists(),
        "fail-before-writes: no QUICKSTART.md placeholder may be written"
    );
}

/// Codex r5 (cid 3329078130): the `Unknown <unknown@example.com>` sentinel
/// passed via `--principal-*` flags is what `build_attribution` rejects. The
/// preflight must reject it the SAME way — fail BEFORE any `.heddle`/
/// `QUICKSTART.md` write, not persist the sentinel and then have the capture
/// refuse mid-write.
#[test]
fn quickstart_rejects_sentinel_principal_flags_before_writes() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    let out = heddle_output(
        &[
            "init",
            "--quickstart",
            "--principal-name",
            "Unknown",
            "--principal-email",
            "unknown@example.com",
            "--no-harness-install",
            "--yes",
            "--output",
            "json",
        ],
        Some(dir),
    )
    .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "sentinel principal flags must be rejected: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        stderr.contains("quickstart_identity_required"),
        "must fail with the quickstart_identity_required advice: stderr={stderr}"
    );
    assert!(
        !dir.join(".heddle").exists(),
        "fail-before-writes: a sentinel identity must leave NO partial .heddle/"
    );
    assert!(
        !dir.join("QUICKSTART.md").exists(),
        "fail-before-writes: no QUICKSTART.md placeholder may be written"
    );
}

/// Codex r5 (cid 3329078132): a higher-precedence sentinel must SHADOW a
/// lower valid source, exactly as `resolve_principal` would. A repo-level
/// `[principal]` pinning the sentinel outranks a valid Git identity, so
/// `resolve_principal` stops at the sentinel and the capture is rejected —
/// the preflight must mirror that STOP-at-first-present precedence and fail
/// before writing the placeholder, NOT fall through to the valid Git source.
#[test]
fn quickstart_higher_precedence_sentinel_shadows_valid_lower_source() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    git_hermetic(&["init"], dir);
    // A VALID lower-precedence Git identity. The old fall-through preflight
    // would have accepted this and let the capture proceed, only for
    // `resolve_principal` (stopping at the repo-config sentinel) to reject it
    // after `.heddle` writes.
    git_hermetic(&["config", "user.name", "Git Valid"], dir);
    git_hermetic(&["config", "user.email", "valid@example.com"], dir);

    let isolate: &[(&str, &str)] = &[
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("HEDDLE_PRINCIPAL_NAME", ""),
        ("HEDDLE_PRINCIPAL_EMAIL", ""),
    ];

    // Plain init to create `.heddle`, then pin a sentinel repo-level
    // principal that OUTRANKS the valid Git identity.
    heddle_output_with_env(&["init", "--no-harness-install"], Some(dir), isolate).unwrap();
    let cfg_path = dir.join(".heddle").join("config.toml");
    let mut cfg = repo::RepoConfig::load(&cfg_path).unwrap_or_default();
    cfg.set_principal("Unknown", "unknown@example.com");
    cfg.save(&cfg_path).unwrap();

    let out = heddle_output_with_env(
        &[
            "init",
            "--quickstart",
            "--no-harness-install",
            "--yes",
            "--output",
            "json",
        ],
        Some(dir),
        isolate,
    )
    .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a higher-precedence sentinel must shadow the valid lower source and fail: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        stderr.contains("quickstart_identity_required"),
        "must fail with the quickstart_identity_required advice: stderr={stderr}"
    );
    assert!(
        !dir.join("QUICKSTART.md").exists(),
        "fail-before-writes: no QUICKSTART.md placeholder may be written"
    );
}

/// Genuine identity still proceeds: with no flags, no repo principal, and
/// no user config, a valid Git `user.*` identity (the lowest-precedence
/// source that the preflight's mirror of `resolve_principal` consults)
/// satisfies the preflight and attributes the capture.
#[test]
fn quickstart_resolves_genuine_git_identity_without_flags() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    git_hermetic(&["init"], dir);
    git_hermetic(&["config", "user.name", "Git Valid"], dir);
    git_hermetic(&["config", "user.email", "valid@example.com"], dir);

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
        out.status.success(),
        "a valid Git identity must satisfy the preflight: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let log: Value =
        serde_json::from_str(&heddle(&["log", "--output", "json"], Some(dir)).unwrap()).unwrap();
    let states = log["states"].as_array().expect("log emits a states array");
    assert_eq!(
        states[0]["principal"].as_str(),
        Some("Git Valid <valid@example.com>"),
        "capture is attributed to the genuine Git identity: {log}"
    );
}

/// Codex r5 (cid 3329078133): an invalid `--quickstart-thread` (a name the
/// ref machinery would reject, e.g. `a..b`) must fail in the preflight,
/// BEFORE init/bootstrap/import write any `.heddle` data — not leave a
/// half-initialized repo for a pure argument error.
#[test]
fn quickstart_rejects_invalid_thread_name_before_writes() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    let out = heddle_output(
        &[
            "init",
            "--quickstart",
            "--quickstart-thread",
            "a..b",
            "--principal-name",
            "CI Sentinel",
            "--principal-email",
            "ci@example.invalid",
            "--no-harness-install",
            "--yes",
            "--output",
            "json",
        ],
        Some(dir),
    )
    .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "an invalid thread name must be rejected: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        stderr.contains("quickstart_thread_name_invalid"),
        "must fail with the quickstart_thread_name_invalid advice: stderr={stderr}"
    );
    assert!(
        !dir.join(".heddle").exists(),
        "fail-before-writes: an invalid thread name must leave NO partial .heddle/"
    );
    assert!(
        !dir.join("QUICKSTART.md").exists(),
        "fail-before-writes: no QUICKSTART.md placeholder may be written"
    );
}

/// Codex r5 (cid 3329078135): re-running `--quickstart --yes` after switching
/// away from an existing quickstart thread must repoint that thread to the
/// CURRENT state before attaching, so the new capture's parent is the current
/// worktree's state — not the thread's stale tip. Previously the code skipped
/// repointing an existing thread and attached HEAD to the stale tip, recording
/// the wrong parent and corrupting history.
#[test]
fn quickstart_rerun_repoints_existing_noncurrent_thread() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    // First quickstart on the default "quickstart" thread → state A (root).
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

    // Move to a different thread and advance it to a new state B, so the
    // quickstart thread is no longer current and its tip A is stale.
    heddle(&["thread", "create", "work"], Some(dir)).unwrap();
    heddle(&["thread", "switch", "work"], Some(dir)).unwrap();
    std::fs::write(dir.join("work.txt"), "work\n").unwrap();
    heddle(&["capture", "-m", "work state"], Some(dir)).unwrap();
    let b_id = state_chain_ids(dir, 1)[0].clone();

    // Re-run quickstart while attached to `work`, with a fresh change so the
    // capture is a real new state C.
    std::fs::write(dir.join("more.txt"), "more\n").unwrap();
    let out = heddle_output(
        &["init", "--quickstart", "--no-harness-install", "--yes"],
        Some(dir),
    )
    .unwrap();
    assert!(
        out.status.success(),
        "rerun quickstart should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // HEAD is now the new quickstart capture C; its parent must be B (the
    // state we were on), NOT the stale quickstart tip A.
    let chain = state_chain_ids(dir, 2);
    assert_eq!(chain.len(), 2, "the new capture has a parent: {chain:?}");
    assert_eq!(
        chain[1], b_id,
        "new capture's parent is the current state B, not the stale quickstart tip: chain={chain:?} b={b_id}"
    );
}

/// Codex r6 (cid 3329175134): a detached Git HEAD has no branch for the
/// checkpoint to advance. The later `create_git_checkpoint` refuses it, but
/// only AFTER the import/capture have written `.heddle/`. The preflight must
/// refuse a detached HEAD BEFORE any write — leaving no partial `.heddle/`.
#[test]
fn quickstart_refuses_detached_git_head_before_writes() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    git_hermetic(&["init"], dir);
    std::fs::write(dir.join("a.txt"), "hi\n").unwrap();
    git_hermetic(&["add", "."], dir);
    git_hermetic(&["commit", "-m", "initial"], dir);
    git_hermetic(&["checkout", "--detach"], dir);

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
            "--output",
            "json",
        ],
        Some(dir),
    )
    .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a detached Git HEAD must be refused: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        stderr.contains("quickstart_detached_head"),
        "must fail with the quickstart_detached_head advice: {stderr}"
    );
    assert!(
        !dir.join(".heddle").exists(),
        "fail-before-writes: a detached HEAD must leave NO partial .heddle/"
    );
    assert!(
        !dir.join("QUICKSTART.md").exists(),
        "fail-before-writes: no QUICKSTART.md placeholder may be written"
    );
}

/// Codex r6 (cid 3329175135): a Git-overlay quickstart creates a real
/// `refs/heads/<name>`. Git's ref-name rules reject names Heddle's
/// `validate_ref_name` accepts (here a `~`), so such a name would pass
/// preflight and then fail when the branch is created — after `create_snapshot`
/// has written Heddle state. The preflight must validate against Git's rules
/// too so it fails before any write.
#[test]
fn quickstart_rejects_git_invalid_thread_name_before_writes() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    git_hermetic(&["init"], dir);

    let out = heddle_output(
        &[
            "init",
            "--quickstart",
            "--quickstart-thread",
            "bad~name",
            "--principal-name",
            "CI Sentinel",
            "--principal-email",
            "ci@example.invalid",
            "--no-harness-install",
            "--yes",
            "--output",
            "json",
        ],
        Some(dir),
    )
    .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a Git-invalid thread name must be rejected on a Git overlay: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        stderr.contains("quickstart_thread_name_invalid"),
        "must fail with the quickstart_thread_name_invalid advice: {stderr}"
    );
    assert!(
        !dir.join(".heddle").exists(),
        "fail-before-writes: a Git-invalid thread name must leave NO partial .heddle/"
    );
}

/// Codex r6 (cid 3329175137): in a materialized thread checkout the local
/// `.heddle/` holds only an objectstore pointer; the real `[principal]` lives
/// in the SHARED dir that `Repository::open` loads. The preflight must resolve
/// identity by following the pointer — not probe the local `config.toml` only,
/// which would wrongly report "no identity" and refuse a runnable quickstart.
#[test]
fn quickstart_resolves_principal_from_shared_dir_in_materialized_checkout() {
    let main = TempDir::new().unwrap();
    let checkout = TempDir::new().unwrap();
    let empty_cfg = TempDir::new().unwrap();
    let empty_cfg_path = empty_cfg.path().join("user.toml");
    std::fs::write(&empty_cfg_path, "").unwrap();

    // Native main repo with a capture, so a thread can be materialized.
    heddle(&["init", "--no-harness-install"], Some(main.path())).unwrap();
    std::fs::write(main.path().join("file.txt"), "v1\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(main.path())).unwrap();

    // Pin the ONLY identity into the SHARED `.heddle/config.toml`.
    let shared_cfg = main.path().join(".heddle").join("config.toml");
    let mut cfg = repo::RepoConfig::load(&shared_cfg).unwrap_or_default();
    cfg.set_principal("Shared Principal", "shared@example.invalid");
    cfg.save(&shared_cfg).unwrap();

    // Materialize a thread checkout whose `.heddle/` is an objectstore pointer.
    heddle(
        &[
            "start",
            "feature/mat",
            "--workspace",
            "materialized",
            "--path",
            checkout.path().to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .unwrap();
    assert!(
        checkout.path().join(".heddle").join("objectstore").is_file(),
        "checkout must be a materialized (objectstore-pointer) checkout"
    );

    // Quickstart in the checkout with NO other identity source: an empty user
    // config + cleared env. The ONLY resolvable identity is the shared
    // `[principal]`, reachable solely by following the objectstore pointer.
    let out = heddle_output_with_env(
        &["init", "--quickstart", "--no-harness-install", "--yes"],
        Some(checkout.path()),
        &[
            ("HEDDLE_CONFIG", empty_cfg_path.to_str().unwrap()),
            ("HEDDLE_PRINCIPAL_NAME", ""),
            ("HEDDLE_PRINCIPAL_EMAIL", ""),
        ],
    )
    .unwrap();
    assert!(
        out.status.success(),
        "a materialized checkout must resolve [principal] from the shared dir: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let log: Value =
        serde_json::from_str(&heddle(&["log", "--output", "json"], Some(checkout.path())).unwrap())
            .unwrap();
    let states = log["states"].as_array().expect("log emits a states array");
    assert_eq!(
        states[0]["principal"].as_str(),
        Some("Shared Principal <shared@example.invalid>"),
        "capture is attributed to the shared-dir principal: {log}"
    );
}

/// Codex r6 (cid 3329175136): when a repo's only `[output].format` is repo-level
/// `json`, the preflight (which computed format from CLI/user config only) used
/// to print a TEXT confirmation prompt before the final JSON render — mixing
/// formats. The preflight must load the repo's `[output].format` so the
/// confirmation output matches the final render.
#[test]
fn quickstart_confirmation_respects_repo_json_format() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    // Existing native repo whose ONLY format setting is repo-level json.
    heddle(&["init", "--no-harness-install"], Some(dir)).unwrap();
    let cfg_path = dir.join(".heddle").join("config.toml");
    let mut cfg = repo::RepoConfig::load(&cfg_path).unwrap_or_default();
    cfg.output.format = Some(repo::OutputFormat::Json);
    cfg.save(&cfg_path).unwrap();

    // Re-run quickstart WITHOUT --yes: the confirmation gate fires. With the
    // repo format honored, it must refuse with a clean JSON envelope and emit
    // NO human-readable text prompt to stdout.
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
        "the confirmation gate must refuse without --yes"
    );
    // The fix: with the repo format honored as json, the preflight must NOT
    // emit the human-readable text confirmation prompt (it would otherwise
    // mix with the JSON final render). Before the fix this warning block was
    // printed to stdout because the preflight computed format without the
    // repo config.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("would act on a directory that already has work"),
        "repo json format must suppress the text confirmation prompt on stdout: stdout={stdout}"
    );
    assert!(
        stdout.trim().is_empty(),
        "no stray text output when the repo format is json: stdout={stdout}"
    );
}

/// Codex r6 (cid 3329175139): a repo with commits on another ref but an unborn
/// current HEAD (e.g. after `git switch --orphan scratch`) must be treated as
/// having existing history — not history-free. History detection must key on
/// ANY local ref, so the existing-history confirmation gate fires (and, with
/// `--yes`, the history is imported) rather than skipping both.
#[test]
fn quickstart_treats_orphan_branch_repo_as_existing_history() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    git_hermetic(&["init"], dir);
    std::fs::write(dir.join("a.txt"), "hi\n").unwrap();
    git_hermetic(&["add", "."], dir);
    git_hermetic(&["commit", "-m", "initial"], dir);
    // Unborn current HEAD, but `main` still carries the commit.
    git_hermetic(&["switch", "--orphan", "scratch"], dir);

    // Without --yes: the existing-history confirmation gate must fire — proof
    // the orphan repo is treated as having history, not as empty.
    let out = heddle_output(
        &[
            "init",
            "--quickstart",
            "--principal-name",
            "CI Sentinel",
            "--principal-email",
            "ci@example.invalid",
            "--no-harness-install",
            "--output",
            "json",
        ],
        Some(dir),
    )
    .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "an orphan-branch repo must hit the existing-history confirmation gate: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        stderr.contains("quickstart_needs_confirmation"),
        "must be the existing-history confirmation refusal, not a history-free skip: {stderr}"
    );
}

/// Codex r6 (cid 3329175133): `resolve_principal` lets env win OUTRIGHT, so a
/// sentinel env identity shadows even valid `--principal-*` flags — the capture
/// would still be attributed to the env sentinel and rejected by
/// `build_attribution`, but only AFTER init has written `.heddle/`. The
/// preflight must reject the env sentinel BEFORE accepting lower-precedence
/// flags, leaving no partial state.
#[test]
fn quickstart_rejects_env_sentinel_even_with_valid_flags_before_writes() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    let out = heddle_output_with_env(
        &[
            "init",
            "--quickstart",
            "--principal-name",
            "Valid Name",
            "--principal-email",
            "valid@example.invalid",
            "--no-harness-install",
            "--yes",
            "--output",
            "json",
        ],
        Some(dir),
        &[
            ("HEDDLE_PRINCIPAL_NAME", "Unknown"),
            ("HEDDLE_PRINCIPAL_EMAIL", "unknown@example.com"),
        ],
    )
    .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a sentinel env identity must shadow valid flags and fail: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        stderr.contains("quickstart_identity_required"),
        "must fail with the quickstart_identity_required advice: {stderr}"
    );
    assert!(
        !dir.join(".heddle").exists(),
        "fail-before-writes: an env sentinel must leave NO partial .heddle/"
    );
    assert!(
        !dir.join("QUICKSTART.md").exists(),
        "fail-before-writes: no QUICKSTART.md placeholder may be written"
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
