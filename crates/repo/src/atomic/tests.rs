// SPDX-License-Identifier: Apache-2.0
//! Unit tests for the atomic-mutation primitive (heddle#330 §7 item 1).

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;

use objects::error::{HeddleError, Result};
use objects::object::{ChangeId, MarkerName, ThreadName};
use oplog::{OpLogBackend, OpRecord};
use refs::{Head, RefExpectation, RefUpdate};
use tempfile::TempDir;

use super::{execute, AtomicMutation, Compensator, EagerMutation, SavepointMutation, StagedCommit, Tx};
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

    let result = execute(&repo, FailingComposite {
        log: Rc::clone(&log),
    });

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

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        let log = Rc::clone(&self.log);
        tx.on_rewind(move || {
            log.borrow_mut().push(99);
            Ok(())
        });
        panic!("apply blew up");
    }
}

/// The `Drop` backstop (heddle#330 §4): a panic that unwinds through `apply`
/// still runs the reverse-order rewind and never commits.
#[test]
fn panic_unwind_runs_drop_backstop() {
    let (_t, repo) = test_repo();
    let log = Rc::new(RefCell::new(Vec::new()));

    let caught = catch_unwind(AssertUnwindSafe(|| {
        let _ = execute(&repo, Panicker {
            log: Rc::clone(&log),
        });
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

/// A leaf mutation that stages one oplog record and surfaces a value.
struct Recorder {
    state: ChangeId,
}

impl AtomicMutation for Recorder {
    type Output = u32;

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

    let result = execute(&repo, EagerThenFail {
        reserved: Rc::clone(&reserved),
        cancelled: Rc::clone(&cancelled),
        log: Rc::clone(&log),
    });

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
    repo.refs().set_thread(&ThreadName::new("main"), &base).unwrap();

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
    reader.refs().set_thread(&ThreadName::new("main"), &base).unwrap();

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
        reader.refs().get_thread(&ThreadName::new("explore")).unwrap(),
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
    repo.oplog().record_marker_create("mk", &marker_state).unwrap();
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
    assert_eq!(refs.get_thread(&ThreadName::new("ft")).unwrap(), Some(thread_state));
    assert_eq!(refs.get_marker(&MarkerName::new("mk")).unwrap(), Some(marker_state));
    assert_eq!(refs.get_undo_recovery().unwrap(), Some(undo_state));
    assert_eq!(
        refs.get_remote_thread("origin", &ThreadName::new("rt")).unwrap(),
        Some(remote_state)
    );
    assert!(refs.list_threads().unwrap().contains(&ThreadName::new("ft")));
    assert!(refs.list_markers().unwrap().contains(&MarkerName::new("mk")));
    assert!(refs.list_remotes().unwrap().contains(&"origin".to_string()));
    assert!(refs
        .list_remote_threads("origin")
        .unwrap()
        .contains(&ThreadName::new("rt")));
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
