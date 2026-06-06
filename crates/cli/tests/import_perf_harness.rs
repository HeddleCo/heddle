//! Ad-hoc timed harness for `heddle adopt` commit import (#550).
//! Run with: `cargo test -p heddle-cli --test import_perf_harness \
//!   --features git-overlay,native,semantic,zstd -- --ignored --nocapture`

use std::time::Instant;

use cli::Repository;
use cli::bridge::{GitBridge, git_import::import_all};
use gix::refs::transaction::PreviousValue;
use tempfile::TempDir;

fn sig() -> gix::actor::Signature {
    gix::actor::Signature {
        name: "Bench".into(),
        email: "bench@test".into(),
        time: gix::date::Time {
            seconds: 0,
            offset: 0,
        },
    }
}

/// Build an N-commit linear history where each commit rewrites one file
/// (so every commit yields a fresh blob + fresh root tree — the realistic
/// adopt shape, not a degenerate all-trees-cached chain).
fn build_history(repo: &gix::Repository, n: usize) {
    let mut parents: Vec<gix::hash::ObjectId> = Vec::new();
    for i in 0..n {
        let blob = repo
            .write_blob(format!("content for commit {i}\n").as_bytes())
            .expect("write blob")
            .detach();
        let mut editor = repo
            .edit_tree(gix::hash::ObjectId::empty_tree(repo.object_hash()))
            .expect("tree editor");
        editor
            .upsert("file.txt", gix::object::tree::EntryKind::Blob, blob)
            .expect("upsert");
        // A second file that stays constant — exercises the tree-cache miss
        // on the changing entry while keeping trees non-trivial.
        let stable = repo
            .write_blob(b"stable\n".as_ref())
            .expect("write stable blob")
            .detach();
        editor
            .upsert("README", gix::object::tree::EntryKind::Blob, stable)
            .expect("upsert stable");
        let tree = editor.write().expect("write tree").detach();

        let mut cbuf = gix::date::parse::TimeBuf::default();
        let mut abuf = gix::date::parse::TimeBuf::default();
        let commit = repo
            .new_commit_as(
                sig().to_ref(&mut cbuf),
                sig().to_ref(&mut abuf),
                format!("commit {i}"),
                tree,
                parents.clone(),
            )
            .expect("commit");
        parents = vec![commit.id];
    }
    // Point main at the tip.
    if let Some(tip) = parents.first() {
        gix::Repository::edit_reference(
            repo,
            gix::refs::transaction::RefEdit {
                change: gix::refs::transaction::Change::Update {
                    log: gix::refs::transaction::LogChange::default(),
                    expected: PreviousValue::Any,
                    new: gix::refs::Target::Object(*tip),
                },
                name: "refs/heads/main".try_into().unwrap(),
                deref: false,
            },
        )
        .expect("set main");
    }
}

#[test]
#[ignore = "perf harness; run explicitly with --ignored --nocapture"]
fn time_commit_import() {
    let n: usize = std::env::var("IMPORT_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000);

    let git_temp = TempDir::new().expect("git temp");
    let git_repo = gix::init(git_temp.path()).expect("init git");
    let build_start = Instant::now();
    build_history(&git_repo, n);
    eprintln!(
        "built {n}-commit fixture in {:?}",
        build_start.elapsed()
    );

    let heddle_temp = TempDir::new().expect("heddle temp");
    let repo = Repository::init(heddle_temp.path()).expect("init heddle");
    let mut bridge = GitBridge::new(&repo);

    let git_path = git_repo.workdir().expect("workdir").to_path_buf();
    let start = Instant::now();
    let stats = import_all(&mut bridge, Some(&git_path)).expect("import");
    let elapsed = start.elapsed();

    eprintln!(
        "IMPORT {n} commits ({} states) in {:?} => {:.0} commits/s",
        stats.states_created,
        elapsed,
        n as f64 / elapsed.as_secs_f64()
    );
}
