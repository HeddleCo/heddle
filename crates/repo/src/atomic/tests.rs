// SPDX-License-Identifier: Apache-2.0
//! Unit tests for the atomic-mutation primitive (heddle#330 §7 item 1).

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use objects::error::{HeddleError, Result};
use objects::object::{ChangeId, MarkerName, ThreadName};
use oplog::{OpLogBackend, OpRecord};
use refs::{Head, RefExpectation, RefUpdate};
use tempfile::TempDir;

use super::{
    AtomicMutation, Compensator, EagerMutation, RewindLedger, SavepointMutation, StagedCommit, Tx,
    execute,
};
use crate::Repository;

fn test_repo() -> (TempDir, Repository) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    (temp, repo)
}

/// A savepoint leg that records its id when its inverse runs, and can be made
/// to fail mid-apply (after registering its inverse).
struct Leg {
    id: u32,
    fail: bool,
    log: Rc<RefCell<Vec<u32>>>,
}

impl AtomicMutation for Leg {
    type Output = ();

    fn transaction_id(&self) -> String {
        format!("leg-{}", self.id)
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        let id = self.id;
        let log = Rc::clone(&self.log);
        tx.on_rewind(move || {
            log.borrow_mut().push(id);
            Ok(())
        });
        if self.fail {
            return Err(HeddleError::Config(format!("leg {id} failed")));
        }
        Ok(StagedCommit::pure(()))
    }
}

impl SavepointMutation for Leg {}

/// A composite that enrolls three legs; the third fails.
struct FailingComposite {
    log: Rc<RefCell<Vec<u32>>>,
}

impl AtomicMutation for FailingComposite {
    type Output = ();

    fn transaction_id(&self) -> String {
        "failing-composite".to_string()
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        tx.enroll(Leg {
            id: 1,
            fail: false,
            log: Rc::clone(&self.log),
        })?;
        tx.enroll(Leg {
            id: 2,
            fail: false,
            log: Rc::clone(&self.log),
        })?;
        tx.enroll(Leg {
            id: 3,
            fail: true,
            log: Rc::clone(&self.log),
        })?;
        Ok(StagedCommit::pure(()))
    }
}

/// Mirrors `refs_transactions.rs:341-377`: a mid-apply failure unwinds the
/// already-staged legs in strict reverse (LIFO) order, and nothing commits.
#[test]
fn reverse_order_rewind_on_failure() {
    let (_t, repo) = test_repo();
    let log = Rc::new(RefCell::new(Vec::new()));

    let result = execute(
        &repo,
        FailingComposite {
            log: Rc::clone(&log),
        },
    );

    assert!(result.is_err(), "the composite must fail");
    assert_eq!(
        *log.borrow(),
        vec![3, 2, 1],
        "inverses must run in reverse enroll order"
    );
    // Nothing committed: no TransactionCommit in the oplog.
    let recent = repo.oplog().recent(64).unwrap();
    assert!(
        !recent
            .iter()
            .any(|e| matches!(e.operation, OpRecord::TransactionCommit { .. })),
        "a failed transaction must not commit"
    );
}

/// A mutation whose `apply` panics after staging an effect.
struct Panicker {
    log: Rc<RefCell<Vec<u32>>>,
}

impl AtomicMutation for Panicker {
    type Output = ();

    fn transaction_id(&self) -> String {
        "panicker".to_string()
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        let log = Rc::clone(&self.log);
        tx.on_rewind(move || {
            log.borrow_mut().push(99);
            Ok(())
        });
        panic!("apply blew up");
    }
}

/// A whole-op-rewind mutation whose `apply` panics after staging state. It
/// registers no granular inverse, so only the pre-enrolled whole-op rewind can
/// clean it up during `Tx::drop`.
struct WholeOpPanicker {
    staged: Rc<RefCell<bool>>,
    rewound: Rc<RefCell<bool>>,
}

impl AtomicMutation for WholeOpPanicker {
    type Output = ();

    fn transaction_id(&self) -> String {
        "whole-op-panicker".to_string()
    }

    fn apply(&mut self, _tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        *self.staged.borrow_mut() = true;
        panic!("apply panicked after whole-op staging");
    }

    fn rewind(&mut self, _ledger: &RewindLedger) -> Result<()> {
        *self.staged.borrow_mut() = false;
        *self.rewound.borrow_mut() = true;
        Ok(())
    }
}

/// The `Drop` backstop (heddle#330 §4): a panic that unwinds through `apply`
/// still runs the reverse-order rewind and never commits.
#[test]
fn panic_unwind_runs_drop_backstop() {
    let (_t, repo) = test_repo();
    let log = Rc::new(RefCell::new(Vec::new()));

    let caught = catch_unwind(AssertUnwindSafe(|| {
        let _ = execute(
            &repo,
            Panicker {
                log: Rc::clone(&log),
            },
        );
    }));

    assert!(caught.is_err(), "the panic must propagate past execute");
    assert_eq!(
        *log.borrow(),
        vec![99],
        "Tx::drop must rewind the staged effect on panic-unwind"
    );
    let recent = repo.oplog().recent(64).unwrap();
    assert!(
        !recent
            .iter()
            .any(|e| matches!(e.operation, OpRecord::TransactionCommit { .. })),
        "a panicked transaction must not commit"
    );
}

#[test]
fn whole_op_rewind_runs_on_apply_panic() {
    let (_t, repo) = test_repo();
    let staged = Rc::new(RefCell::new(false));
    let rewound = Rc::new(RefCell::new(false));

    let caught = catch_unwind(AssertUnwindSafe(|| {
        let _ = execute(
            &repo,
            WholeOpPanicker {
                staged: Rc::clone(&staged),
                rewound: Rc::clone(&rewound),
            },
        );
    }));

    assert!(caught.is_err(), "the panic must propagate past execute");
    assert!(
        *rewound.borrow(),
        "the pre-enrolled whole-op rewind must run during panic unwind"
    );
    assert!(
        !*staged.borrow(),
        "a panicking apply must leave zero whole-op staged state"
    );
}

struct CommitFailsRewindFails;

impl AtomicMutation for CommitFailsRewindFails {
    type Output = ();

    fn transaction_id(&self) -> String {
        "commit-fails-rewind-fails".to_string()
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        let oplog_path = tx.repo().heddle_dir().join("oplog").join("oplog.bin");
        if oplog_path.exists() {
            std::fs::remove_file(&oplog_path)?;
        }
        std::fs::create_dir(&oplog_path)?;
        Ok(StagedCommit::new(
            (),
            vec![OpRecord::Snapshot {
                new_state: ChangeId::generate(),
                prev_head: None,
                thread: None,
            }],
        ))
    }

    fn rewind(&mut self, _ledger: &RewindLedger) -> Result<()> {
        Err(HeddleError::Config("rewind failed too".to_string()))
    }
}

#[test]
fn commit_failure_surfaces_rewind_failure_too() {
    let (_t, repo) = test_repo();

    let err = execute(&repo, CommitFailsRewindFails).unwrap_err();
    let message = err.to_string();

    assert!(
        message.contains("transaction failed"),
        "the original commit failure must be preserved: {message}"
    );
    assert!(
        message.contains("rewind failed too"),
        "the rewind failure must be surfaced with the original error: {message}"
    );
}

/// A leaf mutation that stages one oplog record and surfaces a value.
struct Recorder {
    state: ChangeId,
}

impl AtomicMutation for Recorder {
    type Output = u32;

    fn transaction_id(&self) -> String {
        format!("recorder-{}", self.state.to_string_full())
    }

    fn apply(&mut self, _tx: &mut Tx<'_>) -> Result<StagedCommit<u32>> {
        Ok(StagedCommit::new(
            42,
            vec![OpRecord::Snapshot {
                new_state: self.state,
                prev_head: None,
                thread: None,
            }],
        ))
    }
}

/// The happy path: `execute` surfaces the output and the commit point appends
/// the staged record plus a `TransactionCommit` marker, in one batch.
#[test]
fn execute_commits_at_the_oplog() {
    let (_t, repo) = test_repo();
    let state = ChangeId::generate();

    let out = execute(&repo, Recorder { state }).unwrap();
    assert_eq!(out, 42);

    let recent = repo.oplog().recent(8).unwrap();
    assert!(
        recent.iter().any(|e| matches!(
            &e.operation,
            OpRecord::Snapshot { new_state, .. } if *new_state == state
        )),
        "the staged snapshot record must be committed"
    );
    assert!(
        recent
            .iter()
            .any(|e| matches!(e.operation, OpRecord::TransactionCommit { .. })),
        "the commit point must append a TransactionCommit marker"
    );
}

/// An eager sub-op: the effect lands in `commit_eager`, which returns the
/// compensator. (The op-id reserve exemplar, §3.2/§5.4.)
struct Reserve {
    reserved: Rc<RefCell<bool>>,
    cancelled: Rc<RefCell<bool>>,
}

impl AtomicMutation for Reserve {
    type Output = ();

    fn transaction_id(&self) -> String {
        "reserve".to_string()
    }

    fn apply(&mut self, _tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        Ok(StagedCommit::pure(()))
    }
}

impl EagerMutation for Reserve {
    fn commit_eager(&mut self, _tx: &mut Tx<'_>) -> Result<Compensator> {
        *self.reserved.borrow_mut() = true;
        let cancelled = Rc::clone(&self.cancelled);
        Ok(Compensator::new(move || {
            *cancelled.borrow_mut() = true;
            Ok(())
        }))
    }
}

/// A composite that eagerly reserves, then fails — so the eager compensator
/// must run on the outer rollback.
struct EagerThenFail {
    reserved: Rc<RefCell<bool>>,
    cancelled: Rc<RefCell<bool>>,
    log: Rc<RefCell<Vec<u32>>>,
}

impl AtomicMutation for EagerThenFail {
    type Output = ();

    fn transaction_id(&self) -> String {
        "eager-then-fail".to_string()
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        tx.enroll_eager(Reserve {
            reserved: Rc::clone(&self.reserved),
            cancelled: Rc::clone(&self.cancelled),
        })?;
        tx.enroll(Leg {
            id: 1,
            fail: true,
            log: Rc::clone(&self.log),
        })?;
        Ok(StagedCommit::pure(()))
    }
}

/// The eager-commit exception (heddle#330 §3.2): an eagerly-committed sub-op's
/// compensator runs when the outer transaction later fails, so a leaked
/// reservation is unrepresentable. The savepoint/eager split is enforced at the
/// type level — `tx.enroll(Reserve { .. })` would not compile (`Reserve` is not
/// a `SavepointMutation`), so `enroll_eager` is the only path.
#[test]
fn eager_compensator_runs_on_outer_rollback() {
    let (_t, repo) = test_repo();
    let reserved = Rc::new(RefCell::new(false));
    let cancelled = Rc::new(RefCell::new(false));
    let log = Rc::new(RefCell::new(Vec::new()));

    let result = execute(
        &repo,
        EagerThenFail {
            reserved: Rc::clone(&reserved),
            cancelled: Rc::clone(&cancelled),
            log: Rc::clone(&log),
        },
    );

    assert!(result.is_err(), "the composite must fail");
    assert!(*reserved.borrow(), "the eager effect must have run");
    assert!(
        *cancelled.borrow(),
        "the eager compensator must run on outer rollback"
    );
}

// ---- Read-chokepoint reconciliation (heddle#330 §2.2) ----

/// Crash-replay (heddle#330 §2.4): a fork interrupted between phase 4 (oplog
/// record committed) and phase 5 (ref publish) leaves a committed-but-
/// unpublished thread ref. A read on the **long-held handle** that opened
/// BEFORE the record — the daemon cell an open-time-only pass cannot reach (cid
/// 3328112197) — must reconcile to the committed target.
#[test]
fn crash_replay_reconciles_on_long_held_handle() {
    let (_t, repo) = test_repo();
    let base = ChangeId::generate();
    repo.refs()
        .set_thread(&ThreadName::new("main"), &base)
        .unwrap();

    // Phase 4 only: append the Fork record naming the published thread, with no
    // phase-5 canonical publish of "explore".
    let forked = ChangeId::generate();
    repo.oplog()
        .record_fork(&base, &forked, Some("explore"), None)
        .unwrap();

    // The canonical "explore" ref was never written, yet the long-held handle
    // reconciles per-read (not per-open) to the committed value.
    assert_eq!(
        repo.refs().get_thread(&ThreadName::new("explore")).unwrap(),
        Some(forked),
        "long-held handle must reconcile a committed-but-unpublished ref"
    );
}

/// Crash-replay across reader shapes that share a backend: a handle opened
/// *before* a concurrent committer's fork phase-4 (another process / a second
/// `Arc<Repository>`) reconciles it on its next read, without re-opening — the
/// shared-oplog daemon cell. (A handle opened *after* a prior process's crash
/// relies on the deferred `Repository::open` eager pass, the spike's stated
/// optimization, so it is not asserted here.)
#[test]
fn crash_replay_reconciles_a_concurrent_commit() {
    let temp = TempDir::new().unwrap();
    let reader = Repository::init_default(temp.path()).unwrap();
    let base = ChangeId::generate();
    reader
        .refs()
        .set_thread(&ThreadName::new("main"), &base)
        .unwrap();

    // A second handle stands in for a concurrent process committing a fork's
    // phase-4 record without publishing the ref.
    let committer = Repository::open(temp.path()).unwrap();
    let forked = ChangeId::generate();
    committer
        .oplog()
        .record_fork(&base, &forked, Some("explore"), None)
        .unwrap();

    // The reader handle — opened before the commit, never re-opened —
    // reconciles the unpublished ref on its next read.
    assert_eq!(
        reader
            .refs()
            .get_thread(&ThreadName::new("explore"))
            .unwrap(),
        Some(forked),
        "a pre-opened handle must reconcile a concurrent committed-but-unpublished ref"
    );
}

/// All ten `RefManager` read methods funnel through `reconciled_load` and so
/// reconcile non-vacuously — including the r9 remote-thread / undo-recovery
/// classes that previously had no committed records (heddle#330 §2.2). Each
/// read below resolves a committed-but-unpublished value of its class.
#[test]
fn all_ten_readers_reconcile() {
    let (_t, repo) = test_repo();
    let scope = repo.op_scope();

    // Shared classes — committed records, canonical refs unpublished.
    let thread_state = ChangeId::generate();
    repo.oplog()
        .record_fork(&ChangeId::generate(), &thread_state, Some("ft"), None)
        .unwrap();
    let marker_state = ChangeId::generate();
    repo.oplog()
        .record_marker_create("mk", &marker_state)
        .unwrap();
    let remote_state = ChangeId::generate();
    repo.oplog()
        .record_remote_thread_update("origin", "rt", &remote_state, None)
        .unwrap();

    // Local class — undo-recovery reconciles within this checkout's lane.
    let undo_state = ChangeId::generate();
    repo.oplog()
        .record_undo_recovery_update(&undo_state, Some(&scope))
        .unwrap();

    let refs = repo.refs();
    // 1 read_head, 2 get_thread, 3 get_marker, 4 get_undo_recovery,
    // 5 get_remote_thread, 6 list_threads, 7 list_markers, 8 list_remotes,
    // 9 list_remote_threads, 10 resolve. read_head funnels through the
    // chokepoint but returns the authoritative canonical HEAD (not
    // reconstructed from the oplog in impl-a) — here the seeded "main".
    assert_eq!(
        refs.read_head().unwrap(),
        Head::Attached {
            thread: ThreadName::new("main")
        }
    );
    assert_eq!(
        refs.get_thread(&ThreadName::new("ft")).unwrap(),
        Some(thread_state)
    );
    assert_eq!(
        refs.get_marker(&MarkerName::new("mk")).unwrap(),
        Some(marker_state)
    );
    assert_eq!(refs.get_undo_recovery().unwrap(), Some(undo_state));
    assert_eq!(
        refs.get_remote_thread("origin", &ThreadName::new("rt"))
            .unwrap(),
        Some(remote_state)
    );
    assert!(
        refs.list_threads()
            .unwrap()
            .contains(&ThreadName::new("ft"))
    );
    assert!(
        refs.list_markers()
            .unwrap()
            .contains(&MarkerName::new("mk"))
    );
    assert!(refs.list_remotes().unwrap().contains(&"origin".to_string()));
    assert!(
        refs.list_remote_threads("origin")
            .unwrap()
            .contains(&ThreadName::new("rt"))
    );
    assert_eq!(refs.resolve("ft").unwrap(), Some(thread_state));
}

// ---- Write-chokepoint conformance (heddle#330 §2.2) ----

/// `commit_and_publish` records before it publishes (heddle#330 §2.2): a
/// published ref always has a preceding, ref-carrying committed record. Proven
/// by reopening (a fresh oplog read) and asserting the published thread has its
/// backing record durable.
#[test]
fn write_chokepoint_records_before_publishing() {
    let temp = TempDir::new().unwrap();
    let state = ChangeId::generate();
    {
        let repo = Repository::init_default(temp.path()).unwrap();
        let record = OpRecord::ThreadCreateV2 {
            name: "feature".to_string(),
            state,
            manager_snapshot: None,
        };
        let updates = vec![RefUpdate::Thread {
            name: ThreadName::new("feature"),
            expected: RefExpectation::Missing,
            new: Some(state),
        }];
        repo.commit_and_publish(vec![record], &updates).unwrap();
        // The ref is published on this handle.
        assert_eq!(
            repo.refs().get_thread(&ThreadName::new("feature")).unwrap(),
            Some(state)
        );
    }

    // Reopen for a fresh oplog view: the published ref has a backing record.
    let repo = Repository::open(temp.path()).unwrap();
    let recent = repo.oplog().recent(32).unwrap();
    assert!(
        recent.iter().any(|e| matches!(
            &e.operation,
            OpRecord::ThreadCreateV2 { name, .. } if name == "feature"
        )),
        "every published ref must have a preceding ref-carrying record"
    );
    assert_eq!(
        repo.refs().get_thread(&ThreadName::new("feature")).unwrap(),
        Some(state)
    );
}

/// The `Tx` accessors and the rewind ledger's error-handling paths: the
/// getters, the first-error-wins/suppress-the-rest rewind, the committed
/// early-return on a double commit, and the `Drop` backstop logging a rewind
/// failure instead of panicking.
#[test]
fn tx_accessors_and_rewind_error_paths() {
    let (_t, repo) = test_repo();

    // Accessors.
    let mut tx = Tx::root(&repo, "accessor-tx".to_string());
    assert_eq!(tx.depth(), 0);
    assert_eq!(tx.scope(), repo.op_scope());
    assert_eq!(tx.transaction_id(), "accessor-tx");
    let _ = tx.repo();
    let ledger = tx.ledger_view();
    assert_eq!(ledger.depth, 0);
    assert_eq!(ledger.scope, repo.op_scope());

    // Two failing inverses: LIFO order ⇒ the last-pushed runs first and its
    // error is surfaced; the earlier one's error is attempted then suppressed.
    tx.on_rewind(|| Err(HeddleError::Config("second".to_string())));
    tx.on_rewind(|| Err(HeddleError::Config("first".to_string())));
    let err = tx.rewind_all().unwrap_err();
    assert!(
        matches!(err, HeddleError::Config(m) if m == "first"),
        "the first rewind error (LIFO) must be the one returned"
    );

    // A second commit after a successful one is a no-op (committed guard).
    let mut tx2 = Tx::root(&repo, "double-commit-tx".to_string());
    tx2.commit(vec![OpRecord::Snapshot {
        new_state: ChangeId::generate(),
        prev_head: None,
        thread: None,
    }])
    .unwrap();
    tx2.commit(vec![OpRecord::Snapshot {
        new_state: ChangeId::generate(),
        prev_head: None,
        thread: None,
    }])
    .unwrap();
    // Only one TransactionCommit landed despite two commit calls.
    let commits = repo
        .oplog()
        .recent(64)
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.operation, OpRecord::TransactionCommit { .. }))
        .count();
    assert_eq!(
        commits, 1,
        "the committed guard suppresses the second commit"
    );

    // Drop backstop: an uncommitted Tx whose inverse fails logs (never panics).
    {
        let mut tx3 = Tx::root(&repo, "drop-backstop-tx".to_string());
        tx3.on_rewind(|| Err(HeddleError::Config("drop-time".to_string())));
        // tx3 dropped here without commit ⇒ Drop runs rewind_all, gets Err, logs.
    }
}

/// Drives the reconciler's fold over every published-ref-bearing `OpRecord`
/// shape so each `Fold::apply` arm and each list/remote projection runs. The
/// canonical refs are never published (only the records committed), so a read
/// reconciles the folded value — exercising the read chokepoint end to end.
#[test]
fn reconcile_folds_every_record_shape() {
    let (_t, repo) = test_repo();
    let scope = repo.op_scope();
    let op = repo.oplog();

    let v1 = ChangeId::generate();
    let v1b = ChangeId::generate();
    let v2 = ChangeId::generate();
    let coll = ChangeId::generate();
    let ckpt = ChangeId::generate();
    let ff = ChangeId::generate();
    let mk = ChangeId::generate();
    let remote = ChangeId::generate();
    let undo = ChangeId::generate();

    // Legacy read-back-only variants (V1 create + update).
    op.record_batch(vec![OpRecord::ThreadCreate {
        name: "t_v1".to_string(),
        state: v1,
    }])
    .unwrap();
    op.record_batch(vec![OpRecord::ThreadUpdate {
        name: "t_v1".to_string(),
        old_state: v1,
        new_state: v1b,
    }])
    .unwrap();
    // V2 create + a delete (folds to None).
    op.record_thread_create("t_v2", &v2, None, None).unwrap();
    op.record_batch(vec![OpRecord::ThreadDelete {
        name: "t_del".to_string(),
        state: v2,
    }])
    .unwrap();
    // Collapse with a thread, and one detaching HEAD (thread = None arm).
    op.record_collapse(&[v1, v1b], &coll, Some("t_coll"))
        .unwrap();
    op.record_batch(vec![OpRecord::Collapse {
        sources: vec![v1],
        result: coll,
        thread: None,
    }])
    .unwrap();
    // Checkpoint with a thread, and one without (thread = None arm).
    op.record_batch(vec![OpRecord::Checkpoint {
        parent: Some(v1),
        state: ckpt,
        thread: Some("t_ckpt".to_string()),
    }])
    .unwrap();
    op.record_batch(vec![OpRecord::Checkpoint {
        parent: None,
        state: ckpt,
        thread: None,
    }])
    .unwrap();
    // Fast-forward (V2) folds target_thread → post_target_id.
    op.record_fast_forward("t_src", "t_ff", &v1, &ff, None)
        .unwrap();
    // Fork that publishes no ref (thread = None arm).
    op.record_batch(vec![OpRecord::Fork {
        from: v1,
        new_state: v1b,
        thread: None,
        head: None,
    }])
    .unwrap();
    // Ephemeral collapse retires its thread pointer (folds to None).
    op.record_batch(vec![OpRecord::EphemeralThreadCollapse {
        thread: "t_eph".to_string(),
        final_state: v1,
    }])
    .unwrap();
    // Marker create + delete; remote update + delete; undo-recovery (local).
    op.record_marker_create("mk2", &mk).unwrap();
    op.record_batch(vec![OpRecord::MarkerDelete {
        name: "mk_del".to_string(),
        state: mk,
    }])
    .unwrap();
    op.record_remote_thread_update("origin", "rt2", &remote, None)
        .unwrap();
    op.record_remote_thread_delete("origin", "rt_del", &remote, None)
        .unwrap();
    op.record_undo_recovery_update(&undo, Some(&scope)).unwrap();

    let refs = repo.refs();

    // Point reads fold + fill the absent canonical (fill_point set arm).
    assert_eq!(
        refs.get_thread(&ThreadName::new("t_coll")).unwrap(),
        Some(coll)
    );
    assert_eq!(
        refs.get_thread(&ThreadName::new("t_ckpt")).unwrap(),
        Some(ckpt)
    );
    assert_eq!(refs.get_thread(&ThreadName::new("t_ff")).unwrap(), Some(ff));
    assert_eq!(
        refs.get_thread(&ThreadName::new("t_v1")).unwrap(),
        Some(v1b)
    );
    assert_eq!(refs.get_thread(&ThreadName::new("t_v2")).unwrap(), Some(v2));
    assert_eq!(refs.get_marker(&MarkerName::new("mk2")).unwrap(), Some(mk));
    assert_eq!(
        refs.get_remote_thread("origin", &ThreadName::new("rt2"))
            .unwrap(),
        Some(remote)
    );
    assert_eq!(refs.get_undo_recovery().unwrap(), Some(undo));

    // A deleted ref folds to None and is not filled.
    assert_eq!(refs.get_thread(&ThreadName::new("t_del")).unwrap(), None);

    // List reads exercise the ThreadList/MarkerList/RemoteList/RemoteThreadList
    // projection arms (merge_list union + remote presence filter).
    let threads = refs.list_threads().unwrap();
    assert!(threads.contains(&ThreadName::new("t_coll")));
    assert!(threads.contains(&ThreadName::new("t_ff")));
    assert!(
        refs.list_markers()
            .unwrap()
            .contains(&MarkerName::new("mk2"))
    );
    let remotes = refs.list_remotes().unwrap();
    assert!(remotes.contains(&"origin".to_string()));
    let remote_threads = refs.list_remote_threads("origin").unwrap();
    assert!(remote_threads.contains(&ThreadName::new("rt2")));
    // The deleted remote thread is absent from the projection.
    assert!(!remote_threads.contains(&ThreadName::new("rt_del")));
}

// ---- New correctness regressions (heddle#354 r2 Codex findings) ----

/// A whole-op-rewind mutation (no granular `on_rewind` inverses): it stages
/// state in `apply`, then fails. Its `rewind` — not a ledger inverse — is the
/// only thing that can undo the staged state.
struct StageThenFail {
    staged: Rc<RefCell<bool>>,
    rewound: Rc<RefCell<bool>>,
}

impl AtomicMutation for StageThenFail {
    type Output = ();

    fn transaction_id(&self) -> String {
        "stage-then-fail".to_string()
    }

    fn apply(&mut self, _tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        // Stage state, relying on the whole-op `rewind` (NOT a granular inverse)
        // to undo it — then fail.
        *self.staged.borrow_mut() = true;
        Err(HeddleError::Config(
            "apply failed after staging".to_string(),
        ))
    }

    fn rewind(&mut self, _ledger: &RewindLedger) -> Result<()> {
        *self.staged.borrow_mut() = false;
        *self.rewound.borrow_mut() = true;
        Ok(())
    }
}

impl SavepointMutation for StageThenFail {}

/// The whole-op rewind must run on the `apply`-returns-`Err` path, not only
/// after a successful `apply` (cid 3329490979). Otherwise a mutation that stages
/// state then fails leaks it: the granular ledger is empty, and the whole-op
/// `rewind` was historically registered only post-apply.
#[test]
fn whole_op_rewind_runs_on_apply_err() {
    let (_t, repo) = test_repo();
    let staged = Rc::new(RefCell::new(false));
    let rewound = Rc::new(RefCell::new(false));

    let result = execute(
        &repo,
        StageThenFail {
            staged: Rc::clone(&staged),
            rewound: Rc::clone(&rewound),
        },
    );

    assert!(result.is_err(), "the mutation must fail");
    assert!(
        *rewound.borrow(),
        "the whole-op rewind must run on the apply-Err path"
    );
    assert!(
        !*staged.borrow(),
        "a failing apply must leave zero staged state"
    );
}

struct EnrollStageThenFail {
    staged: Rc<RefCell<bool>>,
    rewound: Rc<RefCell<bool>>,
}

impl AtomicMutation for EnrollStageThenFail {
    type Output = ();

    fn transaction_id(&self) -> String {
        "enroll-stage-then-fail".to_string()
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        tx.enroll(StageThenFail {
            staged: Rc::clone(&self.staged),
            rewound: Rc::clone(&self.rewound),
        })?;
        Ok(StagedCommit::pure(()))
    }
}

#[test]
fn savepoint_whole_op_rewind_runs_on_apply_err() {
    let (_t, repo) = test_repo();
    let staged = Rc::new(RefCell::new(false));
    let rewound = Rc::new(RefCell::new(false));

    let result = execute(
        &repo,
        EnrollStageThenFail {
            staged: Rc::clone(&staged),
            rewound: Rc::clone(&rewound),
        },
    );

    assert!(result.is_err(), "the savepoint child must fail");
    assert!(
        *rewound.borrow(),
        "savepoint enrollment must pre-register the child's whole-op rewind"
    );
    assert!(
        !*staged.borrow(),
        "a failing savepoint apply must leave zero staged state"
    );
}

/// A mutation with a STABLE idempotency key derived from a field, staging one
/// oplog record. Two instances with the same key model the same logical op
/// being re-run after a crash.
struct StableKeyed {
    key: String,
    state: ChangeId,
    applied: Rc<RefCell<u32>>,
}

impl AtomicMutation for StableKeyed {
    type Output = ();

    fn transaction_id(&self) -> String {
        self.key.clone()
    }

    fn apply(&mut self, _tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        *self.applied.borrow_mut() += 1;
        Ok(StagedCommit::new(
            (),
            vec![OpRecord::Snapshot {
                new_state: self.state,
                prev_head: None,
                thread: None,
            }],
        ))
    }
}

/// A crash-retry of the same logical op presents the SAME stable
/// `transaction_id`, so the unbounded dedup scan finds the prior commit and the
/// second run is a no-op at the commit point — exactly-once (cid 3329490982).
/// With a freshly-minted-per-`execute` id the retry would commit a second time.
#[test]
fn stable_transaction_id_dedupes_crash_retry() {
    let (_t, repo) = test_repo();
    let applied = Rc::new(RefCell::new(0u32));
    let state = ChangeId::generate();
    let key = "logical-op-42".to_string();

    // First run commits.
    execute(
        &repo,
        StableKeyed {
            key: key.clone(),
            state,
            applied: Rc::clone(&applied),
        },
    )
    .unwrap();
    // The crash-retry: identical stable key ⇒ the commit deduplicates.
    execute(
        &repo,
        StableKeyed {
            key: key.clone(),
            state,
            applied: Rc::clone(&applied),
        },
    )
    .unwrap();

    let recent = repo.oplog().recent(64).unwrap();
    let commits = recent
        .iter()
        .filter(|e| {
            matches!(
                &e.operation,
                OpRecord::TransactionCommit { transaction_id, .. } if transaction_id == &key
            )
        })
        .count();
    assert_eq!(
        commits, 1,
        "a replayed op with a stable id must commit exactly once"
    );
    let snapshots = recent
        .iter()
        .filter(|e| {
            matches!(
                &e.operation,
                OpRecord::Snapshot { new_state, .. } if *new_state == state
            )
        })
        .count();
    assert_eq!(snapshots, 1, "the staged record must not be double-applied");
}

struct ReplayStages {
    key: String,
    staged_count: Rc<RefCell<u32>>,
}

impl AtomicMutation for ReplayStages {
    type Output = u32;

    fn transaction_id(&self) -> String {
        self.key.clone()
    }

    fn apply(&mut self, _tx: &mut Tx<'_>) -> Result<StagedCommit<u32>> {
        *self.staged_count.borrow_mut() += 1;
        Ok(StagedCommit::new(
            7,
            vec![OpRecord::Snapshot {
                new_state: ChangeId::generate(),
                prev_head: None,
                thread: None,
            }],
        ))
    }

    fn rewind(&mut self, _ledger: &RewindLedger) -> Result<()> {
        *self.staged_count.borrow_mut() -= 1;
        Ok(())
    }
}

#[test]
fn exact_once_replay_compensates_this_runs_staging() {
    let (_t, repo) = test_repo();
    let staged_count = Rc::new(RefCell::new(0u32));
    let key = "stable-replay-with-staging".to_string();

    let first = execute(
        &repo,
        ReplayStages {
            key: key.clone(),
            staged_count: Rc::clone(&staged_count),
        },
    )
    .unwrap();
    assert_eq!(first, 7);
    assert_eq!(*staged_count.borrow(), 1);

    let second = execute(
        &repo,
        ReplayStages {
            key,
            staged_count: Rc::clone(&staged_count),
        },
    )
    .unwrap();
    assert_eq!(second, 7);
    assert_eq!(
        *staged_count.borrow(),
        1,
        "a dedup-hit replay must rewind the staging performed by this run"
    );
}

/// A ref-expectation failure under `commit_and_publish` must append NO oplog
/// record (cid 3329490978): validation (phase 3) precedes the record append
/// (phase 4), so a record never exists for a mutation that did not publish.
#[test]
fn validation_failure_appends_no_record() {
    let (_t, repo) = test_repo();
    let existing = ChangeId::generate();

    // Publish "dup" with a backing record through the chokepoint.
    repo.commit_and_publish(
        vec![OpRecord::ThreadCreateV2 {
            name: "dup".to_string(),
            state: existing,
            manager_snapshot: None,
        }],
        &[RefUpdate::Thread {
            name: ThreadName::new("dup"),
            expected: RefExpectation::Missing,
            new: Some(existing),
        }],
    )
    .unwrap();

    // A second publish whose ref expectation FAILS: `Missing`, but "dup" exists.
    let leaked = ChangeId::generate();
    let result = repo.commit_and_publish(
        vec![OpRecord::Fork {
            from: existing,
            new_state: leaked,
            thread: Some("dup".to_string()),
            head: None,
        }],
        &[RefUpdate::Thread {
            name: ThreadName::new("dup"),
            expected: RefExpectation::Missing,
            new: Some(leaked),
        }],
    );
    assert!(
        result.is_err(),
        "a Missing expectation must fail when the thread already exists"
    );

    // The Fork record must NOT have been appended — validation precedes commit.
    let recent = repo.oplog().recent(64).unwrap();
    assert!(
        !recent.iter().any(|e| matches!(
            &e.operation,
            OpRecord::Fork { new_state, .. } if *new_state == leaked
        )),
        "no oplog record may be appended when ref-expectation validation fails"
    );
}

/// Concurrent `commit_and_publish` to the same ref (permissive `Any`
/// expectations) must not let the record order diverge from the publish order
/// (cid 3329490984): the record append and the ref publish happen as one unit
/// under the refs lock, so the last-committed record for a ref is always the one
/// whose value is published. A fresh handle (watermark seeded to the current
/// generation) reads the RAW published canonical, which must equal the
/// last-committed Fork record's value.
#[test]
fn concurrent_commit_and_publish_serializes_record_and_publish() {
    for _ in 0..10 {
        let temp = TempDir::new().unwrap();
        {
            let repo = Repository::init_default(temp.path()).unwrap();
            let base = ChangeId::generate();
            repo.refs()
                .set_thread(&ThreadName::new("main"), &base)
                .unwrap();
        }
        let va = ChangeId::generate();
        let vb = ChangeId::generate();
        let path = temp.path().to_path_buf();

        std::thread::scope(|s| {
            for v in [va, vb] {
                let p = path.clone();
                s.spawn(move || {
                    let repo = Repository::open(&p).unwrap();
                    let _ = repo.commit_and_publish(
                        vec![OpRecord::Fork {
                            from: v,
                            new_state: v,
                            thread: Some("main".to_string()),
                            head: None,
                        }],
                        &[RefUpdate::Thread {
                            name: ThreadName::new("main"),
                            expected: RefExpectation::Any,
                            new: Some(v),
                        }],
                    );
                });
            }
        });

        let reader = Repository::open(temp.path()).unwrap();
        let published = reader.refs().get_thread(&ThreadName::new("main")).unwrap();
        let last_fork = reader
            .oplog()
            .recent(64)
            .unwrap()
            .into_iter()
            .filter(|e| {
                matches!(
                    &e.operation,
                    OpRecord::Fork { thread: Some(t), .. } if t == "main"
                )
            })
            .max_by_key(|e| e.id)
            .map(|e| match e.operation {
                OpRecord::Fork { new_state, .. } => new_state,
                _ => unreachable!(),
            });

        assert_eq!(
            published, last_fork,
            "the published ref must equal the last-committed record's value (no interleave)"
        );
    }
}

/// A crash-interrupted UPDATE to an ALREADY-EXISTING ref (cid 3329490981):
/// `cmd_collapse` on attached "main" records the `Collapse` (phase 4) then
/// crashes before publishing main's new value (phase 5). On recovery "main"
/// already exists, so the prior fill-if-absent rule kept the STALE value — the
/// committed update must now win.
#[test]
fn crash_replay_reconciles_update_to_existing_ref() {
    let (temp, repo) = test_repo();
    let base = ChangeId::generate();
    repo.refs()
        .set_thread(&ThreadName::new("main"), &base)
        .unwrap();

    // Phase 4 only: record a Collapse updating "main", with no phase-5 publish.
    let result = ChangeId::generate();
    repo.oplog()
        .record_collapse(&[base], &result, Some("main"))
        .unwrap();

    assert_eq!(
        repo.refs().get_thread(&ThreadName::new("main")).unwrap(),
        Some(result),
        "a crash-replayed UPDATE to an existing ref must materialize the committed value"
    );

    // The read materialized the canonical too: a fresh handle reads it raw.
    let reader = Repository::open(temp.path()).unwrap();
    assert_eq!(
        reader.refs().get_thread(&ThreadName::new("main")).unwrap(),
        Some(result),
        "the committed update must be materialized to the canonical ref"
    );
}

#[test]
fn crash_replay_reconciles_delete_in_point_and_list_reads() {
    let (_t, repo) = test_repo();
    let state = ChangeId::generate();
    let deleted = ThreadName::new("deleted");
    repo.refs().set_thread(&deleted, &state).unwrap();

    repo.oplog()
        .record_batch(vec![OpRecord::ThreadDelete {
            name: deleted.to_string(),
            state,
        }])
        .unwrap();

    assert!(
        !repo.refs().list_threads().unwrap().contains(&deleted),
        "a committed-but-unpublished delete must be absent from list reads"
    );
    assert_eq!(
        repo.refs().get_thread(&deleted).unwrap(),
        None,
        "a committed-but-unpublished delete must win for point reads"
    );
}

#[test]
fn crash_replay_reconciles_marker_delete_in_list_reads() {
    let (_t, repo) = test_repo();
    let state = ChangeId::generate();
    let deleted = MarkerName::new("deleted-marker");
    repo.refs().create_marker(&deleted, &state).unwrap();

    repo.oplog()
        .record_batch(vec![OpRecord::MarkerDelete {
            name: deleted.to_string(),
            state,
        }])
        .unwrap();

    assert!(
        !repo.refs().list_markers().unwrap().contains(&deleted),
        "a committed-but-unpublished marker delete must be absent from list reads"
    );
    assert_eq!(
        repo.refs().get_marker(&deleted).unwrap(),
        None,
        "a committed-but-unpublished marker delete must win for point reads"
    );
}

#[test]
fn crash_replay_reconciles_remote_thread_create_and_delete_in_lists() {
    let (_t, repo) = test_repo();
    let original = ChangeId::generate();
    let replacement = ChangeId::generate();
    let deleted = ThreadName::new("deleted-remote");
    let created = ThreadName::new("created-remote");
    repo.refs()
        .set_remote_thread("origin", &deleted, &original)
        .unwrap();

    repo.oplog()
        .record_remote_thread_delete("origin", deleted.as_ref(), &original, None)
        .unwrap();
    repo.oplog()
        .record_remote_thread_update("upstream", created.as_ref(), &replacement, None)
        .unwrap();

    let remotes = repo.refs().list_remotes().unwrap();
    assert!(
        !remotes.contains(&"origin".to_string()),
        "a remote with only a committed-but-unpublished deleted thread must be absent"
    );
    assert!(
        remotes.contains(&"upstream".to_string()),
        "a remote with a committed-but-unpublished created thread must be present"
    );

    let origin_threads = repo.refs().list_remote_threads("origin").unwrap();
    assert!(
        !origin_threads.contains(&deleted),
        "a committed-but-unpublished remote-thread delete must be absent from list reads"
    );
    let upstream_threads = repo.refs().list_remote_threads("upstream").unwrap();
    assert!(
        upstream_threads.contains(&created),
        "a committed-but-unpublished remote-thread create must be present in list reads"
    );

    assert_eq!(
        repo.refs().get_remote_thread("origin", &deleted).unwrap(),
        None,
        "a committed-but-unpublished remote-thread delete must win for point reads"
    );
    assert_eq!(
        repo.refs().get_remote_thread("upstream", &created).unwrap(),
        Some(replacement),
        "a committed-but-unpublished remote-thread create must win for point reads"
    );
}

#[test]
fn crash_replay_reconstructs_committed_head_update() {
    let (_t, repo) = test_repo();
    let base = ChangeId::generate();
    repo.refs()
        .set_thread(&ThreadName::new("main"), &base)
        .unwrap();
    repo.refs()
        .write_head(&Head::Attached {
            thread: ThreadName::new("main"),
        })
        .unwrap();

    let detached = ChangeId::generate();
    repo.oplog()
        .record_batch_scoped(
            vec![OpRecord::Fork {
                from: base,
                new_state: detached,
                thread: None,
                head: Some(detached),
            }],
            Some(&repo.op_scope()),
        )
        .unwrap();

    assert_eq!(
        repo.refs().read_head().unwrap(),
        Head::Detached { state: detached },
        "a committed HEAD publish must be reconstructed after phase-4 crash"
    );
}

// ---- HEAD-class closure (heddle#354 r4) ----
//
// HEAD reconciliation must fold the LATEST HEAD-moving record of ANY shape, not
// a Fork/Collapse allowlist. A publish-first mover (Goto / FastForward /
// detached Snapshot) that lands AFTER an atomic Fork must mask the fork so the
// reconcile cannot resurrect the fork's stale HEAD over the live canonical.
// Each test below records a Fork (whose reconstruction a Fork/Collapse-only
// allowlist would republish) then a later mover, on a handle whose watermark
// predates the fork, and asserts HEAD is the later (canonical) value.

/// Fork-then-goto: a `goto` recorded after a `Fork` must win. The goto writes
/// canonical HEAD directly (publish-first) before recording; the reconcile must
/// defer to that canonical, never republish the stale fork target.
#[test]
fn fork_then_goto_reconcile_yields_goto_target_not_stale_fork() {
    let (_t, repo) = test_repo();
    let scope = repo.op_scope();
    let base = ChangeId::generate();
    repo.refs()
        .set_thread(&ThreadName::new("main"), &base)
        .unwrap();
    repo.refs()
        .write_head(&Head::Attached {
            thread: ThreadName::new("main"),
        })
        .unwrap();

    // An atomic Fork committed (phase 4) a published HEAD = Detached{fork_a}.
    let fork_a = ChangeId::generate();
    repo.oplog()
        .record_batch_scoped(
            vec![OpRecord::Fork {
                from: base,
                new_state: fork_a,
                thread: None,
                head: Some(fork_a),
            }],
            Some(&scope),
        )
        .unwrap();

    // `goto goto_b` then writes canonical HEAD = Detached{goto_b} DIRECTLY
    // (publish-first) and records a `Goto`. The direct write is the canonical
    // the reconciler reads; the record masks the older Fork.
    let goto_b = ChangeId::generate();
    repo.refs()
        .write_head(&Head::Detached { state: goto_b })
        .unwrap();
    repo.oplog()
        .record_batch_scoped(
            vec![OpRecord::Goto {
                target: goto_b,
                prev_head: Some(fork_a),
            }],
            Some(&scope),
        )
        .unwrap();

    assert_eq!(
        repo.refs().read_head().unwrap(),
        Head::Detached { state: goto_b },
        "a goto recorded after a fork must win — the stale fork must not resurrect"
    );
}

/// FastForward-after-Fork: a fast-forward re-attaches HEAD and writes it
/// directly before recording `FastForwardV2`. The reconcile must defer to the
/// re-attached canonical HEAD, not the stale fork. (This is the exact
/// stale-HEAD clobber a Fork/Collapse-only allowlist re-opens: `FastForwardV2`
/// is publish-first, so the fork's reconstruction must be masked.)
#[test]
fn fast_forward_after_fork_reconcile_defers_to_reattached_head() {
    let (_t, repo) = test_repo();
    let scope = repo.op_scope();
    let base = ChangeId::generate();
    repo.refs()
        .set_thread(&ThreadName::new("main"), &base)
        .unwrap();
    repo.refs()
        .write_head(&Head::Attached {
            thread: ThreadName::new("main"),
        })
        .unwrap();

    let fork_a = ChangeId::generate();
    repo.oplog()
        .record_batch_scoped(
            vec![OpRecord::Fork {
                from: base,
                new_state: fork_a,
                thread: None,
                head: Some(fork_a),
            }],
            Some(&scope),
        )
        .unwrap();

    // A fast-forward advances "main" to ff_target and re-attaches HEAD
    // (`fast_forward_attached`): canonical HEAD = Attached{main}, written
    // directly before the FastForwardV2 record.
    let ff_target = ChangeId::generate();
    repo.refs()
        .set_thread(&ThreadName::new("main"), &ff_target)
        .unwrap();
    repo.refs()
        .write_head(&Head::Attached {
            thread: ThreadName::new("main"),
        })
        .unwrap();
    repo.oplog()
        .record_fast_forward("src", "main", &base, &ff_target, Some(&scope))
        .unwrap();

    assert_eq!(
        repo.refs().read_head().unwrap(),
        Head::Attached {
            thread: ThreadName::new("main")
        },
        "a fast-forward after a fork must defer to the re-attached canonical HEAD, not resurrect the fork"
    );
}

/// Detached-Snapshot-after-Fork: a detached snapshot writes HEAD = Detached
/// directly before recording. The reconcile must yield the snapshot's HEAD, not
/// the stale fork.
#[test]
fn detached_snapshot_after_fork_reconcile_yields_snapshot_head() {
    let (_t, repo) = test_repo();
    let scope = repo.op_scope();
    let base = ChangeId::generate();
    repo.refs()
        .set_thread(&ThreadName::new("main"), &base)
        .unwrap();
    repo.refs()
        .write_head(&Head::Attached {
            thread: ThreadName::new("main"),
        })
        .unwrap();

    let fork_a = ChangeId::generate();
    repo.oplog()
        .record_batch_scoped(
            vec![OpRecord::Fork {
                from: base,
                new_state: fork_a,
                thread: None,
                head: Some(fork_a),
            }],
            Some(&scope),
        )
        .unwrap();

    // A detached snapshot writes HEAD = Detached{snap_b} directly, then records.
    let snap_b = ChangeId::generate();
    repo.refs()
        .write_head(&Head::Detached { state: snap_b })
        .unwrap();
    repo.oplog()
        .record_batch_scoped(
            vec![OpRecord::Snapshot {
                new_state: snap_b,
                prev_head: Some(fork_a),
                thread: None,
            }],
            Some(&scope),
        )
        .unwrap();

    assert_eq!(
        repo.refs().read_head().unwrap(),
        Head::Detached { state: snap_b },
        "a detached snapshot after a fork must win — the stale fork must not resurrect"
    );
}

// ---- Eager one-mechanism guard (heddle#354 r4) ----

/// An eager mutation that OVERRIDES `rewind` (sets `rewound`) and whose
/// `commit_eager` returns a compensator (sets `compensated`). The one-mechanism
/// guard must run ONLY the compensator on outer rollback — never the overridden
/// whole-op rewind, which would double-undo.
struct EagerOverrideRewind {
    compensated: Rc<RefCell<bool>>,
    rewound: Rc<RefCell<bool>>,
}

impl AtomicMutation for EagerOverrideRewind {
    type Output = ();

    fn transaction_id(&self) -> String {
        "eager-override-rewind".to_string()
    }

    fn apply(&mut self, _tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        Ok(StagedCommit::pure(()))
    }

    fn rewind(&mut self, _ledger: &RewindLedger) -> Result<()> {
        *self.rewound.borrow_mut() = true;
        Ok(())
    }
}

impl EagerMutation for EagerOverrideRewind {
    fn commit_eager(&mut self, _tx: &mut Tx<'_>) -> Result<Compensator> {
        let compensated = Rc::clone(&self.compensated);
        Ok(Compensator::new(move || {
            *compensated.borrow_mut() = true;
            Ok(())
        }))
    }
}

struct EagerOverrideThenFail {
    compensated: Rc<RefCell<bool>>,
    rewound: Rc<RefCell<bool>>,
    log: Rc<RefCell<Vec<u32>>>,
}

impl AtomicMutation for EagerOverrideThenFail {
    type Output = ();

    fn transaction_id(&self) -> String {
        "eager-override-then-fail".to_string()
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        tx.enroll_eager(EagerOverrideRewind {
            compensated: Rc::clone(&self.compensated),
            rewound: Rc::clone(&self.rewound),
        })?;
        tx.enroll(Leg {
            id: 1,
            fail: true,
            log: Rc::clone(&self.log),
        })?;
        Ok(StagedCommit::pure(()))
    }
}

/// The one-mechanism guard: an `EagerMutation` that overrides `rewind` must not
/// double-undo. After `commit_eager` succeeds, the pre-registered whole-op
/// rewind is taken out of play, so an outer rollback runs ONLY the compensator.
#[test]
fn eager_override_rewind_does_not_double_undo() {
    let (_t, repo) = test_repo();
    let compensated = Rc::new(RefCell::new(false));
    let rewound = Rc::new(RefCell::new(false));
    let log = Rc::new(RefCell::new(Vec::new()));

    let result = execute(
        &repo,
        EagerOverrideThenFail {
            compensated: Rc::clone(&compensated),
            rewound: Rc::clone(&rewound),
            log: Rc::clone(&log),
        },
    );

    assert!(result.is_err(), "the composite must fail");
    assert!(
        *compensated.borrow(),
        "the eager compensator must run on outer rollback"
    );
    assert!(
        !*rewound.borrow(),
        "the overridden whole-op rewind must NOT run for an eager mutation (no double-undo)"
    );
}
