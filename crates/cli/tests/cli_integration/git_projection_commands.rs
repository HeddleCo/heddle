// SPDX-License-Identifier: Apache-2.0
use objects::object::ThreadName;

use super::*;

/// Initialize a colocated (drop-in) Git repo on `main` with one
/// committed file, mirroring the bootstrap the overlay tests use.
fn init_colocated_git_repo(path: &std::path::Path) {
    assert!(
        Command::new("git")
            .arg("init")
            .current_dir(path)
            .status()
            .unwrap()
            .success()
    );
    for (k, v) in [
        ("user.name", "Heddle Test"),
        ("user.email", "heddle@example.com"),
        ("init.defaultBranch", "main"),
    ] {
        Command::new("git")
            .args(["config", k, v])
            .current_dir(path)
            .status()
            .unwrap();
    }
    Command::new("git")
        .args(["checkout", "-B", "main"])
        .current_dir(path)
        .status()
        .unwrap();
}

fn git_commit_all_in(path: &std::path::Path, message: &str) {
    assert!(
        Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(path)
            .status()
            .unwrap()
            .success()
    );
}

fn git_status_porcelain(path: &std::path::Path) -> String {
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "git status --porcelain must succeed");
    String::from_utf8(out.stdout).unwrap()
}

fn git_output_in(path: &std::path::Path, args: &[&str], stdin: Option<&[u8]>) -> String {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("GIT_AUTHOR_NAME", "Heddle Test")
        .env("GIT_AUTHOR_EMAIL", "heddle@example.com")
        .env("GIT_COMMITTER_NAME", "Heddle Test")
        .env("GIT_COMMITTER_EMAIL", "heddle@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    if stdin.is_some() {
        command.stdin(std::process::Stdio::piped());
    }
    let mut child = command.spawn().expect("git command should run");
    if let Some(stdin) = stdin {
        child
            .stdin
            .as_mut()
            .expect("stdin should be piped")
            .write_all(stdin)
            .expect("write git stdin");
    }
    let output = child.wait_with_output().expect("git command output");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn seed_unrepresentable_tree_name_repo(path: &std::path::Path) {
    git_output_in(path, &["init", "-q", "--initial-branch=main"], None);

    let blob = git_output_in(path, &["hash-object", "-w", "--stdin"], Some(b"hello\n"));
    let mut tree_input = Vec::new();
    write!(&mut tree_input, "100644 blob {blob}\t").expect("tree record");
    tree_input.extend_from_slice(b"bad\\\xffname\0");
    let tree = git_output_in(path, &["mktree", "-z"], Some(&tree_input));
    let commit = git_output_in(path, &["commit-tree", &tree, "-m", "invalid name"], None);
    git_output_in(path, &["update-ref", "refs/heads/main", &commit], None);
}

/// The empty-blob object id (SHA-1). An intent-to-add index entry points
/// at this rather than a real blob — that is what makes Git treat the
/// path as "added, content not yet staged".
const EMPTY_BLOB_OID: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";

#[test]
fn git_overlay_commit_recovers_checkpoint_intent_without_bridge_mirror() {
    use oplog::{OpLogBackend, OpRecord};

    for fault in [
        "git_checkpoint_after_publish_before_phase",
        "git_checkpoint_after_metadata_before_oplog",
        "git_checkpoint_after_oplog_before_finalize",
    ] {
        let repo = TempDir::new().unwrap();
        init_colocated_git_repo(repo.path());
        std::fs::write(repo.path().join("tracked.txt"), "base\n").unwrap();
        git_commit_all_in(repo.path(), "base");
        heddle(&["init"], Some(repo.path())).expect("initialize Git Overlay");

        std::fs::write(repo.path().join("tracked.txt"), format!("{fault}\n")).unwrap();
        heddle(&["capture", "-m", fault], Some(repo.path())).expect("capture state");
        let crashed = heddle_output_with_env(
            &["commit"],
            Some(repo.path()),
            &[("HEDDLE_FAULT_INJECT", fault)],
        )
        .expect("run faulted commit");
        assert!(
            !crashed.status.success(),
            "fault point {fault} must stop commit"
        );
        assert!(
            repo.path()
                .join(".heddle/state/git-checkpoint-intent.json")
                .is_file(),
            "fault point {fault} must leave a durable recovery intent"
        );

        heddle(&["commit"], Some(repo.path())).expect("retry checkpoint recovery");
        assert!(
            !repo
                .path()
                .join(".heddle/state/git-checkpoint-intent.json")
                .exists(),
            "retry after {fault} must finalize the intent"
        );
        assert!(
            !repo.path().join(".heddle/git").exists(),
            "commit must write through Sley to the checkout .git"
        );
        assert_eq!(git_status_porcelain(repo.path()), "");
        let reopened = Repository::open(repo.path()).expect("open recovered repository");
        assert_eq!(reopened.list_git_checkpoints().unwrap().len(), 1);
        let checkpoint_ops = reopened
            .oplog()
            .recent(100)
            .unwrap()
            .into_iter()
            .filter(|entry| matches!(entry.operation, OpRecord::GitCheckpoint { .. }))
            .count();
        assert_eq!(
            checkpoint_ops, 1,
            "recovery after {fault} must finalize the checkpoint oplog exactly once"
        );
    }
}

#[test]
fn import_git_refuses_unrepresentable_tree_name_by_default_and_lossy_summarizes() {
    let source = TempDir::new().unwrap();
    seed_unrepresentable_tree_name_repo(source.path());

    let default_target = TempDir::new().unwrap();
    heddle(&["init"], Some(default_target.path())).expect("init default target");
    let default_output = heddle_output(
        &["import", "git", "--path", source.path().to_str().unwrap()],
        Some(default_target.path()),
    )
    .expect("run default import");
    assert!(
        !default_output.status.success(),
        "default git import must fail on unrepresentable tree name"
    );
    let default_stderr = String::from_utf8_lossy(&default_output.stderr);
    assert!(
        default_stderr.contains("bad") && default_stderr.contains("name"),
        "error should name the offending entry: {default_stderr}"
    );
    assert!(
        default_stderr.contains("--lossy"),
        "error should name the opt-in flag: {default_stderr}"
    );

    let lossy_target = TempDir::new().unwrap();
    heddle(&["init"], Some(lossy_target.path())).expect("init lossy target");
    let lossy = heddle(
        &[
            "import",
            "git",
            "--lossy",
            "--path",
            source.path().to_str().unwrap(),
        ],
        Some(lossy_target.path()),
    )
    .expect("lossy import should succeed");

    assert!(
        lossy.contains("lossy import accepted"),
        "lossy import should emit an end-of-run summary: {lossy}"
    );
    assert!(
        lossy.contains("bad") && lossy.contains("name"),
        "summary names entry: {lossy}"
    );
    assert!(lossy.contains("dropped"), "summary names action: {lossy}");
}

#[test]
fn import_git_help_documents_lossy_flag() {
    let output = heddle_help(&["import", "git", "--help"]);

    assert!(output.contains("--lossy"), "help should document --lossy");
}

/// After `heddle capture` records a NEW file in a colocated checkout,
/// `git status` should report it as intent-to-add ("Heddle knows about
/// it; no Git blob committed yet") rather than `??` (untracked, "Git
/// knows nothing"). This is byte-for-byte the state `git add -N`
/// produces — Git 2.43 renders the porcelain code as ` A` (older docs,
/// and the issue, call it `AM`); the version-stable invariant is the
/// empty-blob index entry, which we assert directly. Already-tracked,
/// unchanged files must NOT be touched.
#[test]
fn capture_marks_new_file_intent_to_add_in_colocated_index() {
    let source = TempDir::new().unwrap();
    init_colocated_git_repo(source.path());

    std::fs::write(source.path().join("tracked.txt"), "already tracked\n").unwrap();
    git_commit_all_in(source.path(), "initial");

    heddle(&["adopt", "--ref", "main"], Some(source.path()))
        .expect("adopt should import Git history into Heddle");

    std::fs::write(source.path().join("new_file.txt"), "brand new content\n").unwrap();
    heddle(&["capture", "-m", "add new file"], Some(source.path()))
        .expect("capture should record the new file");

    let status = git_status_porcelain(source.path());

    let new_file_line = status
        .lines()
        .find(|line| line.ends_with("new_file.txt"))
        .unwrap_or_else(|| panic!("new_file.txt must appear in git status. Status was:\n{status}"));
    // Intent-to-add shows an `A` in the status code (` A`, `AM`, or `A `
    // across Git versions) — never `??`.
    assert!(
        new_file_line[..2].contains('A'),
        "new file should be intent-to-add (contains `A`), got {new_file_line:?}. Status was:\n{status}"
    );
    assert!(
        !status.contains("?? new_file.txt"),
        "new file must no longer show as untracked (??). Status was:\n{status}"
    );

    // The version-stable proof of intent-to-add: the staged entry points
    // at the empty blob, not a real object copied into `.git/objects`.
    let staged = Command::new("git")
        .args(["ls-files", "--stage", "new_file.txt"])
        .current_dir(source.path())
        .output()
        .unwrap();
    let staged = String::from_utf8(staged.stdout).unwrap();
    assert!(
        staged.contains(EMPTY_BLOB_OID),
        "new file's index entry must be intent-to-add (empty-blob oid), got: {staged:?}"
    );

    assert!(
        !status.lines().any(|line| line.ends_with("tracked.txt")),
        "an already-tracked, unchanged file must not be marked. Status was:\n{status}"
    );
}

/// `update_intent_to_add` must RECONCILE, not just append: when a file
/// that was previously marked intent-to-add is no longer in the captured
/// state (e.g. it was created, captured, then deleted before checkpoint),
/// its stale index entry must be pruned. A surviving intent-to-add entry
/// whose worktree file is gone makes `git status` report a phantom ` D`
/// deletion that Heddle never intended.
#[test]
fn recapture_prunes_stale_intent_to_add_for_removed_file() {
    let source = TempDir::new().unwrap();
    init_colocated_git_repo(source.path());

    std::fs::write(source.path().join("tracked.txt"), "already tracked\n").unwrap();
    git_commit_all_in(source.path(), "initial");

    heddle(&["adopt", "--ref", "main"], Some(source.path()))
        .expect("adopt should import Git history into Heddle");

    // Capture a new file: it becomes intent-to-add in the colocated index.
    std::fs::write(source.path().join("new_file.txt"), "brand new content\n").unwrap();
    heddle(&["capture", "-m", "add new file"], Some(source.path()))
        .expect("capture should record the new file");

    let status = git_status_porcelain(source.path());
    assert!(
        status.lines().any(|line| line.ends_with("new_file.txt")),
        "precondition: new_file.txt must be intent-to-add after first capture. Status was:\n{status}"
    );

    // Delete the file before any checkpoint commits it, then recapture.
    // The captured state no longer contains new_file.txt, so its
    // intent-to-add index entry is now stale and must be pruned.
    std::fs::remove_file(source.path().join("new_file.txt")).unwrap();
    heddle(&["capture", "-m", "remove new file"], Some(source.path()))
        .expect("recapture should record the deletion");

    let status = git_status_porcelain(source.path());
    assert!(
        !status.lines().any(|line| line.ends_with("new_file.txt")),
        "stale intent-to-add for a deleted file must be pruned — no phantom ` D` entry. Status was:\n{status}"
    );

    // The index entry must be gone entirely, not merely re-pointed.
    let staged = Command::new("git")
        .args(["ls-files", "--stage", "new_file.txt"])
        .current_dir(source.path())
        .output()
        .unwrap();
    let staged = String::from_utf8(staged.stdout).unwrap();
    assert!(
        staged.trim().is_empty(),
        "stale intent-to-add index entry must be removed, got: {staged:?}"
    );
}

/// The prune must run on EVERY recapture path, including the one where
/// the recaptured state is EMPTY (no files at all). An early `captured
/// .is_empty()` fast path that returns before the reconcile lets stale
/// intent-to-add entries survive: capture a new file (it becomes
/// intent-to-add), then delete every file so the next capture yields an
/// empty tree — the old intent-to-add entry must still be pruned, not
/// left behind as a phantom.
#[test]
fn recapture_to_empty_tree_prunes_stale_intent_to_add() {
    let source = TempDir::new().unwrap();
    init_colocated_git_repo(source.path());

    std::fs::write(source.path().join("tracked.txt"), "already tracked\n").unwrap();
    git_commit_all_in(source.path(), "initial");

    heddle(&["adopt", "--ref", "main"], Some(source.path()))
        .expect("adopt should import Git history into Heddle");

    // Capture a new file: it becomes intent-to-add in the colocated index.
    std::fs::write(source.path().join("new_file.txt"), "brand new content\n").unwrap();
    heddle(&["capture", "-m", "add new file"], Some(source.path()))
        .expect("capture should record the new file");

    let staged = Command::new("git")
        .args(["ls-files", "--stage", "new_file.txt"])
        .current_dir(source.path())
        .output()
        .unwrap();
    assert!(
        String::from_utf8(staged.stdout)
            .unwrap()
            .contains(EMPTY_BLOB_OID),
        "precondition: new_file.txt must be intent-to-add after first capture"
    );

    // Delete EVERY file so the recaptured state is an empty tree, hitting
    // the `captured.is_empty()` fast path. The intent-to-add entry for
    // new_file.txt is now stale and must still be pruned.
    std::fs::remove_file(source.path().join("new_file.txt")).unwrap();
    std::fs::remove_file(source.path().join("tracked.txt")).unwrap();
    heddle(&["capture", "-m", "remove everything"], Some(source.path()))
        .expect("recapture should record the empty tree");

    let status = git_status_porcelain(source.path());
    assert!(
        !status.lines().any(|line| line.ends_with("new_file.txt")),
        "stale intent-to-add must be pruned even when the recapture yields an empty tree. Status was:\n{status}"
    );

    // The intent-to-add index entry must be gone entirely.
    let staged = Command::new("git")
        .args(["ls-files", "--stage", "new_file.txt"])
        .current_dir(source.path())
        .output()
        .unwrap();
    let staged = String::from_utf8(staged.stdout).unwrap();
    assert!(
        staged.trim().is_empty(),
        "stale intent-to-add index entry must be removed on the empty-tree path, got: {staged:?}"
    );
}

/// Git's index cannot hold both `foo` (a blob) and `foo/bar` (a blob
/// under a directory) — they are mutually exclusive (a path is either a
/// file or a directory). When a recapture introduces a path that
/// file/dir-PREFIX-conflicts with a still-tracked real entry, the ADD
/// pass must NOT write an intent-to-add entry for it: the real entry
/// wins, and a conflicting placeholder would corrupt the index into a
/// file/dir conflict.
///
/// Direction A: Git tracks `foo` (a file); the worktree replaces it with
/// a directory `foo/` holding `foo/bar`. Recapture must not add an
/// intent-to-add entry for `foo/bar` alongside the tracked `foo`.
#[test]
fn recapture_skips_intent_to_add_that_conflicts_with_tracked_file() {
    let source = TempDir::new().unwrap();
    init_colocated_git_repo(source.path());

    std::fs::write(source.path().join("foo"), "i am a file\n").unwrap();
    git_commit_all_in(source.path(), "initial");

    heddle(&["adopt", "--ref", "main"], Some(source.path()))
        .expect("adopt should import Git history into Heddle");

    // Replace the tracked file `foo` with a directory `foo/` containing
    // `foo/bar`. The captured state now has `foo/bar`, but the real
    // index entry `foo` is still present (no checkpoint has committed
    // the change). Adding intent-to-add for `foo/bar` would conflict.
    std::fs::remove_file(source.path().join("foo")).unwrap();
    std::fs::create_dir(source.path().join("foo")).unwrap();
    std::fs::write(source.path().join("foo").join("bar"), "now a dir\n").unwrap();
    heddle(
        &["capture", "-m", "file becomes directory"],
        Some(source.path()),
    )
    .expect("recapture should record the file→dir change");

    // The index must stay valid: never both `foo` and `foo/bar`.
    let staged = Command::new("git")
        .args(["ls-files", "--stage"])
        .current_dir(source.path())
        .output()
        .unwrap();
    let staged = String::from_utf8(staged.stdout).unwrap();
    let has_foo = staged.lines().any(|l| l.ends_with("\tfoo"));
    let has_foo_bar = staged.lines().any(|l| l.ends_with("\tfoo/bar"));
    assert!(
        !(has_foo && has_foo_bar),
        "index must not hold both `foo` and `foo/bar` (file/dir conflict). ls-files:\n{staged}"
    );
    // git status must still run cleanly over the index.
    let _ = git_status_porcelain(source.path());
}

/// Direction B (reverse): Git tracks `foo/bar` (a blob under a dir); the
/// worktree replaces the directory with a file `foo`. The captured state
/// has `foo`, but the real index entry `foo/bar` is still present.
/// Adding intent-to-add for `foo` would conflict with the tracked
/// `foo/bar`, so it must be skipped.
#[test]
fn recapture_skips_intent_to_add_that_conflicts_with_tracked_dir() {
    let source = TempDir::new().unwrap();
    init_colocated_git_repo(source.path());

    std::fs::create_dir(source.path().join("foo")).unwrap();
    std::fs::write(source.path().join("foo").join("bar"), "i am under a dir\n").unwrap();
    git_commit_all_in(source.path(), "initial");

    heddle(&["adopt", "--ref", "main"], Some(source.path()))
        .expect("adopt should import Git history into Heddle");

    // Replace the directory `foo/` with a file `foo`. The captured state
    // now has `foo`, but the real index entry `foo/bar` is still present.
    std::fs::remove_dir_all(source.path().join("foo")).unwrap();
    std::fs::write(source.path().join("foo"), "now a file\n").unwrap();
    heddle(&["capture", "-m", "dir becomes file"], Some(source.path()))
        .expect("recapture should record the dir→file change");

    let staged = Command::new("git")
        .args(["ls-files", "--stage"])
        .current_dir(source.path())
        .output()
        .unwrap();
    let staged = String::from_utf8(staged.stdout).unwrap();
    let has_foo = staged.lines().any(|l| l.ends_with("\tfoo"));
    let has_foo_bar = staged.lines().any(|l| l.ends_with("\tfoo/bar"));
    assert!(
        !(has_foo && has_foo_bar),
        "index must not hold both `foo` and `foo/bar` (file/dir conflict). ls-files:\n{staged}"
    );
    let _ = git_status_porcelain(source.path());
}

#[test]
fn test_cli_bridge_git_init_leaf_removed() {
    let temp = TempDir::new().unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();

    assert!(
        heddle(&["bridge", "git", "init"], Some(temp.path())).is_err(),
        "public bridge git init leaf should be removed"
    );
    assert!(
        !temp.path().join(".heddle/git").exists(),
        "removed public command must not initialize the legacy Bridge Mirror"
    );
}

#[test]
fn test_cli_export_git_and_clone_roundtrip() {
    let source = TempDir::new().unwrap();
    let target_holder = TempDir::new().unwrap();
    let target = target_holder.path().join("clone");
    let dest_holder = TempDir::new().unwrap();
    let dest = dest_holder.path().join("export");

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "git projection export").unwrap();
    heddle(
        &["capture", "-m", "Git projection source"],
        Some(source.path()),
    )
    .unwrap();

    // Phase A: `export git` requires `--destination`. Pre-Phase-A
    // it silently no-op'd if no flag was given (writing only the sidecar
    // mapping, not actually exporting any git objects). Now it errors.
    let export = heddle(
        &["export", "git", "--destination", dest.to_str().unwrap()],
        Some(source.path()),
    );
    assert!(export.is_ok(), "export git failed: {:?}", export.err());

    let dest_repo = open_git(&dest).unwrap();
    assert!(find_reference(&dest_repo, "refs/heads/main").is_ok());

    let clone = heddle(
        &["clone", dest.to_str().unwrap(), target.to_str().unwrap()],
        Some(dest_holder.path()),
    );
    assert!(clone.is_ok(), "clone failed: {:?}", clone.err());

    let target_repo = Repository::open(&target).unwrap();
    assert!(
        target_repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn test_cli_export_git_writes_bare_repo() {
    let source = TempDir::new().unwrap();
    let dest_holder = TempDir::new().unwrap();
    let dest = dest_holder.path().join("export");

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "export git").unwrap();
    heddle(&["capture", "-m", "Export git source"], Some(source.path())).unwrap();

    let export = heddle(
        &["export", "git", "--destination", dest.to_str().unwrap()],
        Some(source.path()),
    );
    assert!(export.is_ok(), "export git failed: {:?}", export.err());

    let dest_repo = open_git(&dest).unwrap();
    assert!(find_reference(&dest_repo, "refs/heads/main").is_ok());
}

#[test]
fn test_cli_import_git_from_external_repo() {
    let heddle_repo_dir = TempDir::new().unwrap();
    let git_repo_dir = TempDir::new().unwrap();
    let git_repo = SleyRepository::init(git_repo_dir.path()).unwrap();
    let tree_oid = git_empty_tree_oid(&git_repo);
    git_commit_with_tree(
        &git_repo,
        Some("refs/heads/main"),
        tree_oid,
        "Imported commit",
        &[],
    );

    heddle(&["init"], Some(heddle_repo_dir.path())).unwrap();
    let result = heddle(
        &[
            "import",
            "git",
            "--path",
            git_repo_dir.path().to_str().unwrap(),
        ],
        Some(heddle_repo_dir.path()),
    );
    assert!(result.is_ok(), "import git failed: {:?}", result.err());

    let repo = Repository::open(heddle_repo_dir.path()).unwrap();
    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn test_cli_bridge_git_push_pull_leaves_removed() {
    let source = TempDir::new().unwrap();
    heddle(&["init"], Some(source.path())).unwrap();

    for leaf in ["push", "pull"] {
        let result = heddle(&["bridge", "git", leaf], Some(source.path()));
        assert!(
            result.is_err(),
            "bridge git {leaf} should be removed in favor of top-level {leaf}"
        );
    }
}
