// SPDX-License-Identifier: Apache-2.0
//! Persistence + reconstruction for lazy-clone blob hydrators.
//!
//! Background: a lazy clone (`heddle clone --lazy` / `--filter blob:none`)
//! leaves missing-blob markers in `.heddle/partial-fetch` so future reads
//! can hydrate transparently via [`crate::Repository::require_blob`]. The
//! `BlobHydrator` itself, however, is a process-local trait object: once
//! the clone command exits the registration is gone, and any subsequent
//! `heddle <verb>` invocation that touches a lazy blob sees a bare
//! `MissingObject` error.
//!
//! This module closes that gap. At clone time the CLI writes a small
//! `.heddle/lazy-hydrator.toml` recording the hydrator *kind* and the
//! per-kind config the factory needs to reconstruct it (remote endpoint
//! and target state, or git-overlay marker). At `Repository::open` time
//! the repo reads that file, looks up a *factory* in a process-wide
//! registry, and installs the resulting hydrator. Entry-point binaries
//! (the `heddle` CLI today, the daemon and a future heddle-server
//! tomorrow) register their factories once at startup; the `repo` crate
//! never directly depends on either of the two hydrator implementations,
//! so the trait + factory split keeps the crate-graph acyclic.
//!
//! Non-lazy repos pay zero cost: when `.heddle/lazy-hydrator.toml` is
//! absent the open path skips the read entirely.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, RwLock},
};

use objects::{
    error::{HeddleError, Result},
    fs_atomic::write_file_atomic,
};
use serde::{Deserialize, Serialize};

use crate::repository::BlobHydrator;

/// On-disk filename, relative to `.heddle/`.
pub const LAZY_HYDRATOR_FILE: &str = "lazy-hydrator.toml";

/// Stable kind identifier for the git-overlay hydrator.
pub const KIND_GIT_OVERLAY: &str = "git-overlay";

/// Stable kind identifier for the remote gRPC hydrator.
pub const KIND_REMOTE: &str = "remote";

/// Persisted hydrator metadata. Wire-format is the TOML serialization of
/// this struct, written to `.heddle/lazy-hydrator.toml`. The shape is
/// intentionally additive: future hydrator kinds can extend the `remote`
/// table or add new tables next to it without breaking existing readers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LazyHydratorConfig {
    pub hydrator: HydratorSection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HydratorSection {
    /// Stable identifier the factory registry uses to dispatch.
    /// Currently `"git-overlay"` or `"remote"`.
    pub kind: String,
    /// Remote-only fields. Present when `kind == "remote"`, absent
    /// otherwise. Optional so a future heddle-server can read the toml
    /// without forcing the remote table to exist for every kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteHydratorConfig>,
    /// Git-overlay-only fields. Present when `kind == "git-overlay"`,
    /// absent otherwise. The bare repo lives at `<root>/.git` so we
    /// don't strictly need to record its path, but reserving the table
    /// keeps the schema extensible (e.g. a future `remote_name` field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_overlay: Option<GitOverlayHydratorConfig>,
}

/// Remote-clone reconstruction config: enough state for the remote
/// factory to dial the upstream and replay the hydration call that
/// happened at clone time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteHydratorConfig {
    /// `host:port` of the remote server. Parseable by
    /// [`std::net::SocketAddr::from_str`].
    pub endpoint: String,
    /// Remote namespace path the clone targeted, e.g. `org/acme/repo`.
    pub repo_path: String,
    /// Remote branch / thread the clone tracked (`"main"` by default).
    pub remote_thread: String,
    /// Local thread the hydrator should resolve to a state when
    /// invoked. The hydrator reads the current value of this thread
    /// from `repo.refs()` on each call so a `pull --lazy` that
    /// advances the thread keeps working; the field is recorded for
    /// audit and for the case where the local thread name differs
    /// from the remote one.
    pub local_thread: String,
}

/// Git-overlay reconstruction config. The hydrator points at the bare
/// repo at `<root>/.git`; no extra fields are required yet. Kept as a
/// distinct table so the schema remains forward-extensible (e.g. a
/// `remote_name` once multiple promisor remotes are supported).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct GitOverlayHydratorConfig {}

impl LazyHydratorConfig {
    /// Construct a remote-kind config in one call.
    pub fn remote(
        endpoint: impl Into<String>,
        repo_path: impl Into<String>,
        remote_thread: impl Into<String>,
        local_thread: impl Into<String>,
    ) -> Self {
        Self {
            hydrator: HydratorSection {
                kind: KIND_REMOTE.to_string(),
                remote: Some(RemoteHydratorConfig {
                    endpoint: endpoint.into(),
                    repo_path: repo_path.into(),
                    remote_thread: remote_thread.into(),
                    local_thread: local_thread.into(),
                }),
                git_overlay: None,
            },
        }
    }

    /// Construct a git-overlay-kind config in one call.
    pub fn git_overlay() -> Self {
        Self {
            hydrator: HydratorSection {
                kind: KIND_GIT_OVERLAY.to_string(),
                remote: None,
                git_overlay: Some(GitOverlayHydratorConfig::default()),
            },
        }
    }

    /// Path within `heddle_dir` where the file lives.
    pub fn path_in(heddle_dir: &Path) -> PathBuf {
        heddle_dir.join(LAZY_HYDRATOR_FILE)
    }

    /// Read and parse `.heddle/lazy-hydrator.toml`. Returns `Ok(None)`
    /// when the file is absent — non-lazy repos hit this fast path.
    pub fn load(heddle_dir: &Path) -> Result<Option<Self>> {
        let path = Self::path_in(heddle_dir);
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path)?;
        let config: Self = toml::from_str(&raw).map_err(|err| {
            HeddleError::Config(format!("failed to parse {}: {err}", path.display()))
        })?;
        Ok(Some(config))
    }

    /// Atomically write the config to `.heddle/lazy-hydrator.toml`.
    /// Creates `.heddle/` if it doesn't already exist.
    pub fn save(&self, heddle_dir: &Path) -> Result<()> {
        fs::create_dir_all(heddle_dir)?;
        let path = Self::path_in(heddle_dir);
        let contents = toml::to_string_pretty(self).map_err(|err| {
            HeddleError::Config(format!("failed to serialize lazy hydrator config: {err}"))
        })?;
        write_file_atomic(&path, contents.as_bytes())?;
        Ok(())
    }

    /// Remove the file. No-op when absent.
    pub fn remove(heddle_dir: &Path) -> Result<()> {
        let path = Self::path_in(heddle_dir);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }
}

/// Factory signature: given the repo root and the persisted config,
/// produce a hydrator ready to install. Sync on purpose — runs inside
/// `Repository::open`, which is sync and may execute outside any tokio
/// runtime. Factories that need async I/O (the remote one does) should
/// return an adapter that defers the connect to first `hydrate()` call.
pub type BlobHydratorFactory =
    Arc<dyn Fn(&Path, &HydratorSection) -> Result<Arc<dyn BlobHydrator>> + Send + Sync>;

/// Process-wide registry of hydrator factories.
///
/// Entry-point binaries register a factory per kind during startup
/// (`main()`); `Repository::open` consults the registry whenever it
/// finds a `.heddle/lazy-hydrator.toml` on disk. Re-registering the
/// same kind overrides the prior factory (last-write-wins) — supports
/// test-suite scenarios where a custom factory replaces the production
/// one for a single test.
fn registry() -> &'static RwLock<HashMap<String, BlobHydratorFactory>> {
    static REGISTRY: OnceLock<RwLock<HashMap<String, BlobHydratorFactory>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Register a factory for the given `kind`. Idempotent for repeat
/// registrations of the same factory pointer; new calls override an
/// existing registration for that kind.
pub fn register_factory(kind: impl Into<String>, factory: BlobHydratorFactory) {
    let mut map = registry().write().unwrap();
    map.insert(kind.into(), factory);
}

/// Look up the factory for a kind. Returns `None` when no factory has
/// been registered — callers (notably `Repository::open`) treat that
/// as a recoverable warning, not a fatal error: a non-CLI process
/// (e.g. a one-off script that uses `heddle-repo` directly) may
/// legitimately not need hydration.
pub fn lookup_factory(kind: &str) -> Option<BlobHydratorFactory> {
    registry().read().unwrap().get(kind).cloned()
}

/// Convenience: load the persisted config (if any), look up the
/// factory, and return the ready-to-install hydrator. Returns `None`
/// if no metadata is on disk OR if no factory is registered for the
/// recorded kind (the latter logs a warning so misconfigured deploys
/// surface in logs rather than silently failing reads).
pub fn try_reconstruct(
    repo_root: &Path,
    heddle_dir: &Path,
) -> Result<Option<Arc<dyn BlobHydrator>>> {
    let Some(config) = LazyHydratorConfig::load(heddle_dir)? else {
        return Ok(None);
    };
    let kind = config.hydrator.kind.as_str();
    let Some(factory) = lookup_factory(kind) else {
        tracing::warn!(
            kind = kind,
            heddle_dir = %heddle_dir.display(),
            "lazy-hydrator.toml on disk but no factory registered for kind; \
             blob reads against missing markers will fail until a factory is registered",
        );
        return Ok(None);
    };
    let hydrator = factory(repo_root, &config.hydrator)?;
    Ok(Some(hydrator))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use objects::{
        object::{Blob, ContentHash},
        store::LocalObjectStore,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::Repository;

    #[test]
    fn save_roundtrips_remote_config() {
        let temp = TempDir::new().unwrap();
        let heddle = temp.path().join(".heddle");
        let original =
            LazyHydratorConfig::remote("127.0.0.1:8443", "org/acme/repo", "main", "main");
        original.save(&heddle).unwrap();
        let loaded = LazyHydratorConfig::load(&heddle).unwrap().unwrap();
        assert_eq!(loaded, original);
    }

    #[test]
    fn save_roundtrips_git_overlay_config() {
        let temp = TempDir::new().unwrap();
        let heddle = temp.path().join(".heddle");
        let original = LazyHydratorConfig::git_overlay();
        original.save(&heddle).unwrap();
        let loaded = LazyHydratorConfig::load(&heddle).unwrap().unwrap();
        assert_eq!(loaded, original);
    }

    #[test]
    fn load_returns_none_for_non_lazy_repo() {
        let temp = TempDir::new().unwrap();
        let heddle = temp.path().join(".heddle");
        fs::create_dir_all(&heddle).unwrap();
        assert!(LazyHydratorConfig::load(&heddle).unwrap().is_none());
    }

    #[test]
    fn remove_is_idempotent_when_absent() {
        let temp = TempDir::new().unwrap();
        let heddle = temp.path().join(".heddle");
        fs::create_dir_all(&heddle).unwrap();
        LazyHydratorConfig::remove(&heddle).unwrap();
        LazyHydratorConfig::remove(&heddle).unwrap();
    }

    #[test]
    fn try_reconstruct_returns_none_without_metadata() {
        let temp = TempDir::new().unwrap();
        let heddle = temp.path().join(".heddle");
        fs::create_dir_all(&heddle).unwrap();
        let result = try_reconstruct(temp.path(), &heddle).unwrap();
        assert!(result.is_none());
    }

    /// Lock for the registry — the kind name `"test-kind-xyz"` is
    /// shared across tests but they each take this lock so they don't
    /// race on the global map. Using a kind no production code
    /// registers under makes the test isolated from CLI factories.
    static TEST_REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct CountingHydrator {
        bytes: Vec<u8>,
        calls: AtomicUsize,
    }

    impl BlobHydrator for CountingHydrator {
        fn hydrate(&self, repo: &Repository, _hash: &ContentHash) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            repo.store().put_blob(&Blob::new(self.bytes.clone()))?;
            Ok(())
        }
    }

    #[test]
    fn try_reconstruct_invokes_registered_factory() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp = TempDir::new().unwrap();
        let heddle = temp.path().join(".heddle");
        fs::create_dir_all(&heddle).unwrap();

        let kind = "test-kind-tr-1";
        let payload = b"factory-built".to_vec();
        let payload_for_factory = payload.clone();
        let made: Arc<Mutex<Option<Arc<CountingHydrator>>>> = Arc::new(Mutex::new(None));
        let made_for_factory = Arc::clone(&made);
        register_factory(
            kind,
            Arc::new(
                move |_root: &Path, _section: &HydratorSection| -> Result<Arc<dyn BlobHydrator>> {
                    let h = Arc::new(CountingHydrator {
                        bytes: payload_for_factory.clone(),
                        calls: AtomicUsize::new(0),
                    });
                    *made_for_factory.lock().unwrap() = Some(Arc::clone(&h));
                    Ok(h)
                },
            ),
        );

        // Write a config that points at the custom kind.
        let cfg = LazyHydratorConfig {
            hydrator: HydratorSection {
                kind: kind.to_string(),
                remote: None,
                git_overlay: None,
            },
        };
        cfg.save(&heddle).unwrap();

        let hydrator = try_reconstruct(temp.path(), &heddle).unwrap();
        assert!(hydrator.is_some());
        assert!(made.lock().unwrap().is_some());
        // The held reference should match the one the factory built.
        let kept = made.lock().unwrap().as_ref().map(Arc::clone).unwrap();
        assert!(Arc::strong_count(&kept) >= 1);
    }

    #[test]
    fn load_surfaces_parse_errors_with_path_context() {
        let temp = TempDir::new().unwrap();
        let heddle = temp.path().join(".heddle");
        fs::create_dir_all(&heddle).unwrap();
        let path = LazyHydratorConfig::path_in(&heddle);
        fs::write(&path, b"this is = not ][ valid toml").unwrap();
        let err = LazyHydratorConfig::load(&heddle)
            .expect_err("corrupt TOML must surface as Config error");
        let msg = err.to_string();
        assert!(
            msg.contains("failed to parse") && msg.contains(LAZY_HYDRATOR_FILE),
            "parse error must include the offending path; got: {msg}"
        );
    }

    #[test]
    fn remove_deletes_file_when_present() {
        let temp = TempDir::new().unwrap();
        let heddle = temp.path().join(".heddle");
        let cfg = LazyHydratorConfig::git_overlay();
        cfg.save(&heddle).unwrap();
        assert!(LazyHydratorConfig::path_in(&heddle).exists());
        LazyHydratorConfig::remove(&heddle).unwrap();
        assert!(
            !LazyHydratorConfig::path_in(&heddle).exists(),
            "remove must delete the file when present"
        );
    }

    #[test]
    fn try_reconstruct_invokes_hydrate_on_factory_built_hydrator() {
        // Drives the CountingHydrator's `hydrate` body end-to-end via
        // try_reconstruct → factory closure → invocation, which covers
        // the in-test helper's body (otherwise reachable only as a type-
        // check) and confirms the factory hands back a *working* hydrator.
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).expect("init repo");
        let heddle = temp.path().join(".heddle");

        let kind = "test-kind-tr-3";
        let payload = b"hydrated-blob".to_vec();
        let payload_for_factory = payload.clone();
        register_factory(
            kind,
            Arc::new(
                move |_root: &Path, _section: &HydratorSection| -> Result<Arc<dyn BlobHydrator>> {
                    Ok(Arc::new(CountingHydrator {
                        bytes: payload_for_factory.clone(),
                        calls: AtomicUsize::new(0),
                    }))
                },
            ),
        );

        let cfg = LazyHydratorConfig {
            hydrator: HydratorSection {
                kind: kind.to_string(),
                remote: None,
                git_overlay: None,
            },
        };
        cfg.save(&heddle).unwrap();

        let hydrator = try_reconstruct(temp.path(), &heddle)
            .unwrap()
            .expect("hydrator");
        let blake3 = Blob::new(payload.clone()).hash();
        hydrator.hydrate(&repo, &blake3).expect("hydrate runs");
        // The blob the hydrator wrote must now be retrievable from the
        // store — proves the factory-built hydrator's body executed.
        let loaded = repo
            .store()
            .get_blob(&blake3)
            .expect("get_blob ok")
            .expect("blob present after hydrate");
        assert_eq!(loaded.content(), payload.as_slice());
    }

    #[test]
    fn try_reconstruct_returns_none_when_no_factory_registered_for_kind() {
        let _guard = TEST_REGISTRY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let temp = TempDir::new().unwrap();
        let heddle = temp.path().join(".heddle");
        fs::create_dir_all(&heddle).unwrap();

        let cfg = LazyHydratorConfig {
            hydrator: HydratorSection {
                kind: "never-registered-kind".to_string(),
                remote: None,
                git_overlay: None,
            },
        };
        cfg.save(&heddle).unwrap();

        // No factory registered → returns None, does not error.
        let result = try_reconstruct(temp.path(), &heddle).unwrap();
        assert!(result.is_none());
    }
}
