// SPDX-License-Identifier: Apache-2.0

use serde_json::Value;
use tempfile::TempDir;

use super::{heddle, heddle_output};

const BANNED_NEXT_ACTION_FRAGMENTS: &[&str] = &[
    "heddle checkpoint",
    "heddle capture",
    "heddle ship",
    "heddle merge",
    "heddle thread refresh",
    "heddle thread resolve",
];

fn json(args: &[&str], cwd: &std::path::Path) -> Value {
    let output = heddle_output(args, Some(cwd)).unwrap_or_else(|err| {
        panic!("`heddle {}` should run: {err}", args.join(" "))
    });
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
    heddle(&["commit", "-m", "base"], Some(temp.path())).unwrap();
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
fn native_dirty_status_and_thread_list_suggest_commit_not_capture_or_checkpoint() {
    let repo = setup_native_repo();
    std::fs::write(repo.path().join("dirty.txt"), "dirty\n").unwrap();

    let status = json(&["--output", "json", "status"], repo.path());
    assert_eq!(status["recommended_action"], "heddle commit -m \"...\"");
    assert_no_banned_next_actions(&status);

    let threads = json(&["--output", "json", "thread", "list"], repo.path());
    assert_no_banned_next_actions(&threads);
}

#[test]
fn dirty_isolated_checkout_suggests_commit_and_never_checkpoint() {
    let (_main, checkout_owner, execution_path) = setup_managed_thread("feature/dirty");
    let checkout = std::path::Path::new(&execution_path);
    std::fs::write(checkout.join("dirty.txt"), "dirty\n").unwrap();

    let status = json(&["--output", "json", "status"], checkout);
    assert_eq!(status["recommended_action"], "heddle commit -m \"...\"");
    assert_no_banned_next_actions(&status);

    drop(checkout_owner);
}

#[test]
fn ready_thread_surfaces_land_across_ready_show_and_list() {
    let (main, checkout_owner, execution_path) = setup_managed_thread("feature/ready-land");
    let checkout = std::path::Path::new(&execution_path);
    std::fs::write(checkout.join("feature.txt"), "feature\n").unwrap();
    heddle(&["commit", "-m", "feature"], Some(checkout)).unwrap();

    let ready = json(
        &["--output", "json", "ready", "--thread", "feature/ready-land"],
        main.path(),
    );
    assert_eq!(ready["recommended_action"], "heddle land --thread feature/ready-land --no-push");
    assert_eq!(
        ready["report"]["recommended_action"],
        "heddle land --thread feature/ready-land --no-push"
    );
    assert_no_banned_next_actions(&ready);

    let shown = json(
        &["--output", "json", "thread", "show", "feature/ready-land"],
        main.path(),
    );
    assert_eq!(
        shown["next_action"],
        "heddle land --thread feature/ready-land --no-push"
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
        "heddle land --thread feature/ready-land --no-push"
    );
    assert_no_banned_next_actions(&listed);

    drop(checkout_owner);
}

#[test]
fn stale_managed_thread_suggests_sync_not_refresh_or_merge_preview() {
    let (main, checkout_owner, execution_path) = setup_managed_thread("feature/stale-sync");
    let checkout = std::path::Path::new(&execution_path);
    std::fs::write(checkout.join("feature.txt"), "feature\n").unwrap();
    heddle(&["commit", "-m", "feature"], Some(checkout)).unwrap();

    std::fs::write(main.path().join("base.txt"), "base changed\n").unwrap();
    heddle(&["commit", "-m", "advance main"], Some(main.path())).unwrap();

    let ready = json(
        &["--output", "json", "ready", "--thread", "feature/stale-sync"],
        main.path(),
    );
    assert_eq!(
        ready["recommended_action"],
        "heddle sync --thread feature/stale-sync"
    );
    assert_eq!(
        ready["report"]["recommended_action"],
        "heddle sync --thread feature/stale-sync"
    );
    assert_no_banned_next_actions(&ready);

    let shown = json(
        &["--output", "json", "thread", "show", "feature/stale-sync"],
        main.path(),
    );
    assert_eq!(
        shown["next_action"],
        "heddle sync --thread feature/stale-sync"
    );
    assert_no_banned_next_actions(&shown);

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
    heddle(&["commit", "-m", "thread edit"], Some(checkout)).unwrap();

    std::fs::write(main.path().join("base.txt"), "main change\n").unwrap();
    heddle(&["commit", "-m", "advance main"], Some(main.path())).unwrap();

    let sync = json(
        &["--output", "json", "sync", "--thread", "feature/conflict-sync"],
        main.path(),
    );
    assert_eq!(sync["status"], "blocked", "conflicting sync must block: {sync}");
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
