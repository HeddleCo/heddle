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

use anyhow::{Context, Result, anyhow};
use mount::{ContentAddressedMount, FuseShell};
use repo::{
    Repository,
    daemon::{MountRegistryFile, PersistedMount, mount_daemon_registry_path},
};
use tracing::{debug, warn};

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
}

impl LiveMount {
    /// Drop the FUSE session — the destructor sends `unmount` to
    /// the kernel. Best-effort; we never block thread drop on a
    /// flaky FS layer.
    pub fn shutdown(&mut self) {
        self.session = None;
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
}

impl MountRegistry {
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            mounts: HashMap::new(),
        }
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
        let mount = ContentAddressedMount::new(repo, thread_id)
            .map_err(|e| anyhow!("open content-addressed mount for {thread_id}: {e}"))?;
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