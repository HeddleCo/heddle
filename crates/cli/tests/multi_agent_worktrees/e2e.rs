// SPDX-License-Identifier: Apache-2.0
use super::*;

fn expect_json_reserve_failure(args: &[&str], cwd: &std::path::Path) -> Value {
    let output = heddle_output(args, Some(cwd)).expect("invoke reserve failure");
    assert!(
        !output.status.success(),
        "reservation attempt should fail for args {args:?}"
    );
    let stdout = str::from_utf8(&output.stdout).unwrap_or("");
    assert!(
        stdout.trim().is_empty(),
        "JSON-mode reservation failures must not emit a success-shaped stdout object: {stdout}"
    );
    let stderr = str::from_utf8(&output.stderr).unwrap_or("");
    serde_json::from_str(stderr.trim()).expect("reservation failure should emit JSON envelope")
}

#[test]
fn thread_start_rejects_second_active_writer_for_same_thread() {
    let main = setup_repo("base.txt", "shared base");
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();

    heddle(
        &[
            "start",
            "feature/reserved",
            "--workspace",
            "materialized",
            "--path",
            first.path().to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .unwrap();

    let err = heddle(
        &[
            "start",
            "feature/reserved",
            "--workspace",
            "materialized",
            "--path",
            second.path().to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .expect_err("second active writer should be rejected");
    assert!(
        err.contains("already has an active reservation"),
        "thread reservation conflict should be explicit: {err}"
    );
}

#[test]
fn agent_api_reserve_heartbeat_release_round_trips_json() {
    let main = setup_repo("base.txt", "shared base");

    let reserved: Value = inject_post_verification_at(
        main.path(),
        serde_json::from_str(
            &heddle(
                &[
                    "agent",
                    "reserve",
                    "--thread",
                    "feature/api",
                    "--task",
                    "exercise stable API",
                ],
                Some(main.path()),
            )
            .unwrap(),
        )
        .unwrap(),
    );
    let reservation = &reserved["reservation"];
    let session = reservation["session_id"].as_str().unwrap().to_string();
    assert_eq!(reservation["thread"], "feature/api");
    assert!(reservation["reservation_token"].as_str().is_some());
    assert!(reserved["verification"].is_object());

    let heartbeat: Value = inject_post_verification_at(
        main.path(),
        serde_json::from_str(
            &heddle(
                &["agent", "heartbeat", "--session", &session],
                Some(main.path()),
            )
            .unwrap(),
        )
        .unwrap(),
    );
    assert_eq!(heartbeat["reservation"]["session_id"], session);
    assert!(heartbeat["verification"].is_object());

    let released: Value = inject_post_verification_at(
        main.path(),
        serde_json::from_str(
            &heddle(
                &[
                    "agent",
                    "release",
                    "--session",
                    &session,
                    "--status",
                    "complete",
                ],
                Some(main.path()),
            )
            .unwrap(),
        )
        .unwrap(),
    );
    assert_eq!(released["reservation"]["status"], "complete");
    assert!(released["verification"].is_object());

    let listed: Value = serde_json::from_str(
        &heddle(&["--output", "json", "agent", "list"], Some(main.path())).unwrap(),
    )
    .unwrap();
    assert_eq!(listed["reservations"].as_array().unwrap().len(), 1);
    assert!(listed["verification"].is_object());
}

/// `agent reserve` must distinguish three cases:
///   1. existing reservation owned by a *live* process at the same
///      anchor → `live_owner` JSON conflict
///   2. existing reservation owned by a *live* process at a *different*
///      anchor → `anchor_drift` JSON conflict
///   3. existing reservation whose owning process is *dead* → reaped,
///      new reservation succeeds
///
/// One-shot CLI invocations exit before the next call runs, so we
/// simulate a live owner by rewriting the entry's recorded pid to the
/// test-runner pid (always alive while the test is in progress).
#[test]
fn agent_api_reserve_emits_structured_live_owner_and_anchor_drift_conflicts() {
    let main = setup_repo("base.txt", "shared base");

    let reserved: Value = serde_json::from_str(
        &heddle(
            &[
                "agent",
                "reserve",
                "--thread",
                "feature/conflict",
                "--task",
                "first writer",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let reservation = &reserved["reservation"];
    let session = reservation["session_id"].as_str().unwrap().to_string();
    let anchor_state = reservation["anchor_state"].as_str().unwrap().to_string();

    // Pin the recorded pid to this test-runner process so the live-
    // owner branch fires reliably, and clear boot_id to keep the check
    // platform-independent (boot_id mismatch would otherwise reap on
    // hosts that expose one).
    let entry_path = main
        .path()
        .join(".heddle")
        .join("agents")
        .join(format!("{session}.toml"));
    let entry_text = std::fs::read_to_string(&entry_path).unwrap();
    // Pin pid to the test-runner process; leave boot_id alone so the
    // liveness check sees a live owner on the current boot.
    let lines: Vec<String> = entry_text
        .lines()
        .map(|line| {
            if line.starts_with("pid = ") {
                format!("pid = {}", std::process::id())
            } else {
                line.to_string()
            }
        })
        .collect();
    std::fs::write(&entry_path, lines.join("\n")).unwrap();

    // Same-anchor live owner → live_owner conflict.
    let live_conflict = expect_json_reserve_failure(
        &[
            "--output",
            "json",
            "agent",
            "reserve",
            "--thread",
            "feature/conflict",
        ],
        main.path(),
    );
    assert_eq!(live_conflict["kind"], "live_owner");
    assert!(
        live_conflict["error"]
            .as_str()
            .is_some_and(|error| error.contains(&session)),
        "live_owner conflict should name the existing session: {live_conflict}"
    );
    assert!(
        live_conflict["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle thread show feature/conflict")),
        "live_owner conflict should include a recovery hint: {live_conflict}"
    );

    // Different anchor while owner is alive → anchor_drift conflict.
    // We synthesize a divergent anchor by snapshotting on main while
    // the existing reservation still records the original anchor.
    std::fs::write(main.path().join("base.txt"), "advanced base").unwrap();
    heddle(
        &["capture", "-m", "advance main for drift test"],
        Some(main.path()),
    )
    .unwrap();

    let drift_conflict = expect_json_reserve_failure(
        &[
            "--output",
            "json",
            "agent",
            "reserve",
            "--thread",
            "feature/conflict",
        ],
        main.path(),
    );
    assert_eq!(drift_conflict["kind"], "anchor_drift");
    assert!(
        drift_conflict["error"]
            .as_str()
            .is_some_and(|error| error.contains(&anchor_state)),
        "anchor_drift conflict should expose reserved anchor: {drift_conflict}"
    );

    // Now mark the recorded pid as dead. The next reserve must reap
    // the abandoned entry and succeed.
    let entry_text = std::fs::read_to_string(&entry_path).unwrap();
    let lines: Vec<String> = entry_text
        .lines()
        .map(|line| {
            if line.starts_with("pid = ") {
                "pid = 2147483647".to_string() // 0x7fff_ffff is unassignable
            } else if line.starts_with("boot_id = ") {
                "boot_id = \"definitely-stale-boot\"".to_string()
            } else {
                line.to_string()
            }
        })
        .collect();
    std::fs::write(&entry_path, lines.join("\n")).unwrap();

    // Request the original anchor so the ref-side drift check doesn't
    // fire and we exercise the registry reap path cleanly.
    let reaped = heddle(
        &[
            "agent",
            "reserve",
            "--thread",
            "feature/conflict",
            "--anchor",
            &anchor_state,
        ],
        Some(main.path()),
    )
    .expect("dead-owner reservation must succeed after reap");
    let reaped: Value = serde_json::from_str(&reaped).unwrap();
    assert_ne!(
        reaped["reservation"]["session_id"], session,
        "reaped should mint new id"
    );
    assert_eq!(reaped["reservation"]["status"], "active");

    // Old entry should be marked Abandoned, not deleted.
    let stale = std::fs::read_to_string(&entry_path).unwrap();
    assert!(
        stale.contains("status = \"abandoned\""),
        "reaped entry must record abandoned status: {stale}"
    );
}

#[test]
fn agent_api_reserve_anchor_drift_without_owner_uses_error_envelope_only() {
    let main = setup_repo("base.txt", "shared base");

    let reserved: Value = serde_json::from_str(
        &heddle(
            &["agent", "reserve", "--thread", "feature/no-owner"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let session = reserved["reservation"]["session_id"].as_str().unwrap();
    let anchor_state = reserved["reservation"]["anchor_state"]
        .as_str()
        .unwrap()
        .to_string();

    heddle(
        &[
            "agent",
            "release",
            "--session",
            session,
            "--status",
            "complete",
        ],
        Some(main.path()),
    )
    .unwrap();

    std::fs::write(main.path().join("base.txt"), "advanced base").unwrap();
    heddle(
        &["capture", "-m", "advance main for ownerless drift test"],
        Some(main.path()),
    )
    .unwrap();

    let drift_conflict = expect_json_reserve_failure(
        &[
            "--output",
            "json",
            "agent",
            "reserve",
            "--thread",
            "feature/no-owner",
        ],
        main.path(),
    );
    assert_eq!(drift_conflict["kind"], "anchor_drift");
    assert!(
        drift_conflict["error"]
            .as_str()
            .is_some_and(|error| error.contains(&anchor_state)),
        "anchor_drift conflict should expose the existing anchor: {drift_conflict}"
    );
    assert!(
        drift_conflict["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("Refresh the thread")),
        "anchor_drift conflict should include a recovery hint: {drift_conflict}"
    );
}

/// `heddle agent reserve --hold-for-pid PID` binds the reservation's
/// liveness to an external process — typically the orchestrator that
/// wraps the heddle CLI. The contract this test protects:
///
///   1. While the held pid is alive, a second reservation attempt
///      sees a `live_owner` conflict (no recycling).
///   2. When the held pid dies, the reaper picks up the dead-pid
///      signal and the next reservation succeeds.
///
/// We simulate the orchestrator with a long-running `sleep` so we
/// have a real, killable pid; SIGKILL gives us the harshest possible
/// "process gone" signal (no Drop guards, no graceful release).
#[test]
fn agent_reserve_hold_for_pid_binds_reservation_to_external_process() {
    use std::process::Command;
    let main = setup_repo("base.txt", "shared base");

    // Spawn a long-running helper that stands in for the
    // orchestrator. `sleep 60` is portable and exits cleanly on
    // SIGKILL.
    let mut helper = Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawn sleep helper as fake orchestrator");
    let helper_pid = helper.id();

    // Reserve with the helper's pid as the liveness binding.
    let reserved: Value = serde_json::from_str(
        &heddle(
            &[
                "agent",
                "reserve",
                "--thread",
                "feature/held",
                "--hold-for-pid",
                &helper_pid.to_string(),
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let session = reserved["reservation"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // The recorded pid in the .toml entry should be the helper, not
    // the heddle CLI process (which has already exited by now).
    let entry_path = main
        .path()
        .join(".heddle")
        .join("agents")
        .join(format!("{session}.toml"));
    let entry_text = std::fs::read_to_string(&entry_path).unwrap();
    assert!(
        entry_text.contains(&format!("pid = {helper_pid}")),
        "reservation must record the held pid, not the CLI pid: {entry_text}"
    );

    // While the helper is alive: a second reserve attempt must see
    // the live owner.
    let live_conflict = expect_json_reserve_failure(
        &[
            "--output",
            "json",
            "agent",
            "reserve",
            "--thread",
            "feature/held",
        ],
        main.path(),
    );
    assert_eq!(live_conflict["kind"], "live_owner");
    assert!(
        live_conflict["error"]
            .as_str()
            .is_some_and(|error| error.contains(&session)),
        "live_owner conflict must point at the held session: {live_conflict}"
    );

    // Kill the helper and reap. SIGKILL is the harshest case — no
    // signal handler runs, no graceful release. The next reservation
    // attempt must succeed via the dead-pid path.
    helper.kill().expect("kill helper");
    let _ = helper.wait();

    let recovered: Value = serde_json::from_str(
        &heddle(
            &["agent", "reserve", "--thread", "feature/held"],
            Some(main.path()),
        )
        .expect("post-SIGKILL reservation must succeed via dead-pid reap"),
    )
    .unwrap();
    let new_session = recovered["reservation"]["session_id"].as_str().unwrap();
    assert_ne!(
        new_session, session,
        "post-reap reservation must mint a fresh session id"
    );
    assert_eq!(recovered["reservation"]["status"], "active");

    // The original entry should be marked Abandoned, not silently
    // overwritten.
    let stale = std::fs::read_to_string(&entry_path).unwrap();
    assert!(
        stale.contains("status = \"abandoned\""),
        "reaped held reservation must record abandoned status: {stale}"
    );
}

/// `heddle agent capture --session SID` and `heddle agent ready
/// --session SID` must verify the caller still owns an Active
/// reservation on the thread. Releasing the reservation should
/// cause subsequent calls to fail with a clear "no longer active"
/// error so an orchestrator can re-reserve before retrying.
#[test]
fn agent_api_capture_and_ready_require_active_session() {
    let main = setup_repo("base.txt", "shared base");

    // Reserve on the main thread so capture/ready have a live owner
    // pointing at a real anchor.
    let reserved_output = heddle_output_with_env(
        &["agent", "reserve", "--thread", "main"],
        Some(main.path()),
        &[
            ("CODEX_THREAD_ID", "thread-agent-api"),
            ("CODEX_MODEL", "gpt-5.3-codex"),
            ("CODEX_REASONING_EFFORT", "high"),
        ],
    )
    .expect("agent reserve with ambient harness env should run");
    assert!(
        reserved_output.status.success(),
        "agent reserve should succeed: {}",
        str::from_utf8(&reserved_output.stderr).unwrap_or("")
    );
    let reserved: Value = serde_json::from_slice(&reserved_output.stdout).unwrap();
    let session = reserved["reservation"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(reserved["reservation"]["harness"], "codex");
    assert_eq!(reserved["reservation"]["provider"], "openai");
    assert_eq!(reserved["reservation"]["model"], "gpt-5.3-codex");
    assert_eq!(reserved["reservation"]["thinking_level"], "high");
    assert_eq!(reserved["reservation"]["probe_source"], "app_protocol");

    // Live session: capture should succeed.
    fs::write(main.path().join("first.txt"), "first").unwrap();
    let capture: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "capture",
                "--session",
                &session,
                "-m",
                "first agent capture",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(capture["intent"], "first agent capture");
    assert!(capture["change_id"].as_str().unwrap().starts_with("hd-"));
    let log: Value = serde_json::from_str(
        &heddle(&["--output", "json", "log", "-n", "1"], Some(main.path())).unwrap(),
    )
    .unwrap();
    assert_eq!(
        log["states"][0]["agent"], "openai/gpt-5.3-codex",
        "agent capture should preserve the reservation's ambient harness model even when the capture process has no model env: {log}"
    );

    // Live session: ready should also succeed.
    let ready: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "agent", "ready", "--session", &session],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(ready["report"]["thread"], "main");

    // Release the reservation; capture and ready must now refuse.
    heddle(
        &[
            "agent",
            "release",
            "--session",
            &session,
            "--status",
            "complete",
        ],
        Some(main.path()),
    )
    .unwrap();

    let capture_err = heddle(
        &[
            "agent",
            "capture",
            "--session",
            &session,
            "-m",
            "after release",
        ],
        Some(main.path()),
    )
    .expect_err("capture after release must fail");
    assert!(
        capture_err.contains("no longer active"),
        "released session should refuse capture: {capture_err}"
    );

    let ready_err = heddle(
        &["agent", "ready", "--session", &session],
        Some(main.path()),
    )
    .expect_err("ready after release must fail");
    assert!(
        ready_err.contains("no longer active"),
        "released session should refuse ready: {ready_err}"
    );

    // Bogus session id likewise rejected.
    let bogus = heddle(
        &["agent", "capture", "--session", "agent-nope", "-m", "nope"],
        Some(main.path()),
    )
    .expect_err("bogus session id must fail");
    assert!(
        bogus.contains("not found"),
        "bogus session should report not found: {bogus}"
    );
}

#[test]
fn thread_captures_lists_granular_history_for_thread() {
    let main = setup_repo("base.txt", "shared base");
    let work = TempDir::new().unwrap();

    heddle(
        &[
            "start",
            "feature/captures",
            "--workspace",
            "materialized",
            "--path",
            work.path().to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .unwrap();
    fs::write(work.path().join("one.txt"), "one").unwrap();
    heddle(&["capture", "-m", "first granular turn"], Some(work.path())).unwrap();
    fs::write(work.path().join("two.txt"), "two").unwrap();
    heddle(
        &["capture", "-m", "second granular turn"],
        Some(work.path()),
    )
    .unwrap();

    let captures: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "thread",
                "captures",
                "feature/captures",
                "--limit",
                "5",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let captures = captures.as_array().unwrap();
    assert_eq!(captures.len(), 2);
    assert_eq!(captures[0]["message"], "second granular turn");
    assert_eq!(captures[1]["message"], "first granular turn");

    // W7b polish: each capture should expose a per-state diff
    // summary so callers don't have to walk parents themselves.
    // The second turn added one file (`two.txt`) over the first;
    // the first turn added one file (`one.txt`) over the seed.
    for capture in captures {
        let summary = capture
            .get("summary")
            .and_then(|s| s.as_object())
            .unwrap_or_else(|| panic!("missing diff summary on capture: {capture}"));
        assert_eq!(
            summary["added"].as_u64(),
            Some(1),
            "each granular turn added exactly one file: {capture}"
        );
        assert_eq!(summary["modified"].as_u64(), Some(0));
        assert_eq!(summary["deleted"].as_u64(), Some(0));
        assert_eq!(summary["total"].as_u64(), Some(1));
    }
}

/// Regression for the YC-demo prep finding: when an agent thread is
/// spawned with `start --agent-provider X --agent-model Y`, every
/// subsequent `heddle capture` from that thread's worktree must tag the
/// captured state with that agent. Before the fix, the thread's actor
/// only showed up in `heddle status` (read from `AgentRegistry`); the
/// capture handler never consulted it, so every state landed with
/// `attribution.agent = None` and `Principal: Unknown`. That broke the
/// "who/what wrote this line" provenance moment in the demo and left
/// `heddle blame --context` with nothing to surface.
#[test]
fn capture_inherits_agent_from_thread() {
    let main = setup_repo("base.txt", "shared base");
    let work = TempDir::new().unwrap();

    heddle(
        &[
            "start",
            "modulo",
            "--workspace",
            "materialized",
            "--path",
            work.path().to_str().unwrap(),
            "--agent-provider",
            "anthropic",
            "--agent-model",
            "claude-sonnet-4-5",
            "--task",
            "Add modulo",
        ],
        Some(main.path()),
    )
    .unwrap();

    fs::write(work.path().join("modulo.rs"), "pub fn modulo() {}").unwrap();
    heddle(
        &["capture", "--intent", "feat: add modulo"],
        Some(work.path()),
    )
    .unwrap();

    let log: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "log", "modulo", "-n", "1"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let head_state = &log["states"][0];
    assert_eq!(
        head_state["intent"].as_str().unwrap(),
        "feat: add modulo",
        "preflight: the captured state should be the head of the thread"
    );

    // `heddle --output json log` flattens the agent to "provider/model".
    // Before the fix this was null on every captured state; after the
    // fix it carries the thread's actor.
    let agent = head_state
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_eq!(
        agent, "anthropic/claude-sonnet-4-5",
        "captured state.agent must inherit thread's `--agent-provider/--agent-model` \
         (got {agent:?}, full state: {head_state})"
    );
}

#[test]
fn parallel_agents_visible_from_main_repo() {
    let main = setup_repo("base.txt", "shared base");
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();

    heddle(
        &[
            "start",
            "feature/auth",
            "--workspace",
            "materialized",
            "--path",
            dir_a.path().to_str().unwrap(),
            "--agent-provider",
            "anthropic",
            "--agent-model",
            "claude-sonnet-4-6",
        ],
        Some(main.path()),
    )
    .unwrap();
    heddle(
        &[
            "start",
            "feature/search",
            "--workspace",
            "materialized",
            "--path",
            dir_b.path().to_str().unwrap(),
            "--agent-provider",
            "anthropic",
            "--agent-model",
            "claude-sonnet-4-6",
        ],
        Some(main.path()),
    )
    .unwrap();

    fs::write(dir_a.path().join("auth.rs"), "auth impl").unwrap();
    fs::write(dir_b.path().join("search.rs"), "search impl").unwrap();

    heddle(&["capture", "-m", "implement auth"], Some(dir_a.path())).unwrap();
    heddle(&["capture", "-m", "implement search"], Some(dir_b.path())).unwrap();

    let auth_log: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "log", "feature/auth"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let search_log: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "log", "feature/search"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();

    assert_eq!(
        auth_log["states"][0]["intent"].as_str().unwrap(),
        "implement auth"
    );
    assert_eq!(
        search_log["states"][0]["intent"].as_str().unwrap(),
        "implement search"
    );

    let thread_list: Value = serde_json::from_str(
        &heddle(&["--output", "json", "thread", "list"], Some(main.path())).unwrap(),
    )
    .unwrap();
    let threads = thread_list["threads"].as_array().unwrap();
    assert!(
        threads
            .iter()
            .any(|thread| thread["name"] == "feature/auth")
    );
    assert!(
        threads
            .iter()
            .any(|thread| thread["name"] == "feature/search")
    );

    assert_eq!(head_track(main.path()), "main");
}

#[test]
fn merge_agent_track_into_main() {
    let main = setup_repo("base.txt", "base");
    let agent_tmp = TempDir::new().unwrap();

    heddle(
        &[
            "start",
            "feature/to-merge",
            "--workspace",
            "materialized",
            "--path",
            agent_tmp.path().to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .unwrap();

    fs::write(agent_tmp.path().join("added.txt"), "new feature").unwrap();
    heddle(&["capture", "-m", "add feature"], Some(agent_tmp.path())).unwrap();

    let result = heddle(&["merge", "feature/to-merge"], Some(main.path()));
    assert!(
        result.is_ok(),
        "merging agent thread into main should succeed: {:?}",
        result.err()
    );

    assert!(
        main.path().join("added.txt").exists(),
        "merged file should appear in main repo"
    );
}

#[test]
fn thread_start_creates_isolated_thread_and_aliases_work() {
    let main = setup_repo("base.txt", "base");

    let start_json = heddle(
        &[
            "--output",
            "json",
            "start",
            "feature/native-cli",
            "--workspace",
            "auto",
            "--agent-provider",
            "anthropic",
            "--agent-model",
            "claude-sonnet-4-6",
        ],
        Some(main.path()),
    )
    .unwrap();
    let started: Value = serde_json::from_str(&start_json).unwrap();
    let thread_path = started["execution_path"].as_str().unwrap();
    let thread = std::path::PathBuf::from(thread_path);
    // Auto-mode resolves to materialized on reflink-capable filesystems
    // (APFS, btrfs, xfs+reflink) and downgrades to solid on ext4 / HFS+ /
    // NTFS so the mode label reflects on-disk truth.
    let expected_mode = if objects::fs_clone::filesystem_supports_reflink(main.path()) {
        "materialized"
    } else {
        "solid"
    };
    assert_eq!(started["thread"]["thread_mode"], expected_mode);
    // Auto-mode threads materialize at a Heddle-managed path, surfaced
    // both as the user-visible `path` and the work-site `execution_path`.
    assert_eq!(started["path"], started["execution_path"]);

    assert!(
        thread.join(".heddle").is_dir(),
        "isolated thread should have .heddle pointer dir"
    );
    assert!(
        thread.join(".heddle").join("objectstore").is_file(),
        "isolated thread should have .heddle/objectstore pointer file"
    );
    assert!(
        thread.join(".heddle").join("HEAD").exists(),
        "isolated thread should have .heddle/HEAD file"
    );

    let thread_info: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "inspect", "feature/native-cli"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(thread_info["coordination_status"], "clean");
    assert_eq!(thread_info["path"], thread_path);
    assert_eq!(thread_info["execution_path"], thread_path);
    assert_eq!(thread_info["actor"]["provider"], "anthropic");
    assert_eq!(thread_info["thread_mode"], expected_mode);

    std::fs::write(thread.join("native.txt"), "heddle-native").unwrap();
    let capture_json = heddle(
        &[
            "--output",
            "json",
            "capture",
            "-m",
            "native thread snapshot",
        ],
        Some(&thread),
    )
    .unwrap();
    let captured: Value = serde_json::from_str(&capture_json).unwrap();
    assert_eq!(captured["intent"], "native thread snapshot");
    assert_eq!(captured["promotion_suggested"], false);

    let inspect_json = heddle(
        &["--output", "json", "inspect", "feature/native-cli"],
        Some(main.path()),
    )
    .unwrap();
    let inspected: Value = serde_json::from_str(&inspect_json).unwrap();
    assert_eq!(inspected["name"], "feature/native-cli");
    assert_eq!(inspected["coordination_status"], "ahead");

    let thread_show_json = heddle(
        &["--output", "json", "thread", "show", "feature/native-cli"],
        Some(main.path()),
    )
    .unwrap();
    let thread_show: Value = serde_json::from_str(&thread_show_json).unwrap();
    assert_eq!(thread_show["name"], "feature/native-cli");
    assert_eq!(thread_show["thread_mode"], expected_mode);
    assert_eq!(thread_show["thread_state"], "active");

    let status_json = heddle(&["--output", "json", "status"], Some(&thread)).unwrap();
    let status: Value = serde_json::from_str(&status_json).unwrap();
    assert_eq!(
        status["recommended_action"].as_str(),
        Some("heddle ready --thread feature/native-cli")
    );

    let ready_json = heddle(
        &[
            "--output",
            "json",
            "ready",
            "--thread",
            "feature/native-cli",
        ],
        Some(main.path()),
    )
    .unwrap();
    let ready: Value = serde_json::from_str(&ready_json).unwrap();
    assert_eq!(ready["thread_state"], "ready");
    assert_eq!(ready["report"]["merge_relation"], "fast_forward");
    assert_eq!(
        ready["report"]["recommended_action"],
        "heddle land --thread feature/native-cli --no-push"
    );

    let thread_show_json = heddle(
        &["--output", "json", "thread", "show", "feature/native-cli"],
        Some(main.path()),
    )
    .unwrap();
    let thread_show: Value = serde_json::from_str(&thread_show_json).unwrap();
    assert_eq!(thread_show["thread_state"], "ready");
    assert_eq!(
        thread_show["recommended_action"].as_str(),
        Some("heddle land --thread feature/native-cli --no-push")
    );

    let actor_list_json = heddle(&["--output", "json", "actor", "list"], Some(main.path()))
        .expect("actor list should succeed");
    let actor_list: Value = serde_json::from_str(&actor_list_json).unwrap();
    let actor_session = actor_list["actors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|actor| actor["thread"].as_str() == Some("feature/native-cli"))
        .and_then(|actor| actor["session_id"].as_str())
        .expect("feature actor should be registered");
    let actor_done_json = heddle(
        &[
            "--output",
            "json",
            "actor",
            "done",
            "--session",
            actor_session,
        ],
        Some(main.path()),
    )
    .expect("actor done should succeed");
    let actor_done: Value = serde_json::from_str(&actor_done_json).unwrap();
    assert_eq!(actor_done["coordination_status"], "merge-ready");
    assert_eq!(
        actor_done["recommended_action"], "heddle land --thread feature/native-cli --no-push",
        "actor completion should keep agents on the canonical land path: {actor_done}"
    );
    assert_eq!(
        actor_done["recommended_action_template"]["argv_template"],
        heddle_argv_json(["land", "--thread", "feature/native-cli", "--no-push"]),
        "{actor_done}"
    );
}

#[test]
fn ready_blocks_stale_or_heavy_impact_threads_and_status_reports_next_step() {
    let main = setup_repo("base.txt", "base");
    let start_json = heddle(
        &[
            "--output",
            "json",
            "start",
            "feature/dep",
            "--workspace",
            "auto",
            "--task",
            "update dependencies",
        ],
        Some(main.path()),
    )
    .unwrap();
    let started: Value = serde_json::from_str(&start_json).unwrap();
    let thread = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());

    fs::write(
        thread.join("Cargo.toml"),
        "[package]\nname='dep'\nversion='0.1.0'\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "touch deps"], Some(&thread)).unwrap();

    let ready_output = heddle_output(
        &["--output", "json", "ready", "--thread", "feature/dep"],
        Some(main.path()),
    )
    .unwrap();
    assert!(
        !ready_output.status.success(),
        "heavy-impact ready should fail closed"
    );
    let ready: Value = serde_json::from_slice(&ready_output.stdout).unwrap();
    assert_eq!(ready["thread_state"], "blocked");
    // No selected action serializes as null, never "" (HeddleCo/heddle#645
    // action-field contract).
    assert!(ready["report"]["recommended_action"].is_null());
    assert!(ready["recommended_action"].is_null());

    let reviewed: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "resolve", "feature/dep"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(reviewed["status"], "completed");
    assert_eq!(
        reviewed["message"].as_str(),
        Some("Thread manual review recorded")
    );
    assert!(
        reviewed["warnings"]
            .as_array()
            .is_some_and(|warnings| warnings.iter().any(|warning| warning
                .as_str()
                .unwrap_or_default()
                .contains("Heavy-impact change"))),
        "thread resolve should preserve what was manually reviewed: {reviewed}"
    );

    std::fs::write(main.path().join("base.txt"), "base changed").unwrap();
    heddle(&["capture", "-m", "main changed"], Some(main.path())).unwrap();

    let status_json = heddle(&["--output", "json", "status"], Some(&thread)).unwrap();
    let status: Value = serde_json::from_str(&status_json).unwrap();
    assert_eq!(status["thread_health"], "blocked");
    assert_eq!(
        status["recommended_action"].as_str(),
        Some("heddle sync --thread feature/dep")
    );

    let thread_refresh_status = heddle(
        &["--output", "json", "thread", "show", "feature/dep"],
        Some(main.path()),
    )
    .unwrap();
    let thread_show: Value = serde_json::from_str(&thread_refresh_status).unwrap();
    assert_eq!(thread_show["thread_state"], "blocked");
}

#[test]
fn genuine_blocked_thread_surfaces_coordination_axis_in_long_status() {
    // heddle#276 r3 / cid 3327990627. A *genuine* inter-thread block —
    // `heddle ready` failing closed persists `ThreadState::Blocked`, which
    // `build_thread_view` maps to `CoordinationStatus::Blocked` — must
    // surface on the coordination axis of the long status view. r2 masked
    // ANY Blocked whenever `thread_health` was non-clean (here it is
    // `blocked`), so the real coordination block was hidden as "work in
    // progress" and the verdict reason named only checkout health. The
    // provenance-keyed mask masks only the trust/health re-encoding, never
    // a genuine `build_thread_view` Blocked, so the block stays visible.
    let main = setup_repo("base.txt", "base");
    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/dep",
                "--workspace",
                "auto",
                "--task",
                "update dependencies",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let thread = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());

    fs::write(
        thread.join("Cargo.toml"),
        "[package]\nname='dep'\nversion='0.1.0'\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "touch deps"], Some(&thread)).unwrap();

    // Heavy-impact `ready` fails closed and persists ThreadState::Blocked.
    let ready_output = heddle_output(
        &["--output", "json", "ready", "--thread", "feature/dep"],
        Some(main.path()),
    )
    .unwrap();
    assert!(
        !ready_output.status.success(),
        "heavy-impact ready should fail closed and block the thread"
    );

    // Sanity: the worktree is still clean/verified, yet the thread is
    // genuinely Blocked — exactly the case r2 mis-masked.
    let status_json: Value =
        serde_json::from_str(&heddle(&["--output", "json", "status"], Some(&thread)).unwrap())
            .unwrap();
    assert_eq!(status_json["thread_state"], "blocked");
    assert_eq!(status_json["coordination_status"], "blocked");

    // Default long view: the verdict reason must NAME the coordination
    // block (r2 said only "checkout health needs attention").
    let text = heddle(&["--output", "text", "status"], Some(&thread)).unwrap();
    assert!(
        text.contains("thread coordination"),
        "default verdict reason must name the genuine coordination block, not hide it behind health: {text}"
    );

    // Verbose: the coordination axis must read "blocked", not the
    // health-only "work in progress" mask.
    let verbose = heddle(&["--output", "text", "-v", "status"], Some(&thread)).unwrap();
    assert!(
        verbose.contains("Coordination: blocked"),
        "a genuine inter-thread block must surface on the coordination axis: {verbose}"
    );
    assert!(
        !verbose.contains("Coordination: work in progress"),
        "a genuine inter-thread block must not be masked as work in progress: {verbose}"
    );
}

#[test]
fn sync_refreshes_stale_thread_when_replay_is_clean() {
    let main = setup_repo("base.txt", "base");
    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/sync-me",
                "--workspace",
                "auto",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let thread = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());

    std::fs::write(thread.join("feature.txt"), "feature work").unwrap();
    heddle(&["capture", "-m", "feature work"], Some(&thread)).unwrap();

    std::fs::write(main.path().join("base.txt"), "base updated").unwrap();
    heddle(&["capture", "-m", "advance main"], Some(main.path())).unwrap();

    let sync_json = heddle(
        &["--output", "json", "sync", "--thread", "feature/sync-me"],
        Some(main.path()),
    )
    .unwrap();
    let sync: Value = serde_json::from_str(&sync_json).unwrap();
    assert_eq!(sync["status"], "refreshed");
    assert_eq!(sync["chosen_path"], "refresh");

    let thread_show: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/sync-me"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(thread_show["freshness"], "current");
    assert_eq!(
        thread_show["integration_policy_result"]["status"],
        "current"
    );
}

#[test]
fn land_auto_captures_and_merges_clean_thread() {
    let main = setup_repo("base.txt", "base");
    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/land-it",
                "--workspace",
                "auto",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let thread = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());

    std::fs::write(thread.join("land.txt"), "land me").unwrap();

    let ship_json = heddle(
        &["--output", "json", "land", "--thread", "feature/land-it"],
        Some(main.path()),
    )
    .unwrap();
    let landed: Value = serde_json::from_str(&ship_json).unwrap();
    assert_eq!(landed["status"], "landed");
    assert_eq!(landed["captured"], true);
    assert_eq!(landed["integrated"], true);
    assert!(main.path().join("land.txt").exists());

    let thread_show: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/land-it"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(thread_show["thread_state"], "merged");
    assert_eq!(
        thread_show["integration_policy_result"]["status"],
        "auto_integrated"
    );

    let actor_show = heddle_output(&["--output", "json", "actor", "show"], Some(main.path()))
        .expect("invoke actor show after land");
    assert!(
        !actor_show.status.success(),
        "actor show should not select the merged actor implicitly after land"
    );
    let stderr = str::from_utf8(&actor_show.stderr).unwrap_or("");
    let envelope: Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|err| panic!("actor show failure should be JSON: {err}: {stderr}"));
    assert_eq!(envelope["kind"], "no_active_actor");
    assert_eq!(envelope["primary_command"], "heddle actor list");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("landed") && hint.contains("session id")),
        "actor show no-active advice should explain the post-land transition: {envelope}"
    );
}

/// `heddle delegate` with per-task `task:provider:model` syntax —
/// the YC-demo opener primitive. Three children, three different
/// agents, one command. Pre-extension, every child shared the same
/// `--agent-provider/--agent-model`, which made it impossible to race
/// distinct agents in a single invocation.
#[test]
fn delegate_assigns_per_task_agents_when_spec_includes_them() {
    let main = setup_repo("base.txt", "base");
    let parent_started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/race",
                "--workspace",
                "auto",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let parent_path = std::path::PathBuf::from(parent_started["execution_path"].as_str().unwrap());
    let workspace_root = TempDir::new().unwrap();

    let _delegate_json = heddle(
        &[
            "--output",
            "json",
            "delegate",
            "--parent",
            "feature/race",
            "--workspace",
            "materialized",
            "--path-prefix",
            workspace_root.path().to_str().unwrap(),
            "approach:anthropic:claude-sonnet-4-5",
            "approach:openai:gpt-5-codex",
            "approach:opencode:opencode-default",
        ],
        Some(&parent_path),
    )
    .unwrap();

    // Each child must end up with its OWN agent record, not the same
    // one. Verify by reading thread show for each child and asserting
    // its `actor` line carries the right provider/model.
    let triples = [
        ("approach-anthropic", "anthropic", "claude-sonnet-4-5"),
        ("approach-openai", "openai", "gpt-5-codex"),
        ("approach-opencode", "opencode", "opencode-default"),
    ];
    for (slug, expected_provider, expected_model) in triples {
        let full_name = format!("feature/race/{slug}");
        let show: Value = serde_json::from_str(
            &heddle(
                &["--output", "json", "thread", "show", &full_name],
                Some(main.path()),
            )
            .unwrap(),
        )
        .unwrap();
        // `thread show --output json` renders actor as { provider, model }.
        let actor = &show["actor"];
        assert_eq!(
            actor["provider"].as_str().unwrap_or(""),
            expected_provider,
            "{full_name}: provider mismatch (full show: {show})"
        );
        assert_eq!(
            actor["model"].as_str().unwrap_or(""),
            expected_model,
            "{full_name}: model mismatch (full show: {show})"
        );
    }

    // Also assert siblings see each other in the workspace view.
    let show_first: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "thread",
                "show",
                "feature/race/approach-anthropic",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let siblings = show_first["sibling_threads"].as_array().unwrap();
    let sibling_names: Vec<&str> = siblings.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        sibling_names.contains(&"feature/race/approach-openai"),
        "anthropic child should see openai sibling (got {sibling_names:?})"
    );
    assert!(
        sibling_names.contains(&"feature/race/approach-opencode"),
        "anthropic child should see opencode sibling (got {sibling_names:?})"
    );
}

#[test]
fn delegate_creates_child_threads_with_parent_relationship() {
    let main = setup_repo("base.txt", "base");
    let parent_started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/orchestrator",
                "--workspace",
                "auto",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let parent_thread =
        std::path::PathBuf::from(parent_started["execution_path"].as_str().unwrap());

    let delegate_json = heddle(
        &[
            "--output",
            "json",
            "delegate",
            "--parent",
            "feature/orchestrator",
            "parser",
            "tests",
        ],
        Some(&parent_thread),
    )
    .unwrap();
    let delegated: Value = serde_json::from_str(&delegate_json).unwrap();
    let children = delegated["delegated"].as_array().unwrap();
    assert_eq!(children.len(), 2);
    assert!(
        children
            .iter()
            .any(|child| child["name"] == "feature/orchestrator/parser")
    );
    assert!(
        children
            .iter()
            .any(|child| child["name"] == "feature/orchestrator/tests")
    );

    let parser_thread: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "thread",
                "show",
                "feature/orchestrator/parser",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(parser_thread["parent_thread"], "feature/orchestrator");
    assert_eq!(parser_thread["task"], "parser");
}

#[test]
fn undo_is_scoped_to_the_current_thread() {
    let main = setup_repo("base.txt", "shared base");

    let auth_thread: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/auth",
                "--workspace",
                "auto",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let search_thread: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/search",
                "--workspace",
                "auto",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();

    let auth_path = std::path::PathBuf::from(auth_thread["execution_path"].as_str().unwrap());
    let search_path = std::path::PathBuf::from(search_thread["execution_path"].as_str().unwrap());

    fs::write(auth_path.join("auth.rs"), "auth impl").unwrap();
    fs::write(search_path.join("search.rs"), "search impl").unwrap();

    let auth_snapshot: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "capture", "-m", "auth"],
            Some(&auth_path),
        )
        .unwrap(),
    )
    .unwrap();
    let search_snapshot: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "capture", "-m", "search"],
            Some(&search_path),
        )
        .unwrap(),
    )
    .unwrap();

    heddle(&["undo"], Some(&auth_path)).unwrap();

    assert!(
        !auth_path.join("auth.rs").exists(),
        "auth thread should rewind its own worktree"
    );
    assert!(
        search_path.join("search.rs").exists(),
        "search thread should keep its worktree state"
    );

    let auth_thread: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "inspect", "feature/auth"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let search_thread: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "inspect", "feature/search"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();

    assert_ne!(
        auth_thread["current_state"].as_str().unwrap(),
        auth_snapshot["change_id"].as_str().unwrap()
    );
    assert_eq!(
        search_thread["current_state"].as_str().unwrap(),
        search_snapshot["change_id"].as_str().unwrap()
    );

    heddle(&["undo", "--redo"], Some(&auth_path)).unwrap();
    assert!(
        auth_path.join("auth.rs").exists(),
        "redo should restore the auth thread state"
    );
}

#[test]
fn thread_and_workspace_json_match_dirty_current_checkout() {
    let main = setup_repo("base.txt", "base");
    let start_json = heddle(
        &[
            "--output",
            "json",
            "start",
            "feature/dirty-json",
            "--workspace",
            "auto",
        ],
        Some(main.path()),
    )
    .unwrap();
    let started: Value = serde_json::from_str(&start_json).unwrap();
    let thread = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    fs::write(thread.join("README.md"), "dirty before ready\n").unwrap();

    let threads: Value = serde_json::from_str(
        &heddle(&["--output", "json", "thread", "list"], Some(&thread)).unwrap(),
    )
    .unwrap();
    assert_eq!(threads["current"].as_str(), Some("feature/dirty-json"));
    let current_thread = threads["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["is_current"] == true)
        .expect("thread list should mark the current checkout");
    assert!(
        current_thread["changed_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path.as_str() == Some("README.md")),
        "thread list should include live dirty paths for the current checkout: {threads}"
    );

    let workspace: Value = serde_json::from_str(
        &heddle(&["--output", "json", "workspace", "show"], Some(&thread)).unwrap(),
    )
    .unwrap();
    assert_eq!(
        workspace["current_thread"].as_str(),
        Some("feature/dirty-json")
    );
    let current_workspace_thread = workspace["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["threads"].as_array().unwrap())
        .find(|thread| thread["is_current"] == true)
        .expect("workspace should mark the current checkout");
    assert!(
        current_workspace_thread["changed_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path.as_str() == Some("README.md")),
        "workspace should include live dirty paths for the current checkout: {workspace}"
    );
}

#[test]
fn lightweight_thread_capture_marks_heavy_impact_and_merge_preview_reports_it() {
    let main = setup_repo("base.txt", "base");

    let start_json = heddle(
        &[
            "--output",
            "json",
            "start",
            "feature/deps",
            "--workspace",
            "auto",
            "--task",
            "update dependencies",
        ],
        Some(main.path()),
    )
    .unwrap();
    let started: Value = serde_json::from_str(&start_json).unwrap();
    let thread = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());

    fs::write(
        thread.join("Cargo.toml"),
        "[package]\nname='demo'\nversion='0.2.0'\n",
    )
    .unwrap();
    let capture_json = heddle(
        &["--output", "json", "capture", "-m", "dependency update"],
        Some(&thread),
    )
    .unwrap();
    let captured: Value = serde_json::from_str(&capture_json).unwrap();
    assert_eq!(captured["promotion_suggested"], true);
    assert!(
        captured["heavy_impact_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value.as_str() == Some("Cargo.toml"))
    );

    let preview_json = heddle(
        &["--output", "json", "merge", "feature/deps", "--preview"],
        Some(main.path()),
    )
    .unwrap();
    let preview: Value = serde_json::from_str(&preview_json).unwrap();
    assert_eq!(preview["preview_only"], true);
    assert_eq!(preview["promotion_suggested"], true);
    assert_eq!(preview["heavy_impact_paths"][0], "Cargo.toml");
    assert_eq!(
        preview["recommended_action"].as_str(),
        None,
        "merge preview should not recommend a breadcrumb while heavy-impact review is still blocked: {preview}"
    );
}

#[test]
fn thread_promote_materializes_visible_checkout_without_changing_thread_identity() {
    let main = setup_repo("base.txt", "base");

    let start_json = heddle(
        &[
            "--output",
            "json",
            "start",
            "feature/promote",
            "--workspace",
            "auto",
            "--task",
            "prepare visible thread",
        ],
        Some(main.path()),
    )
    .unwrap();
    let started: Value = serde_json::from_str(&start_json).unwrap();
    let visible = TempDir::new().unwrap();

    let promote_json = heddle(
        &[
            "--output",
            "json",
            "thread",
            "promote",
            "feature/promote",
            "--path",
            visible.path().to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .unwrap();
    let promoted: Value = serde_json::from_str(&promote_json).unwrap();
    assert_eq!(promoted["thread"]["id"], "feature/promote");
    assert_eq!(promoted["thread"]["mode"], "solid");
    assert_eq!(
        promoted["thread"]["materialized_path"],
        visible.path().display().to_string()
    );
    assert!(visible.path().join(".heddle").is_dir());
    assert!(visible.path().join(".heddle").join("objectstore").is_file());
    assert!(visible.path().join(".heddle").join("HEAD").exists());
    assert_eq!(started["thread"]["name"], "feature/promote");
}

#[test]
fn status_watch_emits_initial_snapshot_for_local_repos() {
    let main = setup_repo("base.txt", "base");

    let output = heddle(
        &[
            "--output",
            "json",
            "status",
            "--watch",
            "--watch-iterations",
            "1",
            "--watch-interval-ms",
            "5",
        ],
        Some(main.path()),
    )
    .unwrap();
    let status: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(status["thread"], "main");
}

#[test]
fn status_watch_bounded_runs_are_transcript_friendly() {
    let main = setup_repo("base.txt", "base");

    let output = heddle(
        &[
            "--output",
            "text",
            "status",
            "--watch",
            "--watch-iterations",
            "1",
            "--watch-interval-ms",
            "5",
        ],
        Some(main.path()),
    )
    .unwrap();
    assert!(
        !output.contains("\x1B[2J") && !output.contains("\x1B[H"),
        "bounded watch output should not clear the screen in saved transcripts: {output:?}"
    );
    assert!(
        output.contains("Status snapshot 1 of 1"),
        "bounded watch output should identify the captured frame: {output}"
    );
}

#[test]
fn thread_show_watch_emits_initial_snapshot_for_local_repos() {
    let main = setup_repo("base.txt", "base");
    heddle(
        &["start", "feature/watch-thread", "--workspace", "auto"],
        Some(main.path()),
    )
    .unwrap();

    let output = heddle(
        &[
            "--output",
            "json",
            "thread",
            "show",
            "feature/watch-thread",
            "--watch",
            "--watch-iterations",
            "1",
            "--watch-interval-ms",
            "5",
        ],
        Some(main.path()),
    )
    .unwrap();
    let thread: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(thread["name"], "feature/watch-thread");
}

#[test]
fn workspace_show_groups_current_stacked_and_parallel_threads() {
    let main = setup_repo("base.txt", "base");
    let parent_started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/orchestrator",
                "--workspace",
                "auto",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let parent_path = std::path::PathBuf::from(parent_started["execution_path"].as_str().unwrap());
    heddle(
        &[
            "--output",
            "json",
            "delegate",
            "--parent",
            "feature/orchestrator",
            "parser",
        ],
        Some(&parent_path),
    )
    .unwrap();
    heddle(
        &["start", "feature/search", "--workspace", "auto"],
        Some(main.path()),
    )
    .unwrap();

    let output = heddle(
        &["--output", "json", "workspace", "show"],
        Some(&parent_path),
    )
    .unwrap();
    let workspace: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(workspace["current_thread"], "feature/orchestrator");
    let groups = workspace["groups"].as_array().unwrap();
    assert!(groups.iter().any(|group| {
        group["id"] == "stacked"
            && group["threads"]
                .as_array()
                .unwrap()
                .iter()
                .any(|thread| thread["name"] == "feature/orchestrator/parser")
    }));
    assert!(groups.iter().any(|group| {
        group["id"] == "parallel"
            && group["threads"]
                .as_array()
                .unwrap()
                .iter()
                .any(|thread| thread["name"] == "feature/search")
    }));
}

#[test]
fn capture_split_moves_selected_dirty_paths_into_target_thread() {
    let main = setup_repo("base.txt", "base");
    let source_started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/source",
                "--workspace",
                "auto",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let target_started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/target",
                "--workspace",
                "auto",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let source_path = std::path::PathBuf::from(source_started["execution_path"].as_str().unwrap());
    let target_path = std::path::PathBuf::from(target_started["execution_path"].as_str().unwrap());

    fs::write(source_path.join("auth.rs"), "auth impl").unwrap();
    fs::write(source_path.join("search.rs"), "search impl").unwrap();

    let split: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "capture",
                "--split",
                "--into",
                "feature/target",
                "--path",
                "auth.rs",
                "-m",
                "split auth",
            ],
            Some(&source_path),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(split["to_thread"], "feature/target");
    assert!(!source_path.join("auth.rs").exists());
    assert!(source_path.join("search.rs").exists());
    assert!(target_path.join("auth.rs").exists());
}

#[test]
fn thread_move_reassigns_selected_captured_paths_between_threads() {
    let main = setup_repo("base.txt", "base");
    let source_started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/source",
                "--workspace",
                "auto",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let target_started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/target",
                "--workspace",
                "auto",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let source_path = std::path::PathBuf::from(source_started["execution_path"].as_str().unwrap());
    let target_path = std::path::PathBuf::from(target_started["execution_path"].as_str().unwrap());

    fs::write(source_path.join("feature.rs"), "moved work").unwrap();
    heddle(&["capture", "-m", "source work"], Some(&source_path)).unwrap();

    let moved: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "thread",
                "move",
                "feature/source",
                "feature/target",
                "--path",
                "feature.rs",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(moved["from_thread"], "feature/source");
    assert_eq!(moved["to_thread"], "feature/target");
    assert!(!source_path.join("feature.rs").exists());
    assert!(target_path.join("feature.rs").exists());
}

#[test]
fn thread_absorb_merges_child_thread_into_parent_workspace() {
    let main = setup_repo("base.txt", "base");
    let parent_started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/orchestrator",
                "--workspace",
                "auto",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let parent_path = std::path::PathBuf::from(parent_started["execution_path"].as_str().unwrap());
    let delegate_json = heddle(
        &[
            "--output",
            "json",
            "delegate",
            "--parent",
            "feature/orchestrator",
            "parser",
        ],
        Some(&parent_path),
    )
    .unwrap();
    let delegated: Value = serde_json::from_str(&delegate_json).unwrap();
    let child_name = delegated["delegated"][0]["name"]
        .as_str()
        .unwrap()
        .to_string();
    let child_thread: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", &child_name],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let child_path = std::path::PathBuf::from(child_thread["execution_path"].as_str().unwrap());

    fs::write(child_path.join("parser.rs"), "parser impl").unwrap();
    heddle(&["capture", "-m", "parser work"], Some(&child_path)).unwrap();

    let absorbed: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "absorb", &child_name],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(absorbed["into"], "feature/orchestrator");
    assert!(parent_path.join("parser.rs").exists());
}

#[test]
fn thread_resolve_refreshes_clean_stale_threads() {
    let main = setup_repo("base.txt", "base");
    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/stale",
                "--workspace",
                "auto",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());

    std::fs::write(thread_path.join("feature.txt"), "feature work").unwrap();
    heddle(&["capture", "-m", "feature work"], Some(&thread_path)).unwrap();
    std::fs::write(main.path().join("base.txt"), "base updated").unwrap();
    heddle(&["capture", "-m", "advance main"], Some(main.path())).unwrap();

    let resolved: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "resolve", "feature/stale"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    // `thread resolve` reports `synced` for the clean-fast-forward path
    // it just executed; `thread show` below confirms the freshness flip.
    assert_eq!(resolved["status"], "synced");

    let thread_show: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/stale"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(thread_show["freshness"], "current");
}

/// Regression for the YC-demo finding: `heddle log <child-thread>` for
/// a thread spawned via `start` + `delegate` used to surface a phantom
/// state with `Principal: Unknown <unknown@example.com>` and no intent.
/// The phantom was the synthetic empty-tree genesis stamped by
/// `seed_default_thread` at `heddle init` time, before the user's
/// `.heddle/config.toml` principal was written.
///
/// After the fix:
/// - The seed state carries a stable `Heddle <init@heddle>` system
///   principal (never `Unknown`).
/// - User-facing `log` output filters the synthetic root entirely, so
///   every state shown to the user has a real principal.
///
/// This test mirrors the demo's flow: `init`, write `.heddle/config.toml`
/// with a principal, snapshot, start a parent thread, delegate a child,
/// snapshot the child, then walk every reachable thread's log.
#[test]
fn log_never_surfaces_unknown_principal_after_init() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    // The test invocation inherits the test helper's principal env
    // (`HEDDLE_PRINCIPAL_NAME` / `_EMAIL`), which takes precedence
    // over the synthetic Unknown fallback. The historical regression
    // this test pins was that the seed-root state stamped during
    // `init` carried `Unknown <unknown@example.com>` even when a
    // principal was available — verify every reachable log state
    // carries a real principal and never the Unknown fallback.
    let principal_name = "Heddle Test";
    let principal_email = "test@heddle.dev";

    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(
        &["capture", "-m", "Adam-authored initial commit"],
        Some(temp.path()),
    )
    .unwrap();

    heddle(
        &["start", "feature/parent", "--workspace", "auto"],
        Some(temp.path()),
    )
    .unwrap();

    // Walk every reachable thread's full log and assert no Unknown
    // principal — and, while we're here, no `Heddle <init@heddle>`
    // system principal leaks into user-facing output either.
    for thread in &["main", "feature/parent"] {
        let log_json: Value = serde_json::from_str(
            &heddle(
                &["--output", "json", "log", thread, "-n", "20"],
                Some(temp.path()),
            )
            .unwrap(),
        )
        .unwrap();
        let states = log_json["states"]
            .as_array()
            .unwrap_or_else(|| panic!("{thread} log missing states array"));
        assert!(
            !states.is_empty(),
            "{thread} should have at least one state in its log"
        );
        for state in states {
            let principal = state["principal"].as_str().unwrap_or("");
            assert!(
                !principal.contains("Unknown"),
                "every state on every thread must have a real principal — \
                 got: thread={thread}, state={state}"
            );
            assert!(
                !principal.contains("init@heddle"),
                "synthetic seed principal must be filtered from user-facing log — \
                 got: thread={thread}, state={state}"
            );
            assert!(
                principal.contains(principal_name) && principal.contains(principal_email),
                "every state on every thread must inherit the configured principal — \
                 got: thread={thread}, state={state}"
            );
        }
    }
}

// heddle#464 bug 1: when a materialized thread's recorded worktree dir is
// deleted out of band, `land --thread` refuses with `thread_worktree_missing`.
// The recovery used to point at `heddle start <thread> --path <path>`, which
// can never succeed (the thread still holds an active reservation, so `start`
// returns `active_thread_reservation`), and the JSON `recovery_commands` list
// was only the same `land` that just failed — a dead loop. The fix points the
// recovery at `heddle switch <thread>`, which rebuilds the dedicated worktree at
// the recorded path so the follow-up `land` succeeds.
#[test]
fn land_worktree_missing_recovery_points_at_switch_not_failing_loop() {
    let main = setup_repo("hello.txt", "hello world");

    let thread_dir = TempDir::new().unwrap();
    let thread_path = thread_dir.path();

    heddle(
        &[
            "start",
            "feature/gone",
            "--workspace",
            "materialized",
            "--path",
            thread_path.to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .expect("start materialized thread");

    // Capture some work inside the thread so it has a landable state.
    fs::write(thread_path.join("hello.txt"), "agent edits").unwrap();
    heddle(&["capture", "-m", "agent work"], Some(thread_path)).expect("capture in thread");

    // Delete the worktree out of band — the ref + record survive, only the
    // checkout dir is gone.
    fs::remove_dir_all(thread_path).expect("remove thread worktree dir");

    let output = heddle_output(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/gone",
            "--no-push",
        ],
        Some(main.path()),
    )
    .expect("land invocation runs");
    assert!(
        !output.status.success(),
        "land must refuse when the thread worktree is missing"
    );
    let stderr = str::from_utf8(&output.stderr).unwrap_or("");
    let envelope: Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|e| panic!("worktree-missing refusal must emit a JSON envelope: {e}\n{stderr}"));

    assert_eq!(envelope["kind"], "thread_worktree_missing");
    let primary = envelope["primary_command"].as_str().unwrap_or_default();
    assert_eq!(
        primary, "heddle switch feature/gone",
        "primary recovery must rematerialize the existing thread via switch"
    );

    let recovery: Vec<String> = envelope["recovery_commands"]
        .as_array()
        .expect("recovery_commands array present")
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        recovery.contains(&"heddle switch feature/gone".to_string()),
        "recovery_commands must include the rematerialize command: {recovery:?}"
    );
    let land_command = "heddle land --thread feature/gone --no-push".to_string();
    assert!(
        recovery != vec![land_command.clone()],
        "recovery_commands must not be just the failing land command (the old dead loop): {recovery:?}"
    );
    // The switch must come before the land retry so the operator rebuilds the
    // checkout first.
    let switch_idx = recovery.iter().position(|c| c == "heddle switch feature/gone");
    let land_idx = recovery.iter().position(|c| c == &land_command);
    if let (Some(s), Some(l)) = (switch_idx, land_idx) {
        assert!(s < l, "switch must precede the land retry: {recovery:?}");
    }
}
