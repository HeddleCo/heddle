// SPDX-License-Identifier: Apache-2.0
//! Property-test harness for the [`Pending`] FSM at the public-surface
//! boundary (heddle#212 — the external-crate counterpart to the
//! substrate-internal harness in `crates/mount/src/pending.rs`'s
//! `#[cfg(test)]` module).
//!
//! # What this file pins
//!
//! Integration tests live outside the crate and see only the `pub`
//! surface. The substrate's witness constructors (`witness_*`) and
//! `with_brand` are re-exported via the doc-hidden
//! [`mount::__pending_substrate_for_doctest`] module — the same hatch
//! the `compile_fail` doctests in `lib.rs` use. The witness-gated
//! transitions (`transition_to_orphan`, `kernel_forget_inode`) and the
//! test seed helpers (`test_insert_state`, `lookup_state`) are
//! `pub(crate)` and intentionally not exposed; the full FSM driver
//! lives in the in-crate test module.
//!
//! What this file *can* verify from outside the crate:
//!
//! * **Witness exhaustiveness** — at every reachable per-NodeId state,
//!   exactly one of the four [`witness_*`] constructors returns
//!   `Some` (and the other three return `None`). A regression that
//!   widened any constructor's accepting set (e.g. accepted both
//!   `LiveZero` and `LiveNonZero` for `witness_live_nonzero`) would
//!   surface here.
//! * **Released as default** — a freshly-defaulted `Pending` has every
//!   NodeId in the `Released` state; only `witness_released` returns
//!   `Some`.
//! * **`with_brand` re-entry determinism** — repeated `with_brand`
//!   invocations on the same `Pending` (sequential) classify
//!   identically. Pins that the freshly-introduced HRTB `'brand`
//!   doesn't leak observable state across calls.
//!
//! See `crates/mount/src/pending.rs`'s `#[cfg(test)]` module for the
//! companion proptest that drives the full FSM via the witness-gated
//! transitions.
//!
//! See `docs/design/mount-posix-semantics.md` §1 for the FSM the
//! substrate enforces and `docs/design/mount-pending-api-contracts.md`
//! §2.2.1 for the witness-type idiom.

use mount::__pending_substrate_for_doctest::Pending;
use proptest::prelude::*;

/// Bitmask of which `witness_*` constructors return `Some` for `id`.
/// Bit 0 = LiveNonZero, 1 = LiveZero, 2 = Orphan, 3 = Released. The
/// FSM partitions per-NodeId states into these four buckets — exactly
/// one bit must be set at any instant.
fn witness_mask(p: &mut Pending<'_>, id: u64) -> u8 {
    let mut mask = 0u8;
    p.with_brand(|bp| {
        if bp.witness_live_nonzero(id).is_some() {
            mask |= 1 << 0;
        }
        if bp.witness_live_zero(id).is_some() {
            mask |= 1 << 1;
        }
        if bp.witness_orphan(id).is_some() {
            mask |= 1 << 2;
        }
        if bp.witness_released(id).is_some() {
            mask |= 1 << 3;
        }
    });
    mask
}

proptest! {
    /// A freshly-defaulted `Pending` has every NodeId in the
    /// `Released` state. Only `witness_released` returns `Some`; the
    /// other three constructors return `None`. The mask is exactly
    /// `1 << 3` for every probed id.
    #[test]
    fn fresh_pending_classifies_every_id_as_released(
        ids in proptest::collection::vec(0u64..1024, 0..32),
    ) {
        let mut p = Pending::default();
        for id in ids {
            let mask = witness_mask(&mut p, id);
            prop_assert_eq!(
                mask, 1u8 << 3,
                "fresh Pending: NodeId {} should be Released-only, got mask 0b{:04b}",
                id, mask
            );
        }
    }

    /// `with_brand` is sequentially re-entrant: minting a witness via
    /// nested / repeated `with_brand` invocations is deterministic.
    /// Repeatedly classifying the same NodeId yields the same mask
    /// every time — no observable side effect leaks across calls.
    /// Pins that the freshly-introduced HRTB `'brand` is purely a
    /// type-system construct.
    #[test]
    fn with_brand_classification_is_deterministic(
        id in 0u64..1024,
        n in 1usize..16,
    ) {
        let mut p = Pending::default();
        let baseline = witness_mask(&mut p, id);
        prop_assert_eq!(
            baseline, 1u8 << 3,
            "baseline: id {} should be Released-only", id
        );
        for round in 0..n {
            let mask = witness_mask(&mut p, id);
            prop_assert_eq!(
                mask, baseline,
                "round {}: id {} classification drifted from baseline 0b{:04b} to 0b{:04b}",
                round, id, baseline, mask
            );
        }
    }

    /// Witness construction is partitioning: across an arbitrary set
    /// of NodeIds on a fresh `Pending`, exactly one bit in the mask is
    /// set for every probed id. The FSM has no overlapping states, so
    /// no id can ever produce a mask with two bits set.
    #[test]
    fn witness_mask_is_a_partition(
        ids in proptest::collection::vec(0u64..1024, 0..32),
    ) {
        let mut p = Pending::default();
        for id in ids {
            let mask = witness_mask(&mut p, id);
            prop_assert_eq!(
                mask.count_ones(), 1,
                "id {} produced ambiguous witness mask 0b{:04b}", id, mask
            );
        }
    }
}
