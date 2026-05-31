// SPDX-License-Identifier: Apache-2.0
//! The generic executor (heddle#330 §2.1).
//!
//! [`execute`] is the single entry point that enforces the commit point and
//! the reverse-order rewind exactly once. Monomorphized per mutation type —
//! zero vtable. The bound `M: AtomicMutation` makes "register an atomic op
//! without a `rewind`" a compile error.

use objects::error::Result;

use super::traits::{AtomicMutation, StagedCommit};
use super::tx::Tx;
use crate::Repository;

/// Run a mutation as one all-or-nothing transaction.
///
/// 1. Stage every reversible effect (`apply`), registering inverses on the
///    ledger. A pre-commit `Err` (or a panic — caught by `Tx::drop`) unwinds
///    the ledger in reverse order and the oplog is never touched.
/// 2. Reach the single commit point: append the staged records + a
///    `TransactionCommit` marker, deduplicated by the unbounded
///    `transaction_id` index (exact-once at any retry timing).
///
/// On commit failure the staged effects are rewound too, so the call is
/// all-or-nothing.
pub fn execute<'a, M>(repo: &'a Repository, mut m: M) -> Result<M::Output>
where
    M: AtomicMutation + 'a,
{
    // The stable idempotency key, supplied by the mutation (cid 3329490982);
    // identical across retries so a crash-retry deduplicates instead of
    // double-applying.
    let mut tx = Tx::root(repo, m.transaction_id());

    let staged = match m.apply(&mut tx) {
        Ok(staged) => staged,
        Err(e) => {
            // The whole-op rewind must run on the apply-`Err` path too, or a
            // mutation that uses it (rather than granular `on_rewind` inverses)
            // would leak the state its `apply` staged before failing (cid
            // 3329490979). Register it exactly as the commit-failure path below,
            // then unwind: the granular ledger AND the whole-op rewind run
            // together (one is a no-op per the one-mechanism-per-mutation
            // contract), so zero staged state survives a failed `apply`.
            let ledger = tx.ledger_view();
            tx.on_rewind(move || m.rewind(&ledger));
            let _ = tx.rewind_all();
            return Err(e);
        }
    };

    let StagedCommit { output, oplog } = staged;

    // Register the top mutation's whole-op rewind (a no-op for mutations that
    // registered granular inverses). Moves `m` into the ledger closure; on a
    // successful commit it is never invoked and drops with the ledger.
    let ledger = tx.ledger_view();
    tx.on_rewind(move || m.rewind(&ledger));

    match tx.commit(oplog) {
        Ok(()) => Ok(output),
        Err(e) => {
            let _ = tx.rewind_all();
            Err(e)
        }
    }
}
