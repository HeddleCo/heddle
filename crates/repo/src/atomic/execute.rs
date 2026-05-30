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
    let mut tx = Tx::root(repo);

    let staged = match m.apply(&mut tx) {
        Ok(staged) => staged,
        Err(e) => {
            // Reverse-order unwind of whatever `apply` staged before failing.
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
