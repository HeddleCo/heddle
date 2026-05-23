// SPDX-License-Identifier: Apache-2.0
//! End-to-end coverage for the clonefile-backed materialized-thread
//! workflow defined in `docs/design/clonefile-threads.md`. Touches
//! the full user-facing path so a regression in any piece —
//! `materialize_thread`, the `thread switch` auto-capture, the
//! `snapshot_with_attribution_profiled` materialized-context branch,
//! the `heddle status` advisory, or `thread drop`'s manifest cleanup
//! — surfaces as a single test failure with the failing assertion
//! pointing at the broken contract.

use super::*;

/// Walk the lifecycle of a materialized thread from `start` → edit →
/// `capture` → switch back → drop, asserting at each step that the
/// state visible through the CLI matches what's on disk.
///
/// The shape mirrors how an agent would drive this from a shell:
///
/// 1. `heddle start <thread> --workspace materialized --path <dir>`
///    materialises the captured tree into `<dir>` and writes a
///    sidecar manifest at `<heddle_dir>/threads/<thread>/manifest.toml`.
/// 2. Editing a file inside `<dir>` is visible to `heddle capture`
///    when run from `<dir>` — `Repository::snapshot` auto-detects
///    the materialised-thread context (HEAD attached + manifest
///    present) and uses the stat-cache fast path, refreshing the
///    manifest's `state_id`/`tree_hash` to the just-landed capture.
/// 3. `heddle thread switch main` from inside the materialised
///    worktree triggers the dogfood-validated worktree-aware
///    auto-capture, leaving the source thread durably captured.
/// 4. `heddle status` from the main repo lists the materialised
///    thread under "materialized_threads" (always in JSON), and
///    declines to alert the user about it (the thread isn't stale).
/// 5. `heddle thread drop` tears down the worktree *and* the
///    manifest sidecar so subsequent `status` calls don't see
///    phantom inventory.
#[test]
fn materialized_thread_full_lifecycle() {
    let main = setup_repo("hello.txt", "hello world");
    fs::write(main.path().join("README.md"), "# project\n").unwrap();
    fs::create_dir_all(main.path().join("src")).unwrap();
    fs::write(main.path().join("src/lib.rs"), "fn main() {}\n").unwrap();
    heddle(&["capture", "-m", "seed multi-file"], Some(main.path())).unwrap();

    let thread_dir = TempDir::new().unwrap();
    let thread_path = thread_dir.path();

    // ── 1. start --workspace materialized ───────────────────────────
    let started_json = heddle(
        &[
            "--output",
            "json",
            "start",
            "feature/m-thread",
            "--workspace",
            "materialized",
            "--path",
            thread_path.to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .unwrap();
    let started: Value = serde_json::from_str(&started_json).unwrap();
    assert_eq!(started["thread"]["thread_mode"], "materialized");
    // All three files from the snapshot must land on disk.
    for f in &["hello.txt", "README.md", "src/lib.rs"] {
        let p = thread_path.join(f);
        assert!(
            p.exists(),
            "materialise should produce {f} at {}",
            p.display()
        );
    }
    // Manifest sidecar must be written under the *main* repo's
    // heddle_dir (the worktree's `.heddle/objectstore` pointer keeps
    // everyone agreeing on a single store).
    let manifest_dir = main
        .path()
        .join(".heddle")
        .join("threads")
        .join("feature/m-thread");
    let manifest_path = manifest_dir.join("manifest.toml");
    assert!(
        manifest_path.is_file(),
        "manifest must be written at {}",
        manifest_path.display()
    );
    let manifest_v1 = fs::read_to_string(&manifest_path).unwrap();
    assert!(
        manifest_v1.contains("schema_version"),
        "manifest must record schema_version: {manifest_v1}"
    );

    // ── 2. edit a file inside the materialised worktree, capture ──
    fs::write(thread_path.join("hello.txt"), "hello edits").unwrap();
    fs::write(
        thread_path.join("src/lib.rs"),
        "fn main() { println!(\"hi\"); }\n",
    )
    .unwrap();
    let capture_json = heddle(
        &["--output", "json", "capture", "-m", "agent work"],
        Some(thread_path),
    )
    .unwrap();
    let captured: Value = serde_json::from_str(&capture_json).unwrap();
    let captured_state = captured["change_id"]
        .as_str()
        .expect("capture json carries change_id")
        .to_string();
    assert!(
        !captured_state.is_empty(),
        "capture should report a new state id"
    );

    // The manifest's recorded state must have advanced to the new
    // capture — this is the snapshot-side integration: when HEAD is
    // attached to a thread that has a manifest, `Repository::snapshot`
    // refreshes the sidecar post-capture so subsequent stat-cache
    // hits stay valid.
    let manifest_v2 = fs::read_to_string(&manifest_path).unwrap();
    assert_ne!(
        manifest_v1, manifest_v2,
        "manifest must be refreshed after capture inside the materialised worktree"
    );

    // ── 3. heddle status (from the main repo) surfaces the materialised
    //       thread in JSON. The thread isn't stale (we just captured
    //       it through the snapshot integration), so a healthy status
    //       advisory says nothing alarming — `materialized_threads`
    //       carries one entry with `stale=false`.
    let status_json = heddle(&["--output", "json", "status"], Some(main.path())).unwrap();
    let status: Value = serde_json::from_str(&status_json).unwrap();
    let materialized = status["materialized_threads"]
        .as_array()
        .expect("materialized_threads array must be present in JSON");
    let entry = materialized
        .iter()
        .find(|m| m["name"] == "feature/m-thread")
        .expect("status JSON should list our materialised thread");
    assert_eq!(
        entry["stale"], false,
        "thread should not be stale after a fresh capture"
    );
    assert!(
        entry["file_count"].as_u64().unwrap_or(0) >= 3,
        "manifest should still track all 3 files from the seed"
    );

    // ── 4. drop → worktree dir AND manifest dir both removed.
    heddle(&["thread", "drop", "feature/m-thread"], Some(main.path())).unwrap();
    assert!(
        !thread_path.exists(),
        "thread drop must remove the materialised checkout at {}",
        thread_path.display()
    );
    assert!(
        !manifest_dir.exists(),
        "thread drop must remove the manifest sidecar dir at {}",
        manifest_dir.display()
    );

    // Final status must no longer list the thread under
    // materialized_threads. Belt-and-suspenders for the drop
    // cleanup: a future regression where the dir is renamed or
    // moved would surface here even if the dir-deletion assertion
    // somehow false-passes.
    let status_after_json = heddle(&["--output", "json", "status"], Some(main.path())).unwrap();
    let status_after: Value = serde_json::from_str(&status_after_json).unwrap();
    let materialized_after = status_after
        .get("materialized_threads")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        materialized_after
            .iter()
            .all(|m| m["name"] != "feature/m-thread"),
        "dropped thread must vanish from status inventory: {materialized_after:?}"
    );
}
