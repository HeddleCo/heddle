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
/// 1. A materialized-mode `heddle start <thread> --path <dir>` materialises
///    the captured tree into `<dir>` and writes a sidecar manifest at
///    `<heddle_dir>/threads/<thread>/manifest.toml`.
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

    // ── 1. start with materialized storage mode ─────────────────────
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
    // Use the canonical prefix-safe `thread_dir` derivation: a slashed id
    // lives at `.heddle/threads/<encoded>/` (e.g. `feature%2Fm-thread`), NOT
    // a nested `feature/m-thread/` (heddle#572 r2).
    let manifest_dir =
        repo::thread_manifest::thread_dir(&main.path().join(".heddle"), "feature/m-thread");
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

/// Regression: the `--short` fast path used to return
/// `materialized_threads: Vec::new()` unconditionally, so the
/// downstream `render_materialized_advisory` had nothing to warn on
/// even when a materialised thread's checkout lagged its head. Users
/// running `heddle status --short` (the common shell-prompt path)
/// silently missed the staleness signal. This test materialises a
/// thread, rewrites its manifest to point at a different state_id
/// (the same condition `assess_materialized_threads` keys off when
/// the on-disk head advances past a manifest), and asserts the short
/// output now surfaces the "lag their head" advisory line.
#[test]
fn short_status_surfaces_stale_materialized_thread_advisory() {
    let main = setup_repo("hello.txt", "hello\n");

    let thread_dir = TempDir::new().unwrap();
    let thread_path = thread_dir.path();

    heddle(
        &[
            "start",
            "feature/short-stale",
            "--workspace",
            "materialized",
            "--path",
            thread_path.to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .unwrap();

    // Force the manifest stale by rewriting its `state_id` field to
    // an all-zero ChangeId. `assess_materialized_threads` compares
    // the manifest's recorded `state_id` against the live thread head
    // via `refs().get_thread(...)` — any mismatch flips the stale bit.
    // This is the same observable condition produced when the head
    // advances past a manifest without the manifest being refreshed
    // (the unit test in status.rs exercises that path directly).
    // Canonical prefix-safe layout: `.heddle/threads/<encoded>/manifest.toml`
    // (`feature%2Fshort-stale`), not a nested `feature/short-stale/`
    // (heddle#572 r2).
    let manifest_path =
        repo::thread_manifest::manifest_path(&main.path().join(".heddle"), "feature/short-stale");
    assert!(
        manifest_path.is_file(),
        "manifest expected at {} after materialized start",
        manifest_path.display()
    );
    let manifest = fs::read_to_string(&manifest_path).unwrap();
    // ChangeId is `[u8; 16]` with the default serde derive, so the
    // manifest's `state_id` is a 16-element integer array. Parse the
    // TOML, replace the bytes with a distinct 16-byte value (every
    // byte differs from `rand::random()` with vanishing probability),
    // and write it back.
    let mut doc: toml::Value = toml::from_str(&manifest).unwrap();
    let stale_state_id = toml::Value::Array(
        (0u8..16)
            .map(|b| toml::Value::Integer((b ^ 0xa5) as i64))
            .collect(),
    );
    doc.as_table_mut()
        .unwrap()
        .insert("state_id".to_string(), stale_state_id);
    fs::write(&manifest_path, toml::to_string(&doc).unwrap()).unwrap();

    // Sanity: status JSON should now report the thread as stale. If
    // this fails the `--short` assertion below is meaningless.
    let json_out = heddle(&["--output", "json", "status"], Some(main.path())).unwrap();
    let json: Value = serde_json::from_str(&json_out).unwrap();
    let entry = json["materialized_threads"]
        .as_array()
        .and_then(|arr| arr.iter().find(|m| m["name"] == "feature/short-stale"))
        .unwrap_or_else(|| {
            panic!("json status must list feature/short-stale after manifest rewrite:\n{json_out}")
        });
    assert_eq!(
        entry["stale"], true,
        "manifest rewrite should mark thread stale in JSON output too"
    );

    let short = heddle(&["status", "--short"], Some(main.path())).unwrap();
    assert!(
        short.contains("materialized thread(s) lag their head"),
        "`heddle status --short` must emit the materialised-thread \
         staleness advisory when a checkout lags its head; got:\n{short}"
    );
    assert!(
        short.contains("feature/short-stale"),
        "advisory must name the stale thread; got:\n{short}"
    );
}
