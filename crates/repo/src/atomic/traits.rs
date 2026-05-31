// SPDX-License-Identifier: Apache-2.0
//! The `AtomicMutation` trait family (heddle#330 §2.1, §3.3).
//!
//! A single all-or-nothing mutation implements [`AtomicMutation`]; the
//! generic [`execute`](super::execute) enforces the commit point and the
//! reverse-order rewind exactly once. Composition stays **`dyn`-free** on the
//! public surface — `execute`/`enroll` are monomorphized per mutation type and
//! no mutation is ever invoked through a vtable.

use objects::error::Result;
use oplog::OpRecord;

use super::tx::{RewindLedger, Tx};

/// What `apply` returns: the value to surface to the caller plus the oplog
/// record(s) the executor appends **at the commit point**. The mutation never
/// appends to the oplog itself; it hands the records to the executor so the
/// append happens once, last (heddle#330 §2.2 phase 4).
pub struct StagedCommit<T> {
    /// The value produced on a committed run (e.g. the new `ChangeId`).
    pub output: T,
    /// Records to append at the single commit point. A composite mutation
    /// merges its enrolled children's records into this vector so the whole
    /// nest commits in one batch.
    pub oplog: Vec<OpRecord>,
}

impl<T> StagedCommit<T> {
    pub fn new(output: T, oplog: Vec<OpRecord>) -> Self {
        Self { output, oplog }
    }

    /// A staged commit that contributes no oplog records of its own.
    pub fn pure(output: T) -> Self {
        Self {
            output,
            oplog: Vec::new(),
        }
    }
}

/// A single all-or-nothing mutation.
///
/// Implementors supply the staged forward work (`apply`) and their own
/// idempotent rewind. `apply` performs only **staged, not-yet-visible** side
/// effects — object-store puts (orphan until referenced), FS temp writes, ref
/// temp writes — and registers each effect's inverse on the transaction via
/// [`Tx::step`] (forward-first). It MUST NOT publish a canonical ref or append
/// to the oplog; both happen at/after the executor's single commit step.
pub trait AtomicMutation {
    /// The value produced on a committed run.
    type Output;

    /// A **stable** idempotency key for this logical operation — identical
    /// across retries of the *same* op (heddle#330 §2.2 "Idempotency of the
    /// commit", cid 3329490982). It MUST be derived deterministically from the
    /// operation's identity (its inputs / op-id), NEVER minted fresh per
    /// [`execute`](super::execute): a crash after the commit append but before
    /// the caller observes success is re-run by the caller, and a freshly-minted
    /// key would miss the unbounded dedup scan and double-apply. With a stable
    /// key the replayed op presents the same id, the dedup lookup finds the
    /// prior commit, and the second run is a no-op. Required (no default), so the
    /// "minted fresh" footgun is unrepresentable. Only the *root* mutation's key
    /// is used — an enrolled child never reaches the commit point.
    fn transaction_id(&self) -> String;

    /// Forward, staged, fallible side effects. Every effect performed here
    /// MUST be paired with an inverse registered via [`Tx::step`] (the
    /// forward-first granular ledger), OR be undone wholesale by
    /// [`AtomicMutation::rewind`]. Use one mechanism per mutation, not both.
    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<Self::Output>>;

    /// Undo whatever THIS mutation's `apply` staged. Called in reverse order
    /// on any pre-commit failure or panic-unwind. MUST be idempotent (may run
    /// after a partial apply) and MUST undo ONLY what this invocation created,
    /// never pre-existing user state (the #302 r4 lesson). The default is a
    /// no-op for mutations that register their undo via [`Tx::step`].
    fn rewind(&mut self, _ledger: &RewindLedger) -> Result<()> {
        Ok(())
    }

    /// Reconstruct the output the ORIGINAL committed run produced, from the
    /// deduped committed record batch (heddle#354 r5, cid 3329631075). Called
    /// ONLY on a crash-retry that dedup-hits an already-committed
    /// `transaction_id`: this run re-ran `apply` and may have produced a
    /// *different* output than what was persisted — e.g. a freshly generated
    /// `ChangeId` — so returning this run's value would hand the caller an
    /// identity that does not match the committed record. Derive the committed
    /// identity from `committed_records` (the prior batch, marker stripped).
    ///
    /// The default returns this run's output unchanged, correct when the output
    /// is deterministic from the mutation's inputs (the common case). A mutation
    /// whose output is generated non-deterministically MUST override this to read
    /// the committed identity out of `committed_records`.
    fn reconstruct_committed_output(
        &self,
        committed_records: &[OpRecord],
        this_run: Self::Output,
    ) -> Result<Self::Output> {
        let _ = committed_records;
        Ok(this_run)
    }
}

/// Opt-in marker for a **savepoint-enrollable** mutation: its staged effects
/// are invisible to other readers until the outer commit publishes them, so it
/// may defer to the outermost commit (heddle#330 §3.1).
///
/// There is deliberately **no** blanket `impl<M: AtomicMutation>
/// SavepointMutation for M` — a mutation opts in explicitly, so a mutation that
/// is *only* an [`EagerMutation`] does NOT satisfy the [`Tx::enroll`] bound and
/// cannot be enrolled as a savepoint. This is the type-level half of the
/// compile-error guarantee (§3.3).
pub trait SavepointMutation: AtomicMutation {}

/// An **eager** mutation: its forward effect is cross-process-visible the
/// instant it runs (the #251 op-id reserve exemplar, §3.2), so it must commit
/// eagerly AND hand back a [`Compensator`]. The eager effect lives in
/// [`commit_eager`](EagerMutation::commit_eager), never in `apply` — the method
/// performs the effect and *returns* the compensator, so "perform the eager
/// effect" and "produce the compensator" are one call. The method is required,
/// so an eager mutation with no compensator cannot implement the trait at all
/// (§3.3).
pub trait EagerMutation: AtomicMutation {
    /// Run the eager, cross-process-visible effect (e.g. `store.reserve`) and
    /// return the compensator the outer `Tx` stores. Separate from `rewind`
    /// because an eager leg's undo is a *forward* compensating action
    /// (cancel/release), not a staged-state rollback.
    fn commit_eager(&mut self, tx: &mut Tx<'_>) -> Result<Compensator>;
}

/// The compensating action for an eagerly-committed sub-op. Run in reverse
/// order with the rest of the rewind ledger if the outer transaction fails.
///
/// Boxes a `'static` closure (eager compensators capture `Arc`-shared stores,
/// e.g. `store.cancel(op_id)`), kept opaque so callers cannot inspect or skip
/// it — the only way to obtain one is from
/// [`EagerMutation::commit_eager`].
pub struct Compensator(Box<dyn FnOnce() -> Result<()> + 'static>);

impl Compensator {
    pub fn new(f: impl FnOnce() -> Result<()> + 'static) -> Self {
        Self(Box::new(f))
    }

    pub(super) fn into_fn(self) -> Box<dyn FnOnce() -> Result<()> + 'static> {
        self.0
    }
}
