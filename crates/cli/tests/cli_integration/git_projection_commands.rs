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

fn init_direct_git_overlay(path: &std::path::Path) {
    heddle(
        &[
            "init",
            "--principal-name",
            "Heddle Test",
            "--principal-email",
            "heddle@example.com",
        ],
        Some(path),
    )
    .expect("initialize direct Git Overlay");
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

#[test]
fn git_overlay_capture_recovers_checkpoint_intent_without_bridge_mirror() {
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
        let crashed = heddle_output_with_env(
            &["capture", "-m", fault],
            Some(repo.path()),
            &[("HEDDLE_FAULT_INJECT", fault)],
        )
        .expect("run faulted capture");
        assert!(
            !crashed.status.success(),
            "fault point {fault} must stop capture"
        );
        assert!(
            repo.path()
                .join(".heddle/state/git-checkpoint-intent.json")
                .is_file(),
            "fault point {fault} must leave a durable recovery intent"
        );

        heddle(&["capture", "-m", fault], Some(repo.path())).expect("retry checkpoint recovery");
        assert!(
            !repo
                .path()
                .join(".heddle/state/git-checkpoint-intent.json")
                .exists(),
            "retry after {fault} must finalize the intent"
        );
        assert!(
            !repo.path().join(".heddle/git").exists(),
            "capture must write through Sley to the checkout .git"
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
        &[
            "bridge",
            "git",
            "import",
            "--path",
            source.path().to_str().unwrap(),
        ],
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
            "bridge",
            "git",
            "import",
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
    let output = heddle_help(&["bridge", "git", "import", "--help"]);

    assert!(output.contains("--lossy"), "help should document --lossy");
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

    // Phase A: `bridge git export` requires `--destination`. Pre-Phase-A
    // it silently no-op'd if no flag was given (writing only the sidecar
    // mapping, not actually exporting any git objects). Now it errors.
    let export = heddle(
        &[
            "bridge",
            "git",
            "export",
            "--destination",
            dest.to_str().unwrap(),
        ],
        Some(source.path()),
    );
    assert!(
        export.is_ok(),
        "bridge git export failed: {:?}",
        export.err()
    );

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
    std::fs::write(source.path().join("file.txt"), "bridge git export").unwrap();
    heddle(&["capture", "-m", "Export git source"], Some(source.path())).unwrap();

    let export = heddle(
        &[
            "bridge",
            "git",
            "export",
            "--destination",
            dest.to_str().unwrap(),
        ],
        Some(source.path()),
    );
    assert!(
        export.is_ok(),
        "bridge git export failed: {:?}",
        export.err()
    );

    let dest_repo = open_git(&dest).unwrap();
    assert!(find_reference(&dest_repo, "refs/heads/main").is_ok());
}

/// heddle#1096 -- an ordinary Git-side commit pushed onto a Heddle-managed
/// destination branch is foreign: Heddle never published that OID under the ref name.
/// Export must report the divergence and leave the foreign tip untouched, not
/// misclassify it as a now-embargoed tip and force-rewind it.
#[test]
fn test_cli_export_git_preserves_foreign_destination_push_and_reports_divergence() {
    let source = TempDir::new().unwrap();
    let dest_holder = TempDir::new().unwrap();
    let dest = dest_holder.path().join("export");

    init_direct_git_overlay(source.path());
    std::fs::write(source.path().join("file.txt"), "Heddle projection tip\n").unwrap();
    heddle(
        &["capture", "-m", "Heddle projection tip"],
        Some(source.path()),
    )
    .expect("capture projection tip");

    let first = heddle_output(
        &[
            "bridge",
            "git",
            "export",
            "--destination",
            dest.to_str().unwrap(),
        ],
        Some(source.path()),
    )
    .expect("first export");
    assert!(
        first.status.success(),
        "first export must succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr),
    );

    let destination_repo = open_git(&dest).expect("open export destination");
    let heddle_tip = find_reference(&destination_repo, "refs/heads/main")
        .expect("main after first export")
        .peel_to_id()
        .expect("peel Heddle tip");
    assert!(!source.path().join(".heddle/git").exists());
    let tree = destination_repo
        .read_commit(&heddle_tip)
        .expect("read Heddle tip")
        .tree;
    let foreign_tip = git_commit_with_tree(
        &destination_repo,
        Some("refs/heads/main"),
        tree,
        "foreign push",
        &[heddle_tip],
    );

    let second = heddle_output(
        &[
            "bridge",
            "git",
            "export",
            "--destination",
            dest.to_str().unwrap(),
        ],
        Some(source.path()),
    )
    .expect("second export");
    let stdout = String::from_utf8_lossy(&second.stdout);
    let stderr = String::from_utf8_lossy(&second.stderr);
    let destination_tip_after = find_reference(&destination_repo, "refs/heads/main")
        .expect("main after second export")
        .peel_to_id()
        .expect("peel destination tip after second export");

    let mut violations = Vec::new();
    if destination_tip_after != foreign_tip {
        violations.push(format!(
            "foreign tip was clobbered: expected {foreign_tip}, found {destination_tip_after}"
        ));
    }
    if second.status.code() != Some(74) {
        violations.push(format!(
            "second export must exit non-zero (74), got {:?}",
            second.status.code()
        ));
    }
    if !stdout.is_empty() {
        violations.push(format!(
            "a rejected export must not emit a success summary: {stdout}"
        ));
    }
    if !stderr.contains("Remote branch does not fast-forward the local Git checkpoint") {
        violations.push("stderr did not report the remote non-fast-forward".to_string());
    }
    if !stderr.contains("Next: heddle pull") {
        violations.push("stderr did not provide the safe recovery command".to_string());
    }

    assert!(
        violations.is_empty(),
        "{}\nstatus={:?}\nstdout={stdout}\nstderr={stderr}",
        violations.join("\n"),
        second.status.code(),
    );
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
            "bridge",
            "git",
            "import",
            "--path",
            git_repo_dir.path().to_str().unwrap(),
        ],
        Some(heddle_repo_dir.path()),
    );
    assert!(
        result.is_ok(),
        "bridge git import failed: {:?}",
        result.err()
    );

    let repo = Repository::open(heddle_repo_dir.path()).unwrap();
    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .is_some()
    );
    assert!(
        !heddle_repo_dir.path().join(".heddle/git").exists(),
        "explicit Git import must not eagerly populate the retired mirror"
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
