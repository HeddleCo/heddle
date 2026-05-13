// SPDX-License-Identifier: Apache-2.0
use std::{sync::mpsc, time::Duration};

use objects::{error::HeddleError, object::ChangeId};
use tempfile::TempDir;

use super::*;

fn create_ref_manager() -> (TempDir, RefManager) {
    let temp_dir = TempDir::new().unwrap();
    let heddle_dir = temp_dir.path().join(".heddle");
    std::fs::create_dir_all(&heddle_dir).unwrap();
    let refs = RefManager::new(&heddle_dir);
    refs.init().unwrap();
    (temp_dir, refs)
}

#[test]
fn test_head_default() {
    let (_temp, refs) = create_ref_manager();
    let head = refs.read_head().unwrap();
    assert_eq!(
        head,
        Head::Attached {
            thread: "main".to_string()
        }
    );
}
#[test]
fn test_head_roundtrip_attached() {
    let (_temp, refs) = create_ref_manager();
    let head = Head::Attached {
        thread: "feature/auth".to_string(),
    };
    refs.write_head(&head).unwrap();
    let read = refs.read_head().unwrap();
    assert_eq!(read, head);
}
#[test]
fn test_head_roundtrip_detached() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    let head = Head::Detached { state: id };
    refs.write_head(&head).unwrap();
    let read = refs.read_head().unwrap();
    assert_eq!(read, head);
}
#[test]
fn test_track_operations() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    refs.set_thread("main", &id).unwrap();
    let got = refs.get_thread("main").unwrap();
    assert_eq!(got, Some(id));
    let threads = refs.list_threads().unwrap();
    assert_eq!(threads, vec!["main"]);
    let deleted = refs.delete_thread("main").unwrap();
    assert_eq!(deleted, Some(id));
    assert_eq!(refs.get_thread("main").unwrap(), None);
}
#[test]
fn test_nested_threads() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    refs.set_thread("agent/claude/refactor", &id).unwrap();
    let got = refs.get_thread("agent/claude/refactor").unwrap();
    assert_eq!(got, Some(id));
    let threads = refs.list_threads().unwrap();
    assert_eq!(threads, vec!["agent/claude/refactor"]);
}

#[test]
fn test_parent_and_child_threads_can_coexist() {
    let (_temp, refs) = create_ref_manager();
    let parent = ChangeId::generate();
    let child = ChangeId::generate();

    refs.set_thread("feature/orchestrator", &parent).unwrap();
    refs.set_thread("feature/orchestrator/parser", &child)
        .unwrap();

    assert_eq!(
        refs.get_thread("feature/orchestrator").unwrap(),
        Some(parent)
    );
    assert_eq!(
        refs.get_thread("feature/orchestrator/parser").unwrap(),
        Some(child)
    );

    let threads = refs.list_threads().unwrap();
    assert_eq!(
        threads,
        vec![
            "feature/orchestrator".to_string(),
            "feature/orchestrator/parser".to_string()
        ]
    );
}
#[test]
fn test_marker_operations() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    refs.create_marker("v1.0.0", &id).unwrap();
    let got = refs.get_marker("v1.0.0").unwrap();
    assert_eq!(got, Some(id));
    let markers = refs.list_markers().unwrap();
    assert_eq!(markers, vec!["v1.0.0"]);
}
#[test]
fn test_marker_no_overwrite() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    refs.create_marker("v1.0.0", &id).unwrap();
    let result = refs.create_marker("v1.0.0", &id);
    assert!(result.is_err());
}
#[test]
fn test_resolve() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    refs.set_thread("main", &id).unwrap();
    refs.write_head(&Head::Attached {
        thread: "main".to_string(),
    })
    .unwrap();
    let resolved = refs.resolve("main").unwrap();
    assert_eq!(resolved, Some(id));
    let resolved = refs.resolve("@").unwrap();
    assert_eq!(resolved, Some(id));
    let resolved = refs.resolve(&id.to_string_full()).unwrap();
    assert_eq!(resolved, Some(id));
}
#[test]
fn test_track_cas_conflict() {
    let (_temp, refs) = create_ref_manager();
    let id1 = ChangeId::generate();
    let id2 = ChangeId::generate();
    refs.set_thread("main", &id1).unwrap();
    let result = refs.set_thread_cas("main", RefExpectation::Value(id2), &id2);
    assert!(matches!(result, Err(HeddleError::Conflict(_))));
    assert_eq!(refs.get_thread("main").unwrap(), Some(id1));
}
#[test]
fn test_update_refs_transaction_success() {
    let (_temp, refs) = create_ref_manager();
    let id1 = ChangeId::generate();
    let id2 = ChangeId::generate();
    refs.set_thread("main", &id1).unwrap();
    refs.write_head(&Head::Attached {
        thread: "main".to_string(),
    })
    .unwrap();
    let updates = vec![
        RefUpdate::Thread {
            name: "main".to_string(),
            expected: RefExpectation::Value(id1),
            new: Some(id2),
        },
        RefUpdate::Head {
            expected: RefExpectation::Value(Head::Attached {
                thread: "main".to_string(),
            }),
            new: Head::Detached { state: id2 },
        },
    ];
    refs.update_refs(&updates).unwrap();
    assert_eq!(refs.get_thread("main").unwrap(), Some(id2));
    assert_eq!(refs.read_head().unwrap(), Head::Detached { state: id2 });
}
#[test]
fn test_update_refs_transaction_conflict() {
    let (_temp, refs) = create_ref_manager();
    let id1 = ChangeId::generate();
    let id2 = ChangeId::generate();
    refs.set_thread("main", &id1).unwrap();
    refs.write_head(&Head::Attached {
        thread: "main".to_string(),
    })
    .unwrap();
    let updates = vec![
        RefUpdate::Thread {
            name: "main".to_string(),
            expected: RefExpectation::Value(id2),
            new: Some(id2),
        },
        RefUpdate::Head {
            expected: RefExpectation::Value(Head::Attached {
                thread: "main".to_string(),
            }),
            new: Head::Detached { state: id2 },
        },
    ];
    let result = refs.update_refs(&updates);
    assert!(matches!(result, Err(HeddleError::Conflict(_))));
    assert_eq!(refs.get_thread("main").unwrap(), Some(id1));
    assert_eq!(
        refs.read_head().unwrap(),
        Head::Attached {
            thread: "main".to_string()
        }
    );
}
#[test]
fn test_invalid_track_name_path_traversal() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    let result = refs.set_thread("../etc/passwd", &id);
    assert!(matches!(result, Err(HeddleError::InvalidRefName(_))));
}
#[test]
fn test_invalid_track_name_absolute_path() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    let result = refs.set_thread("/etc/passwd", &id);
    assert!(matches!(result, Err(HeddleError::InvalidRefName(_))));
}
#[test]
fn test_invalid_track_name_with_backslash() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    let result = refs.set_thread("\\windows\\system32", &id);
    assert!(matches!(result, Err(HeddleError::InvalidRefName(_))));
}
#[test]
fn test_invalid_marker_name_path_traversal() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    let result = refs.create_marker("../../../root", &id);
    assert!(matches!(result, Err(HeddleError::InvalidRefName(_))));
}
#[test]
fn test_set_remote_thread_waits_for_refs_lock() {
    let (_temp, refs) = create_ref_manager();
    let lock = refs.lock_refs().unwrap();
    let root = refs.root.clone();
    let change_id = ChangeId::generate();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let refs = RefManager::new(root);
        let result = refs.set_remote_thread("origin", "main", &change_id);
        tx.send(result).unwrap();
    });
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    drop(lock);
    let result = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(result.is_ok());
}
#[test]
fn test_delete_remote_thread_waits_for_refs_lock() {
    let (_temp, refs) = create_ref_manager();
    let change_id = ChangeId::generate();
    refs.set_remote_thread("origin", "main", &change_id)
        .unwrap();
    let lock = refs.lock_refs().unwrap();
    let root = refs.root.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let refs = RefManager::new(root);
        let result = refs.delete_remote_thread("origin", "main");
        tx.send(result).unwrap();
    });
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    drop(lock);
    let result = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(result.unwrap(), Some(change_id));
}

#[test]
fn test_ref_summary_index_rebuild_reports_repo_ref_shape() {
    let (_temp, refs) = create_ref_manager();
    let main = ChangeId::generate();
    let feature = ChangeId::generate();
    let marker = ChangeId::generate();
    let remote_main = ChangeId::generate();
    let remote_feature = ChangeId::generate();

    refs.set_thread("main", &main).unwrap();
    refs.set_thread("feature/api", &feature).unwrap();
    refs.create_marker("v1.0.0", &marker).unwrap();
    refs.set_remote_thread("origin", "main", &remote_main)
        .unwrap();
    refs.set_remote_thread("origin", "feature/api", &remote_feature)
        .unwrap();

    let before = refs.inspect_ref_summary_index().unwrap();
    assert!(before.present);
    assert!(before.valid);
    assert_eq!(before.threads, 2);
    assert_eq!(before.markers, 1);
    assert_eq!(before.remotes, 1);
    assert_eq!(before.remote_threads, 2);

    let rebuilt = refs.rebuild_ref_summary_index().unwrap();
    assert!(rebuilt.present);
    assert!(rebuilt.valid);
    assert_eq!(rebuilt.threads, 2);
    assert_eq!(rebuilt.markers, 1);
    assert_eq!(rebuilt.remotes, 1);
    assert_eq!(rebuilt.remote_threads, 2);
    assert!(rebuilt.bytes > 0);

    let after = refs.inspect_ref_summary_index().unwrap();
    assert!(after.present);
    assert!(after.valid);
    assert_eq!(after.threads, 2);
    assert_eq!(after.markers, 1);
    assert_eq!(after.remotes, 1);
    assert_eq!(after.remote_threads, 2);

    assert_eq!(
        refs.list_threads().unwrap(),
        vec!["feature/api".to_string(), "main".to_string()]
    );
    assert_eq!(refs.list_markers().unwrap(), vec!["v1.0.0".to_string()]);
    assert_eq!(refs.list_remotes().unwrap(), vec!["origin".to_string()]);
    assert_eq!(
        refs.list_remote_threads("origin").unwrap(),
        vec!["feature/api".to_string(), "main".to_string()]
    );
}

#[test]
fn test_ref_summary_index_falls_back_when_sidecar_is_corrupt() {
    let (_temp, refs) = create_ref_manager();
    let main = ChangeId::generate();
    let marker = ChangeId::generate();
    let remote = ChangeId::generate();

    refs.set_thread("main", &main).unwrap();
    refs.create_marker("stable", &marker).unwrap();
    refs.set_remote_thread("origin", "main", &remote).unwrap();
    refs.rebuild_ref_summary_index().unwrap();

    std::fs::write(refs.ref_summary_index_path(), "not a valid summary\n").unwrap();

    let inspection = refs.inspect_ref_summary_index().unwrap();
    assert!(inspection.present);
    assert!(!inspection.valid);
    assert!(inspection.error.is_some());

    assert_eq!(refs.list_threads().unwrap(), vec!["main".to_string()]);
    assert_eq!(refs.list_markers().unwrap(), vec!["stable".to_string()]);
    assert_eq!(refs.list_remotes().unwrap(), vec!["origin".to_string()]);
    assert_eq!(
        refs.list_remote_threads("origin").unwrap(),
        vec!["main".to_string()]
    );
}