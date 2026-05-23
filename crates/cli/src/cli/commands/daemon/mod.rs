// SPDX-License-Identifier: Apache-2.0
//! Long-lived mount daemon (`heddle daemon serve`).
//!
//! Reuses the helper-subprocess scaffolding in `repo::daemon` —
//! endpoint file, JSON-over-TCP framing, idle-exit loop — but
//! layers a `MountRegistry` on top so a single daemon process can
//! own multiple FUSE sessions across CLI invocations. See
//! `docs/design/mount-daemon.md` for the full lifecycle and
//! failure-mode analysis.
//!
//! Three CLI verbs hang off this module:
//!
//! * `heddle daemon serve` — foreground entry point. Spawned
//!   detached by the per-thread `--daemon` codepath in
//!   `mount_lifecycle.rs`, but also runnable interactively for
//!   debugging.
//! * `heddle daemon status` — sends a `health` RPC to the running
//!   daemon and prints version + uptime + mount count. No-op if the
//!   daemon isn't running.
//! * `heddle daemon stop` — sends `shutdown`, waits for the
//!   endpoint file to disappear, and sweeps any leftover live
//!   mounts with `fusermount -u` as a safety net.
//!
//! On non-Linux builds (or Linux without `--features mount`) every
//! verb returns the same `mount_unsupported` runtime error the
//! existing in-process path uses; see
//! [`crate::cli::commands::mount_lifecycle::virtualized_unsupported_error`].

pub mod client;

#[cfg(all(target_os = "linux", feature = "mount"))]
pub mod registry;
#[cfg(all(target_os = "linux", feature = "mount"))]
pub mod server;

mod cmd;

pub use cmd::{cmd_daemon_serve, cmd_daemon_status, cmd_daemon_stop};
