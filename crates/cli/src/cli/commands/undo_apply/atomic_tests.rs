// SPDX-License-Identifier: Apache-2.0
//! Atomic undo/redo apply tests for the `undo`/`apply` command surface.
//!
//! Extracted verbatim from `undo_apply.rs` (heddle#609 phase 3): the
//! `mod atomic_tests` body moved into this sibling file unchanged (de-indented
//! one level). It referenced the parent via `super::*` inline and continues
//! to do so as a sibling module -- pure code movement, no logic change.
//!
//! Gated by the `#[cfg(test)] mod atomic_tests;` declaration in the parent module.

use oplog::ThreadUpdateSnapshots;
use tempfile::TempDir;

use super::*;

/// Init a repo and create two snapshots on `main`. The worktree at `s2`
/// holds both `a.txt` (from `s1`) and `b.txt` (from `s2`); `s1` holds only
/// `a.txt`; the initial state holds neither. Returns the repo + temp dir +
/// the two states.
fn repo_with_two_snapshots() -> (TempDir, Repository, StateId, StateId) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    std::fs::write(temp.path().join("a.txt"), "a").unwrap();
    let s1 = repo.snapshot(Some("s1".to_string()), None).unwrap();
    std::fs::write(temp.path().join("b.txt"), "b").unwrap();
    let s2 = repo.snapshot(Some("s2".to_string()), None).unwrap();
    (temp, repo, s1.state_id, s2.state_id)
}

#[test]
fn apply_error_wraps_anyhow_into_conflict() {
    let wrapped = apply_error(anyhow!("boom"));
    assert!(
        matches!(&wrapped, HeddleError::Conflict(message) if message.contains("boom")),
        "an apply-helper error must surface as a HeddleError::Conflict carrying the message"
    );
}

fn commit_marker_count(repo: &Repository) -> usize {
    repo.oplog()
        .recent(256)
        .unwrap()
        .iter()
        .filter(|entry| matches!(entry.operation, OpRecord::TransactionCommit { .. }))
        .count()
}

fn commit_marker_count_for(repo: &Repository, txid: &str) -> usize {
    repo.oplog()
        .recent(256)
        .unwrap()
        .iter()
        .filter(|entry| {
            matches!(
                &entry.operation,
                OpRecord::TransactionCommit { transaction_id, .. } if transaction_id == txid
            )
        })
        .count()
}

fn main_thread(repo: &Repository) -> Option<StateId> {
    repo.refs().get_thread(&ThreadName::new("main")).unwrap()
}

/// Test-only parent mirroring [`UndoOp`] but injecting a fault: the LAST
/// enrolled batch child fails after undoing `fail_after` of its entries.
/// Reuses the REAL [`StageUndoRecovery`] + [`ApplyUndoBatch`] children, so
/// it exercises the real compensators + nesting + rewind path.
struct FaultyUndo {
    batches: Vec<OpBatch>,
    recovery_head: Option<StateId>,
    fail_after: usize,
}

impl AtomicMutation for FaultyUndo {
    type Output = ();

    fn transaction_id(&self) -> String {
        "test-undo-fault".to_string()
    }

    fn isolation_keys(&self, repo: &Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
        Ok(isolation_keys_for_batches(&self.batches, &repo.op_scope()))
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<()>> {
        tx.enroll(StageUndoRecovery {
            head: self.recovery_head,
        })?;
        let last = self.batches.len() - 1;
        for (i, batch) in self.batches.iter().enumerate() {
            if i == last {
                tx.enroll(ApplyUndoBatch::failing_after(
                    batch.clone(),
                    self.fail_after,
                ))?;
            } else {
                tx.enroll(ApplyUndoBatch::new(batch.clone()))?;
            }
        }
        Ok(StagedCommit::pure(()))
    }
}

/// Test-only parent mirroring [`RedoOp`] with an injected fault on the last
/// enrolled batch child.
struct FaultyRedo {
    batches: Vec<OpBatch>,
    fail_after: usize,
}

impl AtomicMutation for FaultyRedo {
    type Output = ();

    fn transaction_id(&self) -> String {
        "test-redo-fault".to_string()
    }

    fn isolation_keys(&self, repo: &Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
        Ok(isolation_keys_for_batches(&self.batches, &repo.op_scope()))
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<()>> {
        let last = self.batches.len() - 1;
        for (i, batch) in self.batches.iter().enumerate() {
            if i == last {
                tx.enroll(ApplyRedoBatch::failing_after(
                    batch.clone(),
                    self.fail_after,
                ))?;
            } else {
                tx.enroll(ApplyRedoBatch::new(batch.clone()))?;
            }
        }
        Ok(StagedCommit::pure(()))
    }
}

/// Behavioral parity: a clean atomic `UndoOp` reverts the worktree, HEAD,
/// and thread ref, marks the batch undone, captures the recovery pointer,
/// and commits exactly one marker — same observable result as the
/// pre-migration sequential path.
#[test]
fn atomic_undo_success_reverts_and_records_recovery() {
    let (temp, repo, s1, s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();

    let recovery_head = repo.head().unwrap();
    let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
    let updated =
        repo::atomic::execute(&repo, UndoOp::new(batches, recovery_head, txid.clone())).unwrap();

    assert_eq!(updated.len(), 1);
    assert!(updated[0].entries.iter().all(|e| e.undone));
    assert_eq!(repo.head().unwrap(), Some(s1), "HEAD reverted to s1");
    assert_eq!(main_thread(&repo), Some(s1));
    assert!(temp.path().join("a.txt").exists(), "s1 file kept");
    assert!(!temp.path().join("b.txt").exists(), "s2 file reverted");
    assert_eq!(
        repo.refs().get_undo_recovery().unwrap(),
        Some(s2),
        "recovery pointer pins the pre-undo tip"
    );
    assert_eq!(
        commit_marker_count_for(&repo, &txid),
        1,
        "exactly one undo commit marker"
    );
}

/// Fault-injection: a failure mid-undo (after the first batch is fully
/// applied, partway into the second) rewinds EVERY applied step back to the
/// exact pre-operation state — no partial ref / oplog / worktree leak.
#[test]
fn fault_mid_undo_rewinds_to_pre_operation_state() {
    let (temp, repo, _s1, s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();

    let pre_head = repo.head().unwrap();
    assert_eq!(pre_head, Some(s2));
    let pre_main = main_thread(&repo);
    assert_eq!(repo.refs().get_undo_recovery().unwrap(), None);
    let pre_markers = commit_marker_count(&repo);

    let batches = repo.oplog().undo_batches_scoped(2, Some(&scope)).unwrap();
    assert_eq!(batches.len(), 2, "two snapshots are undoable");
    let result = repo::atomic::execute(
        &repo,
        FaultyUndo {
            batches,
            recovery_head: pre_head,
            fail_after: 1,
        },
    );
    assert!(result.is_err(), "the injected fault must fail the undo");

    // Exact pre-operation state restored across every dimension.
    assert_eq!(
        repo.head().unwrap(),
        Some(s2),
        "HEAD rewound to pre-undo tip"
    );
    assert_eq!(main_thread(&repo), pre_main, "main ref rewound");
    assert!(temp.path().join("a.txt").exists(), "s1 file restored");
    assert!(temp.path().join("b.txt").exists(), "s2 file restored");
    assert_eq!(
        repo.oplog()
            .undo_batches_scoped(2, Some(&scope))
            .unwrap()
            .len(),
        2,
        "no batch left marked undone"
    );
    assert_eq!(
        repo.refs().get_undo_recovery().unwrap(),
        None,
        "recovery pointer cleared by rewind (it had no prior value)"
    );
    assert_eq!(
        commit_marker_count(&repo),
        pre_markers,
        "a failed transaction commits no marker"
    );
}

/// Fault-injection: a failure mid-redo rewinds the replay back to the
/// fully-undone pre-redo state — no partial effect leaks.
#[test]
fn fault_mid_redo_rewinds_to_pre_operation_state() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    std::fs::write(temp.path().join("a.txt"), "a").unwrap();
    let _s1 = repo.snapshot(Some("s1".to_string()), None).unwrap();
    let scope = repo.op_scope();

    // Cleanly undo the single snapshot through the real atomic UndoOp.
    let recovery_head = repo.head().unwrap();
    let undo_batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("undo", &scope, generation, &undo_batches);
    repo::atomic::execute(&repo, UndoOp::new(undo_batches, recovery_head, txid)).unwrap();

    // Pre-redo state: the initial (pre-s1) state — a.txt gone, one batch
    // redoable.
    assert!(!temp.path().join("a.txt").exists(), "undone: a.txt gone");
    let pre_redo_head = repo.head().unwrap();
    let pre_redo_main = main_thread(&repo);
    assert_eq!(
        repo.oplog()
            .redo_batches_scoped(1, Some(&scope))
            .unwrap()
            .len(),
        1,
        "one batch is redoable"
    );
    let pre_markers = commit_marker_count(&repo);

    let redo_batches = repo.oplog().redo_batches_scoped(1, Some(&scope)).unwrap();
    let result = repo::atomic::execute(
        &repo,
        FaultyRedo {
            batches: redo_batches,
            fail_after: 1,
        },
    );
    assert!(result.is_err(), "the injected fault must fail the redo");

    // Rewound to the fully-undone pre-redo state.
    assert_eq!(repo.head().unwrap(), pre_redo_head, "HEAD rewound");
    assert_eq!(main_thread(&repo), pre_redo_main, "main ref rewound");
    assert!(
        !temp.path().join("a.txt").exists(),
        "s1 file not resurrected"
    );
    assert_eq!(
        repo.oplog()
            .redo_batches_scoped(1, Some(&scope))
            .unwrap()
            .len(),
        1,
        "batch still redoable"
    );
    assert_eq!(
        commit_marker_count(&repo),
        pre_markers,
        "a failed transaction commits no marker"
    );
}

/// Per-effect rollback, UNDO direction (heddle#355 cid 3330966930). A threaded
/// `Snapshot` undo performs several writes — `goto` (moves HEAD + worktree),
/// then the thread-ref / HEAD / record updates. Injecting a failure on the
/// SECOND write, after the goto already moved HEAD + worktree, must roll the
/// goto back too, restoring the EXACT pre-entry state. Under the old
/// whole-entry `step`, the goto leaked: a forward that failed partway had no
/// inverse registered, leaving HEAD/worktree half-rewound.
#[test]
fn per_effect_rollback_threaded_snapshot_undo() {
    let (temp, repo, _s1, s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();

    let pre_head = repo.head().unwrap();
    assert_eq!(pre_head, Some(s2));
    let pre_main = main_thread(&repo);
    let pre_markers = commit_marker_count(&repo);
    assert_eq!(repo.refs().get_undo_recovery().unwrap(), None);

    let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
    // Fail the entry's 2nd per-effect write: the goto (write 1) succeeds and
    // moves HEAD/worktree, then the thread-ref update (write 2) errors.
    let result = with_entry_write_fault(1, || {
        repo::atomic::execute(&repo, UndoOp::new(batches, pre_head, txid))
    });
    assert!(
        result.is_err(),
        "the injected 2nd-write fault must fail the undo"
    );

    // The goto was rolled back along with everything else.
    assert_eq!(
        repo.head().unwrap(),
        Some(s2),
        "HEAD goto rolled back to the pre-undo tip"
    );
    assert_eq!(main_thread(&repo), pre_main, "main ref unchanged");
    assert!(temp.path().join("a.txt").exists(), "s1 file present");
    assert!(
        temp.path().join("b.txt").exists(),
        "s2 file restored by the goto rollback (the per-effect inverse ran)"
    );
    assert_eq!(
        repo.oplog()
            .undo_batches_scoped(1, Some(&scope))
            .unwrap()
            .len(),
        1,
        "no batch left marked undone"
    );
    assert_eq!(
        repo.refs().get_undo_recovery().unwrap(),
        None,
        "recovery pointer cleared by rewind"
    );
    assert_eq!(
        commit_marker_count(&repo),
        pre_markers,
        "no marker committed"
    );
}

/// Per-effect rollback, REDO direction (heddle#355 cid 3330966931). Mirror of
/// the undo case: a threaded `Snapshot` redo's `goto` moves HEAD + worktree,
/// then the 2nd write fails — the goto must roll back to the fully-undone
/// pre-redo state, not leave the s2 worktree material resurrected. The
/// `GitCheckpoint` redo Codex named is the same multi-write class, now routed
/// through the identical per-effect `entry_step` machinery this exercises.
#[test]
fn per_effect_rollback_threaded_snapshot_redo() {
    let (temp, repo, s1, _s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();

    // Cleanly undo s2 so it becomes redoable; pre-redo state is the s1 tip.
    let recovery_head = repo.head().unwrap();
    let undo_batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("undo", &scope, generation, &undo_batches);
    repo::atomic::execute(&repo, UndoOp::new(undo_batches, recovery_head, txid)).unwrap();
    assert_eq!(repo.head().unwrap(), Some(s1), "undone to s1");
    assert!(!temp.path().join("b.txt").exists(), "b.txt gone after undo");

    let pre_redo_head = repo.head().unwrap();
    let pre_redo_main = main_thread(&repo);
    let pre_markers = commit_marker_count(&repo);

    let redo_batches = repo.oplog().redo_batches_scoped(1, Some(&scope)).unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("redo", &scope, generation, &redo_batches);
    let result = with_entry_write_fault(1, || {
        repo::atomic::execute(&repo, RedoOp::new(redo_batches, txid))
    });
    assert!(
        result.is_err(),
        "the injected 2nd-write fault must fail the redo"
    );

    assert_eq!(
        repo.head().unwrap(),
        pre_redo_head,
        "HEAD goto rolled back to the pre-redo (fully-undone) state"
    );
    assert_eq!(main_thread(&repo), pre_redo_main, "main ref unchanged");
    assert!(temp.path().join("a.txt").exists(), "s1 file present");
    assert!(
        !temp.path().join("b.txt").exists(),
        "s2 file NOT resurrected — the goto's per-effect inverse rolled it back"
    );
    assert_eq!(
        repo.oplog()
            .redo_batches_scoped(1, Some(&scope))
            .unwrap()
            .len(),
        1,
        "batch still redoable"
    );
    assert_eq!(
        commit_marker_count(&repo),
        pre_markers,
        "no marker committed"
    );
}

/// Per-effect rollback of marker writes. Undoing a batch whose entries delete
/// one marker (`mc`) and re-create another (`md`) registers a per-effect
/// inverse for each; a later write failing must restore both markers to their
/// exact pre-undo presence (`mc` back, `md` gone again).
#[test]
fn per_effect_rollback_restores_marker_writes() {
    let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();
    // `mc` exists (its MarkerCreate undo = delete_marker, inverse recreate);
    // `md` does not (its MarkerDelete undo = create_marker, inverse delete).
    repo.refs()
        .create_marker(&MarkerName::new("mc"), &s1)
        .unwrap();
    let main_state = main_thread(&repo).unwrap();

    repo.oplog()
        .record_batch_scoped(
            vec![
                OpRecord::ThreadUpdate {
                    name: "main".to_string(),
                    old_state: main_state,
                    new_state: main_state,
                    manager_snapshots: None,
                },
                OpRecord::MarkerCreate {
                    name: "mc".to_string(),
                    state: s1,
                },
                OpRecord::MarkerDelete {
                    name: "md".to_string(),
                    state: s1,
                },
            ],
            Some(&scope),
        )
        .unwrap();

    let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
    let recovery_head = repo.head().unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
    // Undo order (entries.rev()): create `md` [w1], delete `mc` [w2], then the
    // ThreadUpdate undo's set_thread [w3] — trip at w3 so both marker inverses
    // are on the ledger.
    let result = with_entry_write_fault(2, || {
        repo::atomic::execute(&repo, UndoOp::new(batches, recovery_head, txid))
    });
    assert!(result.is_err(), "the injected fault must fail the undo");

    assert_eq!(
        repo.refs().get_marker(&MarkerName::new("mc")).unwrap(),
        Some(s1),
        "mc restored by the delete_marker inverse"
    );
    assert_eq!(
        repo.refs().get_marker(&MarkerName::new("md")).unwrap(),
        None,
        "md removed again by the create_marker inverse"
    );
}

/// Per-effect rollback of thread-ref writes. Undoing a batch that re-creates a
/// deleted thread (`new`) and deletes a created thread (`old`) registers a
/// per-effect inverse for each; a later write failing must restore both refs.
#[test]
fn per_effect_rollback_restores_thread_ref_writes() {
    let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();
    // `old` exists (its ThreadCreate undo = delete_thread, inverse re-set);
    // `new` does not (its ThreadDelete undo = set_thread, inverse delete).
    repo.refs()
        .set_thread(&ThreadName::new("old"), &s1)
        .unwrap();
    let main_state = main_thread(&repo).unwrap();

    repo.oplog()
        .record_batch_scoped(
            vec![
                OpRecord::ThreadUpdate {
                    name: "main".to_string(),
                    old_state: main_state,
                    new_state: main_state,
                    manager_snapshots: None,
                },
                OpRecord::ThreadCreate {
                    name: "old".to_string(),
                    state: s1,
                    manager_snapshot: None,
                },
                OpRecord::ThreadDelete {
                    name: "new".to_string(),
                    state: s1,
                },
            ],
            Some(&scope),
        )
        .unwrap();

    let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
    let recovery_head = repo.head().unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
    // Undo order (entries.rev()): set `new` [w1], delete `old` [w2], then the
    // ThreadUpdate undo's set_thread [w3] — trip at w3.
    let result = with_entry_write_fault(2, || {
        repo::atomic::execute(&repo, UndoOp::new(batches, recovery_head, txid))
    });
    assert!(result.is_err(), "the injected fault must fail the undo");

    assert_eq!(
        repo.refs().get_thread(&ThreadName::new("old")).unwrap(),
        Some(s1),
        "old restored by the delete_thread inverse"
    );
    assert_eq!(
        repo.refs().get_thread(&ThreadName::new("new")).unwrap(),
        None,
        "new removed again by the set_thread inverse"
    );
}

/// A successful round trip via the atomic ops: undo then redo restores the
/// original tip, and the marker-only commit batches are excluded from the
/// undo/redo eligibility scans (so the round trip terminates instead of
/// chasing its own commit sentinels).
#[test]
fn atomic_undo_redo_round_trip_ignores_commit_markers() {
    let (temp, repo, s1, s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();

    // Undo s2.
    let recovery_head = repo.head().unwrap();
    let undo_batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("undo", &scope, generation, &undo_batches);
    repo::atomic::execute(&repo, UndoOp::new(undo_batches, recovery_head, txid)).unwrap();
    assert_eq!(repo.head().unwrap(), Some(s1));

    // The undo's commit marker is a record-less batch — not itself undoable.
    let still_undoable = repo.oplog().undo_batches_scoped(2, Some(&scope)).unwrap();
    assert_eq!(
        still_undoable.len(),
        1,
        "only the s1 snapshot remains undoable; the commit marker is excluded"
    );

    // Redo s2.
    let redo_batches = repo.oplog().redo_batches_scoped(1, Some(&scope)).unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("redo", &scope, generation, &redo_batches);
    repo::atomic::execute(&repo, RedoOp::new(redo_batches, txid)).unwrap();
    assert_eq!(repo.head().unwrap(), Some(s2), "redo restored the s2 tip");
    assert!(
        temp.path().join("b.txt").exists(),
        "s2 file restored by redo"
    );
}

/// The undo/redo serialization lock is mutually exclusive: while one holder
/// has it, a second writer on the same lock file is blocked; once released it
/// is acquirable again (heddle#355 cid 3330867776).
#[test]
fn undo_redo_lock_is_exclusive() {
    let (_temp, repo, _s1, _s2) = repo_with_two_snapshots();
    let lock_path = repo.heddle_dir().join("locks/undo-redo.lock");

    let guard = acquire_undo_redo_lock(&repo).unwrap();
    let contended = RepoLock::at(lock_path.clone()).try_write().unwrap();
    assert!(
        contended.is_none(),
        "a second writer must be blocked while the lock is held"
    );

    drop(guard);
    let reacquired = RepoLock::at(lock_path).try_write().unwrap();
    assert!(
        reacquired.is_some(),
        "the lock is acquirable again after the holder releases it"
    );
}

/// The serialized outcome the lock guarantees: a second undo invocation that
/// (re-)selects its batches only AFTER the first has committed sees the
/// already-undone batch and targets the PRECEDING op instead — it never
/// re-selects the batch the first undid, so the two can't derive the same
/// generation-keyed transaction id and the second can't dedup-hit and
/// self-revert the first (heddle#355 cid 3330867776).
#[test]
fn serialized_second_undo_selects_a_different_batch() {
    let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();

    // Invocation 1, under the lock: undo the newest batch (s2).
    let first_ids: Vec<u64> = {
        let _lock = acquire_undo_redo_lock(&repo).unwrap();
        let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        let ids = batches.iter().map(|b| b.id).collect();
        let recovery = repo.head().unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
        repo::atomic::execute(&repo, UndoOp::new(batches, recovery, txid)).unwrap();
        ids
    };
    assert_eq!(repo.head().unwrap(), Some(s1), "first undo reverted to s1");

    // Invocation 2, under the lock (after 1 released + committed): the s2
    // batch is now undone, so re-selection returns the preceding s1 batch.
    let _lock = acquire_undo_redo_lock(&repo).unwrap();
    let second = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
    let second_ids: Vec<u64> = second.iter().map(|b| b.id).collect();
    assert!(!second_ids.is_empty(), "a preceding op is still undoable");
    assert_ne!(
        second_ids, first_ids,
        "the serialized second undo must not re-select the batch the first already undid"
    );
}

/// `undo --list --depth N` returns N *user-facing* batches even when the
/// newest batch is an undo/redo's record-less commit marker (heddle#355 cid
/// 3330867777). After undoing s2, `recent_batches_scoped(1)` surfaces only
/// the marker sentinel; `recent_user_batches_scoped(1)` skips it and returns
/// the preceding real op (the s1 snapshot).
#[test]
fn list_depth_one_returns_preceding_user_op_past_commit_marker() {
    let (_temp, repo, _s1, _s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();

    // Undo s2 — this appends a marker-only `TransactionCommit` batch that is
    // now the newest batch in the log.
    let recovery_head = repo.head().unwrap();
    let undo_batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("undo", &scope, generation, &undo_batches);
    repo::atomic::execute(&repo, UndoOp::new(undo_batches, recovery_head, txid)).unwrap();

    // The fixed-count fetch surfaces only the commit marker for depth 1...
    let raw = repo.oplog().recent_batches_scoped(1, Some(&scope)).unwrap();
    assert_eq!(raw.len(), 1);
    assert!(
        raw[0].is_transaction_marker_only(),
        "the newest batch is the undo's commit marker"
    );

    // ...while the user-facing query skips it and returns the real op.
    let user = repo
        .oplog()
        .recent_user_batches_scoped(1, Some(&scope))
        .unwrap();
    assert_eq!(
        user.len(),
        1,
        "depth 1 returns exactly one user-facing batch"
    );
    assert!(
        !user[0].is_transaction_marker_only(),
        "the returned batch is a real op, not the marker sentinel"
    );
    assert!(
        user[0]
            .entries
            .iter()
            .any(|e| matches!(e.operation, OpRecord::Snapshot { .. })),
        "it is the preceding s1 snapshot"
    );
}

/// Compensator class, undo direction (heddle#355 cid 3330867774). Undoing a
/// `MarkerDelete` recreates the marker (`create_marker`). When that forward
/// FAILS because a marker of the same name already exists (a pre-existing
/// ref), the migration onto `Tx::step` guarantees NO `delete_marker` inverse
/// was registered — so the rollback leaves the pre-existing marker intact.
/// Pre-`step` (register-then-forward) the inverse ran on rollback and deleted
/// the pre-existing marker.
#[test]
fn undo_marker_delete_forward_failure_keeps_preexisting_marker() {
    let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();
    let marker = MarkerName::new("keep");

    // A `MarkerDelete` batch becomes the newest undoable op; undoing it will
    // attempt `create_marker("keep", s1)`.
    repo.oplog()
        .record_batch_scoped(
            vec![OpRecord::MarkerDelete {
                name: "keep".to_string(),
                state: s1,
            }],
            Some(&scope),
        )
        .unwrap();

    // Plant a pre-existing marker of the same name — the undo's
    // `create_marker` will now collide and fail.
    repo.refs().create_marker(&marker, &s1).unwrap();

    let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
    assert!(
        matches!(
            batches[0].entries[0].operation,
            OpRecord::MarkerDelete { .. }
        ),
        "the newest undoable batch is the MarkerDelete"
    );
    let recovery_head = repo.head().unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
    let result = repo::atomic::execute(&repo, UndoOp::new(batches, recovery_head, txid));

    assert!(
        result.is_err(),
        "the colliding create_marker must fail the undo"
    );
    assert_eq!(
        repo.refs().get_marker(&marker).unwrap(),
        Some(s1),
        "the pre-existing marker survives the rolled-back undo (no delete inverse ran)"
    );
}

/// Compensator class, redo direction (heddle#355 cid 3330867775). Redoing a
/// `MarkerCreate` re-runs `create_marker`. A collision with a pre-existing
/// marker must NOT delete it on rollback — the mirror of the undo case.
#[test]
fn redo_marker_create_forward_failure_keeps_preexisting_marker() {
    let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();
    let marker = MarkerName::new("keep");

    // Record a `MarkerCreate`, then mark it undone so it is REDOABLE; redoing
    // it will attempt `create_marker("keep", s1)`.
    repo.oplog()
        .record_batch_scoped(
            vec![OpRecord::MarkerCreate {
                name: "keep".to_string(),
                state: s1,
            }],
            Some(&scope),
        )
        .unwrap();
    let created = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
    repo.oplog().mark_batch_undone(&created[0]).unwrap();

    // Plant the pre-existing colliding marker.
    repo.refs().create_marker(&marker, &s1).unwrap();

    let redo_batches = repo.oplog().redo_batches_scoped(1, Some(&scope)).unwrap();
    assert!(
        matches!(
            redo_batches[0].entries[0].operation,
            OpRecord::MarkerCreate { .. }
        ),
        "the redoable batch is the MarkerCreate"
    );
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("redo", &scope, generation, &redo_batches);
    let result = repo::atomic::execute(&repo, RedoOp::new(redo_batches, txid));

    assert!(
        result.is_err(),
        "the colliding create_marker must fail the redo"
    );
    assert_eq!(
        repo.refs().get_marker(&marker).unwrap(),
        Some(s1),
        "the pre-existing marker survives the rolled-back redo (no delete inverse ran)"
    );
}

// ---- step_nonatomic: forward-internal partial-failure rollback (r4 §A) ----

/// `goto` is a NON-atomic forward (worktree materialize + HEAD write). When it
/// applies its effect (moves HEAD + worktree) and then fails, the
/// restore-to-snapshot inverse `step_nonatomic` registered BEFORE the forward
/// must unwind it. A plain `step` would register NOTHING on the `Err` return
/// and leak the moved HEAD/worktree (the hazard this combinator closes).
#[test]
fn step_nonatomic_rolls_back_partially_applied_goto() {
    let (temp, repo, _s1, s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();

    let pre_head = repo.head().unwrap();
    assert_eq!(pre_head, Some(s2));
    let pre_main = main_thread(&repo);
    let pre_markers = commit_marker_count(&repo);
    assert_eq!(repo.refs().get_undo_recovery().unwrap(), None);

    let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
    // The goto is the entry's FIRST non-atomic step: it runs (materializing
    // the worktree + moving HEAD) and then fails.
    let result = with_nonatomic_forward_fault(0, || {
        repo::atomic::execute(&repo, UndoOp::new(batches, pre_head, txid))
    });
    assert!(
        result.is_err(),
        "the injected partial-goto fault must fail the undo"
    );

    assert_eq!(
        repo.head().unwrap(),
        Some(s2),
        "HEAD restored to the pre-undo tip after a partially-applied goto"
    );
    assert_eq!(main_thread(&repo), pre_main, "main ref unchanged");
    assert!(temp.path().join("a.txt").exists(), "s1 file present");
    assert!(
        temp.path().join("b.txt").exists(),
        "s2 worktree material restored by the goto's restore-before inverse"
    );
    assert_eq!(
        repo.refs().get_undo_recovery().unwrap(),
        None,
        "recovery pointer cleared by rewind"
    );
    assert_eq!(
        commit_marker_count(&repo),
        pre_markers,
        "no marker committed"
    );
}

fn sample_main_thread(current_state: &str, materialized: &str) -> Thread {
    Thread {
        id: "thread-main".to_string(),
        thread: "main".to_string(),
        target_thread: None,
        parent_thread: None,
        mode: repo::ThreadMode::Solid,
        state: ThreadState::Active,
        base_state: "base".to_string(),
        base_root: "root".to_string(),
        current_state: Some(current_state.to_string()),
        merged_state: None,
        task: None,
        execution_path: PathBuf::from("/work/exec"),
        materialized_path: Some(PathBuf::from(materialized)),
        changed_paths: vec![],
        impact_categories: vec![],
        heavy_impact_paths: vec![],
        promotion_suggested: false,
        freshness: ThreadFreshness::Current,
        verification_summary: repo::ThreadVerificationSummary::default(),
        confidence_summary: repo::ThreadConfidenceSummary::default(),
        integration_policy_result: ThreadIntegrationPolicy::default(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        ephemeral: None,
        auto: false,
        shared_target_dir: None,
    }
}

fn encode_thread_record_set(manager: &ThreadManager, records: &[Thread]) -> Vec<Vec<u8>> {
    records
        .iter()
        .map(|record| manager.encode_thread_record_snapshot(record).unwrap())
        .collect()
}

fn apply_undo_once(repo: &Repository, scope: &str) {
    let batches = repo.oplog().undo_batches_scoped(1, Some(scope)).unwrap();
    let recovery_head = repo.head().unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("undo", scope, generation, &batches);
    repo::atomic::execute(repo, UndoOp::new(batches, recovery_head, txid)).unwrap();
}

fn apply_redo_once(repo: &Repository, scope: &str) {
    let batches = repo.oplog().redo_batches_scoped(1, Some(scope)).unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("redo", scope, generation, &batches);
    repo::atomic::execute(repo, RedoOp::new(batches, txid)).unwrap();
}

#[test]
fn thread_update_undo_preserves_missing_ref_fallback_absence() {
    let (_temp, repo, s1, s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();
    let manager = ThreadManager::new(repo.heddle_dir());
    let mut old_record = sample_main_thread(&s1.short(), "/work/missing-ref");
    old_record.id = "missing-ref".to_string();
    old_record.thread = "missing-ref".to_string();
    old_record.base_state = s1.short();
    old_record.current_state = Some(s1.short());
    let mut new_record = old_record.clone();
    new_record.current_state = Some(s2.short());
    new_record.updated_at = old_record.updated_at + chrono::Duration::seconds(1);
    manager.save(&new_record).unwrap();
    repo.refs()
        .delete_thread(&ThreadName::new("missing-ref"))
        .unwrap();

    repo.oplog()
        .record_batch_scoped(
            vec![OpRecord::ThreadUpdate {
                name: "missing-ref".to_string(),
                old_state: s1,
                new_state: s2,
                manager_snapshots: ThreadUpdateSnapshots::from_record_sets(
                    Some(manager.encode_thread_record_snapshot(&old_record).unwrap()),
                    Some(manager.encode_thread_record_snapshot(&new_record).unwrap()),
                    encode_thread_record_set(&manager, std::slice::from_ref(&old_record)),
                    encode_thread_record_set(&manager, std::slice::from_ref(&new_record)),
                    true,
                ),
            }],
            Some(&scope),
        )
        .unwrap();

    apply_undo_once(&repo, &scope);
    assert_eq!(
        repo.refs()
            .get_thread(&ThreadName::new("missing-ref"))
            .unwrap(),
        None,
        "undo restores the pre-update absence instead of fabricating a ref"
    );
    assert_eq!(
        manager
            .find_by_thread("missing-ref")
            .unwrap()
            .unwrap()
            .current_state
            .as_deref(),
        Some(s1.short().as_str()),
        "undo restores the old ThreadManager record"
    );

    apply_redo_once(&repo, &scope);
    assert_eq!(
        repo.refs()
            .get_thread(&ThreadName::new("missing-ref"))
            .unwrap(),
        Some(s2),
        "redo recreates the post-update thread ref"
    );
    assert_eq!(
        manager
            .find_by_thread("missing-ref")
            .unwrap()
            .unwrap()
            .current_state
            .as_deref(),
        Some(s2.short().as_str()),
        "redo restores the new ThreadManager record"
    );
}

#[test]
fn thread_update_undo_redo_restores_duplicate_same_name_record_sets() {
    let (_temp, repo, s1, s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();
    let manager = ThreadManager::new(repo.heddle_dir());
    let mut winner_old = sample_main_thread(&s1.short(), "/work/winner-old");
    winner_old.id = "rec-winner".to_string();
    winner_old.updated_at = chrono::Utc::now();
    let mut duplicate = sample_main_thread(&s1.short(), "/work/duplicate");
    duplicate.id = "rec-duplicate".to_string();
    duplicate.updated_at = winner_old.updated_at - chrono::Duration::seconds(30);
    let mut winner_new = winner_old.clone();
    winner_new.current_state = Some(s2.short());
    winner_new.materialized_path = Some(PathBuf::from("/work/winner-new"));
    winner_new.updated_at = winner_old.updated_at + chrono::Duration::seconds(30);
    manager.save(&winner_new).unwrap();
    manager.save(&duplicate).unwrap();
    repo.refs()
        .set_thread(&ThreadName::new("main"), &s2)
        .unwrap();

    let old_records = vec![winner_old.clone(), duplicate.clone()];
    let new_records = vec![winner_new.clone(), duplicate.clone()];
    repo.oplog()
        .record_batch_scoped(
            vec![OpRecord::ThreadUpdate {
                name: "main".to_string(),
                old_state: s1,
                new_state: s2,
                manager_snapshots: ThreadUpdateSnapshots::from_record_sets(
                    Some(manager.encode_thread_record_snapshot(&winner_old).unwrap()),
                    Some(manager.encode_thread_record_snapshot(&winner_new).unwrap()),
                    encode_thread_record_set(&manager, &old_records),
                    encode_thread_record_set(&manager, &new_records),
                    false,
                ),
            }],
            Some(&scope),
        )
        .unwrap();

    apply_undo_once(&repo, &scope);
    let undone = manager.snapshot_records("main").unwrap();
    let undone_ids: std::collections::HashSet<_> =
        undone.iter().map(|record| record.id.as_str()).collect();
    assert_eq!(
        undone_ids,
        std::collections::HashSet::from(["rec-winner", "rec-duplicate"]),
        "undo preserves every same-name record"
    );
    assert_eq!(
        manager
            .load("rec-winner")
            .unwrap()
            .unwrap()
            .current_state
            .as_deref(),
        Some(s1.short().as_str()),
        "undo restores the winner's old body"
    );
    assert_eq!(
        manager
            .load("rec-duplicate")
            .unwrap()
            .unwrap()
            .materialized_path,
        Some(PathBuf::from("/work/duplicate")),
        "undo keeps the non-winner duplicate worktree metadata"
    );

    apply_redo_once(&repo, &scope);
    let redone = manager.snapshot_records("main").unwrap();
    let redone_ids: std::collections::HashSet<_> =
        redone.iter().map(|record| record.id.as_str()).collect();
    assert_eq!(
        redone_ids,
        std::collections::HashSet::from(["rec-winner", "rec-duplicate"]),
        "redo preserves every same-name record"
    );
    assert_eq!(
        manager
            .load("rec-winner")
            .unwrap()
            .unwrap()
            .current_state
            .as_deref(),
        Some(s2.short().as_str()),
        "redo restores the winner's new body"
    );
    assert_eq!(
        manager
            .load("rec-duplicate")
            .unwrap()
            .unwrap()
            .materialized_path,
        Some(PathBuf::from("/work/duplicate")),
        "redo keeps the non-winner duplicate worktree metadata"
    );
}

/// Test-only deferred mutation that saves ONE thread record through the
/// `EntrySteps` applier — exercises the real `save_thread_record`
/// (`step_nonatomic`) capture-restore path.
struct SaveOnly {
    record: Thread,
}

impl AtomicMutation for SaveOnly {
    type Output = ();

    fn transaction_id(&self) -> String {
        "test-save-only".to_string()
    }

    fn isolation_keys(&self, _repo: &Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
        let mut keys = BTreeSet::new();
        keys.insert(IsolationKey::Thread(self.record.thread.clone()));
        Ok(keys)
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<()>> {
        let mut steps = EntrySteps::new(tx);
        steps.save_thread_record(self.record.clone())?;
        Ok(StagedCommit::pure(()))
    }
}

impl DeferredMutation for SaveOnly {}

/// `ThreadManager::save` is a NON-atomic forward — it writes the record file
/// AND the workspace file. When the save applies (both halves) and then a
/// failure occurs, the `step_nonatomic` capture-restore must rewrite BOTH
/// halves back to the prior record. A plain `step` would leak the saved
/// record/workspace on the `Err` return.
#[test]
fn step_nonatomic_restores_record_and_workspace_on_save_failure() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());

    // R0: the prior persisted record (record half: current_state; workspace
    // half: materialized_path).
    let r0 = sample_main_thread("current-A", "/work/A");
    manager.save(&r0).unwrap();

    // R1: what the (faulted) save writes — different in BOTH halves.
    let mut r1 = r0.clone();
    r1.current_state = Some("current-B".to_string());
    r1.materialized_path = Some(PathBuf::from("/work/B"));

    let result =
        with_nonatomic_forward_fault(0, || repo::atomic::execute(&repo, SaveOnly { record: r1 }));
    assert!(result.is_err(), "the injected save fault must fail the op");

    let restored = manager.find_by_thread("main").unwrap().unwrap();
    assert_eq!(
        restored.current_state.as_deref(),
        Some("current-A"),
        "the record half (current_state) was restored to R0"
    );
    assert_eq!(
        restored.materialized_path,
        Some(PathBuf::from("/work/A")),
        "the workspace half (materialized_path) was restored to R0"
    );
}

/// A "replacement save" persists the thread under a NEW record id (the prior
/// record had a different id). `find_by_thread` selects among ALL records with
/// that thread name, so if a later failure rolls the save back, the restore
/// must delete the newly-written `new_id` record + its workspace file — not
/// just re-save `prev`. Otherwise the leaked newer record stays visible and
/// record-backed commands observe the rolled-back-away state. A re-save-only
/// restore leaves two records for "main" and `find_by_thread` returns the leak.
#[test]
fn step_nonatomic_restores_replacement_save_deleting_leaked_new_record() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());

    // R0: the prior persisted record for thread "main".
    let mut r0 = sample_main_thread("current-A", "/work/A");
    r0.id = "thread-main-v1".to_string();
    r0.updated_at = chrono::Utc::now();
    manager.save(&r0).unwrap();

    // R1: the replacement save — SAME thread, DIFFERENT record id, and a later
    // `updated_at` so a leaked R1 would win `find_by_thread`'s max-by-updated.
    let mut r1 = r0.clone();
    r1.id = "thread-main-v2".to_string();
    r1.current_state = Some("current-B".to_string());
    r1.updated_at = r0.updated_at + chrono::Duration::seconds(60);

    let result =
        with_nonatomic_forward_fault(0, || repo::atomic::execute(&repo, SaveOnly { record: r1 }));
    assert!(result.is_err(), "the injected save fault must fail the op");

    assert!(
        manager.load("thread-main-v2").unwrap().is_none(),
        "the leaked new_id record must be deleted on rollback"
    );
    let remaining = manager.list().unwrap();
    assert_eq!(
        remaining.len(),
        1,
        "only the prior record survives for the thread, no leaked newer record"
    );
    let restored = manager.find_by_thread("main").unwrap().unwrap();
    assert_eq!(
        restored.id, "thread-main-v1",
        "find_by_thread returns ONLY prev"
    );
    assert_eq!(restored.current_state.as_deref(), Some("current-A"));
}

/// A "create save" persists a thread with NO prior record. On rollback the
/// restore must delete the created record + its workspace file so nothing is
/// left for the thread.
#[test]
fn step_nonatomic_create_save_rollback_removes_created_record() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());

    let mut created = sample_main_thread("current-A", "/work/A");
    created.id = "thread-main-new".to_string();

    let result = with_nonatomic_forward_fault(0, || {
        repo::atomic::execute(&repo, SaveOnly { record: created })
    });
    assert!(result.is_err(), "the injected save fault must fail the op");

    assert!(
        manager.load("thread-main-new").unwrap().is_none(),
        "the created record must be removed on rollback"
    );
    assert!(
        manager.find_by_thread("main").unwrap().is_none(),
        "no record survives for a rolled-back create save"
    );
}

/// Test-only deferred mutation that restores ONE thread record from a redo
/// snapshot through the `EntrySteps` applier — exercises the real
/// `restore_thread_record` (`step_nonatomic`) capture-restore path, the redo
/// arm whose forward writes a record under a snapshot-buried id.
struct RestoreSnapshotOnly {
    name: String,
    bytes: Vec<u8>,
}

impl AtomicMutation for RestoreSnapshotOnly {
    type Output = ();

    fn transaction_id(&self) -> String {
        "test-restore-snapshot-only".to_string()
    }

    fn isolation_keys(&self, _repo: &Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
        let mut keys = BTreeSet::new();
        keys.insert(IsolationKey::Thread(self.name.clone()));
        Ok(keys)
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<()>> {
        let mut steps = EntrySteps::new(tx);
        steps.restore_thread_record(&self.name, &self.bytes, "ThreadCreate")?;
        Ok(StagedCommit::pure(()))
    }
}

impl DeferredMutation for RestoreSnapshotOnly {}

/// The redo-snapshot sibling of `..._restores_replacement_save_...`: the redo
/// of a `ThreadCreate` restores the record from an opaque snapshot whose
/// record id is NOT known to the applier. The forward writes that snapshot-id
/// record (newer timestamp); on rollback the converge must drop it so
/// `find_by_thread` returns ONLY the prior record. A re-save-only restore
/// (the pre-r6 redo arm) left the snapshot-id record and `find_by_thread`
/// returned the leak — this test fails against that arm and passes against the
/// `converge_records` restore.
#[test]
fn step_nonatomic_restores_redo_snapshot_deleting_leaked_record() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());

    // R0: the prior persisted record for thread "main".
    let mut r0 = sample_main_thread("current-A", "/work/A");
    r0.id = "thread-main-v1".to_string();
    r0.updated_at = chrono::Utc::now();
    manager.save(&r0).unwrap();

    // Build a redo snapshot of a DIFFERENT-id, NEWER record (what the redo
    // forward writes). Save it, snapshot it, then remove it so only the prior
    // record remains at capture time.
    let mut snap_rec = r0.clone();
    snap_rec.id = "thread-main-v2".to_string();
    snap_rec.current_state = Some("current-B".to_string());
    snap_rec.updated_at = r0.updated_at + chrono::Duration::seconds(60);
    manager.save(&snap_rec).unwrap();
    let snapshot = manager.snapshot_thread_record("main").unwrap().unwrap();
    manager.delete("thread-main-v2").unwrap();
    assert_eq!(
        manager.list().unwrap().len(),
        1,
        "precondition: only the prior record exists at capture time"
    );

    let result = with_nonatomic_forward_fault(0, || {
        repo::atomic::execute(
            &repo,
            RestoreSnapshotOnly {
                name: "main".to_string(),
                bytes: snapshot,
            },
        )
    });
    assert!(
        result.is_err(),
        "the injected restore fault must fail the op"
    );

    assert!(
        manager.load("thread-main-v2").unwrap().is_none(),
        "the leaked snapshot-id record must be deleted on rollback"
    );
    assert_eq!(
        manager.list().unwrap().len(),
        1,
        "only the prior record survives, no leaked newer record"
    );
    let restored = manager.find_by_thread("main").unwrap().unwrap();
    assert_eq!(
        restored.id, "thread-main-v1",
        "find_by_thread returns ONLY prev"
    );
    assert_eq!(restored.current_state.as_deref(), Some("current-A"));
}

/// SUCCESS-path postcondition of the redo `ThreadCreate` restore (cid
/// 3331603135): when a pre-existing DUPLICATE is already filed under the name,
/// redoing the create restores the snapshot AND leaves ONLY the restored
/// record — the success path has the same single-record postcondition as the
/// rollback converge. The pre-r8 arm `save`d the decoded record without
/// removing the duplicate, so two records survived and `find_by_thread`
/// (max-by-updated) returned the newer duplicate, not the restored record —
/// this test fails against that arm and passes against decode→converge.
#[test]
fn redo_restore_thread_record_converges_away_preexisting_duplicate() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());

    // The record the redo snapshot will restore (older timestamp).
    let mut to_restore = sample_main_thread("current-restored", "/work/R");
    to_restore.id = "rec-restored".to_string();
    to_restore.updated_at = chrono::Utc::now();
    manager.save(&to_restore).unwrap();
    let snapshot = manager.snapshot_thread_record("main").unwrap().unwrap();
    manager.delete("rec-restored").unwrap();

    // A pre-existing DUPLICATE under the same name with a NEWER timestamp, so
    // a raw-save redo would leave it winning `find_by_thread`.
    let mut dup = sample_main_thread("current-dup", "/work/D");
    dup.id = "rec-dup".to_string();
    dup.updated_at = to_restore.updated_at + chrono::Duration::seconds(60);
    manager.save(&dup).unwrap();
    assert_eq!(
        manager
            .list()
            .unwrap()
            .iter()
            .filter(|t| t.thread == "main")
            .count(),
        1,
        "precondition: only the duplicate is filed at redo time"
    );

    // Redo restores the snapshot — SUCCESS path (no fault).
    repo::atomic::execute(
        &repo,
        RestoreSnapshotOnly {
            name: "main".to_string(),
            bytes: snapshot,
        },
    )
    .unwrap();

    let under_name: Vec<_> = manager
        .list()
        .unwrap()
        .into_iter()
        .filter(|t| t.thread == "main")
        .collect();
    assert_eq!(
        under_name.len(),
        1,
        "ONLY the restored record remains — the duplicate is converged away"
    );
    assert_eq!(under_name[0].id, "rec-restored");
    assert_eq!(
        manager.find_by_thread("main").unwrap().unwrap().id,
        "rec-restored",
        "find_by_thread returns the restored record, not the leaked duplicate"
    );
    assert!(
        manager.load("rec-dup").unwrap().is_none(),
        "the pre-existing duplicate record file is gone"
    );
}

/// Test-only deferred mutation that runs `remove_thread_manager_record` — the
/// `ThreadCreate` inverse — through the `EntrySteps` applier, so its single
/// lock-atomic `converge_records`-to-empty step and the converge-back-to-prior
/// rollback can be exercised in isolation.
struct RemoveRecordOnly {
    name: String,
}

impl AtomicMutation for RemoveRecordOnly {
    type Output = ();

    fn transaction_id(&self) -> String {
        "test-remove-record-only".to_string()
    }

    fn isolation_keys(&self, _repo: &Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
        let mut keys = BTreeSet::new();
        keys.insert(IsolationKey::Thread(self.name.clone()));
        Ok(keys)
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<()>> {
        let mut steps = EntrySteps::new(tx);
        remove_thread_manager_record(&mut steps, &self.name)?;
        Ok(StagedCommit::pure(()))
    }
}

impl DeferredMutation for RemoveRecordOnly {}

/// The created-thread inverse converges the name to EMPTY: when the store holds
/// MULTIPLE records under the name (the duplicate class the converge tolerates),
/// undoing the `ThreadCreate` must drop EVERY same-name record, not just the
/// `find_by_thread` winner. The pre-fix arm deleted only the winner, leaving the
/// older duplicate as a phantom whose thread ref is gone — this test fails
/// against that arm (the older record survives) and passes against converge-to-
/// empty.
#[test]
fn remove_thread_manager_record_converges_name_to_empty() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());

    // Two records under "main": a winner (newer) + an older duplicate.
    let mut winner = sample_main_thread("current-A", "/work/A");
    winner.id = "rec-winner".to_string();
    winner.updated_at = chrono::Utc::now();
    manager.save(&winner).unwrap();
    let mut older = sample_main_thread("current-B", "/work/B");
    older.id = "rec-older".to_string();
    older.updated_at = winner.updated_at - chrono::Duration::seconds(60);
    manager.save(&older).unwrap();
    assert_eq!(
        manager.list().unwrap().len(),
        2,
        "precondition: two records"
    );

    repo::atomic::execute(
        &repo,
        RemoveRecordOnly {
            name: "main".to_string(),
        },
    )
    .unwrap();

    assert!(
        manager.find_by_thread("main").unwrap().is_none(),
        "converge-to-empty: no record survives under the name"
    );
    assert!(
        manager.list().unwrap().iter().all(|t| t.thread != "main"),
        "EVERY same-name record removed, not just the find_by_thread winner"
    );
}

/// Rollback of the converge-to-empty inverse: the single `step_nonatomic`
/// converge runs its forward (deleting BOTH same-name records under one write
/// lock) and then fails — the converge-back-to-prior inverse, registered
/// BEFORE the forward, must restore the FULL captured prior set (both
/// records), not just the `find_by_thread` winner. Arming the fault at the
/// converge step (index 0) proves the whole-set capture-restore reverses a
/// lock-atomic all-or-nothing forward in one inverse.
#[test]
fn remove_thread_manager_record_rollback_resaves_all_records() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = ThreadManager::new(repo.heddle_dir());

    let mut winner = sample_main_thread("current-A", "/work/A");
    winner.id = "rec-winner".to_string();
    winner.updated_at = chrono::Utc::now();
    manager.save(&winner).unwrap();
    let mut older = sample_main_thread("current-B", "/work/B");
    older.id = "rec-older".to_string();
    older.updated_at = winner.updated_at - chrono::Duration::seconds(60);
    manager.save(&older).unwrap();

    // Fault the converge forward: it empties the name (both records deleted
    // under one lock), then the op fails — the inverse must re-converge to the
    // full captured prior set, restoring BOTH records.
    let result = with_nonatomic_forward_fault(0, || {
        repo::atomic::execute(
            &repo,
            RemoveRecordOnly {
                name: "main".to_string(),
            },
        )
    });
    assert!(
        result.is_err(),
        "the injected forward fault must fail the op"
    );

    let remaining = manager.list().unwrap();
    assert_eq!(
        remaining.len(),
        2,
        "rollback re-converged to ALL same-name records, not just the winner"
    );
    let ids: std::collections::HashSet<_> = remaining.iter().map(|t| t.id.clone()).collect();
    assert!(
        ids.contains("rec-winner") && ids.contains("rec-older"),
        "both the winner and the older duplicate were restored"
    );
    assert_eq!(
        manager.find_by_thread("main").unwrap().unwrap().id,
        "rec-winner",
        "find_by_thread still selects the newer winner after rollback"
    );
}

/// Undoing a `Redact` removes the per-blob sidecar (re-exposing the blob). If
/// a LATER batch in the same undo transaction fails, the `step_nonatomic`
/// capture-restore — registered BEFORE the removal — must restore the sidecar
/// so the redacted blob is NOT re-exposed. The pre-migration unregistered
/// removal left the blob exposed on a rolled-back undo. (`--allow-redact-undo`
/// gates this at the command level; the apply path is exercised directly.)
#[test]
fn step_nonatomic_restores_redaction_sidecar_when_a_later_batch_fails() {
    use objects::object::{Principal, Redaction};

    let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();
    let main_state = main_thread(&repo).unwrap();

    // A real redaction on disk.
    let blob = ContentHash::from_bytes([7u8; 32]);
    let redaction = Redaction {
        redacted_blob: blob,
        state: s1,
        path: "config/secrets.toml".to_string(),
        reason: "leaked credential".to_string(),
        redactor: Principal {
            name: "Grace Hopper".to_string(),
            email: "grace@example.com".to_string(),
        },
        redacted_at: chrono::Utc::now(),
        signature: None,
        purged_at: None,
        supersedes: None,
    };
    let redaction_id = repo.put_redaction(redaction).unwrap();
    assert_eq!(
        repo.get_redactions_for_blob(&blob)
            .unwrap()
            .redactions
            .len(),
        1,
        "redaction planted on disk"
    );

    // Older batch (a no-op thread update) recorded first; newer Redact batch
    // recorded second, so the Redact is undone FIRST and the older batch —
    // which the injected fault fails — is undone after it.
    repo.oplog()
        .record_batch_scoped(
            vec![OpRecord::ThreadUpdate {
                name: "main".to_string(),
                old_state: main_state,
                new_state: main_state,
                manager_snapshots: None,
            }],
            Some(&scope),
        )
        .unwrap();
    repo.oplog()
        .record_batch_scoped(
            vec![OpRecord::Redact {
                redaction_id,
                blob,
                state: s1,
                path: "config/secrets.toml".to_string(),
            }],
            Some(&scope),
        )
        .unwrap();

    let batches = repo.oplog().undo_batches_scoped(2, Some(&scope)).unwrap();
    assert_eq!(batches.len(), 2);
    assert!(
        matches!(batches[0].entries[0].operation, OpRecord::Redact { .. }),
        "the newest undoable batch is the Redact (undone first)"
    );

    let recovery_head = repo.head().unwrap();
    // FaultyUndo fails the LAST enrolled batch (the older ThreadUpdate) after
    // its first entry — by then the Redact's sidecar removal already ran.
    let result = repo::atomic::execute(
        &repo,
        FaultyUndo {
            batches,
            recovery_head,
            fail_after: 1,
        },
    );
    assert!(
        result.is_err(),
        "the injected fault on the later batch must fail the undo"
    );

    let restored = repo.get_redactions_for_blob(&blob).unwrap();
    assert_eq!(
        restored.redactions.len(),
        1,
        "redaction sidecar restored by the rollback — the blob is NOT re-exposed"
    );
    assert!(
        repo.get_redaction(&redaction_id).unwrap().is_some(),
        "the exact redaction record is back on disk"
    );
}

/// Build a `StateVisibility` record for an existing state with an explicit
/// timestamp (so distinct records on the same state get distinct content
/// hashes and accrete rather than dedup).
fn visibility_record(
    state: StateId,
    tier: objects::object::VisibilityTier,
    ts: i64,
) -> objects::object::StateVisibility {
    objects::object::StateVisibility {
        state,
        tier,
        embargo_until: None,
        declarer: objects::object::Principal {
            name: "Grace Hopper".to_string(),
            email: "grace@example.com".to_string(),
        },
        declared_at: chrono::DateTime::from_timestamp(ts, 0).unwrap(),
        signature: None,
        supersedes: None,
    }
}

/// heddle#317 r7 — the undo/redo restore must be serialized with a concurrent
/// `visibility set`/`promote` so it can never clobber a newer committed
/// record. A visibility set A on state S is committed and selected for undo;
/// then a concurrent set C commits on S (through the same locked transaction),
/// superseding A. Running the undo of A drives its restore through the repo
/// write lock, which re-checks the current sidecar: C no longer matches A's
/// recorded after-image, so the undo ABORTS instead of restoring A's stale
/// before-image over C. C survives.
#[test]
fn concurrent_set_during_undo_is_not_clobbered() {
    use objects::object::VisibilityTier;

    let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();
    let state = s1;

    // Commit visibility A on the state — the op the undo will target.
    repo.commit_state_visibility(
        visibility_record(state, VisibilityTier::Internal, 1_700_000_000),
        repo::VisibilityCommitKind::Set,
    )
    .expect("commit A")
    .expect("a set always commits");

    // Select A's undo batch (its StateVisibilitySet op) BEFORE C lands.
    let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
    assert!(
        batches[0]
            .entries
            .iter()
            .any(|e| matches!(e.operation, OpRecord::StateVisibilitySet { .. })),
        "the newest undoable batch is the visibility set"
    );

    // A concurrent `visibility set` C commits FIRST (through the locked
    // transaction), superseding A on disk.
    repo.commit_state_visibility(
        visibility_record(
            state,
            VisibilityTier::TeamScoped {
                team_id: "infra".to_string(),
            },
            1_700_000_060,
        ),
        repo::VisibilityCommitKind::Set,
    )
    .expect("commit C")
    .expect("a set always commits");
    let after_c = repo
        .get_state_visibility_bytes_for_state(&state)
        .expect("read sidecar after C");
    assert!(after_c.is_some(), "C is on disk");

    // Undo A: the restore takes the repo write lock, re-checks, sees C
    // superseded A's after-image, and aborts rather than clobbering C.
    let recovery = repo.head().unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
    let result = repo::atomic::execute(&repo, UndoOp::new(batches, recovery, txid));
    assert!(
        result.is_err(),
        "the undo must abort on the superseding concurrent visibility commit"
    );

    // C survives untouched — the undo did NOT restore A's stale before-image.
    assert_eq!(
        repo.get_state_visibility_bytes_for_state(&state).unwrap(),
        after_c,
        "the newer concurrent visibility record C must survive the aborted undo"
    );
    assert!(
        repo.has_visibility_for_state(&state).unwrap(),
        "the state stays non-public (C's tier), not dropped to public-by-absence"
    );
}

/// heddle#317 r7 — with NO concurrent writer, an undo→redo of a visibility op
/// still round-trips through the locked, conflict-rechecked restore: undo
/// drops the state back to public-by-absence and redo restores exactly the
/// op's after-image. Guards against the lock/re-check regressing normal
/// undo/redo.
#[test]
fn undo_redo_visibility_roundtrip_still_works() {
    use objects::object::VisibilityTier;

    let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
    let scope = repo.op_scope();
    let state = s1;
    assert!(
        !repo.has_visibility_for_state(&state).unwrap(),
        "the state starts public-by-absence"
    );

    // Commit visibility A.
    repo.commit_state_visibility(
        visibility_record(state, VisibilityTier::Internal, 1_700_000_000),
        repo::VisibilityCommitKind::Set,
    )
    .expect("commit A")
    .expect("a set always commits");
    let after_set = repo
        .get_state_visibility_bytes_for_state(&state)
        .expect("read A");
    assert!(after_set.is_some(), "A is on disk");

    // Undo A: the sidecar drops back to public-by-absence.
    let recovery = repo.head().unwrap();
    let undo_batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("undo", &scope, generation, &undo_batches);
    repo::atomic::execute(&repo, UndoOp::new(undo_batches, recovery, txid))
        .expect("undo succeeds with no concurrent writer");
    assert!(
        !repo.has_visibility_for_state(&state).unwrap(),
        "undo restored public-by-absence"
    );
    assert!(
        repo.get_state_visibility_bytes_for_state(&state)
            .unwrap()
            .is_none(),
        "the sidecar was removed by the undo"
    );

    // Redo A: the sidecar comes back to exactly A's bytes.
    let redo_batches = repo.oplog().redo_batches_scoped(1, Some(&scope)).unwrap();
    let generation = repo.oplog().head_id().unwrap();
    let txid = undo_redo_transaction_id("redo", &scope, generation, &redo_batches);
    repo::atomic::execute(&repo, RedoOp::new(redo_batches, txid)).expect("redo succeeds");
    assert_eq!(
        repo.get_state_visibility_bytes_for_state(&state).unwrap(),
        after_set,
        "redo restored exactly A's sidecar bytes"
    );
    assert!(
        repo.has_visibility_for_state(&state).unwrap(),
        "the state is non-public again after redo"
    );
}
