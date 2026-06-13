// SPDX-License-Identifier: Apache-2.0
//! Unit tests for the atomic-mutation primitive (heddle#330 §7 item 1).

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;

use objects::error::{HeddleError, Result};
use objects::object::{ChangeId, ContentHash, MarkerName, ThreadName, VisibilityTier};
use oplog::{
    isolation_keys_for_record, ConditionalCommitOutcome, IsolationKey, IsolationPrecondition,
    OpLogBackend, OpRecord, ThreadUpdateSnapshots,
};
use refs::{Head, RefExpectation, RefManager, RefUpdate};
use tempfile::TempDir;

use super::{
    execute, AtomicMutation, Compensator, DeferredMutation, EagerMutation, RewindLedger,
    StagedCommit, Tx,
};
use crate::Repository;

fn test_repo() -> (TempDir, Repository) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    (temp, repo)
}

macro_rules! empty_isolation_keys {
    () => {
        fn isolation_keys(&self, _repo: &Repository) -> Result<BTreeSet<IsolationKey>> {
            Ok(BTreeSet::new())
        }
    };
}

fn test_precondition() -> IsolationPrecondition {
    IsolationPrecondition {
        since_head_id: 0,
        keys: BTreeSet::new(),
    }
}

fn thread_precondition(since_head_id: u64, thread: &str) -> IsolationPrecondition {
    let mut keys = BTreeSet::new();
    keys.insert(IsolationKey::Thread(thread.to_string()));
    IsolationPrecondition {
        since_head_id,
        keys,
    }
}

fn snapshot_on(thread: &str) -> OpRecord {
    let state = ChangeId::generate();
    OpRecord::Snapshot {
        new_state: state,
        prev_head: None,
        head: None,
        thread: Some(thread.to_string()),
    }
}

fn commit_marker(transaction_id: &str, op_count: u32) -> OpRecord {
    OpRecord::TransactionCommit {
        transaction_id: transaction_id.to_string(),
        op_count,
    }
}

fn visibility_set_on(state: ChangeId) -> OpRecord {
    OpRecord::StateVisibilitySet {
        state,
        record_id: ContentHash::from_bytes([7u8; 32]),
        tier: VisibilityTier::Internal,
        prior_sidecar: None,
        new_sidecar: Some(vec![1, 2, 3]),
    }
}

fn visibility_precondition(since_head_id: u64, state: ChangeId) -> IsolationPrecondition {
    let mut keys = BTreeSet::new();
    keys.insert(IsolationKey::StateVisibility(state));
    IsolationPrecondition {
        since_head_id,
        keys,
    }
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

    empty_isolation_keys!();

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        let id = self.id;
        let log = Rc::clone(&self.log);
        tx.on_rewind("test-leg", move || {
            log.borrow_mut().push(id);
            Ok(())
        });
        if self.fail {
            return Err(HeddleError::Config(format!("leg {id} failed")));
        }
        Ok(StagedCommit::pure(()))
    }
}

impl DeferredMutation for Leg {}

/// A composite that enrolls three legs; the third fails.
struct FailingComposite {
    log: Rc<RefCell<Vec<u32>>>,
}

impl AtomicMutation for FailingComposite {
    type Output = ();

    fn transaction_id(&self) -> String {
        "failing-composite".to_string()
    }

    empty_isolation_keys!();

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

/// `Tx::step` ordering — the heddle#355 hardening. A `forward` that returns
/// `Err` must leave the ledger EMPTY: no inverse is registered for an effect
/// that never happened, so a later unwind compensates nothing. This is the
/// invariant that makes the register-then-forward footgun unrepresentable.
#[test]
fn step_registers_no_inverse_when_forward_fails() {
    let (_t, repo) = test_repo();
    let mut tx = Tx::root(&repo, "step-forward-fails".to_string(), test_precondition());
    let inverse_ran = Rc::new(RefCell::new(false));
    let flag = Rc::clone(&inverse_ran);

    let result: Result<()> = tx.step(
        || Err(HeddleError::Config("forward failed".to_string())),
        move || {
            *flag.borrow_mut() = true;
            Ok(())
        },
    );

    assert!(result.is_err(), "step surfaces the forward's error");
    // Unwind: with an empty ledger this is a no-op and the inverse never runs.
    tx.rewind_all().unwrap();
    assert!(
        !*inverse_ran.borrow(),
        "a forward that failed must register NO inverse"
    );
}

/// `Tx::step` happy path: a successful `forward` returns its value and registers
/// EXACTLY ONE inverse, which runs (once) on a later unwind — not before.
#[test]
fn step_registers_one_inverse_after_forward_succeeds() {
    let (_t, repo) = test_repo();
    let mut tx = Tx::root(&repo, "step-forward-ok".to_string(), test_precondition());
    let unwound = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&unwound);
    let forward_ran = Rc::new(RefCell::new(false));
    let observed = Rc::clone(&forward_ran);

    let value = tx
        .step(
            || {
                *observed.borrow_mut() = true;
                Ok(7u32)
            },
            move || {
                sink.borrow_mut().push(1u32);
                Ok(())
            },
        )
        .unwrap();

    assert_eq!(value, 7, "step returns the forward's produced value");
    assert!(*forward_ran.borrow(), "forward ran");
    assert!(
        unwound.borrow().is_empty(),
        "the inverse must NOT run until the transaction unwinds"
    );

    tx.rewind_all().unwrap();
    assert_eq!(
        *unwound.borrow(),
        vec![1],
        "exactly one inverse was registered and it runs once on unwind"
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

    empty_isolation_keys!();

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        let log = Rc::clone(&self.log);
        tx.on_rewind("test-panicker", move || {
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

    empty_isolation_keys!();

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

    empty_isolation_keys!();

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        let oplog_path = tx.repo().heddle_dir().join("oplog").join("oplog.bin");
        if oplog_path.exists() {
            std::fs::remove_file(&oplog_path)?;
        }
        std::fs::create_dir(&oplog_path)?;
        let state = ChangeId::generate();
        Ok(StagedCommit::new(
            (),
            vec![OpRecord::Snapshot {
                new_state: state,
                prev_head: None,
                head: Some(state),
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

    empty_isolation_keys!();

    fn apply(&mut self, _tx: &mut Tx<'_>) -> Result<StagedCommit<u32>> {
        Ok(StagedCommit::new(
            42,
            vec![OpRecord::Snapshot {
                new_state: self.state,
                prev_head: None,
                head: Some(self.state),
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

    empty_isolation_keys!();

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

    empty_isolation_keys!();

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
/// reservation is unrepresentable. The deferred/eager split is enforced at the
/// type level — `tx.enroll(Reserve { .. })` would not compile (`Reserve` is not
/// a `DeferredMutation`), so `enroll_eager` is the only path.
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
        .record_fork(&base, &forked, Some("explore"), None, None)
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
        .record_fork(&base, &forked, Some("explore"), None, None)
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
        .record_fork(&ChangeId::generate(), &thread_state, Some("ft"), None, None)
        .unwrap();
    let marker_state = ChangeId::generate();
    repo.oplog()
        .record_marker_create(&MarkerName::new("mk"), &marker_state)
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
    assert!(refs
        .list_threads()
        .unwrap()
        .contains(&ThreadName::new("ft")));
    assert!(refs
        .list_markers()
        .unwrap()
        .contains(&MarkerName::new("mk")));
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
        let record = OpRecord::ThreadCreate {
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
            OpRecord::ThreadCreate { name, .. } if name == "feature"
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
    let mut tx = Tx::root(&repo, "accessor-tx".to_string(), test_precondition());
    assert_eq!(tx.depth(), 0);
    assert_eq!(tx.scope(), repo.op_scope());
    assert_eq!(tx.transaction_id(), "accessor-tx");
    let _ = tx.repo();
    let ledger = tx.ledger_view();
    assert_eq!(ledger.depth, 0);
    assert_eq!(ledger.scope, repo.op_scope());

    // Two failing inverses: LIFO order ⇒ the last-pushed runs first and its
    // error is surfaced; the earlier one's error is attempted then suppressed.
    tx.on_rewind("second", || Err(HeddleError::Config("second".to_string())));
    tx.on_rewind("first", || Err(HeddleError::Config("first".to_string())));
    let err = tx.rewind_all().unwrap_err();
    assert!(
        matches!(err, HeddleError::Config(m) if m == "first"),
        "the first rewind error (LIFO) must be the one returned"
    );

    // A second commit after a successful one is a no-op (committed guard).
    let mut tx2 = Tx::root(&repo, "double-commit-tx".to_string(), test_precondition());
    let first_state = ChangeId::generate();
    tx2.commit(vec![OpRecord::Snapshot {
        new_state: first_state,
        prev_head: None,
        head: Some(first_state),
        thread: None,
    }])
    .unwrap();
    let second_state = ChangeId::generate();
    tx2.commit(vec![OpRecord::Snapshot {
        new_state: second_state,
        prev_head: None,
        head: Some(second_state),
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
        let mut tx3 = Tx::root(&repo, "drop-backstop-tx".to_string(), test_precondition());
        tx3.on_rewind("drop-time", || {
            Err(HeddleError::Config("drop-time".to_string()))
        });
        // tx3 dropped here without commit ⇒ Drop runs rewind_all, gets Err, logs.
    }
}

struct ConflictOnce {
    transaction_id: String,
    thread: String,
    applied: Rc<RefCell<u32>>,
    rewound: Rc<RefCell<u32>>,
    injected: Rc<RefCell<bool>>,
}

impl AtomicMutation for ConflictOnce {
    type Output = u32;

    fn transaction_id(&self) -> String {
        self.transaction_id.clone()
    }

    fn isolation_keys(&self, _repo: &Repository) -> Result<BTreeSet<IsolationKey>> {
        let mut keys = BTreeSet::new();
        keys.insert(IsolationKey::Thread(self.thread.clone()));
        Ok(keys)
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<u32>> {
        *self.applied.borrow_mut() += 1;
        let rewound = Rc::clone(&self.rewound);
        tx.on_rewind("conflict-once", move || {
            *rewound.borrow_mut() += 1;
            Ok(())
        });
        if !*self.injected.borrow() {
            *self.injected.borrow_mut() = true;
            tx.repo().oplog().record_batch(vec![
                snapshot_on(&self.thread),
                commit_marker("racer-once", 1),
            ])?;
        }
        Ok(StagedCommit::new(
            *self.applied.borrow(),
            vec![snapshot_on(&self.thread)],
        ))
    }
}

#[test]
fn same_thread_conflict_rewinds_and_retries_successfully() {
    let (_t, repo) = test_repo();
    let applied = Rc::new(RefCell::new(0));
    let rewound = Rc::new(RefCell::new(0));
    let injected = Rc::new(RefCell::new(false));

    let output = execute(
        &repo,
        ConflictOnce {
            transaction_id: "retry-main".to_string(),
            thread: "main".to_string(),
            applied: Rc::clone(&applied),
            rewound: Rc::clone(&rewound),
            injected,
        },
    )
    .unwrap();

    assert_eq!(output, 2, "the second attempt must be the committed run");
    assert_eq!(*applied.borrow(), 2, "the logical mutation retried once");
    assert_eq!(*rewound.borrow(), 1, "the conflicted staging was rewound");
    let commits = repo
        .oplog()
        .recent(16)
        .unwrap()
        .into_iter()
        .filter(|entry| {
            matches!(
                &entry.operation,
                OpRecord::TransactionCommit { transaction_id, .. }
                    if transaction_id == "retry-main"
            )
        })
        .count();
    assert_eq!(commits, 1, "the retried transaction commits once");
}

#[test]
fn conditional_commit_allows_different_thread_tail() {
    let (_t, repo) = test_repo();
    let since = repo.oplog().head_id().unwrap();
    let precondition = thread_precondition(since, "main");

    repo.oplog()
        .record_batch(vec![
            snapshot_on("feature"),
            commit_marker("feature-advance", 1),
        ])
        .unwrap();

    let outcome = repo
        .oplog()
        .record_batch_exactly_once_if_unchanged(
            vec![snapshot_on("main"), commit_marker("main-after-feature", 1)],
            Some(&repo.op_scope()),
            "main-after-feature",
            &precondition,
        )
        .unwrap();
    assert!(matches!(outcome, ConditionalCommitOutcome::Committed(_)));
}

#[test]
fn visibility_records_contribute_per_state_isolation_key() {
    // Invariant 3 foundation: a StateVisibilitySet/Promote on state S touches
    // the per-state key StateVisibility(S) — and only that state's key, so
    // mutations on distinct states never spuriously conflict.
    let s = ChangeId::generate();
    let other = ChangeId::generate();

    let set_keys = isolation_keys_for_record(&visibility_set_on(s), Some("scope"));
    assert!(set_keys.contains(&IsolationKey::StateVisibility(s)));
    assert!(!set_keys.contains(&IsolationKey::StateVisibility(other)));

    let promote = OpRecord::StateVisibilityPromote {
        state: s,
        superseded: ContentHash::from_bytes([2u8; 32]),
        record_id: ContentHash::from_bytes([3u8; 32]),
        tier: VisibilityTier::Public,
        prior_sidecar: Some(vec![9]),
        new_sidecar: None,
    };
    let promote_keys = isolation_keys_for_record(&promote, Some("scope"));
    assert!(promote_keys.contains(&IsolationKey::StateVisibility(s)));
}

#[test]
fn undo_cannot_discard_concurrent_visibility_change() {
    // Invariant 3 — a visibility mutation on state S carries a per-state
    // isolation key, so an in-flight undo of a visibility batch on S conflicts
    // with a concurrently-committed newer visibility record on S: the undo is
    // rejected (serialized), never allowed to restore an older prior_sidecar
    // over the newer record. Modeled at the conditional-commit layer the undo
    // executor commits through.
    let (_t, repo) = test_repo();
    let state = ChangeId::generate();

    // A visibility batch B1 on S commits (the batch the undo targets).
    repo.oplog()
        .record_batch(vec![visibility_set_on(state)])
        .unwrap();

    // The undo of B1 snapshots the head HERE and declares S's visibility key
    // (the executor derives this from B1's records via isolation_keys_for_record).
    let since = repo.oplog().head_id().unwrap();
    let precondition = visibility_precondition(since, state);

    // A concurrent visibility change on S commits AFTER the undo's snapshot.
    repo.oplog()
        .record_batch(vec![visibility_set_on(state)])
        .unwrap();

    // The undo's conditional commit must detect the conflict and refuse.
    let outcome = repo
        .oplog()
        .record_batch_exactly_once_if_unchanged(
            vec![commit_marker("undo:batch:1", 1)],
            Some(&repo.op_scope()),
            "undo:batch:1",
            &precondition,
        )
        .unwrap();
    assert!(
        matches!(
            outcome,
            ConditionalCommitOutcome::IsolationConflict {
                key: IsolationKey::StateVisibility(s),
                ..
            } if s == state
        ),
        "a concurrent visibility change on S must block the undo: {outcome:?}"
    );

    // Control: a concurrent visibility change on a DIFFERENT state does NOT
    // block the undo — visibility mutations on distinct states are independent.
    let since_ctrl = repo.oplog().head_id().unwrap();
    let precondition_ctrl = visibility_precondition(since_ctrl, state);
    let other = ChangeId::generate();
    repo.oplog()
        .record_batch(vec![visibility_set_on(other)])
        .unwrap();
    let outcome_ctrl = repo
        .oplog()
        .record_batch_exactly_once_if_unchanged(
            vec![commit_marker("undo:batch:ctrl", 1)],
            Some(&repo.op_scope()),
            "undo:batch:ctrl",
            &precondition_ctrl,
        )
        .unwrap();
    assert!(
        matches!(outcome_ctrl, ConditionalCommitOutcome::Committed(_)),
        "a visibility change on a DIFFERENT state must NOT block the undo: {outcome_ctrl:?}"
    );
}

#[test]
fn conditional_commit_dedups_before_isolation_scan() {
    let (_t, repo) = test_repo();
    let precondition = thread_precondition(repo.oplog().head_id().unwrap(), "main");
    let first = repo
        .oplog()
        .record_batch_exactly_once_if_unchanged(
            vec![snapshot_on("main"), commit_marker("dedup-main", 1)],
            Some(&repo.op_scope()),
            "dedup-main",
            &precondition,
        )
        .unwrap();
    assert!(matches!(first, ConditionalCommitOutcome::Committed(_)));

    repo.oplog()
        .record_batch(vec![snapshot_on("main"), commit_marker("main-advanced", 1)])
        .unwrap();

    let replay = repo
        .oplog()
        .record_batch_exactly_once_if_unchanged(
            vec![snapshot_on("main"), commit_marker("dedup-main", 1)],
            Some(&repo.op_scope()),
            "dedup-main",
            &precondition,
        )
        .unwrap();
    assert!(
        matches!(replay, ConditionalCommitOutcome::AlreadyCommitted(records) if records.len() == 1),
        "a replay must dedup even after the isolated thread advanced"
    );
}

struct SavepointConflict {
    log: Rc<RefCell<Vec<u32>>>,
    injected: Rc<RefCell<bool>>,
}

impl AtomicMutation for SavepointConflict {
    type Output = ();

    fn transaction_id(&self) -> String {
        "savepoint-conflict".to_string()
    }

    fn isolation_keys(&self, _repo: &Repository) -> Result<BTreeSet<IsolationKey>> {
        let mut keys = BTreeSet::new();
        keys.insert(IsolationKey::Thread("main".to_string()));
        Ok(keys)
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        tx.enroll(Leg {
            id: *self.log.borrow().last().unwrap_or(&0) + 1,
            fail: false,
            log: Rc::clone(&self.log),
        })?;
        if !*self.injected.borrow() {
            *self.injected.borrow_mut() = true;
            tx.repo().oplog().record_batch(vec![
                snapshot_on("main"),
                commit_marker("savepoint-racer", 1),
            ])?;
        }
        Ok(StagedCommit::new((), vec![snapshot_on("main")]))
    }
}

#[test]
fn savepoint_child_effects_rewind_on_isolation_conflict() {
    let (_t, repo) = test_repo();
    let log = Rc::new(RefCell::new(Vec::new()));
    execute(
        &repo,
        SavepointConflict {
            log: Rc::clone(&log),
            injected: Rc::new(RefCell::new(false)),
        },
    )
    .unwrap();
    assert_eq!(
        *log.borrow(),
        vec![1],
        "the first attempt's savepoint child must rewind on conflict"
    );
}

struct AlwaysConflict {
    applied: Rc<RefCell<u32>>,
    rewound: Rc<RefCell<u32>>,
}

impl AtomicMutation for AlwaysConflict {
    type Output = ();

    fn transaction_id(&self) -> String {
        "always-conflict".to_string()
    }

    fn isolation_keys(&self, _repo: &Repository) -> Result<BTreeSet<IsolationKey>> {
        let mut keys = BTreeSet::new();
        keys.insert(IsolationKey::Thread("main".to_string()));
        Ok(keys)
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        *self.applied.borrow_mut() += 1;
        let attempt = *self.applied.borrow();
        let rewound = Rc::clone(&self.rewound);
        tx.on_rewind("always-conflict", move || {
            *rewound.borrow_mut() += 1;
            Ok(())
        });
        tx.repo().oplog().record_batch(vec![
            snapshot_on("main"),
            commit_marker(&format!("always-racer-{attempt}"), 1),
        ])?;
        Ok(StagedCommit::new((), vec![snapshot_on("main")]))
    }
}

#[test]
fn retry_cap_returns_structured_conflict() {
    let (_t, repo) = test_repo();
    let applied = Rc::new(RefCell::new(0));
    let rewound = Rc::new(RefCell::new(0));
    let err = execute(
        &repo,
        AlwaysConflict {
            applied: Rc::clone(&applied),
            rewound: Rc::clone(&rewound),
        },
    )
    .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("isolation conflict on Thread(\"main\")"));
    assert!(message.contains("oplog entry"));
    assert_eq!(*applied.borrow(), 4, "initial attempt plus three retries");
    assert_eq!(*rewound.borrow(), 4, "every conflicted attempt is rewound");
}

#[test]
fn staged_record_keys_are_covered_by_declared_root_keys() {
    let scope = "lane-a";
    let cases = staged_record_coverage_cases(scope);
    let declared = cases
        .iter()
        .flat_map(|(_, expected)| expected.iter().cloned())
        .collect::<BTreeSet<_>>();

    for (record, expected) in cases {
        let variant = op_record_variant_name(&record);
        let touched = isolation_keys_for_record(&record, Some(scope));
        assert_eq!(
            touched, expected,
            "coverage fixture for {variant} drifted: staged record {record:?}"
        );
        assert!(
            touched.is_subset(&declared),
            "staged record {variant} {record:?} touched {touched:?}, outside declared root keys {declared:?}"
        );
    }
}

fn staged_record_coverage_cases(scope: &str) -> Vec<(OpRecord, BTreeSet<IsolationKey>)> {
    let local_head = IsolationKey::LocalHead {
        scope: scope.to_string(),
    };
    let main = IsolationKey::Thread("main".to_string());
    let feature = IsolationKey::Thread("feature".to_string());
    let visibility_state = ChangeId::generate();
    let promoted_state = ChangeId::generate();

    vec![
        (snapshot_on("main"), keys([main.clone()])),
        (
            OpRecord::Snapshot {
                new_state: ChangeId::generate(),
                prev_head: None,
                head: Some(ChangeId::generate()),
                thread: None,
            },
            keys([local_head.clone()]),
        ),
        (
            OpRecord::Goto {
                target: ChangeId::generate(),
                prev_head: None,
                head: ChangeId::generate(),
            },
            keys([local_head.clone()]),
        ),
        (
            OpRecord::ThreadCreate {
                name: "main".to_string(),
                state: ChangeId::generate(),
                manager_snapshot: Some(vec![1]),
            },
            keys([main.clone()]),
        ),
        (
            OpRecord::ThreadDelete {
                name: "feature".to_string(),
                state: ChangeId::generate(),
            },
            keys([feature.clone()]),
        ),
        (
            OpRecord::ThreadUpdate {
                name: "main".to_string(),
                old_state: ChangeId::generate(),
                new_state: ChangeId::generate(),
                manager_snapshots: ThreadUpdateSnapshots::from_parts(Some(vec![1]), Some(vec![2])),
            },
            keys([main.clone()]),
        ),
        (
            OpRecord::Fork {
                from: ChangeId::generate(),
                new_state: ChangeId::generate(),
                thread: Some("main".to_string()),
                head: None,
            },
            keys([main.clone()]),
        ),
        (
            OpRecord::Fork {
                from: ChangeId::generate(),
                new_state: ChangeId::generate(),
                thread: None,
                head: Some(ChangeId::generate()),
            },
            keys([local_head.clone()]),
        ),
        (
            OpRecord::Fork {
                from: ChangeId::generate(),
                new_state: ChangeId::generate(),
                thread: None,
                head: None,
            },
            BTreeSet::new(),
        ),
        (
            OpRecord::Collapse {
                sources: vec![ChangeId::generate(), ChangeId::generate()],
                result: ChangeId::generate(),
                thread: Some("feature".to_string()),
                pre_thread_state: Some(ChangeId::generate()),
            },
            keys([feature.clone()]),
        ),
        (
            OpRecord::Collapse {
                sources: vec![ChangeId::generate()],
                result: ChangeId::generate(),
                thread: None,
                pre_thread_state: None,
            },
            keys([local_head.clone()]),
        ),
        (
            OpRecord::MarkerCreate {
                name: "release".to_string(),
                state: ChangeId::generate(),
            },
            BTreeSet::new(),
        ),
        (
            OpRecord::MarkerDelete {
                name: "release".to_string(),
                state: ChangeId::generate(),
            },
            BTreeSet::new(),
        ),
        (
            OpRecord::Checkpoint {
                parent: None,
                state: ChangeId::generate(),
                thread: Some("main".to_string()),
            },
            keys([main.clone()]),
        ),
        (
            OpRecord::Checkpoint {
                parent: None,
                state: ChangeId::generate(),
                thread: None,
            },
            keys([local_head.clone()]),
        ),
        (
            OpRecord::TransactionAbort {
                transaction_id: "tx-abort".to_string(),
                reason: "test".to_string(),
            },
            BTreeSet::new(),
        ),
        (
            OpRecord::EphemeralThreadCollapse {
                thread: "main".to_string(),
                final_state: ChangeId::generate(),
            },
            keys([main.clone()]),
        ),
        (
            OpRecord::ConflictResolved {
                conflict_id: "conflict-1".to_string(),
                resolution: "ours".to_string(),
            },
            BTreeSet::new(),
        ),
        (commit_marker("tx-commit", 1), BTreeSet::new()),
        (
            OpRecord::Redact {
                redaction_id: ContentHash::from_bytes([1u8; 32]),
                blob: ContentHash::from_bytes([2u8; 32]),
                state: ChangeId::generate(),
                path: "secret.txt".to_string(),
            },
            BTreeSet::new(),
        ),
        (
            OpRecord::Purge {
                redaction_id: ContentHash::from_bytes([3u8; 32]),
                blob: ContentHash::from_bytes([4u8; 32]),
            },
            BTreeSet::new(),
        ),
        (
            OpRecord::FastForward {
                source_thread: "feature".to_string(),
                target_thread: "main".to_string(),
                pre_target_id: ChangeId::generate(),
                post_target_id: ChangeId::generate(),
            },
            keys([feature.clone(), main.clone()]),
        ),
        (
            OpRecord::GitCheckpoint {
                branch: "main".to_string(),
                state: ChangeId::generate(),
                previous_git_oid: None,
                new_git_oid: "abc123".to_string(),
            },
            keys([main.clone()]),
        ),
        (
            OpRecord::RemoteThreadUpdate {
                remote: "origin".to_string(),
                thread: "main".to_string(),
                state: ChangeId::generate(),
            },
            keys([main.clone()]),
        ),
        (
            OpRecord::RemoteThreadDelete {
                remote: "origin".to_string(),
                thread: "main".to_string(),
                state: ChangeId::generate(),
            },
            keys([main.clone()]),
        ),
        (
            OpRecord::UndoRecoveryUpdate {
                state: ChangeId::generate(),
            },
            keys([local_head.clone()]),
        ),
        (
            visibility_set_on(visibility_state),
            keys([IsolationKey::StateVisibility(visibility_state)]),
        ),
        (
            OpRecord::StateVisibilityPromote {
                state: promoted_state,
                superseded: ContentHash::from_bytes([5u8; 32]),
                record_id: ContentHash::from_bytes([6u8; 32]),
                tier: VisibilityTier::Public,
                prior_sidecar: Some(vec![1]),
                new_sidecar: Some(vec![2]),
            },
            keys([IsolationKey::StateVisibility(promoted_state)]),
        ),
    ]
}

fn keys<const N: usize>(keys: [IsolationKey; N]) -> BTreeSet<IsolationKey> {
    keys.into_iter().collect()
}

fn op_record_variant_name(record: &OpRecord) -> &'static str {
    match record {
        OpRecord::Snapshot { .. } => "Snapshot",
        OpRecord::Goto { .. } => "Goto",
        OpRecord::ThreadCreate { .. } => "ThreadCreate",
        OpRecord::ThreadDelete { .. } => "ThreadDelete",
        OpRecord::ThreadUpdate { .. } => "ThreadUpdate",
        OpRecord::Fork { .. } => "Fork",
        OpRecord::Collapse { .. } => "Collapse",
        OpRecord::MarkerCreate { .. } => "MarkerCreate",
        OpRecord::MarkerDelete { .. } => "MarkerDelete",
        OpRecord::Checkpoint { .. } => "Checkpoint",
        OpRecord::TransactionAbort { .. } => "TransactionAbort",
        OpRecord::EphemeralThreadCollapse { .. } => "EphemeralThreadCollapse",
        OpRecord::ConflictResolved { .. } => "ConflictResolved",
        OpRecord::TransactionCommit { .. } => "TransactionCommit",
        OpRecord::Redact { .. } => "Redact",
        OpRecord::Purge { .. } => "Purge",
        OpRecord::FastForward { .. } => "FastForward",
        OpRecord::GitCheckpoint { .. } => "GitCheckpoint",
        OpRecord::RemoteThreadUpdate { .. } => "RemoteThreadUpdate",
        OpRecord::RemoteThreadDelete { .. } => "RemoteThreadDelete",
        OpRecord::UndoRecoveryUpdate { .. } => "UndoRecoveryUpdate",
        OpRecord::StateVisibilitySet { .. } => "StateVisibilitySet",
        OpRecord::StateVisibilityPromote { .. } => "StateVisibilityPromote",
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

    // Thread create + update records.
    op.record_batch(vec![OpRecord::ThreadCreate {
        name: "t_v1".to_string(),
        state: v1,
        manager_snapshot: None,
    }])
    .unwrap();
    op.record_batch(vec![OpRecord::ThreadUpdate {
        name: "t_v1".to_string(),
        old_state: v1,
        new_state: v1b,
        manager_snapshots: None,
    }])
    .unwrap();
    // V2 create + a delete (folds to None).
    op.record_thread_create(&ThreadName::new("t_v2"), &v2, None, None)
        .unwrap();
    op.record_batch(vec![OpRecord::ThreadDelete {
        name: "t_del".to_string(),
        state: v2,
    }])
    .unwrap();
    // Collapse with a thread, and one detaching HEAD (thread = None arm).
    op.record_collapse(&[v1, v1b], &coll, Some("t_coll"), None)
        .unwrap();
    op.record_batch(vec![OpRecord::Collapse {
        sources: vec![v1],
        result: coll,
        thread: None,
        pre_thread_state: None,
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
    op.record_fast_forward(
        &ThreadName::new("t_src"),
        &ThreadName::new("t_ff"),
        &v1,
        &ff,
        None,
    )
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
    op.record_marker_create(&MarkerName::new("mk2"), &mk)
        .unwrap();
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
    assert!(refs
        .list_markers()
        .unwrap()
        .contains(&MarkerName::new("mk2")));
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

    empty_isolation_keys!();

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

impl DeferredMutation for StageThenFail {}

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

    empty_isolation_keys!();

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

    empty_isolation_keys!();

    fn apply(&mut self, _tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
        *self.applied.borrow_mut() += 1;
        Ok(StagedCommit::new(
            (),
            vec![OpRecord::Snapshot {
                new_state: self.state,
                prev_head: None,
                head: Some(self.state),
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

    empty_isolation_keys!();

    fn apply(&mut self, _tx: &mut Tx<'_>) -> Result<StagedCommit<u32>> {
        *self.staged_count.borrow_mut() += 1;
        let state = ChangeId::generate();
        Ok(StagedCommit::new(
            7,
            vec![OpRecord::Snapshot {
                new_state: state,
                prev_head: None,
                head: Some(state),
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
        vec![OpRecord::ThreadCreate {
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
        .record_collapse(&[base], &result, Some("main"), None)
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
                head: goto_b,
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
/// directly before recording `FastForward`. The reconcile must defer to the
/// re-attached canonical HEAD, not the stale fork. (This is the exact
/// stale-HEAD clobber a Fork/Collapse-only allowlist re-opens: `FastForward`
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
    // directly before the FastForward record.
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
        .record_fast_forward(
            &ThreadName::new("src"),
            &ThreadName::new("main"),
            &base,
            &ff_target,
            Some(&scope),
        )
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
                head: Some(snap_b),
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

// ---- Snapshot record-first close-the-class (heddle#354 r8) ----
//
// The snapshot path now commits its `OpRecord::Snapshot` BEFORE publishing the
// paired ref (record-first via `commit_snapshot_atomic` →
// `commit_and_publish`). These tests pin the two properties that makes that
// safe: (1) a detached snapshot's HEAD is reconstructable from its record after
// a phase-4-committed / phase-5-unpublished crash, and (2) the reconciler's
// authoritative `Snapshot` fold yields the NEWEST committed value, so a snapshot
// can never clobber a newer concurrent write. (The cross-crate conformance that
// FORCES the production reorder lives in `heddle-devtools`
// `check-snapshot-atomicity`.)

/// Detached snapshot is record-first as of r8: a crash between the record
/// commit (phase 4) and the HEAD publish (phase 5) must be recovered by
/// reconstructing `Head::Detached{new_state}` from the committed record. Before
/// r8 the detached `Snapshot` fold deferred to canonical (`HeadFold::Canonical`),
/// which — now that the publish is record-first — would LOSE the snapshot's HEAD
/// move. This test fails under the pre-r8 `Canonical` arm and passes under the
/// `Republish` arm.
#[test]
fn detached_snapshot_record_first_crash_recovery_reconstructs_head() {
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

    // Phase 4 only: a detached snapshot committed its record, the phase-5 HEAD
    // publish never ran (canonical HEAD is still `Attached{main}`).
    let snap = ChangeId::generate();
    repo.oplog()
        .record_snapshot(&snap, Some(&base), None, Some(&scope))
        .unwrap();

    assert_eq!(
        repo.refs().read_head().unwrap(),
        Head::Detached { state: snap },
        "a record-first detached snapshot's HEAD must be reconstructed after a phase-5 crash"
    );
}

#[test]
fn attached_snapshot_record_first_crash_recovery_materializes_head_state() {
    let (_t, repo) = test_repo();
    let scope = repo.op_scope();
    let base = ChangeId::generate();
    let main = ThreadName::new("main");
    repo.refs().set_thread(&main, &base).unwrap();
    repo.refs()
        .write_head(&Head::Attached {
            thread: main.clone(),
        })
        .unwrap();

    let snap = ChangeId::generate();
    repo.oplog()
        .record_snapshot(&snap, Some(&base), Some(main.as_str()), Some(&scope))
        .unwrap();

    assert_eq!(
        repo.head().unwrap(),
        Some(snap),
        "a record-first attached snapshot must reconstruct the resolved HEAD state"
    );

    let raw = RefManager::new(repo.heddle_dir());
    assert_eq!(
        raw.get_thread(&main).unwrap(),
        Some(snap),
        "the attached thread target must be materialized exactly once"
    );

    assert_eq!(
        repo.head().unwrap(),
        Some(snap),
        "a second HEAD read must be idempotent"
    );
}

#[test]
fn goto_record_first_crash_recovery_reconstructs_head() {
    let (_t, repo) = test_repo();
    let scope = repo.op_scope();
    let base = ChangeId::generate();
    repo.refs()
        .write_head(&Head::Detached { state: base })
        .unwrap();

    let target = ChangeId::generate();
    repo.oplog()
        .record_goto(&target, Some(&base), Some(&scope))
        .unwrap();

    assert_eq!(
        repo.refs().read_head().unwrap(),
        Head::Detached { state: target },
        "a record-first goto's HEAD must be reconstructed after a phase-5 crash"
    );

    assert_eq!(
        RefManager::new(repo.heddle_dir()).read_head().unwrap(),
        Head::Detached { state: target },
        "the reconstructed goto HEAD must be materialized exactly once"
    );

    assert_eq!(
        repo.refs().read_head().unwrap(),
        Head::Detached { state: target },
        "a second HEAD read must be idempotent"
    );
}

/// Snapshot-vs-newer-write race (attached): A's snapshot commits its record at a
/// LOWER oplog id than B's later thread write. Because the snapshot is now
/// record-first, the snapshot's record reflects when A actually committed, so
/// B's newer record (higher id) wins under the reconciler's id-ordered
/// authoritative fold — the stale snapshot value must NOT clobber it. (A
/// publish-first snapshot would have recorded AFTER B, inverting the ids and
/// resurrecting the stale snapshot.)
#[test]
fn snapshot_does_not_clobber_newer_committed_thread_write_attached() {
    let (temp, repo) = test_repo();
    let scope = repo.op_scope();
    let s0 = ChangeId::generate();
    repo.refs()
        .set_thread(&ThreadName::new("feature"), &s0)
        .unwrap();
    repo.refs()
        .write_head(&Head::Attached {
            thread: ThreadName::new("feature"),
        })
        .unwrap();

    // A's snapshot (record-first) advances `feature` = sA at the lower id.
    let sa = ChangeId::generate();
    repo.oplog()
        .record_snapshot(&sa, Some(&s0), Some("feature"), Some(&scope))
        .unwrap();
    // B advances `feature` = sB and records (record-first) at the higher id.
    let sb = ChangeId::generate();
    repo.oplog()
        .record_thread_create(&ThreadName::new("feature"), &sb, None, Some(&scope))
        .unwrap();

    assert_eq!(
        repo.refs().get_thread(&ThreadName::new("feature")).unwrap(),
        Some(sb),
        "the newest committed write (B) must win; the older snapshot (A) must not clobber it"
    );

    // The reconcile materialized the canonical too: a raw, reconciler-free
    // handle reads the newest committed value.
    let raw = RefManager::new(repo.heddle_dir());
    assert_eq!(
        raw.get_thread(&ThreadName::new("feature")).unwrap(),
        Some(sb),
        "the newest committed value must be materialized to the canonical ref"
    );
    drop(temp);
}

/// Snapshot-vs-newer-write race (detached): a later detached snapshot (higher
/// id) wins over an earlier one. Both are record-first `Republish(Detached)`
/// movers, so the id-ordered fold takes the newest.
#[test]
fn snapshot_does_not_clobber_newer_committed_write_detached() {
    let (_t, repo) = test_repo();
    let scope = repo.op_scope();
    let base = ChangeId::generate();
    repo.refs()
        .write_head(&Head::Detached { state: base })
        .unwrap();

    let sa = ChangeId::generate();
    repo.oplog()
        .record_snapshot(&sa, Some(&base), None, Some(&scope))
        .unwrap();
    let sb = ChangeId::generate();
    repo.oplog()
        .record_snapshot(&sb, Some(&sa), None, Some(&scope))
        .unwrap();

    assert_eq!(
        repo.refs().read_head().unwrap(),
        Head::Detached { state: sb },
        "the newest committed detached snapshot must win"
    );
}

/// `commit_snapshot_atomic` (attached): records an `OpRecord::Snapshot` AND
/// publishes the thread ref through the record-first chokepoint, atomically.
#[test]
fn commit_snapshot_atomic_attached_records_and_publishes() {
    let (_t, repo) = test_repo();
    let s0 = ChangeId::generate();
    repo.refs()
        .set_thread(&ThreadName::new("feature"), &s0)
        .unwrap();
    repo.refs()
        .write_head(&Head::Attached {
            thread: ThreadName::new("feature"),
        })
        .unwrap();

    let sa = ChangeId::generate();
    repo.commit_snapshot_atomic(&sa, Some(s0), Some(&ThreadName::new("feature")))
        .unwrap();

    assert_eq!(
        repo.refs().get_thread(&ThreadName::new("feature")).unwrap(),
        Some(sa),
        "the snapshot must publish the thread ref"
    );
    let has_snap = repo.oplog().recent(16).unwrap().into_iter().any(|e| {
        matches!(
            &e.operation,
            OpRecord::Snapshot { new_state, thread: Some(t), .. } if *new_state == sa && t == "feature"
        )
    });
    assert!(
        has_snap,
        "commit_snapshot_atomic must append the paired OpRecord::Snapshot"
    );
}

/// `commit_snapshot_atomic` (detached): records an `OpRecord::Snapshot` with no
/// thread AND republishes a detached HEAD, atomically.
#[test]
fn commit_snapshot_atomic_detached_records_and_publishes() {
    let (_t, repo) = test_repo();
    let base = ChangeId::generate();
    repo.refs()
        .write_head(&Head::Detached { state: base })
        .unwrap();

    let sb = ChangeId::generate();
    repo.commit_snapshot_atomic(&sb, Some(base), None).unwrap();

    assert_eq!(
        repo.refs().read_head().unwrap(),
        Head::Detached { state: sb },
        "the detached snapshot must publish HEAD = Detached{{new_state}}"
    );
    let has_snap = repo.oplog().recent(16).unwrap().into_iter().any(|e| {
        matches!(
            &e.operation,
            OpRecord::Snapshot { new_state, thread: None, .. } if *new_state == sb
        )
    });
    assert!(
        has_snap,
        "commit_snapshot_atomic must append the paired detached OpRecord::Snapshot"
    );
}

/// Cross-class recovery atomicity (heddle#354 r8, cid 3330183592): a named fork
/// committed (phase 4) a new thread `topic` AND HEAD = Attached(topic), but the
/// phase-5 publish never ran. A LOCAL-class read (HEAD) must recover BOTH HEAD
/// and the paired thread under one lock, before the Local watermark advances —
/// so HEAD = Attached(topic) is never observable with `topic` missing. Without
/// the cross-class fix the Local reconcile republishes only HEAD and advances
/// its watermark while `topic` is still unmaterialized, which a raw read of
/// `topic` exposes.
#[test]
fn named_fork_recovery_materializes_head_and_paired_thread() {
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

    // Phase 4 only: the fork's record is committed (thread `topic` + HEAD =
    // Attached(topic)); no phase-5 publish ran.
    let forked = ChangeId::generate();
    repo.oplog()
        .record_fork(&base, &forked, Some("topic"), None, Some(&scope))
        .unwrap();

    // A LOCAL-class read recovers HEAD.
    assert_eq!(
        repo.refs().read_head().unwrap(),
        Head::Attached {
            thread: ThreadName::new("topic")
        },
        "named-fork recovery must republish HEAD = Attached(topic)"
    );

    // RAW read (no reconciler): the paired thread must already be on disk after
    // the Local reconcile — materialized atomically with the recovered HEAD, not
    // deferred to a later Shared read.
    let raw = RefManager::new(repo.heddle_dir());
    assert_eq!(
        raw.get_thread(&ThreadName::new("topic")).unwrap(),
        Some(forked),
        "the paired thread must be materialized atomically with the recovered HEAD"
    );
}

/// Attached collapse is NOT a HEAD-mover (heddle#354 r9, cid 3330304665). The
/// collapse command, when HEAD is attached, publishes ONLY the thread ref and
/// never re-attaches HEAD — so a crash-replayed attached collapse must advance
/// the thread but leave HEAD where it is. Republishing `Attached(<named>)`
/// moved HEAD when it should stay put. Non-vacuous: the record names a thread
/// (`topic`) different from the attached one (`main`), so the pre-fix code
/// republished HEAD = Attached(topic) and this assertion failed.
#[test]
fn attached_collapse_advances_thread_without_moving_head() {
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

    // Phase 4 only: an attached collapse naming thread `topic`.
    let result = ChangeId::generate();
    repo.oplog()
        .record_collapse(&[base], &result, Some("topic"), Some(&scope))
        .unwrap();

    // HEAD must stay Attached(main): the attached collapse advances the thread,
    // it does NOT move/republish HEAD.
    assert_eq!(
        repo.refs().read_head().unwrap(),
        Head::Attached {
            thread: ThreadName::new("main")
        },
        "attached collapse must NOT republish/move HEAD"
    );

    // The named thread still advances (Shared-class fold materializes it).
    assert_eq!(
        repo.refs().get_thread(&ThreadName::new("topic")).unwrap(),
        Some(result),
        "attached collapse must still advance the thread it published"
    );
}

/// Detached collapse DOES republish HEAD: the command emits `RefUpdate::Head`
/// Detached record-first, so a phase-4-committed / phase-5-unpublished collapse
/// recovers HEAD = Detached(result). The counterpart to the attached case above.
#[test]
fn detached_collapse_republishes_detached_head() {
    let (_t, repo) = test_repo();
    let scope = repo.op_scope();
    let base = ChangeId::generate();
    repo.refs()
        .write_head(&Head::Detached { state: base })
        .unwrap();

    // Phase 4 only: a detached collapse (thread = None).
    let result = ChangeId::generate();
    repo.oplog()
        .record_collapse(&[base], &result, None, Some(&scope))
        .unwrap();

    assert_eq!(
        repo.refs().read_head().unwrap(),
        Head::Detached { state: result },
        "detached collapse must republish HEAD = Detached(result)"
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

    empty_isolation_keys!();

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

    empty_isolation_keys!();

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

// ---- heddle#354 r5 — reconcile-consistency closures ----

/// Finding B (cid 3329631074) — cross-process pre-open recovery via the
/// persisted reconcile watermark. A prior process durably commits a Fork's
/// phase-4 record naming "explore" but crashes before phase-5 publishes the
/// canonical ref; the handle is dropped without advancing the watermark past
/// that record. A FRESH handle seeds its per-read watermark from the PERSISTED
/// last-clean point (below the unpublished record), so its next read folds
/// `(seed, tip]` — the crash tail — and recovers the committed mutation, instead
/// of seeding at tip and silently losing it cross-process.
#[test]
fn persisted_watermark_recovers_cross_process_crash_tail() {
    let temp = TempDir::new().unwrap();
    let base = ChangeId::generate();
    let forked = ChangeId::generate();
    {
        let repo = Repository::init_default(temp.path()).unwrap();
        repo.refs()
            .set_thread(&ThreadName::new("main"), &base)
            .unwrap();
        // "Prior process": phase-4 commit only (Fork naming "explore"), no
        // phase-5 publish, then the handle drops.
        repo.oplog()
            .record_fork(&base, &forked, Some("explore"), None, None)
            .unwrap();
    }

    // Fresh process: a brand-new handle whose watermark seeds at the current
    // generation (past the unpublished record). The open-time pass recovers it.
    let reopened = Repository::open(temp.path()).unwrap();
    assert_eq!(
        reopened
            .refs()
            .get_thread(&ThreadName::new("explore"))
            .unwrap(),
        Some(forked),
        "a prior process's committed-but-unpublished ref must materialize on fresh open"
    );

    // It was materialized to canonical: a second fresh open reads it the same.
    let reread = Repository::open(temp.path()).unwrap();
    assert_eq!(
        reread
            .refs()
            .get_thread(&ThreadName::new("explore"))
            .unwrap(),
        Some(forked)
    );
}

/// Finding B safety — the persisted watermark must NOT resurrect a ref deleted
/// via an UNRECORDED path (the un-migrated delete the record-first retrofit has
/// not yet reached). A thread is created (recorded + published) and a read
/// advances + persists the watermark past it; the thread is then deleted
/// directly, appending NO oplog record. A fresh handle seeds from the persisted
/// watermark (at/above the create), so its read does not re-fold the stale
/// create — the thread stays deleted. (An eager open-time fold of the tail, by
/// contrast, WOULD resurrect it — the reason this fix is a persisted watermark,
/// not an eager pass.)
#[test]
fn persisted_watermark_does_not_resurrect_unrecorded_delete() {
    let temp = TempDir::new().unwrap();
    let state = ChangeId::generate();
    {
        let repo = Repository::init_default(temp.path()).unwrap();
        // Create through the chokepoint (records + publishes).
        repo.commit_and_publish(
            vec![OpRecord::ThreadCreate {
                name: "fcn".to_string(),
                state,
                manager_snapshot: None,
            }],
            &[RefUpdate::Thread {
                name: ThreadName::new("fcn"),
                expected: RefExpectation::Missing,
                new: Some(state),
            }],
        )
        .unwrap();
        // A read advances + persists the watermark past the create.
        assert_eq!(
            repo.refs().get_thread(&ThreadName::new("fcn")).unwrap(),
            Some(state)
        );
        // Delete directly — NO oplog record (the un-migrated delete path).
        repo.refs().delete_thread(&ThreadName::new("fcn")).unwrap();
    }

    // Fresh process: seeding from the persisted watermark (≥ the create's
    // generation) means the read does not fold the stale create ⇒ no resurrect.
    let reopened = Repository::open(temp.path()).unwrap();
    assert_eq!(
        reopened.refs().get_thread(&ThreadName::new("fcn")).unwrap(),
        None,
        "a ref deleted via an unrecorded path must not be resurrected on reopen"
    );
}

/// Finding 3 (cid 3329711893) — the SHARED-class reconcile watermark is shared
/// across sibling worktrees, not per-worktree. Worktree B persists a shared
/// watermark at a low generation; worktree A then records+publishes a shared
/// (thread) create and a read advances the SHARED watermark past it; the create
/// is deleted via an unrecorded path. When B reopens, it seeds its shared
/// watermark from the SHARED file (which A advanced), so it does NOT re-fold and
/// resurrect the create — even though B's own per-worktree state is behind. With
/// a per-worktree shared watermark (the bug), B would seed below the create and
/// resurrect it.
#[test]
fn shared_watermark_is_cross_worktree_no_resurrect() {
    let main_holder = TempDir::new().unwrap();
    let main = Repository::init_default(main_holder.path()).unwrap();
    let shared_heddle = main.heddle_dir().to_path_buf();

    // Two sibling worktrees pointing at the same shared store.
    let wt_holder = TempDir::new().unwrap();
    let wt_a = wt_holder.path().join("a");
    let wt_b = wt_holder.path().join("b");
    Repository::init_worktree(&wt_a, &shared_heddle).unwrap();
    Repository::init_worktree(&wt_b, &shared_heddle).unwrap();

    // B opens early and reads a shared ref, persisting its shared watermark at
    // the current (pre-create) generation — the "behind" per-worktree state.
    {
        let repo_b = Repository::open(&wt_b).unwrap();
        let _ = repo_b
            .refs()
            .get_thread(&ThreadName::new("not-yet"))
            .unwrap();
    }

    // A records + publishes a shared-ref create, then a read advances + persists
    // the SHARED watermark past it.
    let created = ChangeId::generate();
    {
        let repo_a = Repository::open(&wt_a).unwrap();
        repo_a
            .commit_and_publish(
                vec![OpRecord::ThreadCreate {
                    name: "shared-thread".to_string(),
                    state: created,
                    manager_snapshot: None,
                }],
                &[RefUpdate::Thread {
                    name: ThreadName::new("shared-thread"),
                    expected: RefExpectation::Missing,
                    new: Some(created),
                }],
            )
            .unwrap();
        assert_eq!(
            repo_a
                .refs()
                .get_thread(&ThreadName::new("shared-thread"))
                .unwrap(),
            Some(created)
        );
        // Delete via the unrecorded path (no oplog record): canonical gone, the
        // create record still sits in the tail.
        repo_a
            .refs()
            .delete_thread(&ThreadName::new("shared-thread"))
            .unwrap();
    }

    // B reopens: its per-worktree state is behind the create, but it seeds the
    // SHARED watermark from the shared file A advanced, so the read does not
    // re-fold the stale create. A per-worktree shared watermark would resurrect
    // it here.
    let repo_b = Repository::open(&wt_b).unwrap();
    assert_eq!(
        repo_b
            .refs()
            .get_thread(&ThreadName::new("shared-thread"))
            .unwrap(),
        None,
        "a shared create another worktree processed past the shared watermark \
         must not be re-folded/resurrected by a sibling worktree"
    );
}

// ---- heddle#354 r7 — close-the-class (unified write chokepoint + scoped forks) ----

/// cid 3329765073 — a NON-ATOMIC `update_refs`-class write funnels through the
/// same write chokepoint as the atomic path, which materializes the committed
/// tail FIRST. So a committed-but-unpublished record is persisted to canonical
/// by the write, not discarded. Pre-r7 the non-atomic path folded the tail only
/// for validation and threw the `ReconcileOutcome` away, leaving the record
/// unmaterialized (and re-foldable over the very write that should have
/// superseded it).
#[test]
fn non_atomic_write_materializes_committed_tail() {
    let (_t, repo) = test_repo();
    let v_new = ChangeId::generate();
    // Phase-4 only: a committed-but-unpublished thread "x" (canonical absent,
    // watermark behind).
    repo.oplog()
        .record_batch(vec![OpRecord::ThreadCreate {
            name: "x".to_string(),
            state: v_new,
            manager_snapshot: None,
        }])
        .unwrap();

    // A no-reconciler view reads RAW canonical: "x" is not yet materialized.
    assert_eq!(
        RefManager::new(repo.heddle_dir())
            .get_thread(&ThreadName::new("x"))
            .unwrap(),
        None,
        "precondition: the committed record is not yet materialized to canonical"
    );

    // A NON-ATOMIC write to a DIFFERENT ref runs the write chokepoint, which
    // materializes the committed tail before publishing.
    let y = ChangeId::generate();
    repo.refs().set_thread(&ThreadName::new("y"), &y).unwrap();

    // The committed "x" is now in canonical — observable WITHOUT reconciliation.
    // Pre-r7 the non-atomic write discarded the reconcile outcome and "x" stayed
    // absent until a reconciling read happened to fold it.
    assert_eq!(
        RefManager::new(repo.heddle_dir())
            .get_thread(&ThreadName::new("x"))
            .unwrap(),
        Some(v_new),
        "a non-atomic write must materialize the committed tail (no lost records)"
    );
}

/// cid 3329765073, the clobber face — a non-atomic delete of a ref whose
/// committed-but-unpublished record would otherwise be re-folded over the
/// delete. The write chokepoint materializes + advances the watermark to the
/// tip, so the delete lands on a fully-reconciled canonical and no later read
/// re-derives the deleted value. Pre-r7 the watermark stayed behind and the
/// next read re-folded the create, resurrecting the deleted ref.
#[test]
fn non_atomic_delete_is_not_clobbered_by_a_committed_record() {
    let (_t, repo) = test_repo();
    let v_old = ChangeId::generate();
    let v_new = ChangeId::generate();
    // Publish "x"=v_old (no record), then commit (don't publish) "x"=v_new.
    repo.refs()
        .set_thread(&ThreadName::new("x"), &v_old)
        .unwrap();
    repo.oplog()
        .record_batch(vec![OpRecord::ThreadUpdate {
            name: "x".to_string(),
            old_state: v_old,
            new_state: v_new,
            manager_snapshots: None,
        }])
        .unwrap();

    // Non-atomic delete via `update_refs` (no preceding reconciling read).
    repo.refs()
        .update_refs(&[RefUpdate::Thread {
            name: ThreadName::new("x"),
            expected: RefExpectation::Any,
            new: None,
        }])
        .unwrap();

    // The delete is preserved: a read does NOT re-fold the committed update.
    assert_eq!(
        repo.refs().get_thread(&ThreadName::new("x")).unwrap(),
        None,
        "a non-atomic delete must not be clobbered by re-folding a committed record"
    );
}

/// cid 3329765074 — a HEAD-moving fork recorded via the `record_fork` HELPER is
/// SCOPED to `op_scope`, so the read chokepoint's `Local`-class (scoped) fold
/// reconciles it. Covers BOTH a detached fork (`head = Some`) and an attached
/// fork (`thread = Some`, which also moves HEAD), plus the contrast that an
/// UNSCOPED helper-recorded fork is invisible to the scoped fold (the pre-r7
/// `record_single` bug — proving the scope is load-bearing).
#[test]
fn helper_recorded_head_moving_fork_is_scoped_and_reconciles() {
    // Detached fork, scoped → HEAD reconciles.
    let (_t1, repo) = test_repo();
    let scope = repo.op_scope();
    let base = ChangeId::generate();
    let forked = ChangeId::generate();
    repo.oplog()
        .record_fork(&base, &forked, None, Some(&forked), Some(&scope))
        .unwrap();
    assert_eq!(
        repo.refs().read_head().unwrap(),
        Head::Detached { state: forked },
        "a scoped helper-recorded detached fork must reconcile HEAD"
    );

    // Attached fork, scoped → HEAD reconciles to the new thread AND the thread
    // ref reconciles.
    let (_t2, repo) = test_repo();
    let scope = repo.op_scope();
    let base = ChangeId::generate();
    let forked = ChangeId::generate();
    repo.oplog()
        .record_fork(&base, &forked, Some("topic"), None, Some(&scope))
        .unwrap();
    assert_eq!(
        repo.refs().get_thread(&ThreadName::new("topic")).unwrap(),
        Some(forked),
        "a scoped helper-recorded attached fork must reconcile the thread ref"
    );
    assert_eq!(
        repo.refs().read_head().unwrap(),
        Head::Attached {
            thread: ThreadName::new("topic")
        },
        "a scoped helper-recorded attached fork must reconcile HEAD to the new thread"
    );

    // Contrast: an UNSCOPED detached fork is invisible to the scoped Local fold,
    // so HEAD is NOT reconstructed (the pre-r7 bug — scope is load-bearing).
    let (_t3, repo) = test_repo();
    let base = ChangeId::generate();
    let head_before = repo.refs().read_head().unwrap();
    repo.oplog()
        .record_fork(
            &base,
            &ChangeId::generate(),
            None,
            Some(&ChangeId::generate()),
            None,
        )
        .unwrap();
    assert_eq!(
        repo.refs().read_head().unwrap(),
        head_before,
        "an unscoped detached fork must NOT reconcile HEAD (scope is load-bearing)"
    );
}

/// Class A (cid 3329631079) — a `Missing`/CAS expectation is validated against
/// the RECONCILED state, not a stale on-disk read. A committed-but-unpublished
/// Fork record creates "explore" (canonical still absent); a later
/// `commit_and_publish` with a `Missing` expectation on "explore" must FAIL,
/// because under the lock the reconciled value shows it already exists.
#[test]
fn commit_and_publish_validates_against_reconciled_not_stale_disk() {
    let (_t, repo) = test_repo();
    let base = ChangeId::generate();
    let committed = ChangeId::generate();

    // Phase-4 only: "explore" is committed-but-unpublished (canonical absent).
    repo.oplog()
        .record_fork(&base, &committed, Some("explore"), None, None)
        .unwrap();

    // A Missing expectation must fail: the reconciled state shows "explore"
    // exists even though the raw on-disk ref is absent.
    let result = repo.commit_and_publish(
        vec![OpRecord::Fork {
            from: base,
            new_state: ChangeId::generate(),
            thread: Some("explore".to_string()),
            head: None,
        }],
        &[RefUpdate::Thread {
            name: ThreadName::new("explore"),
            expected: RefExpectation::Missing,
            new: Some(ChangeId::generate()),
        }],
    );
    assert!(
        result.is_err(),
        "a Missing expectation must fail against a committed-but-unpublished value (reconciled, not stale disk)"
    );

    // A CAS to the COMMITTED value (not the absent on-disk value) must succeed.
    let next = ChangeId::generate();
    repo.commit_and_publish(
        vec![OpRecord::Fork {
            from: committed,
            new_state: next,
            thread: Some("explore".to_string()),
            head: None,
        }],
        &[RefUpdate::Thread {
            name: ThreadName::new("explore"),
            expected: RefExpectation::Value(committed),
            new: Some(next),
        }],
    )
    .expect("a CAS against the reconciled committed value must succeed");
    assert_eq!(
        repo.refs().get_thread(&ThreadName::new("explore")).unwrap(),
        Some(next)
    );
}

/// Class A (cid 3329631077) — the fold→publish TOCTOU. A lagging reader folds
/// while a writer concurrently publishes a NEWER value; the reader's lazy
/// re-publish must never clobber the writer's newer publish with a stale folded
/// value. The fold now runs UNDER the publish lock, so it serializes with the
/// writer's commit-and-publish and always observes the newest committed record.
/// Stress loop: each iteration reaches the same end state — the published
/// canonical equals the newest committed Fork record — never an older value.
#[test]
fn lagging_reader_never_clobbers_concurrent_publish() {
    for _ in 0..25 {
        let temp = TempDir::new().unwrap();
        let base = ChangeId::generate();
        {
            let repo = Repository::init_default(temp.path()).unwrap();
            repo.refs()
                .set_thread(&ThreadName::new("main"), &base)
                .unwrap();
            // A committed-but-unpublished OLD value a lagging reader would fold.
            repo.oplog()
                .record_fork(&base, &ChangeId::generate(), Some("main"), None, None)
                .unwrap();
        }
        let path = temp.path().to_path_buf();
        let v_new = ChangeId::generate();

        std::thread::scope(|s| {
            let p_r = path.clone();
            s.spawn(move || {
                let reader = Repository::open(&p_r).unwrap();
                for _ in 0..40 {
                    let _ = reader.refs().get_thread(&ThreadName::new("main"));
                }
            });
            let p_w = path.clone();
            s.spawn(move || {
                let writer = Repository::open(&p_w).unwrap();
                let _ = writer.commit_and_publish(
                    vec![OpRecord::Fork {
                        from: base,
                        new_state: v_new,
                        thread: Some("main".to_string()),
                        head: None,
                    }],
                    &[RefUpdate::Thread {
                        name: ThreadName::new("main"),
                        expected: RefExpectation::Any,
                        new: Some(v_new),
                    }],
                );
            });
        });

        // The published canonical must equal the newest committed Fork record
        // for "main" — the reader's materialize never republished a stale value.
        let reader = Repository::open(temp.path()).unwrap();
        let published = reader.refs().get_thread(&ThreadName::new("main")).unwrap();
        let newest = reader
            .oplog()
            .recent(64)
            .unwrap()
            .into_iter()
            .filter(
                |e| matches!(&e.operation, OpRecord::Fork { thread: Some(t), .. } if t == "main"),
            )
            .max_by_key(|e| e.id)
            .map(|e| match e.operation {
                OpRecord::Fork { new_state, .. } => new_state,
                _ => unreachable!(),
            });
        assert_eq!(
            published, newest,
            "a lagging reader's materialize must not clobber the newest committed publish"
        );
    }
}

/// A mutation that REGENERATES its `ChangeId` output on every `apply` run, and
/// reconstructs the committed identity from the deduped record on a dedup hit.
struct RegeneratesChangeId {
    key: String,
    fresh_ids: Rc<RefCell<Vec<ChangeId>>>,
}

impl AtomicMutation for RegeneratesChangeId {
    type Output = ChangeId;

    fn transaction_id(&self) -> String {
        self.key.clone()
    }

    empty_isolation_keys!();

    fn apply(&mut self, _tx: &mut Tx<'_>) -> Result<StagedCommit<ChangeId>> {
        // A fresh, NON-deterministic output each run — a crash-retry's value
        // would diverge from the originally-committed one.
        let fresh = ChangeId::generate();
        self.fresh_ids.borrow_mut().push(fresh);
        Ok(StagedCommit::new(
            fresh,
            vec![OpRecord::Snapshot {
                new_state: fresh,
                prev_head: None,
                head: Some(fresh),
                thread: None,
            }],
        ))
    }

    fn reconstruct_committed_output(
        &self,
        committed_records: &[OpRecord],
        _this_run: ChangeId,
    ) -> Result<ChangeId> {
        for op in committed_records {
            if let OpRecord::Snapshot { new_state, .. } = op {
                return Ok(*new_state);
            }
        }
        Err(HeddleError::Config(
            "no committed snapshot to reconstruct from".to_string(),
        ))
    }
}

/// Finding C (cid 3329631075) — a dedup-hit replay returns the ORIGINALLY
/// committed output, not this run's freshly regenerated value. The replay's
/// `apply` mints a *different* ChangeId; `execute` must reconstruct the
/// committed identity from the deduped record and return that.
#[test]
fn dedup_hit_returns_prior_committed_output_not_replays_fresh_one() {
    let (_t, repo) = test_repo();
    let fresh_ids = Rc::new(RefCell::new(Vec::new()));
    let key = "regenerating-op".to_string();

    let first = execute(
        &repo,
        RegeneratesChangeId {
            key: key.clone(),
            fresh_ids: Rc::clone(&fresh_ids),
        },
    )
    .unwrap();

    let second = execute(
        &repo,
        RegeneratesChangeId {
            key,
            fresh_ids: Rc::clone(&fresh_ids),
        },
    )
    .unwrap();

    let minted = fresh_ids.borrow();
    assert_eq!(minted.len(), 2, "both runs minted a fresh id");
    assert_ne!(
        minted[0], minted[1],
        "the replay regenerated a DIFFERENT id (non-vacuous)"
    );
    assert_eq!(first, minted[0], "the first run returns its committed id");
    assert_eq!(
        second, minted[0],
        "the dedup-hit replay returns the ORIGINALLY committed id, not its own fresh one"
    );
    assert_ne!(
        second, minted[1],
        "the replay must NOT return the value it freshly regenerated"
    );
}

/// Finding D (cid 3329631081) — a truncated/corrupt oplog header must FAIL a
/// reconciled read loudly, never silently report generation 0 (which would make
/// the read skip every committed record, data-loss masquerading as an empty
/// oplog).
#[test]
fn corrupt_oplog_header_fails_reconciled_read_loudly() {
    let (_t, repo) = test_repo();
    let base = ChangeId::generate();
    repo.refs()
        .set_thread(&ThreadName::new("main"), &base)
        .unwrap();

    // Clobber the oplog magic so the header read fails on an EXISTING file.
    let oplog_path = repo.heddle_dir().join("oplog").join("oplog.bin");
    let mut bytes = std::fs::read(&oplog_path).unwrap();
    assert!(bytes.len() >= 8, "oplog file must have a header");
    for b in bytes.iter_mut().take(8) {
        *b = 0xFF;
    }
    std::fs::write(&oplog_path, &bytes).unwrap();

    let result = repo.refs().get_thread(&ThreadName::new("main"));
    assert!(
        result.is_err(),
        "a corrupt oplog header must fail the reconciled read, not default to generation 0"
    );
}

/// Finding 2 (cid 3329711891) — a right-magic / unsupported-version oplog must
/// FAIL a reconciled read, not yield a silently-trusted generation that lets
/// the `tip == watermark` fast path skip the full parser (which would reject
/// the version). Sibling of Finding D extended from corrupt-magic to
/// wrong-version.
#[test]
fn unsupported_oplog_version_fails_reconciled_read_loudly() {
    let (_t, repo) = test_repo();
    let base = ChangeId::generate();
    repo.refs()
        .set_thread(&ThreadName::new("main"), &base)
        .unwrap();

    // Bump the version field (bytes 8..12) to a forward-incompatible value on
    // an EXISTING file, leaving the magic intact.
    let oplog_path = repo.heddle_dir().join("oplog").join("oplog.bin");
    let mut bytes = std::fs::read(&oplog_path).unwrap();
    assert!(bytes.len() >= 12, "oplog file must have magic + version");
    // Any value distinct from the current on-disk version is unsupported.
    let current = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    bytes[8..12].copy_from_slice(&current.wrapping_add(1).to_le_bytes());
    std::fs::write(&oplog_path, &bytes).unwrap();

    let result = repo.refs().get_thread(&ThreadName::new("main"));
    assert!(
        result.is_err(),
        "an unsupported oplog version must fail the reconciled read, not be silently trusted via the fast path"
    );
}
