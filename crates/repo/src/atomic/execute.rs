// SPDX-License-Identifier: Apache-2.0
//! The generic executor (heddle#330 §2.1).
//!
//! [`execute`] is the single entry point that enforces the commit point and
//! the reverse-order rewind exactly once. Monomorphized per mutation type —
//! zero vtable. The bound `M: AtomicMutation` makes "register an atomic op
//! without a `rewind`" a compile error.

use std::{
    rc::Rc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use objects::error::{HeddleError, Result};
use oplog::IsolationPrecondition;

use super::{
    traits::{AtomicMutation, StagedCommit},
    tx::{CommitOutcome, ReconstructibleTxCommit, Tx},
};
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
    let transaction_id = m.transaction_id();
    execute_attempts(repo, m, transaction_id)
}

pub(crate) struct ReconstructibleExecution<O, A> {
    pub output: O,
    pub artifact: Option<A>,
}

/// Structured-snapshot executor whose immutable pack artifact is the commit
/// point and whose oplog entries are a reconstructible materialized view.
pub(crate) fn execute_reconstructible<'a, M, A>(
    repo: &'a Repository,
    m: M,
    mut install: impl FnMut(&mut M, u64, &[oplog::OpRecord]) -> Result<A>,
) -> Result<ReconstructibleExecution<M::Output, A>>
where
    M: AtomicMutation + 'a,
{
    const MAX_ATTEMPTS: u32 = 4;
    let transaction_id = m.transaction_id();
    let mut m = m;

    for attempt in 0..MAX_ATTEMPTS {
        let keys = m.isolation_keys(repo)?;
        let since_head_id = repo.oplog().head_id()?;
        let isolation = IsolationPrecondition {
            since_head_id,
            keys,
        };
        let mut tx = Tx::root(repo, transaction_id.clone(), isolation);
        let mutation = tx.enroll_root(m);
        let staged = match mutation.borrow_mut().apply(&mut tx) {
            Ok(staged) => staged,
            Err(error) => return rewind_error(&mut tx, error),
        };
        let StagedCommit { output, oplog } = staged;

        let outcome = tx.commit_reconstructible(oplog, |base_head_id, records| {
            install(&mut mutation.borrow_mut(), base_head_id, records)
        });
        match outcome {
            Ok(ReconstructibleTxCommit::Committed(artifact)) => {
                return Ok(ReconstructibleExecution {
                    output,
                    artifact: Some(artifact),
                });
            }
            Ok(ReconstructibleTxCommit::AlreadyCommitted(prior_records)) => {
                let reconstructed = mutation
                    .borrow()
                    .reconstruct_committed_output(&prior_records, output);
                match (reconstructed, tx.rewind_all()) {
                    (Ok(output), Ok(())) => {
                        return Ok(ReconstructibleExecution {
                            output,
                            artifact: None,
                        });
                    }
                    (Ok(_), Err(rewind_err)) => {
                        return Err(HeddleError::Conflict(format!(
                            "transaction {} was already committed, but replay rewind failed: {}",
                            tx.transaction_id(),
                            rewind_err
                        )));
                    }
                    (Err(error), _) => return Err(error),
                }
            }
            Ok(ReconstructibleTxCommit::IsolationConflict {
                key,
                since_head_id,
                conflicting_entry_id,
            }) => {
                if let Err(rewind_err) = tx.rewind_all() {
                    return Err(HeddleError::Conflict(format!(
                        "transaction {} isolation conflict on {:?} since head {} at oplog entry {}; rewind failed: {}",
                        tx.transaction_id(),
                        key,
                        since_head_id,
                        conflicting_entry_id,
                        rewind_err
                    )));
                }
                if attempt + 1 == MAX_ATTEMPTS {
                    return Err(HeddleError::Conflict(format!(
                        "transaction {} isolation conflict on {:?} since head {} at oplog entry {} after {} attempts",
                        tx.transaction_id(),
                        key,
                        since_head_id,
                        conflicting_entry_id,
                        MAX_ATTEMPTS
                    )));
                }
                m = Rc::try_unwrap(mutation)
                    .ok()
                    .expect("root mutation must have no ledger clones after rewind")
                    .into_inner();
                sleep_full_jitter(attempt);
            }
            Err(error) => return rewind_error(&mut tx, error),
        }
    }
    unreachable!("bounded retry loop always returns on final attempt")
}

fn execute_attempts<'a, M>(
    repo: &'a Repository,
    mut m: M,
    transaction_id: String,
) -> Result<M::Output>
where
    M: AtomicMutation + 'a,
{
    const MAX_ATTEMPTS: u32 = 4;

    for attempt in 0..MAX_ATTEMPTS {
        let keys = m.isolation_keys(repo)?;
        let since_head_id = repo.oplog().head_id()?;
        let isolation = IsolationPrecondition {
            since_head_id,
            keys,
        };
        let mut tx = Tx::root(repo, transaction_id.clone(), isolation);
        let mutation = tx.enroll_root(m);

        let staged = mutation.borrow_mut().apply(&mut tx);
        let staged = match staged {
            Ok(staged) => staged,
            Err(e) => return rewind_error(&mut tx, e),
        };

        let StagedCommit { output, oplog } = staged;

        match tx.commit(oplog) {
            Ok(CommitOutcome::Committed) => return Ok(output),
            Ok(CommitOutcome::AlreadyCommitted(prior_records)) => {
                // Dedup hit: this run's `output` may diverge from what was
                // actually committed (e.g. a regenerated `StateId`), so
                // reconstruct the originally-committed identity from the prior
                // batch (cid 3329631075). Reconstruct BEFORE rewinding.
                let reconstructed = mutation
                    .borrow()
                    .reconstruct_committed_output(&prior_records, output);
                match (reconstructed, tx.rewind_all()) {
                    (Ok(committed_output), Ok(())) => return Ok(committed_output),
                    (Ok(_), Err(rewind_err)) => {
                        return Err(HeddleError::Conflict(format!(
                            "transaction {} was already committed, but replay rewind failed: {}",
                            tx.transaction_id(),
                            rewind_err
                        )));
                    }
                    (Err(reconstruct_err), _) => return Err(reconstruct_err),
                }
            }
            Ok(CommitOutcome::IsolationConflict {
                key,
                since_head_id,
                conflicting_entry_id,
            }) => {
                if let Err(rewind_err) = tx.rewind_all() {
                    return Err(HeddleError::Conflict(format!(
                        "transaction {} isolation conflict on {:?} since head {} at oplog entry {}; rewind failed: {}",
                        tx.transaction_id(),
                        key,
                        since_head_id,
                        conflicting_entry_id,
                        rewind_err
                    )));
                }
                if attempt + 1 == MAX_ATTEMPTS {
                    return Err(HeddleError::Conflict(format!(
                        "transaction {} isolation conflict on {:?} since head {} at oplog entry {} after {} attempts",
                        tx.transaction_id(),
                        key,
                        since_head_id,
                        conflicting_entry_id,
                        MAX_ATTEMPTS
                    )));
                }

                m = Rc::try_unwrap(mutation)
                    .ok()
                    .expect("root mutation must have no ledger clones after rewind")
                    .into_inner();
                sleep_full_jitter(attempt);
            }
            Err(e) => return rewind_error(&mut tx, e),
        }
    }

    unreachable!("bounded retry loop always returns on final attempt")
}

fn sleep_full_jitter(attempt: u32) {
    let max_ms = 10u64.saturating_mul(1u64 << attempt).min(250);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos() as u64)
        .unwrap_or(0);
    thread::sleep(Duration::from_millis(nanos % (max_ms + 1)));
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
