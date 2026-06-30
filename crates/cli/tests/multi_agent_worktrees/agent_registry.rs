// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn start_registers_thread_with_agent_metadata() {
    let main = setup_repo("base.txt", "base");

    let out = heddle(
        &[
            "--output",
            "json",
            "start",
            "feature/spawned",
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
    let v: Value = serde_json::from_str(&out).unwrap();

    assert_eq!(v["name"].as_str(), Some("feature/spawned"));
    assert!(v["message"].as_str().unwrap_or("").contains("Started"));

    let inspect: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/spawned"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(inspect["actor"]["provider"].as_str(), Some("anthropic"));
    assert_eq!(
        inspect["actor"]["model"].as_str(),
        Some("claude-sonnet-4-6")
    );
}

#[test]
fn thread_list_returns_all_started_threads() {
    let main = setup_repo("base.txt", "base");

    heddle(
        &["start", "feature/list-a", "--workspace", "auto"],
        Some(main.path()),
    )
    .unwrap();
    heddle(
        &["start", "feature/list-b", "--workspace", "auto"],
        Some(main.path()),
    )
    .unwrap();

    let out = heddle(&["--output", "json", "thread", "list"], Some(main.path())).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    let threads = v["threads"].as_array().unwrap();

    assert!(
        threads
            .iter()
            .any(|thread| thread["name"] == "feature/list-a")
    );
    assert!(
        threads
            .iter()
            .any(|thread| thread["name"] == "feature/list-b")
    );
}

#[test]
fn inspect_reflects_thread_provider_and_model() {
    let main = setup_repo("base.txt", "base");

    heddle(
        &[
            "start",
            "feature/attributed",
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

    let inspect: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/attributed"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();

    assert_eq!(inspect["actor"]["provider"].as_str(), Some("anthropic"));
    assert_eq!(
        inspect["actor"]["model"].as_str(),
        Some("claude-sonnet-4-6")
    );
}

#[test]
fn start_path_inherits_codex_probe_identity_into_actor_metadata() {
    let main = setup_repo("base.txt", "base");
    let work = TempDir::new().unwrap();

    let output = heddle_output_with_env(
        &[
            "--output",
            "json",
            "start",
            "feature/codex-probed",
            "--workspace",
            "materialized",
            "--path",
            work.path().to_str().unwrap(),
        ],
        Some(main.path()),
        &[
            ("CODEX_THREAD_ID", "thread-start-probe"),
            ("OPENAI_MODEL", "gpt-5.3-codex"),
            ("OPENAI_REASONING_EFFORT", "high"),
        ],
    )
    .expect("start with codex environment");
    assert!(
        output.status.success(),
        "start should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let started: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(started["name"].as_str(), Some("feature/codex-probed"));

    let actor: Value = serde_json::from_str(
        &heddle(&["--output", "json", "actor", "show"], Some(main.path())).unwrap(),
    )
    .unwrap();
    let actor_entry = &actor["actor"];
    assert_eq!(actor_entry["thread"].as_str(), Some("feature/codex-probed"));
    assert_eq!(actor_entry["harness"].as_str(), Some("codex"));
    assert_eq!(actor_entry["provider"].as_str(), Some("openai"));
    assert_eq!(actor_entry["model"].as_str(), Some("gpt-5.3-codex"));
    assert_eq!(actor_entry["thinking_level"].as_str(), Some("high"));
    assert_eq!(actor_entry["probe_source"].as_str(), Some("app_protocol"));

    let shown: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/codex-probed"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(shown["harness"].as_str(), Some("codex"));
    assert_eq!(shown["actor"]["provider"].as_str(), Some("openai"));
    assert_eq!(shown["actor"]["model"].as_str(), Some("gpt-5.3-codex"));
}

#[test]
fn actor_show_defaults_to_current_thread_actor() {
    let main = setup_repo("base.txt", "base");

    heddle(
        &[
            "start",
            "feature/current-actor",
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

    let actor: Value = inject_post_verification_at(
        main.path(),
        serde_json::from_str(
            &heddle(&["--output", "json", "actor", "show"], Some(main.path())).unwrap(),
        )
        .unwrap(),
    );

    let actor_entry = &actor["actor"];
    assert_eq!(
        actor_entry["thread"].as_str(),
        Some("feature/current-actor")
    );
    assert_eq!(actor_entry["provider"].as_str(), Some("anthropic"));
    assert_eq!(actor_entry["model"].as_str(), Some("claude-sonnet-4-6"));
    assert!(actor_entry["session_id"].as_str().is_some());
    assert!(actor["verification"].is_object());
}

#[test]
fn actor_explain_reports_attach_reason_for_current_actor() {
    let main = setup_repo("base.txt", "base");

    heddle(
        &[
            "start",
            "feature/explain-actor",
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

    let explained: Value = serde_json::from_str(
        &heddle(&["--output", "json", "actor", "explain"], Some(main.path())).unwrap(),
    )
    .unwrap();

    assert_eq!(explained["thread"].as_str(), Some("feature/explain-actor"));
    assert!(
        explained["attach_reason"]
            .as_str()
            .unwrap_or("")
            .contains("thread")
    );
}

#[test]
fn agent_task_create_list_show_update_round_trip() {
    let main = setup_repo("base.txt", "base");

    let created: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "task",
                "create",
                "--task-id",
                "task-cli-roundtrip",
                "--title",
                "Implement local task store",
                "--body",
                "Persist task provenance locally.",
                "--thread",
                "feature/task-roundtrip",
                "--allow-offline",
                "--delegated-by",
                "coordinator",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(created["output_kind"].as_str(), Some("agent_task_create"));
    assert_eq!(created["task"]["schema_version"].as_u64(), Some(1));
    assert_eq!(
        created["task"]["task_id"].as_str(),
        Some("task-cli-roundtrip")
    );
    assert_eq!(created["task"]["status"].as_str(), Some("open"));
    assert_eq!(created["task"]["allow_offline"].as_bool(), Some(true));

    let listed: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "task",
                "list",
                "--thread",
                "feature/task-roundtrip",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(listed["output_kind"].as_str(), Some("agent_task_list"));
    let tasks = listed["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["task_id"].as_str(), Some("task-cli-roundtrip"));

    let updated: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "task",
                "update",
                "task-cli-roundtrip",
                "--status",
                "complete",
                "--title",
                "Local task store complete",
                "--no-allow-offline",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(updated["output_kind"].as_str(), Some("agent_task_update"));
    assert_eq!(updated["task"]["status"].as_str(), Some("complete"));
    assert_eq!(
        updated["task"]["title"].as_str(),
        Some("Local task store complete")
    );
    assert_eq!(updated["task"]["allow_offline"].as_bool(), Some(false));
    assert!(updated["task"]["completed_at"].as_str().is_some());

    let shown: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "task",
                "show",
                "task-cli-roundtrip",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(shown["output_kind"].as_str(), Some("agent_task_show"));
    assert_eq!(shown["task"]["status"].as_str(), Some("complete"));
    assert_eq!(
        shown["task"]["body"].as_str(),
        Some("Persist task provenance locally.")
    );
}

#[test]
fn agent_reserve_records_task_assignment_id() {
    let main = setup_repo("base.txt", "base");
    heddle(
        &[
            "agent",
            "task",
            "create",
            "--task-id",
            "task-reserve-success",
            "--title",
            "Reserve task",
            "--thread",
            "feature/task-reserve-success",
        ],
        Some(main.path()),
    )
    .unwrap();

    let reserved: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "reserve",
                "--thread",
                "feature/task-reserve-success",
                "--task-id",
                "task-reserve-success",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();

    assert_eq!(
        reserved["reservation"]["task_assignment_id"].as_str(),
        Some("task-reserve-success")
    );
    assert_eq!(
        reserved["reservation"]["thread"].as_str(),
        Some("feature/task-reserve-success")
    );
}

#[test]
fn agent_reserve_rejects_unknown_task_id() {
    let main = setup_repo("base.txt", "base");

    let output = heddle_output(
        &[
            "--output",
            "json",
            "agent",
            "reserve",
            "--thread",
            "feature/missing-task",
            "--task-id",
            "task-does-not-exist",
        ],
        Some(main.path()),
    )
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("agent_task_not_found") || stderr.contains("task-does-not-exist"),
        "stderr should identify missing task: {stderr}"
    );
}

#[test]
fn agent_reserve_rejects_task_target_thread_mismatch() {
    let main = setup_repo("base.txt", "base");
    heddle(
        &[
            "agent",
            "task",
            "create",
            "--task-id",
            "task-thread-mismatch",
            "--title",
            "Wrong thread",
            "--thread",
            "feature/expected-thread",
        ],
        Some(main.path()),
    )
    .unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "agent",
            "reserve",
            "--thread",
            "feature/actual-thread",
            "--task-id",
            "task-thread-mismatch",
        ],
        Some(main.path()),
    )
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("agent_task_mismatch") || stderr.contains("feature/expected-thread"),
        "stderr should identify task/thread mismatch: {stderr}"
    );
}

#[test]
fn start_without_name_is_rejected() {
    let main = setup_repo("base.txt", "base");
    let result = heddle(&["start"], Some(main.path()));
    assert!(result.is_err(), "start without a thread name should fail");
}

#[test]
fn actor_spawn_no_thread_attaches_to_current_thread_without_minting() {
    let main = setup_repo("base.txt", "base");
    let current = head_track(main.path());
    assert!(
        !current.is_empty(),
        "repo should be on a thread after init + capture"
    );

    let before: Value = serde_json::from_str(
        &heddle(&["--output", "json", "thread", "list"], Some(main.path())).unwrap(),
    )
    .unwrap();
    let before_count = before["threads"].as_array().unwrap().len();

    let out = heddle(
        &[
            "--output",
            "json",
            "actor",
            "spawn",
            "--no-thread",
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet-4-6",
        ],
        Some(main.path()),
    )
    .expect("actor spawn --no-thread should succeed");
    let v: Value = serde_json::from_str(&out).unwrap();

    // The detected agent is attached to the current thread, not a fresh
    // `actor/<session>` thread.
    assert_eq!(
        v["actor"]["thread"].as_str(),
        Some(current.as_str()),
        "no-thread spawn should attach to the current thread: {out}"
    );
    assert!(
        !v["actor"]["thread"]
            .as_str()
            .unwrap_or("")
            .starts_with("actor/"),
        "no-thread spawn must not mint a stray actor/<session> thread: {out}"
    );
    assert_eq!(v["actor"]["provider"].as_str(), Some("anthropic"));
    assert_eq!(v["actor"]["model"].as_str(), Some("claude-sonnet-4-6"));

    // No new thread ref was created by the spawn.
    let after: Value = serde_json::from_str(
        &heddle(&["--output", "json", "thread", "list"], Some(main.path())).unwrap(),
    )
    .unwrap();
    let threads = after["threads"].as_array().unwrap();
    assert_eq!(
        threads.len(),
        before_count,
        "no-thread spawn must not add a thread: {}",
        serde_json::to_string(&after).unwrap()
    );
    assert!(
        threads
            .iter()
            .all(|thread| !thread["name"].as_str().unwrap_or("").starts_with("actor/")),
        "no actor/* thread should exist after --no-thread spawn: {}",
        serde_json::to_string(&after).unwrap()
    );
}

#[test]
fn actor_spawn_no_thread_conflicts_with_explicit_thread() {
    let main = setup_repo("base.txt", "base");

    // `--no-thread` and `--thread` are mutually exclusive: one attaches
    // to the current thread, the other targets a named thread.
    let err = heddle(
        &[
            "actor",
            "spawn",
            "--no-thread",
            "--thread",
            "main",
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet-4-6",
        ],
        Some(main.path()),
    )
    .expect_err("--no-thread with --thread should be rejected");
    assert!(
        err.contains("cannot be used with"),
        "clap should report the --no-thread/--thread conflict: {err}"
    );
}

#[test]
fn actor_spawn_no_thread_on_detached_head_fails_cleanly() {
    let main = setup_repo("base.txt", "base");
    // A second state so `goto HEAD~1` lands on a real prior change and
    // detaches HEAD (no current thread to attach an actor to).
    fs::write(main.path().join("base.txt"), "base updated").unwrap();
    heddle(&["capture", "-m", "second"], Some(main.path())).unwrap();
    heddle(&["switch", "HEAD~1"], Some(main.path())).unwrap();

    let before: Value = serde_json::from_str(
        &heddle(&["--output", "json", "thread", "list"], Some(main.path())).unwrap(),
    )
    .unwrap();
    let before_count = before["threads"].as_array().unwrap().len();

    let err = heddle(
        &[
            "actor",
            "spawn",
            "--no-thread",
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet-4-6",
        ],
        Some(main.path()),
    )
    .expect_err("--no-thread on detached HEAD should fail cleanly");
    assert!(
        err.contains("current thread"),
        "detached-HEAD spawn should explain there is no current thread to attach to: {err}"
    );

    // Clean failure: no actor thread minted, no registry side effects.
    let after: Value = serde_json::from_str(
        &heddle(&["--output", "json", "thread", "list"], Some(main.path())).unwrap(),
    )
    .unwrap();
    let threads = after["threads"].as_array().unwrap();
    assert_eq!(
        threads.len(),
        before_count,
        "failed --no-thread spawn must not add a thread: {}",
        serde_json::to_string(&after).unwrap()
    );
    assert!(
        threads
            .iter()
            .all(|thread| !thread["name"].as_str().unwrap_or("").starts_with("actor/")),
        "no actor/* thread should exist after a failed --no-thread spawn: {}",
        serde_json::to_string(&after).unwrap()
    );
}
