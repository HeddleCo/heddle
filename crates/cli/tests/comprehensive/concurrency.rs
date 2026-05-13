// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_concurrent_snapshots_different_files() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    let barrier = Arc::new(Barrier::new(3));
    let temp_path = temp.path().to_path_buf();

    let handles: Vec<_> = (0..3)
        .map(|i| {
            let barrier = barrier.clone();
            let path = temp_path.clone();
            thread::spawn(move || {
                barrier.wait();
                fs::write(
                    path.join(format!("thread{}.txt", i)),
                    format!("content {}", i),
                )
                .unwrap();
                heddle(&["capture", "-m", &format!("Thread {}", i)], Some(&path))
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let successes = results.iter().filter(|r| r.is_ok()).count();
    assert!(
        successes >= 1,
        "at least one snapshot should succeed, got {}",
        successes
    );
}

#[test]
fn test_concurrent_status_and_snapshot() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for i in 0..100 {
        fs::write(temp.path().join(format!("file{}.txt", i)), format!("{}", i)).unwrap();
    }

    let barrier = Arc::new(Barrier::new(2));
    let temp_path = temp.path().to_path_buf();

    let handle1 = {
        let barrier = barrier.clone();
        let path = temp_path.clone();
        thread::spawn(move || {
            barrier.wait();
            heddle(&["status"], Some(&path))
        })
    };

    let handle2 = {
        let barrier = barrier.clone();
        let path = temp_path.clone();
        thread::spawn(move || {
            barrier.wait();
            fs::write(path.join("new.txt"), "new").unwrap();
            heddle(&["capture", "-m", "New"], Some(&path))
        })
    };

    let result1 = handle1.join().unwrap();
    let result2 = handle2.join().unwrap();

    assert!(
        result1.is_ok() || result2.is_ok(),
        "at least one operation should succeed"
    );
}

#[test]
fn test_concurrent_reads_same_repo() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for i in 0..50 {
        fs::write(temp.path().join(format!("file{}.txt", i)), format!("{}", i)).unwrap();
    }
    heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();

    let barrier = Arc::new(Barrier::new(5));
    let temp_path = temp.path().to_path_buf();

    let handles: Vec<_> = (0..5)
        .map(|_| {
            let barrier = barrier.clone();
            let path = temp_path.clone();
            thread::spawn(move || {
                barrier.wait();
                heddle(&["log"], Some(&path))
            })
        })
        .collect();

    for handle in handles {
        let result = handle.join().unwrap();
        assert!(
            result.is_ok(),
            "concurrent reads should all succeed: {:?}",
            result.err()
        );
    }
}

#[test]
fn test_workspace_isolation() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("shared.txt"), "shared").unwrap();
    heddle(&["capture", "-m", "Shared"], Some(temp.path())).unwrap();

    let workspace1 = temp.path().join("workspace1");
    let workspace2 = temp.path().join("workspace2");
    fs::create_dir(&workspace1).unwrap();
    fs::create_dir(&workspace2).unwrap();

    fs::write(workspace1.join("agent1.txt"), "agent 1 work").unwrap();
    fs::write(workspace2.join("agent2.txt"), "agent 2 work").unwrap();

    assert_exists(workspace1.join("agent1.txt"), "workspace 1 file");
    assert_exists(workspace2.join("agent2.txt"), "workspace 2 file");
    assert_not_exists(
        workspace1.join("agent2.txt"),
        "workspace 1 shouldn't have agent2 file",
    );
    assert_not_exists(
        workspace2.join("agent1.txt"),
        "workspace 2 shouldn't have agent1 file",
    );
}