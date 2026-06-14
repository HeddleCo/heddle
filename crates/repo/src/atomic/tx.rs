// SPDX-License-Identifier: Apache-2.0
//! The transaction context `Tx` + the reverse-order rewind ledger
//! (heddle#330 §3.1, §4).
//!
//! `Tx` threads through every `apply` in a nest. Inner mutations enroll into
//! the *same* `Tx` (deferring their commit marker to the outermost transaction)
//! rather than committing independently; only the outermost
//! [`execute`](super::execute) reaches `commit`. The ledger is a LIFO stack of
//! inverse closures, popped in reverse on any unwind.

use std::cell::RefCell;
use std::rc::Rc;

use objects::error::{HeddleError, Result};
use oplog::{
    ConditionalCommitOutcome, IsolationKey, IsolationPrecondition, OpLogBackend, OpRecord,
};

use super::traits::{DeferredMutation, EagerMutation, StagedCommit};
use crate::Repository;

/// The boxed inverse closure half of a [`RewindAction`]. Boxing a *closure* is
/// an implementation detail of the ledger — it is **not** `dyn AtomicMutation`.
/// The public composition surface (`enroll::<Inner>`) is fully
/// static/monomorphized; this is just how the executor stores "the work to undo
/// entry N" uniformly. The `'a` bound lets a closure borrow the `Repository`
/// (e.g. to restore a ref) for the lifetime of the transaction.
type RewindFn<'a> = Box<dyn FnOnce() -> Result<()> + 'a>;

/// One entry in the rewind ledger: a boxed inverse closure plus a `'static`
/// label naming the action that registered it. The label is pure diagnostics —
/// it lets [`rewind_all`](Tx::rewind_all)'s logging name the action that ran or
/// failed during an unwind — and carries no public-API or behavioral weight.
struct RewindAction<'a> {
    label: &'static str,
    run: RewindFn<'a>,
}

pub(crate) type EnrolledMutation<M> = Rc<RefCell<Option<M>>>;
pub(crate) type EnrolledRoot<M> = Rc<RefCell<M>>;

/// The ledger snapshot handed to [`AtomicMutation::rewind`](super::AtomicMutation::rewind).
/// Carries the per-transaction scope + depth captured at apply time; a mutation
/// keeps whatever else it needs in its own fields.
#[derive(Clone, Debug)]
pub struct RewindLedger {
    /// The checkout lane (`Repository::op_scope`) the transaction recorded
    /// under (heddle#330 §1.5).
    pub scope: String,
    /// Nesting depth at which the mutation was enrolled.
    pub depth: u32,
}

/// The transaction context threaded through a mutation nest.
///
/// Holds the `Repository` handle, the checkout `scope`, the idempotency
/// `transaction_id`, the nesting `depth`, and the reverse-order rewind ledger.
/// Bound to the file-backed [`Repository`] (the local CLI path the primitive
/// targets in impl-a); the hosted/Postgres path uses its own gRPC handlers.
pub struct Tx<'a> {
    repo: &'a Repository,
    scope: String,
    transaction_id: String,
    isolation: IsolationPrecondition,
    depth: u32,
    ledger: Vec<RewindAction<'a>>,
    committed: bool,
}

impl<'a> Tx<'a> {
    /// Build the root transaction: depth 0, a fresh ledger, scoped to this
    /// checkout's lane, keyed by the caller-supplied **stable** idempotency
    /// `transaction_id` (heddle#330 §2.2, cid 3329490982). The id must be the
    /// same across retries of the same logical op — see
    /// [`AtomicMutation::transaction_id`](super::AtomicMutation::transaction_id)
    /// — so a crash-retry deduplicates against the prior commit instead of
    /// minting a new key and double-applying.
    pub(crate) fn root(
        repo: &'a Repository,
        transaction_id: String,
        isolation: IsolationPrecondition,
    ) -> Self {
        Self {
            repo,
            scope: repo.op_scope(),
            transaction_id,
            isolation,
            depth: 0,
            ledger: Vec::new(),
            committed: false,
        }
    }

    /// The repository handle this transaction operates on.
    pub fn repo(&self) -> &'a Repository {
        self.repo
    }

    /// The checkout lane (`op_scope`) every record in this transaction is
    /// keyed under, so a sibling checkout's executor never unwinds this one.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// The idempotency key — the same id used for the `TransactionCommit`
    /// marker and (eventually) the on-disk sentinel (§3.4).
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    /// Current nesting depth.
    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// Run a reversible **leaf effect** as one atomic step: execute `forward`
    /// FIRST, and push `inverse` onto the rewind ledger **only if `forward`
    /// returned `Ok`**. A `forward` that fails (or panics) leaves the ledger
    /// untouched, so it is structurally impossible to register an inverse for an
    /// effect that did not happen — the register-then-forward footgun that
    /// corrupted pre-existing refs on rollback when a forward failed after its
    /// compensator was already queued (heddle#355 cid 3330867774 / 3330867775).
    ///
    /// This is the ONLY way for code outside `atomic` to add to the rewind
    /// ledger: the raw `on_rewind` register is `pub(crate)`, so a consumer that
    /// tried to hand-order a compensator ahead of its forward would not compile.
    /// `inverse` runs in reverse (LIFO) order with the rest of the ledger on any
    /// later unwind and may borrow the [`Repository`](crate::Repository) for the
    /// transaction lifetime. Reach for [`enroll`](Self::enroll) /
    /// [`enroll_eager`](Self::enroll_eager) instead when the unit of work is a
    /// whole sub-mutation rather than a single reversible write.
    pub fn step<T, Fwd, Inv>(&mut self, forward: Fwd, inverse: Inv) -> Result<T>
    where
        Fwd: FnOnce() -> Result<T>,
        Inv: FnOnce() -> Result<()> + 'a,
    {
        let value = forward()?;
        // Reached only on a successful forward: the inverse now compensates an
        // effect that demonstrably happened.
        self.on_rewind("step", inverse);
        Ok(value)
    }

    /// Run a reversible leaf effect whose `forward` is **NOT** a single
    /// all-or-nothing write — a composite of several internal writes, or a
    /// materialization that can fail partway and leave a partial effect behind.
    ///
    /// Unlike [`step`](Self::step), which registers its inverse only AFTER an
    /// atomic `forward` returns `Ok`, this captures the prior state FIRST and
    /// registers `restore(snapshot)` on the rewind ledger **before** running
    /// `forward`. So if `forward` applies some of its writes and then fails (or
    /// applies all of them and a later step in the same transaction fails), the
    /// restore-to-snapshot inverse is already on the ledger and unwinds the
    /// partial effect back to exactly the captured state. The two combinators
    /// guard opposite hazards — `step` must never register before a forward
    /// could fail (the register-then-forward footgun, cid 3330867774 /
    /// 3330867775); `step_nonatomic` must register before, because its forward
    /// can leave state changed even when it returns `Err`. Reach for `step` only
    /// when the forward is a genuine single all-or-nothing write.
    ///
    /// This is NOT the register-then-forward footgun: the inverse here is a
    /// restore-to-captured-snapshot, which is correct whether the forward fully
    /// ran, partially failed, or never started — re-applying it from any of
    /// those states lands on the same captured snapshot. The footgun was a
    /// *specific-effect* inverse (e.g. "delete the marker I created") registered
    /// before its forward, which on a failed forward undoes an effect that never
    /// happened (deleting a pre-existing marker). Capture-restore has no such
    /// asymmetry.
    pub fn step_nonatomic<T, S>(
        &mut self,
        capture: impl FnOnce() -> Result<S>,
        restore: impl FnOnce(S) -> Result<()> + 'a,
        forward: impl FnOnce() -> Result<T>,
    ) -> Result<T>
    where
        S: 'a,
    {
        let snapshot = capture()?;
        self.on_rewind("step_nonatomic", move || restore(snapshot));
        forward()
    }

    /// Register an inverse for an effect just staged, under a `'static` `label`
    /// for rollback diagnostics. Closures run in reverse (LIFO) order on unwind.
    /// The closure may borrow the `Repository`.
    ///
    /// `pub(crate)` on purpose (heddle#355): this primitive has NO ordering
    /// enforcement, so calling it directly lets a caller register an inverse
    /// *before* — or *without* — its forward effect, which is the exact
    /// register-then-forward footgun the validation migration removed. Consumer
    /// crates compose reversible leaf effects through the forward-first
    /// [`step`](Self::step) combinator (the capture-restore
    /// [`step_nonatomic`](Self::step_nonatomic) for non-atomic forwards, or
    /// [`enroll`](Self::enroll) for whole sub-mutations); inside `atomic`,
    /// `step` / `step_nonatomic` / `enroll` / `enroll_whole_op` are its only
    /// callers.
    pub(crate) fn on_rewind<F>(&mut self, label: &'static str, f: F)
    where
        F: FnOnce() -> Result<()> + 'a,
    {
        self.ledger.push(RewindAction {
            label,
            run: Box::new(f),
        });
    }

    pub(crate) fn enroll_whole_op<M>(&mut self, m: M) -> EnrolledMutation<M>
    where
        M: super::AtomicMutation + 'a,
    {
        let mutation = Rc::new(RefCell::new(Some(m)));
        let rewind_mutation = Rc::clone(&mutation);
        let ledger = self.ledger_view();
        self.on_rewind("enroll_whole_op", move || {
            let Some(mut mutation) = rewind_mutation.borrow_mut().take() else {
                return Ok(());
            };
            mutation.rewind(&ledger)
        });
        mutation
    }

    pub(crate) fn enroll_root<M>(&mut self, m: M) -> EnrolledRoot<M>
    where
        M: super::AtomicMutation + 'a,
    {
        let mutation = Rc::new(RefCell::new(m));
        let rewind_mutation = Rc::clone(&mutation);
        let ledger = self.ledger_view();
        self.on_rewind("enroll_root", move || {
            rewind_mutation.borrow_mut().rewind(&ledger)
        });
        mutation
    }

    /// Shared `enroll`/`enroll_eager` scaffolding: pre-register the child's
    /// whole-op rewind (so an apply error or panic unwinds it through the shared
    /// ledger), then run its `apply` against the *same* ledger. Returns the
    /// enrolled handle (still live, for the eager `commit_eager` follow-up) plus
    /// the staged result. The caller owns the surrounding `depth` inc/dec.
    fn enroll_then_apply<M>(
        &mut self,
        m: M,
    ) -> (EnrolledMutation<M>, Result<StagedCommit<M::Output>>)
    where
        M: super::AtomicMutation + 'a,
    {
        let mutation = self.enroll_whole_op(m);
        let staged = {
            let mut guard = mutation.borrow_mut();
            guard
                .as_mut()
                .expect("enrolled mutation must be present during apply")
                .apply(self)
        };
        (mutation, staged)
    }

    /// Deferred enroll — bounded to [`DeferredMutation`] (§3.3). Runs only
    /// `apply` (staged, reversible) against the *same* ledger, then registers
    /// the child's `rewind` so an outer failure unwinds it. An
    /// `EagerMutation`-only mutation fails this bound at compile time.
    pub fn enroll<M>(&mut self, m: M) -> Result<StagedCommit<M::Output>>
    where
        M: DeferredMutation + 'a,
    {
        self.depth += 1;
        // On apply error or panic, the child's whole-op rewind is already on
        // the shared ledger, so the root unwind compensates both granular and
        // whole-op deferred staging uniformly.
        let (_mutation, staged) = self.enroll_then_apply(m);
        self.depth -= 1;
        staged
    }

    /// Eager enroll — bounded to [`EagerMutation`], whose sole method *returns*
    /// the [`Compensator`](super::Compensator). Stages via `apply`, runs
    /// `commit_eager`, and registers the returned compensator atomically. The
    /// compensator is guaranteed to exist because the bound requires the method
    /// that produces it — enrolling an eager op without a compensator does not
    /// compile (§3.3).
    pub fn enroll_eager<M>(&mut self, m: M) -> Result<M::Output>
    where
        M: EagerMutation + 'a,
    {
        self.depth += 1;
        let (mutation, staged) = self.enroll_then_apply(m);
        let staged = match staged {
            Ok(staged) => staged,
            Err(err) => {
                self.depth -= 1;
                return Err(err);
            }
        };
        let compensator = {
            let mut guard = mutation.borrow_mut();
            guard
                .as_mut()
                .expect("enrolled mutation must be present during eager commit")
                .commit_eager(self)
        };
        let compensator = match compensator {
            Ok(compensator) => compensator,
            Err(err) => {
                self.depth -= 1;
                return Err(err);
            }
        };
        self.ledger.push(RewindAction {
            label: "compensator",
            run: compensator.into_fn(),
        });
        // One-mechanism contract (heddle#354 r4): an eager mutation's undo is its
        // `Compensator` (returned by `commit_eager`), NEVER the whole-op
        // `rewind`. `enroll_whole_op` above registered the whole-op rewind only
        // to clean up a FAILED `apply`/`commit_eager` (the early returns reach
        // it via the shared ledger). Now that `commit_eager` has succeeded and
        // the compensator owns the undo, take the mutation out of its cell so
        // that pre-registered whole-op rewind closure finds `None` and is a
        // guaranteed no-op. Without this, an `EagerMutation` that overrode
        // `rewind` would run BOTH the override AND the compensator on an outer
        // rollback — a double-undo.
        let _ = mutation.borrow_mut().take();
        self.depth -= 1;
        Ok(staged.output)
    }

    pub(crate) fn ledger_view(&self) -> RewindLedger {
        RewindLedger {
            scope: self.scope.clone(),
            depth: self.depth,
        }
    }

    /// THE commit point (heddle#330 §2.2 phase 4): append the accumulated
    /// records plus a `TransactionCommit` marker as one batch, deduplicated by
    /// the **unbounded indexed `transaction_id`** lookup — so a crash-retry at
    /// any later time is exact-once. After this returns `Ok`, the transaction
    /// is committed and `Drop` becomes a no-op.
    pub(crate) fn commit(&mut self, mut records: Vec<OpRecord>) -> Result<CommitOutcome> {
        if self.committed {
            return Ok(CommitOutcome::Committed);
        }
        let op_count = records.len() as u32;
        records.push(OpRecord::TransactionCommit {
            transaction_id: self.transaction_id.clone(),
            op_count,
        });
        let outcome = self.repo.oplog().record_batch_exactly_once_if_unchanged(
            records,
            Some(&self.scope),
            &self.transaction_id,
            &self.isolation,
        )?;
        match outcome {
            ConditionalCommitOutcome::Committed(_) => {
                self.committed = true;
                Ok(CommitOutcome::Committed)
            }
            ConditionalCommitOutcome::AlreadyCommitted(prior) => {
                // Dedup hit: a prior (possibly cross-process) run already
                // committed this transaction. Carry its records back so the
                // executor can reconstruct the originally-committed output
                // (cid 3329631075).
                Ok(CommitOutcome::AlreadyCommitted(prior))
            }
            ConditionalCommitOutcome::IsolationConflict {
                key,
                since_head_id,
                conflicting_entry_id,
            } => Ok(CommitOutcome::IsolationConflict {
                key,
                since_head_id,
                conflicting_entry_id,
            }),
        }
    }

    /// Walk the ledger in reverse (LIFO) order, running every inverse. Drains
    /// the ledger so a second call (e.g. from `Drop`) is a no-op. Surfaces the
    /// first rewind error after attempting every entry.
    pub(crate) fn rewind_all(&mut self) -> Result<()> {
        let mut first_err: Option<HeddleError> = None;
        while let Some(action) = self.ledger.pop() {
            if let Err(e) = (action.run)() {
                if first_err.is_none() {
                    first_err = Some(e);
                } else {
                    tracing::error!(
                        action = action.label,
                        error = %e,
                        "additional Tx rewind failure (suppressed)"
                    );
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum CommitOutcome {
    Committed,
    /// The transaction was already committed by a prior run; carries that
    /// committed batch's records (marker stripped) so the executor can
    /// reconstruct the originally-committed output (cid 3329631075).
    AlreadyCommitted(Vec<OpRecord>),
    IsolationConflict {
        key: IsolationKey,
        since_head_id: u64,
        conflicting_entry_id: u64,
    },
}

impl Drop for Tx<'_> {
    /// Backstop for the panic path (heddle#330 §4): a `Tx` dropped **without**
    /// having reached `commit` (a panic unwound through `apply`) is by
    /// construction pre-commit, so the safe action is always to rewind the
    /// staged effects — never to half-commit. It NEVER appends to the oplog.
    /// Logs (does not panic) on a rewind error to avoid a double-panic abort.
    fn drop(&mut self) {
        if !self.committed
            && let Err(e) = self.rewind_all()
        {
            tracing::error!(
                error = %e,
                "Tx Drop rewind failed; staged effects may persist as orphans \
                 (gc-collectable) — see transaction sentinel for recovery"
            );
        }
    }
}
