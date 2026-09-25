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
//! What it owns (heddle#1533, piece 1, plus the hosted warm-session
//! bridge):
//!
//! * one Iroh endpoint on the persisted device node id, bound with
//!   relays online (browsers holding only a claim link dial through a
//!   relay),
//! * a box-scoped endpoint-discovery file advertising that node id,
//! * a same-uid control socket for `netd status` / `netd stop`,
//! * a same-uid hosted bridge that caches Weft QUIC sessions for
//!   one-shot CLI verbs,
//! * a single-writer guard so two processes never both bind the
//!   device node id.
//!
//! What it does NOT own yet: the claim-ALPN router (piece 3 /
//! heddle#1620) mounts on the endpoint at the seam marked below, and
//! the weft subscription/doorbell (piece 2) is separate.

use std::{
    future::Future,
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
use tokio::sync::watch;
use tracing::info;

const KEEPALIVE_CADENCE: Duration = Duration::from_secs(120);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

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
    let (endpoint, hosted_sessions) = hosted_client::network::bind_persistent_hosted(
        hosted_client::network::default_relay_mode(),
    )
    .await
    .context("binding persistent device endpoint")?;
    let node_id = endpoint.id().to_string();
    let reachability_endpoint = endpoint.clone();
    let reachability_home = heddle_home.clone();
    let reachability = tokio::spawn(async move {
        if let Err(error) =
            hosted_client::network::advertise_reachability(reachability_endpoint, reachability_home)
                .await
        {
            tracing::warn!(%error, "device relay advertisement stopped");
        }
    });

    // ---- PIECE 3 (heddle#1620): mount the claim-ALPN router ----
    // The endpoint is live, relay-reachable, and pinned to the persisted
    // node id. Mount the persistent native v2 router on it and
    // serve its owner-root co-sign bridge for the daemon's lifetime.
    //
    // The daemon drives native endpoint discovery and claim admission
    // against the file-backed claim state, but it deliberately holds no
    // agent owner-root signer (decision D3): when a browser reaches the
    // owner-root co-sign, the router forwards it over `claim_socket` to a
    // foreground `heddle claim` process, which holds the signer and a live
    // HostedClient and completes the co-sign. The bridge task is aborted
    // and its socket removed in the cleanup block below; because the
    // router re-mounts on the persisted node id, a daemon restart mid-
    // window keeps outstanding claim links resolving.
    let claim_socket = hosted_client::network::claim_bridge_socket_path(&heddle_home);
    let claim_router = hosted_client::network::mount_claim_router(endpoint.clone());
    let claim_bridge = tokio::spawn(claim_router.serve_owner_root_bridge(claim_socket.clone()));

    // The persistent endpoint is already relay-homed. Keep one Weft QUIC
    // session per server and splice foreground v2 RPC streams over a same-uid
    // UDS, avoiding endpoint bind, relay dial, and QUIC handshake per command.
    let hosted_socket = hosted_client::network::hosted_bridge_socket_path(&heddle_home);
    let hosted_bridge = tokio::spawn(hosted_sessions.serve(hosted_socket.clone()));

    let (keepalive_stop, keepalive_stopped) = watch::channel(false);
    let keepalive = tokio::spawn(run_keepalive(
        tokio::time::sleep,
        hosted_client::network::authenticated_keepalive,
        keepalive_stopped,
        retry_jitter,
    ));

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

    let _ = keepalive_stop.send(true);
    let _ = keepalive.await;

    // Cleanup ordering: stop the claim bridge, close the endpoint (which
    // tears the router down), then unlink our own discovery file (only if
    // it still advertises us) and the sockets. `remove_endpoint_if_owned`
    // makes the unlink single-writer safe — a successor that raced in
    // keeps its file.
    claim_bridge.abort();
    hosted_bridge.abort();
    reachability.abort();
    let _ = reachability.await;
    if let Err(error) = hosted_client::network::remove_reachability(&heddle_home) {
        tracing::warn!(%error, "removing device relay advertisement");
    }
    if tokio::time::timeout(Duration::from_secs(3), endpoint.close())
        .await
        .is_err()
    {
        tracing::warn!("timed out closing netd endpoint");
    }
    remove_endpoint_if_owned(&endpoint_path, &advertised);
    let _ = std::fs::remove_file(&claim_socket);
    let _ = std::fs::remove_file(&hosted_socket);
    let _ = std::fs::remove_file(&socket_path);
    info!("heddle network daemon exiting");

    match loop_result {
        Ok(result) => result.map_err(Into::into),
        Err(join_error) => bail!("netd control loop panicked: {join_error}"),
    }
}

fn retry_jitter(delay: Duration) -> Duration {
    let mut random = [0_u8; 2];
    if let Err(error) = getrandom::fill(&mut random) {
        tracing::warn!(%error, "unable to jitter weft keepalive retry");
        return delay;
    }
    let percent = 80 + u64::from(u16::from_le_bytes(random)) % 41;
    delay.mul_f64(percent as f64 / 100.0)
}

fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs(15 * (1_u64 << failures.saturating_sub(1).min(3)))
}

async fn run_keepalive<Wait, WaitFuture, Call, CallFuture, Jitter>(
    mut wait: Wait,
    mut call: Call,
    mut stopped: watch::Receiver<bool>,
    mut jitter: Jitter,
) where
    Wait: FnMut(Duration) -> WaitFuture,
    WaitFuture: Future<Output = ()>,
    Call: FnMut() -> CallFuture,
    CallFuture: Future<Output = Result<()>>,
    Jitter: FnMut(Duration) -> Duration,
{
    let _ = (&mut call, &mut jitter);
    loop {
        tokio::select! {
            _ = wait(KEEPALIVE_CADENCE) => {},
            _ = stopped.changed() => return,
        }
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
        handle_authenticated_unix_connection(stream, move |request: NetworkDaemonRequest| {
            match request {
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
            }
        })
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

#[cfg(test)]
mod keepalive_tests {
    use std::sync::atomic::AtomicUsize;

    use tokio::sync::{Semaphore, mpsc};

    use super::*;

    async fn next<T>(receiver: &mut mpsc::UnboundedReceiver<T>) -> Option<T> {
        tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("keepalive event arrives")
    }

    #[tokio::test]
    async fn authenticated_call_repeats_every_two_minutes() {
        let (wait_tx, mut waits) = mpsc::unbounded_channel();
        let clock = Arc::new(Semaphore::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let (stop, stopped) = watch::channel(false);
        let call_count = Arc::clone(&calls);
        let tick_clock = Arc::clone(&clock);
        let task = tokio::spawn(run_keepalive(
            move |delay| {
                let _ = wait_tx.send(delay);
                let clock = Arc::clone(&tick_clock);
                async move {
                    if let Ok(permit) = clock.acquire().await {
                        permit.forget();
                    }
                }
            },
            move || {
                call_count.fetch_add(1, Ordering::Relaxed);
                async { Ok(()) }
            },
            stopped,
            |delay| delay,
        ));

        assert_eq!(next(&mut waits).await, Some(KEEPALIVE_CADENCE));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        clock.add_permits(1);
        assert_eq!(next(&mut waits).await, Some(KEEPALIVE_CADENCE));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        clock.add_permits(1);
        assert_eq!(next(&mut waits).await, Some(KEEPALIVE_CADENCE));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        let _ = stop.send(true);
        task.await.expect("keepalive stops");
    }

    #[tokio::test]
    async fn shutdown_cancels_an_in_flight_call() {
        let (wait_tx, mut waits) = mpsc::unbounded_channel();
        let clock = Arc::new(Semaphore::new(0));
        let tick_clock = Arc::clone(&clock);
        let (entered_tx, mut entered) = mpsc::unbounded_channel();
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(run_keepalive(
            move |delay| {
                let _ = wait_tx.send(delay);
                let clock = Arc::clone(&tick_clock);
                async move {
                    if let Ok(permit) = clock.acquire().await {
                        permit.forget();
                    }
                }
            },
            move || {
                let _ = entered_tx.send(());
                std::future::pending::<Result<()>>()
            },
            stopped,
            |delay| delay,
        ));

        assert_eq!(next(&mut waits).await, Some(KEEPALIVE_CADENCE));
        clock.add_permits(1);
        assert_eq!(next(&mut entered).await, Some(()));
        let _ = stop.send(true);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("shutdown must not wait for weft")
            .expect("keepalive stops");
    }

    #[tokio::test]
    async fn failure_retries_without_stalling_control_work() {
        let (wait_tx, mut waits) = mpsc::unbounded_channel();
        let clock = Arc::new(Semaphore::new(0));
        let tick_clock = Arc::clone(&clock);
        let calls = Arc::new(AtomicUsize::new(0));
        let call_count = Arc::clone(&calls);
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(run_keepalive(
            move |delay| {
                let _ = wait_tx.send(delay);
                let clock = Arc::clone(&tick_clock);
                async move {
                    if let Ok(permit) = clock.acquire().await {
                        permit.forget();
                    }
                }
            },
            move || {
                let attempt = call_count.fetch_add(1, Ordering::Relaxed);
                async move {
                    if attempt == 0 {
                        anyhow::bail!("transient weft failure");
                    }
                    Ok(())
                }
            },
            stopped,
            |delay| delay,
        ));

        assert_eq!(next(&mut waits).await, Some(KEEPALIVE_CADENCE));
        clock.add_permits(1);
        assert_eq!(next(&mut waits).await, Some(Duration::from_secs(15)));
        let (control_tx, control_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = control_tx.send(());
        });
        control_rx
            .await
            .expect("control work runs during retry wait");
        clock.add_permits(1);
        assert_eq!(next(&mut waits).await, Some(KEEPALIVE_CADENCE));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        let _ = stop.send(true);
        task.await.expect("keepalive stops");
    }
}
