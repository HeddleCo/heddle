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

/// heddle#305 r3: the undo-recovery handle is UNSHADOWABLE by user refs in BOTH
/// directions. The internal pointer lives outside the user-writable namespace
/// (write side), AND the reserved `.`-prefixed handle resolves to it before any
/// user thread/marker (resolve side). This closes the shadowing class
/// structurally rather than relying on a same-named user ref losing a
/// resolution race.
#[test]
fn undo_recovery_ref_is_isolated_from_user_marker_namespace() {
    let (_temp, refs) = create_ref_manager();
    let recovery_state = ChangeId::generate();
    let user_state = ChangeId::generate();
    assert_ne!(recovery_state, user_state);

    refs.set_undo_recovery(&recovery_state).unwrap();

    // The internal recovery ref is invisible to user marker enumeration.
    assert!(
        refs.list_markers().unwrap().is_empty(),
        "the internal recovery ref must never appear as a user marker"
    );

    // WRITE side: the reserved handle cannot be created as a user marker — the
    // leading `.` is rejected by ref-name validation — so no user ref can ever
    // occupy the recovery namespace.
    assert!(
        matches!(
            refs.create_marker(&MarkerName::new(UNDO_RECOVERY_HANDLE), &user_state),
            Err(HeddleError::InvalidRefName(_))
        ),
        "a user must not be able to create a marker with the reserved recovery handle"
    );

    // RESOLVE side: the reserved handle always resolves to the internal pointer.
    assert_eq!(
        refs.resolve(UNDO_RECOVERY_HANDLE).unwrap(),
        Some(recovery_state)
    );

    // A user marker with the BARE name (no leading dot) is a separate, legal
    // ref. It coexists with — and never intercepts — the reserved handle.
    let bare = "undo-recovery";
    refs.create_marker(&MarkerName::new(bare), &user_state)
        .unwrap();
    assert_eq!(
        refs.resolve(bare).unwrap(),
        Some(user_state),
        "the bare user marker resolves to the user's ref"
    );
    assert_eq!(
        refs.resolve(UNDO_RECOVERY_HANDLE).unwrap(),
        Some(recovery_state),
        "the reserved handle still resolves to the internal recovery ref"
    );
}

/// heddle#305 r4: the undo-recovery pointer is scoped to the LOCAL checkout
/// (per-worktree HEAD), exactly like the undo/redo history it recovers
/// (`op_scope`), NOT to the shared ref root. In objectstore-pointer worktrees
/// `Repository::open` builds refs as
/// `RefManager::new(&shared_galeed_dir).with_local_head(<worktree>/.heddle/HEAD)` —
/// the ref root is shared across sibling checkouts but `local_head` is unique.
/// A `heddle undo` in checkout B must NOT overwrite checkout A's recovery
/// pointer; otherwise `goto .undo-recovery` in A would restore B's pre-undo
/// state — cross-checkout data corruption.
#[test]
fn undo_recovery_is_scoped_per_checkout_on_shared_ref_root() {
    let temp = TempDir::new().unwrap();
    let shared_root = temp.path().join("objectstore");
    std::fs::create_dir_all(&shared_root).unwrap();

    // Two materialized checkouts sharing one object store / ref root, each
    // with its own per-worktree HEAD (mirrors the `Repository::open` wiring).
    let head_a = temp.path().join("wt-a").join(".heddle").join("HEAD");
    let head_b = temp.path().join("wt-b").join(".heddle").join("HEAD");
    std::fs::create_dir_all(head_a.parent().unwrap()).unwrap();
    std::fs::create_dir_all(head_b.parent().unwrap()).unwrap();

    let refs_a = RefManager::new(&shared_root).with_local_head(head_a);
    let refs_b = RefManager::new(&shared_root).with_local_head(head_b);

    let state_a = ChangeId::generate();
    let state_b = ChangeId::generate();
    assert_ne!(state_a, state_b);

    // Both checkouts run `heddle undo`, each recording its own pre-undo state.
    refs_a.set_undo_recovery(&state_a).unwrap();
    refs_b.set_undo_recovery(&state_b).unwrap();

    // B's undo must not have clobbered A's recovery pointer (write side).
    assert_eq!(
        refs_a.get_undo_recovery().unwrap(),
        Some(state_a),
        "checkout A's recovery pointer must reflect A's own pre-undo state"
    );
    assert_eq!(
        refs_b.get_undo_recovery().unwrap(),
        Some(state_b),
        "checkout B's recovery pointer must reflect B's own pre-undo state"
    );

    // The advertised reserved handle in each checkout resolves to THAT
    // checkout's recovery pointer, not the sibling's (resolve side).
    assert_eq!(refs_a.resolve(UNDO_RECOVERY_HANDLE).unwrap(), Some(state_a));
    assert_eq!(refs_b.resolve(UNDO_RECOVERY_HANDLE).unwrap(), Some(state_b));
}

// ---- Read/write chokepoint coverage (heddle#330 §2.2) ----
//
// These drive `RefManager`'s reconciler/committer seams directly via injected
// test doubles, exercising `reconciled_load`'s hot path + lag path,
// `materialize`'s fill-if-absent branches, and `commit_and_publish`'s
// record-before-publish ordering — without the `repo`/`oplog` layer.

mod chokepoint {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use objects::error::Result;
    use objects::object::{ChangeId, MarkerName, ThreadName};
    use tempfile::TempDir;

    use super::super::{
        Loaded, LoadRequest, RefCommitter, RefManager, RefReconciler, ReconcileOutcome,
        RefExpectation, RefUpdate,
    };

    fn manager() -> (TempDir, std::path::PathBuf) {
        let temp = TempDir::new().unwrap();
        let heddle_dir = temp.path().join(".heddle");
        std::fs::create_dir_all(&heddle_dir).unwrap();
        let refs = RefManager::new(&heddle_dir);
        refs.init().unwrap();
        (temp, heddle_dir)
    }

    /// A reconciler whose generation is bumpable after injection (so the
    /// watermark, seeded at inject time, lags and the reconcile path runs), and
    /// whose `reconcile` returns a caller-configured materialization set.
    struct FakeReconciler {
        generation: AtomicU64,
        republish: Vec<RefUpdate>,
        remote_updates: Vec<(String, ThreadName, Option<ChangeId>)>,
        undo_recovery: Option<ChangeId>,
        calls: Arc<AtomicU64>,
    }

    impl RefReconciler for FakeReconciler {
        fn generation(&self) -> u64 {
            self.generation.load(Ordering::Acquire)
        }
        fn reconcile(&self, req: &LoadRequest, raw: Loaded, _since: u64) -> Result<ReconcileOutcome> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            // Project the request's authoritative value out of the configured
            // materialization set (mirrors the real reconciler's per-request fold).
            let loaded = match req {
                LoadRequest::Thread(name) => self
                    .republish
                    .iter()
                    .find_map(|u| match u {
                        RefUpdate::Thread { name: n, new, .. } if n == name => Some(Loaded::Point(*new)),
                        _ => None,
                    })
                    .unwrap_or(raw),
                LoadRequest::Marker(name) => self
                    .republish
                    .iter()
                    .find_map(|u| match u {
                        RefUpdate::Marker { name: n, new, .. } if n == name => Some(Loaded::Point(*new)),
                        _ => None,
                    })
                    .unwrap_or(raw),
                LoadRequest::UndoRecovery => Loaded::Point(self.undo_recovery),
                LoadRequest::RemoteThread { remote, thread } => self
                    .remote_updates
                    .iter()
                    .find_map(|(r, t, v)| (r == remote && t == thread).then_some(Loaded::Point(*v)))
                    .unwrap_or(raw),
                _ => raw,
            };
            Ok(ReconcileOutcome {
                loaded,
                republish: self.republish.clone(),
                remote_updates: self.remote_updates.clone(),
                undo_recovery: self.undo_recovery,
            })
        }
    }

    type CommitCall = (Vec<Vec<u8>>, Option<String>);

    struct FakeCommitter {
        seen: Mutex<Vec<CommitCall>>,
    }

    impl RefCommitter for FakeCommitter {
        fn commit_records(&self, encoded_records: &[Vec<u8>], scope: Option<&str>) -> Result<()> {
            self.seen
                .lock()
                .unwrap()
                .push((encoded_records.to_vec(), scope.map(str::to_string)));
            Ok(())
        }
    }

    /// The hot path: when the class watermark equals the reconciler generation,
    /// the raw value is returned with no reconcile call (no tail scan, no write).
    #[test]
    fn reconciled_load_hot_path_skips_reconcile_when_generation_unchanged() {
        let (_t, dir) = manager();
        let calls = Arc::new(AtomicU64::new(0));
        let reconciler = Arc::new(FakeReconciler {
            generation: AtomicU64::new(7),
            republish: Vec::new(),
            remote_updates: Vec::new(),
            undo_recovery: None,
            calls: Arc::clone(&calls),
        });
        // `with_reconciler` seeds both watermarks to generation()==7.
        let refs = RefManager::new(&dir).with_reconciler(reconciler);
        refs.init().unwrap();

        // Generation still 7 ⇒ tip == cached ⇒ reconcile is never invoked.
        let got = refs.get_thread(&ThreadName::new("absent")).unwrap();
        assert_eq!(got, None);
        assert_eq!(calls.load(Ordering::Acquire), 0, "hot path must not reconcile");
    }

    /// The lag path drives `materialize`'s authoritative-apply branches: an
    /// absent thread + marker + remote-thread + undo-recovery are all published
    /// (create), a present-but-STALE thread is overwritten with the committed
    /// value (the cid 3329490981 update-to-existing case), a present thread whose
    /// committed value equals the canonical is a no-op skip, and a `None` remote
    /// update + the watermark advance are exercised. A second read then takes the
    /// hot path because the watermark caught up to the (unchanged) generation.
    #[test]
    fn reconciled_load_lag_path_materializes_committed_values() {
        let (_t, dir) = manager();

        // `present` is pre-published with the same value the fold carries ⇒ skip.
        let present_state = ChangeId::generate();
        // `stale` is pre-published with an OLD value; the fold carries a newer
        // committed value ⇒ it must be overwritten (authoritative-apply).
        let stale_old = ChangeId::generate();
        let stale_new = ChangeId::generate();
        let absent_state = ChangeId::generate();
        let marker_state = ChangeId::generate();
        let remote_state = ChangeId::generate();
        // A present-but-stale remote thread (overwrite) + a present remote thread
        // the fold deleted (remove) + a stale undo-recovery pointer (overwrite).
        let remote_stale_old = ChangeId::generate();
        let remote_stale_new = ChangeId::generate();
        let remote_doomed = ChangeId::generate();
        let undo_old = ChangeId::generate();
        let undo_state = ChangeId::generate();

        let calls = Arc::new(AtomicU64::new(0));
        let reconciler = Arc::new(FakeReconciler {
            generation: AtomicU64::new(1),
            republish: vec![
                RefUpdate::Thread {
                    name: ThreadName::new("present"),
                    expected: RefExpectation::Any,
                    new: Some(present_state),
                },
                RefUpdate::Thread {
                    name: ThreadName::new("stale"),
                    expected: RefExpectation::Any,
                    new: Some(stale_new),
                },
                RefUpdate::Thread {
                    name: ThreadName::new("absent"),
                    expected: RefExpectation::Any,
                    new: Some(absent_state),
                },
                // `new: None` arm — canonical absent ⇒ no-op (nothing to delete).
                RefUpdate::Thread {
                    name: ThreadName::new("deleted"),
                    expected: RefExpectation::Any,
                    new: None,
                },
                RefUpdate::Marker {
                    name: MarkerName::new("mk"),
                    expected: RefExpectation::Any,
                    new: Some(marker_state),
                },
            ],
            remote_updates: vec![
                ("origin".to_string(), ThreadName::new("rt"), Some(remote_state)),
                // `None` value, canonical absent — skipped.
                ("origin".to_string(), ThreadName::new("gone"), None),
                // Present-but-stale remote thread ⇒ overwritten with committed value.
                ("origin".to_string(), ThreadName::new("rt_stale"), Some(remote_stale_new)),
                // Present remote thread the fold deleted ⇒ removed.
                ("origin".to_string(), ThreadName::new("rt_doomed"), None),
            ],
            undo_recovery: Some(undo_state),
            calls: Arc::clone(&calls),
        });
        let refs = RefManager::new(&dir).with_reconciler(Arc::clone(&reconciler) as Arc<dyn RefReconciler>);
        refs.init().unwrap();
        // Publish present + stale AFTER injection (raw writes, bypass chokepoint).
        refs.set_thread(&ThreadName::new("present"), &present_state).unwrap();
        refs.set_thread(&ThreadName::new("stale"), &stale_old).unwrap();
        refs.set_remote_thread("origin", &ThreadName::new("rt_stale"), &remote_stale_old).unwrap();
        refs.set_remote_thread("origin", &ThreadName::new("rt_doomed"), &remote_doomed).unwrap();
        refs.set_undo_recovery(&undo_old).unwrap();

        // Bump generation so the next read lags ⇒ reconcile + materialize run.
        reconciler.generation.store(2, Ordering::Release);

        let got = refs.get_thread(&ThreadName::new("absent")).unwrap();
        assert_eq!(got, Some(absent_state), "reconciled value is surfaced");
        assert_eq!(calls.load(Ordering::Acquire), 1, "lag path reconciles once");

        // The absent refs were materialized; the present (same-value) one is a
        // no-op; the stale present one is overwritten with the committed value.
        assert_eq!(
            refs.get_thread(&ThreadName::new("present")).unwrap(),
            Some(present_state)
        );
        assert_eq!(
            refs.get_thread(&ThreadName::new("stale")).unwrap(),
            Some(stale_new),
            "a stale present ref must be overwritten with the committed value"
        );
        assert_eq!(refs.get_marker(&MarkerName::new("mk")).unwrap(), Some(marker_state));
        assert_eq!(
            refs.get_remote_thread("origin", &ThreadName::new("rt")).unwrap(),
            Some(remote_state)
        );
        // The stale remote thread is overwritten; the doomed one is removed.
        assert_eq!(
            refs.get_remote_thread("origin", &ThreadName::new("rt_stale")).unwrap(),
            Some(remote_stale_new),
            "a stale present remote thread must be overwritten with the committed value"
        );
        assert_eq!(
            refs.get_remote_thread("origin", &ThreadName::new("rt_doomed")).unwrap(),
            None,
            "a committed delete must remove a present remote thread"
        );
        // The stale undo-recovery pointer is overwritten with the committed value.
        assert_eq!(refs.get_undo_recovery().unwrap(), Some(undo_state));

        // Second read: watermark now == generation (2) ⇒ hot path, no new reconcile.
        let before = calls.load(Ordering::Acquire);
        let _ = refs.get_thread(&ThreadName::new("absent")).unwrap();
        assert_eq!(calls.load(Ordering::Acquire), before, "watermark caught up ⇒ hot path");
    }

    /// `commit_and_publish` appends the ref-carrying records (phase 4) before
    /// publishing the ref batch (phase 5), and degrades to a plain publish when
    /// no committer is injected.
    #[test]
    fn commit_and_publish_records_before_it_publishes() {
        let (_t, dir) = manager();
        let committer = Arc::new(FakeCommitter {
            seen: Mutex::new(Vec::new()),
        });
        let refs = RefManager::new(&dir).with_committer(Arc::clone(&committer) as Arc<dyn RefCommitter>);
        refs.init().unwrap();

        let state = ChangeId::generate();
        let records = vec![vec![1u8, 2, 3]];
        let updates = vec![RefUpdate::Thread {
            name: ThreadName::new("feature"),
            expected: RefExpectation::Missing,
            new: Some(state),
        }];
        refs.commit_and_publish(&records, &updates, Some("lane")).unwrap();

        // The committer saw the records (phase 4) and the ref published (phase 5).
        let seen = committer.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, records);
        assert_eq!(seen[0].1.as_deref(), Some("lane"));
        drop(seen);
        assert_eq!(
            refs.get_thread(&ThreadName::new("feature")).unwrap(),
            Some(state)
        );

        // No-committer path: a plain publish (no records) still publishes, and
        // an empty ref batch is a no-op.
        let (_t2, dir2) = manager();
        let plain = RefManager::new(&dir2);
        plain.init().unwrap();
        plain.commit_and_publish(&[], &[], None).unwrap();
        let other = ChangeId::generate();
        plain
            .commit_and_publish(
                &[],
                &[RefUpdate::Marker {
                    name: MarkerName::new("v2"),
                    expected: RefExpectation::Missing,
                    new: Some(other),
                }],
                None,
            )
            .unwrap();
        assert_eq!(plain.get_marker(&MarkerName::new("v2")).unwrap(), Some(other));
    }

    /// The remote-thread raw read/write/delete + list paths and `pack_refs`,
    /// `resolve`, and the `RefBackend`/`CoreRefBackend` trait delegations.
    #[test]
    fn remote_threads_pack_and_trait_delegations() {
        use super::super::{CoreRefBackend, RefBackend};

        let (_t, dir) = manager();
        let refs = RefManager::new(&dir);
        refs.init().unwrap();

        let s1 = ChangeId::generate();
        let s2 = ChangeId::generate();
        // RefBackend trait surface for remotes.
        RefBackend::set_remote_thread(&refs, "origin", &ThreadName::new("rt1"), &s1).unwrap();
        refs.set_remote_thread("origin", &ThreadName::new("rt2"), &s2).unwrap();
        assert_eq!(
            RefBackend::get_remote_thread(&refs, "origin", &ThreadName::new("rt1")).unwrap(),
            Some(s1)
        );
        assert!(RefBackend::list_remotes(&refs).unwrap().contains(&"origin".to_string()));
        let mut rts = RefBackend::list_remote_threads(&refs, "origin").unwrap();
        rts.sort();
        assert_eq!(rts.len(), 2);
        let removed = RefBackend::delete_remote_thread(&refs, "origin", &ThreadName::new("rt1")).unwrap();
        assert_eq!(removed, Some(s1));
        // Deleting an absent remote thread returns None.
        assert_eq!(
            refs.delete_remote_thread("origin", &ThreadName::new("nope")).unwrap(),
            None
        );

        // Threads + markers + pack_refs (packs loose refs into packed-refs).
        let t = ChangeId::generate();
        let m = ChangeId::generate();
        refs.set_thread(&ThreadName::new("main2"), &t).unwrap();
        refs.create_marker(&MarkerName::new("rel"), &m).unwrap();
        RefBackend::pack_refs(&refs).unwrap();
        assert_eq!(refs.get_thread(&ThreadName::new("main2")).unwrap(), Some(t));
        assert_eq!(refs.get_marker(&MarkerName::new("rel")).unwrap(), Some(m));

        // resolve() funnels through read_head/get_thread/get_marker/undo.
        assert_eq!(refs.resolve("main2").unwrap(), Some(t));
        assert_eq!(refs.resolve("rel").unwrap(), Some(m));

        // CoreRefBackend async + sync delegations.
        let got = pollster::block_on(CoreRefBackend::get_thread(&refs, &ThreadName::new("main2"))).unwrap();
        assert_eq!(got, Some(t));
        let gm = pollster::block_on(CoreRefBackend::get_marker(&refs, &MarkerName::new("rel"))).unwrap();
        assert_eq!(gm, Some(m));
        assert!(CoreRefBackend::list_threads(&refs).unwrap().contains(&ThreadName::new("main2")));
        assert!(CoreRefBackend::list_markers(&refs).unwrap().contains(&MarkerName::new("rel")));
        let r = pollster::block_on(CoreRefBackend::resolve(&refs, "main2")).unwrap();
        assert_eq!(r, Some(t));

        // Maintenance trait methods.
        let _ = RefBackend::inspect_ref_summary_index(&refs).unwrap();
        let _ = RefBackend::rebuild_ref_summary_index(&refs).unwrap();
        RefBackend::cleanup_stale_temps(&refs);
    }
}
