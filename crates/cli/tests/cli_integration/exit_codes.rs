// SPDX-License-Identifier: Apache-2.0
//! Documented exit-code contract for the swept command subset.
//!
//! `docs/exit-codes.md` promises a sysexits-style taxonomy for the swept
//! commands (`init`, `status`, `verify`, `commit`, `push`,
//! `pull`, and the Git import/sync/repair verbs). Agents branch on these
//! codes without parsing stderr, so a divergence between the documented
//! code and the runtime exit silently mis-handles a failure path.
//!
//! Persona round 3/5 (HeddleCo/heddle#252) caught two such divergences
//! (`push`/`commit` returning the `IoErr` catch-all instead of the
//! documented `Config`/`DataErr`) plus the `pull` and
//! `fsck repair git` siblings. These tests pin the documented code
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
    assert_exit(&["capture", "-m", "base"], repo.path(), 0);
    assert_exit(&["verify"], repo.path(), 0);
}

#[test]
fn commit_without_git_overlay_is_data_err() {
    let repo = init_repo();
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
fn import_git_exits_zero() {
    // Documented: `0 ok`.
    let repo = adopted_git_overlay();
    assert_exit(&["import", "git", "--ref", "main"], repo.path(), 0);
}

#[test]
fn sync_git_exits_zero() {
    // Documented: `0 ok`.
    let repo = adopted_git_overlay();
    assert_exit(&["import", "git", "--ref", "main"], repo.path(), 0);
    assert_exit(&["sync", "git"], repo.path(), 0);
}

#[test]
fn fsck_git_repair_against_native_authority_is_data_err() {
    // Documented: `65 DataErr` — the retained Git adapter cannot become
    // authoritative through a repair command in a native repository.
    let repo = adopted_git_overlay();
    assert_exit(&["import", "git", "--ref", "main"], repo.path(), 0);
    assert_exit(
        &["fsck", "repair", "git", "--prefer", "git", "--ref", "main"],
        repo.path(),
        65,
    );
}

#[test]
fn unsupported_output_json_is_data_err() {
    // HeddleCo/heddle#648: `--output json` against a text-only command is
    // well-formed syntax the command semantically rejects — DataErr (65),
    // not Usage (64). Agents treat 64 as "fix your argv" and may
    // retry-with-mutation instead of falling back to `--output text`.
    let repo = init_repo();
    assert_exit(
        &["--output", "json", "shell", "completion", "bash"],
        repo.path(),
        65,
    );
}

#[test]
fn unsupported_output_json_compact_is_data_err() {
    // Sibling gate: `--output json-compact` against a command without a
    // compact projection. Same semantics, same code (HeddleCo/heddle#648).
    // The envelope's `exit_code` field must agree with the process exit.
    let repo = init_repo();
    let output = heddle_output(&["--output", "json-compact", "log"], Some(repo.path()))
        .expect("spawn log --output json-compact");
    assert_eq!(
        output.status.code(),
        Some(65),
        "json-compact rejection should exit 65 (DataErr); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|err| panic!("rejection envelope not JSON: {err}\n  stderr: {stderr}"));
    assert_eq!(envelope["kind"], "json_compact_unsupported");
    assert_eq!(
        envelope["exit_code"], 65,
        "envelope exit_code must agree with the process exit: {envelope}"
    );
}

#[test]
fn status_on_corrupted_state_is_data_err_with_recovery_path() {
    // HeddleCo/heddle#642: corrupted msgpack state must not dead-end on
    // raw decoder internals. `heddle status` is the natural recovery
    // probe, so it must name the condition, exit DataErr (65), and hand
    // back executable recovery commands in both output modes.
    let repo = init_repo();
    std::fs::write(repo.path().join("f.txt"), "base\n").expect("write f.txt");
    assert_exit(&["capture", "-m", "base"], repo.path(), 0);

    // Corrupt every stored state object: 0x90 is msgpack FixArray(0),
    // the exact marker from the persona report's dead-end.
    let states_dir = repo.path().join(".heddle/objects/states");
    let mut corrupted = 0usize;
    for entry in std::fs::read_dir(&states_dir).expect("read states dir") {
        let path = entry.expect("dir entry").path();
        std::fs::write(&path, [0x90u8]).expect("corrupt state file");
        corrupted += 1;
    }
    assert!(corrupted > 0, "fixture should have stored state objects");

    let json = heddle_output(&["--output", "json", "status"], Some(repo.path()))
        .expect("spawn status on corrupted repo");
    assert_eq!(
        json.status.code(),
        Some(65),
        "corrupted state should exit 65 (DataErr); stderr: {}",
        String::from_utf8_lossy(&json.stderr)
    );
    let stderr = String::from_utf8_lossy(&json.stderr);
    let envelope: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap_or_else(|err| {
        panic!("corrupted-state envelope not JSON: {err}\n  stderr: {stderr}")
    });
    assert_eq!(envelope["kind"], "state_corrupted");
    assert_eq!(envelope["exit_code"], 65);
    assert_eq!(
        envelope["error"], "Repository state is corrupted or unreadable",
        "user-facing error must name the condition, not echo decoder internals: {envelope}"
    );
    let recovery_commands = envelope["recovery_commands"]
        .as_array()
        .unwrap_or_else(|| panic!("recovery_commands should be an array: {envelope}"));
    assert!(
        !recovery_commands.is_empty(),
        "corrupted state must hand back recovery commands: {envelope}"
    );
    assert!(
        recovery_commands
            .iter()
            .any(|command| command.as_str().is_some_and(|c| c.contains("fsck"))),
        "recovery should point at the integrity tooling: {envelope}"
    );

    let text =
        heddle_output(&["status"], Some(repo.path())).expect("spawn text status on corrupted repo");
    assert_eq!(text.status.code(), Some(65));
    let text_stderr = String::from_utf8_lossy(&text.stderr);
    assert!(
        text_stderr.contains("Repository state is corrupted or unreadable")
            && text_stderr.contains("Next: heddle verify"),
        "text mode should name the condition and the recovery probe: {text_stderr}"
    );
}

#[test]
fn unconfigured_remote_uses_typed_remote_error_exit_code() {
    // Regression guard for the no-default-remote path (HeddleCo/heddle#252).
    // `resolve_remote` preserves `RemoteError::NotFound` in the chain, so the
    // exit code is typed instead of depending on the rendered message.
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
