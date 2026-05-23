// SPDX-License-Identifier: Apache-2.0
//! Integration tests for thread-stack discovery, rebase planning, and the
//! `RepositorySnapshot` projection.
//!
//! These pin down the read-side stack model added in HeddleCo/heddle#138 so
//! that the CLI surface (`heddle stack` / `heddle stack ready` / `heddle
//! stack snapshot`) and the agentic harness hook can rely on a stable
//! discovery API.

use chrono::Utc;
use repo::{
    Repository, RepositorySnapshot, StackNextAction, ThreadFreshness, ThreadManager, ThreadMode,
    ThreadRecord, ThreadState,
};

fn save_thread_record(
    manager: &ThreadManager,
    name: &str,
    parent: Option<&str>,
    base_state: &str,
    current_state: &str,
    state: ThreadState,
) {
    let record = ThreadRecord {
        id: format!("rec-{name}"),
        thread: name.to_string(),
        target_thread: parent.map(str::to_string),
        parent_thread: parent.map(str::to_string),
        mode: ThreadMode::Materialized,
        state,
        base_state: base_state.to_string(),
        base_root: base_state.to_string(),
        current_state: Some(current_state.to_string()),
        merged_state: None,
        task: None,
        changed_paths: Vec::new(),
        impact_categories: Vec::new(),
        heavy_impact_paths: Vec::new(),
        promotion_suggested: false,
        freshness: ThreadFreshness::Current,
        verification_summary: Default::default(),
        confidence_summary: Default::default(),
        integration_policy_result: Default::default(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        ephemeral: None,
        auto: false,
        shared_target_dir: None,
    };
    manager.save_record(&record).unwrap();
}

#[test]
fn compute_stacks_finds_all_roots_and_descendants() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());

    // Two disjoint stacks:
    //   feat-a → feat-b → feat-c
    //   infra-x (alone)
    save_thread_record(
        &manager,
        "feat-a",
        None,
        "main-1",
        "feat-a-tip",
        ThreadState::Active,
    );
    save_thread_record(
        &manager,
        "feat-b",
        Some("feat-a"),
        "feat-a-tip",
        "feat-b-tip",
        ThreadState::Active,
    );
    save_thread_record(
        &manager,
        "feat-c",
        Some("feat-b"),
        "feat-b-tip",
        "feat-c-tip",
        ThreadState::Active,
    );
    save_thread_record(
        &manager,
        "infra-x",
        None,
        "main-1",
        "infra-x-tip",
        ThreadState::Active,
    );

    let stacks = repo.compute_thread_stacks().unwrap();
    assert_eq!(stacks.len(), 2, "expected two stack roots");

    // Sorted by root name.
    assert_eq!(stacks[0].root_name(), "feat-a");
    assert_eq!(stacks[0].member_count(), 3);
    assert_eq!(stacks[0].depth(), 2);

    assert_eq!(stacks[1].root_name(), "infra-x");
    assert_eq!(stacks[1].member_count(), 1);
    assert_eq!(stacks[1].depth(), 0);
}

#[test]
fn thread_stack_for_walks_up_to_root_from_any_descendant() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());

    save_thread_record(
        &manager,
        "feat-a",
        None,
        "main-1",
        "feat-a-tip",
        ThreadState::Active,
    );
    save_thread_record(
        &manager,
        "feat-b",
        Some("feat-a"),
        "feat-a-tip",
        "feat-b-tip",
        ThreadState::Active,
    );
    save_thread_record(
        &manager,
        "feat-c",
        Some("feat-b"),
        "feat-b-tip",
        "feat-c-tip",
        ThreadState::Active,
    );

    let from_root = repo.thread_stack_for("feat-a").unwrap().unwrap();
    let from_leaf = repo.thread_stack_for("feat-c").unwrap().unwrap();
    assert_eq!(from_root, from_leaf);
    assert_eq!(from_root.member_count(), 3);
}

#[test]
fn thread_stack_for_returns_none_for_unknown_thread() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    assert!(repo.thread_stack_for("does-not-exist").unwrap().is_none());
}

#[test]
fn plan_rebase_emits_bfs_steps_against_real_thread_records() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());

    std::fs::write(temp.path().join("file.txt"), "base").unwrap();
    let base = repo.snapshot(Some("base".to_string()), None).unwrap();
    repo.refs().set_thread("main", &base.change_id).unwrap();
    repo.refs().set_thread("feat-a", &base.change_id).unwrap();

    std::fs::write(temp.path().join("file.txt"), "feat-a content").unwrap();
    let feat_a_tip = repo.snapshot(Some("feat-a".to_string()), None).unwrap();
    repo.refs()
        .set_thread("feat-a", &feat_a_tip.change_id)
        .unwrap();
    repo.refs()
        .set_thread("feat-b", &feat_a_tip.change_id)
        .unwrap();

    std::fs::write(temp.path().join("file.txt"), "feat-b content").unwrap();
    let feat_b_tip = repo.snapshot(Some("feat-b".to_string()), None).unwrap();
    repo.refs()
        .set_thread("feat-b", &feat_b_tip.change_id)
        .unwrap();

    save_thread_record(
        &manager,
        "feat-a",
        None,
        &base.change_id.to_string(),
        &feat_a_tip.change_id.to_string(),
        ThreadState::Active,
    );
    save_thread_record(
        &manager,
        "feat-b",
        Some("feat-a"),
        &feat_a_tip.change_id.to_string(),
        &feat_b_tip.change_id.to_string(),
        ThreadState::Active,
    );

    // Move main forward so the planner has somewhere to rebase onto.
    std::fs::write(temp.path().join("main-only.txt"), "main work").unwrap();
    let new_main = repo.snapshot(Some("main moved".to_string()), None).unwrap();
    repo.refs().set_thread("main", &new_main.change_id).unwrap();

    let plan = repo
        .plan_thread_stack_rebase("feat-a", "main")
        .unwrap()
        .unwrap();

    // Root first, child second.
    let order: Vec<&str> = plan.steps.iter().map(|s| s.thread.as_str()).collect();
    assert_eq!(order, vec!["feat-a", "feat-b"]);

    // Root rebases onto `--onto`; child rebases onto its parent's projected
    // new tip.
    assert_eq!(plan.steps[0].new_base, "main");
    assert_eq!(plan.steps[1].new_base, "feat-a@projected");
    assert_eq!(plan.steps[1].parent_thread.as_deref(), Some("feat-a"));

    // Each step's `current_state` matches the live ref tip.
    assert_eq!(plan.steps[0].current_state, feat_a_tip.change_id.to_string());
    assert_eq!(plan.steps[1].current_state, feat_b_tip.change_id.to_string());

    assert!(!plan.is_no_op());
}

#[test]
fn plan_rebase_rejects_non_root_target() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());

    save_thread_record(
        &manager,
        "feat-a",
        None,
        "main",
        "feat-a-tip",
        ThreadState::Active,
    );
    save_thread_record(
        &manager,
        "feat-b",
        Some("feat-a"),
        "feat-a-tip",
        "feat-b-tip",
        ThreadState::Active,
    );

    let err = repo
        .plan_thread_stack_rebase("feat-b", "main")
        .unwrap()
        .unwrap_err();
    assert!(
        format!("{err}").contains("not a stack root"),
        "expected NotARoot error, got: {err}"
    );
}

// ── RepositorySnapshot projection ──────────────────────────────────────────

#[test]
fn repository_snapshot_round_trips_through_json() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());

    save_thread_record(
        &manager,
        "feat-a",
        None,
        "main-1",
        "feat-a-tip",
        ThreadState::Active,
    );
    save_thread_record(
        &manager,
        "feat-b",
        Some("feat-a"),
        "feat-a-tip",
        "feat-b-tip",
        ThreadState::Ready,
    );

    let snapshot = RepositorySnapshot::capture(&repo).unwrap();
    let json = serde_json::to_string(&snapshot).unwrap();
    let parsed: RepositorySnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(snapshot, parsed);

    // Snapshot carries the stack view + the thread records we just wrote.
    assert_eq!(parsed.stacks.len(), 1);
    assert_eq!(parsed.stacks[0].root_name(), "feat-a");
    assert_eq!(parsed.threads.len(), 2);
}

#[test]
fn stack_next_action_all_clean_returns_ready() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());

    save_thread_record(
        &manager,
        "feat-a",
        None,
        "main-1",
        "feat-a-tip",
        ThreadState::Ready,
    );
    save_thread_record(
        &manager,
        "feat-b",
        Some("feat-a"),
        "feat-a-tip",
        "feat-b-tip",
        ThreadState::Ready,
    );
    save_thread_record(
        &manager,
        "feat-c",
        Some("feat-b"),
        "feat-b-tip",
        "feat-c-tip",
        ThreadState::Ready,
    );

    let snapshot = RepositorySnapshot::capture(&repo).unwrap();
    let action = snapshot.next_action_for("feat-b").unwrap();
    assert_eq!(action, StackNextAction::Ready);
}

#[test]
fn stack_next_action_one_blocked_reports_blocked() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());

    save_thread_record(
        &manager,
        "feat-a",
        None,
        "main-1",
        "feat-a-tip",
        ThreadState::Ready,
    );
    save_thread_record(
        &manager,
        "feat-b",
        Some("feat-a"),
        "feat-a-tip",
        "feat-b-tip",
        ThreadState::Blocked,
    );
    save_thread_record(
        &manager,
        "feat-c",
        Some("feat-b"),
        "feat-b-tip",
        "feat-c-tip",
        ThreadState::Ready,
    );

    let snapshot = RepositorySnapshot::capture(&repo).unwrap();
    let action = snapshot.next_action_for("feat-c").unwrap();
    match action {
        StackNextAction::Blocked { thread } => assert_eq!(thread, "feat-b"),
        other => panic!("expected Blocked, got {other:?}"),
    }
}

#[test]
fn stack_next_action_top_active_means_waiting_on_review() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());

    // All threads ready EXCEPT the top, which is still Active — the stack
    // is otherwise clean but the leaf is "waiting on review".
    save_thread_record(
        &manager,
        "feat-a",
        None,
        "main-1",
        "feat-a-tip",
        ThreadState::Ready,
    );
    save_thread_record(
        &manager,
        "feat-b",
        Some("feat-a"),
        "feat-a-tip",
        "feat-b-tip",
        ThreadState::Ready,
    );
    save_thread_record(
        &manager,
        "feat-c",
        Some("feat-b"),
        "feat-b-tip",
        "feat-c-tip",
        ThreadState::Active,
    );

    let snapshot = RepositorySnapshot::capture(&repo).unwrap();
    let action = snapshot.next_action_for("feat-c").unwrap();
    match action {
        StackNextAction::WaitingOnReview { thread } => assert_eq!(thread, "feat-c"),
        other => panic!("expected WaitingOnReview, got {other:?}"),
    }
}
