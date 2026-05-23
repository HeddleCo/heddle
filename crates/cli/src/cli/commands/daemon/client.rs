// SPDX-License-Identifier: Apache-2.0
//! Mount-daemon client: discovers, spawns, and RPCs the daemon.
//!
//! Mirrors the fsmonitor's `try_local_helper_query` pattern:
//!
//! * Read the endpoint file.
//! * If it's missing or stale (PID dead via `kill -0`, or
//!   version-skew), sweep any leftover mounts and respawn
//!   `heddle daemon serve` detached.
//! * Send the request, decode the response.
//!
//! Pure-Rust, no new deps. Spawning uses `std::process::Command`
//! plus `setsid` on Linux (mirrors the fsmonitor pattern; see
//! [`spawn_daemon_detached`]).

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use repo::daemon::{
    ERR_MOUNT_UNSUPPORTED, EndpointState, MOUNT_PROTOCOL_VERSION, MountDaemonRequest,
    MountDaemonResponse, MountRegistryFile, load_endpoint, mount_daemon_endpoint_path,
    mount_daemon_registry_path, pid_alive, remove_endpoint, send_json_request,
};
use tracing::{debug, warn};

/// Outcome of a daemon mount attempt that distinguishes between
/// "the daemon couldn't service this request and we can fall back
/// to the in-process mount path" and "something went wrong that
/// the caller must surface".
///
/// The split exists because, post-default-flip, every
/// `--workspace virtualized` start tries the daemon first and
/// silently falls back when the daemon is unavailable on this
/// host (e.g. no `fusermount`, exec failed, daemon endpoint never
/// appeared). A `Fatal` error means the daemon *did* respond but
/// signalled a real problem (mount conflict, malformed reply,
/// version mismatch in flight), and we must not paper over it.
#[derive(Debug)]
pub enum DaemonMountError {
    /// The daemon could not be reached or could not service the
    /// request because of a host-environment reason (no daemon
    /// endpoint, spawn failed, daemon reports
    /// `ERR_MOUNT_UNSUPPORTED`). Safe to retry in-process. The
    /// `String` is a short reason suitable for a one-line warning
    /// log — it has already been formatted from the underlying
    /// error.
    Unavailable(String),
    /// The daemon rejected the request for a reason that points at
    /// a real bug or a real conflict (mount already held under a
    /// different path, malformed response, etc.). Surface to the
    /// caller — falling back to in-process would either hide the
    /// conflict or duplicate the mount.
    Fatal(anyhow::Error),
}

impl std::fmt::Display for DaemonMountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(f, "{reason}"),
            Self::Fatal(err) => write!(f, "{err}"),
        }
    }
}

/// How long we'll wait between checks for the endpoint file
/// appearing after we spawn the daemon. Mirrors fsmonitor.
const SPAWN_RETRY_DELAY_MS: u64 = 50;
const SPAWN_RETRIES: usize = 10;

/// Resolve a *currently live* daemon endpoint for `repo_root`. If
/// none is present we spawn one in the background and wait up to
/// ~500 ms for the endpoint file to appear. Returns `None` if the
/// caller asked us not to spawn (e.g. `daemon status`) and the
/// daemon isn't already running.
pub fn ensure_daemon_endpoint(
    repo_root: &Path,
    spawn_if_missing: bool,
) -> Result<Option<EndpointState>> {
    let endpoint_path = mount_daemon_endpoint_path(repo_root);

    if let Some(endpoint) = read_live_endpoint(&endpoint_path)? {
        return Ok(Some(endpoint));
    }
    // Endpoint absent or stale. Sweep before respawning so we don't
    // leave a wedged FUSE mount behind from the dead daemon.
    sweep_stale_mounts(repo_root);
    remove_endpoint(&endpoint_path);

    if !spawn_if_missing {
        return Ok(None);
    }

    spawn_daemon_detached(repo_root)?;
    for _ in 0..SPAWN_RETRIES {
        if let Some(endpoint) = read_live_endpoint(&endpoint_path)? {
            return Ok(Some(endpoint));
        }
        std::thread::sleep(Duration::from_millis(SPAWN_RETRY_DELAY_MS));
    }
    Err(anyhow!(
        "daemon endpoint never appeared at {}; check `heddle daemon serve` for errors",
        endpoint_path.display()
    ))
}

/// Read the endpoint file and decide whether to trust it. Returns
/// `Ok(None)` if the file is missing, the version doesn't match
/// what this CLI speaks, or the recorded PID is dead. Errors only
/// propagate when the file is unreadable for a reason other than
/// not-found.
fn read_live_endpoint(endpoint_path: &Path) -> Result<Option<EndpointState>> {
    let endpoint = match load_endpoint(endpoint_path) {
        Ok(endpoint) => endpoint,
        Err(objects::error::HeddleError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(None);
        }
        Err(error) => {
            warn!(%error, path = %endpoint_path.display(), "ignoring unreadable daemon endpoint");
            return Ok(None);
        }
    };
    if endpoint.version != MOUNT_PROTOCOL_VERSION {
        warn!(
            recorded = endpoint.version,
            expected = MOUNT_PROTOCOL_VERSION,
            "daemon version mismatch on endpoint file; treating as stale"
        );
        return Ok(None);
    }
    if let Some(pid) = endpoint.pid
        && !pid_alive(pid)
    {
        warn!(pid, "daemon PID is dead; treating endpoint as stale");
        return Ok(None);
    }
    Ok(Some(endpoint))
}

/// Best-effort cleanup pass: read `mounts.json`, run
/// `fusermount -u` against each registered mount path, then drop
/// the file. Errors are logged but never propagated — the caller
/// can still run; a wedged mount surfaces on the next CLI use as a
/// "wedged mount: run heddle thread drop --force ..." hint.
pub fn sweep_stale_mounts(repo_root: &Path) {
    let registry_path = mount_daemon_registry_path(repo_root);
    let Ok(contents) = fs::read_to_string(&registry_path) else {
        return;
    };
    let registry: MountRegistryFile = match serde_json::from_str(&contents) {
        Ok(registry) => registry,
        Err(error) => {
            warn!(%error, path = %registry_path.display(), "stale mount registry was unparseable; removing");
            let _ = fs::remove_file(&registry_path);
            return;
        }
    };
    for entry in &registry.mounts {
        debug!(thread = %entry.thread_id, path = %entry.mount_path.display(), "sweeping stale mount");
        attempt_fusermount_unmount(&entry.mount_path);
    }
    let _ = fs::remove_file(&registry_path);
}

#[cfg(target_os = "linux")]
fn attempt_fusermount_unmount(mount_path: &Path) {
    if let Err(error) = Command::new("fusermount")
        .arg("-u")
        .arg(mount_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        warn!(%error, path = %mount_path.display(), "fusermount -u failed during sweep");
    }
}

#[cfg(not(target_os = "linux"))]
fn attempt_fusermount_unmount(_mount_path: &Path) {
    // Daemon mode is Linux-only; on other platforms there's nothing
    // to sweep. Keeping the function defined keeps the call sites
    // platform-agnostic.
}

/// Spawn `heddle daemon serve` as a detached background process.
/// Stdin/stdout/stderr nulled so the daemon survives the parent
/// shell exiting. On Linux we additionally call `setsid` (via
/// `pre_exec`) so the process detaches from the controlling
/// terminal. On non-Linux this returns the unsupported error
/// before reaching this code.
pub fn spawn_daemon_detached(repo_root: &Path) -> Result<()> {
    let current_exe = std::env::current_exe().context("locate current heddle executable")?;
    let mut command = Command::new(current_exe);
    command
        .arg("--repo")
        .arg(repo_root)
        .arg("daemon")
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid takes no args, returns the new sid, has no
        // memory effects. Standard Unix daemonisation primitive.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command
        .spawn()
        .with_context(|| format!("spawn heddle daemon for {}", repo_root.display()))?;
    Ok(())
}

/// Send a single request to the (possibly-spawned-on-demand) daemon
/// and decode its response. Convenience wrapper used by every CLI
/// verb that talks to the daemon.
pub fn rpc(
    repo_root: &Path,
    request: &MountDaemonRequest,
    spawn_if_missing: bool,
) -> Result<Option<MountDaemonResponse>> {
    let Some(endpoint) = ensure_daemon_endpoint(repo_root, spawn_if_missing)? else {
        return Ok(None);
    };
    let response: MountDaemonResponse = match send_json_request(&endpoint, request) {
        Ok(response) => response,
        Err(error) => {
            // The send/decode failed. Re-read the endpoint file before
            // surfacing the error: if a v1 daemon binary survived the
            // CLI upgrade, the connection succeeds but the response
            // shape no longer parses. The on-disk version is the
            // ground truth — if it's lower than what we speak, give
            // the operator a clear "stop the daemon" hint instead of
            // a raw `decode helper response: ...` line.
            return Err(refine_rpc_error(repo_root, &endpoint, error));
        }
    };
    if response.version() != MOUNT_PROTOCOL_VERSION {
        // We just verified the version on the endpoint file, so a
        // mismatch here means the daemon shipped a buggy response.
        return Err(anyhow!(
            "daemon responded with protocol version {}, expected {}",
            response.version(),
            MOUNT_PROTOCOL_VERSION
        ));
    }
    Ok(Some(response))
}

/// Wrap a send/decode failure with a clearer hint when the on-disk
/// endpoint advertises an older protocol version than this CLI
/// speaks. A future-version daemon (endpoint version > ours) is left
/// alone — that's a different failure mode (likely a protocol bug)
/// and the raw error is more useful there.
fn refine_rpc_error(
    repo_root: &Path,
    endpoint: &EndpointState,
    error: objects::error::HeddleError,
) -> anyhow::Error {
    let endpoint_path = mount_daemon_endpoint_path(repo_root);
    if let Ok(recorded) = load_endpoint(&endpoint_path)
        && recorded.version < MOUNT_PROTOCOL_VERSION
    {
        return anyhow!(error).context(format!(
            "heddled daemon is older (v{their_version}) than this CLI (v{our_version}); \
             run `heddle daemon stop` to force a respawn at the current version, then retry.",
            their_version = recorded.version,
            our_version = MOUNT_PROTOCOL_VERSION,
        ));
    }
    anyhow!(error).context(format!(
        "RPC to daemon at {}:{}",
        endpoint.host, endpoint.port
    ))
}

/// Convenience helper used by the per-thread mount path. Classifies
/// failure modes into "daemon unavailable, fall back" (`Unavailable`)
/// and "daemon said no for a real reason, surface it" (`Fatal`).
///
/// Used by the default-flipped `--workspace virtualized` path where
/// we silently fall back to the in-process mount when the host can't
/// run the daemon. Splits the RPC into endpoint-discovery + send so
/// each phase can pick its own classification.
pub fn mount_via_daemon_classified(
    repo_root: &Path,
    thread_id: &str,
    mount_path: &Path,
) -> std::result::Result<PathBuf, DaemonMountError> {
    // Phase 1: discover (or spawn) the endpoint. Any failure here
    // means the host can't host the daemon — exec failed, fusermount
    // missing on a non-Linux build, endpoint never appeared. All
    // unavailable.
    let endpoint = match ensure_daemon_endpoint(repo_root, true) {
        Ok(Some(endpoint)) => endpoint,
        Ok(None) => {
            return Err(DaemonMountError::Unavailable(
                "daemon endpoint not available and spawn was disabled".to_string(),
            ));
        }
        Err(error) => {
            return Err(DaemonMountError::Unavailable(format!(
                "could not start daemon: {error:#}"
            )));
        }
    };

    // Phase 2: send the mount request. Network errors here mean the
    // daemon died between endpoint check and send — also unavailable
    // from the user's POV.
    let request = MountDaemonRequest::Mount {
        thread_id: thread_id.to_string(),
        mount_path: mount_path.to_path_buf(),
        repo_root: repo_root.to_path_buf(),
    };
    let response: MountDaemonResponse = match send_json_request(&endpoint, &request) {
        Ok(response) => response,
        Err(error) => {
            return Err(DaemonMountError::Unavailable(format!(
                "RPC to daemon at {}:{} failed: {error}",
                endpoint.host, endpoint.port
            )));
        }
    };
    if response.version() != MOUNT_PROTOCOL_VERSION {
        // Endpoint version was right but the response disagrees: a
        // real daemon-side bug. Don't paper over.
        return Err(DaemonMountError::Fatal(anyhow!(
            "daemon responded with protocol version {}, expected {}",
            response.version(),
            MOUNT_PROTOCOL_VERSION
        )));
    }

    // Phase 3: classify the daemon-level reply. Only
    // `ERR_MOUNT_UNSUPPORTED` is a "fall back, this host can't
    // actually mount" signal; everything else (mount_conflict,
    // version_mismatch, unknown codes) is a real problem the caller
    // must see.
    match response {
        MountDaemonResponse::Mount {
            ok: true,
            mount_path,
            ..
        } => Ok(mount_path),
        MountDaemonResponse::Error { code, message, .. } if code == ERR_MOUNT_UNSUPPORTED => Err(
            DaemonMountError::Unavailable(format!("daemon cannot mount on this host: {message}")),
        ),
        MountDaemonResponse::Error { code, message, .. } => Err(DaemonMountError::Fatal(anyhow!(
            "daemon mount failed: [{code}] {message}"
        ))),
        other => Err(DaemonMountError::Fatal(anyhow!(
            "daemon returned unexpected response: {other:?}"
        ))),
    }
}

/// Convenience helper used by `cmd_thread_drop` for daemon-spawned
/// mounts. Returns `was_mounted`.
pub fn unmount_via_daemon(repo_root: &Path, thread_id: &str) -> Result<bool> {
    let request = MountDaemonRequest::Unmount {
        thread_id: thread_id.to_string(),
    };
    let response = rpc(repo_root, &request, false)?;
    match response {
        Some(MountDaemonResponse::Unmount { was_mounted, .. }) => Ok(was_mounted),
        Some(MountDaemonResponse::Error { code, message, .. }) => {
            Err(anyhow!("daemon unmount failed: [{code}] {message}"))
        }
        Some(other) => Err(anyhow!("daemon returned unexpected response: {other:?}")),
        // No daemon running means there's nothing to unmount via the
        // daemon path. The in-process registry handles its own.
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    //! Stale-endpoint and sweep-after-crash tests. These run on every
    //! supported host because the only Linux-only piece (the actual
    //! `fusermount -u` shellout) is gated by `cfg(target_os = "linux")`
    //! and is a no-op everywhere else. The end-to-end Linux path is
    //! covered by the existing virtualized-mount integration test in
    //! `crates/cli/tests/multi_agent_worktrees/virtualized_mount.rs`.

    use std::{io::Write, net::TcpListener, path::PathBuf};

    use repo::daemon::{
        EndpointState, MOUNT_PROTOCOL_VERSION, MountDaemonRequest, MountRegistryFile,
        PersistedMount, mount_daemon_endpoint_path, mount_daemon_registry_path, persist_endpoint,
    };
    use tempfile::TempDir;

    use super::*;

    fn write_endpoint(repo_root: &Path, endpoint: &EndpointState) {
        let path = mount_daemon_endpoint_path(repo_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        persist_endpoint(&path, endpoint).unwrap();
    }

    /// A version-skewed endpoint file is treated as stale: the
    /// client must drop it and (when allowed) respawn at the
    /// version it speaks. Returning `Ok(None)` here is the
    /// "endpoint is stale, but caller said don't spawn" path that
    /// `daemon status` relies on.
    #[test]
    fn read_live_endpoint_treats_version_skew_as_stale() {
        let tmp = TempDir::new().unwrap();
        write_endpoint(
            tmp.path(),
            &EndpointState {
                version: MOUNT_PROTOCOL_VERSION + 99,
                host: "127.0.0.1".to_string(),
                port: 1,
                pid: Some(1),
            },
        );
        let endpoint_path = mount_daemon_endpoint_path(tmp.path());
        let result = read_live_endpoint(&endpoint_path).unwrap();
        assert!(
            result.is_none(),
            "version-skewed endpoint must be treated as stale"
        );
    }

    /// A live endpoint at the right version with a known-alive PID
    /// is returned as-is. We use init (PID 1) as the "definitely
    /// alive" sentinel; same trick the endpoint-module test uses.
    #[cfg(unix)]
    #[test]
    fn read_live_endpoint_returns_alive_endpoint() {
        let tmp = TempDir::new().unwrap();
        write_endpoint(
            tmp.path(),
            &EndpointState {
                version: MOUNT_PROTOCOL_VERSION,
                host: "127.0.0.1".to_string(),
                port: 9999,
                pid: Some(1),
            },
        );
        let endpoint_path = mount_daemon_endpoint_path(tmp.path());
        let result = read_live_endpoint(&endpoint_path).unwrap();
        assert!(result.is_some(), "alive endpoint must be returned as-is");
    }

    /// An endpoint pointing at a dead PID is treated as stale.
    /// 0x7fff_fffe is just below `i32::MAX` and never assigned.
    #[cfg(unix)]
    #[test]
    fn read_live_endpoint_detects_dead_pid() {
        let tmp = TempDir::new().unwrap();
        write_endpoint(
            tmp.path(),
            &EndpointState {
                version: MOUNT_PROTOCOL_VERSION,
                host: "127.0.0.1".to_string(),
                port: 9999,
                pid: Some(0x7fff_fffe),
            },
        );
        let endpoint_path = mount_daemon_endpoint_path(tmp.path());
        let result = read_live_endpoint(&endpoint_path).unwrap();
        assert!(result.is_none(), "endpoint with dead PID must be stale");
    }

    /// A missing endpoint file is silently treated as "no daemon
    /// running", not an error.
    #[test]
    fn read_live_endpoint_handles_missing_file() {
        let tmp = TempDir::new().unwrap();
        let endpoint_path = mount_daemon_endpoint_path(tmp.path());
        let result = read_live_endpoint(&endpoint_path).unwrap();
        assert!(result.is_none());
    }

    /// `sweep_stale_mounts` must remove the registry file even when
    /// every mount entry's `fusermount -u` fails (or is skipped on
    /// non-Linux). The contract is: leave nothing behind that a
    /// future CLI could re-process.
    #[test]
    fn sweep_stale_mounts_clears_registry_file() {
        let tmp = TempDir::new().unwrap();
        let registry_path = mount_daemon_registry_path(tmp.path());
        std::fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
        let registry = MountRegistryFile {
            mounts: vec![PersistedMount {
                thread_id: "ghost".to_string(),
                mount_path: PathBuf::from("/nonexistent-mount-point"),
                pid: 1,
                since_ms: 0,
            }],
        };
        std::fs::write(
            &registry_path,
            serde_json::to_vec_pretty(&registry).unwrap(),
        )
        .unwrap();

        sweep_stale_mounts(tmp.path());
        assert!(
            !registry_path.exists(),
            "sweep must remove the registry file even when entries can't be unmounted"
        );
    }

    /// Sweeping a non-existent registry is a no-op (no panic, no
    /// error). Boring but load-bearing: every fresh repo hits this
    /// path on first daemon spawn.
    #[test]
    fn sweep_stale_mounts_is_noop_when_registry_absent() {
        let tmp = TempDir::new().unwrap();
        sweep_stale_mounts(tmp.path()); // must not panic
    }

    /// A v1 daemon binary that survived a CLI upgrade is observable
    /// only as a successful TCP connect followed by an undecodable
    /// response. After the decode failure the client must re-read the
    /// endpoint file, notice the recorded version is below
    /// `MOUNT_PROTOCOL_VERSION`, and surface a "stop the daemon" hint
    /// rather than a raw `decode helper response: ...` line.
    ///
    /// We simulate the race in the realistic order: the endpoint file
    /// initially advertises the current version (so
    /// `ensure_daemon_endpoint` accepts it), then the fake daemon
    /// "downgrades" the on-disk record to v1 just before sending a
    /// garbage reply — exactly what a v1 binary would have written
    /// had it been the one to claim the port.
    #[test]
    fn rpc_hints_at_stale_daemon_when_endpoint_version_is_older() {
        let tmp = TempDir::new().unwrap();
        let repo_root: PathBuf = tmp.path().to_path_buf();

        // Bind a real listener on a free port. A single accept loop
        // serves the one request the test sends — we don't validate
        // the bytes, we just write garbage back so JSON decoding
        // fails, which is exactly what a v1 daemon's reply looks like
        // to a v2-speaking CLI in the worst case.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_repo = repo_root.clone();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Stamp the on-disk endpoint with the version a real
                // v1 daemon would have written, then send back
                // something that is not parseable as a
                // `MountDaemonResponse`. The client's decode failure
                // must therefore consult an endpoint file that
                // already reads as stale.
                let endpoint_path = mount_daemon_endpoint_path(&server_repo);
                persist_endpoint(
                    &endpoint_path,
                    &EndpointState {
                        version: MOUNT_PROTOCOL_VERSION - 1,
                        host: "127.0.0.1".to_string(),
                        port,
                        pid: Some(1),
                    },
                )
                .unwrap();
                let _ = stream.write_all(b"this is not json\n");
            }
        });

        write_endpoint(
            &repo_root,
            &EndpointState {
                version: MOUNT_PROTOCOL_VERSION,
                host: "127.0.0.1".to_string(),
                port,
                // Use init's PID (1) so `read_live_endpoint` accepts
                // the file as live and we exercise the decode path.
                pid: Some(1),
            },
        );

        let err = rpc(&repo_root, &MountDaemonRequest::Health {}, false)
            .expect_err("v1 daemon reply must surface as an error");
        let _ = server.join();

        // The hint must reference both the recorded daemon version
        // and the documented remediation.
        let chain = format!("{err:#}");
        assert!(
            chain.contains("heddled daemon is older"),
            "expected stale-daemon hint in error chain, got: {chain}"
        );
        assert!(
            chain.contains("heddle daemon stop"),
            "expected remediation hint in error chain, got: {chain}"
        );
        assert!(
            chain.contains(&format!("v{}", MOUNT_PROTOCOL_VERSION - 1)),
            "expected recorded daemon version in error chain, got: {chain}"
        );
        assert!(
            chain.contains(&format!("v{MOUNT_PROTOCOL_VERSION}")),
            "expected CLI version in error chain, got: {chain}"
        );
    }
}
