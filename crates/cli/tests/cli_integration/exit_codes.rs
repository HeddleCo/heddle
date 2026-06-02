// SPDX-License-Identifier: Apache-2.0
//! Documented exit-code contract for the swept command subset.
//!
//! `docs/exit-codes.md` promises a sysexits-style taxonomy for the swept
//! commands (`init`, `status`, `verify`, `commit`, `merge`, `push`,
//! `pull`, and the three `bridge git` verbs). Agents branch on these
//! codes without parsing stderr, so a divergence between the documented
//! code and the runtime exit silently mis-handles a failure path.
//!
//! Persona round 3/5 (HeddleCo/heddle#252) caught two such divergences
//! (`push`/`commit` returning the `IoErr` catch-all instead of the
//! documented `Config`/`DataErr`) plus the `pull` and
//! `bridge git reconcile` siblings. These tests pin the documented code
//! for a reproducible documented condition of each swept command so the
//! contract can't regress.

use std::path::Path;

use tempfile::TempDir;

use super::{git_hermetic, heddle_output};

/// Assert `heddle <args>` exits with `expected`, surfacing stderr on
/// mismatch so a regression names the divergent code directly.
fn assert_exit(args: &[&str], dir: &Path, expected: i32) {
    let output =
        heddle_output(args, Some(dir)).unwrap_or_else(|err| panic!("spawn {args:?}: {err}"));
    let actual = output.status.code();
    assert_eq!(
        actual,
        Some(expected),
        "{args:?} should exit {expected} (documented in docs/exit-codes.md), got {actual:?}\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// A fresh, initialised native Heddle repo.
fn init_repo() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    assert_exit(
        &[
            "init",
            "--principal-name",
            "Heddle Test",
            "--principal-email",
            "heddle@test.example",
        ],
        temp.path(),
        0,
    );
    temp
}

/// Run `git <args>` in `dir` under an isolated environment, asserting success.
fn git(args: &[&str], dir: &Path) {
    git_hermetic(args, dir);
}

/// A git repo with one commit on `main`, adopted into a Heddle git overlay.
fn adopted_git_overlay() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let dir = temp.path();
    git(&["init", "-q", "-b", "main", "."], dir);
    git(&["config", "user.email", "heddle@test.example"], dir);
    git(&["config", "user.name", "Heddle Test"], dir);
    std::fs::write(dir.join("a.txt"), "hello\n").expect("write a.txt");
    git(&["add", "a.txt"], dir);
    git(&["commit", "-qm", "init"], dir);
    assert_exit(&["adopt"], dir, 0);
    temp
}

#[test]
fn init_exits_zero_on_success() {
    let temp = TempDir::new().expect("tempdir");
    assert_exit(
        &[
            "init",
            "--principal-name",
            "Heddle Test",
            "--principal-email",
            "heddle@test.example",
        ],
        temp.path(),
        0,
    );
}

#[test]
fn status_exits_zero_in_initialized_repo() {
    let repo = init_repo();
    assert_exit(&["status"], repo.path(), 0);
}

#[test]
fn verify_exits_zero_when_clean() {
    let repo = init_repo();
    std::fs::write(repo.path().join("f.txt"), "base\n").expect("write f.txt");
    assert_exit(&["commit", "-m", "base"], repo.path(), 0);
    assert_exit(&["verify"], repo.path(), 0);
}

#[test]
fn commit_with_nothing_staged_is_data_err() {
    // Documented: `65 DataErr` — well-formed input, semantically rejected.
    // Was the `74 IoErr` catch-all before HeddleCo/heddle#252.
    let repo = init_repo();
    std::fs::write(repo.path().join("f.txt"), "base\n").expect("write f.txt");
    assert_exit(&["commit", "-m", "base"], repo.path(), 0);
    // Second commit with a clean worktree: nothing to commit.
    assert_exit(&["commit", "-m", "again"], repo.path(), 65);
}

#[test]
fn push_without_remote_is_config() {
    // Documented: `78 Config` — no remote configured. Was `74 IoErr`.
    let repo = init_repo();
    assert_exit(&["push"], repo.path(), 78);
}

#[test]
fn pull_without_remote_is_config() {
    // Documented: `78 Config` — no remote configured. Was `74 IoErr`.
    let repo = init_repo();
    assert_exit(&["pull"], repo.path(), 78);
}

#[test]
fn merge_preview_exits_zero() {
    // Documented: `0 ok`. A fast-forwardable thread previews cleanly.
    let repo = init_repo();
    std::fs::write(repo.path().join("f.txt"), "base\n").expect("write base");
    assert_exit(&["commit", "-m", "base"], repo.path(), 0);
    assert_exit(&["fork", "--name", "feature"], repo.path(), 0);
    assert_exit(&["goto", "feature"], repo.path(), 0);
    std::fs::write(repo.path().join("f.txt"), "base\nfeat\n").expect("write feat");
    assert_exit(&["commit", "-m", "feat"], repo.path(), 0);
    assert_exit(&["goto", "main"], repo.path(), 0);
    assert_exit(&["merge", "feature", "--preview"], repo.path(), 0);
}

#[test]
fn bridge_git_import_exits_zero() {
    // Documented: `0 ok`.
    let repo = adopted_git_overlay();
    assert_exit(
        &["bridge", "git", "import", "--ref", "main"],
        repo.path(),
        0,
    );
}

#[test]
fn bridge_git_sync_exits_zero() {
    // Documented: `0 ok`.
    let repo = adopted_git_overlay();
    assert_exit(
        &["bridge", "git", "import", "--ref", "main"],
        repo.path(),
        0,
    );
    assert_exit(&["bridge", "git", "sync"], repo.path(), 0);
}

#[test]
fn bridge_git_reconcile_without_side_is_data_err() {
    // Documented: `65 DataErr` — manual resolution required. The
    // `reconcile_direction_required` refusal (no `--prefer` side) was the
    // `74 IoErr` catch-all before HeddleCo/heddle#252.
    let repo = adopted_git_overlay();
    assert_exit(
        &["bridge", "git", "import", "--ref", "main"],
        repo.path(),
        0,
    );
    assert_exit(
        &["bridge", "git", "reconcile", "--ref", "main"],
        repo.path(),
        65,
    );
}

#[test]
fn unconfigured_remote_keeps_no_default_remote_phrasing() {
    // Regression guard for the string sentinel `from_error` relies on for
    // the raw `resolve_remote` path (HeddleCo/heddle#252). If the
    // `RemoteError::NotFound` phrasing changes, the sentinel must follow.
    let repo = init_repo();
    let output = heddle_output(&["--output", "json", "pull"], Some(repo.path()))
        .expect("spawn pull --output json");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim().lines().next().unwrap())
        .unwrap_or_else(|err| panic!("pull envelope not JSON: {err}\n  stderr: {stderr}"));
    assert_eq!(
        output.status.code(),
        Some(78),
        "pull without a remote should exit 78; envelope: {envelope}"
    );
}
