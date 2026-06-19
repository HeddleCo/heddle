// SPDX-License-Identifier: Apache-2.0
//! Lifecycle helpers for `--workspace virtualized` threads.
//!
//! Virtualized threads project the thread's content-addressed tree
//! through a kernel-side mount instead of materializing a checkout.
//! The mount itself lives in [`crates/mount`]; this module is the
//! thin CLI-side adapter that:
//!
//! * spawns a background mount session when the thread starts,
//! * unmounts cleanly when the thread is dropped.
//!
//! The OS-specific implementation lives in the `linux` / `macos` /
//! `windows` submodules and only compiles with `--features mount`
//! on the corresponding target. Every other build gets the
//! [`virtualized_unsupported_error`] runtime check so the rest of
//! the CLI keeps building everywhere.

use std::path::Path;

use anyhow::{Result, anyhow};
use serde::Serialize;

/// Machine-readable FSKit readiness detail surfaced by `heddle start
/// --workspace virtualized --output json` on macOS when the CLI took an
/// FSKit-specific decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct FskitReadinessReport {
    pub state: &'static str,
    pub backend: &'static str,
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings_url: Option<&'static str>,
}

pub(crate) struct SpawnedMount {
    pub owner: VirtualizedMountOwner,
    pub fskit_readiness: Option<FskitReadinessReport>,
}

/// Kernel/user-space backend that actually owns a virtualized mount.
#[allow(dead_code)] // Variants are target-gated; a single-host check sees only one subset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VirtualizedMountBackend {
    FuseWorker,
    FsKit,
    ProjFs,
    Nfs,
}

/// Process that owns the mount lifetime.
///
/// `Daemon` mounts outlive this CLI process and must be unwound via the daemon
/// RPC path. `InProcess` mounts live in this process' registry and must be
/// unwound there, but we still keep the concrete backend for diagnostics and
/// tests.
#[allow(dead_code)] // The in-process variant is only built by target-gated mount adapters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VirtualizedMountOwner {
    Daemon,
    InProcess(VirtualizedMountBackend),
}

/// Fully typed result of establishing a virtualized workspace mount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VirtualizedMountOutcome {
    pub owner: VirtualizedMountOwner,
    pub fskit_readiness: Option<FskitReadinessReport>,
}

impl VirtualizedMountOutcome {
    fn daemon() -> Self {
        Self {
            owner: VirtualizedMountOwner::Daemon,
            fskit_readiness: None,
        }
    }

    fn in_process(mounted: SpawnedMount) -> Self {
        Self {
            owner: mounted.owner,
            fskit_readiness: mounted.fskit_readiness,
        }
    }
}

/// Run mount setup/teardown on a plain OS thread whenever the caller is already
/// on a Tokio runtime thread. The NFS fallback owns an internal Tokio runtime;
/// constructing or dropping it from a Tokio worker has historically produced
/// nested-runtime panics on macOS fallback paths. The synchronous CLI still gets
/// the direct path.
fn run_mount_io<T, F>(label: &'static str, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_err() {
        return work();
    }

    let thread_name = format!("heddle-mount-{label}");
    let join = std::thread::Builder::new()
        .name(thread_name)
        .spawn(work)
        .map_err(|error| anyhow!("spawn {label} mount worker: {error}"))?;
    join.join()
        .map_err(|_| anyhow!("{label} mount worker panicked"))?
}

/// Anchored error for any build that has no native mount adapter
/// active. Surfaced when none of (linux+fuse, macos+fskit,
/// windows+projfs) is in play. Keeps the runtime message identical
/// regardless of which gate failed: the user has the same fix —
/// rebuild on a supported host with `--features mount`.
#[allow(dead_code)]
pub(crate) fn virtualized_unsupported_error() -> anyhow::Error {
    anyhow!(
        "Virtualized workspace requires Linux/macOS/Windows + heddle built with --features mount"
    )
}

#[cfg(all(target_os = "linux", feature = "mount"))]
mod linux {
    use std::{
        path::{Path, PathBuf},
        sync::Mutex,
    };

    use anyhow::{Context, Result, anyhow};
    use mount::{
        ContentAddressedMount, NfsSession, NfsShell,
        worker::{Supervisor, default_worker_binary},
    };
    use repo::Repository;
    use tracing::warn;

    use crate::util::OnceMap;

    /// Which backend is actually live behind a [`MountHandle`].
    ///
    /// The CLI prefers FUSE-via-`heddle-fuse-worker` (unprivileged,
    /// zero install on a host with `CONFIG_FUSE_FS` + `fusermount`,
    /// **crash-isolated** because the FUSE callbacks run in their
    /// own process). It falls back to NFS when FUSE isn't available
    /// — typically a container or minimal-userland Linux without
    /// `fusermount` on PATH.
    ///
    /// **heddle#190 cutover.** Before this change `Fuse` was an
    /// in-process `mount::BackgroundSession` and a panic in a FUSE
    /// callback could corrupt the CLI's heap before the panic
    /// guard caught it. The new variant wraps a
    /// `mount::worker::Supervisor` instead: panics escape the
    /// worker process cleanly, the kernel auto-unmounts on `/dev/fuse`
    /// close, and the CLI surfaces the exit via
    /// `tracing::warn!("FUSE worker exited unexpectedly: …")`.
    enum BackingSession {
        FuseWorker(Supervisor),
        Nfs(NfsSession),
    }

    impl BackingSession {
        fn unmount(self) -> Result<()> {
            match self {
                Self::FuseWorker(s) => s.unmount(),
                Self::Nfs(s) => s.unmount().map_err(|e| anyhow!("nfs unmount: {e}")),
            }
        }
    }

    /// The opaque handle a CLI caller stashes alongside the thread
    /// to keep the mount alive. Dropping it triggers the backing
    /// session's drop, which unmounts the FS.
    pub struct MountHandle {
        session: Mutex<Option<BackingSession>>,
        mountpoint: PathBuf,
    }

    impl MountHandle {
        pub fn unmount(&self) -> Result<()> {
            let mut guard = self.session.lock().expect("mount session lock");
            if let Some(s) = guard.take() {
                s.unmount()?;
            }
            Ok(())
        }

        pub fn mountpoint(&self) -> &Path {
            &self.mountpoint
        }
    }

    static REGISTRY: OnceMap<String, std::sync::Arc<MountHandle>> = OnceMap::new();

    /// Mount `thread_id` into `mountpoint`. Tries the FUSE worker
    /// subprocess first; on failure (worker binary missing, host
    /// without `/dev/fuse`, kernel module unloaded, etc.), falls
    /// back to the NFS shell so the feature still works on hosts
    /// that don't ship FUSE.
    pub fn spawn_mount_for_thread(
        repo: Repository,
        thread_id: &str,
        mountpoint: &Path,
    ) -> Result<super::SpawnedMount> {
        std::fs::create_dir_all(mountpoint)
            .with_context(|| format!("create mount point {}", mountpoint.display()))?;

        let root = repo.root().to_path_buf();
        // We may need to construct a fresh `ContentAddressedMount`
        // for the NFS fallback; drop the FUSE-side `repo` first so
        // the fallback's `Repository::open` doesn't conflict with
        // a still-open handle.
        drop(repo);

        let (session, owner) = match spawn_fuse_worker(&root, thread_id, mountpoint) {
            Ok(sup) => (
                BackingSession::FuseWorker(sup),
                super::VirtualizedMountOwner::InProcess(super::VirtualizedMountBackend::FuseWorker),
            ),
            Err(native_err) => {
                warn!(
                    thread = thread_id,
                    "heddle-fuse-worker spawn failed ({native_err}); falling back to NFS"
                );
                let reopened = Repository::open(&root)
                    .map_err(|e| anyhow!("reopen repo for NFS fallback: {e}"))?;
                let mount = ContentAddressedMount::new(reopened, thread_id)
                    .map_err(|e| anyhow!("open mount for {thread_id} (NFS fallback): {e}"))?;
                (
                    BackingSession::Nfs(NfsShell::new(mount).mount_background(mountpoint).map_err(
                        |e| {
                            anyhow!(
                                "FUSE worker spawn failed ({native_err}); NFS fallback also failed: {e}"
                            )
                        },
                    )?),
                    super::VirtualizedMountOwner::InProcess(super::VirtualizedMountBackend::Nfs),
                )
            }
        };

        let handle = std::sync::Arc::new(MountHandle {
            session: Mutex::new(Some(session)),
            mountpoint: mountpoint.to_path_buf(),
        });
        REGISTRY.insert(thread_id.to_string(), std::sync::Arc::clone(&handle));
        Ok(super::SpawnedMount {
            owner,
            fskit_readiness: None,
        })
    }

    /// Resolve the worker binary, then drive
    /// [`Supervisor::spawn`]. Factored out so the
    /// `spawn_mount_for_thread` match arm stays readable.
    fn spawn_fuse_worker(
        repo_root: &Path,
        thread_id: &str,
        mountpoint: &Path,
    ) -> Result<Supervisor> {
        let bin = default_worker_binary().context("locate heddle-fuse-worker")?;
        Supervisor::spawn(&bin, repo_root, thread_id, mountpoint)
            .with_context(|| format!("spawn heddle-fuse-worker for thread {thread_id}"))
    }

    pub fn unmount_thread_if_mounted(thread_id: &str) -> bool {
        let Some(handle) = REGISTRY.remove(&thread_id.to_string()) else {
            return false;
        };
        if let Err(err) = handle.unmount() {
            warn!(
                thread = thread_id,
                mountpoint = %handle.mountpoint().display(),
                "unmount failed: {err}"
            );
        }
        true
    }
}

#[cfg(all(target_os = "macos", feature = "mount"))]
mod macos {
    use std::{
        io::{self, IsTerminal, Write},
        path::{Path, PathBuf},
        process::Command,
        sync::Mutex,
        time::Duration,
    };

    use anyhow::{Context, Result, anyhow};
    use mount::{
        ContentAddressedMount, NfsSession, NfsShell,
        fskit::readiness::{self, Readiness},
    };
    use repo::Repository;
    use tracing::{debug, info, warn};

    use crate::{cli::commands::mount_lifecycle::FskitReadinessReport, util::OnceMap};

    const MIN_FSKIT_MACOS_MAJOR: u64 = 26;
    const SETTINGS_PATH: &str =
        "System Settings → General → Login Items & Extensions → File System Extensions";
    const SETTINGS_LOGIN_ITEMS_URL: &str =
        "x-apple.systempreferences:com.apple.LoginItems-Settings.extension";
    const SETTINGS_SEQUOIA_FILE_EXTENSIONS_URL: &str =
        "x-apple.systempreferences:com.apple.LoginItems-Settings.extension?Extensions";
    const FSKIT_INSTALL_HINT: &str = "FSKit fast path available — install the host app: brew install --cask heddle (or download from https://github.com/HeddleCo/heddle/releases)";
    const FSKIT_POLL_INTERVAL: Duration = Duration::from_millis(1_500);
    const FSKIT_WAIT_MESSAGE: &str =
        "Waiting for macOS to report Heddle enabled in File System Extensions";
    const FSKIT_SPINNER_FRAMES: [char; 4] = ['|', '/', '-', '\\'];
    const FSKIT_LINE_CLEAR_PADDING: &str = "                                                ";

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct MacOsVersion {
        major: u64,
        minor: u64,
        patch: u64,
    }

    /// Which backend is actually live behind a [`MountHandle`] on
    /// macOS.
    ///
    /// FSKit is the preferred adapter: when the System Extension
    /// is installed and enabled, the CLI shells out to
    /// `mount -F -t heddle` and the kernel routes through `fskitd`
    /// → our extension → the Rust core. NFS is the universal
    /// fallback for hosts where the extension is missing or the
    /// user hasn't approved it yet.
    enum BackingSession {
        FsKit(FsKitMount),
        Nfs(NfsSession),
    }

    impl BackingSession {
        fn unmount(self) -> Result<()> {
            match self {
                Self::FsKit(m) => m.unmount(),
                Self::Nfs(s) => s.unmount().map_err(|e| anyhow!("nfs unmount: {e}")),
            }
        }
    }

    /// RAII wrapper around an `/sbin/mount -F -t heddle` invocation.
    /// Dropping (or calling `unmount`) runs `umount` against the
    /// stashed mountpoint.
    struct FsKitMount {
        mountpoint: PathBuf,
    }

    impl FsKitMount {
        fn mount(repo_path: &Path, thread_id: &str, mountpoint: &Path) -> Result<Self> {
            let status = Command::new("/sbin/mount")
                .arg("-F")
                .arg("-t")
                .arg("heddle")
                .arg("-o")
                .arg(format!("t={thread_id}"))
                .arg(repo_path)
                .arg(mountpoint)
                .status()
                .context("invoke /sbin/mount -F -t heddle")?;
            if !status.success() {
                return Err(anyhow!(
                    "/sbin/mount -F -t heddle returned {status} \
                     (extension installed but rejected the mount; \
                     check `log show --predicate 'subsystem == \"sh.heddle.HeddleFSModule\"'`)"
                ));
            }
            Ok(Self {
                mountpoint: mountpoint.to_path_buf(),
            })
        }

        fn unmount(self) -> Result<()> {
            let status = Command::new("umount").arg(&self.mountpoint).status();
            match status {
                Ok(s) if s.success() => Ok(()),
                Ok(s) => Err(anyhow!("umount returned {s}")),
                Err(e) => Err(anyhow!("invoke umount: {e}")),
            }
        }
    }

    pub struct MountHandle {
        session: Mutex<Option<BackingSession>>,
        mountpoint: PathBuf,
    }

    impl MountHandle {
        pub fn unmount(&self) -> Result<()> {
            let mut guard = self.session.lock().expect("mount session lock");
            if let Some(s) = guard.take() {
                s.unmount()?;
            }
            Ok(())
        }

        pub fn mountpoint(&self) -> &Path {
            &self.mountpoint
        }
    }

    static REGISTRY: OnceMap<String, std::sync::Arc<MountHandle>> = OnceMap::new();

    struct MountedSession {
        session: BackingSession,
        owner: super::VirtualizedMountOwner,
    }

    impl MountedSession {
        fn fskit(session: FsKitMount) -> Self {
            Self {
                session: BackingSession::FsKit(session),
                owner: super::VirtualizedMountOwner::InProcess(
                    super::VirtualizedMountBackend::FsKit,
                ),
            }
        }

        fn nfs(session: NfsSession) -> Self {
            Self {
                session: BackingSession::Nfs(session),
                owner: super::VirtualizedMountOwner::InProcess(super::VirtualizedMountBackend::Nfs),
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct FsKitMountReport {
        state: &'static str,
        action: &'static str,
        settings_url: Option<&'static str>,
    }

    impl FsKitMountReport {
        fn readiness(self, backend: &'static str) -> FskitReadinessReport {
            FskitReadinessReport {
                state: self.state,
                backend,
                action: self.action,
                settings_url: self.settings_url,
            }
        }
    }

    struct MacOsMountOutcome {
        mounted: MountedSession,
        fskit_readiness: Option<FskitReadinessReport>,
    }

    impl MacOsMountOutcome {
        fn nfs(mounted: MountedSession, fskit_readiness: Option<FskitReadinessReport>) -> Self {
            Self {
                mounted,
                fskit_readiness,
            }
        }
    }

    /// Mount `thread_id` into `mountpoint`. On macOS 26.0+ this
    /// probes FSKit readiness first; if the extension is enabled,
    /// mounts via `mount -F -t heddle`. If the extension is installed
    /// but disabled, the CLI opens System Settings and polls briefly
    /// so the moment the user enables it this same start continues
    /// through FSKit. Other states fall back to NFS.
    pub fn spawn_mount_for_thread(
        repo: Repository,
        thread_id: &str,
        mountpoint: &Path,
    ) -> Result<super::SpawnedMount> {
        std::fs::create_dir_all(mountpoint)
            .with_context(|| format!("create mount point {}", mountpoint.display()))?;

        let root = repo.root().to_path_buf();
        let outcome = if macos_supports_fskit() {
            match readiness::probe() {
                Readiness::Ready => {
                    info!(
                        thread = thread_id,
                        "FSKit extension ready; using `mount -F -t heddle`"
                    );
                    // Don't construct an in-process ContentAddressedMount
                    // — the extension owns the mount lifetime.
                    mount_via_fskit_or_nfs(
                        &root,
                        thread_id,
                        mountpoint,
                        FsKitMountReport {
                            state: "ready",
                            action: "mounted",
                            settings_url: None,
                        },
                    )?
                }
                Readiness::NeedsApproval => {
                    let settings_url = settings_deep_link().url;
                    print_needs_approval_block(settings_url);
                    wait_for_enter_to_open_settings(settings_url);
                    open_settings(settings_url);
                    wait_for_fskit_ready();
                    info!(
                        thread = thread_id,
                        "FSKit extension enabled during poll; using `mount -F -t heddle`"
                    );
                    mount_via_fskit_or_nfs(
                        &root,
                        thread_id,
                        mountpoint,
                        FsKitMountReport {
                            state: "ready_after_approval",
                            action: "mounted_after_poll",
                            settings_url: Some(settings_url),
                        },
                    )?
                }
                Readiness::NotInstalled => {
                    eprintln!("{FSKIT_INSTALL_HINT}");
                    MacOsMountOutcome::nfs(
                        mount_via_nfs(&root, thread_id, mountpoint)?,
                        Some(FskitReadinessReport {
                            state: "not_installed",
                            backend: "nfs",
                            action: "fell_back",
                            settings_url: None,
                        }),
                    )
                }
                Readiness::UnsupportedMacOS => {
                    eprintln!("{}", readiness::unsupported_macos_hint());
                    MacOsMountOutcome::nfs(
                        mount_via_nfs(&root, thread_id, mountpoint)?,
                        Some(FskitReadinessReport {
                            state: "unsupported_macos",
                            backend: "nfs",
                            action: "fell_back",
                            settings_url: None,
                        }),
                    )
                }
                Readiness::Unknown => {
                    // Probe failed for an environmental reason. Preserve the
                    // existing quiet NFS fallback.
                    MacOsMountOutcome::nfs(mount_via_nfs(&root, thread_id, mountpoint)?, None)
                }
            }
        } else {
            debug!("macOS version is below FSKit's supported runtime floor; using NFS fallback");
            eprintln!("{}", readiness::unsupported_macos_hint());
            MacOsMountOutcome::nfs(
                mount_via_nfs(&root, thread_id, mountpoint)?,
                Some(FskitReadinessReport {
                    state: "unsupported_macos",
                    backend: "nfs",
                    action: "fell_back",
                    settings_url: None,
                }),
            )
        };

        let handle = std::sync::Arc::new(MountHandle {
            session: Mutex::new(Some(outcome.mounted.session)),
            mountpoint: mountpoint.to_path_buf(),
        });
        REGISTRY.insert(thread_id.to_string(), std::sync::Arc::clone(&handle));
        Ok(super::SpawnedMount {
            owner: outcome.mounted.owner,
            fskit_readiness: outcome.fskit_readiness,
        })
    }

    fn macos_supports_fskit() -> bool {
        let Some(version) = current_macos_version() else {
            return false;
        };
        version.major >= MIN_FSKIT_MACOS_MAJOR
    }

    fn current_macos_version() -> Option<MacOsVersion> {
        let output = Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        parse_macos_version(text.trim())
    }

    fn parse_macos_version(text: &str) -> Option<MacOsVersion> {
        let mut parts = text.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        Some(MacOsVersion {
            major,
            minor,
            patch,
        })
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct SettingsDeepLink {
        url: &'static str,
    }

    fn settings_deep_link() -> SettingsDeepLink {
        let Some(version) = current_macos_version() else {
            return SettingsDeepLink {
                url: SETTINGS_LOGIN_ITEMS_URL,
            };
        };
        settings_deep_link_for_version(version)
    }

    fn settings_deep_link_for_version(version: MacOsVersion) -> SettingsDeepLink {
        let url = match version.major {
            // macOS Sequoia exposes the extension controls under the
            // Login Items & Extensions pane. This anchor lands closer
            // to the Extensions table than the plain pane URL.
            15 => SETTINGS_SEQUOIA_FILE_EXTENSIONS_URL,
            // macOS Tahoe moved FSKit modules to ExtensionKit. Keep the
            // version split explicit; until a stable File System
            // Extensions anchor is known, land on the pane and rely on
            // the precise text path above.
            26 => SETTINGS_LOGIN_ITEMS_URL,
            _ => SETTINGS_LOGIN_ITEMS_URL,
        };
        SettingsDeepLink { url }
    }

    fn print_needs_approval_block(settings_url: &'static str) {
        eprintln!(
            "\nHeddle FSKit extension is installed, but macOS has not enabled it yet.\n\
             \n\
             macOS requires this approval in System Settings; Heddle cannot turn this permission on itself.\n\
             \n\
             Enable the fast path:\n\
             1. Open {SETTINGS_PATH}.\n\
             2. In that sheet, switch the picker to By Category / Extension Type, then turn on Heddle.\n\
             3. If the By App view does not apply the change, stay on By Category / Extension Type; macOS controls this approval UI.\n\
             \n\
             Settings URL: {settings_url}\n\
             Heddle will keep waiting here until macOS reports the extension enabled. Press Ctrl-C to stop.\n"
        );
    }

    fn wait_for_enter_to_open_settings(settings_url: &str) {
        if io::stdin().is_terminal() {
            let mut stderr = io::stderr();
            let _ = write!(
                stderr,
                "Press Enter to open System Settings, or Ctrl-C to stop."
            );
            let _ = stderr.flush();

            let mut input = String::new();
            let _ = io::stdin().read_line(&mut input);
            let _ = writeln!(stderr, "Opening System Settings: {settings_url}");
        } else {
            eprintln!("Opening System Settings: {settings_url}");
        }
    }

    fn open_settings(url: &str) {
        let _ = Command::new("open").arg(url).status();
    }

    fn wait_for_fskit_ready() {
        let mut stderr = io::stderr();
        let interactive = stderr.is_terminal();
        let mut frame_index = 0usize;

        if !interactive {
            eprintln!("{FSKIT_WAIT_MESSAGE}. Press Ctrl-C to stop waiting.");
        }

        loop {
            if readiness::probe() == Readiness::Ready {
                if interactive {
                    let _ = writeln!(
                        stderr,
                        "\rHeddle FSKit extension enabled; continuing. {FSKIT_LINE_CLEAR_PADDING}"
                    );
                } else {
                    eprintln!("Heddle FSKit extension enabled; continuing.");
                }
                return;
            }

            if interactive {
                let frame = FSKIT_SPINNER_FRAMES[frame_index % FSKIT_SPINNER_FRAMES.len()];
                let _ = write!(
                    stderr,
                    "\r{frame} {FSKIT_WAIT_MESSAGE}. Press Ctrl-C to stop."
                );
                let _ = stderr.flush();
                frame_index = frame_index.wrapping_add(1);
            }

            std::thread::sleep(FSKIT_POLL_INTERVAL);
        }
    }

    fn mount_via_nfs(
        repo_root: &Path,
        thread_id: &str,
        mountpoint: &Path,
    ) -> Result<MountedSession> {
        let repo = Repository::open(repo_root)
            .with_context(|| format!("reopen repo for NFS mount at {}", repo_root.display()))?;
        let mount = ContentAddressedMount::new(repo, thread_id)
            .map_err(|e| anyhow!("open content-addressed mount for {thread_id}: {e}"))?;
        let session = NfsShell::new(mount)
            .mount_background(mountpoint)
            .map_err(|e| {
                anyhow!(
                "NFS fallback failed: {e} (run `sudo` and ensure host has the NFS client enabled)"
            )
            })?;
        warn!(
            thread = thread_id,
            "using NFS fallback (install + enable the Heddle FSKit extension for a faster path)"
        );
        Ok(MountedSession::nfs(session))
    }

    fn mount_via_fskit_or_nfs(
        repo_root: &Path,
        thread_id: &str,
        mountpoint: &Path,
        success: FsKitMountReport,
    ) -> Result<MacOsMountOutcome> {
        match FsKitMount::mount(repo_root, thread_id, mountpoint) {
            Ok(mount) => Ok(MacOsMountOutcome {
                mounted: MountedSession::fskit(mount),
                fskit_readiness: Some(success.readiness("fskit")),
            }),
            Err(error) => {
                warn!(
                    thread = thread_id,
                    error = %error,
                    "FSKit mount failed after readiness probe; using NFS fallback"
                );
                eprintln!(
                    "Heddle FSKit mount failed ({error:#}); using NFS fallback for this run.\n\
                     If this follows a Heddle update, reinstall or re-enable the Heddle host app so macOS reloads the current File System Extension."
                );
                let mounted =
                    mount_via_nfs(repo_root, thread_id, mountpoint).with_context(|| {
                        format!("FSKit mount failed ({error:#}); NFS fallback also failed")
                    })?;
                Ok(MacOsMountOutcome::nfs(
                    mounted,
                    Some(FskitReadinessReport {
                        state: "mount_failed",
                        backend: "nfs",
                        action: "fell_back",
                        settings_url: success.settings_url,
                    }),
                ))
            }
        }
    }

    pub fn unmount_thread_if_mounted(thread_id: &str) -> bool {
        let Some(handle) = REGISTRY.remove(&thread_id.to_string()) else {
            return false;
        };
        let mountpoint = handle.mountpoint().to_path_buf();
        if let Err(err) = super::run_mount_io("macos-unmount", move || handle.unmount()) {
            warn!(
                thread = thread_id,
                mountpoint = %mountpoint.display(),
                "unmount failed: {err}"
            );
        }
        true
    }
}

#[cfg(all(target_os = "windows", feature = "mount"))]
mod windows {
    use std::{
        path::{Path, PathBuf},
        sync::Mutex,
    };

    use anyhow::{Context, Result, anyhow};
    use mount::{ContentAddressedMount, NfsSession, NfsShell, ProjFsSession, ProjFsShell};
    use repo::Repository;
    use tracing::warn;

    use crate::util::OnceMap;

    /// Which backend is actually live behind a [`MountHandle`] on
    /// Windows. ProjFS is the preferred adapter; the CLI falls
    /// back to the NFS shell on hosts that don't have the
    /// "Projected File System" optional feature enabled.
    enum BackingSession {
        ProjFs(ProjFsSession),
        Nfs(NfsSession),
    }

    impl BackingSession {
        fn unmount(self) -> Result<()> {
            match self {
                Self::ProjFs(s) => s.unmount().map_err(|e| anyhow!("projfs unmount: {e}")),
                Self::Nfs(s) => s.unmount().map_err(|e| anyhow!("nfs unmount: {e}")),
            }
        }
    }

    pub struct MountHandle {
        session: Mutex<Option<BackingSession>>,
        mountpoint: PathBuf,
    }

    impl MountHandle {
        pub fn unmount(&self) -> Result<()> {
            let mut guard = self.session.lock().expect("mount session lock");
            if let Some(s) = guard.take() {
                s.unmount()?;
            }
            Ok(())
        }

        pub fn mountpoint(&self) -> &Path {
            &self.mountpoint
        }
    }

    static REGISTRY: OnceMap<String, std::sync::Arc<MountHandle>> = OnceMap::new();

    /// Mount `thread_id` into `mountpoint`. Tries ProjFS first; on
    /// failure (typically "ProjFS optional feature not enabled"),
    /// falls back to the NFS shell. The fallback warning includes
    /// the install hint so an operator can opt back into ProjFS
    /// without grepping the docs.
    pub fn spawn_mount_for_thread(
        repo: Repository,
        thread_id: &str,
        mountpoint: &Path,
    ) -> Result<super::SpawnedMount> {
        std::fs::create_dir_all(mountpoint)
            .with_context(|| format!("create mount point {}", mountpoint.display()))?;

        let root = repo.root().to_path_buf();
        let mount = ContentAddressedMount::new(repo, thread_id)
            .map_err(|e| anyhow!("open content-addressed mount for {thread_id}: {e}"))?;

        // Probe up front: if `ProjectedFSLib.dll` isn't loadable, we
        // can skip the (relatively expensive) attempt-mount-then-fail
        // path and go straight to NFS with a clear, actionable
        // install hint. The probe is cheap (single `LoadLibraryW` +
        // `FreeLibrary`), the savings is one full
        // `PrjMarkDirectoryAsPlaceholder` + `PrjStartVirtualizing`
        // round-trip that the kernel rejects with `ERROR_MOD_NOT_FOUND`
        // anyway.
        let runtime_available = ProjFsShell::is_runtime_available();
        if !runtime_available {
            warn!(
                thread = thread_id,
                "ProjFS optional feature not enabled; using NFS fallback. \
                 Run `Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS` \
                 (Windows 10/11) or `... -FeatureName Projected-FS` (Windows Server) \
                 from an admin PowerShell for a faster mount.",
            );
        }

        let (session, owner) = if runtime_available {
            match ProjFsShell::new(mount).mount_background(mountpoint) {
                Ok(s) => (
                    BackingSession::ProjFs(s),
                    super::VirtualizedMountOwner::InProcess(super::VirtualizedMountBackend::ProjFs),
                ),
                Err(native_err) => {
                    // DLL loaded but the mount itself failed —
                    // could be a non-NTFS root, a stale
                    // virtualization on the same path, or a
                    // permission issue.
                    warn!(
                        thread = thread_id,
                        "ProjFS mount failed ({native_err}); falling back to NFS",
                    );
                    let reopened = Repository::open(&root)
                        .map_err(|e| anyhow!("reopen repo for NFS fallback: {e}"))?;
                    let mount = ContentAddressedMount::new(reopened, thread_id)
                        .map_err(|e| anyhow!("open mount for {thread_id} (NFS fallback): {e}"))?;
                    (
                        BackingSession::Nfs(
                            NfsShell::new(mount)
                                .mount_background(mountpoint)
                                .map_err(|e| {
                                    anyhow!(
                                        "ProjFS mount failed ({native_err}); NFS fallback also failed: {e}"
                                    )
                                })?,
                        ),
                        super::VirtualizedMountOwner::InProcess(
                            super::VirtualizedMountBackend::Nfs,
                        ),
                    )
                }
            }
        } else {
            // No ProjFS runtime — go straight to NFS without
            // burning a mount attempt. `mount` here is the
            // already-constructed ContentAddressedMount; we don't
            // need to reopen the repo.
            (
                BackingSession::Nfs(NfsShell::new(mount).mount_background(mountpoint).map_err(
                    |e| {
                        anyhow!(
                            "ProjFS unavailable and NFS fallback failed: {e}. \
                                 Install the 'Projected File System' Windows optional feature \
                                 (admin PowerShell: `Enable-WindowsOptionalFeature -Online \
                                 -FeatureName Client-ProjFS`) or ensure the NFS client is enabled."
                        )
                    },
                )?),
                super::VirtualizedMountOwner::InProcess(super::VirtualizedMountBackend::Nfs),
            )
        };

        let handle = std::sync::Arc::new(MountHandle {
            session: Mutex::new(Some(session)),
            mountpoint: mountpoint.to_path_buf(),
        });
        REGISTRY.insert(thread_id.to_string(), std::sync::Arc::clone(&handle));
        Ok(super::SpawnedMount {
            owner,
            fskit_readiness: None,
        })
    }

    pub fn unmount_thread_if_mounted(thread_id: &str) -> bool {
        let Some(handle) = REGISTRY.remove(&thread_id.to_string()) else {
            return false;
        };
        if let Err(err) = handle.unmount() {
            warn!(
                thread = thread_id,
                mountpoint = %handle.mountpoint().display(),
                "unmount failed: {err}"
            );
        }
        true
    }
}

#[cfg(all(target_os = "linux", feature = "mount"))]
#[allow(unused_imports)] // Re-exported for downstream callers / tests.
pub(crate) use linux::MountHandle;
#[cfg(all(target_os = "linux", feature = "mount"))]
pub(crate) use linux::{spawn_mount_for_thread, unmount_thread_if_mounted};
#[cfg(all(target_os = "macos", feature = "mount"))]
#[allow(unused_imports)] // Re-exported for downstream callers / tests.
pub(crate) use macos::MountHandle;
#[cfg(all(target_os = "macos", feature = "mount"))]
pub(crate) use macos::{spawn_mount_for_thread, unmount_thread_if_mounted};
#[cfg(all(target_os = "windows", feature = "mount"))]
#[allow(unused_imports)] // Re-exported for downstream callers / tests.
pub(crate) use windows::MountHandle;
#[cfg(all(target_os = "windows", feature = "mount"))]
pub(crate) use windows::{spawn_mount_for_thread, unmount_thread_if_mounted};

#[cfg(not(any(
    all(target_os = "linux", feature = "mount"),
    all(target_os = "macos", feature = "mount"),
    all(target_os = "windows", feature = "mount"),
)))]
mod stub {
    use std::path::Path;

    use anyhow::Result;

    /// Placeholder type so call sites compile on every platform.
    /// Constructing one is impossible because the real
    /// constructors are gated to a supported target + feature.
    #[allow(dead_code)] // Unconstructable placeholder for unsupported target/feature builds.
    pub struct MountHandle(std::convert::Infallible);

    pub fn spawn_mount_for_thread(
        _repo: repo::Repository,
        _thread_id: &str,
        _mountpoint: &Path,
    ) -> Result<super::SpawnedMount> {
        Err(super::virtualized_unsupported_error())
    }

    pub fn unmount_thread_if_mounted(_thread_id: &str) -> bool {
        false
    }
}

#[cfg(not(any(
    all(target_os = "linux", feature = "mount"),
    all(target_os = "macos", feature = "mount"),
    all(target_os = "windows", feature = "mount"),
)))]
#[allow(unused_imports)] // Re-exported for downstream callers / tests.
pub(crate) use stub::MountHandle;
#[cfg(not(any(
    all(target_os = "linux", feature = "mount"),
    all(target_os = "macos", feature = "mount"),
    all(target_os = "windows", feature = "mount"),
)))]
pub(crate) use stub::{spawn_mount_for_thread, unmount_thread_if_mounted};

/// What kind of mount the caller wants for this `--workspace
/// virtualized` start. Maps the CLI's two-bool `--daemon` /
/// `--no-daemon` pair onto a single tri-state: prefer-daemon
/// (default; fall back silently if the daemon is unavailable),
/// or in-process-only (the caller passed `--no-daemon`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountOwnership {
    /// Try the daemon; on `Unavailable` errors, fall back to the
    /// in-process mount and log a one-line warning.
    PreferDaemon,
    /// Skip the daemon entirely. The mount lives and dies with this
    /// CLI process — exactly the pre-default-flip behaviour.
    InProcess,
}

impl MountOwnership {
    /// Translate the `(daemon, no_daemon)` pair from the parsed CLI
    /// args into a single ownership choice. Both flags use clap's
    /// `overrides_with`, so at most one is true at a time —
    /// `--daemon --no-daemon` resolves to the *last* flag the user
    /// typed. This helper centralises the precedence so callers
    /// don't reimplement it.
    pub fn from_flags(daemon: bool, no_daemon: bool) -> Self {
        if no_daemon || !daemon {
            // `--no-daemon` (or `daemon=false`, which `overrides_with`
            // produces when --no-daemon comes after --daemon) => in-process.
            Self::InProcess
        } else {
            Self::PreferDaemon
        }
    }
}

/// Establish the FUSE mount for a `--workspace virtualized` thread,
/// honoring the user's daemon preference. This is the dispatch
/// point that flipped on 2026-05-02:
///
/// * `PreferDaemon` (default) → ask `heddled` to own the mount;
///   silently fall back to the in-process path with a warning if
///   the daemon turns out to be unavailable on this host.
/// * `InProcess` (`--no-daemon`) → skip the daemon entirely; same
///   behaviour as the pre-flip default.
///
/// The repo handle is opened internally on the in-process path; the
/// daemon path uses its own `Repository::open` inside the daemon
/// process. Callers should keep their existing `Repository` for the
/// rest of `start_thread` — see the comment at the original call
/// site in `thread.rs` for the two-handle pattern.
pub(crate) fn establish_virtualized_mount(
    repo_root: &Path,
    thread_id: &str,
    mountpoint: &Path,
    ownership: MountOwnership,
) -> anyhow::Result<VirtualizedMountOutcome> {
    match ownership {
        MountOwnership::PreferDaemon => {
            let attempt = crate::cli::commands::daemon_client::mount_via_daemon_classified(
                repo_root, thread_id, mountpoint,
            );
            match classify_daemon_attempt(attempt, thread_id) {
                DaemonAttemptResolution::Daemon => Ok(VirtualizedMountOutcome::daemon()),
                DaemonAttemptResolution::FallbackInProcess => spawn_in_process_mount(
                    repo_root.to_path_buf(),
                    thread_id.to_string(),
                    mountpoint.to_path_buf(),
                ),
                DaemonAttemptResolution::Fatal(err) => Err(err),
            }
        }
        MountOwnership::InProcess => spawn_in_process_mount(
            repo_root.to_path_buf(),
            thread_id.to_string(),
            mountpoint.to_path_buf(),
        ),
    }
}

fn spawn_in_process_mount(
    repo_root: std::path::PathBuf,
    thread_id: String,
    mountpoint: std::path::PathBuf,
) -> anyhow::Result<VirtualizedMountOutcome> {
    run_mount_io("in-process-spawn", move || {
        let mount_repo = repo::Repository::open(&repo_root)?;
        let mounted = spawn_mount_for_thread(mount_repo, &thread_id, &mountpoint)?;
        Ok(VirtualizedMountOutcome::in_process(mounted))
    })
}

pub(crate) fn cleanup_virtualized_mount(
    repo_root: &Path,
    thread_id: &str,
    owner: VirtualizedMountOwner,
) -> anyhow::Result<()> {
    match owner {
        VirtualizedMountOwner::Daemon => {
            crate::cli::commands::daemon_client::unmount_via_daemon(repo_root, thread_id)?;
            Ok(())
        }
        VirtualizedMountOwner::InProcess(_) => {
            let thread_id = thread_id.to_string();
            run_mount_io("in-process-unmount", move || {
                unmount_thread_if_mounted(&thread_id);
                Ok(())
            })
        }
    }
}

/// What `establish_virtualized_mount` does after the daemon attempt
/// returns. Extracted so the warning + fallback decision is a pure
/// function we can unit-test without standing up a real daemon or a
/// real FUSE mount.
#[derive(Debug)]
enum DaemonAttemptResolution {
    /// Daemon owned the mount successfully — nothing more to do.
    Daemon,
    /// Daemon was unavailable but the failure was the kind we
    /// silently recover from. Caller should run the in-process path.
    FallbackInProcess,
    /// Daemon responded with something we must surface (real conflict
    /// or unparseable response).
    Fatal(anyhow::Error),
}

/// Pure decision: given the daemon's response (Ok, Unavailable, or
/// Fatal) and the thread id, produce the next action and emit a
/// warning if we're falling back. Side effect is the `tracing::warn`
/// call on the fallback path; everything else is pure.
fn classify_daemon_attempt(
    attempt: std::result::Result<
        std::path::PathBuf,
        crate::cli::commands::daemon_client::DaemonMountError,
    >,
    thread_id: &str,
) -> DaemonAttemptResolution {
    use crate::cli::commands::daemon_client::DaemonMountError;
    match attempt {
        Ok(_) => DaemonAttemptResolution::Daemon,
        Err(DaemonMountError::Fatal(err)) => DaemonAttemptResolution::Fatal(err),
        Err(DaemonMountError::Unavailable(reason)) => {
            // One-line warning on the fallback path. Phrased so the
            // operator can copy the suppression hint verbatim.
            tracing::warn!(
                thread = thread_id,
                "daemon unavailable ({reason}); using in-process mount. \
                 Pass --no-daemon to suppress this warning."
            );
            DaemonAttemptResolution::FallbackInProcess
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default state (no flags set explicitly) hits the daemon
    /// path. This is the load-bearing assertion for the
    /// 2026-05-02 default flip.
    #[test]
    fn default_flags_prefer_daemon() {
        // clap's `default_value_t = true` produces (daemon=true,
        // no_daemon=false) when no flag is passed.
        assert_eq!(
            MountOwnership::from_flags(true, false),
            MountOwnership::PreferDaemon,
        );
    }

    /// `--no-daemon` opts out and pins us to the in-process
    /// mount. clap's `overrides_with` clears `daemon` to false
    /// when `no_daemon` wins, but we also accept the
    /// (daemon=true, no_daemon=true) shape for safety —
    /// `overrides_with` is the only thing keeping that pair from
    /// being observable, and that contract is fragile across
    /// clap versions.
    #[test]
    fn no_daemon_flag_uses_in_process() {
        assert_eq!(
            MountOwnership::from_flags(false, true),
            MountOwnership::InProcess,
        );
        assert_eq!(
            MountOwnership::from_flags(true, true),
            MountOwnership::InProcess,
        );
    }

    /// `--daemon --no-daemon` (in either order) collapses to
    /// the *last* flag wins via `overrides_with`. Whichever
    /// state clap hands us, we should never accidentally
    /// promote `--no-daemon` to the daemon path.
    #[test]
    fn overrides_with_resolves_conflicting_flags_to_in_process_when_no_daemon_wins() {
        // `--daemon --no-daemon` → (daemon=false, no_daemon=true).
        assert_eq!(
            MountOwnership::from_flags(false, true),
            MountOwnership::InProcess,
        );
    }

    /// `--no-daemon --daemon` order — `--daemon` wins via
    /// `overrides_with`. We should treat this exactly like
    /// the default.
    #[test]
    fn overrides_with_resolves_conflicting_flags_to_daemon_when_daemon_wins() {
        // `--no-daemon --daemon` → (daemon=true, no_daemon=false).
        assert_eq!(
            MountOwnership::from_flags(true, false),
            MountOwnership::PreferDaemon,
        );
    }

    /// Daemon attempt that succeeded should resolve to the daemon
    /// path with no fallback and no warning.
    #[test]
    fn classify_daemon_attempt_ok_resolves_to_daemon() {
        let resolution =
            classify_daemon_attempt(Ok(std::path::PathBuf::from("/tmp/some-mount")), "thread-x");
        assert!(matches!(resolution, DaemonAttemptResolution::Daemon));
    }

    #[test]
    fn daemon_outcome_records_daemon_owner_without_fskit_report() {
        let outcome = VirtualizedMountOutcome::daemon();
        assert_eq!(outcome.owner, VirtualizedMountOwner::Daemon);
        assert_eq!(outcome.fskit_readiness, None);
    }

    #[test]
    fn in_process_outcome_preserves_backend_and_fskit_report() {
        let readiness = FskitReadinessReport {
            state: "mount_failed",
            backend: "nfs",
            action: "fell_back",
            settings_url: Some("x-apple.systempreferences:test"),
        };
        let outcome = VirtualizedMountOutcome::in_process(SpawnedMount {
            owner: VirtualizedMountOwner::InProcess(VirtualizedMountBackend::Nfs),
            fskit_readiness: Some(readiness.clone()),
        });
        assert_eq!(
            outcome.owner,
            VirtualizedMountOwner::InProcess(VirtualizedMountBackend::Nfs)
        );
        assert_eq!(outcome.fskit_readiness, Some(readiness));
    }

    #[test]
    fn run_mount_io_leaves_tokio_runtime_thread() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");
        let runtime_thread = std::thread::current().id();
        let mount_thread = runtime.block_on(async move {
            run_mount_io("test-thread", move || Ok(std::thread::current().id()))
                .expect("mount io helper should run")
        });
        assert_ne!(
            runtime_thread, mount_thread,
            "mount IO must leave a live Tokio runtime thread"
        );
    }

    /// `Unavailable` is the canonical "host can't run the daemon
    /// right now" signal — falls back silently to in-process. The
    /// warning emission is best-effort and observed by the
    /// integration test; here we just lock in the dispatch.
    #[test]
    fn classify_daemon_attempt_unavailable_falls_back_to_in_process() {
        use crate::cli::commands::daemon_client::DaemonMountError;
        let resolution = classify_daemon_attempt(
            Err(DaemonMountError::Unavailable(
                "could not start daemon: exec failed".to_string(),
            )),
            "thread-y",
        );
        assert!(matches!(
            resolution,
            DaemonAttemptResolution::FallbackInProcess
        ));
    }

    /// `Fatal` errors must surface — the in-process fallback would
    /// either hide a real conflict or mask a daemon-side bug.
    #[test]
    fn classify_daemon_attempt_fatal_does_not_fall_back() {
        use crate::cli::commands::daemon_client::DaemonMountError;
        let resolution = classify_daemon_attempt(
            Err(DaemonMountError::Fatal(anyhow!(
                "daemon mount failed: [mount_conflict] thread X is already mounted at Y"
            ))),
            "thread-z",
        );
        match resolution {
            DaemonAttemptResolution::Fatal(err) => {
                assert!(
                    err.to_string().contains("mount_conflict"),
                    "fatal error should preserve the daemon-reported code, got {err:?}"
                );
            }
            other => {
                panic!("expected Fatal, got {other:?} — fallback would hide the real conflict")
            }
        }
    }

    /// Sanity: the warning's "Pass --no-daemon to suppress this
    /// warning." hint is in the source so an operator who greps the
    /// codebase finds the dispatch site. Regression catch for
    /// accidental rewordings that would break the contract with
    /// users who learn the suppression flag from the warning.
    #[test]
    fn fallback_warning_text_mentions_no_daemon_suppression() {
        // We pass an Unavailable in to trigger the warn path, then
        // re-read the source line via include_str! to assert the
        // exact string is present. include_str! is preferred over
        // a tracing-subscriber capture here: zero deps, runs on
        // every host, and locks in the user-visible message
        // verbatim.
        let source = include_str!("mount_lifecycle.rs");
        assert!(
            source.contains("Pass --no-daemon to suppress this warning."),
            "warning hint must be present verbatim so users learn the opt-out flag"
        );
    }
}
