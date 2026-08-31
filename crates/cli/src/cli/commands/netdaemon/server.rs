// SPDX-License-Identifier: Apache-2.0
//! Box-scoped network daemon (`heddle netd serve`).
//!
//! A long-running, *async* daemon — sibling to the synchronous mount
//! daemon in `super::super::daemon::server` — that binds the
//! machine's single persistent Iroh endpoint on the persisted device
//! node id and keeps it relay-reachable. It is deliberately **not**
//! folded into the mount daemon's synchronous serve loop and **not**
//! gated on Linux/`--features mount`: the two daemons have different
//! lifecycles (this one never idle-exits) and different platforms
//! (this one runs on any Unix).
//!
//! What it owns (heddle#1533, piece 1):
//!
//! * one Iroh endpoint on the persisted device node id, bound with
//!   relays online (browsers holding only a claim link dial through a
//!   relay),
//! * a box-scoped endpoint-discovery file advertising that node id,
//! * a same-uid control socket for `netd status` / `netd stop`,
//! * a single-writer guard so two processes never both bind the
//!   device node id.
//!
//! What it does NOT own yet: the claim-ALPN router (piece 3 /
//! heddle#1620) mounts on the endpoint at the seam marked below, and
//! the weft subscription/doorbell (piece 2) is separate.

use std::{
    os::unix::net::UnixStream,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use objects::error::HeddleError;
use repo::daemon::{
    EndpointState, IdleDecision, UnixDaemonHandler, bind_unix_socket,
    handle_authenticated_unix_connection, load_endpoint, persist_endpoint, pid_alive,
    remove_endpoint_if_owned, run_unix_server_loop,
};
use tracing::info;

use super::proto::{
    NETWORK_DAEMON_PROTOCOL_VERSION, NetworkDaemonRequest, NetworkDaemonResponse,
    network_daemon_endpoint_path, network_daemon_socket_path,
};

/// Run the box network daemon in the foreground until an explicit
/// `netd stop`. Binds the control socket + the persisted-node-id
/// endpoint, publishes the discovery file, and serves same-uid
/// control RPCs. Never idle-exits.
pub async fn run_network_daemon() -> Result<()> {
    let heddle_home = repo::identity::heddle_home_dir();
    let endpoint_path = network_daemon_endpoint_path(&heddle_home);
    let socket_path = network_daemon_socket_path(&heddle_home);
    if let Some(parent) = endpoint_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating netd state directory {}", parent.display()))?;
    }

    // Single-writer guard #1: refuse if a live daemon already holds
    // the device node id (reuses the endpoint file's pid + `kill -0`).
    refuse_if_another_daemon_is_live(&endpoint_path)?;

    // Single-writer guard #2 (defense in depth): `bind_unix_socket`
    // fails closed on a live control socket, so even a torn discovery
    // file cannot let two daemons bind the same socket.
    let listener = bind_unix_socket(&socket_path).context("binding netd control socket")?;

    // Bind the machine's single persistent endpoint on the *persisted*
    // device node id, relays online. The node id therefore survives a
    // restart — the acceptance clause the browser claim link relies on
    // (heddle#1620).
    let endpoint = hosted_client::network::bind_persistent_endpoint(
        hosted_client::network::default_relay_mode(),
    )
    .await
    .context("binding persistent device endpoint")?;
    let node_id = endpoint.id().to_string();

    // ---- PIECE 3 (heddle#1620) MOUNT SEAM ----
    // The endpoint is live, relay-reachable, and pinned to the
    // persisted node id. Piece 3 mounts the claim protocol here,
    // without tearing this daemon down:
    //
    //     use hosted_client::network::Router;
    //     let router = Router::builder(endpoint.clone())
    //         .accept(CLAIM_ALPN_V1, claim_protocol)
    //         .spawn();
    //
    // Hold the returned `router` alongside `endpoint` for the daemon's
    // lifetime and shut it down in the cleanup block below.

    let advertised = EndpointState {
        version: NETWORK_DAEMON_PROTOCOL_VERSION,
        host: "iroh".to_string(),
        port: 0,
        pid: Some(std::process::id()),
        socket_path: Some(socket_path.clone()),
        node_id: Some(node_id.clone()),
    };
    persist_endpoint(&endpoint_path, &advertised).context("persisting netd endpoint discovery")?;
    info!(
        node_id = %node_id,
        socket = %socket_path.display(),
        pid = std::process::id(),
        "heddle network daemon serving"
    );

    let started = Instant::now();
    let shutdown = Arc::new(AtomicBool::new(false));

    // The control loop is a blocking accept loop. Run it on a blocking
    // thread so the current-thread runtime stays free to drive the
    // iroh endpoint's background tasks (relay keepalive, inbound
    // accepts) while we await shutdown.
    let loop_shutdown = Arc::clone(&shutdown);
    let control = tokio::task::spawn_blocking(move || {
        let mut handler = NetworkDaemonHandler {
            started,
            shutdown: loop_shutdown,
            node_id,
        };
        run_unix_server_loop(&listener, &mut handler)
    });

    let loop_result = control.await;

    // Cleanup ordering: close the endpoint first, then unlink our own
    // discovery file (only if it still advertises us) and the socket.
    // `remove_endpoint_if_owned` makes the unlink single-writer safe —
    // a successor that raced in keeps its file.
    endpoint.close().await;
    remove_endpoint_if_owned(&endpoint_path, &advertised);
    let _ = std::fs::remove_file(&socket_path);
    info!("heddle network daemon exiting");

    match loop_result {
        Ok(result) => result.map_err(Into::into),
        Err(join_error) => bail!("netd control loop panicked: {join_error}"),
    }
}

/// Refuse to start when a live daemon already owns the device node id.
/// Reuses the endpoint file's recorded pid + [`pid_alive`]; a stale
/// record left by a crashed daemon is unlinked (only if unchanged
/// since we read it) and start proceeds.
fn refuse_if_another_daemon_is_live(endpoint_path: &Path) -> Result<()> {
    let Ok(existing) = load_endpoint(endpoint_path) else {
        return Ok(());
    };
    if let Some(pid) = existing.pid
        && pid_alive(pid)
    {
        bail!(
            "a heddle network daemon is already serving (pid {pid}); \
             refusing to bind a second endpoint on the device node id"
        );
    }
    // Stale record from a crashed daemon — reclaim it, single-writer safe.
    remove_endpoint_if_owned(endpoint_path, &existing);
    Ok(())
}

struct NetworkDaemonHandler {
    started: Instant,
    shutdown: Arc<AtomicBool>,
    node_id: String,
}

impl UnixDaemonHandler for NetworkDaemonHandler {
    fn handle(&mut self, stream: UnixStream) -> Result<(), HeddleError> {
        let started = self.started;
        let node_id = self.node_id.clone();
        let shutdown = Arc::clone(&self.shutdown);
        handle_authenticated_unix_connection(
            stream,
            move |request: NetworkDaemonRequest| match request {
                NetworkDaemonRequest::Health {} => NetworkDaemonResponse::Health {
                    version: NETWORK_DAEMON_PROTOCOL_VERSION,
                    ok: true,
                    uptime_s: started.elapsed().as_secs(),
                    node_id,
                },
                NetworkDaemonRequest::Shutdown {} => {
                    shutdown.store(true, Ordering::Release);
                    NetworkDaemonResponse::Shutdown {
                        version: NETWORK_DAEMON_PROTOCOL_VERSION,
                        ok: true,
                    }
                }
            },
        )
    }

    fn on_tick(&mut self, _idle_for: Duration) -> IdleDecision {
        // No idle-exit. Unlike the mount daemon (300s idle timeout),
        // this endpoint must stay bound and relay-reachable so
        // outstanding claim URLs keep resolving; it exits only on an
        // explicit `netd stop`.
        if self.shutdown.load(Ordering::Acquire) {
            IdleDecision::Exit
        } else {
            IdleDecision::Continue
        }
    }
}
