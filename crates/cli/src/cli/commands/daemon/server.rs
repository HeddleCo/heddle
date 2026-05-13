// SPDX-License-Identifier: Apache-2.0
//! Mount daemon server: glues the shared `repo::daemon` listener
//! loop to a `MountRegistry`. Linux + `--features mount` only.

use std::{
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use objects::error::HeddleError;
use repo::daemon::{
    EndpointState, HELPER_HOST, MOUNT_PROTOCOL_VERSION, MountDaemonRequest, MountDaemonResponse,
    MountStatus, mount_daemon_endpoint_path, mount_idle_policy, persist_endpoint, remove_endpoint,
    server::{DaemonHandler, IdleDecision, handle_json_connection},
};
use tracing::info;

use super::registry::{MountOutcome, MountRegistry};

/// Run the mount daemon for `repo_root` until idle. Binds a
/// localhost TCP port, writes the endpoint file, listens for
/// connections. Exits when both:
///
/// * no RPC has arrived for `HELPER_IDLE_TIMEOUT_SECS` (default
///   300s, mirrors fsmonitor),
/// * AND the mount registry is empty.
///
/// The "and registry empty" gate is the load-bearing change vs.
/// fsmonitor: if the daemon owns a live FUSE session, idle exit
/// would unmount the kernel mountpoint behind the user's back.
/// See `docs/design/mount-daemon.md` § "Lifecycle".
pub fn run_mount_daemon(repo_root: &Path) -> Result<()> {
    let endpoint_path = mount_daemon_endpoint_path(repo_root);
    if let Some(parent) = endpoint_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = TcpListener::bind((HELPER_HOST, 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    persist_endpoint(
        &endpoint_path,
        &EndpointState {
            version: MOUNT_PROTOCOL_VERSION,
            host: HELPER_HOST.to_string(),
            port,
            pid: Some(std::process::id()),
        },
    )
    .context("persist daemon endpoint")?;
    info!(port, pid = std::process::id(), "heddle daemon serving");

    let registry = Arc::new(Mutex::new(MountRegistry::new(repo_root.to_path_buf())));
    let started = Instant::now();
    let shutdown_requested = Arc::new(AtomicBool::new(false));

    let mut handler = MountDaemonHandler {
        registry: Arc::clone(&registry),
        started,
        shutdown_requested: Arc::clone(&shutdown_requested),
    };
    let result = repo::daemon::run_server_loop(&listener, &mut handler);

    // Cleanup ordering — load-bearing for the `cmd_daemon_stop`
    // post-condition documented on that function:
    //
    //   1. `shutdown_all()` drains every live FUSE session and then
    //      removes `mounts.json` (its final `fs::remove_file` after
    //      `persist()`). Errors during shutdown_all are warned in
    //      the method itself.
    //   2. `remove_endpoint()` removes `endpoint.json`.
    //
    // Therefore "endpoint.json absent" is a strict implication of
    // "mounts.json absent" *on the daemon side*. The CLI's
    // `sweep_stale_mounts` is a redundant safety net (and is
    // idempotent), so a CLI observing endpoint-gone after
    // `daemon stop` may treat mounts.json-gone as a hard
    // post-condition.
    {
        let mut guard = registry.lock().expect("mount registry lock");
        guard.shutdown_all();
    }
    remove_endpoint(&endpoint_path);
    info!("heddle daemon exiting");
    result.map_err(Into::into)
}

struct MountDaemonHandler {
    registry: Arc<Mutex<MountRegistry>>,
    started: Instant,
    shutdown_requested: Arc<AtomicBool>,
}

impl DaemonHandler for MountDaemonHandler {
    fn handle(&mut self, stream: TcpStream) -> Result<(), HeddleError> {
        // Capture state before the move so the closure body can
        // borrow them without lifetime headaches.
        let registry = Arc::clone(&self.registry);
        let started = self.started;
        let shutdown_requested = Arc::clone(&self.shutdown_requested);
        handle_json_connection(stream, move |request: MountDaemonRequest| {
            dispatch(&registry, started, &shutdown_requested, request)
        })
    }

    fn on_tick(&mut self, idle_for: Duration) -> IdleDecision {
        // Critical change vs. fsmonitor: stay alive while we own
        // any FUSE session, regardless of RPC inactivity. Without
        // this gate, idle exit would unmount the kernel mountpoint
        // behind the user's back. The decision logic itself lives
        // in `repo::daemon::mount_idle_policy` so the regression
        // tests can exercise it on every host.
        let shutdown = self.shutdown_requested.load(Ordering::Acquire);
        let live_count = self.registry.lock().expect("mount registry lock").len();
        mount_idle_policy(shutdown, live_count, idle_for)
    }
}

fn dispatch(
    registry: &Mutex<MountRegistry>,
    started: Instant,
    shutdown_requested: &AtomicBool,
    request: MountDaemonRequest,
) -> MountDaemonResponse {
    match request {
        MountDaemonRequest::Mount {
            thread_id,
            mount_path,
            repo_root: _,
        } => {
            let mut guard = registry.lock().expect("mount registry lock");
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
        MountDaemonRequest::Unmount { thread_id } => {
            let mut guard = registry.lock().expect("mount registry lock");
            match guard.unmount(&thread_id) {
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
        MountDaemonRequest::ListMounts {} => {
            let guard = registry.lock().expect("mount registry lock");
            MountDaemonResponse::ListMounts {
                version: MOUNT_PROTOCOL_VERSION,
                mounts: guard.snapshot(),
            }
        }
        MountDaemonRequest::Health {} => {
            let guard = registry.lock().expect("mount registry lock");
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
        MountDaemonRequest::Unknown => MountDaemonResponse::Error {
            version: MOUNT_PROTOCOL_VERSION,
            code: "unknown_command".to_string(),
            message: "daemon received an unrecognized command (likely client/server skew)"
                .to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    //! Tests that exercise the idle-exit policy *without* spinning
    //! up FUSE. The full Linux-only mount happy-path lives in
    //! `crates/cli/tests/multi_agent_worktrees/virtualized_mount.rs`
    //! (the existing integration test that already gates on
    //! `target_os = linux`); we don't duplicate it here.

    use std::time::Duration;

    use repo::daemon::HELPER_IDLE_TIMEOUT_SECS;
    use tempfile::TempDir;

    use super::*;

    /// Regression test: the daemon must NOT idle-exit while a mount
    /// is alive in the registry. Without the registry-empty gate,
    /// idle exit would unmount the kernel mountpoint behind the
    /// user's back.
    #[test]
    fn idle_exit_blocked_while_mount_is_live() {
        let tmp = TempDir::new().unwrap();
        let registry = Arc::new(Mutex::new(MountRegistry::new(tmp.path().to_path_buf())));
        // Manually inject an entry into the registry without
        // spawning a real FUSE session: the idle-exit decision
        // only inspects `is_empty`, not the FUSE session.
        registry
            .lock()
            .unwrap()
            .__test_inject_phantom_mount("phantom", tmp.path().to_path_buf());
        let mut handler = MountDaemonHandler {
            registry: Arc::clone(&registry),
            started: Instant::now(),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
        };
        // Way past the idle timeout — but the registry isn't empty,
        // so the daemon must keep going.
        let decision = handler.on_tick(Duration::from_secs(HELPER_IDLE_TIMEOUT_SECS * 10));
        assert_eq!(decision, IdleDecision::Continue);
    }

    /// Counter-test: with an empty registry the daemon does idle-exit
    /// per the original fsmonitor behaviour.
    #[test]
    fn idle_exit_when_registry_empty() {
        let tmp = TempDir::new().unwrap();
        let registry = Arc::new(Mutex::new(MountRegistry::new(tmp.path().to_path_buf())));
        let mut handler = MountDaemonHandler {
            registry: Arc::clone(&registry),
            started: Instant::now(),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
        };
        let decision = handler.on_tick(Duration::from_secs(HELPER_IDLE_TIMEOUT_SECS + 1));
        assert_eq!(decision, IdleDecision::Exit);
    }

    /// Shutdown RPC flips the atomic and the next idle tick exits.
    #[test]
    fn shutdown_request_short_circuits_idle_check() {
        let tmp = TempDir::new().unwrap();
        let registry = Arc::new(Mutex::new(MountRegistry::new(tmp.path().to_path_buf())));
        registry
            .lock()
            .unwrap()
            .__test_inject_phantom_mount("phantom", tmp.path().to_path_buf());
        let shutdown = Arc::new(AtomicBool::new(true));
        let mut handler = MountDaemonHandler {
            registry: Arc::clone(&registry),
            started: Instant::now(),
            shutdown_requested: Arc::clone(&shutdown),
        };
        // Even with a phantom mount in the registry, an explicit
        // shutdown overrides the live-mount keep-alive.
        let decision = handler.on_tick(Duration::from_millis(0));
        assert_eq!(decision, IdleDecision::Exit);
    }
}