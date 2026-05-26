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

/// Runtime discriminator for the three *resident* [`Lifecycle`]
/// markers — the ones that can ever appear in [`crate::core::Pending`]'s
/// `state` map. [`Released`] is the absence-of-entry state and is
/// never returned: the classifier runs only after a successful
/// [`crate::core::Pending::lookup_state`] (the `Option<NodeState>`
/// has already been unwrapped to a `NodeState`), so a `Released` arm
/// would be unreachable. The doc-comment on
/// [`Pending::drain_for_capture`] documents why the match has no
/// fourth arm.
///
/// The variants intentionally mirror the substrate's [`LiveNonZero`]
/// / [`LiveZero`] / [`Orphan`] type-state ZSTs at the value layer.
/// Code that wants to drop `LiveNonZero` (the r11 #2 bug) cannot
/// collapse the discriminant by accident: the variant must be named
/// explicitly in any match against `ResidentLifecycle`, which makes
/// the bug unwritable without an obvious diff
/// ([`docs/design/mount-pending-api-contracts.md`][doc] §3 row 2).
///
/// [doc]: ../../../docs/design/mount-pending-api-contracts.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub(crate) enum ResidentLifecycle {
    /// `Live { open_count >= 1 }`. POSIX last-close-wins applies — at
    /// least one kernel fd holds the NodeId, so the lifecycle row and
    /// per-NodeId byte storage must outlive any path-level operation,
    /// including capture, until the final close.
    LiveNonZero,
    /// `Live { open_count == 0 }`. The path is still resolvable but
    /// no fd references the inode; the per-NodeId byte storage can be
    /// retired alongside the lifecycle row on capture without
    /// breaking POSIX.
    LiveZero,
    /// `Orphan { open_count >= 0 }`. Directory entry already retired
    /// (post-unlink / post-rename-over T1/T3); the per-NodeId bytes
    /// must outlive the entry as long as any fd holds the NodeId.
    /// Refcount-irrelevant — the discriminant is what gates the
    /// drain decision, mirroring the substrate's [`BrandedPending::witness_orphan`]
    /// constructor.
    Orphan,
}

impl ResidentLifecycle {
    /// Project a runtime [`NodeState`] onto its [`ResidentLifecycle`]
    /// discriminator. The match is exhaustive over [`NodeState`]'s
    /// two variants (`Live` / `Orphan`) by language rule, and
    /// exhaustive over the three resident [`Lifecycle`] markers by
    /// codomain.
    pub(crate) fn classify(s: &NodeState) -> Self {
        match s {
            // `LiveZero` is split out from `LiveNonZero` so the drain
            // contract can name the two discriminants separately.
            // Without this split, "drop every Live entry" would be
            // one line; with it, the diff has to say `LiveNonZero`
            // explicitly — the r11 #2 type-system rejection.
            NodeState::Live { open_count: 0 } => Self::LiveZero,
            NodeState::Live { .. } => Self::LiveNonZero,
            NodeState::Orphan { .. } => Self::Orphan,
        }
    }
}

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

    /// Retire per-NodeId entries that the just-completed capture has
    /// folded into the new state, and clear all path-keyed overlays.
    ///
    /// The classifier match is exhaustive over the three *resident*
    /// [`Lifecycle`] markers — [`LiveNonZero`], [`LiveZero`], [`Orphan`]:
    ///
    /// * [`ResidentLifecycle::LiveNonZero`] — survives. At least one
    ///   kernel fd holds the NodeId; POSIX last-close-wins says the
    ///   fd's view of the inode must outlive any path-level operation
    ///   until the final close. The lifecycle row, `hot[id]`, and
    ///   `warm[id]` are all preserved so the open fd keeps seeing its
    ///   own bytes after the capture folds the path into the new tree.
    /// * [`ResidentLifecycle::LiveZero`] — retires. The path is now
    ///   in the captured tree and no fd references the inode, so the
    ///   lifecycle row + per-NodeId bytes are safe to drop.
    /// * [`ResidentLifecycle::Orphan`] — survives. The directory
    ///   entry is already gone (post T1/T3); the bytes must outlive
    ///   the entry as long as any fd holds the NodeId. Refcount-
    ///   irrelevant — `Orphan { open_count: 0 }` is a transient state
    ///   the FSM clears on the next `release_node`, not here.
    ///
    /// [`Released`] is intentionally absent from the match.
    /// [`crate::core::Pending::lifecycle_iter`] only yields entries
    /// that exist in `state`, and the `state` map never stores the
    /// absence-of-entry state by construction — adding a `Released`
    /// arm would be unreachable (and incorrect). See
    /// [`docs/design/mount-pending-api-contracts.md`][doc] §3 row 2
    /// for the bug-by-bug analysis.
    ///
    /// The path-keyed overlays (`hot_by_path`, `tombstones`,
    /// `dir_tombstones`, `explicit_dirs`, `symlinks`) clear
    /// unconditionally: every path they covered is now folded into
    /// the new tree, and the orphan branches in
    /// `unlink_entry` / `rename_entry`'s T1/T3 already retired their
    /// path-side bindings, so no surviving NodeId is reachable through
    /// them.
    ///
    /// # Why the match makes r11 #2 unwritable
    ///
    /// The pre-retrofit drain matched directly on `NodeState`'s
    /// `Live` / `Orphan` discriminants, so collapsing both Live
    /// refcount states into one `=> None` arm was one line. The
    /// retrofitted match goes through [`ResidentLifecycle`], which
    /// splits `LiveZero` and `LiveNonZero` into separate variants —
    /// a future drive-by that wanted to drop Live-with-fds (the r11
    /// #2 bug) has to write `ResidentLifecycle::LiveNonZero => None`
    /// explicitly, which is loud in code review.
    ///
    /// [doc]: ../../../docs/design/mount-pending-api-contracts.md
    pub(crate) fn drain_for_capture(&mut self) {
        let surviving: std::collections::BTreeSet<u64> = self
            .lifecycle_iter()
            .filter_map(|(id, s)| match ResidentLifecycle::classify(&s) {
                ResidentLifecycle::LiveNonZero => Some(id), // POSIX last-close-wins
                ResidentLifecycle::Orphan => Some(id),      // open-but-unlinked
                ResidentLifecycle::LiveZero => None,        // safe to retire
            })
            .collect();
        self.apply_drain_for_capture(&surviving);
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

    /// Witness-gated FUSE-forget discharge. Returns:
    ///
    /// * `Some(warm_still_references)` — the lifecycle check passed
    ///   (`Released` or `Live { open_count == 0 }`); `hot[id]` and
    ///   `state[id]` have been dropped, and the bool tells the caller
    ///   whether `warm[id]` is still populated (i.e., whether the
    ///   inode record is still load-bearing for capture's
    ///   NodeId → path chain). `MountInner::invalidate` retires the
    ///   inode-side record iff this bool is `false`.
    /// * `None` — the lifecycle check failed (`Live { open_count >= 1 }`
    ///   or any `Orphan`); the bytes are still referenced. **The
    ///   entire forget path short-circuits** at the call site:
    ///   `hot[id]` / `state[id]` are preserved and the inode-side
    ///   `forget` is skipped. The kernel will re-issue `forget` once
    ///   the surviving fd closes (or never — the next `release_node`
    ///   retires the record).
    ///
    /// # Why `id` and not a [`KernelForgetWitness`] input?
    ///
    /// The spike doc ([§2.3][doc]) sketched the API as
    /// `kernel_forget_inode(&mut self, w: KernelForgetWitness<'_, 'brand>) -> ...`,
    /// with the caller pre-minting `w` via
    /// [`Self::witness_kernel_forget`] and threading it in. heddle#209
    /// (the analogous `transition_to_orphan` retrofit) found — and
    /// this issue confirms — that the two-step shape does not compile
    /// against the substrate: [`KernelForgetWitness`]'s
    /// `_borrow: PhantomData<&'p mut ()>` field is invariant over
    /// `'p`, so the prior `&mut BrandedPending` reborrow that minted
    /// `w` cannot shrink to admit the second `&mut self` call that
    /// would consume `w` (rustc `E0499`). Relaxing the substrate's
    /// `'p` variance to admit the two-step shape would weaken its
    /// stale-witness protection and is out of scope.
    ///
    /// Folding the FSM check and the mutation into one call preserves
    /// the spike doc's invariant — *the discharge can only fire on a
    /// state where it's safe to drop `hot[id]`* — by construction:
    ///
    /// * The body's `match` IS the [`Self::witness_kernel_forget`]
    ///   FSM check; both refer to the same
    ///   `None | Some(Live { open_count: 0 })` pattern.
    /// * Any non-discharge-safe path returns `None` before touching
    ///   [`crate::core::Pending::apply_kernel_forget`]. The call site
    ///   in `core.rs`'s `MountInner::invalidate` propagates the
    ///   `None` through to the inode-side decision — the missing
    ///   witness IS the short-circuit at the call site.
    /// * The mutation hook
    ///   [`crate::core::Pending::apply_kernel_forget`] takes
    ///   `&KernelForgetWitness` as proof; [`KernelForgetWitness::new`]
    ///   is module-private to this file, so no code outside this
    ///   module's `BrandedPending` impl can synthesize a witness or
    ///   bypass the discharge-safety check.
    ///
    /// Closes [r11 #3][doc] — the pre-retrofit `MountInner::invalidate`
    /// inlined `pending.hot.remove(&node.0)` before any FSM check, so
    /// a kernel `forget` racing an open Orphan fd lost the bytes the
    /// surviving fd needed. Now impossible by construction: the
    /// `Orphan { .. }` branch returns `None`, the call site
    /// short-circuits, and `hot[node.0]` survives.
    ///
    /// [doc]: ../../../docs/design/mount-pending-api-contracts.md
    #[doc(hidden)]
    pub(crate) fn kernel_forget_inode(&mut self, id: u64) -> Option<bool> {
        match self.inner.lookup_state(id) {
            None | Some(NodeState::Live { open_count: 0 }) => {
                let w = KernelForgetWitness::<'_, 'brand>::new(id);
                Some(self.inner.apply_kernel_forget(&w))
            }
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
        p.test_insert_state(
            11,
            NodeState::Live {
                open_count: u32::MAX,
            },
        );
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

    // ------- drain_for_capture (heddle#210 — r11 #2 retrofit) ----------------
    //
    // Pin the per-lifecycle contract of [`Pending::drain_for_capture`]:
    //
    // * `LiveZero` retires — the entry has no open fds, so its byte
    //   storage and lifecycle row are safe to drop once the capture
    //   folds the path into the new tree.
    // * `LiveNonZero` survives — open fds still hold the NodeId; POSIX
    //   last-close-wins says the fd's view outlives the path-level
    //   capture. Dropping this entry strands the kernel fd and loses
    //   the writes it accumulated (r11 #2).
    // * `Orphan` survives — directory entry already gone (T1/T3); the
    //   open-but-unlinked POSIX flow needs the bytes to live until the
    //   last close.
    //
    // Pre-retrofit (the buggy body this commit extracts) drops every
    // `Live` entry indiscriminately, so the `LiveNonZero` tests below
    // fail until the green commit lands the typed-match fix.

    #[test]
    fn drain_for_capture_preserves_live_nonzero_state() {
        let mut p = Pending::default();
        p.test_insert_state(7, NodeState::Live { open_count: 1 });
        p.drain_for_capture();
        assert_eq!(
            p.lookup_state(7),
            Some(NodeState::Live { open_count: 1 }),
            "LiveNonZero must survive capture (POSIX last-close-wins; r11 #2)"
        );
    }

    #[test]
    fn drain_for_capture_preserves_live_nonzero_with_high_refcount() {
        let mut p = Pending::default();
        p.test_insert_state(
            11,
            NodeState::Live {
                open_count: u32::MAX,
            },
        );
        p.drain_for_capture();
        assert_eq!(
            p.lookup_state(11),
            Some(NodeState::Live {
                open_count: u32::MAX
            }),
            "LiveNonZero entries must carry their open_count across capture"
        );
    }

    #[test]
    fn drain_for_capture_preserves_live_nonzero_hot_bytes() {
        let mut p = Pending::default();
        p.test_insert_state(13, NodeState::Live { open_count: 2 });
        p.test_insert_hot(13, std::path::PathBuf::from("file.txt"), b"BYTES".to_vec());
        p.drain_for_capture();
        assert!(
            p.test_has_hot(13),
            "hot[id] for a LiveNonZero entry must survive capture so reads via the open fd serve the buffered bytes"
        );
    }

    #[test]
    fn drain_for_capture_drops_live_zero_state() {
        let mut p = Pending::default();
        p.test_insert_state(9, NodeState::Live { open_count: 0 });
        p.drain_for_capture();
        assert!(
            p.lookup_state(9).is_none(),
            "LiveZero entries (no open fds) must be retired by capture; the new tree owns their data"
        );
    }

    #[test]
    fn drain_for_capture_drops_live_zero_hot_bytes() {
        let mut p = Pending::default();
        p.test_insert_state(9, NodeState::Live { open_count: 0 });
        p.test_insert_hot(9, std::path::PathBuf::from("x.txt"), b"X".to_vec());
        p.drain_for_capture();
        assert!(
            !p.test_has_hot(9),
            "hot[id] for a LiveZero entry should be dropped alongside its state row"
        );
    }

    #[test]
    fn drain_for_capture_preserves_orphan_state() {
        let mut p = Pending::default();
        p.test_insert_state(21, NodeState::Orphan { open_count: 3 });
        p.drain_for_capture();
        assert_eq!(
            p.lookup_state(21),
            Some(NodeState::Orphan { open_count: 3 }),
            "Orphan entries must survive capture (open-but-unlinked POSIX)"
        );
    }

    #[test]
    fn drain_for_capture_preserves_orphan_with_zero_refcount() {
        // Orphan { open_count: 0 } is a transient state the FSM
        // releases on the next `release_node` — it must NOT be
        // collapsed with `LiveZero` and dropped, because the
        // path-side handler that retires it relies on observing the
        // discriminant.
        let mut p = Pending::default();
        p.test_insert_state(23, NodeState::Orphan { open_count: 0 });
        p.drain_for_capture();
        assert_eq!(
            p.lookup_state(23),
            Some(NodeState::Orphan { open_count: 0 }),
            "Orphan with zero refcount must survive capture; the discriminant — not the count — is what matters"
        );
    }

    #[test]
    fn drain_for_capture_preserves_orphan_hot_bytes() {
        let mut p = Pending::default();
        p.test_insert_state(25, NodeState::Orphan { open_count: 1 });
        p.test_insert_hot(
            25,
            std::path::PathBuf::from("gone.txt"),
            b"SURVIVES".to_vec(),
        );
        p.drain_for_capture();
        assert!(
            p.test_has_hot(25),
            "hot[id] for an Orphan entry must survive capture so the surviving fd serves the inode's own bytes"
        );
    }

    #[test]
    fn drain_for_capture_mixed_state_map() {
        // One of each resident lifecycle — the typed match must
        // handle them simultaneously without collapsing distinctions.
        let mut p = Pending::default();
        p.test_insert_state(100, NodeState::Live { open_count: 1 }); // preserve
        p.test_insert_state(101, NodeState::Live { open_count: 0 }); // drop
        p.test_insert_state(102, NodeState::Orphan { open_count: 2 }); // preserve
        p.test_insert_state(103, NodeState::Orphan { open_count: 0 }); // preserve
        p.test_insert_state(104, NodeState::Live { open_count: 7 }); // preserve

        p.drain_for_capture();

        assert_eq!(p.lookup_state(100), Some(NodeState::Live { open_count: 1 }));
        assert!(p.lookup_state(101).is_none(), "LiveZero must retire");
        assert_eq!(
            p.lookup_state(102),
            Some(NodeState::Orphan { open_count: 2 })
        );
        assert_eq!(
            p.lookup_state(103),
            Some(NodeState::Orphan { open_count: 0 })
        );
        assert_eq!(p.lookup_state(104), Some(NodeState::Live { open_count: 7 }));
    }

    // ------- kernel_forget_inode (heddle#211 — r11 #3 retrofit) ---------------
    //
    // Pin the witness-gated contract of
    // [`BrandedPending::kernel_forget_inode`]:
    //
    // * `Released` (no entry) → `Some(false)` — discharge ran, warm
    //   doesn't reference, caller retires the inode record.
    // * `Live { open_count == 0 }` → `Some(false)` — discharge ran,
    //   state row + hot bytes dropped, warm doesn't reference, caller
    //   retires the inode record.
    // * `Live { open_count == 0 }` with warm populated →
    //   `Some(true)` — discharge ran but warm still references; the
    //   inode record must outlive the kernel-side forget so capture
    //   can plant the warm bytes.
    // * `Live { open_count >= 1 }` → `None` — discharge rejected;
    //   `hot[id]`, `state[id]` preserved, caller skips the
    //   inode-side forget.
    // * `Orphan { .. }` (any refcount) → `None` — discharge rejected
    //   (open-but-unlinked POSIX needs the bytes to live as long as
    //   any fd holds the NodeId, which the dentry-side forget
    //   doesn't track).
    //
    // Pre-retrofit the discharge ran unconditionally inline in
    // `MountInner::invalidate`; the `Orphan` tests below would have
    // failed (hot[id] dropped, state[id] dropped) — that's the
    // exact r11 #3 race the retrofit closes.

    #[test]
    fn kernel_forget_inode_some_for_released_no_warm() {
        let mut p = Pending::default();
        let outcome = p.with_brand(|bp| bp.kernel_forget_inode(31));
        assert_eq!(
            outcome,
            Some(false),
            "Released + no warm → discharge ran, inode record retires"
        );
    }

    #[test]
    fn kernel_forget_inode_some_for_live_zero_no_warm() {
        let mut p = Pending::default();
        p.test_insert_state(31, NodeState::Live { open_count: 0 });
        let outcome = p.with_brand(|bp| bp.kernel_forget_inode(31));
        assert_eq!(
            outcome,
            Some(false),
            "LiveZero + no warm → discharge ran, inode record retires"
        );
        assert!(
            p.lookup_state(31).is_none(),
            "discharge must drop state[id] for LiveZero"
        );
    }

    #[test]
    fn kernel_forget_inode_drops_hot_bytes_for_live_zero() {
        let mut p = Pending::default();
        p.test_insert_state(31, NodeState::Live { open_count: 0 });
        p.test_insert_hot(31, std::path::PathBuf::from("x.txt"), b"X".to_vec());
        let outcome = p.with_brand(|bp| bp.kernel_forget_inode(31));
        assert_eq!(outcome, Some(false));
        assert!(
            !p.test_has_hot(31),
            "hot[id] for a LiveZero entry must be dropped by the discharge"
        );
    }

    #[test]
    fn kernel_forget_inode_none_for_live_nonzero() {
        // The r11 #3 race shape: open fds still hold the NodeId. The
        // pre-retrofit inline path removed hot[id] regardless; the
        // retrofit rejects the discharge.
        let mut p = Pending::default();
        p.test_insert_state(31, NodeState::Live { open_count: 1 });
        p.test_insert_hot(31, std::path::PathBuf::from("x.txt"), b"X".to_vec());
        let outcome = p.with_brand(|bp| bp.kernel_forget_inode(31));
        assert_eq!(outcome, None, "LiveNonZero must reject the discharge");
        assert_eq!(
            p.lookup_state(31),
            Some(NodeState::Live { open_count: 1 }),
            "state[id] must be preserved when the witness rejects"
        );
        assert!(
            p.test_has_hot(31),
            "hot[id] must be preserved when the witness rejects \
             (otherwise the open fd loses its bytes)"
        );
    }

    #[test]
    fn kernel_forget_inode_none_for_orphan_nonzero() {
        // The headline r11 #3 case: kernel forget racing an open
        // Orphan fd. Pre-retrofit hot[id] was dropped — the surviving
        // fd then had no readable bytes. Post-retrofit the discharge
        // is rejected and hot[id] survives.
        let mut p = Pending::default();
        p.test_insert_state(31, NodeState::Orphan { open_count: 1 });
        p.test_insert_hot(31, std::path::PathBuf::from("gone.txt"), b"BYTES".to_vec());
        let outcome = p.with_brand(|bp| bp.kernel_forget_inode(31));
        assert_eq!(outcome, None, "Orphan with open fds must reject");
        assert_eq!(
            p.lookup_state(31),
            Some(NodeState::Orphan { open_count: 1 }),
            "Orphan state must be preserved when the witness rejects"
        );
        assert!(
            p.test_has_hot(31),
            "hot[id] for an Orphan with open fds must outlive a kernel \
             forget — the surviving fd needs the bytes (r11 #3)"
        );
    }

    #[test]
    fn kernel_forget_inode_none_for_orphan_zero() {
        // Even with zero open count, `Orphan` signals "directory entry
        // gone, bytes outlive it" — the discharge must reject because
        // the FSM may not have observed a racing open handle yet
        // (matches the substrate's `witness_kernel_forget` contract).
        let mut p = Pending::default();
        p.test_insert_state(31, NodeState::Orphan { open_count: 0 });
        let outcome = p.with_brand(|bp| bp.kernel_forget_inode(31));
        assert_eq!(
            outcome, None,
            "Orphan with zero refcount must still reject the discharge"
        );
        assert_eq!(
            p.lookup_state(31),
            Some(NodeState::Orphan { open_count: 0 }),
            "Orphan state must be preserved when the witness rejects"
        );
    }

    #[test]
    fn kernel_forget_inode_short_circuits_state_and_hot_when_none() {
        // Composite assertion: when the witness rejects, NONE of
        // state[id], hot[id], or hot_by_path is touched. This is the
        // "entire forget path short-circuits" contract from the
        // doc-comment.
        let mut p = Pending::default();
        p.test_insert_state(42, NodeState::Live { open_count: 3 });
        p.test_insert_hot(42, std::path::PathBuf::from("live.txt"), b"L".to_vec());
        let outcome = p.with_brand(|bp| bp.kernel_forget_inode(42));
        assert_eq!(outcome, None);
        assert_eq!(
            p.lookup_state(42),
            Some(NodeState::Live { open_count: 3 }),
            "state[id] untouched on short-circuit"
        );
        assert!(p.test_has_hot(42), "hot[id] untouched on short-circuit");
    }

    // ------- ResidentLifecycle classifier ------------------------------------

    #[test]
    fn resident_lifecycle_classify_live_zero() {
        assert_eq!(
            ResidentLifecycle::classify(&NodeState::Live { open_count: 0 }),
            ResidentLifecycle::LiveZero,
        );
    }

    #[test]
    fn resident_lifecycle_classify_live_nonzero_at_one() {
        assert_eq!(
            ResidentLifecycle::classify(&NodeState::Live { open_count: 1 }),
            ResidentLifecycle::LiveNonZero,
        );
    }

    #[test]
    fn resident_lifecycle_classify_live_nonzero_at_max() {
        assert_eq!(
            ResidentLifecycle::classify(&NodeState::Live {
                open_count: u32::MAX
            }),
            ResidentLifecycle::LiveNonZero,
        );
    }

    #[test]
    fn resident_lifecycle_classify_orphan_any_refcount() {
        // The classifier collapses both `Orphan { 0 }` and
        // `Orphan { n }` onto the same discriminant — the substrate
        // contract is "discriminant gates the drain decision", not
        // refcount.
        assert_eq!(
            ResidentLifecycle::classify(&NodeState::Orphan { open_count: 0 }),
            ResidentLifecycle::Orphan,
        );
        assert_eq!(
            ResidentLifecycle::classify(&NodeState::Orphan { open_count: 5 }),
            ResidentLifecycle::Orphan,
        );
        assert_eq!(
            ResidentLifecycle::classify(&NodeState::Orphan {
                open_count: u32::MAX
            }),
            ResidentLifecycle::Orphan,
        );
    }

    // ------- proptest: FSM-trace-ending-in-capture -----------------------------
    //
    // Generates a random sequence of `(NodeId, NodeState)` writes
    // (last-write-wins per id), replays them into a fresh `Pending`,
    // runs `drain_for_capture`, and asserts the post-capture map is
    // exactly the input collapse minus the `LiveZero` entries — i.e.
    // every `LiveNonZero` / `Orphan` survivor carries its `open_count`
    // intact, and every `LiveZero` retires. The NodeId range is kept
    // small (0..16) to maximise collisions, and the trace length is
    // capped at 32 to keep shrunken counterexamples readable. Covers
    // the heddle#210 AC: "random FSM trace ending in a capture
    // preserves all open-fd refcounts".

    use proptest::prelude::*;

    proptest::proptest! {
        #[test]
        fn drain_for_capture_proptest_preserves_open_counts(
            entries in proptest::collection::vec(
                (
                    0u64..16,
                    proptest::prop_oneof![
                        proptest::num::u32::ANY
                            .prop_map(|n| NodeState::Live { open_count: n }),
                        proptest::num::u32::ANY
                            .prop_map(|n| NodeState::Orphan { open_count: n }),
                    ],
                ),
                0..32usize,
            ),
        ) {
            // Replay the trace; last write wins per NodeId. Build the
            // expected post-drain map alongside.
            let mut p = Pending::default();
            let mut expected: std::collections::BTreeMap<u64, NodeState> =
                std::collections::BTreeMap::new();
            for (id, state) in &entries {
                p.test_insert_state(*id, *state);
                expected.insert(*id, *state);
            }
            // LiveZero retires by contract.
            expected.retain(|_, s| !matches!(s, NodeState::Live { open_count: 0 }));

            p.drain_for_capture();

            // Post-drain state must equal the expected map exactly:
            // every `LiveNonZero` / `Orphan` entry (with its
            // `open_count` intact) is present, and nothing else is.
            let after: std::collections::BTreeMap<u64, NodeState> =
                p.lifecycle_iter().collect();
            proptest::prop_assert_eq!(after, expected);
        }
    }

    // ------- FSM proptest harness (heddle#212) ---------------------------------
    //
    // Generates random sequences of synthetic FUSE callbacks against a
    // fresh `Pending` and asserts the post-sequence state matches an
    // oracle that encodes the §1 FSM in
    // `docs/design/mount-posix-semantics.md` at the substrate boundary
    // — i.e. the four sites the substrate already gates
    // ([`BrandedPending::transition_to_orphan`],
    // [`BrandedPending::kernel_forget_inode`],
    // [`Pending::drain_for_capture`]) plus the saturating-add /
    // saturating-sub refcount arithmetic that
    // [`crate::core::MountInner::on_open`] /
    // [`crate::core::MountInner::release_node`] perform directly on the
    // `state` map.
    //
    // # Op set
    //
    // * [`Op::Open`] / [`Op::Release`] — mimic the refcount arithmetic
    //   the mount shell applies on FUSE `open` / `release` callbacks.
    //   Seeded via [`Pending::test_insert_state`]. `Release` is
    //   *saturating at zero* in the harness: the production
    //   `release_node` removes the `state` entry for `Live { 1 }` and
    //   `Orphan { 1 }` (transitions T2/T6 in §1), but the substrate
    //   exposes state-entry removal only through the witness-gated
    //   [`BrandedPending::kernel_forget_inode`] path (and only for
    //   `Released | LiveZero`). The harness retires `LiveZero` entries
    //   via the subsequent `Forget` / `Drain` ops; T6 (Orphan{1} →
    //   Released via final release) is therefore a saturating no-op in
    //   the harness, an intentional simplification that keeps the
    //   harness scoped to the substrate boundary.
    // * [`Op::Unlink`] — drives [`BrandedPending::transition_to_orphan`].
    //   The substrate rejects every non-`LiveNonZero` state by
    //   returning `None`; the oracle mirrors the same gate.
    // * [`Op::Forget`] — drives [`BrandedPending::kernel_forget_inode`].
    //   Substrate fires only for `Released | LiveZero`; rejects
    //   `LiveNonZero` and any `Orphan`. The oracle mirrors the same
    //   gate.
    // * [`Op::Drain`] — drives [`Pending::drain_for_capture`]. Retires
    //   `LiveZero` entries; preserves `LiveNonZero` and `Orphan`.
    //
    // # Properties
    //
    // * `fsm_state_matches_oracle` — for every NodeId in the small
    //   universe `0..8`, the post-sequence `lookup_state` agrees with
    //   the oracle. Catches any substrate-side divergence from the
    //   modeled FSM, including the four r11 bug-classes (transition
    //   from the wrong state, drop of `LiveNonZero` on capture,
    //   un-gated kernel-forget on `Orphan`, mis-typed caller hitting
    //   `transition_to_orphan` from a non-Live state).
    // * `fsm_open_count_refcount_balance` — for every surviving entry,
    //   the `open_count` field equals the oracle's tracked refcount.
    //   Saturating arithmetic prevents underflow at the type level
    //   (u32 ⇒ no negative); this property additionally catches
    //   *off-by-one* drift (where the substrate would silently update
    //   the count out of step with the on_open / release_node
    //   semantics).
    // * `fsm_witness_constructors_mutually_exclusive` — at any post-
    //   sequence instant, the four `witness_*` constructors classify
    //   each NodeId into exactly one bucket. A regression that widened
    //   or narrowed any witness's accepting set would surface here.
    //
    // # Deliberately-broken counterexample (red commit, heddle#212)
    //
    // The red commit on this branch shipped a fourth property,
    // `fsm_open_count_strictly_positive`, that asserted every
    // surviving entry had `open_count > 0`. The §1 FSM doesn't say
    // that — `Live { 0 }` is a reachable state after any `Open(id)`
    // followed by `Release(id)` — so proptest produced a shrunk
    // counterexample on the first run:
    //
    // ```text
    // ops = [Open(0), Release(0)]
    // pending.state = { 0: Live { open_count: 0 } }
    // ```
    //
    // That two-op minimum reproducer is the proof the harness shrinks
    // counterexamples down to the smallest divergence. The green
    // commit removes the broken property; the artifact survives in
    // this comment and in [`harness_catches_state_divergence`] (which
    // pins the same proof at unit-test scale, so a future regression
    // that quietly broke the harness's divergence-detection wouldn't
    // ship green).

    /// Modeled lifecycle state. Mirrors [`NodeState`] but adds an
    /// explicit [`ModelState::Released`] variant for "no entry in the
    /// `state` map" — the third FSM state per §1.1 of
    /// `mount-posix-semantics.md`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ModelState {
        Released,
        Live(u32),
        Orphan(u32),
    }

    /// One synthetic FUSE-callback worth of FSM input. `u8` NodeIds
    /// keep the strategy's value-space small (0..8 in the strategies
    /// below) so shrunken counterexamples stay readable.
    #[derive(Clone, Debug)]
    enum Op {
        Open(u8),
        Release(u8),
        Unlink(u8),
        Forget(u8),
        Drain,
    }

    /// Pure-data oracle. Implements the substrate-boundary FSM
    /// described in the module-level comment. Replays the same ops the
    /// harness applies to a real `Pending` and stays in lockstep.
    #[derive(Debug, Default, Clone)]
    struct Oracle {
        states: std::collections::BTreeMap<u8, ModelState>,
    }

    impl Oracle {
        fn lookup(&self, id: u8) -> ModelState {
            self.states
                .get(&id)
                .copied()
                .unwrap_or(ModelState::Released)
        }

        fn set(&mut self, id: u8, state: ModelState) {
            if matches!(state, ModelState::Released) {
                self.states.remove(&id);
            } else {
                self.states.insert(id, state);
            }
        }

        fn apply(&mut self, op: &Op) {
            match op {
                Op::Open(id) => {
                    let next = match self.lookup(*id) {
                        ModelState::Released => ModelState::Live(1),
                        ModelState::Live(n) => ModelState::Live(n.saturating_add(1)),
                        ModelState::Orphan(n) => ModelState::Orphan(n.saturating_add(1)),
                    };
                    self.set(*id, next);
                }
                Op::Release(id) => {
                    // Saturating at zero — see the module comment on
                    // T6's intentional simplification.
                    let next = match self.lookup(*id) {
                        ModelState::Released => ModelState::Released,
                        ModelState::Live(n) => ModelState::Live(n.saturating_sub(1)),
                        ModelState::Orphan(n) => ModelState::Orphan(n.saturating_sub(1)),
                    };
                    self.set(*id, next);
                }
                Op::Unlink(id) => {
                    // Substrate's `transition_to_orphan` accepts only
                    // `Live { open_count >= 1 }`. Anything else (incl.
                    // LiveZero, any Orphan, Released) is a no-op.
                    if let ModelState::Live(n) = self.lookup(*id)
                        && n >= 1
                    {
                        self.set(*id, ModelState::Orphan(n));
                    }
                }
                Op::Forget(id) => {
                    // Substrate's `kernel_forget_inode` accepts
                    // `Released | LiveZero`. Both collapse to
                    // Released.
                    match self.lookup(*id) {
                        ModelState::Released | ModelState::Live(0) => {
                            self.set(*id, ModelState::Released);
                        }
                        _ => { /* substrate rejects; no-op */ }
                    }
                }
                Op::Drain => {
                    // Retain LiveNonZero + Orphan; retire LiveZero.
                    self.states.retain(|_, s| !matches!(s, ModelState::Live(0)));
                }
            }
        }
    }

    /// Apply `op` to the real `Pending`. Drives the substrate-gated
    /// methods directly for `Unlink` / `Forget` / `Drain`; mimics the
    /// `MountInner::on_open` / `MountInner::release_node` refcount
    /// arithmetic via [`Pending::test_insert_state`] for `Open` /
    /// `Release`. The mimicked arithmetic is byte-for-byte identical
    /// to the production paths at `core.rs:1368-1381` (on_open) and
    /// `core.rs:2938-2993` (release_node), modulo the T2/T6 state-
    /// removal cases that aren't reachable through the substrate's
    /// exposed surface (see the module comment).
    fn apply_to_pending(p: &mut Pending<'_>, op: &Op) {
        match op {
            Op::Open(id) => {
                let id64 = *id as u64;
                let next = match p.lookup_state(id64) {
                    None => NodeState::Live { open_count: 1 },
                    Some(NodeState::Live { open_count }) => NodeState::Live {
                        open_count: open_count.saturating_add(1),
                    },
                    Some(NodeState::Orphan { open_count }) => NodeState::Orphan {
                        open_count: open_count.saturating_add(1),
                    },
                };
                p.test_insert_state(id64, next);
            }
            Op::Release(id) => {
                let id64 = *id as u64;
                if let Some(s) = p.lookup_state(id64) {
                    let next = match s {
                        NodeState::Live { open_count } => NodeState::Live {
                            open_count: open_count.saturating_sub(1),
                        },
                        NodeState::Orphan { open_count } => NodeState::Orphan {
                            open_count: open_count.saturating_sub(1),
                        },
                    };
                    p.test_insert_state(id64, next);
                }
            }
            Op::Unlink(id) => {
                p.with_brand(|bp| {
                    let _ = bp.transition_to_orphan(*id as u64);
                });
            }
            Op::Forget(id) => {
                p.with_brand(|bp| {
                    let _ = bp.kernel_forget_inode(*id as u64);
                });
            }
            Op::Drain => {
                p.drain_for_capture();
            }
        }
    }

    /// Project a real [`NodeState`] onto the model's [`ModelState`]
    /// for direct comparison. `None` (no entry) → `Released`.
    fn project(s: Option<NodeState>) -> ModelState {
        match s {
            None => ModelState::Released,
            Some(NodeState::Live { open_count }) => ModelState::Live(open_count),
            Some(NodeState::Orphan { open_count }) => ModelState::Orphan(open_count),
        }
    }

    /// Strategy for one `Op`. NodeIds are drawn from `0..8` so a long
    /// sequence reliably revisits the same id and exercises every
    /// transition path. The five-variant `prop_oneof` weights each Op
    /// equally; biasing toward `Drain` / `Unlink` is unnecessary —
    /// length-32 sequences land enough of each.
    fn op_strategy() -> impl Strategy<Value = Op> {
        proptest::prop_oneof![
            (0u8..8u8).prop_map(Op::Open),
            (0u8..8u8).prop_map(Op::Release),
            (0u8..8u8).prop_map(Op::Unlink),
            (0u8..8u8).prop_map(Op::Forget),
            proptest::strategy::Just(Op::Drain),
        ]
    }

    proptest::proptest! {
        /// Property 1: for every NodeId in the small universe `0..8`,
        /// the post-sequence `lookup_state` matches the oracle's
        /// modeled state. This is the §1 FSM coherence check: any
        /// substrate-side transition that lands in a state not
        /// reachable via documented transitions shows up here.
        #[test]
        fn fsm_state_matches_oracle(
            ops in proptest::collection::vec(op_strategy(), 0..32),
        ) {
            let mut p = Pending::default();
            let mut oracle = Oracle::default();
            for op in &ops {
                apply_to_pending(&mut p, op);
                oracle.apply(op);
            }
            for id in 0u8..8u8 {
                let want = oracle.lookup(id);
                let got = project(p.lookup_state(id as u64));
                proptest::prop_assert_eq!(
                    want, got,
                    "FSM divergence at id {} after {:?}", id, ops
                );
            }
        }

        /// Property 2: for every surviving entry the `open_count` field
        /// equals the oracle's modeled refcount — the substrate never
        /// drifts off-by-one against the on_open / release_node
        /// arithmetic. Saturating semantics keep both sides above zero
        /// (so this property is also the "open_count is always non-
        /// negative" refcount-sanity AC); a regression that introduced
        /// wrapping or signed arithmetic would surface here.
        #[test]
        fn fsm_open_count_refcount_balance(
            ops in proptest::collection::vec(op_strategy(), 0..32),
        ) {
            let mut p = Pending::default();
            let mut oracle = Oracle::default();
            for op in &ops {
                apply_to_pending(&mut p, op);
                oracle.apply(op);
            }
            for (id_u64, state) in p.lifecycle_iter() {
                let id = id_u64 as u8;
                let want = match oracle.lookup(id) {
                    ModelState::Live(n) | ModelState::Orphan(n) => n,
                    ModelState::Released => {
                        return Err(proptest::test_runner::TestCaseError::fail(
                            format!("id {id} survives in pending but oracle says Released")
                        ));
                    }
                };
                let got = match state {
                    NodeState::Live { open_count } | NodeState::Orphan { open_count } => open_count,
                };
                proptest::prop_assert_eq!(want, got, "open_count divergence at id {}", id);
            }
        }

        /// Property 3: at any post-sequence instant the four witness
        /// constructors classify each NodeId into exactly one bucket
        /// — i.e. the FSM is a partition of the per-NodeId state
        /// space (LiveNonZero | LiveZero | Orphan | Released). A
        /// regression that widened or narrowed any constructor's
        /// accepting set (e.g. `witness_live_nonzero` returning `Some`
        /// for `Live { 0 }`) would surface as a multi-bit mask here.
        #[test]
        fn fsm_witness_constructors_mutually_exclusive(
            ops in proptest::collection::vec(op_strategy(), 0..32),
        ) {
            let mut p = Pending::default();
            for op in &ops {
                apply_to_pending(&mut p, op);
            }
            for id in 0u8..8u8 {
                let id64 = id as u64;
                let mask = p.with_brand(|bp| {
                    let mut m = 0u8;
                    if bp.witness_live_nonzero(id64).is_some() { m |= 1 << 0; }
                    if bp.witness_live_zero(id64).is_some()    { m |= 1 << 1; }
                    if bp.witness_orphan(id64).is_some()       { m |= 1 << 2; }
                    if bp.witness_released(id64).is_some()     { m |= 1 << 3; }
                    m
                });
                proptest::prop_assert_eq!(
                    mask.count_ones(), 1u32,
                    "witness mask for id {} is 0b{:04b} after {:?}; expected exactly one bit",
                    id, mask, ops
                );
            }
        }
    }

    /// Pin the harness's divergence-detection at unit-test scale: hand
    /// the oracle a "Pending" state that doesn't match the modeled
    /// state and assert that the equality check function fails. This
    /// is the structural twin of the proptest's
    /// `fsm_state_matches_oracle` property, and is what proves the
    /// harness's *check function* (not just the strategy) is sound. A
    /// future regression that quietly made the check trivially pass
    /// would surface here.
    #[test]
    fn harness_catches_state_divergence() {
        // Build a real Pending in a "stuck-Live" state (the r11 #1
        // bug shape: transition_to_orphan never fired, even though
        // the kernel emitted Unlink). The oracle, fed the same op
        // sequence, says Orphan.
        let mut p = Pending::default();
        p.test_insert_state(0, NodeState::Live { open_count: 1 });
        // Oracle replay: Open(0) then Unlink(0).
        let mut oracle = Oracle::default();
        oracle.apply(&Op::Open(0));
        oracle.apply(&Op::Unlink(0));
        // Sanity: oracle is now Orphan(1).
        assert_eq!(oracle.lookup(0), ModelState::Orphan(1));
        // Pending was never transitioned, so it stays Live(1).
        assert_eq!(
            project(p.lookup_state(0)),
            ModelState::Live(1),
            "harness setup precondition"
        );
        // The structural equality check the proptest performs:
        let oracle_state = oracle.lookup(0);
        let pending_state = project(p.lookup_state(0));
        assert_ne!(
            oracle_state, pending_state,
            "harness must report divergence: oracle says {oracle_state:?}, \
             pending says {pending_state:?}"
        );
    }

    /// Pin the saturating-Release invariant at unit-test scale: the
    /// `MountInner::release_node` arithmetic uses `saturating_sub(1)`,
    /// so an unbalanced release stream (more releases than opens) can
    /// never underflow `open_count`. The proptest's
    /// `fsm_open_count_refcount_balance` covers this property at
    /// scale; this test is the minimal hand-picked reproducer so a
    /// regression that swapped `saturating_sub` for `wrapping_sub`
    /// surfaces with a one-line diff.
    #[test]
    fn release_saturates_at_zero_under_unbalanced_stream() {
        let mut p = Pending::default();
        let ops = [
            Op::Open(0),
            Op::Release(0),
            Op::Release(0), // would underflow without saturating
            Op::Release(0),
        ];
        let mut oracle = Oracle::default();
        for op in &ops {
            apply_to_pending(&mut p, op);
            oracle.apply(op);
        }
        assert_eq!(oracle.lookup(0), ModelState::Live(0));
        assert_eq!(project(p.lookup_state(0)), ModelState::Live(0));
    }
}
