// SPDX-License-Identifier: Apache-2.0
//! Mount-daemon wire protocol (v2).
//!
//! These types are the JSON shape exchanged between
//! `heddle <subcommand> --daemon` clients and the long-lived
//! `heddle daemon serve` process. The daemon itself lives in
//! `crates/cli` (which is the only crate that can import both
//! `repo` and `mount`), but the protocol shape lives here so the
//! CLI client side and tests can decode it without pulling the
//! mount stack in.
//!
//! Wire posture: localhost TCP, no auth — same threat model as
//! fsmonitor. Single-user dev workstation.
//!
//! Versioning: `MOUNT_PROTOCOL_VERSION` is bumped together with the
//! daemon binary. The endpoint file under
//! `.heddle/state/heddled.endpoint.json` records this number; CLI
//! clients that read a version they don't recognize remove the
//! file and respawn. fsmonitor's protocol stays at v1 on a
//! separate endpoint file (`monitor-helper.json`) — bumping there
//! is reserved for future breaking changes to the change-monitor
//! verbs.
//!
//! Endpoint file path: `.heddle/state/heddled.endpoint.json`
//! (resolved by [`mount_daemon_endpoint_path`]).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::endpoint::default_state_dir;

/// Wire-protocol version for the mount daemon. Bump in lockstep with
/// any breaking change to the request/response enums below. CLI
/// clients that find a different version on the endpoint file
/// remove the file and respawn the daemon at their version.
pub const MOUNT_PROTOCOL_VERSION: u32 = 2;

/// The endpoint file the mount daemon writes (and CLI clients read)
/// to discover its TCP port + PID.
pub fn mount_daemon_endpoint_path(repo_root: &Path) -> PathBuf {
    default_state_dir(repo_root).join("heddled.endpoint.json")
}

/// File where the daemon persists the list of currently-active
/// mounts. Used by the stale-endpoint sweep: when a CLI invocation
/// finds a dead daemon PID, it reads this file and runs
/// `fusermount -u` against each registered mount path before
/// respawning. Atomically rewritten on every mount/unmount.
pub fn mount_daemon_registry_path(repo_root: &Path) -> PathBuf {
    default_state_dir(repo_root).join("mounts.json")
}

/// Single mount entry persisted to disk + emitted from `list_mounts`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedMount {
    pub thread_id: String,
    pub mount_path: PathBuf,
    pub pid: u32,
    /// Wall-clock millis since UNIX epoch when the mount was
    /// established. Stable across restarts so `list_mounts` can
    /// answer "since" without keeping process-time state.
    pub since_ms: u64,
}

/// On-disk shape of `mounts.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MountRegistryFile {
    #[serde(default)]
    pub mounts: Vec<PersistedMount>,
}

/// Mount-daemon request envelope. Single-line JSON over TCP. Adding
/// a new verb is additive; every existing client can ignore unknown
/// verbs and the daemon sees the version mismatch first anyway.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum MountDaemonRequest {
    /// Mount `thread_id` at `mount_path` if not already mounted.
    /// Idempotent for the same `thread_id`/`mount_path` pair;
    /// conflicting paths return [`MountDaemonResponse::Error`] with
    /// `code = "mount_conflict"`.
    Mount {
        thread_id: String,
        mount_path: PathBuf,
        repo_root: PathBuf,
    },
    /// Tear down the mount for `thread_id` if present. Returns
    /// `was_mounted: false` (not an error) when there is nothing to
    /// unmount — same posture the in-process registry has today.
    Unmount { thread_id: String },
    /// Snapshot of the daemon's mount registry.
    ListMounts {},
    /// Lightweight liveness probe. Used by `heddle daemon status`
    /// and by CLI clients to decide whether to respawn after a
    /// version mismatch.
    Health {},
    /// Request that the daemon exit when it next idles. Used by
    /// `heddle daemon stop`. The daemon sweeps live mounts before
    /// exiting.
    Shutdown {},
    /// Sentinel used internally by the version-skew handler. Never
    /// sent by current clients but reserved so a future hello
    /// handshake stays additive.
    #[serde(other)]
    Unknown,
}

/// Mount-daemon response envelope. Always includes `version` so
/// older clients can detect skew and respawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MountDaemonResponse {
    Mount {
        version: u32,
        ok: bool,
        mount_path: PathBuf,
        status: MountStatus,
    },
    Unmount {
        version: u32,
        ok: bool,
        was_mounted: bool,
    },
    ListMounts {
        version: u32,
        mounts: Vec<PersistedMount>,
    },
    Health {
        version: u32,
        ok: bool,
        uptime_s: u64,
        mount_count: usize,
    },
    Shutdown {
        version: u32,
        ok: bool,
    },
    Error {
        version: u32,
        code: String,
        message: String,
    },
}

/// Disposition of a `mount` request: did we attach to an existing
/// mount, or did we create a new one?
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MountStatus {
    /// New FUSE session was established for this thread.
    Created,
    /// Same thread/mount_path pair was already mounted; the existing
    /// handle was returned. Idempotent success.
    AlreadyMounted,
}

impl MountDaemonResponse {
    pub fn version(&self) -> u32 {
        match self {
            MountDaemonResponse::Mount { version, .. }
            | MountDaemonResponse::Unmount { version, .. }
            | MountDaemonResponse::ListMounts { version, .. }
            | MountDaemonResponse::Health { version, .. }
            | MountDaemonResponse::Shutdown { version, .. }
            | MountDaemonResponse::Error { version, .. } => *version,
        }
    }
}

/// Standard error code used by clients to drop the endpoint file
/// and respawn the daemon. Emitted both when a v1 client speaks to
/// a v2 daemon and vice versa (the latter is detected on the
/// endpoint file before any RPC is sent).
pub const ERR_VERSION_MISMATCH: &str = "version_mismatch";

/// Standard error code for "thread X is already mounted at a
/// different path". Returned by `mount` when the daemon already
/// holds a session for this thread under a path that doesn't match
/// the request.
pub const ERR_MOUNT_CONFLICT: &str = "mount_conflict";

/// Standard error code surfaced when the daemon side's
/// platform/feature gate refuses to spin up a real mount (non-Linux
/// builds, or a Linux build without `--features mount`).
pub const ERR_MOUNT_UNSUPPORTED: &str = "mount_unsupported";

#[cfg(test)]
mod tests {
    //! Wire-protocol round-trip tests. These verify that:
    //!
    //! * Each request variant serialises to the expected JSON shape
    //!   (so a downgrade to a newer-by-one protocol stays additive).
    //! * Each response variant decodes correctly when the daemon
    //!   sends it back.
    //! * The endpoint and registry path conventions are stable.

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn mount_request_serialises_with_command_tag() {
        let request = MountDaemonRequest::Mount {
            thread_id: "agent-7".to_string(),
            mount_path: PathBuf::from("/tmp/foo"),
            repo_root: PathBuf::from("/tmp/repo"),
        };
        let serialised = serde_json::to_string(&request).unwrap();
        assert!(serialised.contains(r#""command":"mount""#));
        assert!(serialised.contains(r#""thread_id":"agent-7""#));
    }

    #[test]
    fn unmount_request_serialises() {
        let request = MountDaemonRequest::Unmount {
            thread_id: "agent-7".to_string(),
        };
        let serialised = serde_json::to_string(&request).unwrap();
        assert!(serialised.contains(r#""command":"unmount""#));
    }

    #[test]
    fn list_mounts_request_has_no_payload() {
        let request = MountDaemonRequest::ListMounts {};
        let serialised = serde_json::to_string(&request).unwrap();
        assert!(serialised.contains(r#""command":"list_mounts""#));
    }

    #[test]
    fn health_response_round_trips() {
        let response = MountDaemonResponse::Health {
            version: MOUNT_PROTOCOL_VERSION,
            ok: true,
            uptime_s: 42,
            mount_count: 3,
        };
        let s = serde_json::to_string(&response).unwrap();
        let decoded: MountDaemonResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(decoded.version(), MOUNT_PROTOCOL_VERSION);
        assert!(matches!(
            decoded,
            MountDaemonResponse::Health {
                uptime_s: 42,
                mount_count: 3,
                ..
            }
        ));
    }

    #[test]
    fn error_response_round_trips_with_code() {
        let response = MountDaemonResponse::Error {
            version: MOUNT_PROTOCOL_VERSION,
            code: ERR_MOUNT_CONFLICT.to_string(),
            message: "thread x is at /a, requested /b".to_string(),
        };
        let s = serde_json::to_string(&response).unwrap();
        let decoded: MountDaemonResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(decoded.version(), MOUNT_PROTOCOL_VERSION);
        match decoded {
            MountDaemonResponse::Error { code, .. } => assert_eq!(code, ERR_MOUNT_CONFLICT),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn endpoint_and_registry_paths_are_under_state_dir() {
        let tmp = TempDir::new().unwrap();
        let endpoint = mount_daemon_endpoint_path(tmp.path());
        let registry = mount_daemon_registry_path(tmp.path());
        assert!(endpoint.ends_with(".heddle/state/heddled.endpoint.json"));
        assert!(registry.ends_with(".heddle/state/mounts.json"));
    }

    #[test]
    fn mount_registry_file_round_trips_through_disk() {
        let registry = MountRegistryFile {
            mounts: vec![PersistedMount {
                thread_id: "alpha".to_string(),
                mount_path: PathBuf::from("/tmp/alpha"),
                pid: 1234,
                since_ms: 1_700_000_000_000,
            }],
        };
        let bytes = serde_json::to_vec_pretty(&registry).unwrap();
        let decoded: MountRegistryFile = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.mounts.len(), 1);
        assert_eq!(decoded.mounts[0].thread_id, "alpha");
        assert_eq!(decoded.mounts[0].pid, 1234);
    }

    /// Version-skew sentinel: a daemon-side response with a different
    /// version is still decodable so the client can read the version
    /// and decide to respawn. This is the contract the CLI client
    /// relies on in `daemon::client::read_live_endpoint`.
    #[test]
    fn response_with_unknown_version_still_decodes() {
        let raw = r#"{"kind":"health","version":99,"ok":true,"uptime_s":1,"mount_count":0}"#;
        let decoded: MountDaemonResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(decoded.version(), 99);
    }
}