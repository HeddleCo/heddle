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
//!   (FuseShell now;        (FSKit/ProjFS/CfAPI later)
//!     ↓
//! ContentAddressedMount   ← pure Rust core
//!     ↓
//! crates/repo + crates/objects  (already exists)
//! ```
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

// Re-export the fuser background-session type so callers (notably the
// CLI's mount lifecycle and daemon registry) don't have to take a
// direct fuser dep just to hold onto a live mount.
#[cfg(all(target_os = "linux", feature = "fuse"))]
pub use fuser::BackgroundSession;

#[cfg(all(target_os = "macos", feature = "fskit"))]
pub use crate::fskit::{FSKitSession, FSKitShell};
#[cfg(all(target_os = "linux", feature = "fuse"))]
pub use crate::fuse::FuseShell;
pub use crate::{
    core::{ContentAddressedMount, PromotionPolicy},
    error::{MountError, Result},
    shell::{Attrs, Entry, NodeId, NodeKind, PlatformShell},
};

#[cfg(test)]
mod tests;