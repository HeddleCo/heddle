// SPDX-License-Identifier: Apache-2.0

use std::{path::Path, process::Command};

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-C", repo.to_str().unwrap()])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn ingest(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_heddle-ingest"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn import_map_and_hermetic_reason_cover_the_user_workflow() {
    let source = tempfile::TempDir::new().unwrap();
    let target = tempfile::TempDir::new().unwrap();
    git(source.path(), &["init", "-b", "main"]);
    git(source.path(), &["config", "user.name", "Ingest Test"]);
    git(
        source.path(),
        &["config", "user.email", "ingest@example.com"],
    );
    std::fs::write(source.path().join("tracked.txt"), "first\n").unwrap();
    git(source.path(), &["add", "tracked.txt"]);
    git(source.path(), &["commit", "-m", "seed"]);
    let sha = git(source.path(), &["rev-parse", "HEAD"]);

    let source_arg = source.path().to_str().unwrap();
    let target_arg = target.path().to_str().unwrap();
    let imported = ingest(&[
        "import", "--git", source_arg, "--heddle", target_arg, "--ref", "main",
    ]);
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let import_text = String::from_utf8(imported.stdout).unwrap();
    assert!(import_text.contains("commits:\n  imported: 1"));
    assert!(import_text.contains("threads written:"));

    let map = target.path().join(".heddle/ingest/sha_map.sqlite");
    let map_arg = map.to_str().unwrap();
    let stats = ingest(&["map", map_arg, "stats"]);
    assert!(stats.status.success());
    assert!(
        String::from_utf8(stats.stdout)
            .unwrap()
            .contains("commits: 1")
    );

    let lookup = ingest(&["map", map_arg, "lookup-git", &sha]);
    assert!(lookup.status.success());
    let lookup_text = String::from_utf8(lookup.stdout).unwrap();
    let state = lookup_text.split_whitespace().last().unwrap();
    assert!(state.starts_with("hs-"), "{lookup_text}");

    let reverse = ingest(&["map", map_arg, "lookup-heddle", state]);
    assert!(reverse.status.success());
    assert!(String::from_utf8(reverse.stdout).unwrap().contains(&sha));

    let reasoned = ingest(&[
        "reason",
        "--git",
        source_arg,
        "--heddle",
        target_arg,
        "--commit",
        &sha,
        "--claude-home",
        "",
        "--codex-home",
        "",
        "--opencode-home",
        "",
        "--dry-run",
    ]);
    assert!(
        reasoned.status.success(),
        "{}",
        String::from_utf8_lossy(&reasoned.stderr)
    );
    let reason_text = String::from_utf8(reasoned.stdout).unwrap();
    assert!(reason_text.contains("loaded 0 transcripts"));
    assert!(reason_text.contains("dry-run: not writing annotations"));
    assert!(reason_text.contains("no commits matched any session"));
}
