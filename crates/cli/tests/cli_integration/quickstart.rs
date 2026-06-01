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

/// Codex r7 (cid 3329270261): the preflight used `gix::discover` to decide
/// `has_git`, which walks to an ANCESTOR Git checkout — so a native Heddle
/// repo nested inside an ancestor Git checkout was wrongly treated as a Git
/// overlay and refused for the ancestor's detached HEAD, even though
/// `Repository::open` keeps it native and runs no Git checkpoint. The preflight
/// now derives capability from the opened repo, so the nested-native quickstart
/// proceeds.
#[test]
fn quickstart_native_repo_nested_in_git_checkout_is_not_overlay_refused() {
    let outer = TempDir::new().unwrap();
    let inner = outer.path().join("project");
    std::fs::create_dir(&inner).unwrap();

    // Native Heddle repo created BEFORE any Git exists anywhere, so it is a
    // genuine NativeHeddle repo (no `.git` at its own root).
    heddle(&["init", "--no-harness-install"], Some(&inner)).unwrap();
    assert!(inner.join(".heddle").is_dir());

    // Now wrap it in an ancestor Git checkout with a DETACHED HEAD.
    git_hermetic(&["init"], outer.path());
    std::fs::write(outer.path().join("a.txt"), "hi\n").unwrap();
    git_hermetic(&["add", "."], outer.path());
    git_hermetic(&["commit", "-m", "initial"], outer.path());
    git_hermetic(&["checkout", "--detach"], outer.path());

    // The nested repo is native, so quickstart creates no Git checkpoint and
    // the ancestor's detached HEAD is irrelevant. It must NOT be refused.
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
        Some(&inner),
    )
    .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "a native repo nested in a detached-HEAD Git checkout must proceed: stdout={stdout} stderr={stderr}",
    );
    assert!(
        !stderr.contains("quickstart_detached_head"),
        "must NOT refuse with the Git-overlay detached-HEAD advice: {stderr}"
    );

    // It is initialized NATIVE — capability comes from the opened repo, not the
    // ancestor Git checkout — so no Git checkpoint is attempted.
    let init: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        init["repository_mode"].as_str(),
        Some("native-heddle"),
        "the nested repo stays native, not a Git overlay: {init}"
    );
    assert!(inner.join(".heddle").is_dir(), "native .heddle/ was created");
}

/// Codex r7 (cid 3329270259): in a materialized checkout whose `.heddle/`
/// objectstore points to a shared repo INSIDE a Git checkout, `resolve_principal`
/// can attribute the capture through `Repository::get_principal` →
/// `shared_checkout_parent_git_principal` (the shared dir's parent Git config).
/// The preflight's hand-rolled probe missed that source, refusing a runnable
/// quickstart. Delegating to the real `resolve_principal` over the opened repo
/// fixes it.
#[test]
fn quickstart_resolves_shared_checkout_parent_git_identity() {
    let main = TempDir::new().unwrap();
    let checkout = TempDir::new().unwrap();
    let empty_cfg = TempDir::new().unwrap();
    let empty_cfg_path = empty_cfg.path().join("user.toml");
    std::fs::write(&empty_cfg_path, "").unwrap();

    // Native main repo with a capture, so a thread can be materialized. NO
    // `[principal]` is written to the shared config — identity must come solely
    // from the shared dir's PARENT Git config below.
    heddle(&["init", "--no-harness-install"], Some(main.path())).unwrap();
    std::fs::write(main.path().join("file.txt"), "v1\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(main.path())).unwrap();

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

    // Make the shared repo's containing dir a Git checkout whose ONLY identity
    // is its Git `user.*` — reachable from the checkout solely via
    // `shared_checkout_parent_git_principal`.
    git_hermetic(&["init"], main.path());
    git_hermetic(&["config", "user.name", "Parent Git"], main.path());
    git_hermetic(
        &["config", "user.email", "parent@example.invalid"],
        main.path(),
    );

    // Quickstart in the checkout with NO other identity source: empty user
    // config + cleared principal env.
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
        "preflight must resolve identity from the shared dir's parent Git config: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let log: Value =
        serde_json::from_str(&heddle(&["log", "--output", "json"], Some(checkout.path())).unwrap())
            .unwrap();
    let states = log["states"].as_array().expect("log emits a states array");
    assert_eq!(
        states[0]["principal"].as_str(),
        Some("Parent Git <parent@example.invalid>"),
        "capture is attributed to the shared-checkout parent Git identity: {log}"
    );
}

/// Codex r7 (cid 3329270260): `refs/heads/<thread>` can be a syntactically
/// valid FULL ref even when `<thread>` is not a usable Git BRANCH name (e.g.
/// the reserved `HEAD`, or a leading `-`). Validating only the full ref let
/// such names pass preflight and fail at branch creation — after Heddle state
/// was written. The preflight now validates the branch shorthand too.
#[test]
fn quickstart_rejects_git_branch_invalid_shorthand_before_writes() {
    for bad in ["HEAD", "-leading"] {
        let temp = TempDir::new().unwrap();
        let dir = temp.path();
        git_hermetic(&["init"], dir);

        // `=` form so clap accepts a value that begins with `-`.
        let thread_arg = format!("--quickstart-thread={bad}");
        let out = heddle_output(
            &[
                "init",
                "--quickstart",
                &thread_arg,
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
            "'{bad}' is not a usable Git branch name and must be rejected: stdout={} stderr={stderr}",
            String::from_utf8_lossy(&out.stdout),
        );
        assert!(
            stderr.contains("quickstart_thread_name_invalid"),
            "'{bad}' must fail with the quickstart_thread_name_invalid advice: {stderr}"
        );
        assert!(
            !dir.join(".heddle").exists(),
            "fail-before-writes: '{bad}' must leave NO partial .heddle/"
        );
    }
}

/// Recursively snapshot every file under `dir` as `(relative_path, bytes)`,
/// sorted — a content fingerprint used to prove a refused quickstart performed
/// ZERO writes (no metadata so it can't flake on mtime/atime).
fn snapshot_tree(dir: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    fn walk(
        base: &std::path::Path,
        dir: &std::path::Path,
        out: &mut Vec<(std::path::PathBuf, Vec<u8>)>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => walk(base, &path, out),
                Ok(ft) if ft.is_file() => {
                    let rel = path.strip_prefix(base).unwrap().to_path_buf();
                    out.push((rel, std::fs::read(&path).unwrap_or_default()));
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Codex r8 (cid 3329342102 / 3329342201): run from a SUBDIRECTORY of an
/// existing NATIVE Heddle repo, quickstart used to `init_default(<cwd>)` — a
/// nested `.heddle/` at the subdir — because both the preflight and the write
/// path keyed on `<cwd>/.heddle`. With a single root resolved by ancestor
/// discovery, it must operate on the DISCOVERED repo, creating no nested repo.
#[test]
fn quickstart_from_subdir_targets_discovered_native_repo() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    heddle(&["init", "--no-harness-install"], Some(root)).unwrap();
    assert!(root.join(".heddle").is_dir(), "native repo created at the root");

    let sub = root.join("nested").join("deeper");
    std::fs::create_dir_all(&sub).unwrap();

    let out = heddle_output(
        &["init", "--quickstart", "--no-harness-install", "--yes"],
        Some(&sub),
    )
    .unwrap();
    assert!(
        out.status.success(),
        "quickstart from a subdir of a native repo must proceed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // The discovered repo is the target — NOT a nested repo at the subdir.
    assert!(
        !sub.join(".heddle").exists(),
        "must NOT create a nested .heddle/ at the subdirectory"
    );
    assert!(
        root.join(".heddle").is_dir(),
        "the discovered repo root stays the single repo"
    );
    // The quickstart ran against the discovered repo: it has the thread.
    let status = status_json(root);
    assert_eq!(
        status.get("thread").and_then(Value::as_str),
        Some("quickstart"),
        "the discovered repo carries the quickstart thread: {status}"
    );
    // The placeholder/capture landed at the discovered root, not the subdir.
    assert!(
        root.join("QUICKSTART.md").is_file(),
        "the capture wrote QUICKSTART.md at the discovered root"
    );
    assert!(
        !sub.join("QUICKSTART.md").exists(),
        "nothing was written into the subdirectory"
    );
}

/// Codex r8 (cid 3329342104 / 3329342205): run from a SUBDIRECTORY of a Git
/// checkout (no Heddle yet), quickstart used to `bootstrap_git_overlay(<cwd>)`
/// — a `.heddle/` at the subdir whose root has no `.git`, so it came up NATIVE
/// and never imported/checkpointed, while the preflight had classified it a Git
/// overlay (and could refuse on the ancestor's detached HEAD). It must now
/// bootstrap at the DISCOVERED Git root and run as a real Git overlay.
#[test]
fn quickstart_from_subdir_targets_discovered_git_root() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    git_hermetic(&["init"], root);
    std::fs::write(root.join("a.txt"), "hi\n").unwrap();
    git_hermetic(&["add", "."], root);
    git_hermetic(&["commit", "-m", "initial"], root);

    let sub = root.join("src").join("inner");
    std::fs::create_dir_all(&sub).unwrap();

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
        Some(&sub),
    )
    .unwrap();
    assert!(
        out.status.success(),
        "quickstart from a subdir of a Git checkout must proceed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Targets the discovered Git ROOT, not a nested repo at the subdir.
    assert!(
        !sub.join(".heddle").exists(),
        "must NOT create a nested .heddle/ at the subdirectory"
    );
    assert!(
        root.join(".heddle").is_dir(),
        "Heddle data is created at the discovered Git root"
    );

    // It runs as a real Git overlay (not the native repo the subdir bug made).
    let init: Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(
        init["repository_mode"].as_str(),
        Some("git-overlay"),
        "the discovered Git root is a Git overlay: {init}"
    );
    assert_eq!(
        init["git_detected"].as_bool(),
        Some(true),
        "git is detected for the discovered root: {init}"
    );

    // The checkpoint advanced a real branch — impossible if it had come up as a
    // native repo at the subdir. History (initial) plus the checkpoint commit.
    let grepo = gix::open(root).expect("open git repo at the discovered root");
    let tip = grepo.head_id().expect("HEAD resolves to a commit");
    let commit_count = grepo
        .rev_walk([tip.detach()])
        .all()
        .expect("rev-walk checkpoint history")
        .count();
    assert!(
        commit_count >= 2,
        "the Git-overlay quickstart imported history and added a checkpoint commit: count={commit_count}"
    );
}

/// Codex r8 (cid 3329342100 / 3329342200): the preflight used to
/// `Repository::open` the existing repo to read its capability/identity. For a
/// Git-overlay repo, `open` synchronizes `.heddle/HEAD` to Git's HEAD — a WRITE
/// that fired BEFORE the confirmation gate could refuse. A refused quickstart
/// (existing repo, non-interactive, no `--yes`) must now perform ZERO writes:
/// the preflight is fully read-only and never opens the repo.
#[test]
fn quickstart_refusal_on_existing_git_overlay_writes_nothing() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    git_hermetic(&["init"], dir);
    std::fs::write(dir.join("a.txt"), "hi\n").unwrap();
    git_hermetic(&["add", "."], dir);
    git_hermetic(&["commit", "-m", "initial"], dir);
    // Existing Heddle Git-overlay repo: `.heddle/HEAD` now exists and matches
    // Git's current branch.
    heddle(&["init", "--no-harness-install"], Some(dir)).unwrap();
    // Drift Git's HEAD away from Heddle's HEAD so that opening the repo WOULD
    // re-sync `.heddle/HEAD` (the exact write the old preflight performed).
    git_hermetic(&["switch", "-c", "feature"], dir);

    let heddle_dir = dir.join(".heddle");
    let before = snapshot_tree(&heddle_dir);

    // Non-interactive, no `--yes`: the existing-history confirmation gate must
    // refuse — before any write.
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
        "must refuse without --yes on an existing repo: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        stderr.contains("quickstart_needs_confirmation"),
        "the refusal is the confirmation gate: {stderr}"
    );

    let after = snapshot_tree(&heddle_dir);
    assert!(
        before == after,
        "a refused quickstart must not write to .heddle/ (HEAD-sync must not fire): \
         the preflight is read-only and must not open the repo"
    );
}

/// Codex r8 (cid 3329342103 / 3329342203): harness installs used to run BEFORE
/// the first capture, so `ensure_capturable_content` saw the generated
/// scaffolding (`.claude/settings.json`, …) as the user's content — skipping
/// the `QUICKSTART.md` placeholder and recording integration files as the first
/// state. Capture must run first: the first state is the user's content; the
/// install lands after and stays uncaptured.
#[test]
fn quickstart_captures_before_installing_harness() {
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
            "--install-harnesses",
            "claude-code",
            "--harness-install-scope",
            "repo",
            "--yes",
        ],
        Some(dir),
    )
    .unwrap();
    assert!(
        out.status.success(),
        "quickstart with a harness install must succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // The install ran (after the capture).
    assert!(
        dir.join(".claude").join("settings.json").is_file(),
        "the selected harness was installed"
    );
    // Capture-before-install: an EMPTY dir still got the QUICKSTART.md
    // placeholder. With install-first, `.claude/` would have made the dir look
    // non-empty and the placeholder would have been skipped.
    assert!(
        dir.join("QUICKSTART.md").is_file(),
        "the capture ran before the install, so the empty dir got QUICKSTART.md"
    );

    // Exactly one capture, and it recorded the user's content — not the harness
    // scaffolding, which remains UNTRACKED (installed after the capture).
    let log: Value =
        serde_json::from_str(&heddle(&["log", "--output", "json"], Some(dir)).unwrap()).unwrap();
    assert_eq!(
        log["states"].as_array().map(Vec::len),
        Some(1),
        "exactly one capture: {log}"
    );
    let status = status_json(dir);
    let added: Vec<String> = status["changes"]["added"]
        .as_array()
        .map(|paths| {
            paths
                .iter()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        added.iter().any(|p| p.contains(".claude")),
        "harness scaffolding is UNTRACKED — it was installed after the capture: {status}"
    );
    assert!(
        !added.iter().any(|p| p.contains("QUICKSTART.md")),
        "QUICKSTART.md was the captured first state, not untracked: {status}"
    );
}

/// Codex r7 (cid 3329270262): an invalid `--harness-install-scope` was not
/// parsed until the post-init install step, so a pure argument error left a
/// partially initialized repo. The pre-write decision now validates the scope
/// with the same `IntegrationScope::parse` the install uses.
#[test]
fn quickstart_rejects_invalid_harness_scope_before_writes() {
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
            "--install-harnesses",
            "claude-code",
            "--harness-install-scope",
            "bogus",
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
        "an invalid harness scope must be rejected before writes: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        stderr.contains("integration_scope_invalid"),
        "must fail with the integration_scope_invalid advice: {stderr}"
    );
    assert!(
        !dir.join(".heddle").exists(),
        "fail-before-writes: an invalid harness scope must leave NO partial .heddle/"
    );
    assert!(
        !dir.join(".claude").exists(),
        "no harness integration may be installed on a scope error"
    );
}

/// Codex r9 (cid 3329409818): `--install-harnesses codex` with the default
/// `--harness-install-scope repo` passed the generic-scope preflight (the enum
/// parses), but `install_codex` rejects any non-`user` scope — AFTER `.heddle/`,
/// `QUICKSTART.md`, and the capture/checkpoint were already written. The
/// preflight now mirrors each harness's OWN scope rule, so a scope a harness
/// will reject fails before any write.
#[test]
fn quickstart_rejects_codex_repo_scope_before_writes() {
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
            "--install-harnesses",
            "codex",
            "--harness-install-scope",
            "repo",
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
        "codex + repo scope must be rejected before writes: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        stderr.contains("integration_codex_scope_invalid"),
        "must fail with the harness-specific scope advice: {stderr}"
    );
    assert!(
        !dir.join(".heddle").exists(),
        "fail-before-writes: a harness/scope mismatch must leave NO partial .heddle/"
    );
    assert!(
        !dir.join("QUICKSTART.md").exists(),
        "fail-before-writes: no QUICKSTART.md placeholder may be written"
    );
}

/// Codex r9 (cid 3329409826): a shallow Git checkout (`.git/shallow`) is
/// rejected by `import_all` — but only AFTER `bootstrap_git_overlay` created
/// `.heddle/` and edited the Git excludes, leaving a half-initialized sidecar.
/// The preflight must detect the shallow clone and refuse before any write.
#[test]
fn quickstart_rejects_shallow_git_clone_before_writes() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    git_hermetic(&["init"], dir);
    std::fs::write(dir.join("a.txt"), "hi\n").unwrap();
    git_hermetic(&["add", "."], dir);
    git_hermetic(&["commit", "-m", "initial"], dir);
    // Mark the checkout shallow the way a `--depth` clone would: a `.git/shallow`
    // file listing the shallow boundary. `import_all` keys on its presence
    // (`git_dir()/shallow`), and so does the preflight's `git_is_shallow`.
    let grepo = gix::open(dir).unwrap();
    let head = grepo.head_id().unwrap().to_string();
    std::fs::write(dir.join(".git").join("shallow"), format!("{head}\n")).unwrap();

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
        "a shallow clone must be rejected before writes: stdout={} stderr={stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        stderr.contains("quickstart_shallow_clone"),
        "must fail with the quickstart_shallow_clone advice: {stderr}"
    );
    assert!(
        !dir.join(".heddle").exists(),
        "fail-before-writes: a shallow clone must leave NO partial .heddle/"
    );
    assert!(
        !dir.join("QUICKSTART.md").exists(),
        "fail-before-writes: no QUICKSTART.md placeholder may be written"
    );
}

/// Codex r9 (cid 3329409822): run inside a nested Git checkout below an ancestor
/// Heddle repo, quickstart used to return the ancestor `.heddle` root — but
/// `Repository::open` has a special case that bootstraps the NESTED Git root
/// instead. Quickstart would then write the thread into the parent and never
/// import the nested Git history. Target resolution now mirrors
/// `Repository::open`'s nested-Git-first walk, so the nested Git root is
/// bootstrapped and imported.
#[test]
fn quickstart_nested_git_below_ancestor_heddle_targets_nested_git() {
    let outer = TempDir::new().unwrap();
    let ancestor = outer.path();
    // Ancestor native Heddle repo (no Git at its own root).
    heddle(&["init", "--no-harness-install"], Some(ancestor)).unwrap();
    assert!(ancestor.join(".heddle").is_dir());

    // A nested Git checkout below it, with history and NO `.heddle` of its own.
    let nested = ancestor.join("nested");
    std::fs::create_dir(&nested).unwrap();
    git_hermetic(&["init"], &nested);
    std::fs::write(nested.join("n.txt"), "nested\n").unwrap();
    git_hermetic(&["add", "."], &nested);
    git_hermetic(&["commit", "-m", "nested initial"], &nested);

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
        Some(&nested),
    )
    .unwrap();
    assert!(
        out.status.success(),
        "quickstart in a nested Git checkout must bootstrap the nested root: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // The nested Git root is the target: it gets its OWN `.heddle/`, as a Git
    // overlay — NOT a write into the ancestor.
    assert!(
        nested.join(".heddle").is_dir(),
        "the nested Git root was bootstrapped with its own .heddle/"
    );
    let init: Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(
        init["repository_mode"].as_str(),
        Some("git-overlay"),
        "the nested Git root runs as a Git overlay: {init}"
    );
    // The nested quickstart carries the thread; the ancestor is untouched.
    let nested_status = status_json(&nested);
    assert_eq!(
        nested_status.get("thread").and_then(Value::as_str),
        Some("quickstart"),
        "the nested repo carries the quickstart thread: {nested_status}"
    );
    let ancestor_status = status_json(ancestor);
    assert_ne!(
        ancestor_status.get("thread").and_then(Value::as_str),
        Some("quickstart"),
        "the ancestor Heddle repo must NOT have been written into: {ancestor_status}"
    );
    // The nested Git history was imported AND checkpointed: the nested Git root
    // now has the original commit plus the checkpoint commit.
    let grepo = gix::open(&nested).expect("open nested git repo");
    let tip = grepo.head_id().expect("nested HEAD resolves");
    let commit_count = grepo
        .rev_walk([tip.detach()])
        .all()
        .expect("rev-walk nested history")
        .count();
    assert!(
        commit_count >= 2,
        "the nested Git history was imported and a checkpoint added: count={commit_count}"
    );
}

/// Codex r9 (cid 3329409824): in an existing Git-overlay repo, writing only
/// `.heddle/HEAD = quickstart` is not enough — `head_ref()` deliberately
/// resolves back to the live Git branch (`main`), so the capture/checkpoint
/// advanced `main` while the `quickstart` thread stayed at the imported tip,
/// even though the output said `Thread: quickstart`. Quickstart now attaches the
/// Git checkout to the thread's branch (the same write-through `thread switch`
/// uses), so the capture AND checkpoint land on `quickstart` — `main` does not
/// move.
#[test]
fn quickstart_git_overlay_capture_and_checkpoint_land_on_quickstart_thread() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    git_hermetic(&["init", "-b", "main"], dir);
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
        "quickstart on a Git overlay must succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // The active thread is `quickstart`, matching the reported output.
    let status = status_json(dir);
    assert_eq!(
        status.get("thread").and_then(Value::as_str),
        Some("quickstart"),
        "the active thread is quickstart: {status}"
    );

    let grepo = gix::open(dir).expect("open git repo");
    // `quickstart` advanced past the imported tip (initial + checkpoint = 2),
    // while `main` stayed at the imported tip (1 commit) — proof the capture and
    // checkpoint landed on the requested thread, not on main.
    let count_on = |branch: &str| -> usize {
        let mut reference = grepo
            .find_reference(branch)
            .unwrap_or_else(|_| panic!("ref {branch} exists"));
        let tip = reference.peel_to_id_in_place().expect("ref peels");
        grepo
            .rev_walk([tip.detach()])
            .all()
            .expect("rev-walk")
            .count()
    };
    assert_eq!(count_on("main"), 1, "main stayed at the imported tip");
    assert_eq!(
        count_on("quickstart"),
        2,
        "quickstart advanced: imported tip + the checkpoint commit"
    );
}
