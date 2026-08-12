// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
#[serial_test::serial(performance)]
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
#[serial_test::serial(performance)]
fn test_snapshot_performance_many_files() {
    let file_count = 1_000usize;
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    cli_test_support::write_many_small_files(temp.path(), file_count);

    let start = Instant::now();
    heddle(&["capture", "-m", "Many files"], Some(temp.path())).unwrap();
    let cli_elapsed = start.elapsed();

    assert!(
        cli_elapsed < Duration::from_secs(20),
        "snapshot 1000 files took {:?}, expected under {:?}",
        cli_elapsed,
        Duration::from_secs(20)
    );
    let status = status_json(temp.path());
    for kind in ["added", "modified", "deleted"] {
        assert_eq!(status["changes"][kind], serde_json::json!([]), "{status}");
    }
}

#[test]
#[serial_test::serial(performance)]
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
            heddle(&["status"], Some(temp.path())).expect("status should succeed");
        },
        performance_budget(Duration::from_secs(5), Duration::from_secs(10)),
    );
}

#[test]
#[serial_test::serial(performance)]
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
            heddle(&["diff"], Some(temp.path())).expect("diff should succeed");
        },
        budget,
    );
}

#[test]
#[serial_test::serial(performance)]
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
            heddle(&["log", "--oneline"], Some(temp.path())).expect("log should succeed");
        },
        performance_budget(Duration::from_secs(2), Duration::from_secs(4)),
    );
}

#[test]
#[serial_test::serial(performance)]
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
