// SPDX-License-Identifier: Apache-2.0
//! Box-scoped network daemon (`heddle netd …`).
//!
//! A long-running, async daemon that binds the machine's single
//! persistent Iroh endpoint on the persisted device node id and keeps
//! it relay-reachable (heddle#1533, piece 1). Distinct from the
//! Linux/FUSE mount daemon in [`super::daemon`]: separate process,
//! separate lifecycle (no idle-exit), any Unix host.
//!
//! Three verbs hang off this module, mirroring the mount daemon's
//! control surface but box scoped:
//!
//! * `heddle netd serve` — foreground async daemon.
//! * `heddle netd status` — liveness + advertised node id.
//! * `heddle netd stop` — drain and exit.
//!
//! Piece 3 (heddle#1620) mounts the claim-ALPN router on the endpoint
//! at the seam documented in [`server`].

mod cmd;
pub mod proto;
#[cfg(all(unix, feature = "client"))]
mod server;

pub use cmd::{cmd_netd_serve, cmd_netd_status, cmd_netd_stop};
