// SPDX-License-Identifier: Apache-2.0
//! Local-mode gRPC daemon over a Unix-domain socket.
//!
//! Hosts the W2 [`grpc_local_impl`](crate::grpc_local_impl) services on
//! a UDS inside a single repo, reachable by the same-user CLI for the
//! latency-sensitive agent loop. No Biscuit, no TLS, no multi-tenant —
//! local-only, single-user, same-process auth via SO_PEERCRED on Linux and
//! `getpeereid` on macOS.
//!
//! The CLI wraps this behind `heddle agent serve` (W2 / A16). Out of scope
//! for first ship: multi-user, remote daemon-as-service, TLS. Documented
//! in the verb's `--help` long form.
//!
//! # Lifecycle
//!
//! 1. `serve(...)` opens the [`Repository`], the [`OperationDedupStore`],
//!    and the UDS listener.
//! 2. A pidfile and the socket path are guarded by [`PidGuard`] so a stale
//!    daemon's leftover files don't block restart and a clean exit removes
//!    them.
//! 3. tonic's [`Server::serve_with_shutdown`] runs the W2 services until the
//!    `shutdown` future resolves.
//!
//! # Cross-platform notes
//!
//! Building the daemon binary on Windows is not supported — UDS support
//! there is nascent. The module compiles only on `unix` and the rest of the
//! crate doesn't reach for it on other platforms.

#![cfg(unix)]

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use grpc::{
    DiscussionServiceServer, HookServiceServer, OperationLogQueryServiceServer,
    SignalServiceServer, StateReviewServiceServer, TimelineServiceServer, TransactionServiceServer,
};
use objects::error::{HeddleError, Result};
use repo::{Repository, operation_dedup::OperationDedupStore};
use tokio::net::UnixListener;
use tokio_stream::{StreamExt, wrappers::UnixListenerStream};
use tonic::transport::Server;

use crate::grpc_local_impl::{
    GrpcLocalService, LocalDiscussionService, LocalHookService, LocalOperationLogQueryService,
    LocalSignalService, LocalStateReviewService, LocalTimelineService, LocalTransactionService,
};

const PRIVATE_SOCKET_UMASK: libc::mode_t = 0o177;

static SOCKET_BIND_UMASK_LOCK: Mutex<()> = Mutex::new(());

/// Default socket path inside a repo: `<heddle_dir>/sockets/grpc.sock`.
pub fn default_socket_path(heddle_dir: &Path) -> PathBuf {
    heddle_dir.join("sockets").join("grpc.sock")
}

/// Default pidfile path inside a repo: `<heddle_dir>/sockets/grpc.pid`.
pub fn default_pid_path(heddle_dir: &Path) -> PathBuf {
    heddle_dir.join("sockets").join("grpc.pid")
}

/// Configuration for [`serve`]. The socket and pidfile default to the
/// well-known locations under the repo's `.heddle/sockets/` directory.
pub struct LocalDaemonConfig {
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
}

impl LocalDaemonConfig {
    pub fn from_repo(repo: &Repository) -> Self {
        let heddle_dir = repo.heddle_dir();
        Self {
            socket_path: default_socket_path(heddle_dir),
            pid_path: default_pid_path(heddle_dir),
        }
    }

    pub fn with_socket(mut self, path: PathBuf) -> Self {
        self.socket_path = path;
        self
    }
}

/// RAII guard that removes the pidfile and socket on drop. Constructed by
/// [`serve`]; callers don't typically use it directly.
struct PidGuard {
    pid_path: PathBuf,
    socket_path: PathBuf,
}

/// Magic marker line written to the pidfile so `heddle agent stop` can
/// distinguish a heddle pidfile from a foreign one before signalling the
/// PID. See [`PidFileContents`] for the on-disk format.
pub const PIDFILE_MARKER: &str = "heddle-agent";

/// Parsed pidfile contents. Format on disk is three newline-terminated
/// lines:
///
/// ```text
/// <pid>
/// heddle-agent
/// <start_time_unix_secs>
/// ```
///
/// The marker line lets `agent stop` reject a pidfile that wasn't written
/// by us. Combined with the same-executable identity check in
/// [`is_heddle_process`], this closes the "PID got reused after a dirty
/// crash" hole that the reviewer flagged: even if `<pid>` now belongs to
/// some unrelated process, we won't SIGTERM it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PidFileContents {
    pub pid: i32,
    pub started_at_secs: i64,
}

impl PidFileContents {
    /// Render the file body. Always trailing-newline so `cat` round-trips.
    pub fn render(&self) -> String {
        format!(
            "{}\n{}\n{}\n",
            self.pid, PIDFILE_MARKER, self.started_at_secs
        )
    }

    /// Parse a pidfile body. Returns `None` when the file isn't in the
    /// heddle format — the caller should treat this as "not a heddle
    /// pidfile" and refuse to act on it.
    pub fn parse(body: &str) -> Option<Self> {
        let mut lines = body.lines();
        let pid = lines.next()?.trim().parse::<i32>().ok()?;
        let marker = lines.next()?.trim();
        if marker != PIDFILE_MARKER {
            return None;
        }
        let started_at_secs = lines.next()?.trim().parse::<i64>().ok()?;
        Some(Self {
            pid,
            started_at_secs,
        })
    }
}

impl PidGuard {
    fn install(pid_path: PathBuf, socket_path: PathBuf) -> Result<Self> {
        if let Some(parent) = pid_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // If a stale pidfile exists for a dead PID, clean both files and
        // proceed. If the PID is alive AND the file contains our marker
        // AND the running process is this exact executable, refuse to
        // start. A foreign-format pidfile is treated as stale (we wrote
        // it, or it's debris) — we don't want to refuse forever because
        // some other tool dropped a file with the same name.
        if pid_path.exists() {
            let raw = std::fs::read_to_string(&pid_path).ok();
            let parsed = raw.as_deref().and_then(PidFileContents::parse);
            if let Some(existing) = parsed
                && pid_alive(existing.pid)
                && is_heddle_process(existing.pid)
            {
                return Err(HeddleError::Conflict(format!(
                    "heddle agent serve already running on this repo (pid {}); \
                     stop it first or remove {} if it's stale",
                    existing.pid,
                    pid_path.display()
                )));
            }
            // Stale or foreign pidfile; sweep both files.
            let _ = std::fs::remove_file(&pid_path);
            if socket_path.exists() {
                let _ = std::fs::remove_file(&socket_path);
            }
        }
        // Write our own pidfile in the (pid, marker, start_time) format.
        let contents = PidFileContents {
            pid: std::process::id() as i32,
            started_at_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        };
        std::fs::write(&pid_path, contents.render())?;
        Ok(Self {
            pid_path,
            socket_path,
        })
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.pid_path);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn pid_alive(pid: i32) -> bool {
    // SAFETY: kill(pid, 0) returns 0 on permission-checked success and -1
    // (errno = ESRCH) when the process no longer exists. No signal is
    // delivered with sig == 0.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn pid_alive(_pid: i32) -> bool {
    // Conservative fallback for other unixes: assume the pidfile is fresh
    // rather than blowing it away. Operators can `--force-clear` later.
    true
}

/// Verify that `pid` belongs to the same executable as this process.
///
/// The pidfile marker alone doesn't protect against the "daemon dies
/// uncleanly, OS reuses the PID" case the reviewer flagged: the marker
/// stays in the file but the PID now points at someone else. So before
/// any signal is delivered we also verify that the process at `pid` is
/// running this exact executable, not merely a path containing "heddle".
///
/// On Linux we read the `/proc/{pid}/exe` symlink — the kernel resolves
/// it to the absolute on-disk path of the running binary. On macOS we
/// use `libc::proc_pidpath`. On other platforms the check returns
/// `false` (operators on those platforms can use `--force-clear` to
/// override; better to refuse than to SIGTERM the wrong process).
pub fn is_heddle_process(pid: i32) -> bool {
    process_uid_matches_self(pid) && process_exe_matches_current(pid)
}

#[cfg(target_os = "linux")]
fn process_uid_matches_self(pid: i32) -> bool {
    use std::os::unix::fs::MetadataExt;

    let path = PathBuf::from(format!("/proc/{pid}"));
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    // SAFETY: geteuid() never fails.
    metadata.uid() == unsafe { libc::geteuid() }
}

#[cfg(not(target_os = "linux"))]
fn process_uid_matches_self(_pid: i32) -> bool {
    true
}

fn process_exe_matches_current(pid: i32) -> bool {
    let Some(process_exe) = process_exe_path(pid) else {
        return false;
    };
    let Ok(current_exe) = std::env::current_exe() else {
        return false;
    };
    executable_identity_matches(&process_exe, &current_exe)
}

fn executable_identity_matches(process_exe: &Path, current_exe: &Path) -> bool {
    let Ok(process_exe) = process_exe.canonicalize() else {
        return false;
    };
    let Ok(current_exe) = current_exe.canonicalize() else {
        return false;
    };
    process_exe == current_exe
}

#[cfg(target_os = "linux")]
fn process_exe_path(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(target_os = "macos")]
fn process_exe_path(pid: i32) -> Option<PathBuf> {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: buf is owned and large enough per the macOS contract.
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut _, buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(
        buf[..len as usize].to_vec(),
    )))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_exe_path(_pid: i32) -> Option<PathBuf> {
    None
}

/// Open a [`Repository`] at `repo_path`, then run the local gRPC daemon
/// over the configured UDS until `shutdown` resolves.
pub async fn serve(
    repo: Repository,
    config: LocalDaemonConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    create_private_socket_parent(&config.socket_path)?;
    // PidGuard refuses to start when another daemon owns this repo.
    let _guard = PidGuard::install(config.pid_path.clone(), config.socket_path.clone())?;

    // Remove any stale socket left by a non-graceful previous exit. The
    // pidfile check above ruled out a live owner.
    if config.socket_path.exists() {
        std::fs::remove_file(&config.socket_path)?;
    }
    let listener = bind_private_unix_listener(&config.socket_path)?;
    // Mode 0600 — same-user only. The umask guard in
    // `bind_private_unix_listener` makes the socket born private; this chmod
    // remains defense-in-depth and normalizes platforms that report mode bits
    // differently after bind.
    set_socket_mode_0600(&config.socket_path)?;
    listener.set_nonblocking(true).map_err(|e| {
        HeddleError::Io(std::io::Error::new(
            e.kind(),
            format!(
                "UnixListener::set_nonblocking({}): {e}",
                config.socket_path.display()
            ),
        ))
    })?;
    let listener = UnixListener::from_std(listener).map_err(|e| {
        HeddleError::Io(std::io::Error::new(
            e.kind(),
            format!(
                "UnixListener::from_std({}): {e}",
                config.socket_path.display()
            ),
        ))
    })?;

    // Crash recovery for the transaction sentinel directory. Runs before
    // any service starts handling RPCs so an in-flight transaction from a
    // prior `kill -9` cannot race with a brand-new `begin_transaction`.
    // See [`crate::transaction_replay`] for the state machine.
    //
    // Log level reflects the failure shape, not just "did anything
    // happen": clean recoveries are operator-informational (info), but
    // when `scan_error` or `failed_oplog_appends` is set the pass either
    // never ran or permanently lost an audit-trail entry (error), and
    // every other non-clean tail — failed sentinel rewrites, undeletable
    // orphan tmps, unparseable sentinels, unreadable directory entries
    // — needs operator attention even though the next startup may
    // retry (warn). The original info-level branch implied "recovered
    // prior in-flight state" even when scan/write errors meant recovery
    // had stalled, which gave operators false assurance during triage.
    let report = crate::transaction_replay::replay_active_transactions(&repo);
    if report.has_hard_failures() {
        tracing::error!(
            recovered_txns = report.recovered_transaction_ids.len(),
            orphan_tmps = report.orphan_temp_files_removed,
            unparseable = report.unparseable_sentinels.len(),
            failed_sentinel_writes = report.failed_sentinel_writes.len(),
            failed_orphan_deletes = report.failed_orphan_deletes.len(),
            failed_oplog_appends = report.failed_oplog_appends.len(),
            unreadable_entries = report.unreadable_entries,
            scan_error = report.scan_error.as_deref().unwrap_or(""),
            "local-daemon: transaction replay hit hard failures; \
             scan may not have run or audit-trail entries were lost"
        );
    } else if report.has_recoverable_failures() {
        tracing::warn!(
            recovered_txns = report.recovered_transaction_ids.len(),
            orphan_tmps = report.orphan_temp_files_removed,
            unparseable = report.unparseable_sentinels.len(),
            failed_sentinel_writes = report.failed_sentinel_writes.len(),
            failed_orphan_deletes = report.failed_orphan_deletes.len(),
            unreadable_entries = report.unreadable_entries,
            "local-daemon: transaction replay left recoverable failures on disk; \
             next startup will retry, but operator inspection is recommended"
        );
    } else if !report.is_clean() {
        tracing::info!(
            recovered_txns = report.recovered_transaction_ids.len(),
            orphan_tmps = report.orphan_temp_files_removed,
            "local-daemon: transaction replay recovered prior in-flight state"
        );
    }

    let dedup = Arc::new(OperationDedupStore::open(repo.heddle_dir())?);
    let inner = GrpcLocalService::new(Arc::new(repo), dedup);

    let state_review = StateReviewServiceServer::new(LocalStateReviewService::new(inner.clone()));
    let discussion = DiscussionServiceServer::new(LocalDiscussionService::new(inner.clone()));
    let signal = SignalServiceServer::new(LocalSignalService::new(inner.clone()));
    let query =
        OperationLogQueryServiceServer::new(LocalOperationLogQueryService::new(inner.clone()));
    let timeline = TimelineServiceServer::new(LocalTimelineService::new(inner.clone()));
    let transaction = TransactionServiceServer::new(LocalTransactionService::new(inner.clone()));
    let hook = HookServiceServer::new(LocalHookService::new(inner));

    // Per-connection SO_PEERCRED gate. Mode 0600 already keeps other users
    // from opening the socket; this filter is the defense-in-depth layer
    // the module docs promise — every accepted connection is checked
    // against the daemon's uid (and dropped on mismatch) before tonic ever
    // sees it.
    let incoming = UnixListenerStream::new(listener).filter_map(guard_peer_connection);

    Server::builder()
        .add_service(state_review)
        .add_service(discussion)
        .add_service(signal)
        .add_service(query)
        .add_service(timeline)
        .add_service(transaction)
        .add_service(hook)
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await
        .map_err(|e| HeddleError::InvalidObject(format!("local daemon transport failed: {e}")))?;
    Ok(())
}

fn create_private_socket_parent(socket_path: &Path) -> Result<()> {
    if let Some(parent) = socket_path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(parent)?;
    }
    Ok(())
}

fn bind_private_unix_listener(socket_path: &Path) -> Result<std::os::unix::net::UnixListener> {
    let _lock = SOCKET_BIND_UMASK_LOCK
        .lock()
        .map_err(|_| HeddleError::InvalidObject("daemon socket umask lock poisoned".to_string()))?;
    // Pathname UDS permissions are chosen by the kernel at bind time from
    // the process umask, so chmod-after-bind is too late for security. This
    // async `serve` path performs all startup work synchronously before the
    // daemon accepts connections or spawns service work; no `.await` occurs
    // while the process-global umask is narrowed. The mutex serializes other
    // Heddle socket binds in this process, and the guard restores the prior
    // umask even when bind fails.
    let _umask = UmaskGuard::set(PRIVATE_SOCKET_UMASK);
    std::os::unix::net::UnixListener::bind(socket_path).map_err(|e| {
        HeddleError::Io(std::io::Error::new(
            e.kind(),
            format!("UnixListener::bind({}): {e}", socket_path.display()),
        ))
    })
}

struct UmaskGuard {
    previous: libc::mode_t,
}

impl UmaskGuard {
    fn set(mask: libc::mode_t) -> Self {
        // SAFETY: umask is process-global and always succeeds. The caller
        // keeps the guarded section synchronous and holds
        // SOCKET_BIND_UMASK_LOCK for Heddle's own socket-bind paths.
        let previous = unsafe { libc::umask(mask) };
        Self { previous }
    }
}

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        // SAFETY: restoring the previously returned umask is infallible.
        unsafe {
            libc::umask(self.previous);
        }
    }
}

#[cfg(unix)]
fn set_socket_mode_0600(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

/// Verify that a connecting peer's UID matches the daemon's own. Wired
/// into the [`serve`] accept path via [`guard_peer_connection`]: every UDS
/// connection passes through this check before tonic handles it. The
/// socket's mode 0600 already keeps other users from opening it; this
/// SO_PEERCRED check (`getpeereid` on macOS) is the defense-in-depth layer
/// that enforces the same-user boundary per-connection even if the socket
/// permissions were somehow widened.
pub fn check_peer_uid_matches_self(stream: &tokio::net::UnixStream) -> Result<()> {
    let creds = stream
        .peer_cred()
        .map_err(|e| HeddleError::InvalidObject(format!("peer_cred failed: {e}")))?;
    // SAFETY: geteuid() never fails.
    let our_uid = unsafe { libc::geteuid() };
    enforce_peer_uid(creds.uid(), our_uid)
}

/// Compare a peer's UID against the daemon's effective UID, erroring when
/// they differ. Split out from [`check_peer_uid_matches_self`] so the
/// accept/reject decision is unit-testable without a real cross-UID
/// connection (which would need root or a second user).
fn enforce_peer_uid(peer_uid: u32, our_uid: u32) -> Result<()> {
    if peer_uid != our_uid {
        return Err(HeddleError::Conflict(format!(
            "peer uid {peer_uid} does not match daemon uid {our_uid}"
        )));
    }
    Ok(())
}

/// Per-connection gate applied to the [`serve`] accept stream. Each
/// accepted UDS connection is checked with [`check_peer_uid_matches_self`];
/// a peer whose UID doesn't match the daemon's is dropped here (its
/// `UnixStream` is closed) and never handed to tonic. Listener-level I/O
/// errors pass through unchanged so tonic's own error handling still runs.
fn guard_peer_connection(
    conn: std::io::Result<tokio::net::UnixStream>,
) -> Option<std::io::Result<tokio::net::UnixStream>> {
    match conn {
        Ok(stream) => match check_peer_uid_matches_self(&stream) {
            Ok(()) => Some(Ok(stream)),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "local-daemon: rejecting connection from peer with mismatched uid"
                );
                None
            }
        },
        Err(e) => Some(Err(e)),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    #[serial_test::serial(process_global)]
    fn default_socket_path_lives_under_heddle_dir() {
        let temp = TempDir::new().unwrap();
        let heddle = temp.path().join(".heddle");
        std::fs::create_dir_all(&heddle).unwrap();
        let path = default_socket_path(&heddle);
        assert!(path.starts_with(&heddle));
        assert!(path.ends_with("grpc.sock"));
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn create_private_socket_parent_creates_new_parent_0700() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let socket = temp
            .path()
            .join(".heddle")
            .join("sockets")
            .join("grpc.sock");
        create_private_socket_parent(&socket).unwrap();

        let mode = std::fs::metadata(socket.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "new socket parent must be private");
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn bind_private_unix_listener_creates_socket_0600_before_chmod() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("grpc.sock");

        let _listener = match bind_private_unix_listener(&socket) {
            Ok(listener) => listener,
            Err(HeddleError::Io(err)) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping daemon socket mode test: local Unix listener bind denied: {err}"
                );
                return;
            }
            Err(err) => panic!("bind private Unix listener: {err}"),
        };

        let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "socket must be born private before set_socket_mode_0600 runs"
        );
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn bind_private_unix_listener_restores_umask_after_bind_error() {
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("missing").join("grpc.sock");
        let before = current_umask();

        let result = bind_private_unix_listener(&socket);

        let after = current_umask();
        assert!(result.is_err(), "bind should fail for a missing parent");
        assert_eq!(after, before, "bind errors must restore the prior umask");
    }

    fn current_umask() -> libc::mode_t {
        // SAFETY: reading umask requires the standard set-then-restore
        // sequence; tests that call this are serialized with the bind tests.
        unsafe {
            let current = libc::umask(0);
            libc::umask(current);
            current
        }
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn pid_guard_writes_and_removes_pidfile() {
        let temp = TempDir::new().unwrap();
        let pid = temp.path().join("grpc.pid");
        let sock = temp.path().join("grpc.sock");
        let guard = PidGuard::install(pid.clone(), sock.clone()).unwrap();
        assert!(pid.exists());
        drop(guard);
        assert!(!pid.exists());
        assert!(!sock.exists());
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn pid_guard_refuses_when_live_heddle_process_owns_pidfile() {
        let temp = TempDir::new().unwrap();
        let pid = temp.path().join("grpc.pid");
        let sock = temp.path().join("grpc.sock");
        // Pre-installing a guard writes the current process PID with the
        // marker. A second install must refuse because the recorded PID is
        // alive and resolves to this exact test executable.
        let first = PidGuard::install(pid.clone(), sock.clone()).unwrap();
        let result = PidGuard::install(pid.clone(), sock.clone());
        assert!(result.is_err(), "expected refusal for live owner");
        drop(first);
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn pid_guard_sweeps_stale_pidfile_with_dead_pid() {
        let temp = TempDir::new().unwrap();
        let pid = temp.path().join("grpc.pid");
        let sock = temp.path().join("grpc.sock");
        // 2_147_483_646 is well above realistic pid_max; almost certainly dead.
        let stale = PidFileContents {
            pid: 2_147_483_646,
            started_at_secs: 0,
        };
        std::fs::write(&pid, stale.render()).unwrap();
        std::fs::write(&sock, "stale").unwrap();
        let _guard = PidGuard::install(pid.clone(), sock.clone()).unwrap();
        // The stale socket was removed and our PID is the new one.
        let raw = std::fs::read_to_string(&pid).unwrap();
        let parsed = PidFileContents::parse(&raw).expect("guard wrote structured pidfile");
        assert_eq!(parsed.pid, std::process::id() as i32);
        assert!(parsed.started_at_secs > 0);
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn pid_guard_sweeps_legacy_unstructured_pidfile() {
        // Pidfiles written by older daemons that pre-date the marker
        // are treated as foreign — the new `parse()` returns None and
        // the install path sweeps them rather than refusing forever.
        let temp = TempDir::new().unwrap();
        let pid = temp.path().join("grpc.pid");
        let sock = temp.path().join("grpc.sock");
        std::fs::write(&pid, "12345").unwrap();
        let _guard = PidGuard::install(pid.clone(), sock.clone()).unwrap();
        let parsed = PidFileContents::parse(&std::fs::read_to_string(&pid).unwrap()).unwrap();
        assert_eq!(parsed.pid, std::process::id() as i32);
    }

    #[test]
    fn pidfile_contents_round_trip() {
        let original = PidFileContents {
            pid: 4321,
            started_at_secs: 1_700_000_000,
        };
        let body = original.render();
        let parsed = PidFileContents::parse(&body).expect("round-trip");
        assert_eq!(parsed, original);
    }

    #[test]
    fn pidfile_contents_rejects_missing_marker() {
        // Same shape as the structured format but with the wrong marker
        // — must be rejected so we don't mistake a foreign file for ours.
        let body = "1234\nnot-heddle-agent\n100\n";
        assert!(PidFileContents::parse(body).is_none());
    }

    #[test]
    fn pidfile_contents_rejects_bare_pid() {
        // Legacy single-integer pidfile body. Parser refuses because it
        // can't verify the file is ours.
        assert!(PidFileContents::parse("12345").is_none());
    }

    #[test]
    fn executable_identity_accepts_same_canonical_path() {
        let current = std::env::current_exe().unwrap();
        assert!(executable_identity_matches(&current, &current));
    }

    #[test]
    fn executable_identity_rejects_spoofed_heddle_path() {
        let temp = TempDir::new().unwrap();
        let spoofed = temp.path().join("contains-heddle").join("heddle-spoof");
        std::fs::create_dir_all(spoofed.parent().unwrap()).unwrap();
        std::fs::write(&spoofed, "not the current executable").unwrap();

        let current = std::env::current_exe().unwrap();

        assert!(
            !executable_identity_matches(&spoofed, &current),
            "a pathname containing heddle must not satisfy executable identity"
        );
    }

    #[test]
    fn is_heddle_process_accepts_self_pid() {
        assert!(
            is_heddle_process(std::process::id() as i32),
            "the current process should resolve to the current executable"
        );
    }

    #[test]
    fn enforce_peer_uid_admits_matching_uid() {
        // The everyday case: the CLI and the daemon run as the same user,
        // so the peer's uid equals the daemon's. The gate must admit it.
        assert!(enforce_peer_uid(1000, 1000).is_ok());
    }

    #[test]
    fn enforce_peer_uid_rejects_mismatched_uid() {
        // A connection from a different uid (only reachable if the socket's
        // mode 0600 were somehow widened) must be refused with a Conflict.
        let err = enforce_peer_uid(1001, 1000).unwrap_err();
        assert!(
            matches!(err, HeddleError::Conflict(_)),
            "mismatched peer uid must be a Conflict, got {err:?}"
        );
    }

    #[test]
    fn guard_propagates_listener_io_errors() {
        // Listener-level accept() errors must reach tonic unchanged — the
        // peer-cred gate only drops mismatched peers, it never swallows
        // I/O errors that tonic's own error handling should see.
        let io_err = std::io::Error::other("accept failed");
        let out = guard_peer_connection(Err(io_err));
        assert!(matches!(out, Some(Err(_))), "io errors must propagate");
    }

    #[tokio::test]
    async fn guard_admits_same_process_peer() {
        // Both ends of a socketpair share this process's uid, so the gate
        // must admit the connection — proving the serve-path filter does
        // not reject the everyday same-user CLI connection.
        let (peer, _local) = tokio::net::UnixStream::pair().expect("socketpair");
        let out = guard_peer_connection(Ok(peer));
        assert!(
            matches!(out, Some(Ok(_))),
            "a same-uid peer must be admitted by the gate"
        );
    }

    #[tokio::test]
    async fn check_peer_uid_matches_self_admits_socketpair() {
        // Direct check on a real UnixStream: same-process socketpair ends
        // share our uid, so the SO_PEERCRED / getpeereid comparison passes.
        let (peer, _local) = tokio::net::UnixStream::pair().expect("socketpair");
        assert!(check_peer_uid_matches_self(&peer).is_ok());
    }
}
