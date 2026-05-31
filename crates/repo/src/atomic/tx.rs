// SPDX-License-Identifier: Apache-2.0
//! The transaction context `Tx` + the reverse-order rewind ledger
//! (heddle#330 §3.1, §4).
//!
//! `Tx` threads through every `apply` in a nest. Inner mutations enroll into
//! the *same* `Tx` (savepoint default) rather than committing independently;
//! only the outermost [`execute`](super::execute) reaches `commit`. The ledger
//! is a LIFO stack of inverse closures, popped in reverse on any unwind.

use objects::error::{HeddleError, Result};
use oplog::OpRecord;

use super::traits::{EagerMutation, SavepointMutation, StagedCommit};
use crate::Repository;

/// One entry in the rewind ledger: a boxed inverse closure. Boxing a *closure*
/// is an implementation detail of the ledger — it is **not** `dyn
/// AtomicMutation`. The public composition surface (`enroll::<Inner>`) is fully
/// static/monomorphized; this is just how the executor stores "the work to undo
/// entry N" uniformly. The `'a` bound lets a closure borrow the `Repository`
/// (e.g. to restore a ref) for the lifetime of the transaction.
type RewindFn<'a> = Box<dyn FnOnce() -> Result<()> + 'a>;

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
    depth: u32,
    ledger: Vec<RewindFn<'a>>,
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
    pub(crate) fn root(repo: &'a Repository, transaction_id: String) -> Self {
        Self {
            repo,
            scope: repo.op_scope(),
            transaction_id,
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

    /// Register an inverse for an effect just staged. Closures run in reverse
    /// (LIFO) order on unwind. The closure may borrow the `Repository`.
    pub fn on_rewind<F>(&mut self, f: F)
    where
        F: FnOnce() -> Result<()> + 'a,
    {
        self.ledger.push(Box::new(f));
    }

    /// Savepoint enroll — bounded to [`SavepointMutation`] (§3.3). Runs only
    /// `apply` (staged, reversible) against the *same* ledger, then registers
    /// the child's `rewind` so an outer failure unwinds it. An
    /// `EagerMutation`-only mutation fails this bound at compile time.
    pub fn enroll<M>(&mut self, mut m: M) -> Result<StagedCommit<M::Output>>
    where
        M: SavepointMutation + 'a,
    {
        self.depth += 1;
        // On apply error, any granular inverses the child already pushed stay
        // on the ledger and run when the root unwinds.
        let staged = m.apply(self)?;
        let ledger = self.ledger_view();
        self.on_rewind(move || m.rewind(&ledger));
        self.depth -= 1;
        Ok(staged)
    }

    /// Eager enroll — bounded to [`EagerMutation`], whose sole method *returns*
    /// the [`Compensator`](super::Compensator). Stages via `apply`, runs
    /// `commit_eager`, and registers the returned compensator atomically. The
    /// compensator is guaranteed to exist because the bound requires the method
    /// that produces it — enrolling an eager op without a compensator does not
    /// compile (§3.3).
    pub fn enroll_eager<M>(&mut self, mut m: M) -> Result<M::Output>
    where
        M: EagerMutation + 'a,
    {
        self.depth += 1;
        let staged = m.apply(self)?;
        let compensator = m.commit_eager(self)?;
        self.ledger.push(compensator.into_fn());
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
    pub(crate) fn commit(&mut self, mut records: Vec<OpRecord>) -> Result<()> {
        if self.committed {
            return Ok(());
        }
        let op_count = records.len() as u32;
        records.push(OpRecord::TransactionCommit {
            transaction_id: self.transaction_id.clone(),
            op_count,
        });
        self.repo.oplog().record_batch_exactly_once(
            records,
            Some(&self.scope),
            &self.transaction_id,
        )?;
        self.committed = true;
        Ok(())
    }

    /// Walk the ledger in reverse (LIFO) order, running every inverse. Drains
    /// the ledger so a second call (e.g. from `Drop`) is a no-op. Surfaces the
    /// first rewind error after attempting every entry.
    pub(crate) fn rewind_all(&mut self) -> Result<()> {
        let mut first_err: Option<HeddleError> = None;
        while let Some(f) = self.ledger.pop() {
            if let Err(e) = f() {
                if first_err.is_none() {
                    first_err = Some(e);
                } else {
                    tracing::error!(error = %e, "additional Tx rewind failure (suppressed)");
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
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
