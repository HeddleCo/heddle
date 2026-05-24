// SPDX-License-Identifier: Apache-2.0
//! Type-state substrate for the [`crate::core::Pending`] write-overlay
//! FSM (heddle#208 — sub 1 of the heddle#206 retrofit chain).
//!
//! Substrate-only — the types and witness constructors land here; the
//! callsite retrofits that replace the per-method runtime preconditions
//! with witness-gated signatures land in heddle#209 / #210 / #211 / #212.
//!
//! See [`docs/design/mount-pending-api-contracts.md`][doc] §2 for the
//! design and §3 for how each r11 finding becomes a compile-time error
//! once the retrofits land.
//!
//! [doc]: ../../../../docs/design/mount-pending-api-contracts.md

// Red-commit stub. The substrate types and `Pending::witness_*`
// constructors land in the green commit; the tests below name the
// expected API surface so the compile failure pins the contract.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{NodeState, Pending};

    #[test]
    fn witness_live_nonzero_some_for_live_with_open() {
        let mut p = Pending::default();
        p.test_insert_state(7, NodeState::Live { open_count: 1 });
        let w = p.witness_live_nonzero(7).expect("LiveNonZero witness");
        assert_eq!(w.id(), 7);
    }

    #[test]
    fn witness_live_nonzero_none_for_live_zero() {
        let mut p = Pending::default();
        p.test_insert_state(7, NodeState::Live { open_count: 0 });
        assert!(p.witness_live_nonzero(7).is_none());
    }
}
