// SPDX-License-Identifier: Apache-2.0
use super::*;

fn temp_leaf(temp: &RepoFixture) -> String {
    temp.path()
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo")
        .to_string()
}

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
        &heddle(
            &["--output", "json", "agent", "presence", "show"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let actor_entry = &actor["presence"];
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
            &heddle(
                &["--output", "json", "agent", "presence", "show"],
                Some(main.path()),
            )
            .unwrap(),
        )
        .unwrap(),
    );

    let actor_entry = &actor["presence"];
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
        &heddle(
            &["--output", "json", "agent", "presence", "explain"],
            Some(main.path()),
        )
        .unwrap(),
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
fn agent_fanout_plan_is_read_only_and_returns_start_commands() {
    let main = setup_repo("base.txt", "base");
    let lane_path = main
        .path()
        .with_file_name(format!("{}-fanout-plan-lane", temp_leaf(&main)));
    let lane_spec = format!(
        "feature/fanout-plan={}:Implement fanout plan lane",
        lane_path.display()
    );

    let planned: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "fanout",
                "plan",
                "--title",
                "Coordinate fanout",
                "--coordination-discussion-id",
                "discussion-123",
                "--lane",
                &lane_spec,
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();

    assert_eq!(planned["output_kind"].as_str(), Some("agent_fanout_plan"));
    assert_eq!(planned["parent_task"], Value::Null);
    assert_eq!(
        planned["coordination_discussion_id"].as_str(),
        Some("discussion-123")
    );
    assert_eq!(planned["lanes"][0]["status"].as_str(), Some("planned"));
    assert_eq!(
        planned["commands"][0]["argv"].as_array().unwrap()[1].as_str(),
        Some("agent")
    );
    assert_eq!(
        planned["commands"][0]["argv"].as_array().unwrap()[2].as_str(),
        Some("fanout")
    );
    assert_eq!(
        planned["commands"][0]["argv"].as_array().unwrap()[3].as_str(),
        Some("start")
    );
    assert!(
        !main.path().join(".heddle").join("agent-tasks").exists(),
        "plan must not create task records"
    );
    assert!(
        !lane_path.exists(),
        "plan must not materialize the lane checkout"
    );
}

#[test]
fn agent_fanout_start_preflights_all_lanes_before_creating_tasks() {
    let main = setup_repo("base.txt", "base");
    let first_lane_path = main
        .path()
        .with_file_name(format!("{}-fanout-preflight-first", temp_leaf(&main)));
    let blocked_lane_path = main
        .path()
        .with_file_name(format!("{}-fanout-preflight-blocked", temp_leaf(&main)));
    std::fs::create_dir_all(&blocked_lane_path).unwrap();
    std::fs::write(blocked_lane_path.join("already-here.txt"), "occupied").unwrap();

    let first_lane = format!(
        "feature/fanout-preflight-a={}:First lane",
        first_lane_path.display()
    );
    let blocked_lane = format!(
        "feature/fanout-preflight-b={}:Blocked lane",
        blocked_lane_path.display()
    );
    let output = heddle_output(
        &[
            "--output",
            "json",
            "agent",
            "fanout",
            "start",
            "--title",
            "Coordinate failing fanout",
            "--lane",
            &first_lane,
            "--lane",
            &blocked_lane,
        ],
        Some(main.path()),
    )
    .unwrap();

    assert!(
        !output.status.success(),
        "fanout start should fail before creating any lane; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !main.path().join(".heddle").join("agent-tasks").exists(),
        "failed fanout preflight must not create task records"
    );
    assert!(
        !first_lane_path.exists(),
        "failed fanout preflight must not materialize earlier lanes"
    );
}

#[test]
fn agent_fanout_start_rejects_duplicate_lane_threads_before_creating_tasks() {
    let main = setup_repo("base.txt", "base");
    let lane_path_a = main
        .path()
        .with_file_name(format!("{}-fanout-dup-a", temp_leaf(&main)));
    let lane_path_b = main
        .path()
        .with_file_name(format!("{}-fanout-dup-b", temp_leaf(&main)));
    let lane_a = format!(
        "feature/fanout-duplicate={}:First duplicate lane",
        lane_path_a.display()
    );
    let lane_b = format!(
        "feature/fanout-duplicate={}:Second duplicate lane",
        lane_path_b.display()
    );
    let output = heddle_output(
        &[
            "--output",
            "json",
            "agent",
            "fanout",
            "start",
            "--title",
            "Coordinate duplicate fanout",
            "--lane",
            &lane_a,
            "--lane",
            &lane_b,
        ],
        Some(main.path()),
    )
    .unwrap();

    assert!(!output.status.success());
    assert!(
        !main.path().join(".heddle").join("agent-tasks").exists(),
        "duplicate lane preflight must not create task records"
    );
    assert!(!lane_path_a.exists());
    assert!(!lane_path_b.exists());
}

#[test]
fn agent_fanout_start_creates_tasks_lanes_and_reservation_links() {
    let main = setup_repo("base.txt", "base");
    let lane_path = main
        .path()
        .with_file_name(format!("{}-fanout-start-lane", temp_leaf(&main)));
    let lane_spec = format!(
        "feature/fanout-start={}:Implement fanout start lane",
        lane_path.display()
    );

    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "fanout",
                "start",
                "--title",
                "Coordinate fanout start",
                "--coordination-discussion-id",
                "discussion-start",
                "--lane",
                &lane_spec,
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();

    assert_eq!(started["output_kind"].as_str(), Some("agent_fanout_start"));
    let parent_task_id = started["parent_task"]["task_id"]
        .as_str()
        .expect("parent task id");
    let child_task_id = started["lanes"][0]["task"]["task_id"]
        .as_str()
        .expect("child task id");
    assert_ne!(parent_task_id, child_task_id);
    assert_eq!(
        started["lanes"][0]["task"]["parent_task_id"].as_str(),
        Some(parent_task_id)
    );
    assert_eq!(
        started["lanes"][0]["task"]["coordination_discussion_id"].as_str(),
        Some("discussion-start")
    );
    let parent_body = started["parent_task"]["body"].as_str().unwrap_or("");
    assert!(parent_body.contains("feature/fanout-start"));
    assert!(parent_body.contains("Implement fanout start lane"));
    assert!(
        !parent_body.contains(&lane_path.display().to_string()),
        "parent task body should not persist checkout paths"
    );
    assert!(
        lane_path.join(".heddle").exists(),
        "fanout start should materialize a real lane checkout"
    );

    let listed: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "list",
                "--thread",
                "feature/fanout-start",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        listed["reservations"][0]["task_assignment_id"].as_str(),
        Some(child_task_id)
    );

    let shown: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/fanout-start"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(shown["parent_thread"].as_str(), Some("main"));
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
fn agent_task_correlation_surfaces_in_capture_thread_and_retro() {
    let main = setup_repo("base.txt", "base");
    let payload_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    heddle(
        &[
            "agent",
            "task",
            "create",
            "--task-id",
            "task-main-correlation",
            "--title",
            "Correlate agent work",
            "--thread",
            "main",
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
                "main",
                "--task-id",
                "task-main-correlation",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        reserved["reservation"]["task_assignment_id"].as_str(),
        Some("task-main-correlation")
    );

    heddle(
        &[
            "--output",
            "json",
            "timeline",
            "record-start",
            "--tool-call",
            "call-task-correlation",
            "--tool-name",
            "edit",
            "--summary",
            "safe timeline summary",
            "--payload-hash",
            payload_hash,
        ],
        Some(main.path()),
    )
    .unwrap();
    fs::write(main.path().join("private-secret-name.txt"), "changed\n").unwrap();
    let captured: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "capture", "-m", "correlated capture"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        captured["task_assignment_id"].as_str(),
        Some("task-main-correlation")
    );
    heddle(
        &[
            "--output",
            "json",
            "timeline",
            "record-finish",
            "--tool-call",
            "call-task-correlation",
            "--status",
            "succeeded",
            "--summary",
            "safe timeline finish",
            "--payload-hash",
            payload_hash,
        ],
        Some(main.path()),
    )
    .unwrap();

    let shown: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "main"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        shown["task_assignment_id"].as_str(),
        Some("task-main-correlation")
    );
    assert_eq!(
        shown["task_summary"]["title"].as_str(),
        Some("Correlate agent work")
    );

    let listed: Value = serde_json::from_str(
        &heddle(&["--output", "json", "thread", "list"], Some(main.path())).unwrap(),
    )
    .unwrap();
    let main_thread = listed["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["name"] == "main")
        .expect("main thread appears in thread list");
    assert_eq!(
        main_thread["task_assignment_id"].as_str(),
        Some("task-main-correlation")
    );
    assert_eq!(main_thread["task_summary"]["status"].as_str(), Some("open"));

    let retro: Value = serde_json::from_str(
        &heddle(&["--output", "json", "retro", "--full"], Some(main.path())).unwrap(),
    )
    .unwrap();
    assert!(
        retro["agent_tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["task_id"] == "task-main-correlation"),
        "retro should include the active task assignment: {retro}"
    );
    assert!(
        retro["timeline_steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|step| step["step_id"].as_str().is_some()
                && step["payload_summary"] == "safe timeline summary"),
        "retro should include scrubbed timeline steps: {retro}"
    );
    let retro_text = retro.to_string();
    assert!(
        !retro_text.contains("private-secret-name.txt"),
        "retro timeline/task correlation must not leak touched filenames: {retro_text}"
    );
}

#[test]
fn retro_defaults_scrub_task_text_and_skip_timeline_expansion() {
    let main = setup_repo("base.txt", "base");
    let payload_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    heddle(
        &[
            "agent",
            "task",
            "create",
            "--task-id",
            "task-retro-privacy",
            "--title",
            "Investigate private-secret-name.txt before release",
            "--thread",
            "main",
        ],
        Some(main.path()),
    )
    .unwrap();
    heddle(
        &[
            "--output",
            "json",
            "timeline",
            "record-start",
            "--tool-call",
            "call-retro-privacy",
            "--tool-name",
            "edit",
            "--summary",
            "Touched private-secret-name.txt with sensitive details",
            "--payload-hash",
            payload_hash,
        ],
        Some(main.path()),
    )
    .unwrap();

    let retro: Value =
        serde_json::from_str(&heddle(&["--output", "json", "retro"], Some(main.path())).unwrap())
            .unwrap();
    assert_eq!(
        retro["timeline_steps"].as_array().unwrap().len(),
        0,
        "default retro should not rebuild/expand timeline steps: {retro}"
    );
    let retro_text = retro.to_string();
    assert!(
        !retro_text.contains("private-secret-name.txt"),
        "default retro must scrub path-like task/timeline free text: {retro_text}"
    );
    assert!(
        retro_text.contains("[redacted-path]"),
        "default retro should leave a redaction marker for scrubbed task text: {retro_text}"
    );
}

#[test]
fn retro_fails_loudly_on_corrupt_task_metadata() {
    let main = setup_repo("base.txt", "base");
    let tasks_dir = main.path().join(".heddle").join("agent-tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("task-corrupt.toml"),
        "schema_version = [broken\n",
    )
    .unwrap();

    let output = heddle_output(&["--output", "json", "retro"], Some(main.path())).unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to list agent tasks")
            || stderr.contains("retro task correlation")
            || stderr.contains("schema_version"),
        "retro should identify corrupt task metadata: {stderr}"
    );
}

#[test]
fn retro_fails_loudly_on_corrupt_actor_presence_metadata() {
    let main = setup_repo("base.txt", "base");
    let agents_dir = main.path().join(".heddle").join("actor-presence");
    fs::create_dir_all(&agents_dir).unwrap();
    fs::write(
        agents_dir.join("agent-corrupt.toml"),
        "schema_version = [broken\n",
    )
    .unwrap();

    let output = heddle_output(&["--output", "json", "retro"], Some(main.path())).unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to list agent registry")
            || stderr.contains("failed to parse agent registry")
            || stderr.contains("schema_version"),
        "retro should identify corrupt agent registry metadata: {stderr}"
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
fn removed_actor_surface_is_rejected() {
    let main = setup_repo("base.txt", "base");
    let err = heddle(&["actor", "list"], Some(main.path()))
        .expect_err("the removed top-level actor surface must not parse");
    assert!(err.contains("unrecognized subcommand"), "{err}");
}
