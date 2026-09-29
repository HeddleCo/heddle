// SPDX-License-Identifier: Apache-2.0

#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::Path,
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(dir)
        .env(
            "HEDDLE_HOME",
            dir.parent().expect("repo parent").join("home"),
        )
        .env("TMPDIR", "/home/scratch")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HEDDLE_PRINCIPAL_NAME", "Owner")
        .env("HEDDLE_PRINCIPAL_EMAIL", "owner@example.com")
        .env("GIT_AUTHOR_NAME", "C")
        .env("GIT_AUTHOR_EMAIL", "c@e")
        .env("GIT_COMMITTER_NAME", "C")
        .env("GIT_COMMITTER_EMAIL", "c@e");
    command.output().expect("run Git or Heddle")
}

fn checked(dir: &Path, program: &str, args: &[&str]) -> String {
    let output = run(dir, program, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("UTF-8 output")
        .trim()
        .to_string()
}

#[test]
fn failed_undo_rollback_preserves_concurrent_git_commit() {
    let temp = TempDir::new().expect("tempdir");
    let repo_path = temp.path().join("repo");
    fs::create_dir(&repo_path).expect("repo");
    let dir = repo_path.as_path();
    fs::create_dir(temp.path().join("home")).expect("home");
    let heddle = env!("CARGO_BIN_EXE_heddle");
    checked(dir, "git", &["init", "-q", "-b", "main"]);
    checked(dir, "git", &["config", "user.name", "Owner"]);
    checked(dir, "git", &["config", "user.email", "o@e"]);
    fs::write(dir.join("a"), "base\n").expect("base file");
    fs::create_dir(dir.join("sub")).expect("subdir");
    fs::write(dir.join("sub/b"), "base\n").expect("nested base file");
    checked(dir, "git", &["add", "."]);
    checked(dir, "git", &["commit", "-qm", "base"]);
    checked(dir, heddle, &["--output", "json", "init"]);
    checked(
        dir,
        heddle,
        &[
            "--output", "json", "bridge", "git", "import", "--ref", "main",
        ],
    );
    fs::write(dir.join("sub/b"), "captured\n").expect("captured file");
    checked(
        dir,
        heddle,
        &["--output", "json", "capture", "-m", "capture"],
    );
    let previous = checked(dir, "git", &["rev-parse", "HEAD~1"]);
    let captured = checked(dir, "git", &["rev-parse", "HEAD"]);
    let old_tree = checked(dir, "git", &["rev-parse", "HEAD~1^{tree}"]);
    let foreign = checked(
        dir,
        "git",
        &[
            "commit-tree",
            &old_tree,
            "-p",
            &previous,
            "-m",
            "concurrent commit on previous",
        ],
    );

    // Force the Heddle worktree restore to fail after Git has moved main.
    fs::set_permissions(dir.join("sub/b"), fs::Permissions::from_mode(0o444))
        .expect("make file readonly");
    fs::set_permissions(dir.join("sub"), fs::Permissions::from_mode(0o555))
        .expect("make directory readonly");
    let stop = Arc::new(AtomicBool::new(false));
    let watcher_stop = Arc::clone(&stop);
    let watcher_dir = dir.to_path_buf();
    let watcher_previous = previous.clone();
    let watcher_foreign = foreign.clone();
    let watcher = thread::spawn(move || {
        let reference = watcher_dir.join(".git/refs/heads/main");
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && !watcher_stop.load(Ordering::Relaxed) {
            if fs::read_to_string(&reference)
                .ok()
                .as_deref()
                .map(str::trim)
                == Some(watcher_previous.as_str())
            {
                return run(
                    &watcher_dir,
                    "git",
                    &[
                        "update-ref",
                        "refs/heads/main",
                        &watcher_foreign,
                        &watcher_previous,
                    ],
                )
                .status
                .success();
            }
            thread::sleep(Duration::from_micros(200));
        }
        false
    });
    let undo = run(dir, heddle, &["--output", "json", "undo", "--hard"]);
    stop.store(true, Ordering::Relaxed);
    let raced = watcher.join().expect("watcher thread");
    fs::set_permissions(dir.join("sub"), fs::Permissions::from_mode(0o755))
        .expect("restore directory permissions");
    fs::set_permissions(dir.join("sub/b"), fs::Permissions::from_mode(0o644))
        .expect("restore file permissions");

    assert!(
        !undo.status.success(),
        "worktree restore should fail: {}",
        String::from_utf8_lossy(&undo.stdout)
    );
    assert!(raced, "concurrent update-ref must win the race");
    let main = checked(dir, "git", &["rev-parse", "main"]);
    assert_eq!(
        main, foreign,
        "rollback replaced the concurrent commit; capture was {captured}"
    );
    assert!(
        run(
            dir,
            "git",
            &["merge-base", "--is-ancestor", &foreign, "main"]
        )
        .status
        .success(),
        "concurrent commit must remain reachable from main"
    );
}
