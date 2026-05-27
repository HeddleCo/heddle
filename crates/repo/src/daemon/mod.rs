// SPDX-License-Identifier: Apache-2.0
//! Long-lived helper-daemon scaffolding.
//!
//! Heddle's local helper subprocesses (today: the fsmonitor change
//! watcher; tomorrow: the FUSE mount daemon) all share the same
//! plumbing — a JSON-over-TCP protocol on `127.0.0.1`, an endpoint
//! file at `.heddle/state/<name>.endpoint.json` that the spawning
//! CLI uses for discovery, atomic write-on-bind, idle-timeout exit,
//! crashed-PID detection via `kill -0`. The fsmonitor invented this
//! pattern; the mount daemon reuses it.
//!
//! Threat model: the endpoint file binds to localhost with no auth.
//! That matches the existing fsmonitor posture and assumes a
//! single-user dev workstation. Anyone with a shell on the box can
//! talk to the helper anyway; we are not a security boundary.
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
pub mod mount_proto;
pub mod protocol;
pub mod server;

pub use endpoint::{
    EndpointState, default_state_dir, endpoint_path_for, load_endpoint, persist_endpoint,
    pid_alive, remove_endpoint,
};
pub use mount_proto::{
    ERR_MOUNT_CONFLICT, ERR_MOUNT_UNSUPPORTED, ERR_VERSION_MISMATCH, MOUNT_PROTOCOL_VERSION,
    MountDaemonRequest, MountDaemonResponse, MountRegistryFile, MountStatus, PersistedMount,
    mount_daemon_endpoint_path, mount_daemon_registry_path,
};
pub use protocol::{HELPER_HOST, HELPER_IDLE_POLL_MS, HELPER_IDLE_TIMEOUT_SECS, send_json_request};
pub use server::{IdleDecision, mount_idle_policy, run_server_loop};
