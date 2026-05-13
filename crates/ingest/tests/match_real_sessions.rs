// SPDX-License-Identifier: Apache-2.0
//! Smoke test: score real Claude/Codex sessions against real Heddle commits.
//!
//! Ignored by default (the test touches `$HOME` and is worthless in CI).
//! Run manually from the repo root:
//!
//! ```text
//! HEDDLE_MATCHER_SMOKE_REPO=/Users/lukethorne/dev/heddle \
//!   cargo test -p ingest --test match_real_sessions -- --ignored --nocapture
//! ```
//!
//! The test is a smoke harness, not an assertion: it prints the top
//! candidates for the N most recent commits so a human can eyeball
//! whether the scorer picks sessions that plausibly produced each
//! commit. The only hard assertion is that the matcher runs without
//! panicking and that *some* commits get non-zero matches.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use ingest::{MatchParams, TranscriptMatcher, TranscriptRoots, load_transcripts};

/// Resolve the repo to probe. Defaults to the crate's workspace root so
/// developers running the test from a fresh clone don't need to set any
/// env var.
fn resolve_repo_root() -> PathBuf {
    if let Ok(p) = std::env::var("HEDDLE_MATCHER_SMOKE_REPO") {
        return PathBuf::from(p);
    }
    // Fall back to the workspace root discovered relative to this file.
    // `CARGO_MANIFEST_DIR` is the crate dir; the workspace sits two up.
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent() // crates/
        .and_then(Path::parent) // workspace
        .map(Path::to_path_buf)
        .expect("workspace root")
}

/// `git log -n <n> --pretty=%H` — newest first.
fn recent_commit_shas(repo: &Path, n: usize) -> Vec<String> {
    let out = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["log", "--pretty=%H", "-n", &n.to_string()])
        .output()
        .expect("git log");
    assert!(out.status.success(), "git log failed: {out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// `git diff-tree --no-commit-id --name-only -r <sha>` — works for both
/// merge and root commits (root commits emit everything).
fn changed_files(repo: &Path, sha: &str) -> Vec<String> {
    let out = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["diff-tree", "--no-commit-id", "--name-only", "-r", sha])
        .output()
        .expect("git diff-tree");
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
#[ignore = "reads real $HOME transcript store; run with --ignored"]
fn matches_real_sessions_against_recent_heddle_commits() {
    let repo_root = resolve_repo_root();
    if !repo_root.join(".git").exists() {
        eprintln!("skip: {} is not a git repo", repo_root.display());
        return;
    }

    let roots = TranscriptRoots::default();
    let transcripts = load_transcripts(&repo_root, &roots);
    println!(
        "loaded {} transcripts for {}",
        transcripts.len(),
        repo_root.display()
    );
    if transcripts.is_empty() {
        eprintln!("no transcripts under $HOME — nothing to score");
        return;
    }

    let matcher =
        TranscriptMatcher::new(&transcripts, &repo_root).with_params(MatchParams::default());

    let shas = recent_commit_shas(&repo_root, 25);
    assert!(!shas.is_empty(), "no git commits found under repo root");

    // Build a CommitEntry per sha via the public GitSource. That mirrors
    // how the real importer hands commits to the matcher, so we test the
    // same path end-to-end.
    let src = ingest::GitSource::open(&repo_root).expect("git open");

    let mut commits_with_hits = 0usize;
    let mut commits_scored = 0usize;
    for sha in &shas {
        let commit = match src.read_commit(sha) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  skip {sha}: {e}");
                continue;
            }
        };
        let files = changed_files(&repo_root, sha);
        let matches = matcher.score_commit(&commit, &files);
        commits_scored += 1;

        let subject = commit
            .message
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(72)
            .collect::<String>();
        println!("\n{sha:.12}  {subject}");
        println!("  files: {}", files.len());
        if matches.is_empty() {
            println!("  (no candidates passed gates)");
            continue;
        }
        commits_with_hits += 1;
        for m in &matches {
            println!(
                "  {:>5.2}  {}  {:.8}  overlap={:.2} time={:.2} hint={:.2} paths={}",
                m.confidence,
                m.provider.as_str(),
                m.session_id,
                m.file_overlap,
                m.time_fit,
                m.provider_hint,
                m.overlap_count,
            );
        }
    }

    println!("\nsummary: {commits_with_hits}/{commits_scored} commits had at least one candidate");
    // Soft lower bound — if we score 25 recent commits and *nothing*
    // matches, something is broken (assuming the operator has ever used
    // Claude or Codex in this repo). Keep the bar low so a quiet week
    // doesn't fail the test.
    assert!(
        commits_with_hits > 0,
        "expected at least one scored commit to have a candidate"
    );
}