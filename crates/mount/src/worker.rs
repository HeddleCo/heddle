// SPDX-License-Identifier: Apache-2.0
//! Out-of-process FUSE worker — supervisor handle + IPC framing.
//!
//! ## Why this module exists
//!
//! A panic in a FUSE callback corrupts the host process's heap on
//! the way to the panic guard (heddle#74 added `guard_call` to keep
//! a panic from aborting the process via the C-ABI boundary, but it
//! doesn't undo whatever damage the panicking callback did to a
//! poisoned mutex or a half-mutated cache entry). The heddle#88
//! spike's locked decision (`docs/design/fuse-worker-ipc-decision.md`)
//! is to move FUSE dispatch into a **subprocess** — `heddle-fuse-worker`,
//! a small Linux-only binary that owns the kernel-side mount and
//! talks to its parent over a single inherited Unix socketpair.
//!
//! This module is shared by both sides:
//!
//! * the worker binary (`crates/mount/src/bin/heddle-fuse-worker.rs`)
//!   uses [`run_worker`] as its `main` body,
//! * the parent (the CLI's mount lifecycle, eventually the daemon)
//!   constructs a [`Supervisor`] to spawn + observe the worker.
//!
//! ## IPC protocol — minimal-by-design
//!
//! The spike picks gRPC-over-UDS in §3 because the *daemon-supervisor*
//! model wants the same proto + tonic discipline as the rest of the
//! daemon's RPCs. The current PR (heddle#190) ships the **CLI-dispatched
//! variant** the issue AC calls for — the daemon-supervisor work lands
//! later — and a single inherited socketpair with length-prefixed JSON
//! frames is the right shape for that variant: no proto crate, no
//! tonic, no `tokio` runtime in the worker, no per-mount UDS
//! discovery file. We revisit gRPC when the daemon owns the
//! supervisor and the [`SupervisorCommand`] surface needs to be
//! reachable from any concurrent CLI invocation — see
//! `docs/design/fuse-worker-ipc-decision.md` §6 for that follow-up.
//!
//! Wire format on the socketpair: `u32` little-endian payload length
//! followed by that many bytes of UTF-8 JSON. Each frame is one
//! [`WorkerEvent`] (worker → parent) or [`SupervisorCommand`]
//! (parent → worker).
//!
//! ## Lifecycle the supervisor enforces
//!
//! 1. Parent creates a `socketpair(AF_UNIX, SOCK_STREAM)`, spawns
//!    the worker binary with `--ipc-fd <n>` and the other end
//!    inherited on that fd.
//! 2. Parent reads the first frame from the IPC socket. It is
//!    either [`WorkerEvent::MountReady`] (the kernel attached the
//!    FS) or [`WorkerEvent::MountError`] (mount failed; child
//!    will exit non-zero shortly).
//! 3. While the worker runs, the parent's [`Supervisor`] holds the
//!    IPC sender and a watcher thread that owns the
//!    [`std::process::Child`]. Dropping the supervisor sends
//!    [`SupervisorCommand::Stop`]; if the worker hasn't exited
//!    inside the grace window, the supervisor falls through to
//!    `SIGTERM` and then `SIGKILL`.
//! 4. The watcher thread blocks on the child's `wait()`. On exit
//!    (clean or otherwise) it flips the supervisor's `liveness`
//!    state to [`Liveness::Exited`] so [`Supervisor::is_alive`]
//!    sees the worker as gone, and logs `tracing::warn!("FUSE
//!    worker exited unexpectedly: {status}")` for any non-clean
//!    exit. The kernel auto-unmounts in that path: when the
//!    worker's `/dev/fuse` fd closes, the FUSE driver flushes and
//!    tears the mount down.
//!
//! ## Crash-isolation properties this module locks in
//!
//! Both tested by `crates/mount/tests/fuse_worker_crash.rs`:
//!
//! * **Panic in a FUSE callback** — the worker's process aborts,
//!   the parent observes a non-zero `ExitStatus`, the kernel
//!   auto-unmounts. Parent heap is untouched.
//! * **SIGKILL of the worker** — same shape. The parent's watcher
//!   observes `signal=SIGKILL`, the kernel reclaims the mount.
//!
//! ## Bench discipline
//!
//! `benches/fuse_worker_ipc.rs` measures the round-trip cost of a
//! [`SupervisorCommand::Status`] roundtrip against a live worker.
//! The spike's budget (§7) is **< 1 ms for any control-plane RTT**.
//! That gate fires at bench-run time, not on every test.

#![cfg(all(target_os = "linux", feature = "fuse"))]

use std::{
    io,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// Environment variable a test sets to ask the worker to panic
/// after constructing its [`crate::FuseShell`] but before signalling
/// [`WorkerEvent::MountReady`]. The supervisor's panic-isolation
/// red-commit test asserts the worker's exit is observable by the
/// parent and the parent's process keeps running.
///
/// Production builds should never set this. The env-var sentinel
/// is deliberately preferred over a Rust-side feature flag so the
/// crash injection is exercised against the **shipped binary** —
/// gating it behind `#[cfg(test)]` would test a different artifact
/// than what production runs.
pub const PANIC_ON_INIT_ENV: &str = "HEDDLE_FUSE_WORKER_PANIC_ON_INIT";

/// Optional override (env var) for the supervisor's grace window
/// between sending [`SupervisorCommand::Stop`] and falling through
/// to `SIGTERM`. Defaults to [`DEFAULT_STOP_GRACE`]. Used by the
/// crash-isolation tests to keep their runtime tight.
pub const STOP_GRACE_ENV: &str = "HEDDLE_FUSE_WORKER_STOP_GRACE_MS";

/// Override env var pointing at an alternative `heddle-fuse-worker`
/// binary. Production never sets this; tests use it to point at
/// the `CARGO_BIN_EXE_heddle-fuse-worker` test artifact.
pub const WORKER_BINARY_ENV: &str = "HEDDLE_FUSE_WORKER_BIN";

/// Default grace window between a clean `Stop` and the supervisor
/// falling through to `SIGTERM` → `SIGKILL`. 2 seconds matches the
/// daemon's existing reap window in `crates/daemon/src/local_daemon.rs`.
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(2);

/// The supervisor sends one of these to the worker. The worker
/// reads them off the IPC socket; the receive side is documented
/// in [`run_worker`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum SupervisorCommand {
    /// Tear down cleanly: flush the hot tier, drop the
    /// [`crate::BackgroundSession`] which unmounts, exit 0.
    Stop,
    /// Round-trip health probe. The worker replies with
    /// [`WorkerEvent::StatusOk`] carrying its own pid + the mount
    /// path it owns. Used by the IPC RTT bench and by future
    /// liveness checks from the supervisor.
    Status,
}

/// The worker sends one of these to the supervisor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum WorkerEvent {
    /// The kernel attached the FS at `mount_path`; userspace can
    /// now `open(2)` paths underneath. The supervisor returns
    /// from `spawn` only after seeing this (or [`MountError`]).
    MountReady { pid: u32, mount_path: PathBuf },
    /// Mount setup failed before [`MountReady`] could fire. The
    /// worker will exit shortly with a non-zero status; the
    /// supervisor surfaces `message` to its caller.
    MountError { message: String },
    /// Reply to [`SupervisorCommand::Status`].
    StatusOk { pid: u32, mount_path: PathBuf },
    /// Acknowledgement of a clean [`SupervisorCommand::Stop`]. The
    /// worker emits this immediately before dropping the FUSE
    /// session (which blocks until the kernel finishes unmount).
    /// On the wire it gives the supervisor a positive "graceful
    /// shutdown in progress" signal vs an unexpected exit.
    Stopping,
}

/// Length-prefixed JSON framing.
pub mod framing {
    use std::io::{self, Read, Write};

    use serde::{de::DeserializeOwned, Serialize};

    /// Maximum payload size we'll accept on the wire. Sized large
    /// enough for any plausible control-plane message; refuses to
    /// allocate a multi-gigabyte buffer if a foreign writer ever
    /// lands on the socket. (Production has socket-mode 0600
    /// gating + SO_PEERCRED at the supervisor side; this is
    /// defense in depth for the tests.)
    pub const MAX_FRAME_BYTES: usize = 64 * 1024;

    pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
        let bytes = serde_json::to_vec(msg)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "frame body {} exceeds MAX_FRAME_BYTES {}",
                    bytes.len(),
                    MAX_FRAME_BYTES
                ),
            ));
        }
        let len = bytes.len() as u32;
        w.write_all(&len.to_le_bytes())?;
        w.write_all(&bytes)?;
        w.flush()
    }

    pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
        let mut len_buf = [0u8; 4];
        r.read_exact(&mut len_buf)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "frame body claims {len} bytes, exceeds MAX_FRAME_BYTES {MAX_FRAME_BYTES}"
                ),
            ));
        }
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)?;
        serde_json::from_slice(&buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }
}

/// CLI arguments the worker binary parses. Lives in this module
/// (not the binary file) so the supervisor can serialize the same
/// argv shape with no string-format drift between sides.
#[derive(Debug, Clone)]
pub struct WorkerArgs {
    pub repo_root: PathBuf,
    pub thread_id: String,
    pub mountpoint: PathBuf,
    pub ipc_fd: RawFd,
}

impl WorkerArgs {
    /// Parse from a `String`-shaped argv (skipping argv[0]). The
    /// supervisor builds the exact same arg shape so any drift here
    /// surfaces immediately in the integration tests.
    pub fn parse<S: AsRef<str>>(args: &[S]) -> Result<Self> {
        let mut repo_root: Option<PathBuf> = None;
        let mut thread_id: Option<String> = None;
        let mut mountpoint: Option<PathBuf> = None;
        let mut ipc_fd: Option<RawFd> = None;

        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_ref();
            let value = || -> Result<&str> {
                args.get(i + 1)
                    .map(|s| s.as_ref())
                    .ok_or_else(|| anyhow!("expected value after {a}"))
            };
            match a {
                "--repo-root" => {
                    repo_root = Some(PathBuf::from(value()?));
                    i += 2;
                }
                "--thread-id" => {
                    thread_id = Some(value()?.to_string());
                    i += 2;
                }
                "--mountpoint" => {
                    mountpoint = Some(PathBuf::from(value()?));
                    i += 2;
                }
                "--ipc-fd" => {
                    let raw = value()?.to_string();
                    ipc_fd = Some(
                        raw.parse::<RawFd>()
                            .with_context(|| format!("parse --ipc-fd value '{raw}'"))?,
                    );
                    i += 2;
                }
                other => bail!("unrecognised argument: {other}"),
            }
        }

        Ok(WorkerArgs {
            repo_root: repo_root.ok_or_else(|| anyhow!("--repo-root is required"))?,
            thread_id: thread_id.ok_or_else(|| anyhow!("--thread-id is required"))?,
            mountpoint: mountpoint.ok_or_else(|| anyhow!("--mountpoint is required"))?,
            ipc_fd: ipc_fd.ok_or_else(|| anyhow!("--ipc-fd is required"))?,
        })
    }
}

/// The worker binary's `main` body. Mounts the FUSE session,
/// signals [`WorkerEvent::MountReady`], serves the IPC loop until
/// the supervisor sends [`SupervisorCommand::Stop`] (or the IPC
/// socket EOFs), then drops the session — which unmounts —
/// and returns.
///
/// Panics inside this function (or in any FUSE callback the
/// session dispatches) escape to the worker's `main` and Rust's
/// default panic handler prints + exits 101. That is the **point**:
/// the panic propagates to a clean process exit, the parent
/// observes it on the IPC socket EOF, the kernel auto-unmounts.
pub fn run_worker(args: WorkerArgs) -> Result<()> {
    use crate::FuseShell;
    use repo::Repository;

    // SAFETY: ipc_fd was inherited from the parent over fork+exec
    // (see [`Supervisor::spawn`]). It's a valid UDS file descriptor
    // pointing at the parent's end of the socketpair. We take
    // ownership so it closes on `Drop`.
    let owned: OwnedFd = unsafe { OwnedFd::from_raw_fd(args.ipc_fd) };
    let mut ipc: UnixStream = owned.into();

    // The worker's IPC socket should not survive `exec` — the
    // worker doesn't re-exec, but better hygiene than leaking the
    // fd into any future child.
    set_cloexec(ipc.as_raw_fd())?;

    debug!(
        repo_root = %args.repo_root.display(),
        thread_id = %args.thread_id,
        mountpoint = %args.mountpoint.display(),
        "heddle-fuse-worker starting"
    );

    // Open repo + construct the mount. Errors before MountReady
    // get sent to the supervisor so the user sees a clean message
    // rather than just "worker exited with status 1".
    let mount_result = (|| -> Result<crate::FuseShell> {
        let repo = Repository::open(&args.repo_root)
            .with_context(|| format!("open repo at {}", args.repo_root.display()))?;
        let mount = crate::ContentAddressedMount::new(repo, &args.thread_id)
            .map_err(|e| anyhow!("open content-addressed mount for {}: {e}", args.thread_id))?;
        Ok(FuseShell::new(mount))
    })();
    let shell = match mount_result {
        Ok(s) => s,
        Err(err) => {
            let msg = format!("{err:#}");
            let _ = framing::write_frame(
                &mut ipc,
                &WorkerEvent::MountError {
                    message: msg.clone(),
                },
            );
            bail!("{msg}");
        }
    };

    // Test crash injection — see [`PANIC_ON_INIT_ENV`]. Production
    // never sets this; tests set it to assert the supervisor sees
    // a worker that panics before MountReady as a clean `Err` and
    // keeps the parent's heap intact.
    if std::env::var(PANIC_ON_INIT_ENV).is_ok() {
        panic!("heddle-fuse-worker: panic injected by {PANIC_ON_INIT_ENV}");
    }

    // Background-mount so the FUSE session runs on its own thread
    // and this thread can serve the IPC loop.
    let session = match shell.mount_background(&args.mountpoint) {
        Ok(s) => s,
        Err(err) => {
            let msg = format!("mount_background failed: {err}");
            let _ = framing::write_frame(
                &mut ipc,
                &WorkerEvent::MountError {
                    message: msg.clone(),
                },
            );
            bail!("{msg}");
        }
    };

    framing::write_frame(
        &mut ipc,
        &WorkerEvent::MountReady {
            pid: std::process::id(),
            mount_path: args.mountpoint.clone(),
        },
    )
    .context("signal MountReady to supervisor")?;

    debug!(mountpoint = %args.mountpoint.display(), "FUSE worker ready");

    // Control loop: parse each frame, respond appropriately. Loop
    // exits when the supervisor sends [`SupervisorCommand::Stop`]
    // or the IPC socket reaches EOF (parent process gone).
    loop {
        match framing::read_frame::<_, SupervisorCommand>(&mut ipc) {
            Ok(SupervisorCommand::Stop) => {
                debug!("received Stop; unmounting");
                let _ = framing::write_frame(&mut ipc, &WorkerEvent::Stopping);
                break;
            }
            Ok(SupervisorCommand::Status) => {
                framing::write_frame(
                    &mut ipc,
                    &WorkerEvent::StatusOk {
                        pid: std::process::id(),
                        mount_path: args.mountpoint.clone(),
                    },
                )
                .context("reply to Status")?;
            }
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                warn!("IPC socket EOF; supervisor gone, unmounting");
                break;
            }
            Err(err) => {
                warn!(%err, "IPC read error; unmounting");
                break;
            }
        }
    }

    // Dropping `session` unmounts (fuser sends `FUSE_DESTROY` and
    // waits for the kernel to release the device).
    drop(session);
    Ok(())
}

fn set_cloexec(fd: RawFd) -> Result<()> {
    // SAFETY: `fcntl(F_GETFD)` returns the flag set or -1/errno;
    // we never deref a pointer.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error()).context("fcntl(F_GETFD) on ipc fd");
    }
    // SAFETY: same shape as the GETFD; we only OR in FD_CLOEXEC.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    if rc < 0 {
        return Err(io::Error::last_os_error()).context("fcntl(F_SETFD, FD_CLOEXEC) on ipc fd");
    }
    Ok(())
}

fn clear_cloexec(fd: RawFd) -> Result<()> {
    // SAFETY: same shape as set_cloexec.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error()).context("fcntl(F_GETFD)");
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    if rc < 0 {
        return Err(io::Error::last_os_error()).context("fcntl(F_SETFD, !FD_CLOEXEC)");
    }
    Ok(())
}

/// Worker liveness state — flipped from [`Running`] to
/// [`Exited`] by the watcher thread once it observes the child's
/// `wait()` return. `AtomicU8` so [`Supervisor::is_alive`] is a
/// lock-free check.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    Running = 0,
    Exited = 1,
}

/// The parent-side handle for a running `heddle-fuse-worker`.
///
/// Owns the IPC socket; the [`Child`] is moved into the watcher
/// thread at spawn time so the watcher is the unique reaper. The
/// supervisor talks to the worker via the IPC socket and signals
/// it via the recorded `pid`. This split keeps `wait()` (blocking)
/// and `kill()` (non-blocking) in different threads with no shared
/// ownership of `Child`.
///
/// Dropping the supervisor runs the graceful-shutdown sequence —
/// see [`Supervisor::unmount`].
pub struct Supervisor {
    /// Held in `Mutex<Option<...>>` so [`Supervisor::unmount`] can
    /// `take()` the IPC stream and run the shutdown sequence
    /// idempotently. Idempotent matters because the CLI's mount
    /// lifecycle calls `unmount()` from both `unmount_thread_if_mounted`
    /// and the `Drop` impl, and a double-shutdown should be a no-op.
    ipc: Mutex<Option<UnixStream>>,
    pid: u32,
    mountpoint: PathBuf,
    stop_grace: Duration,
    liveness: Arc<AtomicU8>,
    watcher: Mutex<Option<JoinHandle<()>>>,
}

impl Supervisor {
    /// Spawn `heddle-fuse-worker` against `worker_binary`. Returns
    /// once the worker has signalled [`WorkerEvent::MountReady`]
    /// (success) or [`WorkerEvent::MountError`] (failure → returned
    /// as `Err`).
    ///
    /// `worker_binary` is typically resolved by the supervisor's
    /// caller via [`default_worker_binary`].
    pub fn spawn(
        worker_binary: &Path,
        repo_root: &Path,
        thread_id: &str,
        mountpoint: &Path,
    ) -> Result<Self> {
        let stop_grace = stop_grace_from_env().unwrap_or(DEFAULT_STOP_GRACE);

        // Build a socketpair. The worker inherits one end (an
        // already-known fd it'll read with `--ipc-fd <n>`); the
        // supervisor keeps the other.
        let (parent_end, child_end) = UnixStream::pair().context("create supervisor socketpair")?;

        // Strip CLOEXEC from the child's end so it survives the
        // exec(2) into `heddle-fuse-worker`. The default for
        // sockets created in Rust is CLOEXEC-on, which would
        // close the fd inside the new process and the worker
        // would observe an EBADF on its first `read`.
        clear_cloexec(child_end.as_raw_fd())?;

        // We need the raw fd to survive the `Stdio` redirections
        // and the exec. Hold the fd in an `OwnedFd` until the spawn
        // result is in hand so an early-return from `Command::spawn`
        // (missing binary, EACCES) drops the guard and closes the
        // fd — without the guard, the raw fd leaks once per failed
        // attempt and repeated mount fallbacks march toward EMFILE.
        // SAFETY: `child_end` was just constructed via `UnixStream::pair`
        // and uniquely owned; `into_raw_fd` transfers that ownership
        // to us, which we immediately re-wrap.
        let child_owned: OwnedFd = unsafe { OwnedFd::from_raw_fd(child_end.into_raw_fd()) };
        let child_raw: RawFd = child_owned.as_raw_fd();

        let mut command = Command::new(worker_binary);
        command
            .arg("--repo-root")
            .arg(repo_root)
            .arg("--thread-id")
            .arg(thread_id)
            .arg("--mountpoint")
            .arg(mountpoint)
            .arg("--ipc-fd")
            .arg(child_raw.to_string())
            // The worker's own stdin is nulled; stdout/stderr stay
            // attached so panic backtraces land in the parent's
            // terminal / log capture. The CLI may swap these for
            // file redirects in a future hardening pass.
            .stdin(Stdio::null());

        // Make sure the inherited fd is still un-CLOEXEC inside
        // the child between fork and exec. We already cleared it
        // in the parent above; this pre_exec is a defense against
        // the parent's clear racing with another thread that
        // re-sets the flag. Cheap and harmless if already cleared.
        // SAFETY: `pre_exec` runs in the child between fork and
        // exec. `fcntl` is async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                let cur = libc::fcntl(child_raw, libc::F_GETFD);
                if cur < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(child_raw, libc::F_SETFD, cur & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = command
            .spawn()
            .with_context(|| format!("spawn {}", worker_binary.display()))?;

        // The child took its dup of `child_raw` via fork. Drop the
        // parent's `OwnedFd` to close the parent's copy — the
        // parent's IPC end is `parent_end`. On the error branch
        // above, the `?` returns early and `child_owned` drops on
        // the way out, closing the fd without the explicit step.
        drop(child_owned);

        let pid = child.id();

        // Wait for the worker's MountReady (or MountError) on the
        // parent's end of the IPC.
        let mut ipc = parent_end;
        let event: WorkerEvent = match framing::read_frame(&mut ipc) {
            Ok(ev) => ev,
            Err(err) => {
                // Reap the child; whatever it did, it didn't make
                // it to MountReady.
                let mut child = child;
                let _ = child.wait();
                return Err(anyhow!(
                    "read worker handshake (worker may have crashed before MountReady): {err}"
                ));
            }
        };
        match event {
            WorkerEvent::MountReady { .. } => {}
            WorkerEvent::MountError { message } => {
                let mut child = child;
                let _ = child.wait();
                bail!("worker reported mount error: {message}");
            }
            other => {
                let mut child = child;
                let _ = child.wait();
                bail!("worker sent unexpected handshake frame: {other:?}");
            }
        }

        // Hand the child off to the watcher thread. The watcher
        // is the unique reaper; the supervisor signals via pid.
        let liveness = Arc::new(AtomicU8::new(Liveness::Running as u8));
        let watcher_liveness = Arc::clone(&liveness);
        let watcher_mount = mountpoint.to_path_buf();
        let watcher = thread::Builder::new()
            .name(format!("fuse-worker-watcher:{thread_id}"))
            .spawn(move || watch_child(child, watcher_liveness, watcher_mount))
            .context("spawn watcher thread")?;

        Ok(Supervisor {
            ipc: Mutex::new(Some(ipc)),
            pid,
            mountpoint: mountpoint.to_path_buf(),
            stop_grace,
            liveness,
            watcher: Mutex::new(Some(watcher)),
        })
    }

    /// Shut the worker down. Sends [`SupervisorCommand::Stop`], waits
    /// for the watcher to observe the child's exit inside
    /// `stop_grace`, falls through to `SIGTERM` (then `SIGKILL`) if
    /// it doesn't. Idempotent — the second call is a no-op.
    pub fn unmount(&self) -> Result<()> {
        // `take()` the IPC stream so a concurrent caller (or a
        // re-entry via Drop) sees the supervisor as already shut.
        let ipc_opt = self
            .ipc
            .lock()
            .expect("supervisor ipc lock")
            .take();
        let Some(mut ipc) = ipc_opt else {
            // Already shut down; still try to join the watcher.
            self.join_watcher();
            return Ok(());
        };

        // If the worker is already gone (panic, OOM, SIGKILL),
        // the write errors out and we proceed straight to the
        // watcher-join. That's the correct behaviour — there's
        // nothing left to stop.
        let _ = framing::write_frame(&mut ipc, &SupervisorCommand::Stop);

        if !self.wait_for_exit(self.stop_grace) {
            warn!(
                pid = self.pid,
                grace_ms = self.stop_grace.as_millis() as u64,
                "FUSE worker did not exit on Stop; escalating to SIGTERM"
            );
            send_signal(self.pid as i32, libc::SIGTERM);
            if !self.wait_for_exit(self.stop_grace) {
                warn!(
                    pid = self.pid,
                    "FUSE worker did not exit on SIGTERM; escalating to SIGKILL"
                );
                send_signal(self.pid as i32, libc::SIGKILL);
                // SIGKILL is unblockable; the watcher will observe
                // the wait() return very shortly. Wait one more
                // grace window for the watcher to flip liveness.
                self.wait_for_exit(self.stop_grace);
            }
        }
        // Drop the IPC explicitly so the worker's read loop sees
        // EOF if it somehow ignored our Stop.
        drop(ipc);
        self.join_watcher();
        Ok(())
    }

    fn join_watcher(&self) {
        if let Some(handle) = self.watcher.lock().expect("watcher lock").take() {
            let _ = handle.join();
        }
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Send [`SupervisorCommand::Status`] and read the
    /// [`WorkerEvent::StatusOk`] reply. The supervisor takes the
    /// IPC mutex for the duration; bench callers should treat the
    /// roundtrip as exclusive. Returns the worker's reported pid +
    /// mount path.
    pub fn status(&self) -> Result<(u32, PathBuf)> {
        let mut guard = self.ipc.lock().expect("supervisor ipc lock");
        let Some(ipc) = guard.as_mut() else {
            bail!("supervisor already shut down");
        };
        framing::write_frame(ipc, &SupervisorCommand::Status).context("send Status command")?;
        let event: WorkerEvent = framing::read_frame(ipc).context("read Status reply")?;
        match event {
            WorkerEvent::StatusOk { pid, mount_path } => Ok((pid, mount_path)),
            other => bail!("expected StatusOk, got {other:?}"),
        }
    }

    /// Returns `true` if the watcher has not yet observed the
    /// worker's exit. After the worker dies (clean exit, panic,
    /// signal) the watcher flips this to `false`. Lock-free — safe
    /// from a busy-poll.
    pub fn is_alive(&self) -> bool {
        self.liveness.load(Ordering::SeqCst) == Liveness::Running as u8
    }

    /// Block until the watcher sees the child exit, or until
    /// `dur` elapses. Returns whether the child has exited.
    pub fn wait_for_exit(&self, dur: Duration) -> bool {
        let deadline = Instant::now() + dur;
        while self.is_alive() {
            if Instant::now() >= deadline {
                return !self.is_alive();
            }
            thread::sleep(Duration::from_millis(5));
        }
        true
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        // Best-effort; we don't want a Drop panic if the worker is
        // already gone.
        let _ = self.unmount();
    }
}

fn watch_child(mut child: Child, liveness: Arc<AtomicU8>, mountpoint: PathBuf) {
    let result = child.wait();
    // Flip liveness BEFORE logging so [`Supervisor::is_alive`]
    // observes the exit immediately. A delayed flip would race
    // with `Supervisor::wait_for_exit`'s polling loop and risk
    // a spurious timeout under load.
    liveness.store(Liveness::Exited as u8, Ordering::SeqCst);
    match result {
        Ok(s) if s.success() => {
            debug!(mountpoint = %mountpoint.display(), "FUSE worker exited cleanly");
        }
        Ok(s) => {
            warn!(
                mountpoint = %mountpoint.display(),
                exit = format_exit(&s),
                "FUSE worker exited unexpectedly"
            );
        }
        Err(e) => {
            warn!(
                mountpoint = %mountpoint.display(),
                error = %e,
                "FUSE worker wait() failed"
            );
        }
    }
}

fn format_exit(status: &ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        format!("exit code {code}")
    } else if let Some(sig) = status.signal() {
        format!("signal {sig}")
    } else {
        format!("{status:?}")
    }
}

fn send_signal(pid: i32, sig: i32) {
    // SAFETY: kill is async-signal-safe and we own the child pid.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

fn stop_grace_from_env() -> Option<Duration> {
    std::env::var(STOP_GRACE_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
}

/// Resolve the path to `heddle-fuse-worker`. Convention: the
/// worker lives next to the running binary (`heddle`), in the
/// same directory. Mirrors the pattern in
/// `crates/cli/src/cli/commands/daemon/client.rs::spawn_daemon_detached`.
///
/// Tests override via the [`WORKER_BINARY_ENV`] env var pointing
/// at the `env!("CARGO_BIN_EXE_heddle-fuse-worker")` artifact.
/// Production code reads the env var too — there's no security
/// gain from refusing the override here, and a packager who needs
/// a non-sibling install layout can use it.
pub fn default_worker_binary() -> Result<PathBuf> {
    if let Ok(override_path) = std::env::var(WORKER_BINARY_ENV) {
        return Ok(PathBuf::from(override_path));
    }
    let exe = std::env::current_exe().context("locate current heddle executable")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("current exe has no parent dir: {}", exe.display()))?;
    let candidate = dir.join("heddle-fuse-worker");
    if !candidate.exists() {
        bail!(
            "heddle-fuse-worker not found next to {} (looked at {}); \
             reinstall heddle or set {WORKER_BINARY_ENV}",
            exe.display(),
            candidate.display(),
        );
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_args_parses_a_minimal_set() {
        let argv = vec![
            "--repo-root",
            "/tmp/repo",
            "--thread-id",
            "main",
            "--mountpoint",
            "/tmp/mnt",
            "--ipc-fd",
            "3",
        ];
        let parsed = WorkerArgs::parse(&argv).expect("parse");
        assert_eq!(parsed.repo_root, PathBuf::from("/tmp/repo"));
        assert_eq!(parsed.thread_id, "main");
        assert_eq!(parsed.mountpoint, PathBuf::from("/tmp/mnt"));
        assert_eq!(parsed.ipc_fd, 3);
    }

    #[test]
    fn worker_args_rejects_unknown_flags() {
        let argv = vec![
            "--repo-root",
            "/tmp/repo",
            "--thread-id",
            "main",
            "--mountpoint",
            "/tmp/mnt",
            "--ipc-fd",
            "3",
            "--mystery",
        ];
        assert!(WorkerArgs::parse(&argv).is_err());
    }

    #[test]
    fn worker_args_requires_all_fields() {
        let argv = vec![
            "--repo-root",
            "/tmp/repo",
            "--thread-id",
            "main",
            "--mountpoint",
            "/tmp/mnt",
        ];
        let err = WorkerArgs::parse(&argv).unwrap_err();
        assert!(err.to_string().contains("--ipc-fd"));
    }

    /// Round-trip a tagged enum through the framing module. Locks
    /// in the wire format so a future "let's compress" change
    /// doesn't silently break the worker.
    #[test]
    fn framing_round_trips_typed_messages() {
        use std::io::Cursor;
        let mut buf = Vec::new();
        framing::write_frame(
            &mut buf,
            &WorkerEvent::MountReady {
                pid: 12345,
                mount_path: PathBuf::from("/tmp/x"),
            },
        )
        .unwrap();
        let mut r = Cursor::new(buf);
        let parsed: WorkerEvent = framing::read_frame(&mut r).unwrap();
        match parsed {
            WorkerEvent::MountReady { pid, mount_path } => {
                assert_eq!(pid, 12345);
                assert_eq!(mount_path, PathBuf::from("/tmp/x"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn framing_rejects_oversize_frame() {
        use std::io::Cursor;
        let mut buf = Vec::new();
        buf.extend_from_slice(&(1u32 << 30).to_le_bytes());
        let mut r = Cursor::new(buf);
        let err = framing::read_frame::<_, WorkerEvent>(&mut r).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
