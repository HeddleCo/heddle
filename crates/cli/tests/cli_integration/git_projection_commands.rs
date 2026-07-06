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
    heddle(&["capture", "-m", "Git projection source"], Some(source.path())).unwrap();

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

/// `heddle push --mirror=<git-remote>` performs the primary push to the
/// heddle remote AND a Git projection push to the configured mirror, in one
/// invocation.
#[test]
fn test_cli_push_mirror_dual_push_to_weft_and_git_remote() {
    let source = TempDir::new().unwrap();
    let weft_target = TempDir::new().unwrap();
    let git_remote = TempDir::new().unwrap();
    let mirror_repo = SleyRepository::init_bare(git_remote.path()).unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(weft_target.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "dual push").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(source.path())).unwrap();

    let weft_path = weft_target.path().to_string_lossy().to_string();
    let git_path = git_remote.path().to_string_lossy().to_string();
    let mirror_arg = format!("--mirror={}", git_path);
    // `--output text` forces the text branch of `render_mirror_outcome`.
    // Default `auto` resolves to JSON when stdout is piped (which it is
    // under `cargo test`), so without this flag the text-success path
    // would never execute and codecov/patch would miss it.
    let stdout = heddle(
        &[
            "--output",
            "text",
            "push",
            &weft_path,
            "--thread",
            "main",
            &mirror_arg,
        ],
        Some(source.path()),
    )
    .expect("dual push (--mirror=<remote>) should succeed");

    // Text branch on success emits a "mirrored to <remote>" line.
    assert!(
        stdout.contains("mirrored to") && stdout.contains(&git_path),
        "text-mode success line missing: {}",
        stdout
    );

    // Primary push landed at the heddle target.
    let threads = heddle(&["thread", "list"], Some(weft_target.path())).unwrap();
    assert!(
        threads.contains("main"),
        "weft target should have main thread after primary push: {}",
        threads
    );

    // Mirror push landed at the bare git remote.
    assert!(
        find_reference(&mirror_repo, "refs/heads/main").is_ok(),
        "git mirror remote should have refs/heads/main after mirror push"
    );
}

/// Mirror push failure is reported as a warning but does NOT cause the
/// primary push to fail. The user still sees the primary push succeed.
#[test]
fn test_cli_push_mirror_failure_does_not_abort_primary_push() {
    let source = TempDir::new().unwrap();
    let weft_target = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(weft_target.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "warn on mirror fail").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(source.path())).unwrap();

    let weft_path = weft_target.path().to_string_lossy().to_string();
    // Pointing the mirror at a nonexistent path is a failure.
    let bogus_mirror = source
        .path()
        .join("does-not-exist-mirror")
        .to_string_lossy()
        .to_string();
    let mirror_arg = format!("--mirror={}", bogus_mirror);
    // `--output text` forces the text branch of `render_mirror_outcome`
    // on the failure path. The warning lands on stderr, so this test
    // proves the primary push still succeeds and leaves separate
    // stderr-checking to the JSON variant (which captures the structured
    // failure on stdout).
    let result = heddle(
        &[
            "--output",
            "text",
            "push",
            &weft_path,
            "--thread",
            "main",
            &mirror_arg,
        ],
        Some(source.path()),
    );
    assert!(
        result.is_ok(),
        "primary push must still succeed even when mirror push fails: {:?}",
        result.err()
    );

    // Primary push still landed.
    let threads = heddle(&["thread", "list"], Some(weft_target.path())).unwrap();
    assert!(
        threads.contains("main"),
        "primary push should land even if mirror push fails: {}",
        threads
    );
}

/// `--mirror` MUST require `=` to take an explicit value. Without
/// `require_equals = true`, clap would consume the next token (the
/// positional primary remote) as the mirror value, silently pushing
/// the primary to the configured default and the mirror to the
/// intended primary target.
///
/// Pins the behavior: `heddle push --mirror <PRIMARY>` parses
/// `<PRIMARY>` as the positional remote, and `--mirror` takes its
/// `default_missing_value` ("origin"). Since no `origin` git remote
/// is configured here, the mirror push warns but does not abort.
#[test]
fn test_cli_push_mirror_requires_equals_does_not_swallow_positional() {
    let source = TempDir::new().unwrap();
    let weft_target = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(weft_target.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "require equals").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(source.path())).unwrap();

    let weft_path = weft_target.path().to_string_lossy().to_string();
    // Space form with the positional remote immediately after
    // `--mirror`. Without `require_equals=true`, clap consumes
    // `weft_path` as the mirror's value, leaving the primary remote
    // unspecified — silently inverting the user's intent. With
    // `require_equals=true`, `--mirror` takes its
    // `default_missing_value` and `weft_path` parses as the
    // positional primary remote.
    let result = heddle(
        &["push", "--mirror", &weft_path, "--thread", "main"],
        Some(source.path()),
    );
    assert!(
        result.is_ok(),
        "push must succeed; primary should land at <PRIMARY> and mirror default (origin) is best-effort: {:?}",
        result.err()
    );

    // Primary push landed at the heddle target — proving the
    // positional was NOT swallowed by --mirror.
    let threads = heddle(&["thread", "list"], Some(weft_target.path())).unwrap();
    assert!(
        threads.contains("main"),
        "primary push should land at the positional remote, not be swallowed by --mirror: {}",
        threads
    );
}

/// `--mirror=<name>` parses the explicit value and `--mirror` alone
/// takes the `default_missing_value`. Pins both forms in one test
/// so the parse table is asserted from end to end.
#[test]
fn test_cli_push_mirror_explicit_equals_form_parses_value() {
    let source = TempDir::new().unwrap();
    let weft_target = TempDir::new().unwrap();
    let git_remote = TempDir::new().unwrap();
    let mirror_repo = SleyRepository::init_bare(git_remote.path()).unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(weft_target.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "explicit eq").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(source.path())).unwrap();

    let weft_path = weft_target.path().to_string_lossy().to_string();
    let git_path = git_remote.path().to_string_lossy().to_string();
    let mirror_arg = format!("--mirror={}", git_path);
    // Flag-before-positional ordering — the form Codex's finding said
    // the original parse table mishandled.
    let result = heddle(
        &["push", &mirror_arg, "--thread", "main", &weft_path],
        Some(source.path()),
    );
    assert!(
        result.is_ok(),
        "--mirror=<remote> followed by positional must parse cleanly: {:?}",
        result.err()
    );

    let threads = heddle(&["thread", "list"], Some(weft_target.path())).unwrap();
    assert!(threads.contains("main"));
    assert!(
        find_reference(&mirror_repo, "refs/heads/main").is_ok(),
        "mirror push should land at the explicit <git_path>"
    );
}

/// JSON output path on mirror success: covers the `mirrored:true`
/// branch of `render_mirror_outcome`.
#[test]
fn test_cli_push_mirror_json_success_emits_mirrored_true() {
    let source = TempDir::new().unwrap();
    let weft_target = TempDir::new().unwrap();
    let git_remote = TempDir::new().unwrap();
    SleyRepository::init_bare(git_remote.path()).unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(weft_target.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "json ok").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(source.path())).unwrap();

    let weft_path = weft_target.path().to_string_lossy().to_string();
    let git_path = git_remote.path().to_string_lossy().to_string();
    let mirror_arg = format!("--mirror={}", git_path);
    let output = heddle_output(
        &[
            "--output",
            "json",
            "push",
            &weft_path,
            "--thread",
            "main",
            &mirror_arg,
        ],
        Some(source.path()),
    )
    .expect("push --output json --mirror=<remote> must invoke");
    let stderr = std::str::from_utf8(&output.stderr).unwrap_or("");
    assert!(
        output.status.success(),
        "primary push must succeed: stderr={stderr}"
    );

    // Mirror diagnostics land on stderr to keep `heddle push --output json`
    // a single JSON object on stdout (PR #251 contract).
    assert!(
        stderr.contains("\"mirrored\":true"),
        "JSON mirror success line missing on stderr: {stderr}"
    );
    assert!(
        stderr.contains(&git_path),
        "stderr should echo the mirror remote: {stderr}"
    );
}

/// `heddle push --mirror=<git-remote>` in a Git-overlay (non-hosted)
/// repo must push to BOTH the primary and the mirror. The cmd_push
/// early-return for the `GitOverlay && !hosted_enabled` branch
/// previously skipped the mirror block entirely, silently ignoring
/// `--mirror` for the overlay drop-in case.
#[test]
fn test_cli_push_mirror_in_git_overlay_pushes_to_both_remotes() {
    let source = TempDir::new().unwrap();
    let primary_remote = TempDir::new().unwrap();
    let mirror_remote = TempDir::new().unwrap();
    let primary_repo = SleyRepository::init_bare(primary_remote.path()).unwrap();
    let mirror_repo = SleyRepository::init_bare(mirror_remote.path()).unwrap();

    // Plain `git init` → RepositoryCapability::GitOverlay,
    // hosted_enabled() == false. This is the drop-in case the
    // early-return in cmd_push handles.
    assert!(
        Command::new("git")
            .arg("init")
            .current_dir(source.path())
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
            .current_dir(source.path())
            .status()
            .unwrap();
    }
    Command::new("git")
        .args(["checkout", "-B", "main"])
        .current_dir(source.path())
        .status()
        .unwrap();
    std::fs::write(source.path().join("file.txt"), "overlay dual push").unwrap();
    Command::new("git")
        .args(["add", "."])
        .current_dir(source.path())
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(source.path())
        .status()
        .unwrap();

    // Bootstrap the heddle overlay sidecar so Git projection has content
    // to push. Without an imported state, `push` silently
    // succeeds but copies nothing — masking the real --mirror bug.
    heddle(&["import", "git", "--ref", "main"], Some(source.path()))
        .expect("import git should bootstrap the overlay sidecar");

    let primary_path = primary_remote.path().to_string_lossy().to_string();
    let mirror_path = mirror_remote.path().to_string_lossy().to_string();
    let mirror_arg = format!("--mirror={}", mirror_path);
    heddle(&["push", &primary_path, &mirror_arg], Some(source.path()))
        .expect("push --mirror in GitOverlay repo should succeed");

    assert!(
        find_reference(&primary_repo, "refs/heads/main").is_ok(),
        "primary remote should have refs/heads/main after overlay push"
    );
    assert!(
        find_reference(&mirror_repo, "refs/heads/main").is_ok(),
        "mirror remote MUST ALSO have refs/heads/main — the GitOverlay early-return previously bypassed --mirror"
    );
}

/// `render_mirror_outcome` JSON must use RFC 8259 escaping — not
/// Rust's `Debug` format. A remote name containing U+2028
/// (LINE SEPARATOR) round-trips through `{:?}` as `"\u{2028}"`
/// (Rust syntax), which is NOT valid JSON. With proper serde
/// serialization the output parses and the field round-trips.
#[test]
fn test_cli_push_mirror_json_uses_rfc8259_escaping_for_unicode() {
    let source = TempDir::new().unwrap();
    let weft_target = TempDir::new().unwrap();
    let mirror_parent = TempDir::new().unwrap();
    // A real bare git repo at a path containing U+2028, so the mirror
    // push succeeds and the `"remote"` field carries the bad codepoint.
    let mirror_dir = mirror_parent.path().join("mirror\u{2028}suffix");
    std::fs::create_dir_all(&mirror_dir).unwrap();
    let mirror_repo = SleyRepository::init_bare(&mirror_dir).unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(weft_target.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "u+2028").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(source.path())).unwrap();

    let weft_path = weft_target.path().to_string_lossy().to_string();
    let mirror_path = mirror_dir.to_string_lossy().to_string();
    let mirror_arg = format!("--mirror={}", mirror_path);
    let output = heddle_output(
        &[
            "--output",
            "json",
            "push",
            &weft_path,
            "--thread",
            "main",
            &mirror_arg,
        ],
        Some(source.path()),
    )
    .expect("push --output json --mirror=<U+2028> must invoke");
    let stderr = std::str::from_utf8(&output.stderr).unwrap_or("");
    assert!(
        output.status.success(),
        "primary push must succeed: stderr={stderr}"
    );

    // Mirror outcome JSON now lives on stderr (PR #251 contract — stdout
    // stays a single JSON object).
    let mirror_line = stderr
        .lines()
        .find(|line| line.contains("\"mirrored\""))
        .unwrap_or_else(|| panic!("mirror outcome JSON line missing on stderr: {stderr}"));

    // The Debug-format bug emits `"\u{2028}"` (literal braces), which
    // is not valid JSON — `serde_json::from_str` rejects it.
    let parsed: serde_json::Value = serde_json::from_str(mirror_line).unwrap_or_else(|err| {
        panic!(
            "mirror outcome must be RFC 8259 JSON, got {}: {:?}",
            err, mirror_line
        )
    });
    assert_eq!(
        parsed["remote"].as_str(),
        Some(mirror_path.as_str()),
        "remote field must round-trip the U+2028 codepoint exactly"
    );
    // Sanity: mirror push landed too.
    assert!(
        find_reference(&mirror_repo, "refs/heads/main").is_ok(),
        "mirror push should have landed at the U+2028 path"
    );
}

/// JSON output path on mirror failure: covers the `mirrored:false`
/// + `error` branch of `render_mirror_outcome`.
#[test]
fn test_cli_push_mirror_json_failure_emits_mirrored_false_with_error() {
    let source = TempDir::new().unwrap();
    let weft_target = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(weft_target.path())).unwrap();
    std::fs::write(source.path().join("file.txt"), "json err").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(source.path())).unwrap();

    let weft_path = weft_target.path().to_string_lossy().to_string();
    let bogus = source
        .path()
        .join("nope-mirror")
        .to_string_lossy()
        .to_string();
    let mirror_arg = format!("--mirror={}", bogus);
    let output = heddle_output(
        &[
            "--output",
            "json",
            "push",
            &weft_path,
            "--thread",
            "main",
            &mirror_arg,
        ],
        Some(source.path()),
    )
    .expect("primary push must invoke even when mirror push fails");
    let stderr = std::str::from_utf8(&output.stderr).unwrap_or("");
    assert!(
        output.status.success(),
        "primary push must succeed even when mirror push fails: stderr={stderr}"
    );

    // Mirror failure JSON lives on stderr (PR #251 contract).
    assert!(
        stderr.contains("\"mirrored\":false"),
        "JSON mirror-failure line missing on stderr: {stderr}"
    );
    assert!(
        stderr.contains("\"error\""),
        "JSON mirror failure must include error field: {stderr}"
    );
}
