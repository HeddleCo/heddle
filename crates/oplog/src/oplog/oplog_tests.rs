// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::BTreeSet,
    sync::{Arc, Barrier},
    thread,
};

use objects::object::ChangeId;
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

/// Covers the **default** (non-atomic) `record_batch_scoped_if_no_transaction`
/// on the trait. `OpLog` overrides it, so a backend that keeps the default
/// is the only way to exercise that fallback body.
mod default_backend {
    use std::sync::{Arc, Mutex};

    use chrono::Utc;
    use objects::error::Result;
    use objects::object::Principal;

    use super::OpLogBackend;
    use super::super::oplog_types::{OpBatch, OpEntry, OpRecord};

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
}
