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
            &["--output", "json", "inspect", "feature/spawned"],
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
            &["--output", "json", "inspect", "feature/attributed"],
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
fn start_without_name_is_rejected() {
    let main = setup_repo("base.txt", "base");
    let result = heddle(&["start"], Some(main.path()));
    assert!(result.is_err(), "start without a thread name should fail");
}
