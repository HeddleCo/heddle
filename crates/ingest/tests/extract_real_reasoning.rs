// SPDX-License-Identifier: Apache-2.0
//! Smoke test: run the reasoning extractor over real Claude sessions
//! for the Heddle repo and print the top candidates. Ignored by default.
//!
//! ```text
//! HEDDLE_MATCHER_SMOKE_REPO=/Users/foo/dev/heddle \
//!   cargo test -p ingest --test extract_real_reasoning -- --ignored --nocapture
//! ```
//!
//! The test keeps assertions minimal — it only fails if the extractor
//! produces zero points across *all* recent sessions, which would
//! indicate a regression in the harvest or keep stage. It mainly
//! exists as a human-reviewable print-out for tuning the keyword
//! rules and keep thresholds.

use std::path::{Path, PathBuf};

use ingest::{
    HarvestParams, KeepParams, Provider, TranscriptRoots, extract_reasoning_points,
    load_transcripts,
};

fn resolve_repo_root() -> PathBuf {
    if let Ok(p) = std::env::var("HEDDLE_MATCHER_SMOKE_REPO") {
        return PathBuf::from(p);
    }
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("workspace root")
}

#[test]
#[ignore = "reads real $HOME transcript store; run with --ignored"]
fn extracts_reasoning_points_from_recent_sessions() {
    let repo_root = resolve_repo_root();
    if !repo_root.join(".git").exists() {
        eprintln!("skip: {} is not a git repo", repo_root.display());
        return;
    }

    let roots = TranscriptRoots::default();
    let mut transcripts = load_transcripts(&repo_root, &roots);
    transcripts.sort_by_key(|t| std::cmp::Reverse(t.ended_at));
    // Focus on the 5 newest Claude sessions — Codex is stubbed so we'd
    // get empty harvests for those.
    let picks: Vec<_> = transcripts
        .iter()
        .filter(|t| matches!(t.provider, Provider::Claude))
        .take(20)
        .collect();
    println!(
        "extracting from {} Claude sessions (newest first)",
        picks.len()
    );
    if picks.is_empty() {
        eprintln!(
            "skip: no Claude transcripts found for cwd={}; \
             set HEDDLE_MATCHER_SMOKE_REPO to a path with cached sessions to exercise the extractor",
            repo_root.display()
        );
        return;
    }

    let hparams = HarvestParams::default();
    let kparams = KeepParams::default();

    let mut total_points = 0usize;
    for t in picks {
        let points =
            extract_reasoning_points(t, "0000000000000000", &hparams, &kparams).unwrap_or_default();
        println!(
            "\nsession {:.8}  cwd={:?}  points={}",
            t.session_id,
            t.cwd,
            points.len(),
        );
        // Show top 6 by confidence so the output stays readable.
        let mut sorted = points.clone();
        sorted.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for p in sorted.iter().take(6) {
            let kind = format!("{:?}", p.kind).to_lowercase();
            let file = if p.target.file.is_empty() {
                "(file-scope)".to_string()
            } else {
                p.target.file.clone()
            };
            println!(
                "  {:>4.2}  {:<9} {}  →  {}",
                p.confidence, kind, p.text, file
            );
        }
        total_points += points.len();
    }

    println!("\nsummary: {total_points} reasoning points across sessions");
    assert!(
        total_points > 0,
        "expected at least one point from a real Claude session"
    );
}
