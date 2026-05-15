// SPDX-License-Identifier: Apache-2.0
//! In-daemon `MountRegistry`: the live FUSE sessions plus the
//! atomic mirror written to `mounts.json` so a stale-endpoint
//! sweep from a future CLI invocation can clean up after a daemon
//! crash.
//!
//! Linux + `--features mount` only. The sibling `cmd::serve_unsupported`
//! shim handles every other platform.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use mount::{BlobCachePool, ContentAddressedMount, FuseShell, MountOptions, PrewarmHandle};
use repo::{
    daemon::{mount_daemon_registry_path, MountRegistryFile, PersistedMount},
    Repository,
};
use tracing::{debug, warn};

/// Default blob-cache cap for the daemon: `min(4 GiB, 25% of
/// physical RAM)`. Sized large enough that a typical agent
/// workspace fits without pre-warmer churn, capped so the daemon
/// doesn't dominate machine RSS on memory-constrained hosts.
/// Probed once via `sysctl hw.memsize` (macOS) / `sysinfo`
/// (Linux); on probe failure falls back to a 1 GiB heuristic that
/// matches the typical M-series laptop sweet spot.
fn default_blob_cache_cap_bytes() -> usize {
    const FOUR_GIB: usize = 4 * 1024 * 1024 * 1024;
    const FALLBACK: usize = 1024 * 1024 * 1024;
    let physical = probe_physical_ram_bytes().unwrap_or(0);
    if physical == 0 {
        return FALLBACK;
    }
    std::cmp::min(FOUR_GIB, physical / 4)
}

/// Best-effort physical-RAM probe. `None` when we can't determine
/// the host's memory size; callers fall back to a fixed default.
fn probe_physical_ram_bytes() -> Option<usize> {
    #[cfg(target_os = "macos")]
    {
        // `sysctl hw.memsize` returns a u64 in bytes. Wired this
        // way (rather than via the `sysctl` crate) to avoid pulling
        // in a dep for a single byte-shaped value.
        use std::process::Command;
        let out = Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = std::str::from_utf8(&out.stdout).ok()?.trim();
        s.parse::<usize>().ok()
    }
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                let n = rest.split_whitespace().next()?.parse::<usize>().ok()?;
                return Some(n * 1024); // /proc/meminfo reports KiB
            }
        }
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Active mount entry inside the daemon. The `BackgroundSession`
/// drop is what triggers the unmount, so we keep it alive in the
/// `Mutex`-guarded map until either an explicit unmount RPC or
/// daemon shutdown.
pub struct LiveMount {
    /// `BackgroundSession` is held in an `Option` to make explicit
    /// drop-via-take possible.
    session: Option<mount::BackgroundSession>,
    pub mount_path: PathBuf,
    pub since_ms: u64,
    /// Pre-warm handle. Held only so the prewarmer keeps running
    /// until the cache is filled (or the cache is ≥90% full). On
    /// unmount the handle drops and the workers cancel-and-join.
    /// `None` after the handle has been moved out by shutdown.
    _prewarm: Option<PrewarmHandle>,
}

impl LiveMount {
    /// Drop the FUSE session — the destructor sends `unmount` to
    /// the kernel. Best-effort; we never block thread drop on a
    /// flaky FS layer.
    pub fn shutdown(&mut self) {
        self.session = None;
        self._prewarm = None;
    }
}

/// Outcome of [`MountRegistry::mount`]. `Existing` means the
/// daemon was already holding this thread at the same path, so
/// the call was idempotent.
pub enum MountOutcome {
    Created,
    Existing,
}

/// Daemon-local registry of live mounts, keyed by thread_id.
pub struct MountRegistry {
    repo_root: PathBuf,
    mounts: HashMap<String, LiveMount>,
    /// Process-shared blob cache pool. Every mount we spawn gets the
    /// same `Arc<BlobCachePool>`, so forked-thread mounts inherit
    /// fully-warm cache for any blob a sibling already touched.
    /// Sized once at daemon construction based on available RAM.
    blob_cache: Arc<BlobCachePool>,
}

impl MountRegistry {
    pub fn new(repo_root: PathBuf) -> Self {
        Self::with_blob_cache_capacity(repo_root, default_blob_cache_cap_bytes())
    }

    /// Construct with an explicit blob-cache cap. Daemon `main`
    /// uses this when sizing the cache from `sysctl hw.memsize` /
    /// `/proc/meminfo`; tests use the default-cap shim above.
    pub fn with_blob_cache_capacity(repo_root: PathBuf, cap_bytes: usize) -> Self {
        Self {
            repo_root,
            mounts: HashMap::new(),
            blob_cache: Arc::new(BlobCachePool::with_capacity(cap_bytes)),
        }
    }

    /// Share the daemon's blob cache pool with other in-process
    /// consumers (e.g. a future direct-RPC read endpoint).
    pub fn blob_cache_pool(&self) -> &Arc<BlobCachePool> {
        &self.blob_cache
    }

    /// Companion to `len()` for symmetry; reserved for the daemon's
    /// idle-shutdown path, which is currently driven directly off the
    /// JSON file rather than the live registry. Tagged `#[allow]` so
    /// it survives `-D dead-code` until that wiring lands.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.mounts.len()
    }

    /// Mount `thread_id` at `mount_path`. Idempotent for matching
    /// thread/path pairs; conflicting paths return a structured
    /// error so the CLI can surface it to the user.
    pub fn mount(&mut self, thread_id: &str, mount_path: &Path) -> Result<MountOutcome> {
        if let Some(existing) = self.mounts.get(thread_id) {
            return if existing.mount_path == mount_path {
                Ok(MountOutcome::Existing)
            } else {
                Err(anyhow!(
                    "thread '{thread_id}' is already mounted at {} (requested {})",
                    existing.mount_path.display(),
                    mount_path.display(),
                ))
            };
        }

        fs::create_dir_all(mount_path)
            .with_context(|| format!("create mount point {}", mount_path.display()))?;

        let repo = Repository::open(&self.repo_root)
            .with_context(|| format!("open repo at {} for mount", self.repo_root.display()))?;
        let mount = ContentAddressedMount::with_options(
            repo,
            thread_id,
            MountOptions {
                blob_cache: Some(Arc::clone(&self.blob_cache)),
            },
        )
        .map_err(|e| anyhow!("open content-addressed mount for {thread_id}: {e}"))?;
        // Kick off a background tree-walker so the agent's first
        // reads land on a hot cache. Handle stays in `LiveMount`
        // until shutdown so the prewarmer can run to completion.
        let prewarm = mount.prewarm();
        let shell = FuseShell::new(mount);
        let session = shell.mount_background(mount_path).map_err(|e| {
            anyhow!(
                "spawn FUSE background session at {}: {e}",
                mount_path.display()
            )
        })?;

        let since_ms = current_millis();
        self.mounts.insert(
            thread_id.to_string(),
            LiveMount {
                session: Some(session),
                mount_path: mount_path.to_path_buf(),
                since_ms,
                _prewarm: Some(prewarm),
            },
        );
        self.persist()?;
        debug!(thread = thread_id, path = %mount_path.display(), "mount registered");
        Ok(MountOutcome::Created)
    }

    /// Tear down the mount for `thread_id` if any. Returns `true`
    /// when there was a mount to remove. Persists the registry on
    /// success so a later stale-endpoint sweep doesn't try to
    /// unmount this path again.
    pub fn unmount(&mut self, thread_id: &str) -> Result<bool> {
        let Some(mut live) = self.mounts.remove(thread_id) else {
            return Ok(false);
        };
        live.shutdown();
        self.persist()?;
        debug!(thread = thread_id, "mount unregistered");
        Ok(true)
    }

    /// Tear down every live mount. Used on `shutdown` so the daemon
    /// doesn't leave wedged FUSE sessions behind on clean exit.
    /// Errors during persist are warned; we never abort shutdown
    /// on disk-write failure.
    pub fn shutdown_all(&mut self) {
        let drained: Vec<_> = self.mounts.drain().collect();
        for (thread_id, mut live) in drained {
            debug!(thread = %thread_id, path = %live.mount_path.display(), "unmounting on shutdown");
            live.shutdown();
        }
        if let Err(error) = self.persist() {
            warn!(%error, "failed to persist empty mount registry on shutdown");
        }
        // Best-effort: drop the registry file outright once we've
        // unmounted, so a future CLI doesn't see ghost entries.
        let _ = fs::remove_file(mount_daemon_registry_path(&self.repo_root));
    }

    /// Snapshot the registry for `list_mounts` / `health` RPCs.
    pub fn snapshot(&self) -> Vec<PersistedMount> {
        self.mounts
            .iter()
            .map(|(thread_id, live)| PersistedMount {
                thread_id: thread_id.clone(),
                mount_path: live.mount_path.clone(),
                pid: std::process::id(),
                since_ms: live.since_ms,
            })
            .collect()
    }

    /// Atomically rewrite `.heddle/state/mounts.json` to mirror the
    /// in-memory registry. Only invoked on mount/unmount transitions.
    fn persist(&self) -> Result<()> {
        let file = MountRegistryFile {
            mounts: self.snapshot(),
        };
        let path = mount_daemon_registry_path(&self.repo_root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&file).context("encode mount registry")?;
        objects::fs_atomic::write_file_atomic(&path, &bytes)
            .with_context(|| format!("persist mount registry at {}", path.display()))?;
        Ok(())
    }

    /// Test-only: inject a registry entry without spawning a real
    /// FUSE session. The idle-exit policy only checks `is_empty`,
    /// so this lets the unit tests in `server.rs` verify the
    /// keep-alive gate without requiring a kernel-level FUSE mount
    /// (which CI may not have).
    #[doc(hidden)]
    #[allow(non_snake_case)]
    pub fn __test_inject_phantom_mount(&mut self, thread_id: &str, mount_path: PathBuf) {
        self.mounts.insert(
            thread_id.to_string(),
            LiveMount {
                session: None,
                mount_path,
                since_ms: current_millis(),
                _prewarm: None,
            },
        );
    }
}

fn current_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Type alias used by the server layer to share the registry across
/// every accepted connection. The handler is single-threaded
/// (sequential accept loop) but we keep the lock for forward
/// compatibility with future async daemon work — when that lands the
/// `#[allow]` comes off.
#[allow(dead_code)]
pub type SharedRegistry = Arc<std::sync::Mutex<MountRegistry>>;
