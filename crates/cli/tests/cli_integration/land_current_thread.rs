// SPDX-License-Identifier: Apache-2.0
//! Exact field-study repros from HeddleCo/heddle#1436 (comment 2026-08-19).
//!
//! Isolated native thread `feature/search` via
//! `heddle start feature/search --path ../<repo>-search`.
//! Done-criteria are those commands and exits:
//! - bare `heddle land` must not exit 74 / treat `main` as an imported Git ref
//! - bare `heddle land --dry-run` must preview the current thread, not `main`,
//!   and must not claim Git-ref writes or network I/O
//! - `heddle ready` then `land --thread feature/search` works; a second run
//!   is already-landed, not another "automatic integration merge" at exit 0
//! - Next verbs stay native (`heddle thread list`). No `repair git`.

use std::{fs, path::Path, path::PathBuf, process::Output, str};

use serde_json::Value;
use tempfile::TempDir;

use super::{heddle, heddle_output};

fn sibling_checkout_path(repo: &std::path::Path, suffix: &str) -> PathBuf {
    let repo_name = repo
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo");
    repo.with_file_name(format!("{repo_name}-{suffix}"))
}

fn stdout(output: &Output) -> &str {
    str::from_utf8(&output.stdout).unwrap_or("")
}

fn stderr(output: &Output) -> &str {
    str::from_utf8(&output.stderr).unwrap_or("")
}

fn combined(output: &Output) -> String {
    format!("{}\n{}", stdout(output), stderr(output))
}

/// Native isolated checkout with captured work. Does not run `ready`.
fn setup_feature_search() -> (TempDir, PathBuf) {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let checkout = sibling_checkout_path(temp.path(), "search");
    let checkout_arg = checkout.to_str().expect("checkout path utf8");
    heddle(
        &["start", "feature/search", "--path", checkout_arg],
        Some(temp.path()),
    )
    .unwrap();
    fs::write(checkout.join("search.txt"), "find it\n").unwrap();
    heddle(&["capture", "-m", "feature work"], Some(&checkout)).unwrap();
    (temp, checkout)
}

fn assert_no_repair_git(output: &Output, context: &str) {
    let text = combined(output);
    assert!(
        !text.contains("repair git") && !text.contains("fsck repair"),
        "{context} must not recommend repair git on a native repo:\n{text}"
    );
}

/// Field study: from the isolated checkout, `heddle land --dry-run` then
/// bare `heddle land`. Pre-fix: dry-run previewed `main` and claimed Git
/// writes; bare land exited 74 with `repair git --ref main`.
#[test]
fn field_study_bare_land_and_dry_run_use_current_thread() {
    let (main, checkout) = setup_feature_search();

    let dry = heddle_output(&["land", "--dry-run"], Some(&checkout))
        .expect("land --dry-run should spawn");
    assert_eq!(
        dry.status.code(),
        Some(0),
        "bare land --dry-run must succeed:\n{}",
        combined(&dry)
    );
    let dry_text = combined(&dry);
    assert!(
        dry_text.contains("feature/search"),
        "dry-run must preview the current thread:\n{dry_text}"
    );
    assert!(
        !dry_text.contains("land thread 'main'") && !dry_text.contains("thread 'main' not found"),
        "dry-run must not preview main as the land subject:\n{dry_text}"
    );
    assert!(
        !dry_text.contains("writes Git refs") && !dry_text.contains("network I/O"),
        "native dry-run must not claim Git-ref writes or network I/O:\n{dry_text}"
    );
    assert_no_repair_git(&dry, "land --dry-run");

    let land = heddle_output(&["land"], Some(&checkout)).expect("heddle land should spawn");
    assert_ne!(
        land.status.code(),
        Some(74),
        "bare land must not exit 74 treating main as an imported Git ref:\n{}",
        combined(&land)
    );
    assert_eq!(
        land.status.code(),
        Some(0),
        "bare land from the isolated checkout must land the current thread:\n{}",
        combined(&land)
    );
    let land_text = combined(&land);
    assert!(
        land_text.contains("feature/search"),
        "bare land must name the current thread:\n{land_text}"
    );
    assert!(
        !land_text.contains("imported Git ref"),
        "bare land must not treat main as an imported Git ref:\n{land_text}"
    );
    assert_no_repair_git(&land, "bare land");
    assert_eq!(
        fs::read_to_string(main.path().join("search.txt")).unwrap(),
        "find it\n",
        "bare land must integrate the current thread into the target"
    );
}

/// Field study: `heddle ready` prints `heddle --repo … land --thread
/// feature/search`. That command works. A second run must be already-landed,
/// not another successful "automatic integration merge" at exit 0.
#[test]
fn field_study_ready_then_double_land_thread_is_already_landed() {
    let (main, checkout) = setup_feature_search();

    let ready = heddle_output(&["ready"], Some(&checkout)).expect("ready should spawn");
    assert_eq!(
        ready.status.code(),
        Some(0),
        "ready must succeed:\n{}",
        combined(&ready)
    );
    let ready_text = combined(&ready);
    assert!(
        ready_text.contains("land --thread feature/search"),
        "ready must recommend land --thread feature/search:\n{ready_text}"
    );
    assert_no_repair_git(&ready, "ready");

    let first = run_land_thread(main.path(), &checkout);
    assert_eq!(
        first.status.code(),
        Some(0),
        "land --thread feature/search must succeed after ready:\n{}",
        combined(&first)
    );
    assert!(
        combined(&first).contains("feature/search"),
        "first land --thread must name the thread:\n{}",
        combined(&first)
    );
    assert_no_repair_git(&first, "first land --thread");
    assert_eq!(
        fs::read_to_string(main.path().join("search.txt")).unwrap(),
        "find it\n"
    );

    let second = run_land_thread(main.path(), &checkout);
    assert_eq!(
        second.status.code(),
        Some(0),
        "second land --thread must stay a clean exit 0:\n{}",
        combined(&second)
    );
    let second_text = combined(&second);
    assert!(
        second_text.contains("already landed"),
        "second land --thread must be already-landed, not a second merge:\n{second_text}"
    );
    assert!(
        !second_text.contains("automatic integration merge"),
        "second land --thread must not claim another automatic integration merge:\n{second_text}"
    );
    assert_no_repair_git(&second, "second land --thread");
}

fn run_land_thread(main: &Path, checkout: &Path) -> Output {
    // Field study ran the ready breadcrumb: `heddle --repo … land --thread feature/search`.
    heddle_output(
        &[
            "--repo",
            main.to_str().expect("main path utf8"),
            "land",
            "--thread",
            "feature/search",
        ],
        Some(checkout),
    )
    .expect("land --thread feature/search should spawn")
}

/// Same comment: exit 74 / `repair git --ref main` is the wrong recovery
/// when authority is native. Explicit `--thread main` must stay on
/// `heddle thread list`.
#[test]
fn land_unmanaged_main_on_native_recommends_thread_list() {
    let (_main, checkout) = setup_feature_search();

    let output = heddle_output(
        &["--output", "json", "land", "--thread", "main"],
        Some(&checkout),
    )
    .expect("land --thread main should spawn");
    assert_eq!(
        output.status.code(),
        Some(74),
        "unmanaged main still fails closed (field-study exit 74), but recovery must change:\n{}",
        combined(&output)
    );
    let stderr = stderr(&output);
    let envelope: Value = serde_json::from_str(
        stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or(stderr.trim()),
    )
    .unwrap_or_else(|err| panic!("JSON envelope: {err}\n{stderr}"));
    assert_eq!(envelope["kind"], "imported_git_ref_not_managed_thread");
    assert_eq!(envelope["primary_command"], "heddle thread list");
    assert_no_repair_git(&output, "land --thread main");
}
