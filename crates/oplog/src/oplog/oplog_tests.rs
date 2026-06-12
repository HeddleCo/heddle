// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::BTreeSet,
    sync::{Arc, Barrier},
    thread,
};

use objects::object::{ChangeId, ContentHash, MarkerName, ThreadName};
use tempfile::TempDir;

use super::oplog_backend::OpLogBackend;
use super::{OpLog, OpRecord};

fn create_oplog() -> (TempDir, OpLog) {
    let temp_dir = TempDir::new().unwrap();
    let heddle_dir = temp_dir.path().join(".heddle");
    std::fs::create_dir_all(&heddle_dir).unwrap();
    let oplog = OpLog::new_unattributed(&heddle_dir);
    oplog.init().unwrap();
    (temp_dir, oplog)
}

#[test]
fn test_record_snapshot() {
    let (_temp, oplog) = create_oplog();
    let state = ChangeId::generate();

    let id = oplog
        .record_snapshot(&state, None, None, Some("lane-a"))
        .unwrap();
    assert_eq!(id, 1);

    let entry = oplog.last().unwrap().unwrap();
    assert_eq!(entry.id, 1);
    assert!(!entry.undone);
    assert_eq!(entry.batch_id, 1);
    assert_eq!(entry.batch_index, 0);

    match entry.operation {
        OpRecord::Snapshot {
            new_state,
            prev_head,
            ..
        } => {
            assert_eq!(new_state, state);
            assert_eq!(prev_head, None);
        }
        _ => panic!("Expected Snapshot"),
    }
    assert_eq!(entry.scope.as_deref(), Some("lane-a"));
}

#[test]
fn test_record_multiple() {
    let (_temp, oplog) = create_oplog();

    let state1 = ChangeId::generate();
    let state2 = ChangeId::generate();

    oplog
        .record_snapshot(&state1, None, None, Some("lane-a"))
        .unwrap();
    oplog
        .record_snapshot(&state2, Some(&state1), None, Some("lane-a"))
        .unwrap();

    let entries = oplog.recent(2).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].id, 2);
    assert_eq!(entries[1].id, 1);
}

#[test]
fn test_record_batch() {
    let (_temp, oplog) = create_oplog();

    let state = ChangeId::generate();
    let ids = oplog
        .record_batch(vec![
            OpRecord::ThreadCreate {
                name: "main".to_string(),
                state,
                manager_snapshot: None,
            },
            OpRecord::ThreadDelete {
                name: "legacy".to_string(),
                state,
            },
        ])
        .unwrap();

    assert_eq!(ids.len(), 2);
    let entries = oplog.recent(2).unwrap();
    assert_eq!(entries[0].batch_id, ids[0]);
    assert_eq!(entries[1].batch_id, ids[0]);
}

#[test]
fn test_undo_batches() {
    let (_temp, oplog) = create_oplog();

    let state1 = ChangeId::generate();
    let state2 = ChangeId::generate();

    oplog
        .record_snapshot(&state1, None, None, Some("lane-a"))
        .unwrap();
    oplog
        .record_snapshot(&state2, Some(&state1), None, Some("lane-a"))
        .unwrap();

    let batches = oplog.undo_batches(1).unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].entries[0].id, 2);

    let updated = oplog.mark_batch_undone(&batches[0]).unwrap();
    assert!(updated.entries.iter().all(|entry| entry.undone));
}

#[test]
fn test_redo_batches() {
    let (_temp, oplog) = create_oplog();

    let state1 = ChangeId::generate();
    let state2 = ChangeId::generate();

    oplog
        .record_snapshot(&state1, None, None, Some("lane-a"))
        .unwrap();
    oplog
        .record_snapshot(&state2, Some(&state1), None, Some("lane-a"))
        .unwrap();

    let batches = oplog.undo_batches(1).unwrap();
    oplog.mark_batch_undone(&batches[0]).unwrap();

    let redo_batches = oplog.redo_batches(1).unwrap();
    assert_eq!(redo_batches.len(), 1);
    assert_eq!(redo_batches[0].entries[0].id, 2);

    let updated = oplog.mark_batch_redone(&redo_batches[0]).unwrap();
    assert!(updated.entries.iter().all(|entry| !entry.undone));
}

#[test]
fn test_record_snapshot_serializes_concurrent_writers() {
    let (_temp, oplog) = create_oplog();
    let oplog = Arc::new(oplog);
    let thread_count = 8;
    let barrier = Arc::new(Barrier::new(thread_count));

    let handles: Vec<_> = (0..thread_count)
        .map(|_| {
            let oplog = Arc::clone(&oplog);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let state = ChangeId::generate();
                barrier.wait();
                oplog
                    .record_snapshot(&state, None, None, Some("lane-a"))
                    .unwrap()
            })
        })
        .collect();

    let ids: BTreeSet<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();

    assert_eq!(ids.len(), thread_count);
    assert_eq!(
        ids.into_iter().collect::<Vec<_>>(),
        (1..=thread_count as u64).collect::<Vec<_>>()
    );
    assert_eq!(oplog.recent(thread_count).unwrap().len(), thread_count);
}

#[test]
fn test_undo_batches_scoped() {
    let (_temp, oplog) = create_oplog();
    let state1 = ChangeId::generate();
    let state2 = ChangeId::generate();
    let state3 = ChangeId::generate();

    oplog
        .record_snapshot(&state1, None, None, Some("lane-a"))
        .unwrap();
    oplog
        .record_snapshot(&state2, Some(&state1), None, Some("lane-b"))
        .unwrap();
    oplog
        .record_snapshot(&state3, Some(&state2), None, Some("lane-a"))
        .unwrap();

    let lane_a = oplog.undo_batches_scoped(2, Some("lane-a")).unwrap();
    assert_eq!(lane_a.len(), 2);
    assert_eq!(lane_a[0].entries[0].scope.as_deref(), Some("lane-a"));
    assert_eq!(lane_a[1].entries[0].scope.as_deref(), Some("lane-a"));

    let lane_b = oplog.undo_batches_scoped(2, Some("lane-b")).unwrap();
    assert_eq!(lane_b.len(), 1);
    assert_eq!(lane_b[0].entries[0].scope.as_deref(), Some("lane-b"));
}

#[test]
fn recent_batches_scoped_merges_non_adjacent_coalesced_batches() {
    let (_temp, oplog) = create_oplog();
    let state1 = ChangeId::generate();
    let state2 = ChangeId::generate();
    let state3 = ChangeId::generate();

    let first = oplog
        .record_snapshot(&state1, None, None, Some("lane-a"))
        .unwrap();
    let middle = oplog
        .record_snapshot(&state2, Some(&state1), None, Some("lane-a"))
        .unwrap();
    let last = oplog
        .record_snapshot(&state3, Some(&state2), None, Some("lane-a"))
        .unwrap();

    oplog.coalesce_batches(first, last).unwrap();

    let batches = oplog.recent_batches_scoped(2, Some("lane-a")).unwrap();

    assert_eq!(
        batches.iter().map(|batch| batch.id).collect::<Vec<_>>(),
        vec![first, middle]
    );
    assert_eq!(
        batches[0]
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec![first, last]
    );
    assert_eq!(
        batches[0]
            .entries
            .iter()
            .map(|entry| entry.batch_index)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );

    let limited = oplog.recent_batches_scoped(1, Some("lane-a")).unwrap();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].id, first);
    assert_eq!(limited[0].entries.len(), 2);
}

#[test]
fn committed_batch_records_uses_rebuilt_index_for_non_adjacent_coalesced_batch() {
    let (_temp, oplog) = create_oplog();
    let committed_state = ChangeId::generate();
    let later_state = ChangeId::generate();
    let middle_state = ChangeId::generate();

    let committed = oplog
        .record_batch_exactly_once(
            vec![
                OpRecord::Snapshot {
                    new_state: committed_state,
                    prev_head: None,
                    head: Some(committed_state),
                    thread: None,
                },
                OpRecord::TransactionCommit {
                    transaction_id: "tx-coalesced".into(),
                    op_count: 1,
                },
            ],
            Some("lane-a"),
            "tx-coalesced",
        )
        .unwrap()
        .unwrap();
    let middle = oplog
        .record_snapshot(&middle_state, Some(&committed_state), None, Some("lane-a"))
        .unwrap();
    let later = oplog
        .record_snapshot(&later_state, Some(&middle_state), None, Some("lane-a"))
        .unwrap();

    oplog.coalesce_batches(committed[0], later).unwrap();

    let records = oplog.committed_batch_records("tx-coalesced").unwrap();
    let recovered = records
        .iter()
        .filter_map(|record| match record {
            OpRecord::Snapshot { new_state, .. } => Some(*new_state),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(middle, committed[0] + 2, "precondition: non-adjacent batch");
    assert_eq!(recovered, vec![committed_state, later_state]);
}

#[test]
fn test_oplogbackend_trait_async_methods_dispatch() {
    // The CLI calls OpLog's inherent (sync) batch methods; the generic
    // backend plumbing and the hosted server reach the async trait
    // surface. Drive the trait methods explicitly so the
    // `impl OpLogBackend for OpLog` async bodies and the non-scoped trait
    // defaults that delegate to them are covered.
    let (_temp, oplog) = create_oplog();
    let state1 = ChangeId::generate();
    let state2 = ChangeId::generate();
    oplog
        .record_snapshot(&state1, None, None, Some("lane-a"))
        .unwrap();
    oplog
        .record_snapshot(&state2, Some(&state1), None, Some("lane-a"))
        .unwrap();

    // Non-scoped trait defaults delegate to the *_scoped overrides.
    let recent = pollster::block_on(OpLogBackend::recent_batches(&oplog, 2)).unwrap();
    assert_eq!(recent.len(), 2);
    let undo = pollster::block_on(OpLogBackend::undo_batches(&oplog, 2)).unwrap();
    assert_eq!(undo.len(), 2);
    oplog.mark_batch_undone(&undo[0]).unwrap();
    let redo = pollster::block_on(OpLogBackend::redo_batches(&oplog, 2)).unwrap();
    assert_eq!(redo.len(), 1);

    // OpLog's override of the dedup'd transaction commit: first append
    // records, the second call with the same transaction id is deduped.
    let tx_op = || {
        vec![OpRecord::TransactionCommit {
            transaction_id: "tx-1".to_string(),
            op_count: 0,
        }]
    };
    let first = pollster::block_on(OpLogBackend::record_batch_scoped_if_no_transaction(
        &oplog,
        tx_op(),
        Some("lane-a"),
        "tx-1",
        16,
    ))
    .unwrap();
    assert!(first.is_some());
    let dup = pollster::block_on(OpLogBackend::record_batch_scoped_if_no_transaction(
        &oplog,
        tx_op(),
        Some("lane-a"),
        "tx-1",
        16,
    ))
    .unwrap();
    assert!(dup.is_none());
}

/// The unbounded `record_batch_exactly_once` dedups a retry even after far
/// more intervening commits than any fixed window — the property
/// `record_batch_scoped_if_no_transaction`'s window cannot give
/// (heddle#330 §2.2 "Idempotency of the commit").
#[test]
fn record_batch_exactly_once_dedups_past_any_window() {
    let (_temp, oplog) = create_oplog();
    let commit_ops = || {
        vec![OpRecord::TransactionCommit {
            transaction_id: "tx-delayed".to_string(),
            op_count: 0,
        }]
    };

    // First commit lands.
    let first = oplog
        .record_batch_exactly_once(commit_ops(), Some("lane"), "tx-delayed")
        .unwrap();
    assert!(first.is_some(), "first commit must append");

    // Bury it under 200 unrelated batches — far past any plausible
    // recent-window the bounded helper would scan.
    for _ in 0..200 {
        oplog
            .record_snapshot(&ChangeId::generate(), None, None, Some("lane"))
            .unwrap();
    }

    // A delayed retry still finds the original commit and refuses to
    // double-append — exact-once regardless of how much aged out.
    let retry = oplog
        .record_batch_exactly_once(commit_ops(), Some("lane"), "tx-delayed")
        .unwrap();
    assert!(retry.is_none(), "delayed retry must dedup to None");

    // A distinct transaction id still commits.
    let other = oplog
        .record_batch_exactly_once(
            vec![OpRecord::TransactionCommit {
                transaction_id: "tx-other".to_string(),
                op_count: 0,
            }],
            Some("lane"),
            "tx-other",
        )
        .unwrap();
    assert!(other.is_some(), "a different transaction id must commit");
}

/// Finding 1 (heddle#354 r6, cid 3329711888) — the dedup-hit output
/// reconstruction reads a FRESH oplog, not a long-lived handle's stale cache.
/// Process A commits tx T; process B (a separate handle whose cache was
/// populated *before* T) replays T: the commit dedups to `None` AND
/// `committed_batch_records` recovers the batch A actually committed via a
/// refresh — not an empty stale-cache miss.
#[test]
fn committed_batch_records_refreshes_for_cross_process_dedup_hit() {
    let temp = TempDir::new().unwrap();
    let heddle_dir = temp.path().join(".heddle");
    std::fs::create_dir_all(&heddle_dir).unwrap();

    // Two independent handles on the same on-disk oplog — two "processes",
    // each with its own in-memory cache.
    let proc_a = OpLog::new_unattributed(&heddle_dir);
    let proc_b = OpLog::new_unattributed(&heddle_dir);
    proc_a.init().unwrap();

    // B reads first, populating its cache with the pre-T (empty) state.
    let before = proc_b.recent(10).unwrap();
    assert!(
        before.iter().all(|e| !matches!(
            &e.operation,
            OpRecord::TransactionCommit { transaction_id, .. } if transaction_id == "tx-shared"
        )),
        "precondition: B's cache must not yet contain the shared tx"
    );

    // A commits the transaction with a distinguishable Snapshot payload.
    let committed_state = ChangeId::generate();
    let appended = proc_a
        .record_batch_exactly_once(
            vec![
                OpRecord::Snapshot {
                    new_state: committed_state,
                    prev_head: None,
                    head: Some(committed_state),
                    thread: None,
                },
                OpRecord::TransactionCommit {
                    transaction_id: "tx-shared".into(),
                    op_count: 1,
                },
            ],
            Some("lane"),
            "tx-shared",
        )
        .unwrap();
    assert!(appended.is_some(), "A's commit must append");

    // B replays the same transaction id: a cross-process dedup hit. B's
    // regenerated Snapshot id differs from A's, so a stale reconstruction
    // would diverge even if it found anything.
    let replay_state = ChangeId::generate();
    let replay = proc_b
        .record_batch_exactly_once(
            vec![
                OpRecord::Snapshot {
                    new_state: replay_state,
                    prev_head: None,
                    head: Some(replay_state),
                    thread: None,
                },
                OpRecord::TransactionCommit {
                    transaction_id: "tx-shared".into(),
                    op_count: 1,
                },
            ],
            Some("lane"),
            "tx-shared",
        )
        .unwrap();
    assert!(replay.is_none(), "cross-process replay must dedup to None");

    // The reconstruction must read the FRESH oplog and recover A's record,
    // even though B's cache predates A's commit. Pre-fix (stale `load_cached`)
    // this returned an empty vec.
    let prior = proc_b.committed_batch_records("tx-shared").unwrap();
    let recovered: Vec<_> = prior
        .iter()
        .filter_map(|op| match op {
            OpRecord::Snapshot { new_state, .. } => Some(*new_state),
            _ => None,
        })
        .collect();
    assert_eq!(
        recovered,
        vec![committed_state],
        "B reconstructs the batch A committed (via refresh), not a stale miss"
    );
}

/// rmp-serde round-trips every `OpRecord` variant — the on-disk encoding
/// the oplog actually uses. Asserts the variant survives encode→decode
/// unchanged (catches a reordered/inserted discriminant) and that
/// `description()` produces a non-empty line for each. The new heddle#330
/// tail variants (`RemoteThreadUpdate` / `RemoteThreadDelete` /
/// `UndoRecoveryUpdate`) and the Fork/Collapse published-ref retrofit are
/// the load-bearing cases.
#[test]
fn op_record_variants_roundtrip_and_describe() {
    let st = ChangeId::from_bytes([3u8; 16]);
    let st2 = ChangeId::from_bytes([5u8; 16]);
    let blob = ContentHash::from_bytes([7u8; 32]);
    let redaction = ContentHash::from_bytes([9u8; 32]);

    let records = vec![
        OpRecord::Snapshot {
            new_state: st,
            prev_head: Some(st2),
            head: None,
            thread: Some("main".into()),
        },
        OpRecord::Snapshot {
            new_state: st,
            prev_head: None,
            head: Some(st),
            thread: None,
        },
        OpRecord::Goto {
            target: st,
            prev_head: Some(st2),
            head: st,
        },
        OpRecord::ThreadCreate {
            name: "feat".into(),
            state: st,
            manager_snapshot: Some(vec![1, 2, 3]),
        },
        OpRecord::ThreadDelete {
            name: "feat".into(),
            state: st,
        },
        OpRecord::ThreadUpdate {
            name: "feat".into(),
            old_state: st2,
            new_state: st,
            manager_snapshots: None,
        },
        // Fork retrofit: both published-ref shapes (attached thread / detached head).
        OpRecord::Fork {
            from: st2,
            new_state: st,
            thread: Some("topic".into()),
            head: None,
        },
        OpRecord::Fork {
            from: st2,
            new_state: st,
            thread: None,
            head: Some(st),
        },
        // Collapse retrofit: published thread ref / detached head.
        OpRecord::Collapse {
            sources: vec![st, st2],
            result: st,
            thread: Some("trunk".into()),
        },
        OpRecord::Collapse {
            sources: vec![st],
            result: st2,
            thread: None,
        },
        OpRecord::MarkerCreate {
            name: "v1".into(),
            state: st,
        },
        OpRecord::MarkerDelete {
            name: "v1".into(),
            state: st,
        },
        OpRecord::Checkpoint {
            parent: Some(st2),
            state: st,
            thread: Some("agent".into()),
        },
        OpRecord::Checkpoint {
            parent: None,
            state: st,
            thread: None,
        },
        OpRecord::TransactionAbort {
            transaction_id: "tx".into(),
            reason: "conflict".into(),
        },
        OpRecord::EphemeralThreadCollapse {
            thread: "scratch".into(),
            final_state: st,
        },
        OpRecord::ConflictResolved {
            conflict_id: "c1".into(),
            resolution: "ours".into(),
        },
        OpRecord::TransactionCommit {
            transaction_id: "tx".into(),
            op_count: 3,
        },
        OpRecord::Redact {
            redaction_id: redaction,
            blob,
            state: st,
            path: "secret.txt".into(),
        },
        OpRecord::Purge {
            redaction_id: redaction,
            blob,
        },
        OpRecord::FastForward {
            source_thread: "topic".into(),
            target_thread: "main".into(),
            pre_target_id: st2,
            post_target_id: st,
        },
        OpRecord::GitCheckpoint {
            branch: "main".into(),
            state: st,
            previous_git_oid: Some("abc".into()),
            new_git_oid: "def".into(),
        },
        OpRecord::GitCheckpoint {
            branch: "main".into(),
            state: st,
            previous_git_oid: None,
            new_git_oid: "def".into(),
        },
        // heddle#330 r9 tail variants.
        OpRecord::RemoteThreadUpdate {
            remote: "origin".into(),
            thread: "main".into(),
            state: st,
        },
        OpRecord::RemoteThreadDelete {
            remote: "origin".into(),
            thread: "main".into(),
            state: st,
        },
        OpRecord::UndoRecoveryUpdate { state: st },
    ];

    for rec in &records {
        assert!(
            !rec.description().is_empty(),
            "description must be non-empty for {rec:?}"
        );
        let bytes = rmp_serde::to_vec(rec).expect("encode");
        let back: OpRecord = rmp_serde::from_slice(&bytes).expect("decode");
        // Re-encode the decoded value; identical bytes ⇒ structural round-trip.
        let bytes2 = rmp_serde::to_vec(&back).expect("re-encode");
        assert_eq!(bytes, bytes2, "round-trip mismatch for {rec:?}");
        assert_eq!(back.description(), rec.description());
    }
}

/// The Fork/Collapse published-ref fields default to `None` when absent —
/// pre-retrofit records (encoded without `thread`/`head`) must still
/// deserialize via `#[serde(default)]`. Round-trip the retrofit fields and
/// assert they survive.
#[test]
fn fork_collapse_published_ref_fields_roundtrip() {
    let from = ChangeId::from_bytes([1u8; 16]);
    let new_state = ChangeId::from_bytes([2u8; 16]);

    let fork = OpRecord::Fork {
        from,
        new_state,
        thread: Some("published".into()),
        head: Some(new_state),
    };
    let back: OpRecord = rmp_serde::from_slice(&rmp_serde::to_vec(&fork).unwrap()).unwrap();
    match back {
        OpRecord::Fork {
            from: f,
            new_state: n,
            thread,
            head,
        } => {
            assert_eq!(f, from);
            assert_eq!(n, new_state);
            assert_eq!(thread.as_deref(), Some("published"));
            assert_eq!(head, Some(new_state));
        }
        other => panic!("expected Fork, got {other:?}"),
    }

    let collapse = OpRecord::Collapse {
        sources: vec![from, new_state],
        result: new_state,
        thread: Some("trunk".into()),
    };
    let back: OpRecord = rmp_serde::from_slice(&rmp_serde::to_vec(&collapse).unwrap()).unwrap();
    match back {
        OpRecord::Collapse {
            sources,
            result,
            thread,
        } => {
            assert_eq!(sources, vec![from, new_state]);
            assert_eq!(result, new_state);
            assert_eq!(thread.as_deref(), Some("trunk"));
        }
        other => panic!("expected Collapse, got {other:?}"),
    }
}

/// Exercises every recording method on `OpLog` (the `oplog_records.rs`
/// surface) and reads the persisted variant back. Confirms `record_fork`
/// writes its args in the documented order (`from = source`) and that the
/// published-ref fields land on the stored record.
#[test]
fn record_methods_persist_expected_variants() {
    let (_temp, oplog) = create_oplog();
    let from = ChangeId::generate();
    let result = ChangeId::generate();
    let blob = ContentHash::from_bytes([7u8; 32]);
    let redaction = ContentHash::from_bytes([9u8; 32]);

    oplog
        .record_goto(&result, Some(&from), Some("lane"))
        .unwrap();
    oplog
        .record_thread_create(
            &ThreadName::new("feat"),
            &result,
            Some(vec![9, 8, 7]),
            Some("lane"),
        )
        .unwrap();
    oplog
        .record_thread_delete(&ThreadName::new("legacy"), &result, None)
        .unwrap();
    let rename_ids = oplog
        .record_thread_rename(
            &ThreadName::new("old"),
            &ThreadName::new("new"),
            &result,
            Some("lane"),
        )
        .unwrap();
    assert_eq!(rename_ids.len(), 2);
    oplog
        .record_fork(&from, &result, Some("topic"), None, None)
        .unwrap();
    oplog
        .record_collapse(&[from, result], &result, Some("trunk"), None)
        .unwrap();
    oplog
        .record_marker_create(&MarkerName::new("v1"), &result)
        .unwrap();
    oplog
        .record_marker_delete(&MarkerName::new("v1"), &result)
        .unwrap();
    oplog
        .record_redact(&redaction, &blob, &result, "secret.txt", Some("lane"))
        .unwrap();
    oplog.record_purge(&redaction, &blob, Some("lane")).unwrap();
    oplog
        .record_fast_forward(
            &ThreadName::new("topic"),
            &ThreadName::new("main"),
            &from,
            &result,
            Some("lane"),
        )
        .unwrap();

    // record_fork must store `from` as the source state, not the result.
    let entries = oplog.recent(64).unwrap();
    let fork = entries
        .iter()
        .find_map(|e| match &e.operation {
            OpRecord::Fork {
                from: f,
                new_state,
                thread,
                head,
            } => Some((*f, *new_state, thread.clone(), *head)),
            _ => None,
        })
        .expect("a Fork record was written");
    assert_eq!(
        fork.0, from,
        "record_fork's `from` must be the source state"
    );
    assert_eq!(fork.1, result);
    assert_eq!(fork.2.as_deref(), Some("topic"));
    assert_eq!(fork.3, None);

    // A ThreadCreate is always emitted with the snapshot.
    let created = entries.iter().find_map(|e| match &e.operation {
        OpRecord::ThreadCreate {
            name,
            manager_snapshot,
            ..
        } if name == "feat" => Some(manager_snapshot.clone()),
        _ => None,
    });
    assert_eq!(created, Some(Some(vec![9u8, 8, 7])));

    // The fast-forward always carries post_target_id.
    assert!(entries.iter().any(|e| matches!(
        &e.operation,
        OpRecord::FastForward { post_target_id, .. } if *post_target_id == result
    )));
}

/// `head_id()` reads the fixed-size header generation gate: 0 before any
/// write (and for a not-yet-created log), then the last appended id.
#[test]
fn head_id_tracks_generation() {
    let temp = TempDir::new().unwrap();
    let heddle_dir = temp.path().join(".heddle");
    std::fs::create_dir_all(&heddle_dir).unwrap();
    let oplog = OpLog::new_unattributed(&heddle_dir);

    // Not-yet-initialized oplog reads as generation 0.
    assert_eq!(oplog.head_id().unwrap(), 0);

    oplog.init().unwrap();
    assert_eq!(oplog.head_id().unwrap(), 0);

    oplog
        .record_snapshot(&ChangeId::generate(), None, None, Some("lane"))
        .unwrap();
    assert_eq!(oplog.head_id().unwrap(), 1);

    let ids = oplog
        .record_batch(vec![
            OpRecord::MarkerCreate {
                name: "v1".into(),
                state: ChangeId::generate(),
            },
            OpRecord::MarkerDelete {
                name: "v1".into(),
                state: ChangeId::generate(),
            },
        ])
        .unwrap();
    assert_eq!(oplog.head_id().unwrap(), *ids.last().unwrap());
}

/// `record_batch_exactly_once` returns `Ok(Some(empty))` for an empty op
/// list without writing, and commits two *distinct* transaction ids
/// independently (the no-collision path).
#[test]
fn record_batch_exactly_once_empty_and_distinct_ids() {
    let (_temp, oplog) = create_oplog();

    let empty = oplog
        .record_batch_exactly_once(Vec::new(), Some("lane"), "tx-empty")
        .unwrap();
    assert_eq!(empty, Some(Vec::new()));
    assert_eq!(oplog.head_id().unwrap(), 0, "empty commit writes nothing");

    let a = oplog
        .record_batch_exactly_once(
            vec![OpRecord::TransactionCommit {
                transaction_id: "tx-a".into(),
                op_count: 1,
            }],
            Some("lane"),
            "tx-a",
        )
        .unwrap();
    let b = oplog
        .record_batch_exactly_once(
            vec![OpRecord::TransactionCommit {
                transaction_id: "tx-b".into(),
                op_count: 1,
            }],
            Some("lane"),
            "tx-b",
        )
        .unwrap();
    assert!(a.is_some() && b.is_some());
    assert_ne!(a, b, "distinct transaction ids commit independently");
}

/// Covers the **default** (non-atomic) `record_batch_scoped_if_no_transaction`
/// on the trait. `OpLog` overrides it, so a backend that keeps the default
/// is the only way to exercise that fallback body.
mod default_backend {
    use std::sync::{Arc, Mutex};

    use chrono::Utc;
    use objects::error::Result;
    use objects::object::Principal;

    use super::super::oplog_types::{OpBatch, OpEntry, OpRecord};
    use super::OpLogBackend;

    #[derive(Default)]
    struct MemOpLog {
        batches: Mutex<Vec<OpBatch>>,
        next_id: Mutex<u64>,
    }

    impl OpLogBackend for MemOpLog {
        fn record_batch_scoped(
            &self,
            operations: Vec<OpRecord>,
            scope: Option<&str>,
        ) -> Result<Vec<u64>> {
            let mut next = self.next_id.lock().unwrap();
            let batch_id = *next + 1;
            let mut ids = Vec::new();
            let mut entries = Vec::new();
            for (i, op) in operations.into_iter().enumerate() {
                *next += 1;
                ids.push(*next);
                entries.push(OpEntry {
                    id: *next,
                    timestamp: Utc::now(),
                    operation: op,
                    undone: false,
                    batch_id,
                    batch_index: i as u32,
                    scope: scope.map(str::to_string),
                    actor: Arc::new(Principal::new("test", "test@example.com")),
                    operation_id: None,
                });
            }
            self.batches.lock().unwrap().push(OpBatch {
                id: batch_id,
                entries,
            });
            Ok(ids)
        }

        fn last(&self) -> Result<Option<OpEntry>> {
            Ok(None)
        }
        fn recent(&self, _count: usize) -> Result<Vec<OpEntry>> {
            Ok(Vec::new())
        }
        async fn recent_batches_scoped(
            &self,
            count: usize,
            _scope: Option<&str>,
        ) -> Result<Vec<OpBatch>> {
            Ok(self
                .batches
                .lock()
                .unwrap()
                .iter()
                .rev()
                .take(count)
                .cloned()
                .collect())
        }
        async fn undo_batches_scoped(
            &self,
            _count: usize,
            _scope: Option<&str>,
        ) -> Result<Vec<OpBatch>> {
            Ok(Vec::new())
        }
        async fn redo_batches_scoped(
            &self,
            _count: usize,
            _scope: Option<&str>,
        ) -> Result<Vec<OpBatch>> {
            Ok(Vec::new())
        }
        fn mark_batch_undone(&self, batch: &OpBatch) -> Result<OpBatch> {
            Ok(batch.clone())
        }
        fn mark_batch_redone(&self, batch: &OpBatch) -> Result<OpBatch> {
            Ok(batch.clone())
        }
        // `record_batch_scoped_if_no_transaction` left at the trait default.
    }

    #[test]
    fn default_dedup_appends_then_skips() {
        let backend = MemOpLog::default();
        let ops = || {
            vec![OpRecord::TransactionCommit {
                transaction_id: "tx-9".to_string(),
                op_count: 1,
            }]
        };

        // Nothing recorded yet → the default appends and returns the ids.
        let first = pollster::block_on(backend.record_batch_scoped_if_no_transaction(
            ops(),
            Some("scope"),
            "tx-9",
            16,
        ))
        .unwrap();
        assert!(first.is_some());

        // tx-9 now appears in the recent window → deduped to None.
        let second = pollster::block_on(backend.record_batch_scoped_if_no_transaction(
            ops(),
            Some("scope"),
            "tx-9",
            16,
        ))
        .unwrap();
        assert!(second.is_none());
    }

    /// The `record_*` convenience wrappers on `OpLogBackend` are *default*
    /// trait methods. On `OpLog` they are shadowed by the inherent
    /// `oplog_records.rs` methods, so a backend that does NOT redefine them
    /// (the `MemOpLog` mock, like the Postgres backend) is the only way to
    /// drive the default bodies. Calls each through the trait and asserts the
    /// expected variant landed in a recorded batch.
    #[test]
    fn default_record_wrappers_emit_expected_variants() {
        use objects::object::{ChangeId, ContentHash, MarkerName, ThreadName};

        let backend = MemOpLog::default();
        let from = ChangeId::from_bytes([1u8; 16]);
        let cid = ChangeId::from_bytes([2u8; 16]);
        let blob = ContentHash::from_bytes([7u8; 32]);
        let redaction = ContentHash::from_bytes([9u8; 32]);

        // `record_batch` default delegates to `record_batch_scoped(_, None)`.
        backend
            .record_batch(vec![OpRecord::Goto {
                target: cid,
                prev_head: Some(from),
                head: cid,
            }])
            .unwrap();

        backend
            .record_snapshot(&cid, Some(&from), Some("thread"), Some("s"))
            .unwrap();
        backend.record_goto(&cid, Some(&from), Some("s")).unwrap();
        backend
            .record_thread_create(&ThreadName::new("ft"), &cid, Some(vec![1, 2, 3]), Some("s"))
            .unwrap();
        backend
            .record_thread_delete(&ThreadName::new("ft"), &cid, Some("s"))
            .unwrap();
        let rename_ids = backend
            .record_thread_rename(
                &ThreadName::new("old"),
                &ThreadName::new("new"),
                &cid,
                Some("s"),
            )
            .unwrap();
        assert_eq!(rename_ids.len(), 2, "rename emits create + delete");
        backend
            .record_fork(&from, &cid, Some("topic"), Some(&cid), Some("s"))
            .unwrap();
        backend
            .record_collapse(&[from, cid], &cid, Some("trunk"), Some("s"))
            .unwrap();
        backend
            .record_remote_thread_update("origin", "rt", &cid, Some("s"))
            .unwrap();
        backend
            .record_remote_thread_delete("origin", "rt", &cid, Some("s"))
            .unwrap();
        backend
            .record_undo_recovery_update(&cid, Some("s"))
            .unwrap();
        backend
            .record_marker_create(&MarkerName::new("v1"), &cid)
            .unwrap();
        backend
            .record_marker_delete(&MarkerName::new("v1"), &cid)
            .unwrap();
        backend
            .record_redact(&redaction, &blob, &cid, "secret.txt", Some("s"))
            .unwrap();
        backend.record_purge(&redaction, &blob, Some("s")).unwrap();
        backend
            .record_fast_forward(
                &ThreadName::new("topic"),
                &ThreadName::new("main"),
                &from,
                &cid,
                Some("s"),
            )
            .unwrap();

        // `coalesce_batches` default is fail-closed.
        assert!(
            backend.coalesce_batches(1, 2).is_err(),
            "default coalesce must refuse"
        );

        // Every wrapper appended through `record_batch_scoped`; spot-check the
        // distinctive published-ref variants made it into the recorded batches.
        let batches = pollster::block_on(backend.recent_batches(256)).unwrap();
        let ops: Vec<&OpRecord> = batches
            .iter()
            .flat_map(|b| b.entries.iter().map(|e| &e.operation))
            .collect();
        assert!(ops.iter().any(|o| matches!(
            o,
            OpRecord::Fork { from: f, new_state, thread, head }
                if *f == from && *new_state == cid && thread.as_deref() == Some("topic") && *head == Some(cid)
        )));
        assert!(ops.iter().any(|o| matches!(
            o,
            OpRecord::ThreadCreate { name, manager_snapshot, .. }
                if name == "ft" && manager_snapshot.as_deref() == Some(&[1u8, 2, 3][..])
        )));
        assert!(ops.iter().any(|o| matches!(
            o,
            OpRecord::RemoteThreadUpdate { remote, thread, .. } if remote == "origin" && thread == "rt"
        )));
        assert!(
            ops.iter()
                .any(|o| matches!(o, OpRecord::UndoRecoveryUpdate { .. }))
        );
        assert!(ops.iter().any(|o| matches!(
            o,
            OpRecord::FastForward { post_target_id, .. } if *post_target_id == cid
        )));
    }
}
