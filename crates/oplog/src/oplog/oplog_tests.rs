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
