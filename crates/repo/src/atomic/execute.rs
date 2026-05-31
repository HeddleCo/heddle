// SPDX-License-Identifier: Apache-2.0
//! The generic executor (heddle#330 §2.1).
//!
//! [`execute`] is the single entry point that enforces the commit point and
//! the reverse-order rewind exactly once. Monomorphized per mutation type —
//! zero vtable. The bound `M: AtomicMutation` makes "register an atomic op
//! without a `rewind`" a compile error.

use objects::error::{HeddleError, Result};

use super::traits::{AtomicMutation, StagedCommit};
use super::tx::{CommitOutcome, Tx};
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
pub fn execute<'a, M>(repo: &'a Repository, m: M) -> Result<M::Output>
where
    M: AtomicMutation + 'a,
{
    // The stable idempotency key, supplied by the mutation (cid 3329490982);
    // identical across retries so a crash-retry deduplicates instead of
    // double-applying.
    let mut tx = Tx::root(repo, m.transaction_id());
    let mutation = tx.enroll_whole_op(m);

    let staged = {
        let mut guard = mutation.borrow_mut();
        guard
            .as_mut()
            .expect("root mutation must be present during apply")
            .apply(&mut tx)
    };
    let staged = match staged {
        Ok(staged) => staged,
        Err(e) => return rewind_error(&mut tx, e),
    };

    let StagedCommit { output, oplog } = staged;

    match tx.commit(oplog) {
        Ok(CommitOutcome::Committed) => Ok(output),
        Ok(CommitOutcome::AlreadyCommitted) => match tx.rewind_all() {
            Ok(()) => Ok(output),
            Err(rewind_err) => Err(HeddleError::Conflict(format!(
                "transaction {} was already committed, but replay rewind failed: {}",
                tx.transaction_id(),
                rewind_err
            ))),
        },
        Err(e) => rewind_error(&mut tx, e),
    }
}

fn rewind_error<T>(tx: &mut Tx<'_>, original: HeddleError) -> Result<T> {
    match tx.rewind_all() {
        Ok(()) => Err(original),
        Err(rewind_err) => Err(HeddleError::Conflict(format!(
            "transaction failed: {}; rewind failed: {}",
            original, rewind_err
        ))),
    }
}
