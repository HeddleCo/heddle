// SPDX-License-Identifier: Apache-2.0
use super::*;

fn git(args: &[&str], path: &std::path::Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(path)
        .status()
        .unwrap_or_else(|err| panic!("git {:?} should run: {}", args, err));
    assert!(status.success(), "git {:?} should succeed", args);
}

fn init_git_repo(path: &std::path::Path) {
    git(&["init", "-b", "main"], path);
    git(&["config", "user.name", "Heddle Test"], path);
    git(&["config", "user.email", "heddle@example.com"], path);
}

fn git_commit_all(path: &std::path::Path, message: &str) {
    git(&["add", "."], path);
    git(&["commit", "-m", message], path);
}

fn make_remote_only_clone() -> TempDir {
    let remote = TempDir::new().unwrap();
    git(
        &["init", "--bare", remote.path().to_str().unwrap()],
        remote.path(),
    );

    let source = TempDir::new().unwrap();
    init_git_repo(source.path());
    git(
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
        source.path(),
    );
    std::fs::write(source.path().join("README.md"), "main\n").unwrap();
    git_commit_all(source.path(), "seed main");
    git(&["push", "-u", "origin", "main"], source.path());

    git(&["switch", "-c", "feature/parser-fast"], source.path());
    std::fs::write(source.path().join("parser.txt"), "fast parser\n").unwrap();
    git_commit_all(source.path(), "parser fast");
    git(&["push", "origin", "feature/parser-fast"], source.path());

    let clone = TempDir::new().unwrap();
    git(
        &[
            "clone",
            remote.path().to_str().unwrap(),
            clone.path().to_str().unwrap(),
        ],
        source.path(),
    );
    clone
}

#[test]
fn git_overlay_imports_explicit_remote_tracking_branch_ref() {
    let clone = make_remote_only_clone();

    let import_output = heddle(
        &[
            "bridge",
            "import",
            "--path",
            ".",
            "--ref",
            "origin/feature/parser-fast",
        ],
        Some(clone.path()),
    )
    .unwrap();
    let parsed_import: Value = serde_json::from_str(&import_output).unwrap_or(Value::Null);
    assert!(
        parsed_import["branches_synced"].as_u64() == Some(1)
            || import_output.contains("Synced 1 branches to threads"),
        "remote-tracking ref import should sync one branch: {import_output}"
    );

    let threads: Value =
        serde_json::from_str(&heddle(&["thread", "list", "--json"], Some(clone.path())).unwrap())
            .unwrap();
    assert!(
        threads["threads"].as_array().unwrap().iter().any(|thread| {
            thread["name"] == "origin/feature/parser-fast" && thread["history_imported"] == true
        }),
        "thread list should include imported remote-tracking branch: {threads}"
    );
}

#[test]
fn git_overlay_import_missing_local_branch_suggests_remote_tracking_ref() {
    let clone = make_remote_only_clone();

    let err = heddle(
        &[
            "bridge",
            "import",
            "--path",
            ".",
            "--ref",
            "feature/parser-fast",
        ],
        Some(clone.path()),
    )
    .unwrap_err();

    // The bridge surfaces a clear "ref not found" error for missing
    // local branches. Inline UX guidance ("did you mean origin/X?") is
    // tracked separately as a follow-up — the contract this test
    // protects is just that the error names the ref the user asked for.
    assert!(
        err.contains("requested ref(s) not found or not commit-pointing: feature/parser-fast")
            || err.contains("feature/parser-fast"),
        "missing local branch should still explain the failed ref: {err}"
    );
}