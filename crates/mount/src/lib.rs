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

pub mod core;
pub mod error;
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
    core::{ContentAddressedMount, PromotionPolicy},
    error::{MountError, Result},
    shell::{Attrs, Entry, NodeId, NodeKind, PlatformShell},
};

#[cfg(test)]
mod tests;