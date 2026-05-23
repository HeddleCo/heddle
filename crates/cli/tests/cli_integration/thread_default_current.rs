// SPDX-License-Identifier: Apache-2.0
//! Regression coverage for defaulting `heddle thread show`,
//! `heddle thread refresh`, and `heddle thread captures` to the
//! current thread when the positional argument is omitted.
//!
//! Pre-fix, all three subcommands required a `<THREAD>` argument even
//! when the working checkout was attached to a thread. This was a
//! high-frequency ergonomic gap (the user is already inside a thread
//! checkout 99% of the time they ask for its details). The fix makes
//! the positional `Option<String>` and falls back to
//! `Repository::current_lane()`. When HEAD is detached, the command
//! still errors — but with an explicit message that names both the
//! missing positional and the unavailable fallback.

use std::fs;

use repo::{Repository, ThreadManager};
use serde_json::Value;
use tempfile::TempDir;

use super::heddle;

/// Bootstrap a fresh repo with one snapshot. We then run
/// `thread create` followed by `thread switch` so HEAD is attached
/// to the new thread; this is the state any `heddle start <name>`
/// user is in once they `cd` into the materialized worktree. We
/// can't use `start` directly here because `start` does not change
/// the calling shell's cwd (the new thread's checkout lives
/// elsewhere), and we need the test harness to drive heddle from a
/// directory whose HEAD is attached to the target thread.
fn setup_thread(name: &str) -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "init"], Some(temp.path())).unwrap();
    heddle(&["thread", "create", name], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", name], Some(temp.path())).unwrap();
    temp
}

/// `heddle thread show` with no positional should resolve to whatever
/// thread the working checkout is attached to and return that
/// thread's metadata — same shape as passing the name explicitly.
#[test]
fn thread_show_without_arg_resolves_current_thread() {
    let repo = setup_thread("probe");

    let omitted = heddle(&["--json", "thread", "show"], Some(repo.path()))
        .expect("thread show should succeed without a positional when HEAD is attached");
    let with_arg = heddle(&["--json", "thread", "show", "probe"], Some(repo.path()))
        .expect("thread show with explicit positional should still succeed");

    let omitted: Value = serde_json::from_str(&omitted).unwrap();
    let with_arg: Value = serde_json::from_str(&with_arg).unwrap();

    assert_eq!(
        omitted["name"], with_arg["name"],
        "omitted positional should resolve to the same thread as explicit"
    );
    assert_eq!(omitted["name"].as_str(), Some("probe"));
}

/// `heddle thread captures` with no positional should also default
/// to the current thread. Same shape as `show`.
#[test]
fn thread_captures_without_arg_resolves_current_thread() {
    let repo = setup_thread("probe");

    // Should not require the name; should not panic; should produce
    // *some* output (the captures list may be empty for a fresh
    // thread, which is fine — we only assert that the command exits 0).
    heddle(&["--json", "thread", "captures"], Some(repo.path()))
        .expect("thread captures should succeed without a positional when HEAD is attached");
}

/// When HEAD is detached and no positional is supplied, the command
/// must exit non-zero with a precise message that names both the
/// missing positional and the unavailable fallback.
///
/// `heddle init` auto-attaches HEAD to `main`, so we have to detach
/// it manually: overwrite `.heddle/HEAD` with a parseable change id
/// (the ID of the snapshot we just created), which the `Head` parser
/// will read as `Detached`. From a detached HEAD, `current_lane()`
/// returns `None` — exactly the state we need to exercise the
/// fallback branch.
#[test]
fn thread_show_without_arg_errors_when_no_current_thread() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "init"], Some(temp.path())).unwrap();
    // Snapshot's wire format only exposes the short id; we need the
    // full form to write a parseable Detached HEAD. Reach in via the
    // `repo` API, which both the CLI and these tests already share.
    let repo = Repository::open(temp.path()).unwrap();
    let head = repo
        .head()
        .unwrap()
        .expect("repo should have a current state after snapshot");
    fs::write(
        temp.path().join(".heddle").join("HEAD"),
        format!("{}\n", head.to_string_full()),
    )
    .unwrap();
    drop(repo);

    let err = heddle(&["thread", "show"], Some(temp.path()))
        .expect_err("thread show should fail when HEAD has no attached thread");

    assert!(
        err.contains("no thread specified and no current thread"),
        "expected the explicit fallback error message; got: {err}"
    );
    assert!(
        err.contains("pass <THREAD> explicitly"),
        "expected guidance on how to recover; got: {err}"
    );
}

/// Regression: when HEAD is detached but the working checkout is
/// still associated with a thread record by `execution_path`,
/// `thread show` (and friends) must resolve via the broader
/// `current_thread` lookup rather than only consulting
/// `current_lane()`. PR #69's review surfaced this — the helper was
/// hard-failing inside materialized worktrees whose HEAD had drifted
/// detached, even though the thread record's `execution_path` still
/// pointed at the cwd.
///
/// We seed a thread record whose `execution_path` is the repo root,
/// then detach HEAD by overwriting `.heddle/HEAD` with the snapshot's
/// state id (same trick `thread_show_without_arg_errors_when_no_current_thread`
/// uses). After that, `thread show` (no positional) must resolve to
/// the seeded thread and succeed.
#[test]
fn thread_show_without_arg_resolves_via_execution_path_when_detached() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "init"], Some(temp.path())).unwrap();
    heddle(&["thread", "create", "feat/probe"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feat/probe"], Some(temp.path())).unwrap();

    // Pin the seeded thread's execution_path to the repo root so
    // `current_thread` will find it via `find_by_execution_root`.
    let repo = Repository::open(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());
    let mut thread = manager
        .load("feat/probe")
        .unwrap()
        .expect("thread record exists after create");
    thread.execution_path = temp.path().to_path_buf();
    manager.save(&thread).unwrap();

    // Detach HEAD to the snapshot's state id. Overwriting
    // `.heddle/HEAD` directly is what `Head` parses as Detached.
    let head = repo
        .head()
        .unwrap()
        .expect("repo should have a current state after snapshot");
    fs::write(
        temp.path().join(".heddle").join("HEAD"),
        format!("{}\n", head.to_string_full()),
    )
    .unwrap();
    drop(repo);

    // Sanity: confirm `thread show` with the explicit positional
    // still works, since that's the baseline we're matching.
    let with_arg = heddle(
        &["--json", "thread", "show", "feat/probe"],
        Some(temp.path()),
    )
    .expect("thread show with positional should succeed");
    let with_arg: Value = serde_json::from_str(&with_arg).unwrap();
    assert_eq!(with_arg["name"].as_str(), Some("feat/probe"));

    // The actual regression assertion: no positional, HEAD detached,
    // but execution_path-keyed lookup resolves the thread.
    let omitted = heddle(&["--json", "thread", "show"], Some(temp.path())).expect(
        "thread show without positional must resolve via execution_path when HEAD is detached",
    );
    let omitted: Value = serde_json::from_str(&omitted).unwrap();
    assert_eq!(
        omitted["name"].as_str(),
        Some("feat/probe"),
        "execution-path fallback should resolve to the seeded thread; got {omitted}"
    );
}

/// `heddle thread refresh` with no positional should ALSO default to
/// the current thread. We don't assert the refresh succeeds (refresh
/// requires a target thread, which may or may not be set on a
/// freshly-created thread); we only assert that the resolution path
/// is reached — i.e., that clap doesn't reject the missing positional
/// before our code runs. Distinguishing the two failure modes is the
/// point: pre-fix this errored at the clap layer with "the following
/// required arguments were not provided: <THREAD>".
#[test]
fn thread_refresh_without_arg_does_not_require_positional() {
    let repo = setup_thread("probe");

    let result = heddle(&["thread", "refresh"], Some(repo.path()));

    // Either it succeeds, or it fails for some downstream reason
    // (e.g., "no target thread"). What it MUST NOT do is fail with
    // clap's missing-argument error, which is what the pre-fix
    // behavior produced.
    if let Err(err) = result {
        assert!(
            !err.contains("required arguments were not provided"),
            "thread refresh should not require <THREAD> at the clap layer; got: {err}"
        );
        assert!(
            !err.contains("<THREAD>"),
            "thread refresh should not surface <THREAD> as a missing argument; got: {err}"
        );
    }
}
