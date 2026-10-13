// SPDX-License-Identifier: Apache-2.0
//! Run in a killable child: a worker deadlock must fail even under cargo test.

use std::{
    process::Command,
    thread,
    time::{Duration, Instant},
};

use objects::{
    lock::RepositoryLockExt,
    object::{Blob, Principal, Redaction, StateId, Tree, TreeEntry},
    store::ObjectStore,
};
use repo::Repository;

fn materialize(threads: &str, test: &str) {
    if std::env::var("HEDDLE_MATERIALIZATION_CHILD").as_deref() == Ok(test) {
        let directory = tempfile::tempdir().expect("repository");
        let repository = Repository::init_default(directory.path()).expect("init");
        let mut tree = Tree::new();
        for index in 0..64 {
            let blob = Blob::new(format!("file {index}\n").into_bytes());
            let hash = repository.store().put_blob(&blob).expect("blob");
            tree.insert(TreeEntry::file(format!("f{index}.txt"), hash, index == 0).expect("entry"));
        }
        // Import and thread operations hold this lock while joining workers.
        let _guard = repository.locker().write().expect("caller lock");
        for _ in 0..2 {
            repository
                .materialize_computed_tree(&tree, directory.path())
                .expect("materialize");
            for index in 0..64 {
                assert_eq!(
                    std::fs::read_to_string(directory.path().join(format!("f{index}.txt")))
                        .expect("file"),
                    format!("file {index}\n")
                );
            }
        }
        let hash = Blob::from_slice(b"file 0\n").hash();
        repository
            .put_redaction(Redaction {
                redacted_blob: hash,
                state: StateId::from_bytes([1; 32]),
                path: "f0.txt".into(),
                reason: "parallel materialization redaction".into(),
                redactor: Principal::new("Tester", "tester@example.test"),
                redacted_at: chrono::Utc::now(),
                signature: None,
                purge: None,
                supersedes: None,
            })
            .expect("redaction");
        repository
            .materialize_computed_tree(&tree, directory.path())
            .expect("materialize redaction");
        let stub = std::fs::read_to_string(directory.path().join("f0.txt")).expect("stub");
        assert!(stub.contains("parallel materialization redaction"));
        assert!(!stub.contains("file 0\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(directory.path().join("f0.txt"))
                .expect("stub metadata")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o111,
                0,
                "redacted executable must lose executable bits"
            );
        }
        return;
    }

    let home = tempfile::tempdir().expect("fresh home");
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", test, "--nocapture"])
        .env("HEDDLE_MATERIALIZATION_CHILD", test)
        .env("HEDDLE_MATERIALIZE_THREADS", threads)
        .env("HEDDLE_HOME", home.path())
        .spawn()
        .expect("child");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            assert!(status.success(), "{threads} workers: {status}");
            return;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill deadlocked child");
            child.wait().expect("reap child");
            panic!("64-file materialization with {threads} workers timed out after 30s");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn materialization_2_workers() {
    materialize("2", "materialization_2_workers");
}

#[test]
fn materialization_4_workers() {
    materialize("4", "materialization_4_workers");
}

#[test]
fn materialization_8_workers() {
    materialize("8", "materialization_8_workers");
}
