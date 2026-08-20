// SPDX-License-Identifier: Apache-2.0

use refs::Head;
use repo::{Repository, ThreadIntegrationPolicy, ThreadManager, ThreadState};
use serde_json::Value;
use tempfile::TempDir;

use super::{heddle, heddle_output};

const BANNED_NEXT_ACTION_FRAGMENTS: &[&str] = &["heddle thread refresh", "heddle thread resolve"];

fn json(args: &[&str], cwd: &std::path::Path) -> Value {
    let output = heddle_output(args, Some(cwd))
        .unwrap_or_else(|err| panic!("`heddle {}` should run: {err}", args.join(" ")));
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|err| {
        panic!(
            "`heddle {}` should emit JSON: {err}\nstdout: {stdout}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn setup_native_repo() -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();
    temp
}

fn setup_managed_thread(name: &str) -> (TempDir, TempDir, String) {
    let main = setup_native_repo();
    let checkout = TempDir::new().unwrap();
    let checkout_arg = checkout.path().join("work");
    let started = json(
        &[
            "--output",
            "json",
            "start",
            name,
            "--path",
            checkout_arg.to_str().unwrap(),
        ],
        main.path(),
    );
    let execution_path = started["execution_path"]
        .as_str()
        .expect("start should report execution_path")
        .to_string();
    (main, checkout, execution_path)
}

fn setup_current_blocked_thread(name: &str) -> (TempDir, TempDir, String) {
    let (main, checkout_owner, execution_path) = setup_managed_thread(name);
    let checkout = std::path::Path::new(&execution_path);
    std::fs::write(checkout.join("feature.txt"), "feature\n").unwrap();
    heddle(&["capture", "-m", "feature"], Some(checkout)).unwrap();

    let repo = Repository::open(main.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());
    let mut thread = manager
        .load(name)
        .unwrap()
        .expect("managed thread should have a record");
    thread.state = ThreadState::Blocked;
    thread.current_state = Some(thread.base_state.clone());
    thread.integration_policy_result = ThreadIntegrationPolicy {
        status: Some("blocked".to_string()),
        reason: Some("Thread needs attention before integration".to_string()),
        ..ThreadIntegrationPolicy::default()
    };
    manager.save(&thread).unwrap();

    (main, checkout_owner, execution_path)
}

fn assert_no_banned_next_actions(value: &Value) {
    fn walk(value: &Value, path: &str) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let child_path = format!("{path}.{key}");
                    if matches!(key.as_str(), "next_action" | "recommended_action")
                        && let Some(action) = child.as_str()
                    {
                        for banned in BANNED_NEXT_ACTION_FRAGMENTS {
                            assert!(
                                !action.starts_with(banned),
                                "{child_path} used banned next-action `{action}`"
                            );
                        }
                    }
                    walk(child, &child_path);
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    walk(child, &format!("{path}[{index}]"));
                }
            }
            _ => {}
        }
    }
    walk(value, "$");
}

#[test]
fn native_dirty_status_and_thread_list_suggest_capture() {
    let repo = setup_native_repo();
    std::fs::write(repo.path().join("dirty.txt"), "dirty\n").unwrap();

    let status = json(&["--output", "json", "status"], repo.path());
    assert_eq!(status["recommended_action"], "heddle capture -m \"...\"");
    assert_no_banned_next_actions(&status);

    let threads = json(&["--output", "json", "thread", "list"], repo.path());
    assert_no_banned_next_actions(&threads);
}

#[test]
fn dirty_isolated_checkout_suggests_capture() {
    let (_main, checkout_owner, execution_path) = setup_managed_thread("feature/dirty");
    let checkout = std::path::Path::new(&execution_path);
    std::fs::write(checkout.join("dirty.txt"), "dirty\n").unwrap();

    let status = json(&["--output", "json", "status"], checkout);
    assert_eq!(status["recommended_action"], "heddle capture -m \"...\"");
    assert_no_banned_next_actions(&status);

    drop(checkout_owner);
}

#[test]
fn ready_thread_surfaces_land_across_ready_show_and_list() {
    let (main, checkout_owner, execution_path) = setup_managed_thread("feature/ready-land");
    let checkout = std::path::Path::new(&execution_path);
    std::fs::write(checkout.join("feature.txt"), "feature\n").unwrap();
    heddle(&["capture", "-m", "feature"], Some(checkout)).unwrap();

    let ready = json(
        &[
            "--output",
            "json",
            "ready",
            "--thread",
            "feature/ready-land",
        ],
        main.path(),
    );
    assert_eq!(
        ready["recommended_action"],
        "heddle land --thread feature/ready-land"
    );
    assert_eq!(
        ready["report"]["recommended_action"],
        "heddle land --thread feature/ready-land"
    );
    assert_no_banned_next_actions(&ready);

    let shown = json(
        &["--output", "json", "thread", "show", "feature/ready-land"],
        main.path(),
    );
    assert_eq!(
        shown["next_action"],
        "heddle land --thread feature/ready-land"
    );
    assert_no_banned_next_actions(&shown);

    let listed = json(&["--output", "json", "thread", "list"], main.path());
    let thread = listed["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["name"] == "feature/ready-land")
        .expect("thread list should include ready thread");
    assert_eq!(
        thread["recommended_action"],
        "heddle land --thread feature/ready-land"
    );
    assert_no_banned_next_actions(&listed);

    drop(checkout_owner);
}

#[test]
fn ready_from_isolated_checkout_recommends_bare_land() {
    let (_main, checkout_owner, execution_path) = setup_managed_thread("feature/current-land");
    let checkout = std::path::Path::new(&execution_path);
    std::fs::write(checkout.join("feature.txt"), "feature\n").unwrap();
    heddle(&["capture", "-m", "feature"], Some(checkout)).unwrap();

    let ready = json(&["--output", "json", "ready"], checkout);
    assert_eq!(ready["next_action"], "heddle land", "{ready}");
    assert_eq!(ready["recommended_action"], "heddle land", "{ready}");
    assert_eq!(
        ready["report"]["recommended_action"], "heddle land",
        "{ready}"
    );
    let next = ready["next_action"]
        .as_str()
        .expect("ready next_action should be a string");
    assert!(
        !next.contains("--thread") && !next.contains("--repo"),
        "current-checkout land must not emit selectors: {ready}"
    );
    assert_no_banned_next_actions(&ready);

    drop(checkout_owner);
}

#[test]
fn presence_show_multi_match_next_is_not_help_catalog() {
    use chrono::Utc;
    use objects::store::{
        ActorPresence, ActorPresenceStatus, ActorPresenceStore, AgentUsageSummary,
    };

    let repo = setup_native_repo();
    let opened = Repository::open(repo.path()).unwrap();
    let actors = ActorPresenceStore::new(opened.heddle_dir());
    for session_id in ["agent-alpha", "agent-beta"] {
        actors
            .save(&ActorPresence {
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
                path: Some(repo.path().to_path_buf()),
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
            })
            .unwrap();
    }

    let output = heddle_output(&["--output", "json", "presence", "show"], Some(repo.path()))
        .expect("presence show should spawn");
    assert!(
        !output.status.success(),
        "multi-match presence show must fail closed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value = serde_json::from_str(
        stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or(stderr.trim()),
    )
    .unwrap_or_else(|err| panic!("JSON envelope: {err}\n{stderr}"));
    assert_eq!(envelope["kind"], "ambiguous_actor_selection", "{envelope}");
    assert_eq!(
        envelope["primary_command"], "heddle presence show <session>",
        "{envelope}"
    );
    assert_ne!(
        envelope["primary_command"], "heddle help --output json",
        "{envelope}"
    );
}

#[test]
fn ready_clean_stale_managed_thread_refreshes_and_surfaces_land() {
    let (main, checkout_owner, execution_path) = setup_managed_thread("feature/stale-sync");
    let checkout = std::path::Path::new(&execution_path);
    std::fs::write(checkout.join("feature.txt"), "feature\n").unwrap();
    heddle(&["capture", "-m", "feature"], Some(checkout)).unwrap();

    std::fs::write(main.path().join("base.txt"), "base changed\n").unwrap();
    heddle(&["capture", "-m", "advance main"], Some(main.path())).unwrap();

    let ready = json(
        &[
            "--output",
            "json",
            "ready",
            "--thread",
            "feature/stale-sync",
        ],
        main.path(),
    );
    assert_eq!(
        ready["status"], "completed",
        "clean stale ready should refresh and finish readiness: {ready}"
    );
    assert_eq!(
        ready["recommended_action"],
        "heddle land --thread feature/stale-sync"
    );
    assert_eq!(
        ready["report"]["recommended_action"],
        "heddle land --thread feature/stale-sync"
    );
    assert_eq!(ready["report"]["freshness"], "current", "{ready}");
    assert_no_banned_next_actions(&ready);

    let shown = json(
        &["--output", "json", "thread", "show", "feature/stale-sync"],
        main.path(),
    );
    assert_eq!(
        shown["next_action"],
        "heddle land --thread feature/stale-sync"
    );
    assert_eq!(shown["freshness"], "current", "{shown}");
    assert_no_banned_next_actions(&shown);

    drop(checkout_owner);
}

#[test]
fn land_blocker_payload_names_thread_state_condition() {
    let (main, checkout_owner, _execution_path) =
        setup_current_blocked_thread("feature/blocked-state-payload");

    let land = json(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/blocked-state-payload",
        ],
        main.path(),
    );

    assert_eq!(
        land["status"], "blocked",
        "land must preserve the gate: {land}"
    );
    assert!(
        land["blockers"].as_array().is_some_and(|blockers| {
            blockers.iter().any(|blocker| {
                blocker
                    .as_str()
                    .is_some_and(|message| message.contains("thread state check"))
            })
        }),
        "human blockers must name the failed check: {land}"
    );
    let detail = &land["blocker_details"][0];
    assert_eq!(detail["code"], "thread_state_blocked", "{land}");
    assert_eq!(detail["check"], "thread_state", "{land}");
    assert_eq!(detail["paths"], serde_json::json!([]), "{land}");
    assert_eq!(
        detail["state_context"]["recorded_thread_state"], "blocked",
        "{land}"
    );
    assert_ne!(
        detail["state_context"]["recorded_state_id"],
        detail["state_context"]["thread_tip_state_id"],
        "the fixture must preserve the state-id mismatch from #1185: {land}"
    );
    assert_eq!(
        detail["state_context"]["merge_relation"], "fast_forward",
        "{land}"
    );
    assert_eq!(detail["state_context"]["conflict_count"], 0, "{land}");

    drop(checkout_owner);
}

#[test]
fn land_never_recommends_no_op_sync_for_current_thread_state_blocker() {
    let (main, checkout_owner, _execution_path) =
        setup_current_blocked_thread("feature/blocked-state-no-sync");

    let sync = json(
        &[
            "--output",
            "json",
            "sync",
            "--thread",
            "feature/blocked-state-no-sync",
        ],
        main.path(),
    );
    assert_eq!(
        sync["chosen_path"], "no_op",
        "fixture must hit the no-op path: {sync}"
    );

    let land = json(
        &[
            "--output",
            "json",
            "land",
            "--thread",
            "feature/blocked-state-no-sync",
        ],
        main.path(),
    );
    assert_eq!(land["status"], "blocked", "{land}");
    assert_eq!(land["next_action"], Value::Null, "{land}");
    assert_eq!(land["recommended_action"], Value::Null, "{land}");
    assert_eq!(land["blocker_details"][0]["code"], "thread_state_blocked");

    drop(checkout_owner);
}

// heddle#464 r2: `sync --thread` on a stale thread whose replay genuinely
// conflicts used to emit `heddle resolve --list` *before* refreshing — a dead
// breadcrumb, because no merge state existed yet and the top-level `resolve`
// failed with `no_merge_in_progress`. sync must now materialize the conflict
// (merge state + worktree markers) so the emitted breadcrumb actually runs.
#[test]
fn sync_conflicting_stale_thread_emits_runnable_resolve_breadcrumb() {
    let (main, checkout_owner, execution_path) = setup_managed_thread("feature/conflict-sync");
    let checkout = std::path::Path::new(&execution_path);

    // Both sides edit the SAME file divergently so the refresh genuinely
    // conflicts. (Disjoint-file edits 3-way merge cleanly — that path is
    // covered by `stale_managed_thread_suggests_sync_not_refresh_or_merge_preview`.)
    std::fs::write(checkout.join("base.txt"), "thread change\n").unwrap();
    heddle(&["capture", "-m", "thread edit"], Some(checkout)).unwrap();

    std::fs::write(main.path().join("base.txt"), "main change\n").unwrap();
    heddle(&["capture", "-m", "advance main"], Some(main.path())).unwrap();

    let sync = json(
        &[
            "--output",
            "json",
            "sync",
            "--thread",
            "feature/conflict-sync",
        ],
        main.path(),
    );
    assert_eq!(
        sync["status"], "blocked",
        "conflicting sync must block: {sync}"
    );
    let next_action = sync["next_action"]
        .as_str()
        .unwrap_or_else(|| panic!("sync conflict must carry a next_action: {sync}"));
    assert!(
        next_action.contains("resolve --list"),
        "sync conflict breadcrumb should drive the resolve flow: {sync}"
    );
    assert_no_banned_next_actions(&sync);

    // The breadcrumb must actually run: the conflict was materialized in the
    // thread's checkout, so `resolve --list` there reads real merge state
    // instead of failing with `no_merge_in_progress`.
    let resolve = heddle_output(&["--output", "json", "resolve", "--list"], Some(checkout))
        .expect("resolve --list should spawn");
    assert!(
        resolve.status.success(),
        "materialized resolve --list must succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&resolve.stdout),
        String::from_utf8_lossy(&resolve.stderr),
    );

    drop(checkout_owner);
}

/// heddle#1461: the default checkout thread is named `main` and has a ref
/// but often no ThreadManager record. `heddle sync` must treat that as the
/// current / default thread, not `Thread 'main' not found`.
#[test]
fn sync_on_default_main_thread_succeeds() {
    let repo = setup_native_repo();

    let sync = json(&["--output", "json", "sync"], repo.path());
    assert_eq!(sync["status"], "current", "{sync}");
    assert_eq!(sync["thread"], "main", "{sync}");
    assert_eq!(sync["chosen_path"], "no_op", "{sync}");
    assert_eq!(sync["output_kind"], "sync", "{sync}");
    let next = sync["next_action"].as_str().unwrap_or("");
    assert!(
        !next.contains("land --thread"),
        "sync next_action must not unconditionally recommend land --thread: {sync}"
    );
    assert_no_banned_next_actions(&sync);
}

/// heddle#1461 / #1467: `--thread` must not bypass `load_thread`'s
/// imported-ref advice just because the unmanaged ref is currently checked out.
#[test]
fn sync_named_unmanaged_ref_keeps_imported_ref_advice() {
    let repo = setup_native_repo();
    let output = heddle_output(
        &["--output", "json", "sync", "--thread", "main"],
        Some(repo.path()),
    )
    .expect("sync --thread main should spawn");
    assert!(
        !output.status.success(),
        "unmanaged main must not silently no-op: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: Value = serde_json::from_str(
        stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or(stderr.trim()),
    )
    .unwrap_or_else(|err| panic!("JSON envelope: {err}\n{stderr}"));
    assert_eq!(envelope["kind"], "imported_git_ref_not_managed_thread");
}

/// heddle#1461 / #1467: a managed thread created from detached HEAD has
/// `target_thread: None`. Sync must not treat that as already-current.
#[test]
fn sync_detached_head_managed_thread_without_target_is_not_noop() {
    let repo = setup_native_repo();
    let opened = Repository::open(repo.path()).unwrap();
    let head = opened
        .head()
        .unwrap()
        .expect("repo should have a current state before detaching");
    opened
        .write_head_recorded(&Head::Detached { state: head })
        .unwrap();
    drop(opened);

    let checkout = TempDir::new().unwrap();
    let checkout_arg = checkout.path().join("work");
    let started = json(
        &[
            "--output",
            "json",
            "start",
            "feature/no-target",
            "--path",
            checkout_arg.to_str().unwrap(),
        ],
        repo.path(),
    );
    let _execution_path = started["execution_path"]
        .as_str()
        .expect("start should report execution_path");

    let manager = ThreadManager::new(Repository::open(repo.path()).unwrap().heddle_dir());
    let thread = manager
        .load("feature/no-target")
        .unwrap()
        .expect("managed thread should have a record");
    assert!(
        thread.target_thread.is_none(),
        "detached-HEAD start must persist no target, got {:?}",
        thread.target_thread
    );

    let output = heddle_output(
        &["--output", "json", "sync", "--thread", "feature/no-target"],
        Some(repo.path()),
    )
    .expect("sync --thread feature/no-target should spawn");
    assert!(
        !output.status.success(),
        "no-target managed thread must not report success as a no-op: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !combined.contains("already current") && !combined.contains("\"chosen_path\":\"no_op\""),
        "no-target managed sync must not take the synthesized-checkout no-op: {combined}"
    );
    let envelope: Value = serde_json::from_str(
        combined
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with('{'))
            .unwrap_or(combined.trim()),
    )
    .unwrap_or_else(|err| panic!("JSON envelope: {err}\n{combined}"));
    assert_eq!(envelope["kind"], "missing_target_thread", "{envelope}");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("has no target thread")),
        "missing-target advice must be actionable: {envelope}"
    );

    drop(checkout);
}

/// heddle#1461: default human output is prose, not the JSON envelope.
#[test]
fn sync_default_text_is_prose_not_json() {
    let repo = setup_native_repo();
    let text = heddle(&["sync"], Some(repo.path())).expect("sync text");
    assert!(
        text.contains("Thread 'main' is already current"),
        "sync text should name the current thread in prose:\n{text}"
    );
    assert!(
        !text.trim_start().starts_with('{'),
        "sync default text must not be a JSON blob:\n{text}"
    );
    assert!(
        !text.contains("land --thread"),
        "sync text must not unconditionally recommend land --thread:\n{text}"
    );
}

/// heddle#1461: after a clean refresh, sync is done — next is not land --thread.
#[test]
fn sync_refresh_does_not_prescribe_land_thread() {
    let (main, checkout_owner, execution_path) = setup_managed_thread("feature/sync-next");
    let checkout = std::path::Path::new(&execution_path);
    std::fs::write(checkout.join("feature.txt"), "feature\n").unwrap();
    heddle(&["capture", "-m", "feature"], Some(checkout)).unwrap();

    std::fs::write(main.path().join("base.txt"), "base changed\n").unwrap();
    heddle(&["capture", "-m", "advance main"], Some(main.path())).unwrap();

    let sync = json(
        &["--output", "json", "sync", "--thread", "feature/sync-next"],
        main.path(),
    );
    assert_eq!(sync["status"], "refreshed", "{sync}");
    let next = sync["next_action"].as_str().unwrap_or("");
    assert!(
        !next.contains("land --thread"),
        "refreshed sync must not unconditionally recommend land --thread: {sync}"
    );
    assert_no_banned_next_actions(&sync);

    drop(checkout_owner);
}
