// SPDX-License-Identifier: Apache-2.0
//! Local-daemon auto-detection.
//!
//! Every gRPC-using verb in the CLI checks this first. When the per-repo
//! `.heddle/sockets/grpc.sock` exists and the pidfile points at a live
//! process, callers can route their RPC over the UDS instead of opening
//! an in-process [`GrpcLocalService`]. The latency win matters for tight
//! agent loops.
//!
//! Three layers:
//!
//! 1. [`detect_local_daemon`] — file-stat probe (pidfile + liveness via
//!    `kill(pid, 0)`). Cheap, syscall-only, used as the cheap negative
//!    case ("no daemon, fall through to in-process").
//! 2. [`detect_local_daemon_with_connect_probe`] — same as (1) but
//!    actually opens a `UnixStream` to confirm the listener accepts.
//!    Catches the "stale socket file with a live unrelated PID" race.
//! 3. [`connect_local_daemon_channel`] — full path: build a tonic
//!    [`tonic::transport::Channel`] over the UDS, run the gRPC
//!    `Health.Check` handshake, and cache the working channel for the
//!    rest of the process. This is what the read-shaped CLI verbs
//!    route through.
//!
//! All three caches are keyed by canonical heddle-dir path, so a CLI
//! invocation that touches one repo pays the probe cost exactly once.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use crate::util::OnceMap;

/// A reachable local daemon — the path of the UDS socket the caller
/// can connect to. Returned by [`detect_local_daemon`] when the probe
/// reports `Running`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdsTarget {
    pub socket_path: PathBuf,
    pub pid: u32,
}

/// Cache key — canonical heddle-dir path so two probes from different
/// CWDs against the same repo share a result.
type ProbeCacheKey = PathBuf;

/// Process-wide probe cache. Each heddle dir is probed at most once
/// per process lifetime, after which subsequent calls return the
/// cached `Option<UdsTarget>` without touching the filesystem.
///
/// Keyed by canonical heddle-dir path so a process that touches more
/// than one repo (test binaries, agent dispatch loops) caches each
/// repo independently.
static DETECT_CACHE: OnceMap<ProbeCacheKey, Option<UdsTarget>> = OnceMap::new();

/// Run the probe and, when the daemon is `Running`, return the UDS
/// target a tonic client can dial. Cached for the process lifetime so
/// hot agent loops don't pay two stat-syscalls per RPC.
///
/// Probing failure (`Absent`, `Stale`) returns `None` — the caller
/// should fall through to its in-process or remote fallback. The
/// full `Health.Check` + version handshake over a tonic UDS Channel
/// layers on top of [`detect_local_daemon_with_connect_probe`] and
/// [`connect_local_daemon_channel`].
pub fn detect_local_daemon(heddle_dir: &Path) -> Option<UdsTarget> {
    let key: ProbeCacheKey = heddle_dir.to_path_buf();
    DETECT_CACHE.get_or_init_with(&key, || {
        let probe = probe(heddle_dir);
        match probe.status {
            LocalDaemonStatus::Running { pid } => Some(UdsTarget {
                socket_path: probe.socket_path,
                pid,
            }),
            LocalDaemonStatus::Stale { .. } | LocalDaemonStatus::Absent => None,
        }
    })
}

/// Stronger variant of [`detect_local_daemon`] — runs the file-stat
/// probe, then attempts a UDS connect to confirm the daemon is
/// actually accepting connections (not just a stale pidfile that
/// happens to point at a live unrelated process).
///
/// The connect step is intentionally bounded by `timeout`. Default
/// callers should pass something tight (50ms is plenty for a local
/// socket) so a hung daemon doesn't stall every CLI invocation.
///
/// Returns `None` if either the file-stat probe says `Absent`/`Stale`
/// or the UDS connect fails / times out. Caches the *first* outcome
/// for the given heddle dir; subsequent calls are O(1).
#[cfg(unix)]
pub async fn detect_local_daemon_with_connect_probe(
    heddle_dir: &Path,
    timeout: Duration,
) -> Option<UdsTarget> {
    // The file-stat probe handles the cache and the obvious negative
    // cases; only the positive path needs the live connect.
    let target = detect_local_daemon(heddle_dir)?;
    match tokio::time::timeout(
        timeout,
        tokio::net::UnixStream::connect(&target.socket_path),
    )
    .await
    {
        Ok(Ok(_stream)) => Some(target),
        // Either the connect errored (socket present but listener
        // dead — rare but possible during a graceful shutdown) or
        // the connect timed out (daemon hung). Either way it's not
        // safe to route RPCs through it. The next probe will retry.
        Ok(Err(_)) | Err(_) => None,
    }
}

/// Process-wide cache of working tonic [`tonic::transport::Channel`]s
/// keyed by canonical heddle-dir path. Once we've successfully passed
/// the Health.Check handshake, every subsequent caller in the same
/// process gets the same channel (which is itself internally pooled
/// by tonic / hyper).
///
/// `Channel` is cheap to clone (it's a handle to the underlying
/// connection pool), so handing out clones is the fastest way to
/// amortize the connect cost across a hot agent loop. Keyed by
/// canonical heddle-dir path so a process touching multiple repos
/// (test binaries, multi-repo agents) keeps a per-repo channel.
#[cfg(unix)]
static CHANNEL_CACHE: OnceMap<ProbeCacheKey, tonic::transport::Channel> = OnceMap::new();

/// Connect-and-handshake outcome for [`connect_local_daemon_channel`].
///
/// `target` is repeated for the convenience of callers that only need
/// to know whether a daemon is reachable; the live `channel` is what
/// actually issues RPCs.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct LocalDaemonChannel {
    pub target: UdsTarget,
    pub channel: tonic::transport::Channel,
}

/// Build a tonic [`tonic::transport::Channel`] over the per-repo UDS,
/// perform the gRPC `Health.Check` handshake, and return both
/// alongside the [`UdsTarget`].
///
/// On the happy path the channel is cached in [`CHANNEL_CACHE`] for
/// the lifetime of this process and subsequent calls return clones of
/// it in O(1). This is the path agent loops should use.
///
/// `connect_timeout` bounds the UDS connect *and* the Health.Check —
/// 50–250ms is appropriate for a same-host socket; anything longer
/// implies a hung daemon. Returns `None` on any failure mode (no
/// daemon, connect failed, health refused) — the caller falls through
/// to its in-process or remote fallback.
///
/// # First consumer (TODO)
///
/// `cmd_status` is the natural first consumer — its read-shaped output
/// is built from a handful of `Repository` lookups that the
/// `OperationLogQueryService` already covers. A future patch should
/// branch in `crates/cli/src/cli/commands/status.rs::cmd_status` like:
///
/// ```ignore
/// if let Some(LocalDaemonChannel { channel, .. }) =
///     connect_local_daemon_channel(repo.heddle_dir(), Duration::from_millis(150)).await
/// {
///     let mut client = OperationLogQueryServiceClient::new(channel);
///     // Build StatusOutput from RPCs instead of direct Repository reads.
/// } else {
///     // Existing in-process path.
/// }
/// ```
///
/// Held back from this patch because (a) the query surface doesn't
/// yet cover every field in `StatusOutput`, and (b) the brief calls
/// out the channel-construction primitive as the deliverable.
#[cfg(unix)]
pub async fn connect_local_daemon_channel(
    heddle_dir: &Path,
    connect_timeout: Duration,
) -> Option<LocalDaemonChannel> {
    let key: ProbeCacheKey = heddle_dir.to_path_buf();
    if let Some(channel) = CHANNEL_CACHE.get(&key) {
        // The detect cache holds the matching target — pull it back
        // out so the returned struct stays self-contained.
        let target = detect_local_daemon(heddle_dir)?;
        return Some(LocalDaemonChannel { target, channel });
    }

    match build_channel(heddle_dir, connect_timeout).await {
        Ok(LocalDaemonChannel { target, channel }) => {
            CHANNEL_CACHE.insert(key, channel.clone());
            Some(LocalDaemonChannel { target, channel })
        }
        Err(_) => None,
    }
}

#[cfg(unix)]
async fn build_channel(
    heddle_dir: &Path,
    connect_timeout: Duration,
) -> std::result::Result<LocalDaemonChannel, ChannelError> {
    let target = detect_local_daemon(heddle_dir).ok_or(ChannelError::NoDaemon)?;
    // `unix:` URIs aren't usable as the *origin* on a HTTP/2 channel
    // (the authority pseudo-header has to be a plausible host). The
    // standard tonic UDS recipe is to give the endpoint an opaque
    // `http://heddle-uds` URI for routing and override the connector
    // with a service that returns a `UnixStream` regardless of what
    // URI it's asked for.
    let endpoint = tonic::transport::Endpoint::try_from("http://heddle-uds")
        .map_err(ChannelError::EndpointBuild)?
        .connect_timeout(connect_timeout);

    let socket_path = target.socket_path.clone();
    let connector = tower::service_fn(move |_uri: tonic::transport::Uri| {
        let socket_path = socket_path.clone();
        async move {
            let stream = tokio::net::UnixStream::connect(&socket_path).await?;
            // tonic 0.14 requires the connector's response type to
            // implement `hyper::rt::{Read, Write}`. `TokioIo` is the
            // standard adapter and it's what tonic's own UDS connector
            // uses internally — see
            // `tonic/src/transport/channel/uds_connector.rs`.
            std::io::Result::Ok(hyper_util::rt::TokioIo::new(stream))
        }
    });

    let channel = endpoint
        .connect_with_connector(connector)
        .await
        .map_err(ChannelError::Connect)?;

    // Health.Check is the version handshake. Today the local daemon
    // doesn't install a `tonic_health` reporter, so we expect either
    // `Ok(Serving)` or `Err(Unimplemented)` — the latter is treated
    // as "channel works, daemon predates the handshake" and accepted.
    // Any other error means the channel is wedged and we should fall
    // back to in-process.
    let mut health = tonic_health::pb::health_client::HealthClient::new(channel.clone());
    let request = tonic::Request::new(tonic_health::pb::HealthCheckRequest {
        // Empty service name → "is the whole server serving?" per the
        // gRPC health protocol spec.
        service: String::new(),
    });
    match tokio::time::timeout(connect_timeout, health.check(request)).await {
        Ok(Ok(response)) => {
            let status = response.into_inner().status;
            if status == tonic_health::pb::health_check_response::ServingStatus::Serving as i32 {
                Ok(LocalDaemonChannel { target, channel })
            } else {
                Err(ChannelError::HealthNotServing)
            }
        }
        // Unimplemented: daemon doesn't ship Health (today's case).
        // We still trust the connection — the underlying HTTP/2
        // handshake succeeded above, which is itself a strong signal.
        Ok(Err(status)) if status.code() == tonic::Code::Unimplemented => {
            Ok(LocalDaemonChannel { target, channel })
        }
        Ok(Err(status)) => Err(ChannelError::HealthRpc(status)),
        Err(_elapsed) => Err(ChannelError::HealthRpc(tonic::Status::deadline_exceeded(
            "Health.Check timed out",
        ))),
    }
}

/// Errors from the channel-build path. Kept private to the module —
/// callers see `Option<LocalDaemonChannel>` from
/// [`connect_local_daemon_channel`] and treat `None` as "no daemon,
/// fall through to in-process".
#[cfg(unix)]
#[derive(Debug)]
#[allow(dead_code)]
enum ChannelError {
    /// Detect probe said no daemon (cheap negative case).
    NoDaemon,
    /// Tonic refused to build the endpoint URI. Programmer error in
    /// practice, but we surface it for the test path.
    EndpointBuild(tonic::transport::Error),
    /// `connect_with_connector` failed — daemon not accepting.
    Connect(tonic::transport::Error),
    /// Health.Check round-trip failed (transport, codec, etc.).
    HealthRpc(tonic::Status),
    /// Health.Check came back with `NOT_SERVING`. We don't trust it.
    HealthNotServing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDaemonProbe {
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
    pub status: LocalDaemonStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalDaemonStatus {
    /// Socket and pidfile both exist and the pid is alive.
    Running { pid: u32 },
    /// Pidfile exists but the pid is dead. The socket may be a leftover.
    Stale { pid: u32 },
    /// No pidfile or socket.
    Absent,
}

/// Probe the per-repo daemon directory. Cheap (two file stats + one
/// `kill(pid, 0)`).
pub fn probe(heddle_dir: &Path) -> LocalDaemonProbe {
    let socket_path = heddle_dir.join("sockets").join("grpc.sock");
    let pid_path = heddle_dir.join("sockets").join("grpc.pid");
    let status = match read_pid(&pid_path) {
        Some(pid) if pid_alive(pid) => LocalDaemonStatus::Running { pid },
        Some(pid) => LocalDaemonStatus::Stale { pid },
        None => LocalDaemonStatus::Absent,
    };
    LocalDaemonProbe {
        socket_path,
        pid_path,
        status,
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    // The hardened pidfile written by `daemon::local_daemon` has three
    // lines: `<pid>\nheddle-agent\n<unix_secs>\n`. We only need the
    // first line for liveness checks. Parse the leading line, falling
    // back to the entire file (legacy single-line format) so older
    // pidfiles still resolve.
    let raw = std::fs::read_to_string(path).ok()?;
    let first = raw.lines().next().unwrap_or("").trim();
    first
        .parse::<u32>()
        .ok()
        .or_else(|| raw.trim().parse::<u32>().ok())
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) only validates existence; signal 0 sends nothing.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn absent_when_no_files() {
        let temp = TempDir::new().unwrap();
        let probe = probe(temp.path());
        assert_eq!(probe.status, LocalDaemonStatus::Absent);
    }

    #[test]
    fn stale_when_pidfile_holds_dead_pid() {
        let temp = TempDir::new().unwrap();
        let sockets = temp.path().join("sockets");
        std::fs::create_dir_all(&sockets).unwrap();
        // PID 2_147_483_646 is well beyond pid_max and not in use.
        std::fs::write(sockets.join("grpc.pid"), "2147483646").unwrap();
        let probe = probe(temp.path());
        assert!(matches!(probe.status, LocalDaemonStatus::Stale { .. }));
    }

    #[test]
    fn running_when_pidfile_holds_self_pid() {
        let temp = TempDir::new().unwrap();
        let sockets = temp.path().join("sockets");
        std::fs::create_dir_all(&sockets).unwrap();
        std::fs::write(sockets.join("grpc.pid"), std::process::id().to_string()).unwrap();
        let probe = probe(temp.path());
        match probe.status {
            LocalDaemonStatus::Running { pid } => assert_eq!(pid, std::process::id()),
            other => panic!("expected Running, got {other:?}"),
        }
    }

    #[test]
    fn detect_returns_target_when_running() {
        let temp = TempDir::new().unwrap();
        let sockets = temp.path().join("sockets");
        std::fs::create_dir_all(&sockets).unwrap();
        std::fs::write(sockets.join("grpc.pid"), std::process::id().to_string()).unwrap();
        let target = detect_local_daemon(temp.path()).expect("daemon detected");
        assert_eq!(target.pid, std::process::id());
        assert!(
            target.socket_path.ends_with("sockets/grpc.sock"),
            "socket path was {:?}",
            target.socket_path
        );
    }

    #[test]
    fn detect_returns_none_when_absent() {
        let temp = TempDir::new().unwrap();
        // A fresh temp dir with no `sockets/` subtree — probe returns
        // Absent, detect collapses that to None.
        assert!(detect_local_daemon(temp.path()).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connect_probe_rejects_socketless_pidfile() {
        // Pidfile points at our own pid (so the file-stat probe says
        // `Running`), but no listener is bound to the socket path.
        // The connect probe must catch this and return None — that's
        // its whole job.
        let temp = TempDir::new().unwrap();
        let sockets = temp.path().join("sockets");
        std::fs::create_dir_all(&sockets).unwrap();
        std::fs::write(sockets.join("grpc.pid"), std::process::id().to_string()).unwrap();
        let result = detect_local_daemon_with_connect_probe(
            temp.path(),
            std::time::Duration::from_millis(50),
        )
        .await;
        assert!(
            result.is_none(),
            "connect probe should reject when no listener is bound"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connect_probe_accepts_live_listener() {
        use tokio::net::UnixListener;
        let temp = TempDir::new().unwrap();
        let sockets = temp.path().join("sockets");
        std::fs::create_dir_all(&sockets).unwrap();
        let socket_path = sockets.join("grpc.sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        std::fs::write(sockets.join("grpc.pid"), std::process::id().to_string()).unwrap();
        let result = detect_local_daemon_with_connect_probe(
            temp.path(),
            std::time::Duration::from_millis(200),
        )
        .await;
        assert!(
            result.is_some(),
            "connect probe should succeed when a listener is bound"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connect_channel_is_none_when_daemon_absent() {
        // No pidfile, no socket — `connect_local_daemon_channel`
        // should short-circuit on the detect probe and return None
        // without attempting a connect.
        let temp = TempDir::new().unwrap();
        let result =
            connect_local_daemon_channel(temp.path(), std::time::Duration::from_millis(50)).await;
        assert!(result.is_none());
    }
}
