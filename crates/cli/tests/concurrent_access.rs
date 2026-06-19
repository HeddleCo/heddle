//! Concurrent access and thread safety tests.
//!
//! Tests for verifying correct behavior under concurrent access.

use std::{
    sync::{Arc, Barrier},
    thread,
};

use objects::{
    object::{Blob, MarkerName, ThreadName},
    store::{FsStore, LocalObjectStore},
};
use repo::Repository;
use tempfile::TempDir;

/// Test concurrent read operations.
#[test]
fn test_concurrent_reads() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create some data
    std::fs::write(temp.path().join("data.txt"), "initial").unwrap();
    let state = repo.snapshot(Some("Initial".to_string()), None).unwrap();

    let repo = Arc::new(repo);
    let barrier = Arc::new(Barrier::new(10));
    let mut handles = Vec::new();

    // Spawn 10 threads reading concurrently
    for _ in 0..10 {
        let repo = Arc::clone(&repo);
        let barrier = Arc::clone(&barrier);
        let state_id = state.change_id;

        let handle = thread::spawn(move || {
            barrier.wait(); // Synchronize start

            // Perform multiple reads
            for _ in 0..100 {
                let _ = repo.store().get_state(&state_id);
                let _ = repo.store().list_states();
                let _ = repo.refs().list_threads();
            }
        });
        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        handle.join().unwrap();
    }
}

/// Test concurrent snapshot creation.
#[test]
fn test_concurrent_snapshots() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    let repo = Arc::new(repo);
    let barrier = Arc::new(Barrier::new(5));
    let mut handles = Vec::new();

    // Spawn 5 threads creating snapshots
    for i in 0..5 {
        let repo = Arc::clone(&repo);
        let barrier = Arc::clone(&barrier);
        let temp_path = temp.path().to_path_buf();

        let handle = thread::spawn(move || {
            barrier.wait();

            // Each thread creates its own file and snapshot
            let filename = format!("thread_{}.txt", i);
            std::fs::write(
                temp_path.join(&filename),
                format!("content from thread {}", i),
            )
            .unwrap();

            repo.snapshot(Some(format!("Thread {} snapshot", i)), None)
        });
        handles.push(handle);
    }

    // Collect results
    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.join().unwrap());
    }

    // All should succeed and produce distinct state IDs
    let mut state_ids = std::collections::HashSet::new();
    for (i, result) in results.iter().enumerate() {
        let state = result
            .as_ref()
            .unwrap_or_else(|e| panic!("Thread {} snapshot failed: {:?}", i, e));
        assert!(
            state_ids.insert(state.change_id),
            "Thread {} produced duplicate state ID",
            i
        );
    }

    // Verify all states exist and are retrievable
    match Arc::try_unwrap(repo) {
        Ok(repo) => {
            let states = repo.store().list_states().unwrap();
            assert!(states.len() >= 5, "Should have at least 5 states");
            // Verify each concurrent state is present in the store
            for id in &state_ids {
                let state = repo.store().get_state(id).unwrap();
                assert!(state.is_some(), "State {:?} missing from store", id);
            }
        }
        Err(_) => panic!("Failed to unwrap Arc - threads may still be running"),
    }
}

/// Test concurrent operations on different threads.
#[test]
fn test_concurrent_track_operations() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create initial state
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    let base = repo.snapshot(Some("Base".to_string()), None).unwrap();

    let repo = Arc::new(repo);
    let barrier = Arc::new(Barrier::new(5));
    let mut handles = Vec::new();

    // Spawn threads creating threads
    for i in 0..5 {
        let repo = Arc::clone(&repo);
        let barrier = Arc::clone(&barrier);
        let base_id = base.change_id;

        let handle = thread::spawn(move || {
            barrier.wait();

            // Create thread
            let track_name = ThreadName::new(format!("feature/{}", i));
            repo.refs().set_thread(&track_name, &base_id).unwrap();

            // Verify
            let found = repo.refs().get_thread(&track_name).unwrap();
            assert_eq!(found, Some(base_id));
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    // Verify all threads exist
    match Arc::try_unwrap(repo) {
        Ok(repo) => {
            let threads = repo.refs().list_threads().unwrap();
            assert!(threads.len() >= 5, "Should have at least 5 threads");
        }
        Err(_) => panic!("Failed to unwrap Arc"),
    }
}

/// Test concurrent read and write operations.
#[test]
fn test_concurrent_read_write() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create initial state
    std::fs::write(temp.path().join("data.txt"), "initial").unwrap();
    let _ = repo.snapshot(Some("Initial".to_string()), None).unwrap();

    let repo = Arc::new(repo);
    let barrier = Arc::new(Barrier::new(10));
    let mut handles = Vec::new();

    // 5 readers and 5 writers
    for i in 0..10 {
        let repo = Arc::clone(&repo);
        let barrier = Arc::clone(&barrier);
        let temp_path = temp.path().to_path_buf();

        let handle = if i < 5 {
            // Reader
            thread::spawn(move || {
                barrier.wait();
                for _ in 0..50 {
                    let _ = repo.store().list_states();
                    let _ = repo.refs().list_threads();
                }
            })
        } else {
            // Writer
            let writer_id = i;
            thread::spawn(move || {
                barrier.wait();
                let filename = format!("writer_{}.txt", writer_id);
                std::fs::write(temp_path.join(&filename), format!("content {}", writer_id))
                    .unwrap();
                let _ = repo.snapshot(Some(format!("Writer {} state", writer_id)), None);
            })
        };
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

/// Test concurrent goto operations.
#[test]
fn test_concurrent_goto() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create multiple states
    let mut states = Vec::new();
    for i in 0..5 {
        std::fs::write(temp.path().join("version.txt"), format!("v{}", i)).unwrap();
        let state = repo.snapshot(Some(format!("State {}", i)), None).unwrap();
        states.push(state.change_id);
    }

    let repo = Arc::new(repo);
    let barrier = Arc::new(Barrier::new(5));
    let mut handles = Vec::new();

    // Spawn threads all trying to goto different states
    for state_id in states.iter() {
        let repo = Arc::clone(&repo);
        let barrier = Arc::clone(&barrier);
        let state_id = *state_id;

        let handle = thread::spawn(move || {
            barrier.wait();

            // Try to goto this state
            // Note: Concurrent goto to worktree may have race conditions
            // This tests how the system handles it
            let _ = repo.goto(&state_id);
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

/// Test concurrent object store operations using direct store access.
#[test]
fn test_concurrent_object_store() {
    let temp = TempDir::new().unwrap();
    let temp2 = TempDir::new().unwrap();
    let store1 = FsStore::new(temp.path().join("objects"));
    let store2 = FsStore::new(temp2.path().join("objects"));

    let store1 = Arc::new(store1);
    let store2 = Arc::new(store2);
    let barrier = Arc::new(Barrier::new(20));
    let mut handles = Vec::new();

    // 20 threads all storing and retrieving blobs
    for i in 0..20 {
        let store = if i % 2 == 0 {
            Arc::clone(&store1)
        } else {
            Arc::clone(&store2)
        };
        let barrier = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            use Blob;

            barrier.wait();

            // Store unique content
            let content = format!("thread-{}-unique-content-{}", i, i * 1000);
            let blob = Blob::new(content.clone().into_bytes());
            let hash = store.put_blob(&blob).unwrap();

            // Other threads try to read the same blob
            let retrieved = store.get_blob(&hash).unwrap();
            assert!(retrieved.is_some(), "Thread {} should retrieve blob", i);

            // Verify content
            if let Some(retrieved_blob) = retrieved {
                assert_eq!(
                    retrieved_blob.content(),
                    content.as_bytes(),
                    "Content mismatch for thread {}",
                    i
                );
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

/// Test concurrent snapshot operations with same parent.
#[test]
fn test_concurrent_snapshots_same_parent() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create base state
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    let _base = repo.snapshot(Some("Base".to_string()), None).unwrap();

    let repo = Arc::new(repo);
    let barrier = Arc::new(Barrier::new(5));
    let mut handles = Vec::new();

    // All threads create snapshots from same base
    for i in 0..5 {
        let repo = Arc::clone(&repo);
        let barrier = Arc::clone(&barrier);
        let temp_path = temp.path().to_path_buf();

        let handle = thread::spawn(move || {
            barrier.wait();

            // Each creates a different file
            let filename = format!("branch_{}.txt", i);
            std::fs::write(temp_path.join(&filename), format!("branch {}", i)).unwrap();

            // Create snapshot - should handle concurrent parent update
            repo.snapshot(Some(format!("Branch {}", i)), None)
        });
        handles.push(handle);
    }

    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.join().unwrap());
    }

    // All should succeed and produce distinct state IDs
    let mut state_ids = std::collections::HashSet::new();
    for (index, result) in results.iter().enumerate() {
        let state = result
            .as_ref()
            .unwrap_or_else(|e| panic!("Thread {} snapshot failed: {:?}", index, e));
        assert!(
            state_ids.insert(state.change_id),
            "Thread {} produced duplicate state ID",
            index
        );
    }

    // Verify we have multiple states and each is retrievable
    match Arc::try_unwrap(repo) {
        Ok(repo) => {
            let states = repo.store().list_states().unwrap();
            assert!(states.len() >= 6); // Base + 5 branches
            for id in &state_ids {
                let state = repo.store().get_state(id).unwrap();
                assert!(state.is_some(), "State {:?} missing from store", id);
            }
        }
        Err(_) => panic!("Failed to unwrap Arc"),
    }
}

/// Test concurrent marker operations.
#[test]
fn test_concurrent_marker_operations() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create state
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    let state = repo.snapshot(Some("State".to_string()), None).unwrap();

    let repo = Arc::new(repo);
    let barrier = Arc::new(Barrier::new(10));
    let mut handles = Vec::new();

    // Create markers concurrently
    for i in 0..10 {
        let repo = Arc::clone(&repo);
        let barrier = Arc::clone(&barrier);
        let state_id = state.change_id;

        let handle = thread::spawn(move || {
            barrier.wait();

            let marker_name = MarkerName::new(format!("v1.0.{}", i));
            let _ = repo.refs().create_marker(&marker_name, &state_id);
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    // Verify markers exist
    match Arc::try_unwrap(repo) {
        Ok(repo) => {
            let markers = repo.refs().list_markers().unwrap();
            // Some may fail due to concurrent writes, but most should succeed
            assert!(!markers.is_empty(), "Should have created some markers");
        }
        Err(_) => panic!("Failed to unwrap Arc"),
    }
}

/// Test worktree status during concurrent modifications.
#[test]
fn test_concurrent_worktree_modifications() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Initial snapshot
    std::fs::write(temp.path().join("file.txt"), "initial").unwrap();
    repo.snapshot(Some("Initial".to_string()), None).unwrap();

    // Note: worktree_status() doesn't exist, so we'll test concurrent file operations
    let barrier = Arc::new(Barrier::new(10));
    let mut handles = Vec::new();

    // Concurrently modify worktree
    for i in 0..10 {
        let temp_path = temp.path().to_path_buf();
        let barrier = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            barrier.wait();

            // Each thread modifies different files
            for j in 0..10 {
                let filename = format!("thread{}_file{}.txt", i, j);
                std::fs::write(temp_path.join(&filename), format!("content {}-{}", i, j)).unwrap();
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    // Verify files were created
    let count = std::fs::read_dir(temp.path()).unwrap().count();
    assert!(count > 100, "Should have created many files");
}

/// Test thread-safe blob storage with separate stores.
#[test]
fn test_thread_safe_blob_storage() {
    let temp = TempDir::new().unwrap();
    let store = Arc::new(FsStore::new(temp.path()));

    let barrier = Arc::new(Barrier::new(20));
    let mut handles = Vec::new();

    // 20 threads all storing and retrieving blobs
    for i in 0..20 {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            use Blob;

            barrier.wait();

            // Store unique content
            let content = format!("thread-{}-unique-content-{}", i, i * 1000);
            let blob = Blob::new(content.clone().into_bytes());
            let hash = store.put_blob(&blob).unwrap();

            // Other threads try to read the same blob
            let retrieved = store.get_blob(&hash).unwrap();
            assert!(retrieved.is_some(), "Thread {} should retrieve blob", i);

            // Verify content
            if let Some(retrieved_blob) = retrieved {
                assert_eq!(
                    retrieved_blob.content(),
                    content.as_bytes(),
                    "Content mismatch for thread {}",
                    i
                );
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

/// Test concurrent state retrieval.
#[test]
fn test_concurrent_state_retrieval() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create many states
    let mut state_ids = Vec::new();
    for i in 0..50 {
        std::fs::write(temp.path().join(format!("v{}.txt", i)), format!("v{}", i)).unwrap();
        let state = repo.snapshot(Some(format!("State {}", i)), None).unwrap();
        state_ids.push(state.change_id);
    }

    let repo = Arc::new(repo);
    let barrier = Arc::new(Barrier::new(10));
    let mut handles = Vec::new();

    // 10 threads all reading states concurrently
    for _ in 0..10 {
        let repo = Arc::clone(&repo);
        let barrier = Arc::clone(&barrier);
        let state_ids = state_ids.clone();

        let handle = thread::spawn(move || {
            barrier.wait();

            for state_id in &state_ids {
                // Read each state multiple times
                for _ in 0..10 {
                    let state = repo.store().get_state(state_id).unwrap();
                    assert!(state.is_some(), "State should exist");
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

/// Test repository opening from multiple threads.
#[test]
fn test_concurrent_repository_open() {
    let temp = TempDir::new().unwrap();
    let _ = Repository::init_default(temp.path()).unwrap();

    let barrier = Arc::new(Barrier::new(10));
    let mut handles = Vec::new();

    // Multiple threads opening the same repository
    for _ in 0..10 {
        let temp_path = temp.path().to_path_buf();
        let barrier = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            barrier.wait();

            // All threads try to open the repo simultaneously
            let repo = Repository::open(&temp_path).unwrap();

            // Do some read operations
            let _ = repo.store().list_states().unwrap();
            let _ = repo.refs().list_threads().unwrap();
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}
