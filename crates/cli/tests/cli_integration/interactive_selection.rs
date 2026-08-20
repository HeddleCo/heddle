use chrono::Utc;
use objects::{
    object::ThreadName,
    store::{ActorPresence, ActorPresenceStatus, ActorPresenceStore, AgentUsageSummary},
};
use refs::Head;
use repo::Repository;
use tempfile::TempDir;

use super::{heddle, heddle_output_with_stdin};

fn detached_repo_with_threads() -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let repo = Repository::open(temp.path()).unwrap();
    let state = repo.head().unwrap().expect("init creates a state");
    repo.refs()
        .set_thread(&ThreadName::new("alpha"), &state)
        .unwrap();
    repo.refs()
        .set_thread(&ThreadName::new("beta"), &state)
        .unwrap();
    repo.refs().write_head(&Head::Detached { state }).unwrap();
    temp
}

fn assert_non_tty_ambiguity(output: std::process::Output, kind: &str, command: &str) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(kind), "missing {kind}: {stderr}");
    assert!(stderr.contains(command), "missing {command}: {stderr}");
    assert!(!stderr.contains("Select one:"), "non-TTY prompt: {stderr}");
    assert!(!stderr.contains("Selection ["), "non-TTY prompt: {stderr}");
}

#[test]
fn piped_thread_ambiguity_never_prompts_or_consumes_a_selection() {
    let temp = detached_repo_with_threads();
    let output =
        heddle_output_with_stdin(&["--output", "json", "thread", "show"], temp.path(), "2\n")
            .unwrap();

    assert_non_tty_ambiguity(
        output,
        "ambiguous_thread_selection",
        "heddle thread show <THREAD>",
    );
}

#[test]
fn piped_remote_ambiguity_never_prompts_or_consumes_a_selection() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let repo = Repository::open(temp.path()).unwrap();
    let mut remotes = cli_shared::remote::RemoteConfig::open(&repo).unwrap();
    remotes
        .add(
            "alpha",
            cli_shared::remote::Remote {
                url: "file:///tmp/heddle-alpha".to_string(),
                insecure: false,
            },
        )
        .unwrap();
    remotes
        .add(
            "beta",
            cli_shared::remote::Remote {
                url: "file:///tmp/heddle-beta".to_string(),
                insecure: false,
            },
        )
        .unwrap();
    remotes.clear_default().unwrap();

    let output =
        heddle_output_with_stdin(&["--output", "json", "push"], temp.path(), "1\n").unwrap();
    assert_non_tty_ambiguity(output, "ambiguous_remote_selection", "heddle push <REMOTE>");
}

fn active_actor(session_id: &str, root: &std::path::Path) -> ActorPresence {
    ActorPresence {
        session_id: session_id.to_string(),
        client_instance_id: None,
        native_actor_key: None,
        native_parent_actor_key: None,
        native_instance_key: None,
        heddle_session_id: None,
        thread_id: None,
        thread: "main".to_string(),
        anchor_state: None,
        anchor_root: None,
        path: Some(root.to_path_buf()),
        base_state: "test-base".to_string(),
        started_at: Utc::now(),
        provider: Some("openai".to_string()),
        model: Some("gpt-test".to_string()),
        harness: Some("codex".to_string()),
        thinking_level: None,
        usage_summary: AgentUsageSummary::default(),
        last_progress_at: None,
        report_flush_state: None,
        attach_reason: Some("test fixture".to_string()),
        task_assignment_id: None,
        attach_precedence: vec![],
        winning_attach_rule: None,
        probe_source: None,
        probe_confidence: None,
        status: ActorPresenceStatus::Active,
        completed_at: None,
        context_queries: vec![],
    }
}

#[test]
fn piped_actor_ambiguity_never_prompts_or_consumes_a_selection() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let repo = Repository::open(temp.path()).unwrap();
    let actors = ActorPresenceStore::new(repo.heddle_dir());
    actors
        .save(&active_actor("agent-alpha", temp.path()))
        .unwrap();
    actors
        .save(&active_actor("agent-beta", temp.path()))
        .unwrap();

    let output = heddle_output_with_stdin(
        &["--output", "json", "presence", "show"],
        temp.path(),
        "1\n",
    )
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_non_tty_ambiguity(
        output,
        "ambiguous_actor_selection",
        "heddle presence show <session>",
    );
    let envelope: serde_json::Value = serde_json::from_str(
        stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or(stderr.trim()),
    )
    .unwrap_or_else(|err| panic!("JSON envelope: {err}\n{stderr}"));
    assert_eq!(
        envelope["primary_command"],
        "heddle presence show <session>"
    );
    assert_ne!(envelope["primary_command"], "heddle help --output json");
}

#[test]
fn piped_actor_completion_never_picks_a_destructive_target() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let repo = Repository::open(temp.path()).unwrap();
    let actors = ActorPresenceStore::new(repo.heddle_dir());
    actors
        .save(&active_actor("agent-alpha", temp.path()))
        .unwrap();
    actors
        .save(&active_actor("agent-beta", temp.path()))
        .unwrap();

    let output = heddle_output_with_stdin(
        &["--output", "json", "presence", "complete"],
        temp.path(),
        "1\n",
    )
    .unwrap();
    assert_non_tty_ambiguity(
        output,
        "ambiguous_actor_selection",
        "heddle presence complete --session <session>",
    );
    assert!(matches!(
        actors.load("agent-alpha").unwrap().unwrap().status,
        ActorPresenceStatus::Active
    ));
    assert!(matches!(
        actors.load("agent-beta").unwrap().unwrap().status,
        ActorPresenceStatus::Active
    ));
}
