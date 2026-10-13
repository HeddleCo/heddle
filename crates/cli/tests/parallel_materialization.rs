// SPDX-License-Identifier: Apache-2.0
//! Parallel checkout must finish through both incremental and full-tree paths.

use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(path)
        .args(args)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn heddle(path: &Path, home: &Path, workers: &str, args: &[&str]) {
    let stdout = tempfile::tempfile().expect("stdout");
    let stderr = tempfile::tempfile().expect("stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .current_dir(path)
        .args(["--output", "json"])
        .args(args)
        .env("HEDDLE_HOME", home)
        .env("HEDDLE_MATERIALIZE_THREADS", workers)
        .env("HEDDLE_FSMONITOR", "off")
        .env("HEDDLE_PRINCIPAL_NAME", "Materialization Test")
        .env("HEDDLE_PRINCIPAL_EMAIL", "materialize@example.test")
        .stdout(Stdio::from(stdout.try_clone().expect("stdout clone")))
        .stderr(Stdio::from(stderr.try_clone().expect("stderr clone")))
        .spawn()
        .expect("heddle");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            use std::io::{Read, Seek};
            let read = |mut file: fs::File| {
                file.rewind().expect("rewind");
                let mut text = String::new();
                file.read_to_string(&mut text).expect("output");
                text
            };
            assert!(
                status.success(),
                "heddle {args:?} with {workers} workers: {status}\n{}\n{}",
                read(stdout),
                read(stderr)
            );
            return;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill deadlocked CLI");
            child.wait().expect("reap CLI");
            panic!("heddle {args:?} with {workers} workers timed out after 30s");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn assert_files(path: &Path) {
    for index in 1..=64 {
        assert_eq!(
            fs::read_to_string(path.join(format!("d{}/f{index}.txt", index % 7)))
                .expect("materialized file"),
            format!("file {index}\n")
        );
    }
}

fn checkout_paths(workers: &str) {
    let temp = tempfile::tempdir().expect("fixture");
    let src = temp.path().join("src");
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    fs::create_dir(&src).expect("source");
    fs::create_dir(&repo).expect("repository");
    git(&src, &["init", "-q", "-b", "main"]);
    git(&src, &["config", "user.name", "Materialization Test"]);
    git(&src, &["config", "user.email", "materialize@example.test"]);
    for index in 1..=64 {
        let dir = src.join(format!("d{}", index % 7));
        fs::create_dir_all(&dir).expect("directory");
        fs::write(dir.join(format!("f{index}.txt")), format!("file {index}\n")).expect("file");
    }
    git(&src, &["add", "."]);
    git(&src, &["commit", "-qm", "seed"]);
    heddle(&repo, &home, workers, &["init", "--no-harness-install"]);
    heddle(&repo, &home, workers, &["thread", "create", "empty"]);
    heddle(
        &repo,
        &home,
        workers,
        &[
            "bridge", "git", "import", "--path", "../src", "--ref", "main",
        ],
    );
    assert_files(&repo);
    heddle(&repo, &home, workers, &["thread", "switch", "empty"]);
    assert!(!repo.join("d1/f1.txt").exists(), "switch must remove files");
    heddle(&repo, &home, workers, &["thread", "switch", "main"]);
    assert_files(&repo);
    heddle(&repo, &home, workers, &["thread", "create", "checkout"]);
    let checkout = temp.path().join("checkout");
    heddle(
        &repo,
        &home,
        workers,
        &[
            "thread",
            "checkout",
            "checkout",
            "--path",
            checkout.to_str().expect("path"),
        ],
    );
    assert_files(&checkout);
}

#[test]
fn checkout_paths_2_workers() {
    checkout_paths("2");
}

#[test]
fn checkout_paths_4_workers() {
    checkout_paths("4");
}

#[test]
fn checkout_paths_8_workers() {
    checkout_paths("8");
}
