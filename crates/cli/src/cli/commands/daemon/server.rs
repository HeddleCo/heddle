// SPDX-License-Identifier: Apache-2.0
//! Mount daemon server: glues the shared `repo::daemon` listener
//! loop to a `MountRegistry`. Linux + `--features mount` only.

use std::{
    os::unix::net::UnixStream,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use objects::{error::HeddleError, sync::LockExt};
use repo::daemon::{
    EndpointState, IdleDecision, MOUNT_PROTOCOL_VERSION, MountClientAuth, MountDaemonRequest,
    UnixDaemonHandler, bind_unix_socket, handle_authenticated_unix_connection,
    mount_daemon_endpoint_path, mount_daemon_socket_path, mount_idle_policy, persist_endpoint,
    remove_endpoint, run_unix_server_loop,
};
use tracing::info;

use super::{dispatch::dispatch, registry::MountRegistry};

/// Run the mount daemon for `repo_root` until idle. Binds a
/// mode-0600 Unix socket, writes the endpoint file, listens for
/// same-uid peers. Exits when both:
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

    let socket_path = mount_daemon_socket_path(repo_root);
    let listener = bind_unix_socket(&socket_path).context("bind mount daemon socket")?;
    persist_endpoint(
        &endpoint_path,
        &EndpointState {
            version: MOUNT_PROTOCOL_VERSION,
            host: "unix".to_string(),
            port: 0,
            pid: Some(std::process::id()),
            socket_path: Some(socket_path.clone()),
            node_id: None,
        },
    )
    .context("persist daemon endpoint")?;
    info!(
        socket = %socket_path.display(),
        pid = std::process::id(),
        "heddle daemon serving"
    );

    let registry = Arc::new(Mutex::new(MountRegistry::new(repo_root.to_path_buf())));
    let started = Instant::now();
    let shutdown_requested = Arc::new(AtomicBool::new(false));

    let mut handler = MountDaemonHandler {
        registry: Arc::clone(&registry),
        started,
        shutdown_requested: Arc::clone(&shutdown_requested),
    };
    let result = run_unix_server_loop(&listener, &mut handler);

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
        let mut guard = registry.lock_or_poisoned();
        guard.shutdown_all();
    }
    remove_endpoint(&endpoint_path);
    let _ = std::fs::remove_file(&socket_path);
    info!("heddle daemon exiting");
    result.map_err(Into::into)
}

struct MountDaemonHandler {
    registry: Arc<Mutex<MountRegistry>>,
    started: Instant,
    shutdown_requested: Arc<AtomicBool>,
}

impl UnixDaemonHandler for MountDaemonHandler {
    fn handle(&mut self, stream: UnixStream) -> Result<(), HeddleError> {
        // Capture state before the move so the closure body can
        // borrow them without lifetime headaches.
        let registry = Arc::clone(&self.registry);
        let started = self.started;
        let shutdown_requested = Arc::clone(&self.shutdown_requested);
        handle_authenticated_unix_connection(stream, move |request: MountDaemonRequest| {
            dispatch(
                &registry,
                started,
                &shutdown_requested,
                MountClientAuth::SameUid,
                request,
            )
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
        let live_count = self.registry.lock_or_poisoned().len();
        mount_idle_policy(shutdown, live_count, idle_for)
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
