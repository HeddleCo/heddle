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
