// SPDX-License-Identifier: Apache-2.0
use std::{sync::mpsc, time::Duration};

use objects::{
    error::HeddleError,
    object::{MarkerName, StateId, ThreadName},
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

/// Product-path packed-refs stress below the ~10k degradation threshold.
///
/// Keeps CI honest that `pack_refs` + cold load remain correct as ref count
/// grows into the low thousands. Full 10k/50k/100k scale lives in Criterion
/// (`reftable_vs_packed`) — see `docs/program/PACKED_REFS_STRESS.md`.
#[test]
fn packed_refs_product_stress_two_thousand_threads() {
    let (temp, refs) = create_ref_manager();
    const N: usize = 2_000;
    let mut ids = Vec::with_capacity(N);
    for i in 0..N {
        let id = crate::refs::fresh_state_id();
        let name = ThreadName::new(format!("stress/thread-{i:05}"));
        refs.set_thread(&name, &id).unwrap();
        ids.push((name, id));
    }
    refs.pack_refs().unwrap();

    // Cold load via a new manager on the same heddle dir.
    let heddle_dir = temp.path().join(".heddle");
    let reloaded = RefManager::new(&heddle_dir);
    for (name, id) in &ids {
        let got = reloaded
            .get_thread(name)
            .unwrap()
            .expect("packed thread must resolve after pack_refs");
        assert_eq!(&got, id);
    }
    let listed = reloaded.list_threads().unwrap();
    assert!(
        listed.len() >= N,
        "expected at least {N} threads after pack, got {}",
        listed.len()
    );
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
    let id = crate::refs::fresh_state_id();
    let head = Head::Detached { state: id };
    refs.write_head(&head).unwrap();
    let read = refs.read_head().unwrap();
    assert_eq!(read, head);
}
#[test]
fn test_track_operations() {
    let (_temp, refs) = create_ref_manager();
    let id = crate::refs::fresh_state_id();
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
    let id = crate::refs::fresh_state_id();
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
    let parent = crate::refs::fresh_state_id();
    let child = crate::refs::fresh_state_id();

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
    let id = crate::refs::fresh_state_id();
    refs.create_marker(&MarkerName::new("v1.0.0"), &id).unwrap();
    let got = refs.get_marker(&MarkerName::new("v1.0.0")).unwrap();
    assert_eq!(got, Some(id));
    let markers = refs.list_markers().unwrap();
    assert_eq!(markers, vec![MarkerName::new("v1.0.0")]);
}
#[test]
fn test_marker_no_overwrite() {
    let (_temp, refs) = create_ref_manager();
    let id = crate::refs::fresh_state_id();
    refs.create_marker(&MarkerName::new("v1.0.0"), &id).unwrap();
    let result = refs.create_marker(&MarkerName::new("v1.0.0"), &id);
    assert!(result.is_err());
}
#[test]
fn test_resolve() {
    let (_temp, refs) = create_ref_manager();
    let id = crate::refs::fresh_state_id();
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
    let thread_id = crate::refs::fresh_state_id();
    let marker_id = crate::refs::fresh_state_id();

    CoreRefBackend::set_thread(&refs, &ThreadName::new("main"), &thread_id).unwrap();
    assert_eq!(
        pollster::block_on(CoreRefBackend::get_thread(&refs, &ThreadName::new("main"))).unwrap(),
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
    let id1 = crate::refs::fresh_state_id();
    let id2 = crate::refs::fresh_state_id();
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
    let id1 = crate::refs::fresh_state_id();
    let id2 = crate::refs::fresh_state_id();
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
    let id1 = crate::refs::fresh_state_id();
    let id2 = crate::refs::fresh_state_id();
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
    let id = crate::refs::fresh_state_id();
    let result = refs.set_thread(&ThreadName::new("../etc/passwd"), &id);
    assert!(matches!(result, Err(HeddleError::InvalidRefName(_))));
}
#[test]
fn test_invalid_track_name_absolute_path() {
    let (_temp, refs) = create_ref_manager();
    let id = crate::refs::fresh_state_id();
    let result = refs.set_thread(&ThreadName::new("/etc/passwd"), &id);
    assert!(matches!(result, Err(HeddleError::InvalidRefName(_))));
}
#[test]
fn test_invalid_track_name_with_backslash() {
    let (_temp, refs) = create_ref_manager();
    let id = crate::refs::fresh_state_id();
    let result = refs.set_thread(&ThreadName::new("\\windows\\system32"), &id);
    assert!(matches!(result, Err(HeddleError::InvalidRefName(_))));
}
#[test]
fn test_invalid_marker_name_path_traversal() {
    let (_temp, refs) = create_ref_manager();
    let id = crate::refs::fresh_state_id();
    let result = refs.create_marker(&MarkerName::new("../../../root"), &id);
    assert!(matches!(result, Err(HeddleError::InvalidRefName(_))));
}
#[test]
fn test_set_remote_thread_waits_for_refs_lock() {
    let (_temp, refs) = create_ref_manager();
    let lock = refs.lock_refs().unwrap();
    let root = refs.root.clone();
    let state_id = crate::refs::fresh_state_id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let refs = RefManager::new(root);
        let result = refs.set_remote_thread("origin", &ThreadName::new("main"), &state_id);
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
    let state_id = crate::refs::fresh_state_id();
    refs.set_remote_thread("origin", &ThreadName::new("main"), &state_id)
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
    assert_eq!(result.unwrap(), Some(state_id));
}

#[test]
fn test_ref_summary_index_rebuild_reports_repo_ref_shape() {
    let (_temp, refs) = create_ref_manager();
    let main = crate::refs::fresh_state_id();
    let feature = crate::refs::fresh_state_id();
    let marker = crate::refs::fresh_state_id();
    let remote_main = crate::refs::fresh_state_id();
    let remote_feature = crate::refs::fresh_state_id();

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

/// The incremental per-publish summary-index update (perf/adopt: avoids the
/// O(refs²) full-dir rescan) must produce a sidecar that is byte-identical to a
/// full from-storage rebuild after the same sequence of mixed set/delete/pack
/// operations — including the loose-over-packed `source` distinction. A drift
/// here would corrupt ref resolution.
#[test]
fn test_incremental_ref_summary_index_matches_full_rebuild() {
    let (_temp, refs) = create_ref_manager();

    // A mixed sequence exercising: many sets (the adopt-style growth), an
    // overwrite, deletes (loose + packed purge), packed entries, and a loose
    // override of a packed ref (the LooseAndPacked source case).
    let ids: Vec<StateId> = (0..12).map(|_| crate::refs::fresh_state_id()).collect();

    refs.set_thread(&ThreadName::new("main"), &ids[0]).unwrap();
    refs.set_thread(&ThreadName::new("feature/api"), &ids[1])
        .unwrap();
    refs.set_thread(&ThreadName::new("feature/ui"), &ids[2])
        .unwrap();
    refs.create_marker(&MarkerName::new("v1.0.0"), &ids[3])
        .unwrap();
    refs.create_marker(&MarkerName::new("v1.1.0"), &ids[4])
        .unwrap();

    // Pack everything, then add loose refs on top — including a loose override
    // of a packed thread (must record source = loose+packed incrementally).
    refs.pack_refs().unwrap();
    refs.set_thread(&ThreadName::new("main"), &ids[5]).unwrap();
    refs.set_thread(&ThreadName::new("hotfix"), &ids[6])
        .unwrap();
    refs.create_marker(&MarkerName::new("v2.0.0"), &ids[7])
        .unwrap();

    // Overwrite + delete a mix of loose and packed-backed refs.
    refs.set_thread(&ThreadName::new("feature/ui"), &ids[8])
        .unwrap();
    refs.delete_thread(&ThreadName::new("feature/api")).unwrap();
    refs.delete_marker(&MarkerName::new("v1.0.0")).unwrap();

    // Remote threads (untouched by the incremental publish path) must survive.
    refs.set_remote_thread("origin", &ThreadName::new("main"), &ids[9])
        .unwrap();
    refs.set_remote_thread("origin", &ThreadName::new("dev"), &ids[10])
        .unwrap();

    refs.set_thread(&ThreadName::new("release"), &ids[11])
        .unwrap();

    // Snapshot the incrementally-maintained on-disk sidecar...
    let incremental = std::fs::read_to_string(refs.ref_summary_index_path()).unwrap();

    // ...then force a full from-storage rebuild and compare byte-for-byte.
    refs.rebuild_ref_summary_index().unwrap();
    let from_storage = std::fs::read_to_string(refs.ref_summary_index_path()).unwrap();

    assert_eq!(
        incremental, from_storage,
        "incremental summary index diverged from a full from-storage rebuild"
    );

    // And the resolved values must still be correct through the public API.
    assert_eq!(
        refs.get_thread(&ThreadName::new("main")).unwrap(),
        Some(ids[5])
    );
    assert_eq!(
        refs.get_thread(&ThreadName::new("feature/ui")).unwrap(),
        Some(ids[8])
    );
    assert_eq!(
        refs.get_thread(&ThreadName::new("feature/api")).unwrap(),
        None
    );
    assert_eq!(refs.get_marker(&MarkerName::new("v1.0.0")).unwrap(), None);
    assert_eq!(
        refs.list_threads().unwrap(),
        vec![
            ThreadName::new("feature/ui"),
            ThreadName::new("hotfix"),
            ThreadName::new("main"),
            ThreadName::new("release"),
        ]
    );
}

/// Scaling proof for perf/adopt (run with `--ignored --nocapture`). Two views:
///
/// 1. **Marginal per-publish index cost** at a refs dir already holding N refs:
///    one incremental delta-fold (the new path, O(changed)) vs one full
///    from-storage rebuild (the old per-publish behavior, O(refs)). This is the
///    cost that ran once *per publish* and made `adopt` quadratic.
/// 2. **End-to-end** cost of growing the dir to N refs the new way vs forcing a
///    full rebuild after every publish — the gap is the eliminated O(refs²)
///    rescan term.
#[test]
#[ignore = "timing harness; run explicitly with --ignored --nocapture"]
fn bench_incremental_vs_full_rebuild_scaling() {
    use std::time::Instant;

    const ITERS: u32 = 50;

    println!("\n-- marginal cost of ONE index update at a dir already holding N refs --");
    for n in [101usize, 401, 801, 1548] {
        let (_t, refs) = create_ref_manager();
        for i in 0..n {
            refs.set_thread(
                &ThreadName::new(format!("branch-{i:05}")),
                &crate::refs::fresh_state_id(),
            )
            .unwrap();
        }

        // One incremental delta-fold (set an existing thread -> single delta).
        let incr = Instant::now();
        for _ in 0..ITERS {
            refs.set_thread(
                &ThreadName::new("branch-00000"),
                &crate::refs::fresh_state_id(),
            )
            .unwrap();
        }
        let incr_per = incr.elapsed() / ITERS;

        // One full from-storage rebuild (the old per-publish cost).
        let full = Instant::now();
        for _ in 0..ITERS {
            refs.rebuild_ref_summary_index().unwrap();
        }
        let full_per = full.elapsed() / ITERS;

        println!(
            "n={n:5}  incremental-fold={:>10.1?}  full-rebuild={:>10.1?}  speedup={:.1}x",
            incr_per,
            full_per,
            full_per.as_secs_f64() / incr_per.as_secs_f64()
        );
    }

    println!("\n-- end-to-end: grow dir to N refs (gap = eliminated O(refs²) rescan) --");
    for n in [101usize, 401, 801, 1548] {
        let (_t1, inc) = create_ref_manager();
        let start = Instant::now();
        for i in 0..n {
            inc.set_thread(
                &ThreadName::new(format!("branch-{i:05}")),
                &crate::refs::fresh_state_id(),
            )
            .unwrap();
        }
        let inc_elapsed = start.elapsed();

        let (_t2, full) = create_ref_manager();
        let start = Instant::now();
        for i in 0..n {
            full.set_thread(
                &ThreadName::new(format!("branch-{i:05}")),
                &crate::refs::fresh_state_id(),
            )
            .unwrap();
            full.rebuild_ref_summary_index().unwrap();
        }
        let full_elapsed = start.elapsed();

        println!(
            "n={n:5}  incremental={:>9.1?}  full-rebuild-per-publish={:>9.1?}  delta={:>9.1?}",
            inc_elapsed,
            full_elapsed,
            full_elapsed.saturating_sub(inc_elapsed)
        );
    }
}

#[test]
fn test_ref_summary_index_falls_back_when_sidecar_is_corrupt() {
    let (_temp, refs) = create_ref_manager();
    let main = crate::refs::fresh_state_id();
    let marker = crate::refs::fresh_state_id();
    let remote = crate::refs::fresh_state_id();

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
    let recovery_state = crate::refs::fresh_state_id();
    let user_state = crate::refs::fresh_state_id();
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
/// pointer; otherwise `undo --recover` in A would restore B's pre-undo
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

    let state_a = crate::refs::fresh_state_id();
    let state_b = crate::refs::fresh_state_id();
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
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    };

    use objects::{
        error::Result,
        object::{MarkerName, StateId, ThreadName},
        sync::LockExt,
    };
    use tempfile::TempDir;

    use super::super::{
        LoadRequest, Loaded, ReconcileOutcome, RefCommitter, RefExpectation, RefManager,
        RefReconciler, RefUpdate,
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
        remote_updates: Vec<(String, ThreadName, Option<StateId>)>,
        undo_recovery: Option<StateId>,
        calls: Arc<AtomicU64>,
    }

    impl RefReconciler for FakeReconciler {
        fn generation(&self) -> Result<u64> {
            Ok(self.generation.load(Ordering::Acquire))
        }
        fn reconcile(
            &self,
            req: &LoadRequest,
            raw: Loaded,
            _since: u64,
        ) -> Result<ReconcileOutcome> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            // Project the request's authoritative value out of the configured
            // materialization set (mirrors the real reconciler's per-request fold).
            let loaded = match req {
                LoadRequest::Thread(name) => self
                    .republish
                    .iter()
                    .find_map(|u| match u {
                        RefUpdate::Thread { name: n, new, .. } if n == name => {
                            Some(Loaded::Point(*new))
                        }
                        _ => None,
                    })
                    .unwrap_or(raw),
                LoadRequest::Marker(name) => self
                    .republish
                    .iter()
                    .find_map(|u| match u {
                        RefUpdate::Marker { name: n, new, .. } if n == name => {
                            Some(Loaded::Point(*new))
                        }
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
        assert_eq!(
            calls.load(Ordering::Acquire),
            0,
            "hot path must not reconcile"
        );
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
        let present_state = crate::refs::fresh_state_id();
        // `stale` is pre-published with an OLD value; the fold carries a newer
        // committed value ⇒ it must be overwritten (authoritative-apply).
        let stale_old = crate::refs::fresh_state_id();
        let stale_new = crate::refs::fresh_state_id();
        let absent_state = crate::refs::fresh_state_id();
        let marker_state = crate::refs::fresh_state_id();
        let remote_state = crate::refs::fresh_state_id();
        // A present-but-stale remote thread (overwrite) + a present remote thread
        // the fold deleted (remove) + a stale undo-recovery pointer (overwrite).
        let remote_stale_old = crate::refs::fresh_state_id();
        let remote_stale_new = crate::refs::fresh_state_id();
        let remote_doomed = crate::refs::fresh_state_id();
        let undo_old = crate::refs::fresh_state_id();
        let undo_state = crate::refs::fresh_state_id();

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
                (
                    "origin".to_string(),
                    ThreadName::new("rt"),
                    Some(remote_state),
                ),
                // `None` value, canonical absent — skipped.
                ("origin".to_string(), ThreadName::new("gone"), None),
                // Present-but-stale remote thread ⇒ overwritten with committed value.
                (
                    "origin".to_string(),
                    ThreadName::new("rt_stale"),
                    Some(remote_stale_new),
                ),
                // Present remote thread the fold deleted ⇒ removed.
                ("origin".to_string(), ThreadName::new("rt_doomed"), None),
            ],
            undo_recovery: Some(undo_state),
            calls: Arc::clone(&calls),
        });
        let refs = RefManager::new(&dir)
            .with_reconciler(Arc::clone(&reconciler) as Arc<dyn RefReconciler>);
        refs.init().unwrap();
        // Publish present + stale AFTER injection (raw writes, bypass chokepoint).
        refs.set_thread(&ThreadName::new("present"), &present_state)
            .unwrap();
        refs.set_thread(&ThreadName::new("stale"), &stale_old)
            .unwrap();
        refs.set_remote_thread("origin", &ThreadName::new("rt_stale"), &remote_stale_old)
            .unwrap();
        refs.set_remote_thread("origin", &ThreadName::new("rt_doomed"), &remote_doomed)
            .unwrap();
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
        assert_eq!(
            refs.get_marker(&MarkerName::new("mk")).unwrap(),
            Some(marker_state)
        );
        assert_eq!(
            refs.get_remote_thread("origin", &ThreadName::new("rt"))
                .unwrap(),
            Some(remote_state)
        );
        // The stale remote thread is overwritten; the doomed one is removed.
        assert_eq!(
            refs.get_remote_thread("origin", &ThreadName::new("rt_stale"))
                .unwrap(),
            Some(remote_stale_new),
            "a stale present remote thread must be overwritten with the committed value"
        );
        assert_eq!(
            refs.get_remote_thread("origin", &ThreadName::new("rt_doomed"))
                .unwrap(),
            None,
            "a committed delete must remove a present remote thread"
        );
        // The stale undo-recovery pointer is overwritten with the committed value.
        assert_eq!(refs.get_undo_recovery().unwrap(), Some(undo_state));

        // Second read: watermark now == generation (2) ⇒ hot path, no new reconcile.
        let before = calls.load(Ordering::Acquire);
        let _ = refs.get_thread(&ThreadName::new("absent")).unwrap();
        assert_eq!(
            calls.load(Ordering::Acquire),
            before,
            "watermark caught up ⇒ hot path"
        );
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
        let refs =
            RefManager::new(&dir).with_committer(Arc::clone(&committer) as Arc<dyn RefCommitter>);
        refs.init().unwrap();

        let state = crate::refs::fresh_state_id();
        let records = vec![vec![1u8, 2, 3]];
        let updates = vec![RefUpdate::Thread {
            name: ThreadName::new("feature"),
            expected: RefExpectation::Missing,
            new: Some(state),
        }];
        refs.commit_and_publish(&records, &updates, Some("lane"))
            .unwrap();

        // The committer saw the records (phase 4) and the ref published (phase 5).
        let seen = committer.seen.lock_or_poisoned();
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
        let other = crate::refs::fresh_state_id();
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
        assert_eq!(
            plain.get_marker(&MarkerName::new("v2")).unwrap(),
            Some(other)
        );
    }

    /// Fail closed (heddle#354 r9, cid 3330304656): a `commit_and_publish` with
    /// records but NO committer must error and publish NOTHING — committed data
    /// can never be silently dropped while the ref is published anyway.
    #[test]
    fn commit_and_publish_without_committer_fails_closed_on_records() {
        let (_t, dir) = manager();
        let plain = RefManager::new(&dir);
        plain.init().unwrap();

        let state = crate::refs::fresh_state_id();
        let result = plain.commit_and_publish(
            &[vec![1u8, 2, 3]],
            &[RefUpdate::Thread {
                name: ThreadName::new("feature"),
                expected: RefExpectation::Missing,
                new: Some(state),
            }],
            None,
        );

        // Errors rather than silently dropping the record...
        assert!(
            result.is_err(),
            "records with no committer must fail closed, not drop silently"
        );
        // ...and the ref was NOT published (no half-applied write).
        assert_eq!(plain.get_thread(&ThreadName::new("feature")).unwrap(), None);
    }

    /// heddle#354 r10 (cid 3330632443): when phase-5 publish fails AFTER the
    /// record durably committed, the operation has already linearized — the arm
    /// LOGS the swallowed publish error (operator visibility) and still returns
    /// `Ok(())`. Returning `Err` here would falsely report failure for a
    /// successful op; reconciliation materializes the committed effect later.
    #[test]
    #[cfg(unix)]
    fn publish_failure_after_commit_logs_and_returns_ok() {
        use std::os::unix::fs::PermissionsExt;

        let (_t, dir) = manager();
        let refs = RefManager::new(&dir);
        refs.init().unwrap();

        // Read-only threads dir makes the phase-5 temp write fail, while the
        // phase-3 read of the still-missing ref (which needs no write perm)
        // succeeds — isolating a publish-after-commit failure.
        let threads_dir = refs.threads_dir();
        let original = std::fs::metadata(&threads_dir)
            .unwrap()
            .permissions()
            .mode();
        std::fs::set_permissions(&threads_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let lock = refs.lock_refs().unwrap();
        let updates = vec![RefUpdate::Thread {
            name: ThreadName::new("feature"),
            expected: RefExpectation::Missing,
            new: Some(crate::refs::fresh_state_id()),
        }];
        // commit() reports the record durably committed (committed_for_reconcile);
        // publish then fails. Behavior must be Ok(()), not Err.
        let result = refs.validate_commit_publish(&updates, &lock, || Ok(true));

        std::fs::set_permissions(&threads_dir, std::fs::Permissions::from_mode(original)).unwrap();
        drop(lock);

        assert!(
            result.is_ok(),
            "a publish failure after a durable commit linearized the op: must return Ok(()), not Err"
        );
        // The ref was NOT published (publish failed) — reconciliation will
        // materialize the committed effect on the next read.
        assert_eq!(refs.get_thread(&ThreadName::new("feature")).unwrap(), None);
    }

    /// The remote-thread raw read/write/delete + list paths and `pack_refs`,
    /// `resolve`, and the `RefBackend`/`CoreRefBackend` trait delegations.
    #[test]
    fn remote_threads_pack_and_trait_delegations() {
        use super::super::{CoreRefBackend, RefBackend};

        let (_t, dir) = manager();
        let refs = RefManager::new(&dir);
        refs.init().unwrap();

        let s1 = crate::refs::fresh_state_id();
        let s2 = crate::refs::fresh_state_id();
        // RefBackend trait surface for remotes.
        RefBackend::set_remote_thread(&refs, "origin", &ThreadName::new("rt1"), &s1).unwrap();
        refs.set_remote_thread("origin", &ThreadName::new("rt2"), &s2)
            .unwrap();
        assert_eq!(
            RefBackend::get_remote_thread(&refs, "origin", &ThreadName::new("rt1")).unwrap(),
            Some(s1)
        );
        assert!(
            RefBackend::list_remotes(&refs)
                .unwrap()
                .contains(&"origin".to_string())
        );
        let mut rts = RefBackend::list_remote_threads(&refs, "origin").unwrap();
        rts.sort();
        assert_eq!(rts.len(), 2);
        let removed =
            RefBackend::delete_remote_thread(&refs, "origin", &ThreadName::new("rt1")).unwrap();
        assert_eq!(removed, Some(s1));
        // Deleting an absent remote thread returns None.
        assert_eq!(
            refs.delete_remote_thread("origin", &ThreadName::new("nope"))
                .unwrap(),
            None
        );

        // Threads + markers + pack_refs (packs loose refs into packed-refs).
        let t = crate::refs::fresh_state_id();
        let m = crate::refs::fresh_state_id();
        refs.set_thread(&ThreadName::new("main2"), &t).unwrap();
        refs.create_marker(&MarkerName::new("rel"), &m).unwrap();
        RefBackend::pack_refs(&refs).unwrap();
        assert_eq!(refs.get_thread(&ThreadName::new("main2")).unwrap(), Some(t));
        assert_eq!(refs.get_marker(&MarkerName::new("rel")).unwrap(), Some(m));

        // resolve() funnels through read_head/get_thread/get_marker/undo.
        assert_eq!(refs.resolve("main2").unwrap(), Some(t));
        assert_eq!(refs.resolve("rel").unwrap(), Some(m));

        // CoreRefBackend async + sync delegations.
        let got = pollster::block_on(CoreRefBackend::get_thread(&refs, &ThreadName::new("main2")))
            .unwrap();
        assert_eq!(got, Some(t));
        let gm =
            pollster::block_on(CoreRefBackend::get_marker(&refs, &MarkerName::new("rel"))).unwrap();
        assert_eq!(gm, Some(m));
        assert!(
            CoreRefBackend::list_threads(&refs)
                .unwrap()
                .contains(&ThreadName::new("main2"))
        );
        assert!(
            CoreRefBackend::list_markers(&refs)
                .unwrap()
                .contains(&MarkerName::new("rel"))
        );
        let r = pollster::block_on(CoreRefBackend::resolve(&refs, "main2")).unwrap();
        assert_eq!(r, Some(t));

        // Maintenance trait methods.
        let _ = RefBackend::inspect_ref_summary_index(&refs).unwrap();
        let _ = RefBackend::rebuild_ref_summary_index(&refs).unwrap();
        RefBackend::cleanup_stale_temps(&refs);
    }

    /// heddle#354 r7 (cid 3329765075) — a long-lived handle re-reads the CURRENT
    /// persisted (shared) watermark on each reconcile, so a sibling worktree's
    /// advance stops it re-folding records the sibling already materialized.
    /// Two contrasting cases on a handle whose in-memory watermark is frozen at
    /// its open value (5) while a sibling advanced the oplog tip to 10:
    ///
    /// * **A** — the sibling never persisted the advance: the handle still folds
    ///   the lag (the frozen-at-open behaviour, unchanged).
    /// * **B** — THE FIX: the sibling persisted the shared watermark to the tip;
    ///   the handle re-reads it and does NOT re-fold. Pre-r7 (no refresh) this
    ///   would fold from the frozen 5 and re-derive the sibling's work.
    #[test]
    fn long_lived_handle_refreshes_persisted_shared_watermark() {
        // Case A — no persisted advance ⇒ the lag is folded.
        let (_ta, dir_a) = manager();
        let calls_a = Arc::new(AtomicU64::new(0));
        let recon_a = Arc::new(FakeReconciler {
            generation: AtomicU64::new(5),
            republish: Vec::new(),
            remote_updates: Vec::new(),
            undo_recovery: None,
            calls: Arc::clone(&calls_a),
        });
        let refs_a =
            RefManager::new(&dir_a).with_reconciler(Arc::clone(&recon_a) as Arc<dyn RefReconciler>);
        refs_a.init().unwrap();
        // A sibling advanced the oplog tip to 10 but never persisted the shared
        // watermark; this handle's in-memory watermark stays frozen at 5.
        recon_a.generation.store(10, Ordering::Release);
        let _ = refs_a.get_marker(&MarkerName::new("any")).unwrap();
        assert_eq!(
            calls_a.load(Ordering::Acquire),
            1,
            "no persisted advance ⇒ the lag is folded"
        );

        // Case B — the sibling persisted the shared watermark to the tip; the
        // long-lived handle re-reads it and does NOT re-fold.
        let (_tb, dir_b) = manager();
        let calls_b = Arc::new(AtomicU64::new(0));
        let recon_b = Arc::new(FakeReconciler {
            generation: AtomicU64::new(5),
            republish: Vec::new(),
            remote_updates: Vec::new(),
            undo_recovery: None,
            calls: Arc::clone(&calls_b),
        });
        let refs_b =
            RefManager::new(&dir_b).with_reconciler(Arc::clone(&recon_b) as Arc<dyn RefReconciler>);
        refs_b.init().unwrap();
        recon_b.generation.store(10, Ordering::Release);
        // The sibling's persisted SHARED last-clean point (the shared-dir file).
        std::fs::write(dir_b.join("RECONCILE_WATERMARK_SHARED"), "10\n").unwrap();
        let _ = refs_b.get_marker(&MarkerName::new("any")).unwrap();
        assert_eq!(
            calls_b.load(Ordering::Acquire),
            0,
            "a long-lived handle must re-read the sibling-advanced shared \
             watermark and NOT re-fold (frozen-at-open would re-fold here)"
        );
    }
}

/// heddle#354 r7 — source-level conformance checks (the point of the
/// close-the-class round): they FAIL CI if any path bypasses the read/write
/// chokepoints. They walk the PRODUCTION source of `refs_manager.rs` +
/// `refs_transactions.rs`, partition it into per-function chunks, and assert the
/// no-bypass invariants:
///
/// * every raw backend WRITER is reached only from the chokepoint body or
///   `materialize` (so no write reaches the backend without first
///   reconciling+materializing under the lock);
/// * the per-request raw-loader funnel `raw_load` is reached only from the read
///   chokepoint (so no logical read bypasses `reconciled_load`);
/// * every public writer/reader funnels through the respective chokepoint; and
/// * the read chokepoint re-reads the persisted watermark each reconcile.
///
/// A planted-bypass fixture proves the analyzer is non-vacuous.
mod write_read_conformance {
    use std::collections::BTreeSet;

    const MANAGER_SRC: &str = include_str!("refs_manager.rs");
    const TXNS_SRC: &str = include_str!("refs_transactions.rs");

    /// Everything before the in-file `#[cfg(test)]` module — the invariant is
    /// enforced on production code only, never on the tests' own scaffolding.
    fn production(src: &str) -> &str {
        src.split("#[cfg(test)]").next().unwrap()
    }

    /// The function name a declaration line introduces, if any (`pub fn x(`,
    /// `pub(super) fn x(`, `async fn x(`, `fn x<T>(`, …). `None` otherwise.
    fn parse_fn_name(trimmed: &str) -> Option<String> {
        let idx = trimmed.find("fn ")?;
        let before = &trimmed[..idx];
        if !(before.is_empty() || before.ends_with(' ')) {
            return None;
        }
        let rest = &trimmed[idx + 3..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            return None;
        }
        let after = rest[name.len()..].trim_start();
        (after.starts_with('(') || after.starts_with('<')).then_some(name)
    }

    /// Partition source into `(fn_name, body_text)` chunks: a new chunk starts at
    /// each function declaration and runs until the next one. Comment lines never
    /// start a chunk. Sufficient for "which fn contains this call" without brace
    /// counting, since every method here is a 4-space-indented `impl` item.
    fn fn_chunks(src: &str) -> Vec<(String, String)> {
        let mut chunks = Vec::new();
        let mut name = "<prologue>".to_string();
        let mut body = String::new();
        for line in production(src).lines() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("//")
                && let Some(decl) = parse_fn_name(trimmed)
            {
                chunks.push((std::mem::take(&mut name), std::mem::take(&mut body)));
                name = decl;
            }
            body.push_str(line);
            body.push('\n');
        }
        chunks.push((name, body));
        chunks
    }

    /// The set of function names whose body contains a call `.callee(`.
    fn callers_of(srcs: &[&str], callee: &str) -> BTreeSet<String> {
        let pat = format!(".{callee}(");
        let mut out = BTreeSet::new();
        for src in srcs {
            for (name, body) in fn_chunks(src) {
                if body.contains(&pat) {
                    out.insert(name);
                }
            }
        }
        out
    }

    /// The body of the FIRST function named `name` (the inherent `impl
    /// RefManager` definition precedes the trait-delegation `impl`s).
    fn first_body(srcs: &[&str], name: &str) -> String {
        for src in srcs {
            for (fname, body) in fn_chunks(src) {
                if fname == name {
                    return body;
                }
            }
        }
        panic!("function `{name}` not found in sources");
    }

    fn strays(callers: &BTreeSet<String>, allow: &[&str]) -> Vec<String> {
        let allow: BTreeSet<String> = allow.iter().map(|s| s.to_string()).collect();
        callers.difference(&allow).cloned().collect()
    }

    fn assert_only(callers: &BTreeSet<String>, allow: &[&str], primitive: &str) {
        let strays = strays(callers, allow);
        assert!(
            strays.is_empty(),
            "chokepoint bypass: `{primitive}` is reached from {strays:?}, outside the \
             allowlist {allow:?} — route the write through `write_chokepoint` (or the \
             read through `reconciled_load`) instead of calling the raw primitive"
        );
    }

    /// WRITE no-bypass: every raw backend writer is reached only from a
    /// chokepoint body or from `materialize` (the read/write catch-up).
    #[test]
    fn raw_writers_are_reached_only_through_the_chokepoint() {
        let srcs = [MANAGER_SRC, TXNS_SRC];
        assert_only(
            &callers_of(&srcs, "publish_ref_plans"),
            &[
                "materialize",
                "update_refs_with_lock",
                "validate_commit_publish",
            ],
            "publish_ref_plans",
        );
        assert_only(
            &callers_of(&srcs, "set_remote_thread_locked"),
            &["materialize", "set_remote_thread_raw"],
            "set_remote_thread_locked",
        );
        assert_only(
            &callers_of(&srcs, "delete_remote_thread_locked"),
            &["materialize", "delete_remote_thread_raw"],
            "delete_remote_thread_locked",
        );
        assert_only(
            &callers_of(&srcs, "set_undo_recovery_locked"),
            &["materialize", "set_undo_recovery_raw"],
            "set_undo_recovery_locked",
        );
    }

    /// READ no-bypass: the per-request raw-loader funnel `raw_load` is reached
    /// only from the read chokepoint and the write-side reconcile catch-up.
    #[test]
    fn raw_load_is_reached_only_through_the_read_chokepoint() {
        assert_only(
            &callers_of(&[MANAGER_SRC], "raw_load"),
            &[
                "reconciled_load",
                "reconciled_value_under_lock",
                "materialize_class",
            ],
            "raw_load",
        );
    }

    /// Every public ref WRITER funnels through `write_chokepoint`.
    #[test]
    fn public_writers_funnel_through_write_chokepoint() {
        for writer in [
            "update_refs",
            "commit_and_publish",
            "set_undo_recovery_raw",
            "set_remote_thread_raw",
            "delete_remote_thread_raw",
        ] {
            assert!(
                first_body(&[MANAGER_SRC], writer).contains("write_chokepoint("),
                "write bypass: `{writer}` must route through `write_chokepoint`"
            );
        }
    }

    /// Every public ref READER funnels through `reconciled_load` and never calls
    /// a raw loader directly.
    #[test]
    fn public_readers_funnel_through_reconciled_load() {
        for reader in [
            "read_head",
            "get_thread",
            "get_marker",
            "get_undo_recovery",
            "get_remote_thread",
            "list_threads",
            "list_markers",
            "list_remotes",
            "list_remote_threads",
        ] {
            let body = first_body(&[MANAGER_SRC], reader);
            assert!(
                body.contains("reconciled_load(") || body.contains("reconciled_point("),
                "read bypass: `{reader}` must funnel through `reconciled_load` \
                 (directly or via the `reconciled_point` helper)"
            );
            for raw in [".raw_load(", ".raw_get_", ".read_head_state(", ".raw_list_"] {
                assert!(
                    !body.contains(raw),
                    "read bypass: `{reader}` calls a raw loader (`{raw}`) directly"
                );
            }
        }
        // The `reconciled_point` helper is the only sanctioned indirection a reader
        // may funnel through; assert it itself routes to the read chokepoint so a
        // future bypass planted inside the helper still trips this guard.
        assert!(
            first_body(&[MANAGER_SRC], "reconciled_point").contains("reconciled_load("),
            "read bypass: the `reconciled_point` helper must funnel through `reconciled_load`"
        );
    }

    /// The read chokepoint re-reads the persisted watermark each reconcile, so a
    /// long-lived handle never folds from a frozen-at-open value (cid 3329765075).
    #[test]
    fn read_chokepoint_refreshes_watermark_fresh() {
        assert!(
            first_body(&[MANAGER_SRC], "reconciled_load").contains("refresh_persisted_watermark("),
            "the read chokepoint must re-read the persisted watermark each reconcile"
        );
    }

    /// The analyzer has TEETH: a planted bypass (a function calling a raw writer
    /// outside the allowlist) is both detected by `callers_of` AND surfaced as a
    /// stray by the allowlist check — proving the conformance checks above are
    /// non-vacuous and would fail if such a function existed in production.
    #[test]
    fn analyzer_detects_a_planted_bypass() {
        let fixture = "\
    fn legit(&self, lock: &RefsLock) -> Result<()> {
        self.materialize(outcome, lock)
    }
    fn sneaky_bypass(&self, lock: &RefsLock) -> Result<()> {
        self.publish_ref_plans(plans, lock)
    }
";
        let callers = callers_of(&[fixture], "publish_ref_plans");
        assert!(
            callers.contains("sneaky_bypass"),
            "analyzer must flag a planted raw-writer bypass"
        );
        assert!(
            strays(&callers, &["materialize"]).contains(&"sneaky_bypass".to_string()),
            "the allowlist check must surface the planted bypass as a stray (non-vacuous)"
        );
    }
}
