// SPDX-License-Identifier: Apache-2.0
//! Long-lived helper-daemon scaffolding.
//!
//! Heddle's local helper subprocesses share endpoint discovery
//! (`.heddle/state/<name>.endpoint.json`), idle-timeout exit, and
//! crashed-PID detection via `kill -0`. Transports differ:
//!
//! * fsmonitor still speaks JSON-over-TCP on `127.0.0.1`.
//! * the mount daemon binds a mode-0600 Unix socket and checks
//!   same-uid `SO_PEERCRED`. Localhost TCP is not an authz
//!   boundary (heddle#901).
//!
//! What lives here:
//!
//! * [`endpoint`] — the on-disk endpoint state shape
//!   ([`EndpointState`]), atomic persist, `kill -0` staleness
//!   probe, file-path conventions.
//! * [`protocol`] — JSON-over-TCP framing (one request, one
//!   newline-delimited response per connection) plus the shared
//!   helper-version constants.
//! * [`server`] — listener loop with idle exit, generic over a
//!   request/response handler so callers (fsmonitor, mountd) plug
//!   in their own verb set.
//!
//! What does NOT live here: the fsmonitor's `LocalMonitorServer` and
//! its protocol enum. Those stayed in `fsmonitor.rs` because moving
//! them would multiply the diff for no reviewer benefit — the
//! behaviour we wanted to share is the *plumbing*, not the verb set.
//! See `crates/repo/src/fsmonitor.rs` for the existing fsmonitor
//! consumer of this module.

pub mod endpoint;
pub mod mount_auth;
pub mod mount_proto;
#[cfg(unix)]
pub mod peer;
pub mod predecessor;
pub mod protocol;
pub mod server;
#[cfg(unix)]
pub mod unix_server;

pub use endpoint::{
    EndpointState, default_state_dir, endpoint_path_for, load_endpoint, persist_endpoint,
    pid_alive, remove_endpoint, remove_endpoint_if_owned,
};
pub use mount_auth::{
    MountAuthDenied, MountClientAuth, authorize_mount_request, trusted_mount_path,
};
pub use mount_proto::{
    ERR_MOUNT_CONFLICT, ERR_MOUNT_UNSUPPORTED, ERR_UNAUTHORIZED, ERR_VERSION_MISMATCH,
    MOUNT_PROTOCOL_V2, MOUNT_PROTOCOL_VERSION, MountDaemonRequest, MountDaemonResponse,
    MountEndpointDisposition, MountRegistryFile, MountStatus, PersistedMount,
    mount_daemon_endpoint_path, mount_daemon_registry_path, mount_daemon_socket_path,
    mount_endpoint_disposition,
};
#[cfg(unix)]
pub use peer::check_peer_uid_matches_self;
pub use predecessor::retire_live_tcp_predecessor;
pub use protocol::{
    HELPER_HOST, HELPER_IDLE_POLL_MS, HELPER_IDLE_TIMEOUT_SECS, send_json_request,
    send_json_request_unix, send_mount_daemon_request,
};
pub use server::{IdleDecision, mount_idle_policy, run_server_loop};
#[cfg(unix)]
pub use unix_server::{
    UnixDaemonHandler, bind_unix_socket, handle_authenticated_unix_connection, run_unix_server_loop,
};
