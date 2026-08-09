// SPDX-License-Identifier: Apache-2.0
//! Coverage for item 2.2 of the heddle 6→8 plan: auto-prune
//! harness-generated threads.
//!
//! The plan's three acceptance shapes are exercised here:
//!
//!   1. `heddle thread list` (no flags) hides threads tagged
//!      `auto: true`; `--include-auto` surfaces them.
//!   2. `heddle thread cleanup --merged` (with and without
//!      `--dry-run`) drops merged threads and reports the count
//!      and reclaimed bytes.
//!   3. `heddle thread cleanup --auto --older-than <duration>`
//!      filters by `updated_at` age before sweeping auto-threads.
//!
//! The auto-tagging path itself (harness segment-rotation) is
//! exercised via direct `ThreadManager::save` of synthetic auto
//! records — the harness is hard to fire from a unit test, but the
//! storage and visibility halves are what `thread list` and
//! `thread cleanup` actually read.

use std::fs;

use chrono::{Duration, Utc};
use objects::{
    object::ThreadName,
    store::{
        ActorPresence, ActorPresenceStatus, ActorPresenceStore, AgentUsageSummary, ObjectStore,
        WriterLeaseDraft, WriterLeaseReserveOutcome, WriterLeaseStatus, WriterLeaseStore,
    },
};
use oplog::{OpLogBackend, OpRecord};
use repo::{
    Repository, Thread, ThreadConfidenceSummary, ThreadFreshness, ThreadIntegrationPolicy,
    ThreadManager, ThreadMode, ThreadState, ThreadVerificationSummary,
};
use serde_json::Value;
use tempfile::TempDir;

use super::{heddle, heddle_output};

/// Bootstrap a minimal repo with one snapshot. Tests that need a
/// thread on top either use the CLI (`thread create`) for explicit
/// threads or seed a synthetic record through `ThreadManager` for
/// auto-threads.
fn setup_repo() -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "init"], Some(temp.path())).unwrap();
    temp
}

/// Save a synthetic thread record directly through `ThreadManager`.
/// Mirrors how the harness path stores its records (sans real
/// checkout). Lets us pin `auto` and `updated_at` to whatever the
/// test needs without booting a Claude session.
fn seed_thread(
    repo_path: &std::path::Path,
    name: &str,
    auto: bool,
    state: ThreadState,
    updated_at: chrono::DateTime<Utc>,
) {
    let repo = Repository::open(repo_path).unwrap();
    let head = repo
        .head()
        .unwrap()
        .expect("repo should have a current state after snapshot");
    let head_short = head.short();
    let base_root = repo
        .store()
        .get_state(&head)
        .unwrap()
        .map(|state| state.tree.short())
        .unwrap_or_default();
    let manager = ThreadManager::new(repo.heddle_dir());
    let thread = Thread {
        id: name.to_string(),
        thread: name.to_string(),
        target_thread: Some("main".to_string()),
        parent_thread: None,
        mode: ThreadMode::Materialized,
        state,
        base_state: head_short.clone(),
        base_root,
        current_state: Some(head_short),
        merged_state: None,
        task: None,
        execution_path: std::path::PathBuf::new(),
        materialized_path: None,
        changed_paths: vec![],
        impact_categories: vec![],
        heavy_impact_paths: vec![],
        promotion_suggested: false,
        freshness: ThreadFreshness::Current,
        verification_summary: ThreadVerificationSummary::default(),
        confidence_summary: ThreadConfidenceSummary::default(),
        integration_policy_result: ThreadIntegrationPolicy::default(),
        created_at: updated_at,
        updated_at,
        ephemeral: None,
        auto,
        shared_target_dir: None,
    };
    // `ThreadManager::save` requires the ref to exist for `find_by_*`
    // to round-trip, but `cmd_thread_list` reads from the record
    // store directly. Add the ref to keep us honest about what the
    // CLI sees.
    repo.refs()
        .set_thread(&ThreadName::new(name), &head)
        .unwrap();
    manager.save(&thread).unwrap();
}

fn list_thread_names(repo_path: &std::path::Path, args: &[&str]) -> Vec<String> {
    let mut argv = vec!["--output", "json", "thread", "list"];
    argv.extend_from_slice(args);
    let out = heddle(&argv, Some(repo_path)).expect("thread list should succeed");
    let value: Value = serde_json::from_str(&out).expect("thread list output should be JSON");
    value["threads"]
        .as_array()
        .expect("threads array")
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Auto-threads must be hidden from the default `heddle thread list`
/// view — that's the whole point of item 2.2 (the bug was 200+
/// auto-threads burying the explicit ones). `--include-auto` brings
/// them back.
#[test]
fn thread_list_hides_auto_threads_by_default() {
    let temp = setup_repo();
    seed_thread(
        temp.path(),
        "explicit",
        false,
        ThreadState::Active,
        Utc::now(),
    );
    seed_thread(temp.path(), "auto-1", true, ThreadState::Active, Utc::now());
    seed_thread(temp.path(), "auto-2", true, ThreadState::Active, Utc::now());

    let default_view = list_thread_names(temp.path(), &[]);
    assert!(
        default_view.iter().any(|name| name == "explicit"),
        "default view should include the explicit thread; got {default_view:?}"
    );
    assert!(
        !default_view.iter().any(|name| name == "auto-1"),
        "default view must hide auto threads; got {default_view:?}"
    );
    assert!(
        !default_view.iter().any(|name| name == "auto-2"),
        "default view must hide auto threads; got {default_view:?}"
    );

    let with_auto = list_thread_names(temp.path(), &["--include-auto"]);
    assert!(
        with_auto.iter().any(|name| name == "auto-1"),
        "--include-auto should surface auto threads; got {with_auto:?}"
    );
    assert!(
        with_auto.iter().any(|name| name == "auto-2"),
        "--include-auto should surface auto threads; got {with_auto:?}"
    );
    assert!(
        with_auto.iter().any(|name| name == "explicit"),
        "--include-auto should still include explicit threads; got {with_auto:?}"
    );
}

#[test]
fn thread_list_hides_abandoned_in_text_and_json_and_composes_include_flags() {
    let temp = setup_repo();
    seed_thread(
        temp.path(),
        "abandoned/explicit",
        false,
        ThreadState::Abandoned,
        Utc::now(),
    );
    seed_thread(
        temp.path(),
        "abandoned/auto",
        true,
        ThreadState::Abandoned,
        Utc::now(),
    );

    let default_json = list_thread_names(temp.path(), &[]);
    assert!(!default_json.iter().any(|name| name == "abandoned/explicit"));
    assert!(!default_json.iter().any(|name| name == "abandoned/auto"));
    let default_text = heddle(&["thread", "list"], Some(temp.path())).unwrap();
    assert!(!default_text.contains("abandoned/explicit"));
    assert!(!default_text.contains("abandoned/auto"));

    let abandoned_only = list_thread_names(temp.path(), &["--include-abandoned"]);
    assert!(
        abandoned_only
            .iter()
            .any(|name| name == "abandoned/explicit")
    );
    assert!(!abandoned_only.iter().any(|name| name == "abandoned/auto"));

    let auto_only = list_thread_names(temp.path(), &["--include-auto"]);
    assert!(!auto_only.iter().any(|name| name == "abandoned/explicit"));
    assert!(!auto_only.iter().any(|name| name == "abandoned/auto"));

    let audit = list_thread_names(temp.path(), &["--include-auto", "--include-abandoned"]);
    assert!(audit.iter().any(|name| name == "abandoned/explicit"));
    assert!(audit.iter().any(|name| name == "abandoned/auto"));
}

#[test]
fn thread_cleanup_abandoned_dry_run_then_cleans_ref_but_retains_audit_history() {
    let temp = setup_repo();
    seed_thread(
        temp.path(),
        "abandoned/audit",
        false,
        ThreadState::Abandoned,
        Utc::now(),
    );
    let repo = Repository::open(temp.path()).unwrap();
    let retained_state = repo.head().unwrap().expect("seeded repository has HEAD");
    let oplog_len_before = repo.oplog().recent(256).unwrap().len();

    let dry_run = heddle(
        &[
            "--output",
            "json",
            "thread",
            "cleanup",
            "--abandoned",
            "--dry-run",
        ],
        Some(temp.path()),
    )
    .expect("abandoned cleanup dry run");
    let dry_run: Value = serde_json::from_str(&dry_run).unwrap();
    assert_eq!(
        dry_run["message"],
        "would clean 1 abandoned thread(s) (would reclaim 0 B)"
    );
    assert_eq!(dry_run["abandoned"].as_array().unwrap().len(), 1);
    assert_eq!(dry_run["abandoned"][0]["reason"], "abandoned");
    assert_eq!(dry_run["reclaimed_bytes"], 0);
    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("abandoned/audit"))
            .unwrap()
            .is_some(),
        "dry-run must retain the live ref"
    );
    assert_eq!(repo.oplog().recent(256).unwrap().len(), oplog_len_before);

    let cleaned = heddle(
        &["--output", "json", "thread", "cleanup", "--abandoned"],
        Some(temp.path()),
    )
    .expect("clean abandoned residue");
    let cleaned: Value = serde_json::from_str(&cleaned).unwrap();
    assert_eq!(
        cleaned["message"],
        "cleaned 1 abandoned thread(s) (reclaimed 0 B)"
    );
    assert_eq!(cleaned["abandoned"].as_array().unwrap().len(), 1);

    let reopened = Repository::open(temp.path()).unwrap();
    assert!(
        reopened
            .refs()
            .get_thread(&ThreadName::new("abandoned/audit"))
            .unwrap()
            .is_none(),
        "cleanup must prune the abandoned live ref"
    );
    let manager = ThreadManager::new(reopened.heddle_dir());
    let record = manager
        .load("abandoned/audit")
        .unwrap()
        .expect("abandoned manager record is retained");
    assert!(matches!(record.state, ThreadState::Abandoned));
    assert!(
        reopened
            .store()
            .get_state(&retained_state)
            .unwrap()
            .is_some(),
        "captured state objects must be retained"
    );
    assert!(
        reopened.oplog().recent(256).unwrap().iter().any(|entry| {
            matches!(
                &entry.operation,
                OpRecord::ThreadDelete { name, .. } if name == "abandoned/audit"
            )
        }),
        "recorded ref deletion must remain in the oplog"
    );
    assert!(
        list_thread_names(temp.path(), &["--include-abandoned"])
            .iter()
            .any(|name| name == "abandoned/audit"),
        "audit view must include the retained no-ref abandoned record"
    );

    let second = heddle(
        &["--output", "json", "thread", "cleanup", "--abandoned"],
        Some(temp.path()),
    )
    .expect("idempotent abandoned cleanup rerun");
    let second: Value = serde_json::from_str(&second).unwrap();
    assert!(second["abandoned"].as_array().unwrap().is_empty());
    assert_eq!(
        second["message"],
        "cleaned 0 abandoned thread(s) (reclaimed 0 B)"
    );
    let text = heddle(&["thread", "cleanup", "--abandoned"], Some(temp.path())).unwrap();
    assert!(text.contains("No threads matched the cleanup criteria."));
}

#[test]
fn thread_cleanup_abandoned_selects_each_residue_kind_and_dry_run_is_immutable() {
    let temp = setup_repo();
    for name in [
        "abandoned/path",
        "abandoned/manifest",
        "abandoned/presence",
        "abandoned/lease",
        "abandoned/clean",
    ] {
        seed_thread(temp.path(), name, false, ThreadState::Abandoned, Utc::now());
    }
    let repo = Repository::open(temp.path()).unwrap();
    for name in [
        "abandoned/path",
        "abandoned/manifest",
        "abandoned/presence",
        "abandoned/lease",
        "abandoned/clean",
    ] {
        repo.refs().delete_thread(&ThreadName::new(name)).unwrap();
    }
    let manager = ThreadManager::new(repo.heddle_dir());

    let execution_path = temp.path().join("abandoned-path-residue");
    fs::create_dir_all(&execution_path).unwrap();
    fs::write(execution_path.join("residue.txt"), "residue").unwrap();
    let mut path_record = manager.load("abandoned/path").unwrap().unwrap();
    path_record.execution_path = execution_path.clone();
    manager.save(&path_record).unwrap();

    let manifest_path =
        repo::thread_manifest::manifest_path(repo.heddle_dir(), "abandoned/manifest");
    fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    fs::write(&manifest_path, "residue").unwrap();

    let presence_store = ActorPresenceStore::new(repo.heddle_dir());
    presence_store
        .save(&ActorPresence {
            session_id: "agent-abandoned-presence".to_string(),
            client_instance_id: None,
            native_actor_key: None,
            native_parent_actor_key: None,
            native_instance_key: None,
            heddle_session_id: None,
            thread_id: Some("abandoned/presence".to_string()),
            thread: "abandoned/presence".to_string(),
            anchor_state: None,
            anchor_root: None,
            path: None,
            base_state: String::new(),
            started_at: Utc::now(),
            provider: None,
            model: None,
            harness: None,
            thinking_level: None,
            usage_summary: AgentUsageSummary::default(),
            last_progress_at: None,
            report_flush_state: None,
            attach_reason: None,
            task_assignment_id: None,
            attach_precedence: Vec::new(),
            winning_attach_rule: None,
            probe_source: None,
            probe_confidence: None,
            status: ActorPresenceStatus::Active,
            completed_at: None,
            context_queries: Vec::new(),
        })
        .unwrap();

    let lease_store = WriterLeaseStore::new(repo.heddle_dir());
    let WriterLeaseReserveOutcome::Reserved(lease_grant) = lease_store
        .reserve(
            WriterLeaseDraft {
                thread: "abandoned/lease".to_string(),
                actor_session_id: None,
                task_assignment_id: None,
                anchor_state: None,
                anchor_root: None,
                path: None,
                pid: None,
                boot_id: None,
            },
            Utc::now(),
        )
        .unwrap()
    else {
        panic!("fresh writer lease should reserve");
    };

    let dry_run = heddle(
        &[
            "--output",
            "json",
            "thread",
            "cleanup",
            "--abandoned",
            "--dry-run",
        ],
        Some(temp.path()),
    )
    .unwrap();
    let dry_run: Value = serde_json::from_str(&dry_run).unwrap();
    let selected = dry_run["abandoned"].as_array().unwrap();
    assert_eq!(
        selected.len(),
        4,
        "clean audit record must not rematch: {dry_run}"
    );
    assert!(execution_path.exists());
    assert!(manifest_path.exists());
    assert!(
        presence_store
            .load("agent-abandoned-presence")
            .unwrap()
            .is_some()
    );
    assert_eq!(
        lease_store
            .load(&lease_grant.lease.lease_id)
            .unwrap()
            .unwrap()
            .status,
        WriterLeaseStatus::Active
    );

    heddle(&["thread", "cleanup", "--abandoned"], Some(temp.path())).unwrap();
    assert!(!execution_path.exists());
    assert!(!manifest_path.exists());
    assert!(
        presence_store
            .load("agent-abandoned-presence")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        lease_store
            .load(&lease_grant.lease.lease_id)
            .unwrap()
            .unwrap()
            .status,
        WriterLeaseStatus::Abandoned
    );
}

#[test]
fn thread_cleanup_abandoned_skips_the_current_checkout() {
    let temp = setup_repo();
    seed_thread(
        temp.path(),
        "main",
        false,
        ThreadState::Abandoned,
        Utc::now(),
    );
    assert!(
        list_thread_names(temp.path(), &[])
            .iter()
            .any(|name| name == "main"),
        "the current checkout is never hidden even with inconsistent abandoned metadata"
    );

    let output = heddle(
        &["--output", "json", "thread", "cleanup", "--abandoned"],
        Some(temp.path()),
    )
    .unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert!(output["abandoned"].as_array().unwrap().is_empty());
    assert_eq!(output["skipped"][0]["thread"], "main");
    assert_eq!(output["skipped"][0]["reason"], "active");
    let repo = Repository::open(temp.path()).unwrap();
    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .is_some(),
        "current thread ref must remain untouched"
    );
}

#[test]
fn thread_cleanup_modes_combine_without_double_counting_abandoned_auto_threads() {
    let temp = setup_repo();
    seed_thread(
        temp.path(),
        "merged/one",
        false,
        ThreadState::Merged,
        Utc::now(),
    );
    seed_thread(
        temp.path(),
        "auto/stale",
        true,
        ThreadState::Active,
        Utc::now() - Duration::days(14),
    );
    seed_thread(
        temp.path(),
        "abandoned/auto",
        true,
        ThreadState::Abandoned,
        Utc::now() - Duration::days(14),
    );

    let output = heddle(
        &[
            "--output",
            "json",
            "thread",
            "cleanup",
            "--merged",
            "--auto",
            "--older-than",
            "7d",
            "--abandoned",
            "--dry-run",
        ],
        Some(temp.path()),
    )
    .unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output["merged"].as_array().unwrap().len(), 1);
    assert_eq!(output["auto"].as_array().unwrap().len(), 1);
    assert_eq!(output["abandoned"].as_array().unwrap().len(), 1);
    assert_eq!(output["abandoned"][0]["thread"], "abandoned/auto");
    assert!(
        output["auto"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["thread"] != "abandoned/auto")
    );
}

/// `heddle thread cleanup` with no mode flag must refuse — we don't
/// want the user to type `heddle thread cleanup` and have it silently
/// no-op (or worse, sweep something they didn't ask to).
#[test]
fn thread_cleanup_without_mode_flag_refuses() {
    let temp = setup_repo();
    let err = heddle(&["thread", "cleanup"], Some(temp.path()))
        .expect_err("cleanup with no mode flag should exit non-zero");
    assert!(
        err.contains("requires at least one mode flag"),
        "expected refusal message naming both modes; got: {err}"
    );
    assert!(
        err.contains("--merged"),
        "refusal should name --merged; got: {err}"
    );
    assert!(
        err.contains("--auto"),
        "refusal should name --auto; got: {err}"
    );
    assert!(
        err.contains("--abandoned"),
        "refusal should name --abandoned; got: {err}"
    );
}

#[test]
fn thread_cleanup_without_mode_flag_uses_typed_advice_json() {
    let temp = setup_repo();
    let output = heddle_output(
        &["--output", "json", "thread", "cleanup"],
        Some(temp.path()),
    )
    .expect("invoke cleanup without mode");
    assert!(!output.status.success(), "cleanup without mode should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode cleanup refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("cleanup mode refusal should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_cleanup_mode_required");
    assert_eq!(
        envelope["error"],
        "heddle thread cleanup requires at least one mode flag: --merged, --auto --older-than <duration>, or --abandoned."
    );
    assert_eq!(
        envelope["hint"],
        "Add --dry-run to the cleanup mode you intend; for abandoned threads, run `heddle thread cleanup --abandoned --dry-run`."
    );
}

/// `--merged --dry-run` lists merged threads without actually
/// removing their records from the store.
#[test]
fn thread_cleanup_merged_dry_run_reports_without_dropping() {
    let temp = setup_repo();
    seed_thread(
        temp.path(),
        "feat/done",
        false,
        ThreadState::Merged,
        Utc::now(),
    );
    seed_thread(
        temp.path(),
        "feat/active",
        false,
        ThreadState::Active,
        Utc::now(),
    );

    let out = heddle(
        &[
            "--output",
            "json",
            "thread",
            "cleanup",
            "--merged",
            "--dry-run",
        ],
        Some(temp.path()),
    )
    .expect("dry-run cleanup should succeed");
    let value: Value = serde_json::from_str(&out).unwrap();

    assert_eq!(value["dry_run"], Value::Bool(true));
    let merged = value["merged"].as_array().expect("merged array");
    assert_eq!(
        merged.len(),
        1,
        "only the merged thread should be queued; got {value}"
    );
    assert_eq!(merged[0]["thread"], Value::String("feat/done".to_string()));
    assert_eq!(merged[0]["reason"], Value::String("merged".to_string()));

    // The active thread must still be visible; the merged one must
    // still exist in the record store (dry-run did not delete it).
    let after_with_auto = list_thread_names(temp.path(), &["--include-auto"]);
    assert!(after_with_auto.iter().any(|n| n == "feat/active"));
    let repo_for_manager = Repository::open(temp.path()).unwrap();
    let manager = ThreadManager::new(repo_for_manager.heddle_dir());
    let merged_after = manager.load("feat/done").unwrap().expect("still on disk");
    assert!(matches!(merged_after.state, ThreadState::Merged));
}

/// `--merged` (no dry-run) actually drops the matching threads.
/// Cleanup marks the record `Abandoned`, removes any execution path,
/// and prunes the live thread ref so cleaned work disappears from
/// everyday thread/push surfaces.
#[test]
fn thread_cleanup_merged_drops_matching_threads() {
    let temp = setup_repo();
    seed_thread(
        temp.path(),
        "feat/done",
        false,
        ThreadState::Merged,
        Utc::now(),
    );
    seed_thread(
        temp.path(),
        "feat/active",
        false,
        ThreadState::Active,
        Utc::now(),
    );

    let out = heddle(
        &["--output", "json", "thread", "cleanup", "--merged"],
        Some(temp.path()),
    )
    .expect("cleanup --merged should succeed");
    let value: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["dry_run"], Value::Bool(false));
    let merged = value["merged"].as_array().expect("merged array");
    assert_eq!(merged.len(), 1);

    let repo = Repository::open(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());
    let dropped = manager
        .load("feat/done")
        .unwrap()
        .expect("record still exists (we only mark Abandoned, not delete)");
    assert!(
        matches!(dropped.state, ThreadState::Abandoned),
        "merged thread should be marked Abandoned after cleanup; got {:?}",
        dropped.state
    );
    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("feat/done"))
            .unwrap()
            .is_none(),
        "merged cleanup should remove the live thread ref so default surfaces stop treating it as active"
    );
    let default_view = list_thread_names(temp.path(), &[]);
    assert!(
        !default_view.iter().any(|name| name == "feat/done"),
        "cleaned merged thread should disappear from the default thread list; got {default_view:?}"
    );
    let still_active = manager.load("feat/active").unwrap().expect("loads");
    assert!(
        matches!(still_active.state, ThreadState::Active),
        "non-merged thread must be left alone; got {:?}",
        still_active.state
    );
}

/// `--auto --older-than <duration>` only sweeps auto-threads whose
/// `updated_at` is older than the cutoff. A fresh auto-thread must
/// survive the sweep.
#[test]
fn thread_cleanup_auto_filters_by_age() {
    let temp = setup_repo();
    let stale = Utc::now() - Duration::days(14);
    let fresh = Utc::now() - Duration::hours(1);
    seed_thread(temp.path(), "auto/old", true, ThreadState::Active, stale);
    seed_thread(temp.path(), "auto/new", true, ThreadState::Active, fresh);
    seed_thread(
        temp.path(),
        "explicit",
        false,
        ThreadState::Active,
        Utc::now() - Duration::days(30),
    );

    let out = heddle(
        &[
            "--output",
            "json",
            "thread",
            "cleanup",
            "--auto",
            "--older-than",
            "7d",
        ],
        Some(temp.path()),
    )
    .expect("cleanup --auto should succeed");
    let value: Value = serde_json::from_str(&out).unwrap();
    let auto = value["auto"].as_array().expect("auto array");
    assert_eq!(
        auto.len(),
        1,
        "only the stale auto-thread should be swept; got {value}"
    );
    assert_eq!(auto[0]["thread"], Value::String("auto/old".to_string()));

    let repo = Repository::open(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());
    let old_auto = manager.load("auto/old").unwrap().expect("loads");
    assert!(matches!(old_auto.state, ThreadState::Abandoned));
    let new_auto = manager.load("auto/new").unwrap().expect("loads");
    assert!(matches!(new_auto.state, ThreadState::Active));
    let explicit = manager.load("explicit").unwrap().expect("loads");
    assert!(
        matches!(explicit.state, ThreadState::Active),
        "explicit threads must never be swept by --auto, regardless of age"
    );
}

/// In dry-run mode, `reclaimed_bytes` must report `0` (we did not
/// reclaim anything). The estimate of what *would* have been
/// reclaimed lives in `would_reclaim_bytes`. Mirrors what cron-style
/// automation expects when watching the field for actual reclaim.
#[test]
fn thread_cleanup_dry_run_reports_zero_reclaimed_bytes() {
    let temp = setup_repo();
    seed_thread(
        temp.path(),
        "feat/done",
        false,
        ThreadState::Merged,
        Utc::now(),
    );

    let out = heddle(
        &[
            "--output",
            "json",
            "thread",
            "cleanup",
            "--merged",
            "--dry-run",
        ],
        Some(temp.path()),
    )
    .expect("dry-run cleanup should succeed");
    let value: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["dry_run"], Value::Bool(true));
    assert_eq!(
        value["reclaimed_bytes"],
        Value::Number(serde_json::Number::from(0)),
        "dry-run must zero out reclaimed_bytes; got {value}"
    );
    assert!(
        value.get("would_reclaim_bytes").is_some(),
        "dry-run output must include would_reclaim_bytes; got {value}"
    );
}

/// Cleanup must skip the active thread — running `cleanup --auto
/// --older-than 0s` (or `--merged`) from inside a qualifying thread
/// previously dropped its own checkout mid-command, leaving the user
/// in a deleted directory. Now the active thread is surfaced under
/// `skipped` and the record stays Active.
#[test]
fn thread_cleanup_skips_active_thread() {
    let temp = setup_repo();
    let stale = Utc::now() - Duration::days(14);
    seed_thread(temp.path(), "auto/active", true, ThreadState::Active, stale);
    seed_thread(
        temp.path(),
        "auto/passive",
        true,
        ThreadState::Active,
        stale,
    );
    // Attach HEAD to `auto/active` so `current_thread()` in the
    // cleanup loop classifies it as the active thread.
    heddle(&["thread", "switch", "auto/active"], Some(temp.path()))
        .expect("switch should attach HEAD to the seeded thread");

    let out = heddle(
        &[
            "--output",
            "json",
            "thread",
            "cleanup",
            "--auto",
            "--older-than",
            "1d",
        ],
        Some(temp.path()),
    )
    .expect("cleanup --auto should succeed even when active thread qualifies");
    let value: Value = serde_json::from_str(&out).unwrap();
    let auto = value["auto"].as_array().expect("auto array");
    assert_eq!(
        auto.len(),
        1,
        "the passive auto thread is the only one swept; got {value}"
    );
    assert_eq!(auto[0]["thread"], Value::String("auto/passive".to_string()));

    let skipped = value["skipped"].as_array().expect("skipped array");
    assert_eq!(
        skipped.len(),
        1,
        "active thread should be reported as skipped; got {value}"
    );
    assert_eq!(
        skipped[0]["thread"],
        Value::String("auto/active".to_string())
    );
    assert_eq!(skipped[0]["reason"], Value::String("active".to_string()));

    let repo = Repository::open(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());
    let active_after = manager.load("auto/active").unwrap().expect("loads");
    assert!(
        matches!(active_after.state, ThreadState::Active),
        "active thread must survive cleanup; got {:?}",
        active_after.state
    );
    let passive_after = manager.load("auto/passive").unwrap().expect("loads");
    assert!(
        matches!(passive_after.state, ThreadState::Abandoned),
        "passive thread should be dropped; got {:?}",
        passive_after.state
    );
}

/// `--auto` without `--older-than` must refuse: implicitly treating
/// every auto-thread as fair game would risk wiping the user's
/// just-spawned subagent.
#[test]
fn thread_cleanup_auto_requires_older_than() {
    let temp = setup_repo();
    let err = heddle(&["thread", "cleanup", "--auto"], Some(temp.path()))
        .expect_err("--auto without --older-than should refuse");
    assert!(
        err.contains("--older-than"),
        "refusal must point at --older-than; got: {err}"
    );
}

#[test]
fn thread_cleanup_invalid_duration_uses_typed_advice_json() {
    let temp = setup_repo();
    let output = heddle_output(
        &[
            "--output",
            "json",
            "thread",
            "cleanup",
            "--auto",
            "--older-than",
            "1x",
        ],
        Some(temp.path()),
    )
    .expect("invoke cleanup with invalid duration");
    assert!(
        !output.status.success(),
        "cleanup with invalid duration should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode invalid duration refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("invalid duration should emit JSON envelope");
    assert_eq!(envelope["kind"], "thread_cleanup_invalid_duration");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("unknown duration unit")),
        "invalid duration refusal should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("7d")),
        "invalid duration hint should recommend a valid duration: {stderr}"
    );
}

/// Regression: `apply_thread_drop` must unmount by *thread name*, not
/// by record `id`. The two diverge for legacy/synced records (the
/// `Thread` model treats `id` and `thread` as separate fields, and
/// mounts are keyed by name at `establish_virtualized_mount` time).
/// When they diverge and we keyed unmount on `id`, the live mount
/// stayed up and the subsequent rmdir of the mount point hit EBUSY,
/// hard-failing `thread cleanup`.
///
/// This test seeds a Virtualized record where `id != thread`, with a
/// real on-disk execution_path, runs `cleanup --merged`, and asserts
/// the cleanup succeeds end-to-end (record marked Abandoned, path
/// removed). With the bug, the no-op stub paths still let this pass
/// on macOS/non-mount builds — but the keying assertion is enforced
/// by code review and the broader Linux mount integration test.
#[test]
fn thread_cleanup_handles_id_diverging_from_name() {
    let temp = setup_repo();
    let repo = Repository::open(temp.path()).unwrap();
    let head = repo.head().unwrap().expect("repo has head after init");
    let head_short = head.short();
    let base_root = repo
        .store()
        .get_state(&head)
        .unwrap()
        .map(|state| state.tree.short())
        .unwrap_or_default();

    // Build a real execution path that cleanup must remove. We pin
    // it inside the tempdir so the rmdir target is under our control.
    let exec_path = temp.path().join(".synced-thread-checkout");
    fs::create_dir_all(&exec_path).unwrap();
    fs::write(exec_path.join("scratch.txt"), b"keep").unwrap();

    let manager = ThreadManager::new(repo.heddle_dir());
    // The bug case: `id` is a synced/legacy record id, `thread` is
    // the user-facing name (and the mount key).
    let synthetic = Thread {
        id: "synced-record-deadbeef".to_string(),
        thread: "feat/light-mount".to_string(),
        target_thread: Some("main".to_string()),
        parent_thread: None,
        mode: ThreadMode::Virtualized,
        state: ThreadState::Merged,
        base_state: head_short.clone(),
        base_root,
        current_state: Some(head_short),
        merged_state: None,
        task: None,
        execution_path: exec_path.clone(),
        materialized_path: None,
        changed_paths: vec![],
        impact_categories: vec![],
        heavy_impact_paths: vec![],
        promotion_suggested: false,
        freshness: ThreadFreshness::Current,
        verification_summary: ThreadVerificationSummary::default(),
        confidence_summary: ThreadConfidenceSummary::default(),
        integration_policy_result: ThreadIntegrationPolicy::default(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        ephemeral: None,
        auto: false,
        shared_target_dir: None,
    };
    repo.refs()
        .set_thread(&ThreadName::new(&synthetic.thread), &head)
        .unwrap();
    manager.save(&synthetic).unwrap();

    let out = heddle(
        &["--output", "json", "thread", "cleanup", "--merged"],
        Some(temp.path()),
    )
    .expect("cleanup --merged must succeed even when id != thread (the mount-key invariant)");
    let value: Value = serde_json::from_str(&out).unwrap();
    let merged = value["merged"].as_array().expect("merged array");
    assert_eq!(
        merged.len(),
        1,
        "the synthetic merged thread is the only candidate; got {value}"
    );
    assert_eq!(
        merged[0]["thread"],
        Value::String("feat/light-mount".to_string()),
    );
    assert_eq!(
        merged[0]["id"],
        Value::String("synced-record-deadbeef".to_string()),
    );

    // Round-trip: load the record by id (the storage key) and confirm
    // it transitioned to Abandoned, and the on-disk checkout was
    // removed.
    let after = manager
        .load("synced-record-deadbeef")
        .unwrap()
        .expect("record still on disk (Abandoned, not deleted)");
    assert!(
        matches!(after.state, ThreadState::Abandoned),
        "synced record must be marked Abandoned after cleanup; got {:?}",
        after.state
    );
    assert!(
        !exec_path.exists(),
        "execution path must be removed; still present at {}",
        exec_path.display()
    );
}
