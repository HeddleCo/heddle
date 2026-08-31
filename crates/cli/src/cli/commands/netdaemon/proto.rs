// SPDX-License-Identifier: Apache-2.0
//! Wire protocol + on-disk conventions for the box network daemon
//! (`heddle netd …`).
//!
//! The control transport is the same same-uid JSON-over-UDS framing
//! the mount daemon uses (`repo::daemon`), but the daemon is box
//! scoped: it is anchored at `<heddle_home>`, not a repository root,
//! and it is located by its cryptographic node id rather than a
//! host:port. One line in, one line out, one request per connection.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Wire-protocol version. Bumped in lockstep between the daemon and
/// the `netd status` / `netd stop` clients; a mismatch tells the
/// client the on-disk endpoint file was written by a different build.
pub const NETWORK_DAEMON_PROTOCOL_VERSION: u32 = 1;

/// Discovery-file stem under `<heddle_home>/state/`. Distinct from the
/// mount daemon's `heddled` so the two daemons never collide.
pub const NETWORK_DAEMON_NAME: &str = "heddle-netd";

/// Same-uid control socket file name under `<heddle_home>/state/`.
pub const NETWORK_DAEMON_SOCKET: &str = "heddle-netd.sock";

/// Box-scoped endpoint-discovery file:
/// `<heddle_home>/state/heddle-netd.endpoint.json`.
pub fn network_daemon_endpoint_path(heddle_home: &Path) -> PathBuf {
    repo::daemon::box_endpoint_path_in(heddle_home, NETWORK_DAEMON_NAME)
}

/// Box-scoped control socket path:
/// `<heddle_home>/state/heddle-netd.sock`.
pub fn network_daemon_socket_path(heddle_home: &Path) -> PathBuf {
    repo::daemon::box_state_dir_in(heddle_home).join(NETWORK_DAEMON_SOCKET)
}

/// Local control RPC sent by `netd status` / `netd stop`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NetworkDaemonRequest {
    /// Liveness + node-id probe.
    Health {},
    /// Ask the daemon to close its endpoint and exit.
    Shutdown {},
}

/// Single-line reply to a [`NetworkDaemonRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NetworkDaemonResponse {
    /// Daemon is up; carries the advertised device node id.
    Health {
        version: u32,
        ok: bool,
        uptime_s: u64,
        node_id: String,
    },
    /// Shutdown was accepted; the daemon is draining and will exit.
    Shutdown { version: u32, ok: bool },
    /// A verb-independent refusal (e.g. same-uid check failed upstream).
    Error {
        version: u32,
        code: String,
        message: String,
    },
}
