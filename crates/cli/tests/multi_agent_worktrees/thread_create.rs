// SPDX-License-Identifier: Apache-2.0
//! Regression coverage for `heddle thread create`.
//!
//! Before fcc4c7a5, `thread create` wrote a thread ref + an oplog entry
//! but no Thread record file. Subsequent commands that go through
//! `ThreadManager::load` (`delegate`, `ship`, `thread show`'s record
//! path, integration policy) would fail with `Thread '<name>' not
//! found` even though `thread switch` (which only consults refs) still
//! worked. The fix in `cmd_thread_create` now persists a Lightweight
//! ref-only Thread record alongside the ref. These tests pin that
//! behavior.
//!
//! There is no migration for half-baked threads created before the
//! fix; the user is expected to drop and recreate.

use std::fs;

use serde_json::Value;

use super::*;

/// The original repro: `thread create` followed by `delegate` should
/// not fail with "Thread '<name>' not found". Before the fix, the
/// missing Thread record left `delegate` (which routes through
/// `ThreadManager::load`) unable to discover the parent.
#[test]
fn test_thread_create_then_delegate() {
    let main = setup_repo("main.rs", "fn main() {}");

    heddle(&["thread", "create", "modulo-race"], Some(main.path())).unwrap();

    // Switch to the new thread so `delegate --parent modulo-race` has
    // a valid parent on disk. (The repro uses `--parent`; we mirror
    // it.)
    let delegate_out = heddle(
        &[
            "--json",
            "delegate",
            "--parent",
            "modulo-race",
            "task:anthropic:claude-sonnet-4-6",
        ],
        Some(main.path()),
    )
    .expect(
        "delegate must succeed once `thread create` writes a record; \
         pre-fix this errored with `Thread 'modulo-race' not found`",
    );

    let delegated: Value = serde_json::from_str(&delegate_out).unwrap();
    let children = delegated["delegated"].as_array().unwrap();
    assert!(
        !children.is_empty(),
        "delegate should create at least one child thread, got: {delegated}"
    );
}

/// `thread create` must produce a ThreadManager-loadable record. We
/// check both the on-disk shape (a `.heddle/thread_records/<hex>.toml`
/// file) and the round-trip through the loader.
#[test]
fn test_thread_create_writes_record() {
    let main = setup_repo("main.rs", "fn main() {}");
    let name = "ref-only-thread";

    heddle(&["thread", "create", name], Some(main.path())).unwrap();

    // The record store hex-encodes the thread id as the filename.
    let encoded: String =
        name.as_bytes()
            .iter()
            .fold(String::with_capacity(name.len() * 2), |mut acc, b| {
                use std::fmt::Write as _;
                let _ = write!(&mut acc, "{:02x}", b);
                acc
            });
    let record_path = main
        .path()
        .join(".heddle")
        .join("thread_records")
        .join(format!("{encoded}.toml"));
    assert!(
        record_path.exists(),
        "thread create should write a record file at {}",
        record_path.display()
    );

    // And the loader must surface the thread.
    let show_out = heddle(&["--json", "thread", "show", name], Some(main.path())).expect(
        "thread show should succeed after thread create — it routes \
             through find_thread_summary which reads the record store",
    );
    let summary: Value = serde_json::from_str(&show_out).unwrap();
    assert_eq!(summary["name"], name);
    assert_eq!(
        summary["thread_state"], "active",
        "ref-only thread should be Active, got: {summary}"
    );
}

/// `thread show` should report the ref-only thread's record fields:
/// no execution_path/path, lightweight mode, active state.
#[test]
fn test_thread_create_then_show_via_record() {
    let main = setup_repo("main.rs", "fn main() {}");
    let name = "show-via-record";

    heddle(&["thread", "create", name], Some(main.path())).unwrap();

    let show_out = heddle(&["--json", "thread", "show", name], Some(main.path())).unwrap();
    let summary: Value = serde_json::from_str(&show_out).unwrap();

    assert_eq!(summary["name"], name);
    assert_eq!(
        summary["thread_mode"], "materialized",
        "no-worktree create records the closest existing variant; \
         got: {summary}"
    );
    assert_eq!(summary["thread_state"], "active");
    assert!(
        summary["path"].is_null(),
        "create does not materialize a worktree, so path must be null; \
         got: {summary}"
    );
    // The summary maps `Thread::execution_path: PathBuf` to
    // `Option<String>` via `Some(path.display().to_string())`, so an
    // empty `PathBuf::new()` serializes as `""` rather than null. We
    // only require that no real path leaked in (no worktree was
    // materialized).
    assert_eq!(
        summary["execution_path"], "",
        "create does not materialize a worktree, so execution_path must \
         be empty; got: {summary}"
    );
    assert!(
        summary["base_state"].is_string(),
        "base_state should be set from current HEAD; got: {summary}"
    );
}

/// Full happy path: create, switch, capture changes, delegate. Each
/// step should work and produce sensible state. This is the workflow
/// the bug originally broke.
#[test]
fn test_thread_create_then_switch_then_capture_then_delegate() {
    let main = setup_repo("main.rs", "fn main() {}");
    let parent = "feature/parent";

    heddle(&["thread", "create", parent], Some(main.path())).unwrap();
    heddle(&["thread", "switch", parent], Some(main.path())).unwrap();

    // Now on the new thread; capture should land a state on it.
    fs::write(main.path().join("lib.rs"), "pub fn lib() {}").unwrap();
    heddle(&["capture", "-m", "feat: add lib"], Some(main.path())).unwrap();

    // Sanity-check we are tracking the parent thread.
    let track = head_track(main.path());
    assert_eq!(track, parent, "HEAD should still be attached to {parent}");

    // Delegate from the parent. Pre-fix this errored even though the
    // ref existed, because the record was missing.
    let delegate_out = heddle(
        &[
            "--json",
            "delegate",
            "--parent",
            parent,
            "task:anthropic:claude-sonnet-4-6",
        ],
        Some(main.path()),
    )
    .unwrap();

    let delegated: Value = serde_json::from_str(&delegate_out).unwrap();
    let children = delegated["delegated"].as_array().unwrap();
    assert!(
        !children.is_empty(),
        "delegate from a switched-and-captured thread should produce \
         a child, got: {delegated}"
    );
}