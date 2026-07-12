// SPDX-License-Identifier: Apache-2.0
use std::{path::Path, process::Command};

use objects::store::ObjectStore;
use repo::Repository;

use super::*;

#[derive(Debug)]
struct RepositorySnapshotProfile {
    tree_walk_ms: u128,
    blob_prep_ms: u128,
    blob_write_ms: u128,
    tree_write_ms: u128,
    state_ref_oplog_ms: u128,
    snapshot_total: Duration,
}

fn write_snapshot_bench_files(root: &Path, file_count: usize) {
    for i in 0..file_count {
        fs::write(
            root.join(format!("file{i}.txt")),
            format!("content {i}\n{}\n", "x".repeat(48)),
        )
        .unwrap();
    }
}

fn measure_repository_snapshot(file_count: usize) -> RepositorySnapshotProfile {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    write_snapshot_bench_files(temp.path(), file_count);
    let attribution = repo.get_attribution().unwrap();

    let start = Instant::now();
    let execution = repo
        .snapshot_with_attribution_profiled(Some("Many files".to_string()), None, attribution)
        .unwrap();
    let snapshot_total = start.elapsed();

    assert!(
        repo.store()
            .get_tree(&execution.state.tree)
            .unwrap()
            .is_some(),
        "repository snapshot benchmark should materialize a tree"
    );

    RepositorySnapshotProfile {
        tree_walk_ms: execution.profile.tree_walk_ms,
        blob_prep_ms: execution.profile.blob_prep_ms,
        blob_write_ms: execution.profile.blob_write_ms,
        tree_write_ms: execution.profile.tree_write_ms,
        state_ref_oplog_ms: execution.profile.state_ref_oplog_ms,
        snapshot_total,
    }
}

fn try_run_git(dir: &Path, args: &[&str]) -> Option<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .ok()?;
    status.success().then_some(())
}

fn try_measure_git_snapshot(file_count: usize) -> Option<(Duration, Duration)> {
    let version = Command::new("git").arg("--version").status().ok()?;
    if !version.success() {
        return None;
    }

    let temp = TempDir::new().ok()?;
    try_run_git(temp.path(), &["init", "-q"])?;
    try_run_git(temp.path(), &["config", "user.name", "Heddle Bench"])?;
    try_run_git(
        temp.path(),
        &["config", "user.email", "heddle-bench@example.com"],
    )?;
    write_snapshot_bench_files(temp.path(), file_count);

    let add_start = Instant::now();
    try_run_git(temp.path(), &["add", "-A"])?;
    let add_elapsed = add_start.elapsed();

    let commit_start = Instant::now();
    try_run_git(temp.path(), &["commit", "-qm", "benchmark"])?;
    let commit_elapsed = commit_start.elapsed();

    Some((add_elapsed, commit_elapsed))
}

fn print_snapshot_cli_report(
    file_count: usize,
    repository_profile: &RepositorySnapshotProfile,
    cli_elapsed: Duration,
    git_baseline: Option<(Duration, Duration)>,
) {
    let cli_overhead = cli_elapsed
        .checked_sub(repository_profile.snapshot_total)
        .unwrap_or_default();
    println!(
        "snapshot perf report: files={} repository_snapshot={:?} tree_walk_ms={} blob_prep_ms={} blob_write_ms={} tree_write_ms={} state_ref_oplog_ms={} cli_end_to_end={:?} cli_overhead_estimate={:?}",
        file_count,
        repository_profile.snapshot_total,
        repository_profile.tree_walk_ms,
        repository_profile.blob_prep_ms,
        repository_profile.blob_write_ms,
        repository_profile.tree_write_ms,
        repository_profile.state_ref_oplog_ms,
        cli_elapsed,
        cli_overhead
    );

    match git_baseline {
        Some((add, commit)) => println!(
            "snapshot perf report: git_add={:?} git_commit={:?} git_total={:?}",
            add,
            commit,
            add + commit
        ),
        None => println!("snapshot perf report: git baseline unavailable; skipping parity output"),
    }
}

#[test]
fn test_snapshot_performance_small_repo() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");
    // Debug budget: L8 pack-install journal adds durable fsyncs; parallel
    // comprehensive harness adds scheduler noise. Keep release tight.
    let max_duration = performance_budget(Duration::from_millis(500), Duration::from_secs(2));

    assert_performance(
        "snapshot small repo",
        || {
            fs::write(temp.path().join("new.txt"), "new").unwrap();
            heddle(&["capture", "-m", "Test"], Some(temp.path())).unwrap();
        },
        max_duration,
    );
}

#[test]
fn test_snapshot_performance_many_files() {
    let file_count = 1_000usize;
    let repository_profile = measure_repository_snapshot(file_count);
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    write_snapshot_bench_files(temp.path(), file_count);

    let start = Instant::now();
    heddle(&["capture", "-m", "Many files"], Some(temp.path())).unwrap();
    let cli_elapsed = start.elapsed();

    print_snapshot_cli_report(
        file_count,
        &repository_profile,
        cli_elapsed,
        try_measure_git_snapshot(file_count),
    );

    assert!(
        cli_elapsed < Duration::from_secs(20),
        "snapshot 1000 files took {:?}, expected under {:?}",
        cli_elapsed,
        Duration::from_secs(20)
    );
}

#[test]
fn test_status_performance_large_repo() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for i in 0..500 {
        fs::write(
            temp.path().join(format!("file{}.txt", i)),
            format!("content {}", i),
        )
        .unwrap();
    }
    heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();

    for i in 0..100 {
        fs::write(
            temp.path().join(format!("file{}.txt", i)),
            format!("modified {}", i),
        )
        .unwrap();
    }

    assert_performance(
        "status with 500 files, 100 modified",
        || {
            let _ = heddle(&["status"], Some(temp.path()));
        },
        performance_budget(Duration::from_secs(5), Duration::from_secs(10)),
    );
}

#[test]
// 10k-line × 1k-change diff: 3s on release, ~6× slower in debug. We
// scale the budget when `debug_assertions` are on so
// `--include-ignored` (debug) still catches catastrophic regressions
// without flapping on the slow path. Run with
// `cargo test -- --include-ignored --release` for the production budget.
#[ignore = "release-build perf budget; run with --include-ignored --release"]
fn test_diff_performance_large_file() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let mut content = String::new();
    for i in 0..10000 {
        content.push_str(&format!("Line {} content here with some data\n", i));
    }
    fs::write(temp.path().join("large.txt"), content).unwrap();
    heddle(&["capture", "-m", "Large"], Some(temp.path())).unwrap();

    let mut modified = String::new();
    for i in 0..10000 {
        if i % 10 == 0 {
            modified.push_str(&format!("Line {} MODIFIED content\n", i));
        } else {
            modified.push_str(&format!("Line {} content here with some data\n", i));
        }
    }
    fs::write(temp.path().join("large.txt"), modified).unwrap();

    let budget = if cfg!(debug_assertions) {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(3)
    };
    assert_performance(
        "diff 10k line file with 1k changes",
        || {
            let _ = heddle(&["diff"], Some(temp.path()));
        },
        budget,
    );
}

#[test]
fn test_log_performance_deep_history() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for i in 0..100 {
        fs::write(temp.path().join("counter.txt"), format!("{}", i)).unwrap();
        heddle(
            &["capture", "-m", &format!("Commit {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    assert_performance(
        "log with 100 commits",
        || {
            let _ = heddle(&["log", "--oneline"], Some(temp.path()));
        },
        performance_budget(Duration::from_secs(2), Duration::from_secs(4)),
    );
}

#[test]
fn test_gc_performance_many_objects() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for i in 0..50 {
        for j in 0..20 {
            fs::write(
                temp.path().join(format!("file{}_{}.txt", i, j)),
                format!("content {} {}", i, j),
            )
            .unwrap();
        }
        heddle(
            &["capture", "-m", &format!("Commit {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    assert_performance(
        "gc with 1000 objects",
        || {
            heddle(&["maintenance", "gc", "--aggressive"], Some(temp.path())).unwrap();
        },
        performance_budget(Duration::from_secs(5), Duration::from_secs(10)),
    );
}
