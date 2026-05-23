// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn start_registers_thread_with_agent_metadata() {
    let main = setup_repo("base.txt", "base");

    let out = heddle(
        &[
            "--json",
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
        &heddle(&["--json", "inspect", "feature/spawned"], Some(main.path())).unwrap(),
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

    let out = heddle(&["--json", "thread", "list"], Some(main.path())).unwrap();
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
            &["--json", "inspect", "feature/attributed"],
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

    let actor: Value =
        serde_json::from_str(&heddle(&["--json", "actor", "show"], Some(main.path())).unwrap())
            .unwrap();

    assert_eq!(actor["thread"].as_str(), Some("feature/current-actor"));
    assert_eq!(actor["provider"].as_str(), Some("anthropic"));
    assert_eq!(actor["model"].as_str(), Some("claude-sonnet-4-6"));
    assert!(actor["session_id"].as_str().is_some());
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

    let explained: Value =
        serde_json::from_str(&heddle(&["--json", "actor", "explain"], Some(main.path())).unwrap())
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
