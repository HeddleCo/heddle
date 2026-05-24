// SPDX-License-Identifier: Apache-2.0
//! Type-state substrate for the [`crate::core::Pending`] write-overlay
//! FSM (heddle#208 — sub 1 of the heddle#206 retrofit chain).
//!
//! # What this module is
//!
//! A set of zero-sized marker types — one per per-NodeId lifecycle
//! state — plus two witness structs that, once owned, are proof at the
//! type level that `Pending` saw that NodeId in that state under the
//! current `&mut` borrow.
//!
//! The four lifecycle states match
//! [`docs/design/mount-posix-semantics.md`][posix] §1:
//!
//! * [`LiveNonZero`] — `Live { open_count >= 1 }`. Inode has an active
//!   binding in the inode map and at least one kernel fd open.
//! * [`LiveZero`] — `Live { open_count == 0 }`. Inode is still
//!   resolvable; no open fds.
//! * [`Orphan`] — directory entry gone (post-unlink / post-rename-over),
//!   bytes outlive the entry for as long as a kernel fd holds the
//!   NodeId.
//! * [`Released`] — entry retired; absence from
//!   [`crate::core::Pending`]'s `state` map. Never stored in the map —
//!   the marker exists so a witness can prove "no entry at this id"
//!   for the `Pending::witness_*` constructors that need it.
//!
//! # What this module is **not**
//!
//! Substrate-only — the witness-gated method signatures and the
//! callsite retrofits that fix the four r11 P1 bugs land in the
//! follow-up issues (heddle#209/#210/#211/#212). Nothing in
//! [`crate::core`] consumes the substrate in this PR; every existing
//! method keeps its current signature. The substrate is `pub(crate)`-
//! visible from `core.rs` but unused there.
//!
//! See [`docs/design/mount-pending-api-contracts.md`][doc] §2.2.1 for
//! the witness-type idiom rationale and §3 for the bug-by-bug
//! impossibility analysis the retrofits will realise.
//!
//! [doc]: ../../../docs/design/mount-pending-api-contracts.md
//! [posix]: ../../../docs/design/mount-posix-semantics.md

// The substrate is intentionally unused by `core.rs` in this PR — the
// callsite retrofits that consume it land in heddle#209 / #210 / #211
// / #212. Test coverage exercises every witness constructor, so the
// types are not dead, but lints don't see through `#[cfg(test)]`-only
// use sites.
#![allow(dead_code)]

use std::marker::PhantomData;

use crate::core::{NodeState, Pending};

/// Crate-private "sealed" trait. Lives in a private inner module so
/// only this file can implement it for new types; the
/// [`Lifecycle`] supertrait bound therefore restricts the set of
/// types that can ever be a lifecycle state to the four named below.
mod sealed {
    pub trait Sealed {}
}

/// The per-NodeId lifecycle the FSM tracks. Sealed — only the four
/// ZSTs in this module implement it, and only this module can ever
/// add more.
pub(crate) trait Lifecycle: sealed::Sealed {}

/// Marker: `Live { open_count >= 1 }`.
///
/// At least one kernel fd holds this NodeId. The only state from
/// which `transition_to_orphan` is sound (closing the r11 #1 bug
/// once the retrofit lands).
#[derive(Debug, Clone, Copy)]
pub(crate) struct LiveNonZero;

/// Marker: `Live { open_count == 0 }`.
///
/// The inode is still resolvable but has no open kernel fds. Distinct
/// from [`LiveNonZero`] so a method that requires the "has open fds"
/// invariant can name it at the type level.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LiveZero;

/// Marker: `Orphan { open_count >= 0 }`.
///
/// Directory entry gone; bytes outlive it. The reverse of
/// [`LiveNonZero`] in the transition diagram — the destination of
/// `transition_to_orphan`, the only state from which the open-unlinked
/// last-close-wins flow runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Orphan;

/// Marker: entry absent from the `state` map.
///
/// `Released` is the lifecycle's "no entry here" state — distinct
/// from [`LiveZero`] (which has an entry with `open_count == 0`).
/// Never stored in the map; the marker exists purely so a
/// [`Witness<'_, Released>`] can prove the absence at the call site.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Released;

impl sealed::Sealed for LiveNonZero {}
impl sealed::Sealed for LiveZero {}
impl sealed::Sealed for Orphan {}
impl sealed::Sealed for Released {}

impl Lifecycle for LiveNonZero {}
impl Lifecycle for LiveZero {}
impl Lifecycle for Orphan {}
impl Lifecycle for Released {}

/// Type-level proof that `id` was in lifecycle state `S` under the
/// `&mut Pending` borrow `'p` that minted it.
///
/// # Invariants
///
/// * Constructed only by [`Pending::witness_live_nonzero`] /
///   [`Pending::witness_live_zero`] / [`Pending::witness_orphan`] /
///   [`Pending::witness_released`], each of which performs the
///   matching FSM check and returns `Some` iff it holds.
/// * The lifetime parameter `'p` is the lifetime of the `&mut Pending`
///   borrow that produced the witness. The borrow checker therefore
///   forbids any other code from mutating `Pending` while the witness
///   exists — closing the "stale witness across a mutation" hole
///   spelled out in
///   [`docs/design/mount-pending-api-contracts.md`][doc] §2.2.1.
/// * `S` is invariant via the [`PhantomData<&'p mut ()>`] field — the
///   compiler will not silently widen or narrow the state parameter.
/// * `!Send` and `!Sync` via the raw-pointer marker — the witness is
///   a single-thread, single-borrow token by design.
///
/// [doc]: ../../../docs/design/mount-pending-api-contracts.md
#[derive(Debug)]
pub(crate) struct Witness<'p, S: Lifecycle> {
    id: u64,
    _state: PhantomData<S>,
    // Invariance over `'p` + ties the witness to a `&mut Pending`
    // borrow. The `&'p mut ()` is for the lifetime relationship; the
    // borrow extension on `&'p mut Pending -> Witness<'p, _>` is what
    // makes the borrow checker refuse a concurrent mutable borrow of
    // `Pending` while the witness is alive.
    _borrow: PhantomData<&'p mut ()>,
    // `!Send` + `!Sync` marker. The witness is short-lived and tied
    // to one thread of execution by design; cross-thread transfer is
    // never sound.
    _not_send: PhantomData<*const ()>,
}

impl<'p, S: Lifecycle> Witness<'p, S> {
    /// Mint a witness. Visible only inside this module — every
    /// witness must come from a `Pending::witness_*` constructor that
    /// performed the FSM check.
    fn new(id: u64) -> Self {
        Self {
            id,
            _state: PhantomData,
            _borrow: PhantomData,
            _not_send: PhantomData,
        }
    }

    /// The NodeId the witness is bound to. Retrofitted method bodies
    /// will read this to know which entry of `Pending` to act on.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
}

/// Type-level proof that `id` is in a state where the FUSE forget
/// callback may safely drop the hot-tier buffer.
///
/// Distinct from [`Witness`] because the FUSE forget check has a
/// different shape than the per-state lifecycle checks — it asserts
/// "no open kernel handles AND no orphan-pending bytes," not "matches
/// exactly one of the four [`Lifecycle`] states." See
/// [`docs/design/mount-pending-api-contracts.md`][doc] §2.3.
///
/// # Invariants
///
/// * Constructed only by [`Pending::witness_kernel_forget`], whose
///   body IS the discharge-safety check.
/// * Lifetime + `!Send` / `!Sync` semantics identical to [`Witness`].
///
/// [doc]: ../../../docs/design/mount-pending-api-contracts.md
#[derive(Debug)]
pub(crate) struct KernelForgetWitness<'p> {
    id: u64,
    _borrow: PhantomData<&'p mut ()>,
    _not_send: PhantomData<*const ()>,
}

impl<'p> KernelForgetWitness<'p> {
    /// Mint a kernel-forget witness. Visible only inside this module.
    fn new(id: u64) -> Self {
        Self {
            id,
            _borrow: PhantomData,
            _not_send: PhantomData,
        }
    }

    /// The NodeId the witness is bound to.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
}

impl Pending {
    /// Witness that `id` is in `Live { open_count >= 1 }`. Returns
    /// `None` for any other state, including `Live { open_count == 0 }`,
    /// any `Orphan`, and `Released` (no entry). This is the
    /// constructor that closes
    /// [r11 #1][doc] — the
    /// `transition_to_orphan` retrofit will accept only
    /// [`Witness<LiveNonZero>`], so a `LiveZero` node cannot be
    /// orphaned by construction.
    ///
    /// [doc]: ../../../docs/design/mount-pending-api-contracts.md
    pub(crate) fn witness_live_nonzero(&mut self, id: u64) -> Option<Witness<'_, LiveNonZero>> {
        match self.lookup_state(id) {
            Some(NodeState::Live { open_count }) if open_count >= 1 => Some(Witness::new(id)),
            _ => None,
        }
    }

    /// Witness that `id` is in `Live { open_count == 0 }`. Returns
    /// `None` for `Live` with any non-zero refcount, for any `Orphan`,
    /// and for `Released`.
    pub(crate) fn witness_live_zero(&mut self, id: u64) -> Option<Witness<'_, LiveZero>> {
        match self.lookup_state(id) {
            Some(NodeState::Live { open_count: 0 }) => Some(Witness::new(id)),
            _ => None,
        }
    }

    /// Witness that `id` is in `Orphan { .. }` (any refcount).
    /// Returns `None` for `Live` (any refcount) and `Released`.
    pub(crate) fn witness_orphan(&mut self, id: u64) -> Option<Witness<'_, Orphan>> {
        match self.lookup_state(id) {
            Some(NodeState::Orphan { .. }) => Some(Witness::new(id)),
            _ => None,
        }
    }

    /// Witness that `id` is `Released` — i.e. has no entry in the
    /// `state` map. Returns `None` for any of the three resident
    /// variants. Useful for the "first open" path of the lifecycle
    /// (`record_open` minting a `LiveZero -> LiveNonZero` transition
    /// when there is no prior entry).
    pub(crate) fn witness_released(&mut self, id: u64) -> Option<Witness<'_, Released>> {
        match self.lookup_state(id) {
            None => Some(Witness::new(id)),
            _ => None,
        }
    }

    /// Witness that the kernel `forget` callback may safely drop
    /// `hot[id]` for this NodeId. Returns `Some` iff one of:
    ///
    /// * `Released` (no entry in the `state` map) — nothing referencing
    ///   the bytes.
    /// * `Live { open_count == 0 }` — the entry is still resolvable,
    ///   but no kernel fd holds the bytes; the forget can drop them
    ///   without racing an open handle.
    ///
    /// Returns `None` for `Live { open_count >= 1 }` (open fds still
    /// hold the bytes) and for any `Orphan` (the open-unlinked POSIX
    /// flow needs the bytes to live as long as any fd references the
    /// NodeId — closing
    /// [r11 #3][doc] under
    /// the retrofit).
    ///
    /// [doc]: ../../../docs/design/mount-pending-api-contracts.md
    pub(crate) fn witness_kernel_forget(&mut self, id: u64) -> Option<KernelForgetWitness<'_>> {
        match self.lookup_state(id) {
            None => Some(KernelForgetWitness::new(id)),
            Some(NodeState::Live { open_count: 0 }) => Some(KernelForgetWitness::new(id)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------- witness_live_nonzero --------------------------------------------

    #[test]
    fn witness_live_nonzero_some_for_live_with_open() {
        let mut p = Pending::default();
        p.test_insert_state(7, NodeState::Live { open_count: 1 });
        let w = p.witness_live_nonzero(7).expect("LiveNonZero witness");
        assert_eq!(w.id(), 7);
    }

    #[test]
    fn witness_live_nonzero_some_for_high_refcount() {
        let mut p = Pending::default();
        p.test_insert_state(11, NodeState::Live { open_count: u32::MAX });
        let w = p.witness_live_nonzero(11).expect("LiveNonZero witness");
        assert_eq!(w.id(), 11);
    }

    #[test]
    fn witness_live_nonzero_none_for_live_zero() {
        let mut p = Pending::default();
        p.test_insert_state(7, NodeState::Live { open_count: 0 });
        assert!(p.witness_live_nonzero(7).is_none());
    }

    #[test]
    fn witness_live_nonzero_none_for_orphan() {
        let mut p = Pending::default();
        p.test_insert_state(7, NodeState::Orphan { open_count: 3 });
        assert!(p.witness_live_nonzero(7).is_none());
    }

    #[test]
    fn witness_live_nonzero_none_for_released() {
        let mut p = Pending::default();
        assert!(p.witness_live_nonzero(7).is_none());
    }

    // ------- witness_live_zero ------------------------------------------------

    #[test]
    fn witness_live_zero_some_for_live_zero() {
        let mut p = Pending::default();
        p.test_insert_state(9, NodeState::Live { open_count: 0 });
        let w = p.witness_live_zero(9).expect("LiveZero witness");
        assert_eq!(w.id(), 9);
    }

    #[test]
    fn witness_live_zero_none_for_live_nonzero() {
        let mut p = Pending::default();
        p.test_insert_state(9, NodeState::Live { open_count: 2 });
        assert!(p.witness_live_zero(9).is_none());
    }

    #[test]
    fn witness_live_zero_none_for_orphan_zero() {
        // Orphan with open_count == 0 is shaped like LiveZero
        // numerically but is a fundamentally different state; the
        // witness must reject it.
        let mut p = Pending::default();
        p.test_insert_state(9, NodeState::Orphan { open_count: 0 });
        assert!(p.witness_live_zero(9).is_none());
    }

    #[test]
    fn witness_live_zero_none_for_released() {
        let mut p = Pending::default();
        assert!(p.witness_live_zero(9).is_none());
    }

    // ------- witness_orphan ---------------------------------------------------

    #[test]
    fn witness_orphan_some_for_orphan_nonzero() {
        let mut p = Pending::default();
        p.test_insert_state(13, NodeState::Orphan { open_count: 4 });
        let w = p.witness_orphan(13).expect("Orphan witness");
        assert_eq!(w.id(), 13);
    }

    #[test]
    fn witness_orphan_some_for_orphan_zero() {
        // The orphan witness must match the discriminant, not the
        // refcount — `Orphan { open_count == 0 }` is a real state
        // (just one the FSM transitions out of immediately on
        // release).
        let mut p = Pending::default();
        p.test_insert_state(13, NodeState::Orphan { open_count: 0 });
        let w = p.witness_orphan(13).expect("Orphan witness");
        assert_eq!(w.id(), 13);
    }

    #[test]
    fn witness_orphan_none_for_live_nonzero() {
        let mut p = Pending::default();
        p.test_insert_state(13, NodeState::Live { open_count: 1 });
        assert!(p.witness_orphan(13).is_none());
    }

    #[test]
    fn witness_orphan_none_for_live_zero() {
        let mut p = Pending::default();
        p.test_insert_state(13, NodeState::Live { open_count: 0 });
        assert!(p.witness_orphan(13).is_none());
    }

    #[test]
    fn witness_orphan_none_for_released() {
        let mut p = Pending::default();
        assert!(p.witness_orphan(13).is_none());
    }

    // ------- witness_released -------------------------------------------------

    #[test]
    fn witness_released_some_for_absent_entry() {
        let mut p = Pending::default();
        let w = p.witness_released(21).expect("Released witness");
        assert_eq!(w.id(), 21);
    }

    #[test]
    fn witness_released_none_for_live_nonzero() {
        let mut p = Pending::default();
        p.test_insert_state(21, NodeState::Live { open_count: 1 });
        assert!(p.witness_released(21).is_none());
    }

    #[test]
    fn witness_released_none_for_live_zero() {
        let mut p = Pending::default();
        p.test_insert_state(21, NodeState::Live { open_count: 0 });
        assert!(p.witness_released(21).is_none());
    }

    #[test]
    fn witness_released_none_for_orphan() {
        let mut p = Pending::default();
        p.test_insert_state(21, NodeState::Orphan { open_count: 2 });
        assert!(p.witness_released(21).is_none());
    }

    // ------- witness_kernel_forget --------------------------------------------

    #[test]
    fn witness_kernel_forget_some_for_released() {
        let mut p = Pending::default();
        let w = p.witness_kernel_forget(31).expect("KernelForget witness");
        assert_eq!(w.id(), 31);
    }

    #[test]
    fn witness_kernel_forget_some_for_live_zero() {
        // The discharge-safety condition: a Live entry with no open
        // kernel fds. The forget cannot race an open handle here.
        let mut p = Pending::default();
        p.test_insert_state(31, NodeState::Live { open_count: 0 });
        let w = p.witness_kernel_forget(31).expect("KernelForget witness");
        assert_eq!(w.id(), 31);
    }

    #[test]
    fn witness_kernel_forget_none_for_live_nonzero() {
        // The r11 #3 race: open fds still hold the bytes; dropping
        // hot[id] here loses data. The constructor must reject.
        let mut p = Pending::default();
        p.test_insert_state(31, NodeState::Live { open_count: 1 });
        assert!(p.witness_kernel_forget(31).is_none());
    }

    #[test]
    fn witness_kernel_forget_none_for_orphan_zero() {
        // Even with zero open count, Orphan presence signals
        // "directory entry gone, bytes outlive it" — the forget
        // cannot discharge the bytes without risking a racing open
        // handle the FSM has not yet observed.
        let mut p = Pending::default();
        p.test_insert_state(31, NodeState::Orphan { open_count: 0 });
        assert!(p.witness_kernel_forget(31).is_none());
    }

    #[test]
    fn witness_kernel_forget_none_for_orphan_nonzero() {
        let mut p = Pending::default();
        p.test_insert_state(31, NodeState::Orphan { open_count: 5 });
        assert!(p.witness_kernel_forget(31).is_none());
    }

    // ------- substrate hygiene -----------------------------------------------

    /// Compile-time `!Send` / `!Sync` assertion. Stable Rust has no
    /// negative trait bound; the pattern below (from the
    /// `static_assertions` crate, inlined to avoid a new dep) creates
    /// two blanket impls of a helper trait — one unconditional, one
    /// gated on `Send` (or `Sync`). Inference then resolves
    /// unambiguously iff the target type is `!Send` (resp. `!Sync`).
    /// If a future drive-by removes the `PhantomData<*const ()>`
    /// field on either witness, the type becomes `Send` and this
    /// `const _` block fails to compile with "type annotations
    /// needed."
    const _ASSERT_NOT_SEND: () = {
        trait AmbiguousIfSend<A> {
            fn some() {}
        }
        impl<T: ?Sized> AmbiguousIfSend<()> for T {}
        #[allow(dead_code)]
        struct Invalid;
        impl<T: ?Sized + Send> AmbiguousIfSend<Invalid> for T {}

        let _ = <Witness<'static, LiveNonZero> as AmbiguousIfSend<_>>::some;
        let _ = <Witness<'static, LiveZero> as AmbiguousIfSend<_>>::some;
        let _ = <Witness<'static, Orphan> as AmbiguousIfSend<_>>::some;
        let _ = <Witness<'static, Released> as AmbiguousIfSend<_>>::some;
        let _ = <KernelForgetWitness<'static> as AmbiguousIfSend<_>>::some;
    };

    const _ASSERT_NOT_SYNC: () = {
        trait AmbiguousIfSync<A> {
            fn some() {}
        }
        impl<T: ?Sized> AmbiguousIfSync<()> for T {}
        #[allow(dead_code)]
        struct Invalid;
        impl<T: ?Sized + Sync> AmbiguousIfSync<Invalid> for T {}

        let _ = <Witness<'static, LiveNonZero> as AmbiguousIfSync<_>>::some;
        let _ = <Witness<'static, LiveZero> as AmbiguousIfSync<_>>::some;
        let _ = <Witness<'static, Orphan> as AmbiguousIfSync<_>>::some;
        let _ = <Witness<'static, Released> as AmbiguousIfSync<_>>::some;
        let _ = <KernelForgetWitness<'static> as AmbiguousIfSync<_>>::some;
    };
}
