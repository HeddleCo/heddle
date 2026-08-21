// SPDX-License-Identifier: Apache-2.0
//! Mount-daemon request dispatch. Auth is decided before any
//! `mount_path` is honored (heddle#901).

use std::{
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use objects::sync::LockExt;
use repo::daemon::{
    MOUNT_PROTOCOL_VERSION, MountClientAuth, MountDaemonRequest, MountDaemonResponse, MountStatus,
    authorize_mount_request, trusted_mount_path,
};

use super::registry::{MountOutcome, MountRegistry};

pub(super) fn dispatch(
    registry: &Mutex<MountRegistry>,
    started: Instant,
    shutdown_requested: &AtomicBool,
    auth: MountClientAuth,
    request: MountDaemonRequest,
) -> MountDaemonResponse {
    if let Err(denied) = authorize_mount_request(auth, &request) {
        return error_response(denied.code, denied.message);
    }
    match request {
        MountDaemonRequest::Mount {
            thread_id,
            mount_path,
            repo_root: _,
        } => dispatch_mount(registry, auth, thread_id, mount_path),
        MountDaemonRequest::Unmount { thread_id } => dispatch_unmount(registry, &thread_id),
        MountDaemonRequest::ListMounts {} => {
            let guard = registry.lock_or_poisoned();
            MountDaemonResponse::ListMounts {
                version: MOUNT_PROTOCOL_VERSION,
                mounts: guard.snapshot(),
            }
        }
        MountDaemonRequest::Health {} => {
            let guard = registry.lock_or_poisoned();
            MountDaemonResponse::Health {
                version: MOUNT_PROTOCOL_VERSION,
                ok: true,
                uptime_s: started.elapsed().as_secs(),
                mount_count: guard.len(),
            }
        }
        MountDaemonRequest::Shutdown {} => {
            shutdown_requested.store(true, Ordering::Release);
            MountDaemonResponse::Shutdown {
                version: MOUNT_PROTOCOL_VERSION,
                ok: true,
            }
        }
        MountDaemonRequest::Unknown => error_response(
            "unknown_command",
            "daemon received an unrecognized command (likely client/server skew)",
        ),
    }
}

fn dispatch_mount(
    registry: &Mutex<MountRegistry>,
    auth: MountClientAuth,
    thread_id: String,
    supplied: PathBuf,
) -> MountDaemonResponse {
    let mount_path = match trusted_mount_path(auth, &supplied) {
        Ok(path) => path.to_path_buf(),
        Err(denied) => return error_response(denied.code, denied.message),
    };
    let mut guard = registry.lock_or_poisoned();
    match guard.mount(&thread_id, &mount_path) {
        Ok(MountOutcome::Created) => MountDaemonResponse::Mount {
            version: MOUNT_PROTOCOL_VERSION,
            ok: true,
            mount_path,
            status: MountStatus::Created,
        },
        Ok(MountOutcome::Existing) => MountDaemonResponse::Mount {
            version: MOUNT_PROTOCOL_VERSION,
            ok: true,
            mount_path,
            status: MountStatus::AlreadyMounted,
        },
        Err(error) => MountDaemonResponse::Error {
            version: MOUNT_PROTOCOL_VERSION,
            code: repo::daemon::ERR_MOUNT_CONFLICT.to_string(),
            message: error.to_string(),
        },
    }
}

fn dispatch_unmount(registry: &Mutex<MountRegistry>, thread_id: &str) -> MountDaemonResponse {
    let mut guard = registry.lock_or_poisoned();
    match guard.unmount(thread_id) {
        Ok(was_mounted) => MountDaemonResponse::Unmount {
            version: MOUNT_PROTOCOL_VERSION,
            ok: true,
            was_mounted,
        },
        Err(error) => MountDaemonResponse::Error {
            version: MOUNT_PROTOCOL_VERSION,
            code: "unmount_failed".to_string(),
            message: error.to_string(),
        },
    }
}

fn error_response(code: impl Into<String>, message: impl Into<String>) -> MountDaemonResponse {
    MountDaemonResponse::Error {
        version: MOUNT_PROTOCOL_VERSION,
        code: code.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Mutex, atomic::AtomicBool},
        time::Instant,
    };

    use repo::daemon::{ERR_UNAUTHORIZED, MountClientAuth, MountDaemonRequest};

    use super::dispatch;
    use crate::cli::commands::daemon::registry::MountRegistry;

    #[test]
    fn unauthenticated_tcp_mount_is_rejected_without_touching_registry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = Mutex::new(MountRegistry::new(tmp.path().to_path_buf()));
        let response = dispatch(
            &registry,
            Instant::now(),
            &AtomicBool::new(false),
            MountClientAuth::Unauthenticated,
            MountDaemonRequest::Mount {
                thread_id: "agent-7".to_string(),
                mount_path: PathBuf::from("/tmp/evil"),
                repo_root: tmp.path().to_path_buf(),
            },
        );
        match response {
            repo::daemon::MountDaemonResponse::Error { code, .. } => {
                assert_eq!(code, ERR_UNAUTHORIZED);
            }
            other => panic!("expected unauthorized, got {other:?}"),
        }
        assert_eq!(registry.lock().unwrap().len(), 0);
    }
}
