// SPDX-License-Identifier: Apache-2.0
//! The atomic-mutation primitive (heddle#330 impl-a).
//!
//! A heddle-native primitive that makes "multi-step mutation with a forgotten
//! or mis-ordered cleanup" structurally unrepresentable. The primitive is a
//! [`trait`](AtomicMutation) each mutation implements plus a generic
//! [`execute`] that enforces the commit point and the reverse-order rewind
//! exactly once.
//!
//! Design (see `docs/spikes/heddle-330-atomic-mutation-primitive.md`):
//! - **The oplog append is the SOLE commit point** ([`Tx::commit`]). Refs are a
//!   post-commit materialized view; a mutation is committed iff its
//!   `TransactionCommit` marker is durable, deduplicated by the **unbounded
//!   indexed `transaction_id`** lookup (`OpLog::record_batch_exactly_once`).
//! - **Nesting = enroll-into-outermost (defer the commit marker) by default**
//!   ([`Tx::enroll`]); eager-commit only for cross-process-visible effects
//!   ([`Tx::enroll_eager`] + [`EagerMutation`]). The deferred/eager split is
//!   enforced at the **type** level (a compile error, no runtime const).
//! - **Panic-safety:** explicit `Result` plumbing is primary; [`Tx`]'s `Drop`
//!   is an abort-only backstop that never half-commits.
//! - **`dyn`-free public surface:** `execute`/`enroll` are monomorphized; the
//!   ledger boxes *closures* internally, never `dyn AtomicMutation`.

mod committer;
mod execute;
mod reconciler;
mod traits;
mod tx;

#[cfg(test)]
mod tests;

pub use committer::OplogRefCommitter;
pub use execute::execute;
pub use reconciler::OplogRefReconciler;
pub use traits::{AtomicMutation, Compensator, DeferredMutation, EagerMutation, StagedCommit};
pub use tx::{RewindLedger, Tx};
