// SPDX-License-Identifier: Apache-2.0
//! Heddle's content-addressed mount.
//!
//! `mount` is the platform-agnostic core (and Linux FUSE shell) that
//! exposes a heddle thread as a directory tree. Reads walk the
//! Merkle DAG lazily; writes (eventually) flow into a per-thread
//! overlay that drains to a heddle commit on `heddle capture`.
//!
//! The architecture is:
//!
//! ```text
//! PlatformShell trait     ← thin platform adapters
//!   (FuseShell, FSKitShell, ProjFsShell, NfsShell)
//!     ↓
//! ContentAddressedMount   ← pure Rust core
//!     ↓
//! crates/repo + crates/objects  (already exists)
//! ```
//!
//! Three of those adapters are per-OS (FUSE on Linux, FSKit on
//! macOS, ProjFS on Windows). [`NfsShell`] is the universal
//! fallback: it stands up an in-process NFSv3 server and asks the
//! host's built-in NFS client to mount it. The CLI's mount
//! lifecycle prefers the native adapter and falls back to NFS
//! when the native one is unavailable at runtime.
//!
//! See [`PlatformShell`] for the trait every adapter implements,
//! and [`ContentAddressedMount`] for the heddle-aware core.

pub mod cache;
pub mod core;
pub mod error;
mod pending;
pub mod shell;

#[cfg(all(target_os = "linux", feature = "fuse"))]
pub mod fuse;

#[cfg(all(target_os = "macos", feature = "fskit"))]
pub mod fskit;

#[cfg(all(target_os = "windows", feature = "projfs"))]
pub mod projfs;

#[cfg(feature = "nfs")]
pub mod nfs;

// Re-export the fuser background-session type so callers (notably the
// CLI's mount lifecycle and daemon registry) don't have to take a
// direct fuser dep just to hold onto a live mount.
#[cfg(all(target_os = "linux", feature = "fuse"))]
pub use fuser::BackgroundSession;

#[cfg(all(target_os = "macos", feature = "fskit"))]
pub use crate::fskit::{FSKitSession, FSKitShell};
#[cfg(all(target_os = "linux", feature = "fuse"))]
pub use crate::fuse::FuseShell;
#[cfg(feature = "nfs")]
pub use crate::nfs::{NfsSession, NfsShell};
#[cfg(all(target_os = "windows", feature = "projfs"))]
pub use crate::projfs::{ProjFsSession, ProjFsShell};
pub use crate::{
    cache::{BlobCachePool, BlobCacheStats, DEFAULT_BLOB_CACHE_BYTES},
    core::{ContentAddressedMount, MountOptions, PrewarmHandle, PrewarmStats, PromotionPolicy},
    error::{MountError, Result},
    shell::{AttrUpdate, Attrs, Entry, NodeId, NodeKind, PlatformShell},
};

#[cfg(test)]
mod tests;

/// NOT a stable public API. Re-exports the `pub(crate)` witness
/// substrate so a `compile_fail` doctest can pin the brand-isolation
/// invariant for Codex PR #217 r2 (`crates/mount/src/pending.rs`
/// finding `3293832936`). Hidden from rendered docs; consumers must
/// not depend on this module.
///
/// # Brand isolation (post heddle#208 r2)
///
/// The substrate brands `Pending`, `Witness`, and `KernelForgetWitness`
/// with an invariant `'brand` lifetime that's introduced per call via
/// [`core::Pending::with_brand`]. Witnesses minted under one `'brand`
/// cannot be passed to methods on a `Pending` carrying a different
/// `'brand` — the cross-instance bug Codex flagged on r1 stops
/// type-checking.
///
/// The doctest below is the executable proof. It compiles against
/// the pre-brand substrate (r1; cross-instance use is allowed —
/// THE BUG) and fails to compile against the post-brand substrate
/// (r2; brand mismatch — THE FIX).
///
/// ```compile_fail
/// use mount::__pending_substrate_for_doctest::*;
/// let mut p1 = Pending::default();
/// let mut p2 = Pending::default();
/// p1.with_brand(|p1_branded| {
///     p2.with_brand(|p2_branded| {
///         // `w` carries p1_branded's brand. Pre-fix it's
///         // `Witness<'_, LiveNonZero>` (no brand); post-fix it's
///         // `Witness<'_, 'brand_of_p1, LiveNonZero>`.
///         let w = p1_branded
///             .witness_live_nonzero(0)
///             .expect("doctest never runs — compile_fail");
///         // Pre-fix: `peek_witness` accepts any `&Witness<'_, S>`.
///         //          Compiles → `compile_fail` assertion fails → RED.
///         // Post-fix: `peek_witness` on `Pending<'brand_of_p2>`
///         //           wants `&Witness<'_, 'brand_of_p2, S>`. Brand
///         //           mismatch → fails to compile → assertion holds → GREEN.
///         //
///         // `peek_witness` takes `&self` + `&Witness` (not consuming)
///         // so the proof stays orthogonal to the borrow-checker
///         // constraint introduced by `_borrow: PhantomData<&'p mut ()>`
///         // on the witness — that's a separate spike-doc question
///         // the retrofit issues will address, not this PR.
///         let _ = p2_branded.peek_witness(&w);
///     });
/// });
/// ```
///
/// # Constructor bypass (post heddle#208 r3)
///
/// r2 brand-isolated `Witness` cross-instance use *inside* `with_brand`,
/// but the `witness_*` constructors were still callable directly on
/// `&mut Pending<'brand>`. Because `MountInner` stores
/// `Pending<'static>`, an internal caller could mint
/// `Witness<..., 'static, _>` without going through `with_brand` at all
/// — and two distinct `Pending<'static>` values share the `'static`
/// brand, so witnesses crossed between them (Codex PR #217 r2 finding
/// `3293898540`).
///
/// r3 closes that hole by moving every `witness_*` (and `peek_witness`)
/// onto a sealed `BrandedPending<'p, 'brand>` wrapper whose only
/// constructor is `Pending::with_brand`. The doctest below is the
/// executable proof. It compiles against r2 (constructors live on
/// `Pending<'brand>`; direct mint succeeds → THE BUG) and fails to
/// compile against r3 (constructors are unreachable outside
/// `with_brand` → THE FIX).
///
/// ```compile_fail
/// use mount::__pending_substrate_for_doctest::*;
/// let mut p1 = Pending::default();
/// let mut p2 = Pending::default();
/// // Pre-r3: `Pending::witness_live_nonzero` exists on `Pending<'brand>`
/// //         so this compiles; `w` ends up `Witness<'_, 'static, _>`.
/// // Post-r3: `Pending` no longer has any `witness_*` method — this
/// //         call refers to a non-existent name and fails to compile.
/// let w = p1
///     .witness_live_nonzero(0)
///     .expect("doctest never runs — compile_fail");
/// // Pre-r3: `Pending::peek_witness` accepts `&Witness<'_, 'static, _>`
/// //         (both `p2` and `w` carry the `'static` brand). Compiles →
/// //         the cross-instance bypass exists → `compile_fail`
/// //         assertion fails → RED.
/// // Post-r3: even if you somehow had a witness in scope, `peek_witness`
/// //         is on `BrandedPending` now, so this also fails to compile.
/// let _ = p2.peek_witness(&w);
/// ```
#[doc(hidden)]
pub mod __pending_substrate_for_doctest {
    pub use crate::core::Pending;
    pub use crate::pending::{
        KernelForgetWitness, Lifecycle, LiveNonZero, LiveZero, Orphan, Released, Witness,
    };
}