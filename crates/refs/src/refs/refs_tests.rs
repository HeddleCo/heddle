// SPDX-License-Identifier: Apache-2.0
use std::{sync::mpsc, time::Duration};

use objects::{
    error::HeddleError,
    object::{ChangeId, MarkerName, ThreadName},
};
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
            thread: ThreadName::new("main")
        }
    );
}
#[test]
fn test_head_roundtrip_attached() {
    let (_temp, refs) = create_ref_manager();
    let head = Head::Attached {
        thread: ThreadName::new("feature/auth"),
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
    refs.set_thread(&ThreadName::new("main"), &id).unwrap();
    let got = refs.get_thread(&ThreadName::new("main")).unwrap();
    assert_eq!(got, Some(id));
    let threads = refs.list_threads().unwrap();
    assert_eq!(threads, vec![ThreadName::new("main")]);
    let deleted = refs.delete_thread(&ThreadName::new("main")).unwrap();
    assert_eq!(deleted, Some(id));
    assert_eq!(refs.get_thread(&ThreadName::new("main")).unwrap(), None);
}
#[test]
fn test_nested_threads() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    refs.set_thread(&ThreadName::new("agent/claude/refactor"), &id)
        .unwrap();
    let got = refs
        .get_thread(&ThreadName::new("agent/claude/refactor"))
        .unwrap();
    assert_eq!(got, Some(id));
    let threads = refs.list_threads().unwrap();
    assert_eq!(threads, vec![ThreadName::new("agent/claude/refactor")]);
}

#[test]
fn test_parent_and_child_threads_can_coexist() {
    let (_temp, refs) = create_ref_manager();
    let parent = ChangeId::generate();
    let child = ChangeId::generate();

    refs.set_thread(&ThreadName::new("feature/orchestrator"), &parent)
        .unwrap();
    refs.set_thread(&ThreadName::new("feature/orchestrator/parser"), &child)
        .unwrap();

    assert_eq!(
        refs.get_thread(&ThreadName::new("feature/orchestrator"))
            .unwrap(),
        Some(parent)
    );
    assert_eq!(
        refs.get_thread(&ThreadName::new("feature/orchestrator/parser"))
            .unwrap(),
        Some(child)
    );

    let threads = refs.list_threads().unwrap();
    assert_eq!(
        threads,
        vec![
            ThreadName::new("feature/orchestrator"),
            ThreadName::new("feature/orchestrator/parser")
        ]
    );
}
#[test]
fn test_marker_operations() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    refs.create_marker(&MarkerName::new("v1.0.0"), &id).unwrap();
    let got = refs.get_marker(&MarkerName::new("v1.0.0")).unwrap();
    assert_eq!(got, Some(id));
    let markers = refs.list_markers().unwrap();
    assert_eq!(markers, vec![MarkerName::new("v1.0.0")]);
}
#[test]
fn test_marker_no_overwrite() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    refs.create_marker(&MarkerName::new("v1.0.0"), &id).unwrap();
    let result = refs.create_marker(&MarkerName::new("v1.0.0"), &id);
    assert!(result.is_err());
}
#[test]
fn test_resolve() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    refs.set_thread(&ThreadName::new("main"), &id).unwrap();
    refs.write_head(&Head::Attached {
        thread: ThreadName::new("main"),
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
fn test_corerefbackend_trait_methods_dispatch() {
    // The CLI calls RefManager's inherent (sync) methods; the generic
    // backend plumbing (Importer/RefEmitter, hosted server) calls the
    // async trait methods. Exercise the trait surface explicitly so the
    // `impl CoreRefBackend for RefManager` async bodies are covered.
    let (_temp, refs) = create_ref_manager();
    let thread_id = ChangeId::generate();
    let marker_id = ChangeId::generate();

    CoreRefBackend::set_thread(&refs, &ThreadName::new("main"), &thread_id).unwrap();
    assert_eq!(
        pollster::block_on(CoreRefBackend::get_thread(
            &refs,
            &ThreadName::new("main")
        ))
        .unwrap(),
        Some(thread_id)
    );

    pollster::block_on(CoreRefBackend::create_marker(
        &refs,
        &MarkerName::new("v1.0.0"),
        &marker_id,
    ))
    .unwrap();
    assert_eq!(
        pollster::block_on(CoreRefBackend::get_marker(
            &refs,
            &MarkerName::new("v1.0.0")
        ))
        .unwrap(),
        Some(marker_id)
    );

    refs.write_head(&Head::Attached {
        thread: ThreadName::new("main"),
    })
    .unwrap();
    assert_eq!(
        pollster::block_on(CoreRefBackend::resolve(&refs, "main")).unwrap(),
        Some(thread_id)
    );
    assert_eq!(
        pollster::block_on(CoreRefBackend::resolve(&refs, "@")).unwrap(),
        Some(thread_id)
    );
}
#[test]
fn test_track_cas_conflict() {
    let (_temp, refs) = create_ref_manager();
    let id1 = ChangeId::generate();
    let id2 = ChangeId::generate();
    refs.set_thread(&ThreadName::new("main"), &id1).unwrap();
    let result = refs.set_thread_cas(&ThreadName::new("main"), RefExpectation::Value(id2), &id2);
    assert!(matches!(result, Err(HeddleError::Conflict(_))));
    assert_eq!(
        refs.get_thread(&ThreadName::new("main")).unwrap(),
        Some(id1)
    );
}
#[test]
fn test_update_refs_transaction_success() {
    let (_temp, refs) = create_ref_manager();
    let id1 = ChangeId::generate();
    let id2 = ChangeId::generate();
    refs.set_thread(&ThreadName::new("main"), &id1).unwrap();
    refs.write_head(&Head::Attached {
        thread: ThreadName::new("main"),
    })
    .unwrap();
    let updates = vec![
        RefUpdate::Thread {
            name: ThreadName::new("main"),
            expected: RefExpectation::Value(id1),
            new: Some(id2),
        },
        RefUpdate::Head {
            expected: RefExpectation::Value(Head::Attached {
                thread: ThreadName::new("main"),
            }),
            new: Head::Detached { state: id2 },
        },
    ];
    refs.update_refs(&updates).unwrap();
    assert_eq!(
        refs.get_thread(&ThreadName::new("main")).unwrap(),
        Some(id2)
    );
    assert_eq!(refs.read_head().unwrap(), Head::Detached { state: id2 });
}
#[test]
fn test_update_refs_transaction_conflict() {
    let (_temp, refs) = create_ref_manager();
    let id1 = ChangeId::generate();
    let id2 = ChangeId::generate();
    refs.set_thread(&ThreadName::new("main"), &id1).unwrap();
    refs.write_head(&Head::Attached {
        thread: ThreadName::new("main"),
    })
    .unwrap();
    let updates = vec![
        RefUpdate::Thread {
            name: ThreadName::new("main"),
            expected: RefExpectation::Value(id2),
            new: Some(id2),
        },
        RefUpdate::Head {
            expected: RefExpectation::Value(Head::Attached {
                thread: ThreadName::new("main"),
            }),
            new: Head::Detached { state: id2 },
        },
    ];
    let result = refs.update_refs(&updates);
    assert!(matches!(result, Err(HeddleError::Conflict(_))));
    assert_eq!(
        refs.get_thread(&ThreadName::new("main")).unwrap(),
        Some(id1)
    );
    assert_eq!(
        refs.read_head().unwrap(),
        Head::Attached {
            thread: ThreadName::new("main")
        }
    );
}
#[test]
fn test_invalid_track_name_path_traversal() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    let result = refs.set_thread(&ThreadName::new("../etc/passwd"), &id);
    assert!(matches!(result, Err(HeddleError::InvalidRefName(_))));
}
#[test]
fn test_invalid_track_name_absolute_path() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    let result = refs.set_thread(&ThreadName::new("/etc/passwd"), &id);
    assert!(matches!(result, Err(HeddleError::InvalidRefName(_))));
}
#[test]
fn test_invalid_track_name_with_backslash() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    let result = refs.set_thread(&ThreadName::new("\\windows\\system32"), &id);
    assert!(matches!(result, Err(HeddleError::InvalidRefName(_))));
}
#[test]
fn test_invalid_marker_name_path_traversal() {
    let (_temp, refs) = create_ref_manager();
    let id = ChangeId::generate();
    let result = refs.create_marker(&MarkerName::new("../../../root"), &id);
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
        let result = refs.set_remote_thread("origin", &ThreadName::new("main"), &change_id);
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
    refs.set_remote_thread("origin", &ThreadName::new("main"), &change_id)
        .unwrap();
    let lock = refs.lock_refs().unwrap();
    let root = refs.root.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let refs = RefManager::new(root);
        let result = refs.delete_remote_thread("origin", &ThreadName::new("main"));
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

    refs.set_thread(&ThreadName::new("main"), &main).unwrap();
    refs.set_thread(&ThreadName::new("feature/api"), &feature)
        .unwrap();
    refs.create_marker(&MarkerName::new("v1.0.0"), &marker)
        .unwrap();
    refs.set_remote_thread("origin", &ThreadName::new("main"), &remote_main)
        .unwrap();
    refs.set_remote_thread("origin", &ThreadName::new("feature/api"), &remote_feature)
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
        vec![ThreadName::new("feature/api"), ThreadName::new("main")]
    );
    assert_eq!(
        refs.list_markers().unwrap(),
        vec![MarkerName::new("v1.0.0")]
    );
    assert_eq!(refs.list_remotes().unwrap(), vec!["origin".to_string()]);
    assert_eq!(
        refs.list_remote_threads("origin").unwrap(),
        vec![ThreadName::new("feature/api"), ThreadName::new("main")]
    );
}

#[test]
fn test_ref_summary_index_falls_back_when_sidecar_is_corrupt() {
    let (_temp, refs) = create_ref_manager();
    let main = ChangeId::generate();
    let marker = ChangeId::generate();
    let remote = ChangeId::generate();

    refs.set_thread(&ThreadName::new("main"), &main).unwrap();
    refs.create_marker(&MarkerName::new("stable"), &marker)
        .unwrap();
    refs.set_remote_thread("origin", &ThreadName::new("main"), &remote)
        .unwrap();
    refs.rebuild_ref_summary_index().unwrap();

    std::fs::write(refs.ref_summary_index_path(), "not a valid summary\n").unwrap();

    let inspection = refs.inspect_ref_summary_index().unwrap();
    assert!(inspection.present);
    assert!(!inspection.valid);
    assert!(inspection.error.is_some());

    assert_eq!(refs.list_threads().unwrap(), vec![ThreadName::new("main")]);
    assert_eq!(
        refs.list_markers().unwrap(),
        vec![MarkerName::new("stable")]
    );
    assert_eq!(refs.list_remotes().unwrap(), vec!["origin".to_string()]);
    assert_eq!(
        refs.list_remote_threads("origin").unwrap(),
        vec![ThreadName::new("main")]
    );
}

/// heddle#305 r2: the internal undo-recovery ref must live OUTSIDE the
/// user-writable marker namespace. It coexists with a user marker of the same
/// literal name, is invisible to `list_markers`, and a user `create_marker` /
/// `delete_marker` of that name never touches the internal pointer — and vice
/// versa. This closes the namespace-collision class structurally: the
/// `MarkerDelete` undo inverse (`create_marker(undo-recovery, Missing)`) can
/// never collide with recovery bookkeeping.
#[test]
fn undo_recovery_ref_is_isolated_from_user_marker_namespace() {
    let (_temp, refs) = create_ref_manager();
    let recovery_state = ChangeId::generate();
    let user_state = ChangeId::generate();
    assert_ne!(recovery_state, user_state);

    refs.set_undo_recovery(&recovery_state).unwrap();

    // The internal recovery ref is invisible to user marker enumeration and
    // unreadable through the marker API.
    assert!(
        refs.list_markers().unwrap().is_empty(),
        "the internal recovery ref must never appear as a user marker"
    );
    assert_eq!(
        refs.get_marker(&MarkerName::new(UNDO_RECOVERY_HANDLE)).unwrap(),
        None,
        "the internal recovery ref must not be readable as a user marker"
    );

    // A user may create a marker with the same literal name; the `Missing`
    // expectation succeeds because the internal ref does not occupy the
    // marker namespace. This is exactly the `MarkerDelete` undo-inverse shape
    // that previously risked colliding with the recovery write.
    refs.create_marker(&MarkerName::new(UNDO_RECOVERY_HANDLE), &user_state)
        .unwrap();
    // The two pointers are independent.
    assert_eq!(refs.get_undo_recovery().unwrap(), Some(recovery_state));
    assert_eq!(
        refs.get_marker(&MarkerName::new(UNDO_RECOVERY_HANDLE)).unwrap(),
        Some(user_state)
    );

    // Deleting the user marker leaves the internal recovery ref intact.
    refs.delete_marker(&MarkerName::new(UNDO_RECOVERY_HANDLE)).unwrap();
    assert_eq!(refs.get_undo_recovery().unwrap(), Some(recovery_state));

    // With no user marker, the handle resolves the internal recovery ref.
    assert_eq!(refs.resolve(UNDO_RECOVERY_HANDLE).unwrap(), Some(recovery_state));
}
