// SPDX-License-Identifier: Apache-2.0
//! heddle#1145 — the annotations/discussions boundary must be honest.
//!
//! Context annotations and discussions are native-only: they live in
//! `.heddle`, travel over `heddle push`/`heddle pull`, and are never projected
//! into Git. A `git clone` therefore carries none of them. These tests pin the
//! four things the CLI must say about that, and the one thing it must not do.

use std::{path::Path, str};

use serde_json::Value;
use tempfile::TempDir;

use super::{git_hermetic, heddle, heddle_help, heddle_output};

fn git(args: &[&str], dir: &Path) {
    git_hermetic(args, dir);
}

/// A Git Overlay repository holding one annotation and one discussion.
fn overlay_with_records() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let dir = temp.path();
    git(&["init", "-q", "-b", "main", "."], dir);
    std::fs::write(dir.join("f.txt"), "hello\n").expect("write f.txt");
    git(&["add", "f.txt"], dir);
    git(&["commit", "-qm", "init"], dir);
    heddle(&["init"], Some(dir)).expect("heddle init");
    heddle(
        &[
            "context",
            "set",
            "--path",
            "f.txt",
            "--kind",
            "invariant",
            "-m",
            "stays lowercase",
        ],
        Some(dir),
    )
    .expect("context set");
    heddle(
        &["discuss", "open", "f.txt", "greeting", "why lowercase?"],
        Some(dir),
    )
    .expect("discuss open");
    temp
}

/// `git clone` of that repository: source history arrives, `.heddle` does not.
fn clone_of(source: &Path) -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let dir = temp.path();
    git(
        &[
            "clone",
            "-q",
            source.to_str().expect("utf8 source path"),
            ".",
        ],
        dir,
    );
    assert!(
        !dir.join(".heddle").exists(),
        "a git clone must not carry `.heddle`"
    );
    temp
}

fn native_repo() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    heddle(&["init"], Some(temp.path())).expect("heddle init");
    temp
}

fn json(args: &[&str], dir: &Path) -> Value {
    let stdout = heddle(args, Some(dir)).unwrap_or_else(|e| panic!("`{args:?}` failed: {e}"));
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("`{args:?}` should emit JSON, got {stdout:?}: {e}"))
}

/// Criterion 1. A clone with no `.heddle` must not be reported as a Heddle
/// repository — and reading annotations must not quietly make it into one.
///
/// The lie was a side effect: read-only `context`/`discuss` commands went
/// through `Repository::open`, which bootstraps a Git-overlay sidecar on a
/// plain Git tree. `heddle context list` created the store, and the next
/// `heddle status` truthfully reported the store it had just created.
#[test]
fn reads_in_a_clone_neither_claim_nor_create_a_heddle_store() {
    let source = overlay_with_records();
    let clone = clone_of(source.path());
    let dir = clone.path();

    let before = json(&["--output", "json", "status"], dir);
    assert_eq!(before["repository_capability"], "plain-git", "{before}");
    assert_eq!(before["heddle_initialized"], Value::Bool(false), "{before}");

    for args in [
        vec!["context", "list"],
        vec!["context", "get", "--path", "f.txt"],
        vec!["context", "check"],
        vec!["context", "audit"],
        vec!["discuss", "list"],
    ] {
        heddle(&args, Some(dir)).unwrap_or_else(|e| panic!("`{args:?}` should succeed: {e}"));
        assert!(
            !dir.join(".heddle").exists(),
            "`{args:?}` bootstrapped a Heddle store in a plain Git clone"
        );
    }

    let after = json(&["--output", "json", "status"], dir);
    assert_eq!(after["repository_capability"], "plain-git", "{after}");
    assert_eq!(after["heddle_initialized"], Value::Bool(false), "{after}");
    let text = heddle(&["status"], Some(dir)).expect("status");
    assert!(
        !text.contains("Git + Heddle"),
        "status must not claim `Git + Heddle` for a clone with no store: {text}"
    );
}

/// Criterion 2, and the load-bearing half of it: "no store here" and "no
/// annotations exist" are different answers and must read differently.
/// Collapsing them back into one message fails this test from both sides.
#[test]
fn absent_store_and_empty_store_report_differently() {
    let source = overlay_with_records();
    let clone = clone_of(source.path());
    let native = native_repo();

    let absent = heddle(&["context", "list"], Some(clone.path())).expect("context list in clone");
    let empty = heddle(&["context", "list"], Some(native.path())).expect("context list in native");

    assert!(
        absent.contains("No Heddle store here"),
        "clone must report the missing store: {absent}"
    );
    assert!(
        absent.contains("git clone"),
        "clone must explain why the annotations are not here: {absent}"
    );
    assert!(
        !absent.contains("No context annotations."),
        "clone must not answer as if the store existed and was empty: {absent}"
    );
    assert!(
        empty.contains("No context annotations."),
        "an empty native store still reports zero annotations: {empty}"
    );
    assert!(
        !empty.contains("No Heddle store here"),
        "a native store is present; do not claim otherwise: {empty}"
    );
    assert_ne!(absent, empty, "the two branches must not collapse");

    let absent_discuss =
        heddle(&["discuss", "list"], Some(clone.path())).expect("discuss list in clone");
    let empty_discuss =
        heddle(&["discuss", "list"], Some(native.path())).expect("discuss list in native");
    assert!(
        absent_discuss.contains("No Heddle store here"),
        "{absent_discuss}"
    );
    assert!(
        empty_discuss.contains("(no discussions)"),
        "{empty_discuss}"
    );
    assert_ne!(absent_discuss, empty_discuss);
}

/// The machine contract carries the same distinction, so an agent parsing
/// JSON cannot read an absent store as an empty one.
#[test]
fn absent_store_is_distinguishable_in_json() {
    let source = overlay_with_records();
    let clone = clone_of(source.path());
    let native = native_repo();

    for (surface, items_key) in [("context", "items"), ("discuss", "discussions")] {
        let absent = json(&["--output", "json", surface, "list"], clone.path());
        assert_eq!(absent["store_present"], Value::Bool(false), "{absent}");
        assert_eq!(absent["store_scope"], "native-heddle-only", "{absent}");
        assert_eq!(absent["git_checkout"], Value::Bool(true), "{absent}");
        assert_eq!(
            absent["output_kind"],
            format!("{surface}_list"),
            "the absent-store envelope keeps the command's discriminator: {absent}"
        );
        assert_eq!(
            absent[items_key],
            Value::Array(vec![]),
            "shape stays compatible with a populated list: {absent}"
        );

        let empty = json(&["--output", "json", surface, "list"], native.path());
        assert!(
            empty.get("store_present").is_none(),
            "a present store does not carry the absent-store marker: {empty}"
        );
        assert_eq!(empty[items_key], Value::Array(vec![]), "{empty}");
    }
}

/// Criterion 3. One line at creation, in Git Overlay mode only, exactly once
/// per working copy — a boundary statement, not a nag.
#[test]
fn overlay_creation_warns_once_per_working_copy() {
    let temp = TempDir::new().expect("tempdir");
    let dir = temp.path();
    git(&["init", "-q", "-b", "main", "."], dir);
    std::fs::write(dir.join("f.txt"), "hello\n").expect("write f.txt");
    git(&["add", "f.txt"], dir);
    git(&["commit", "-qm", "init"], dir);
    heddle(&["init"], Some(dir)).expect("heddle init");

    let notice = "local to this working copy";

    let first = heddle_output(
        &[
            "context",
            "set",
            "--path",
            "f.txt",
            "--kind",
            "invariant",
            "-m",
            "one",
        ],
        Some(dir),
    )
    .expect("context set");
    let first_stderr = String::from_utf8_lossy(&first.stderr).into_owned();
    assert!(
        first_stderr.contains(notice) && first_stderr.contains("do not travel"),
        "first overlay annotation must state the boundary: {first_stderr}"
    );

    for message in ["two", "three"] {
        let repeat = heddle_output(
            &[
                "context",
                "set",
                "--path",
                "f.txt",
                "--scope",
                "file",
                "--kind",
                "invariant",
                "-m",
                message,
            ],
            Some(dir),
        )
        .expect("context set");
        let stderr = String::from_utf8_lossy(&repeat.stderr).into_owned();
        assert!(
            !stderr.contains(notice),
            "the notice must not repeat on every invocation: {stderr}"
        );
    }

    // `discuss` states the same boundary for its own records, also once.
    let first_discuss = heddle_output(&["discuss", "open", "f.txt", "greeting", "why?"], Some(dir))
        .expect("discuss open");
    let discuss_stderr = String::from_utf8_lossy(&first_discuss.stderr).into_owned();
    assert!(
        discuss_stderr.contains(notice),
        "first overlay discussion must state the boundary: {discuss_stderr}"
    );
    let second_discuss = heddle_output(
        &["discuss", "open", "f.txt", "greeting", "still why?"],
        Some(dir),
    )
    .expect("discuss open");
    assert!(
        !String::from_utf8_lossy(&second_discuss.stderr).contains(notice),
        "the discuss notice must not repeat"
    );
}

/// The notice is false in a native repository — there the records *do* travel,
/// over `heddle push`/`heddle pull`. It must stay silent.
#[test]
fn native_creation_does_not_claim_records_are_local() {
    let native = native_repo();
    let output = heddle_output(
        &[
            "context",
            "set",
            "--path",
            "g.txt",
            "--kind",
            "invariant",
            "-m",
            "native",
        ],
        Some(native.path()),
    )
    .expect("context set");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !stderr.contains("local to this working copy"),
        "native annotations travel with the repository; do not claim otherwise: {stderr}"
    );
}

/// Criterion 4. The help for both surfaces states the native-only scope, and
/// the Git Overlay topic qualifies the pitch for the mode where it is false.
#[test]
fn help_states_the_native_only_scope() {
    let context_help = heddle_help(&["help", "context"]);
    assert!(
        context_help.contains("Native Heddle only"),
        "`heddle help context` must state the scope: {context_help}"
    );
    assert!(
        context_help.contains("git clone"),
        "`heddle help context` must say Git does not carry annotations: {context_help}"
    );

    let discuss_help = heddle_help(&["help", "discuss"]);
    assert!(
        discuss_help.contains("native Heddle only"),
        "`heddle help discuss` must state the scope: {discuss_help}"
    );
    assert!(
        discuss_help.contains("not into `refs/notes/*`"),
        "`heddle help discuss` must name the storage design that was rejected: {discuss_help}"
    );

    let overlay_topic = heddle_help(&["help", "git-overlay"]);
    assert!(
        overlay_topic.contains("local to this working copy"),
        "the Git Overlay pitch must be qualified: {overlay_topic}"
    );
}

/// #1144's fix, re-verified here because this issue's storage decision depends
/// on it: creating annotations in a Git Overlay repository writes nothing to
/// Git — no worktree change, no ref, no dangling object.
#[test]
fn overlay_records_leave_no_git_residue() {
    let temp = overlay_with_records();
    let dir = temp.path();

    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .output()
        .expect("git status");
    assert!(
        str::from_utf8(&status.stdout)
            .expect("utf8")
            .trim()
            .is_empty(),
        "annotations must not touch the Git worktree: {}",
        String::from_utf8_lossy(&status.stdout)
    );

    let refs = std::process::Command::new("git")
        .args(["for-each-ref", "--format=%(refname)"])
        .current_dir(dir)
        .output()
        .expect("git for-each-ref");
    let refs = String::from_utf8_lossy(&refs.stdout);
    assert!(
        !refs.contains("refs/notes"),
        "context is native-only and must not be projected into notes: {refs}"
    );

    let fsck = std::process::Command::new("git")
        .args(["fsck", "--no-progress"])
        .current_dir(dir)
        .output()
        .expect("git fsck");
    let fsck_out = format!(
        "{}{}",
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr)
    );
    assert!(
        !fsck_out.contains("dangling"),
        "annotations must not leave orphan Git objects: {fsck_out}"
    );
}
