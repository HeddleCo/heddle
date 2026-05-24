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
//!   for the `BrandedPending::witness_*` constructors that need it.
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
#[doc(hidden)]
pub trait Lifecycle: sealed::Sealed {}

/// Marker: `Live { open_count >= 1 }`.
///
/// At least one kernel fd holds this NodeId. The only state from
/// which `transition_to_orphan` is sound (closing the r11 #1 bug
/// once the retrofit lands).
#[derive(Debug, Clone, Copy)]
#[doc(hidden)]
pub struct LiveNonZero;

/// Marker: `Live { open_count == 0 }`.
///
/// The inode is still resolvable but has no open kernel fds. Distinct
/// from [`LiveNonZero`] so a method that requires the "has open fds"
/// invariant can name it at the type level.
#[derive(Debug, Clone, Copy)]
#[doc(hidden)]
pub struct LiveZero;

/// Marker: `Orphan { open_count >= 0 }`.
///
/// Directory entry gone; bytes outlive it. The reverse of
/// [`LiveNonZero`] in the transition diagram — the destination of
/// `transition_to_orphan`, the only state from which the open-unlinked
/// last-close-wins flow runs.
#[derive(Debug, Clone, Copy)]
#[doc(hidden)]
pub struct Orphan;

/// Marker: entry absent from the `state` map.
///
/// `Released` is the lifecycle's "no entry here" state — distinct
/// from [`LiveZero`] (which has an entry with `open_count == 0`).
/// Never stored in the map; the marker exists purely so a
/// [`Witness<'_, Released>`] can prove the absence at the call site.
#[derive(Debug, Clone, Copy)]
#[doc(hidden)]
pub struct Released;

impl sealed::Sealed for LiveNonZero {}
impl sealed::Sealed for LiveZero {}
impl sealed::Sealed for Orphan {}
impl sealed::Sealed for Released {}

impl Lifecycle for LiveNonZero {}
impl Lifecycle for LiveZero {}
impl Lifecycle for Orphan {}
impl Lifecycle for Released {}

/// Type-level proof that `id` was in lifecycle state `S` under the
/// `&mut Pending<'brand>` borrow `'p` that minted it.
///
/// # Invariants
///
/// * Constructed only by [`BrandedPending::witness_live_nonzero`] /
///   [`BrandedPending::witness_live_zero`] /
///   [`BrandedPending::witness_orphan`] /
///   [`BrandedPending::witness_released`], each of which performs the
///   matching FSM check and returns `Some` iff it holds. Those
///   constructors live on [`BrandedPending`] (not [`Pending`]), and
///   [`BrandedPending`] is only constructible inside
///   [`Pending::with_brand`] — so every witness in the system
///   originates from a `with_brand` invocation by construction
///   (Codex PR #217 r2 finding `3293898540`).
/// * The lifetime parameter `'p` is the lifetime of the
///   `&mut BrandedPending` borrow that produced the witness. The
///   borrow checker therefore forbids any other code from mutating
///   the underlying `Pending` while the witness exists — closing the
///   "stale witness across a mutation" hole spelled out in
///   [`docs/design/mount-pending-api-contracts.md`][doc] §2.2.1.
/// * The lifetime parameter `'brand` ties the witness to the specific
///   [`BrandedPending`] instance that minted it. Two
///   `BrandedPending<'_, 'brand>` values handed out by separate
///   [`Pending::with_brand`] calls carry distinct, invariant brands;
///   a witness from one cannot be passed to methods on the other
///   (closes Codex PR #217 r2 finding `3293832936`).
/// * `S` is invariant via the [`PhantomData<&'p mut ()>`] field — the
///   compiler will not silently widen or narrow the state parameter.
/// * `!Send` and `!Sync` via the raw-pointer marker — the witness is
///   a single-thread, single-borrow token by design.
///
/// [doc]: ../../../docs/design/mount-pending-api-contracts.md
#[derive(Debug)]
#[doc(hidden)]
pub struct Witness<'p, 'brand, S: Lifecycle> {
    id: u64,
    _state: PhantomData<S>,
    // Invariance over `'p` + ties the witness to a
    // `&mut BrandedPending` borrow. The `&'p mut ()` is for the
    // lifetime relationship; the borrow extension on
    // `&'p mut BrandedPending -> Witness<'p, _, _>` is what makes
    // the borrow checker refuse a concurrent mutable borrow of the
    // wrapper (and hence of the underlying `Pending`) while the
    // witness is alive.
    _borrow: PhantomData<&'p mut ()>,
    // Invariant brand binding the witness to its originating
    // `BrandedPending<'_, 'brand>` instance.
    // `fn(&'brand ()) -> &'brand ()` is the canonical invariant
    // carrier — neither covariant nor contravariant. Witnesses
    // minted under one `with_brand` HRTB closure cannot be passed
    // to a `BrandedPending` with a different brand, even when both
    // carry the same lifecycle state `S`.
    _brand: PhantomData<fn(&'brand ()) -> &'brand ()>,
    // `!Send` + `!Sync` marker. The witness is short-lived and tied
    // to one thread of execution by design; cross-thread transfer is
    // never sound.
    _not_send: PhantomData<*const ()>,
}

impl<'p, 'brand, S: Lifecycle> Witness<'p, 'brand, S> {
    /// Mint a witness. Visible only inside this module — every
    /// witness must come from a `BrandedPending::witness_*`
    /// constructor that performed the FSM check.
    fn new(id: u64) -> Self {
        Self {
            id,
            _state: PhantomData,
            _borrow: PhantomData,
            _brand: PhantomData,
            _not_send: PhantomData,
        }
    }

    /// The NodeId the witness is bound to. Retrofitted method bodies
    /// will read this to know which entry of `Pending` to act on.
    #[doc(hidden)]
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl<'a> Pending<'a> {
    /// Re-borrow `self` under a freshly-minted, invariant `'brand`
    /// lifetime that's unique to this call, and hand the resulting
    /// [`BrandedPending`] to `f`.
    ///
    /// `with_brand` is the **only** way to obtain a [`BrandedPending`]
    /// — and [`BrandedPending`] is the **only** type that exposes the
    /// `witness_*` constructors. Internal callers that hold a
    /// `&mut Pending<'static>` (e.g. through `MountInner::pending`)
    /// therefore cannot mint a witness without first going through
    /// `with_brand`, which forces the brand to be a fresh HRTB-bound
    /// existential rather than the storage's `'static` slot. This
    /// closes Codex PR #217 r2 follow-up `3293898540`.
    ///
    /// The HRTB on `f` (`for<'brand>`) forces the compiler to treat
    /// the brand as opaque inside the closure. Each call to
    /// `with_brand` issues a brand that can't unify with any other
    /// call's brand, even if both originate from the same physical
    /// `Pending`. Witnesses minted on the inner
    /// [`BrandedPending<'_, 'brand>`] carry that closure's `'brand`
    /// and cannot be passed to a [`BrandedPending`] borrowed under a
    /// different `with_brand` invocation — closing Codex PR #217 r2
    /// finding `3293832936`.
    ///
    /// # Soundness
    ///
    /// The transmute changes only the phantom brand lifetime, which
    /// has no representation in the type's layout (`PhantomData<fn(..)>`
    /// is zero-sized). All other lifetime + ownership invariants
    /// transfer verbatim. The HRTB bound on `f` prevents the inner
    /// brand from escaping the closure: any value returned by `f`
    /// cannot mention `'brand` because there's no fixed `'brand` to
    /// mention from outside.
    #[doc(hidden)]
    pub fn with_brand<R>(
        &mut self,
        f: impl for<'brand> FnOnce(&mut BrandedPending<'_, 'brand>) -> R,
    ) -> R {
        // SAFETY: `Pending<'a>` and `Pending<'brand>` have identical
        // layout — `_brand` is `PhantomData<fn(&_ ()) -> &_ ()>`,
        // which is zero-sized regardless of the brand lifetime, so
        // only the phantom-only lifetime parameter changes. The
        // HRTB on `f` ensures the freshly-introduced `'brand`
        // cannot leak via the return type.
        let rebranded: &mut Pending<'_> =
            unsafe { std::mem::transmute::<&mut Pending<'a>, &mut Pending<'_>>(self) };
        let mut bp = BrandedPending { inner: rebranded };
        f(&mut bp)
    }
}

/// Sealed wrapper that grants access to the witness constructors.
///
/// `BrandedPending` is the only type with `witness_*` (and
/// [`BrandedPending::peek_witness`]) methods. Its single field is
/// private to this module, so the wrapper can only be constructed
/// from inside [`Pending::with_brand`]; the HRTB on `with_brand`'s
/// closure then forces `'brand` to be a fresh existential per call.
///
/// Together those two properties enforce: every witness in the
/// system originates from a `with_brand` invocation, and every
/// witness's `'brand` is unique to that invocation. The
/// `MountInner`-stores-`Pending<'static>` escape that Codex flagged
/// in `3293898540` is closed by construction — internal code with
/// `&mut Pending<'static>` literally cannot name a `witness_*`
/// method on it.
#[doc(hidden)]
pub struct BrandedPending<'p, 'brand> {
    // Private — outside this module, no one can build a
    // `BrandedPending`. Inside this module, only `Pending::with_brand`
    // does. That single chokepoint is what enforces brand gating.
    inner: &'p mut Pending<'brand>,
}

impl<'p, 'brand> BrandedPending<'p, 'brand> {
    /// Substrate hook for the brand-isolation doctest: takes a
    /// borrowed witness branded to this [`BrandedPending`]'s `'brand`
    /// and returns its NodeId without doing anything else.
    ///
    /// Retrofit issues (heddle#209/#210/#211/#212) will introduce
    /// the real witness-gated transitions, which need to mutate
    /// `self` and consume the witness; those signatures live there,
    /// not here. For the substrate PR we only need a non-consuming
    /// brand-matching method so the
    /// [`crate::__pending_substrate_for_doctest`] `compile_fail`
    /// proof can demonstrate that a witness minted under a different
    /// `with_brand` closure fails to type-check (the brand lifetimes
    /// are invariant and don't unify).
    ///
    /// `&self` (not `&mut self`) plus `&Witness` (not `Witness`)
    /// keeps the proof orthogonal to the separate borrow-checker
    /// constraint introduced by the witness's `_borrow:
    /// PhantomData<&'p mut ()>` field — the consume pattern
    /// `p.transition_to_orphan(w)` that retrofit issues will need
    /// is a pre-existing substrate design question (the `'p` field
    /// makes it not compile today; see the spike doc §2.2.1) and
    /// isn't this PR's scope.
    #[doc(hidden)]
    pub fn peek_witness<S: Lifecycle>(&self, w: &Witness<'_, 'brand, S>) -> u64 {
        let _ = self;
        w.id()
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
/// * Constructed only by [`BrandedPending::witness_kernel_forget`],
///   whose body IS the discharge-safety check. Same brand-gating
///   logic as [`Witness`]: the constructor lives on
///   [`BrandedPending`], which is only constructible inside
///   [`Pending::with_brand`].
/// * Lifetime + `!Send` / `!Sync` semantics identical to [`Witness`].
///
/// [doc]: ../../../docs/design/mount-pending-api-contracts.md
#[derive(Debug)]
#[doc(hidden)]
pub struct KernelForgetWitness<'p, 'brand> {
    id: u64,
    _borrow: PhantomData<&'p mut ()>,
    // Same invariant brand carrier as [`Witness`]. Binds the witness
    // to the originating `BrandedPending<'_, 'brand>` instance so a
    // forget witness from one mount cannot be discharged against
    // another's hot-tier buffer (Codex PR #217 r2 finding `3293832936`).
    _brand: PhantomData<fn(&'brand ()) -> &'brand ()>,
    _not_send: PhantomData<*const ()>,
}

impl<'p, 'brand> KernelForgetWitness<'p, 'brand> {
    /// Mint a kernel-forget witness. Visible only inside this module.
    fn new(id: u64) -> Self {
        Self {
            id,
            _borrow: PhantomData,
            _brand: PhantomData,
            _not_send: PhantomData,
        }
    }

    /// The NodeId the witness is bound to.
    #[doc(hidden)]
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl<'p, 'brand> BrandedPending<'p, 'brand> {
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
    #[doc(hidden)]
    pub fn witness_live_nonzero(&mut self, id: u64) -> Option<Witness<'_, 'brand, LiveNonZero>> {
        match self.inner.lookup_state(id) {
            Some(NodeState::Live { open_count }) if open_count >= 1 => Some(Witness::new(id)),
            _ => None,
        }
    }

    /// Witness that `id` is in `Live { open_count == 0 }`. Returns
    /// `None` for `Live` with any non-zero refcount, for any `Orphan`,
    /// and for `Released`.
    #[doc(hidden)]
    pub fn witness_live_zero(&mut self, id: u64) -> Option<Witness<'_, 'brand, LiveZero>> {
        match self.inner.lookup_state(id) {
            Some(NodeState::Live { open_count: 0 }) => Some(Witness::new(id)),
            _ => None,
        }
    }

    /// Witness that `id` is in `Orphan { .. }` (any refcount).
    /// Returns `None` for `Live` (any refcount) and `Released`.
    #[doc(hidden)]
    pub fn witness_orphan(&mut self, id: u64) -> Option<Witness<'_, 'brand, Orphan>> {
        match self.inner.lookup_state(id) {
            Some(NodeState::Orphan { .. }) => Some(Witness::new(id)),
            _ => None,
        }
    }

    /// Witness that `id` is `Released` — i.e. has no entry in the
    /// `state` map. Returns `None` for any of the three resident
    /// variants. Useful for the "first open" path of the lifecycle
    /// (`record_open` minting a `LiveZero -> LiveNonZero` transition
    /// when there is no prior entry).
    #[doc(hidden)]
    pub fn witness_released(&mut self, id: u64) -> Option<Witness<'_, 'brand, Released>> {
        match self.inner.lookup_state(id) {
            None => Some(Witness::new(id)),
            _ => None,
        }
    }

    /// Witness-gated FSM transition: `LiveNonZero` → `Orphan`,
    /// carrying the current `open_count` over to the orphan record.
    /// Returns `Some(Witness<Orphan>)` iff `id` was in
    /// `Live { open_count >= 1 }` at the moment of the call; returns
    /// `None` (without touching `state`) for any other lifecycle
    /// state, including `Live { open_count == 0 }`, any `Orphan`, and
    /// `Released` (no entry).
    ///
    /// # Why `id` and not a [`Witness<LiveNonZero>`] input?
    ///
    /// The spike doc ([§2.2.1][doc]) sketched the API as
    /// `transition_to_orphan(&mut self, w: Witness<LiveNonZero>) -> Witness<Orphan>`,
    /// with the caller pre-minting `w` via
    /// [`Self::witness_live_nonzero`] and threading it in. The
    /// heddle#208 r3 self-audit flagged — and an attempted retrofit at
    /// this issue confirmed — that the literal shape does not compile
    /// against the substrate: [`Witness`]'s
    /// `_borrow: PhantomData<&'p mut ()>` field is invariant over
    /// `'p`, so the prior `&mut BrandedPending` reborrow that minted
    /// `w` cannot shrink to admit the second `&mut self` call that
    /// would consume `w` (rustc `E0499`). The fix the brief proposed
    /// in [Option B][option-b] — relax `'p`'s variance — would weaken
    /// the substrate's stale-witness protection and is out of scope
    /// for this retrofit.
    ///
    /// Folding the FSM check and the mutation into one call preserves
    /// the spike doc's invariant — *the transition can only fire on
    /// a [`LiveNonZero`] state* — by construction:
    ///
    /// * The body's `match` IS the
    ///   [`Self::witness_live_nonzero`] FSM check; both refer to the
    ///   same `Live { open_count >= 1 }` discriminant.
    /// * Any non-`LiveNonZero` state path returns `None` before
    ///   touching [`crate::core::Pending::apply_transition_to_orphan`].
    ///   Callers in `core.rs` propagate the `None` with `if let
    ///   Some(_) = bp.transition_to_orphan(id) { … }` — the missing
    ///   witness IS the short-circuit at the call site.
    /// * The returned `Witness<Orphan>` is the only path to evidence
    ///   that the transition fired; [`Witness::new`] is
    ///   module-private to this file and the
    ///   [`crate::core::Pending::apply_transition_to_orphan`]
    ///   accessor takes `&Witness<Orphan>` as proof — together they
    ///   keep the discipline that no code outside this module's
    ///   `BrandedPending` impl can synthesize an orphan witness or
    ///   record a transition.
    ///
    /// Closes [r11 #1][doc] (`transition_to_orphan` records
    /// `Orphan { open_count: 0 }` for nodes with no live fds — now
    /// impossible because the `Live { open_count: 0 }` branch returns
    /// `None`) and [r11 #4][doc] (`rename_entry_with_options` calls
    /// the transition for symlinks/dirs that have no `open`/`release`
    /// lifecycle — symlinks never enter `state`, so the lookup
    /// returns `None` and the transition never fires).
    ///
    /// [doc]: ../../../docs/design/mount-pending-api-contracts.md
    /// [option-b]: ../../../docs/design/mount-pending-api-contracts.md
    #[doc(hidden)]
    pub(crate) fn transition_to_orphan<'a>(
        &'a mut self,
        id: u64,
    ) -> Option<Witness<'a, 'brand, Orphan>> {
        match self.inner.lookup_state(id) {
            Some(NodeState::Live { open_count }) if open_count >= 1 => {
                let w = Witness::<'a, 'brand, Orphan>::new(id);
                self.inner.apply_transition_to_orphan(&w);
                Some(w)
            }
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
    #[doc(hidden)]
    pub fn witness_kernel_forget(&mut self, id: u64) -> Option<KernelForgetWitness<'_, 'brand>> {
        match self.inner.lookup_state(id) {
            None => Some(KernelForgetWitness::new(id)),
            Some(NodeState::Live { open_count: 0 }) => Some(KernelForgetWitness::new(id)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The witnesses can't escape `with_brand`'s HRTB closure (they
    // carry the fresh `'brand`), but the brand-free `Option<u64>`
    // shape can. Every test below extracts `.id()` inside the
    // closure and asserts against the unbranded `Option<u64>` outside;
    // negative tests use `.is_some()` for the same reason. This
    // mirrors the canonical ghost-cell / branded-vec test style.

    // ------- witness_live_nonzero --------------------------------------------

    #[test]
    fn witness_live_nonzero_some_for_live_with_open() {
        let mut p = Pending::default();
        p.test_insert_state(7, NodeState::Live { open_count: 1 });
        let id = p
            .with_brand(|bp| bp.witness_live_nonzero(7).map(|w| w.id()))
            .expect("LiveNonZero witness");
        assert_eq!(id, 7);
    }

    #[test]
    fn witness_live_nonzero_some_for_high_refcount() {
        let mut p = Pending::default();
        p.test_insert_state(11, NodeState::Live { open_count: u32::MAX });
        let id = p
            .with_brand(|bp| bp.witness_live_nonzero(11).map(|w| w.id()))
            .expect("LiveNonZero witness");
        assert_eq!(id, 11);
    }

    #[test]
    fn witness_live_nonzero_none_for_live_zero() {
        let mut p = Pending::default();
        p.test_insert_state(7, NodeState::Live { open_count: 0 });
        assert!(!p.with_brand(|bp| bp.witness_live_nonzero(7).is_some()));
    }

    #[test]
    fn witness_live_nonzero_none_for_orphan() {
        let mut p = Pending::default();
        p.test_insert_state(7, NodeState::Orphan { open_count: 3 });
        assert!(!p.with_brand(|bp| bp.witness_live_nonzero(7).is_some()));
    }

    #[test]
    fn witness_live_nonzero_none_for_released() {
        let mut p = Pending::default();
        assert!(!p.with_brand(|bp| bp.witness_live_nonzero(7).is_some()));
    }

    // ------- witness_live_zero ------------------------------------------------

    #[test]
    fn witness_live_zero_some_for_live_zero() {
        let mut p = Pending::default();
        p.test_insert_state(9, NodeState::Live { open_count: 0 });
        let id = p
            .with_brand(|bp| bp.witness_live_zero(9).map(|w| w.id()))
            .expect("LiveZero witness");
        assert_eq!(id, 9);
    }

    #[test]
    fn witness_live_zero_none_for_live_nonzero() {
        let mut p = Pending::default();
        p.test_insert_state(9, NodeState::Live { open_count: 2 });
        assert!(!p.with_brand(|bp| bp.witness_live_zero(9).is_some()));
    }

    #[test]
    fn witness_live_zero_none_for_orphan_zero() {
        // Orphan with open_count == 0 is shaped like LiveZero
        // numerically but is a fundamentally different state; the
        // witness must reject it.
        let mut p = Pending::default();
        p.test_insert_state(9, NodeState::Orphan { open_count: 0 });
        assert!(!p.with_brand(|bp| bp.witness_live_zero(9).is_some()));
    }

    #[test]
    fn witness_live_zero_none_for_released() {
        let mut p = Pending::default();
        assert!(!p.with_brand(|bp| bp.witness_live_zero(9).is_some()));
    }

    // ------- witness_orphan ---------------------------------------------------

    #[test]
    fn witness_orphan_some_for_orphan_nonzero() {
        let mut p = Pending::default();
        p.test_insert_state(13, NodeState::Orphan { open_count: 4 });
        let id = p
            .with_brand(|bp| bp.witness_orphan(13).map(|w| w.id()))
            .expect("Orphan witness");
        assert_eq!(id, 13);
    }

    #[test]
    fn witness_orphan_some_for_orphan_zero() {
        // The orphan witness must match the discriminant, not the
        // refcount — `Orphan { open_count == 0 }` is a real state
        // (just one the FSM transitions out of immediately on
        // release).
        let mut p = Pending::default();
        p.test_insert_state(13, NodeState::Orphan { open_count: 0 });
        let id = p
            .with_brand(|bp| bp.witness_orphan(13).map(|w| w.id()))
            .expect("Orphan witness");
        assert_eq!(id, 13);
    }

    #[test]
    fn witness_orphan_none_for_live_nonzero() {
        let mut p = Pending::default();
        p.test_insert_state(13, NodeState::Live { open_count: 1 });
        assert!(!p.with_brand(|bp| bp.witness_orphan(13).is_some()));
    }

    #[test]
    fn witness_orphan_none_for_live_zero() {
        let mut p = Pending::default();
        p.test_insert_state(13, NodeState::Live { open_count: 0 });
        assert!(!p.with_brand(|bp| bp.witness_orphan(13).is_some()));
    }

    #[test]
    fn witness_orphan_none_for_released() {
        let mut p = Pending::default();
        assert!(!p.with_brand(|bp| bp.witness_orphan(13).is_some()));
    }

    // ------- witness_released -------------------------------------------------

    #[test]
    fn witness_released_some_for_absent_entry() {
        let mut p = Pending::default();
        let id = p
            .with_brand(|bp| bp.witness_released(21).map(|w| w.id()))
            .expect("Released witness");
        assert_eq!(id, 21);
    }

    #[test]
    fn witness_released_none_for_live_nonzero() {
        let mut p = Pending::default();
        p.test_insert_state(21, NodeState::Live { open_count: 1 });
        assert!(!p.with_brand(|bp| bp.witness_released(21).is_some()));
    }

    #[test]
    fn witness_released_none_for_live_zero() {
        let mut p = Pending::default();
        p.test_insert_state(21, NodeState::Live { open_count: 0 });
        assert!(!p.with_brand(|bp| bp.witness_released(21).is_some()));
    }

    #[test]
    fn witness_released_none_for_orphan() {
        let mut p = Pending::default();
        p.test_insert_state(21, NodeState::Orphan { open_count: 2 });
        assert!(!p.with_brand(|bp| bp.witness_released(21).is_some()));
    }

    // ------- witness_kernel_forget --------------------------------------------

    #[test]
    fn witness_kernel_forget_some_for_released() {
        let mut p = Pending::default();
        let id = p
            .with_brand(|bp| bp.witness_kernel_forget(31).map(|w| w.id()))
            .expect("KernelForget witness");
        assert_eq!(id, 31);
    }

    #[test]
    fn witness_kernel_forget_some_for_live_zero() {
        // The discharge-safety condition: a Live entry with no open
        // kernel fds. The forget cannot race an open handle here.
        let mut p = Pending::default();
        p.test_insert_state(31, NodeState::Live { open_count: 0 });
        let id = p
            .with_brand(|bp| bp.witness_kernel_forget(31).map(|w| w.id()))
            .expect("KernelForget witness");
        assert_eq!(id, 31);
    }

    #[test]
    fn witness_kernel_forget_none_for_live_nonzero() {
        // The r11 #3 race: open fds still hold the bytes; dropping
        // hot[id] here loses data. The constructor must reject.
        let mut p = Pending::default();
        p.test_insert_state(31, NodeState::Live { open_count: 1 });
        assert!(!p.with_brand(|bp| bp.witness_kernel_forget(31).is_some()));
    }

    #[test]
    fn witness_kernel_forget_none_for_orphan_zero() {
        // Even with zero open count, Orphan presence signals
        // "directory entry gone, bytes outlive it" — the forget
        // cannot discharge the bytes without risking a racing open
        // handle the FSM has not yet observed.
        let mut p = Pending::default();
        p.test_insert_state(31, NodeState::Orphan { open_count: 0 });
        assert!(!p.with_brand(|bp| bp.witness_kernel_forget(31).is_some()));
    }

    #[test]
    fn witness_kernel_forget_none_for_orphan_nonzero() {
        let mut p = Pending::default();
        p.test_insert_state(31, NodeState::Orphan { open_count: 5 });
        assert!(!p.with_brand(|bp| bp.witness_kernel_forget(31).is_some()));
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

        let _ = <Witness<'static, 'static, LiveNonZero> as AmbiguousIfSend<_>>::some;
        let _ = <Witness<'static, 'static, LiveZero> as AmbiguousIfSend<_>>::some;
        let _ = <Witness<'static, 'static, Orphan> as AmbiguousIfSend<_>>::some;
        let _ = <Witness<'static, 'static, Released> as AmbiguousIfSend<_>>::some;
        let _ = <KernelForgetWitness<'static, 'static> as AmbiguousIfSend<_>>::some;
    };

    const _ASSERT_NOT_SYNC: () = {
        trait AmbiguousIfSync<A> {
            fn some() {}
        }
        impl<T: ?Sized> AmbiguousIfSync<()> for T {}
        #[allow(dead_code)]
        struct Invalid;
        impl<T: ?Sized + Sync> AmbiguousIfSync<Invalid> for T {}

        let _ = <Witness<'static, 'static, LiveNonZero> as AmbiguousIfSync<_>>::some;
        let _ = <Witness<'static, 'static, LiveZero> as AmbiguousIfSync<_>>::some;
        let _ = <Witness<'static, 'static, Orphan> as AmbiguousIfSync<_>>::some;
        let _ = <Witness<'static, 'static, Released> as AmbiguousIfSync<_>>::some;
        let _ = <KernelForgetWitness<'static, 'static> as AmbiguousIfSync<_>>::some;
    };

    // ------- brand isolation (Codex PR #217 r2) ------------------------------

    /// `with_brand` is the only way to mint a brand-bound witness;
    /// each invocation introduces a freshly-typed, invariant
    /// `'brand` lifetime via HRTB. Verify it threads through
    /// witness construction at the type level — and that
    /// brand-free data (a `u64` NodeId) can escape the closure
    /// while a `Witness<'_, 'brand, S>` (carrying `'brand`) cannot.
    /// The cross-instance brand-rejection invariant itself is
    /// pinned by the `compile_fail` doctest at
    /// `crates/mount/src/lib.rs::__pending_substrate_for_doctest`.
    #[test]
    fn with_brand_threads_a_fresh_brand_through_witness_construction() {
        let mut p = Pending::default();
        p.test_insert_state(7, NodeState::Live { open_count: 1 });
        let id = p
            .with_brand(|bp| {
                // `w` is `Witness<'_, 'brand, LiveNonZero>` for the
                // closure's fresh `'brand`. We extract the brand-free
                // NodeId and drop `w` — the witness itself cannot
                // escape because it carries `'brand`.
                bp.witness_live_nonzero(7).map(|w| w.id())
            })
            .expect("LiveNonZero witness");
        assert_eq!(id, 7);
    }

    /// Two `Pending` values, each accessed under its own
    /// `with_brand` closure, mint witnesses with brands that don't
    /// unify. Verify the positive side — each closure round-trips
    /// its own NodeIds independently — so a future regression that
    /// accidentally collapses the brand into a shared one (e.g., by
    /// removing the `PhantomData<fn(&'brand ()) -> &'brand ()>`
    /// invariance marker) is visible in the lib test output rather
    /// than only in the doctest.
    #[test]
    fn brand_round_trips_independently_across_two_pendings() {
        let mut p1 = Pending::default();
        let mut p2 = Pending::default();
        p1.test_insert_state(11, NodeState::Live { open_count: 2 });
        p2.test_insert_state(13, NodeState::Live { open_count: 0 });

        let id1 = p1
            .with_brand(|bp| bp.witness_live_nonzero(11).map(|w| w.id()))
            .expect("p1 LiveNonZero witness");
        let id2 = p2
            .with_brand(|bp| bp.witness_live_zero(13).map(|w| w.id()))
            .expect("p2 LiveZero witness");

        assert_eq!(id1, 11);
        assert_eq!(id2, 13);
    }

    /// `peek_witness` is the cross-instance brand-check entry
    /// point used by the `compile_fail` doctest in
    /// [`crate::__pending_substrate_for_doctest`]. Cross-instance
    /// is the *only* shape the substrate proves compile-rejects
    /// here; the same-instance consume pattern
    /// (`p.transition_to_orphan(w)`) that retrofit issues
    /// (heddle#209-#212) want is a separate spike-doc question
    /// — the witness's `_borrow: PhantomData<&'p mut ()>` field
    /// already prevents it independent of brand. Verify the
    /// cross-instance positive path: `peek_witness` on a *third*
    /// `Pending` (`p_observer`) with a witness from `p_subject`
    /// type-checks when both share the same `with_brand`-induced
    /// brand (they don't here, so it can't — the negative shape is
    /// in the doctest). Use a witness already-discharged via
    /// `w.id()` to keep the test focused on `with_brand` ergonomics
    /// rather than the borrow-pattern.
    #[test]
    fn with_brand_can_return_unbranded_node_id() {
        let mut p = Pending::default();
        p.test_insert_state(17, NodeState::Orphan { open_count: 3 });
        let extracted: u64 = p.with_brand(|bp| {
            // `u64` is brand-free, so it escapes the closure.
            // A `Witness<'_, 'brand, Orphan>` could not.
            bp.witness_orphan(17).map(|w| w.id()).unwrap_or(0)
        });
        assert_eq!(extracted, 17);
    }
}
