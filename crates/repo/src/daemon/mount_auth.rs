// SPDX-License-Identifier: Apache-2.0
//! Fail-closed authorization for mount-daemon RPCs.
//!
//! Localhost TCP is not an authz boundary. A client-supplied
//! `mount_path` is honored only when the peer has the same uid the
//! UDS `SO_PEERCRED` path proves. Unauthenticated transports
//! (including the historical `127.0.0.1` listener) must refuse.

use std::path::Path;

use objects::error::HeddleError;

use super::mount_proto::MountDaemonRequest;

/// How the daemon authenticated the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountClientAuth {
    /// `SO_PEERCRED` / `getpeereid` proved the peer uid matches this process.
    SameUid,
    /// No peer identity. Localhost TCP is this. Fail closed.
    Unauthenticated,
}

/// Why a mount-daemon request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountAuthDenied {
    pub code: &'static str,
    pub message: String,
}

impl MountAuthDenied {
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            code: super::mount_proto::ERR_UNAUTHORIZED,
            message: message.into(),
        }
    }
}

impl From<MountAuthDenied> for HeddleError {
    fn from(denied: MountAuthDenied) -> Self {
        HeddleError::Config(format!("{}: {}", denied.code, denied.message))
    }
}

/// Honor a client-supplied `mount_path` only for same-uid peers.
pub fn trusted_mount_path(
    auth: MountClientAuth,
    supplied: &Path,
) -> Result<&Path, MountAuthDenied> {
    match auth {
        MountClientAuth::SameUid => Ok(supplied),
        MountClientAuth::Unauthenticated => Err(MountAuthDenied::unauthorized(
            "refusing client-supplied mount_path over an unauthenticated transport; same-uid UDS is required",
        )),
    }
}

/// Every mount-daemon verb needs same-uid proof. Health and list leak
/// mount paths; mount/unmount/shutdown are filesystem primitives.
pub fn authorize_mount_request(
    auth: MountClientAuth,
    request: &MountDaemonRequest,
) -> Result<(), MountAuthDenied> {
    match auth {
        MountClientAuth::SameUid => Ok(()),
        MountClientAuth::Unauthenticated => Err(MountAuthDenied::unauthorized(format!(
            "refusing {verb} over an unauthenticated transport; same-uid UDS is required",
            verb = request_verb(request)
        ))),
    }
}

fn request_verb(request: &MountDaemonRequest) -> &'static str {
    match request {
        MountDaemonRequest::Mount { .. } => "mount",
        MountDaemonRequest::Unmount { .. } => "unmount",
        MountDaemonRequest::ListMounts {} => "list_mounts",
        MountDaemonRequest::Health {} => "health",
        MountDaemonRequest::Shutdown {} => "shutdown",
        MountDaemonRequest::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{MountClientAuth, authorize_mount_request, trusted_mount_path};
    use crate::daemon::MountDaemonRequest;

    fn mount_request() -> MountDaemonRequest {
        MountDaemonRequest::Mount {
            thread_id: "agent-7".to_string(),
            mount_path: PathBuf::from("/tmp/evil"),
            repo_root: PathBuf::from("/tmp/repo"),
        }
    }

    #[test]
    fn unauthenticated_tcp_must_not_honor_client_mount_path() {
        let denied = trusted_mount_path(MountClientAuth::Unauthenticated, Path::new("/tmp/evil"));
        assert!(
            denied.is_err(),
            "localhost TCP must not honor a client-supplied mount_path"
        );
    }

    #[test]
    fn same_uid_peer_may_supply_mount_path() {
        let path = Path::new("/tmp/ok");
        let accepted = trusted_mount_path(MountClientAuth::SameUid, path)
            .expect("same-uid UDS may honor mount_path");
        assert_eq!(accepted, path);
    }

    #[test]
    fn unauthenticated_tcp_must_not_drive_mount_or_unmount() {
        let mount = authorize_mount_request(MountClientAuth::Unauthenticated, &mount_request());
        assert!(mount.is_err(), "unauthenticated mount must fail closed");

        let unmount = authorize_mount_request(
            MountClientAuth::Unauthenticated,
            &MountDaemonRequest::Unmount {
                thread_id: "agent-7".to_string(),
            },
        );
        assert!(unmount.is_err(), "unauthenticated unmount must fail closed");
    }

    #[test]
    fn same_uid_peer_is_authorized() {
        authorize_mount_request(MountClientAuth::SameUid, &mount_request())
            .expect("same-uid mount is allowed");
    }
}
