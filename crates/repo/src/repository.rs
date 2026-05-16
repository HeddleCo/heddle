// SPDX-License-Identifier: Apache-2.0
//! Repository: high-level interface for Heddle operations.

#[path = "bloom_filter.rs"]
mod bloom_filter;
#[path = "commit_graph.rs"]
pub(crate) mod commit_graph;
#[path = "commit_graph_persistence.rs"]
mod commit_graph_persistence;
#[path = "context_suggestions.rs"]
mod context_suggestions;
#[path = "repo_config.rs"]
mod repo_config;
#[path = "repository_context.rs"]
mod repository_context;
#[path = "repository_diff.rs"]
mod repository_diff;
#[path = "repository_goto.rs"]
mod repository_goto;
#[path = "repository_history.rs"]
mod repository_history;
#[path = "repository_maintenance.rs"]
mod repository_maintenance;
#[path = "repository_materialization.rs"]
mod repository_materialization;
#[path = "repository_partial_fetch.rs"]
mod repository_partial_fetch;
#[path = "repository_provenance/mod.rs"]
mod repository_provenance;
#[path = "repository_resolve.rs"]
mod repository_resolve;
#[path = "repository_signing.rs"]
mod repository_signing;
#[path = "repository_snapshot.rs"]
mod repository_snapshot;
#[cfg(test)]
#[path = "repository_tests.rs"]
mod repository_tests;
#[path = "repository_tree.rs"]
mod repository_tree;
#[path = "repository_worktree_apply.rs"]
mod repository_worktree_apply;
#[path = "repository_worktree_status.rs"]
mod repository_worktree_status;
#[path = "status_tracked_refresh.rs"]
mod status_tracked_refresh;
#[path = "status_untracked_scan.rs"]
mod status_untracked_scan;

use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, RwLock},
};

use chrono::Utc;
pub use commit_graph::{CommitGraphIndex, find_merge_base};
pub use context_suggestions::{
    ContextSuggestion, ContextSuggestionTier, HIGH_SUGGESTION_THRESHOLD,
    MAJOR_REWRITE_THRESHOLD_PCT, MEDIUM_SUGGESTION_THRESHOLD, SUGGESTION_WINDOW,
    compute_rewrite_pct, is_major_rewrite,
};
pub use objects::object::DiffKind;
use objects::{
    error::{HeddleError, Result},
    fs_atomic::write_file_atomic,
    lock::{RepoLock, RepositoryLockExt},
    object::{Attribution, ChangeId, ContentHash, Principal, State, Tree},
    store::{FsStore, ObjectStore, ShallowInfo},
    worktree::WorktreeStatus,
};
use oplog::{OpLog, OpLogBackend};
pub use refs::RefSummaryIndexInspection;
use refs::{Head, RefBackend, RefManager};
pub use repo_config::{HostedConfig, OutputFormat, RedactConfig, RepoConfig, TrustedKey};
// Review-epic config types — re-exported here so the new
// `repository_signals.rs` (and external crates wanting to construct a
// custom signals config) don't need to reach into a private module path.
#[allow(unused_imports)]
pub use repo_config::{
    PatternDeviationToml, ReviewConfig, ReviewSignalsToml, SelfFlaggedToml, SignalEnableToml,
    SignalModuleToml, TestReachabilityToml,
};
pub use repository_history::{ChangedPathFilter, ChangedPathFilters, HistoryQuery};
pub use repository_maintenance::{
    ChangeMonitorInspection, CommitGraphInspection, PackFilesInspection, PartialFetchInspection,
    PullPlannerCacheInspection, RefCountsInspection, RepositoryMaintenanceRunReport,
    RepositoryPerformanceInspectionReport, WorktreeIndexInspection,
};
pub use repository_materialization::WarmCanonicalStoreStats;
pub use repository_partial_fetch::MissingBlob;
pub use repository_snapshot::{SnapshotExecution, SnapshotProfile};
pub use repository_tree::{TreeBuildProfile, WorktreeCompareProfile};
pub use repository_worktree_status::{UntrackedSet, UntrackedSubtree, WorktreeStatusDetailed};
use serde::{Deserialize, Serialize};

const GIT_CHECKPOINTS_FILE: &str = "git-checkpoints.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryCapability {
    GitOverlay,
    NativeHeddle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitCheckpointRecord {
    pub change_id: String,
    pub git_commit: String,
    pub summary: String,
    pub committed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitOverlayImportHint {
    pub current_branch: String,
    pub missing_branch_count: usize,
    pub missing_branches: Vec<String>,
    pub recommended_command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitOverlayBranchTip {
    pub branch: String,
    pub git_commit: String,
    pub history_imported: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitOverlayTagTip {
    pub tag: String,
    pub git_commit: String,
    pub history_imported: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OperationScope {
    Git,
    Heddle,
}

impl std::fmt::Display for OperationScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Git => write!(f, "git"),
            Self::Heddle => write!(f, "heddle"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OperationKind {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

impl std::fmt::Display for OperationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Merge => write!(f, "merge"),
            Self::Rebase => write!(f, "rebase"),
            Self::CherryPick => write!(f, "cherry-pick"),
            Self::Revert => write!(f, "revert"),
            Self::Bisect => write!(f, "bisect"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryOperationStatus {
    pub scope: OperationScope,
    pub kind: OperationKind,
    pub in_progress: bool,
    pub state: String,
    pub message: String,
    pub next_action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitRemoteTrackingStatus {
    pub branch: String,
    pub upstream: String,
    pub ahead: usize,
    pub behind: usize,
    pub message: String,
    pub next_action: String,
}

#[derive(Debug, Deserialize)]
struct GitBridgeMappingEntry {
    change_id: String,
    git_oid: String,
}

#[derive(Debug, Deserialize, Default)]
struct GitBridgeMappingFile {
    entries: Vec<GitBridgeMappingEntry>,
}

/// Lazy-clone read-time hydration hook.
///
/// When `Repository::require_blob` is called for a blob that's recorded
/// in `.heddle/partial-fetch` (the marker the lazy-pull plumbing leaves
/// behind) and absent from the local object store, the repo delegates to
/// a registered `BlobHydrator` to fetch the bytes from the upstream.
///
/// Two production implementations exist:
/// - Git-overlay clones: `cli::commands::clone::GitOverlayBlobHydrator`
///   uses gix promisor-fetch semantics against the bare `.git/` repo.
/// - Hosted clones: `heddle_client::grpc_hosted::LazyHostedHydrator`
///   bridges sync `hydrate` calls to async gRPC via a dedicated worker
///   thread + private Tokio runtime; on each call the worker invokes
///   `HostedGrpcClient::hydrate_pulled_state` for the current local-thread
///   tip.
///
/// On success the hydrator is expected to write the blob into
/// `repo.store()`; the read path then clears the missing marker and
/// returns the blob. On failure the error is propagated verbatim — the
/// hook is deliberately not allowed to swallow upstream outages.
pub trait BlobHydrator: Send + Sync {
    fn hydrate(&self, repo: &Repository, hash: &ContentHash) -> Result<()>;
}

/// A Heddle repository.
pub struct Repository {
    root: PathBuf,
    heddle_dir: PathBuf,
    store: Box<dyn ObjectStore>,
    refs: Box<dyn RefBackend>,
    oplog: Box<dyn OpLogBackend>,
    config: RepoConfig,
    shallow: RwLock<ShallowInfo>,
    blob_hydrator: RwLock<Option<Arc<dyn BlobHydrator>>>,
}

impl RepositoryLockExt for Repository {
    fn locker(&self) -> RepoLock {
        let lock_root = self.heddle_dir.parent().expect(
            "heddle_dir has no parent component; cannot determine lock root. This indicates a misconfigured repository.",
        );
        RepoLock::new(lock_root)
    }
}

impl Repository {
    /// Expert-only constructor for callers that already own the repository's
    /// component backends and invariant state.
    ///
    /// Callers must ensure all backends point at the same repository root, the
    /// `heddle_dir` exists and is canonical for that root, and `shallow` matches
    /// the on-disk shallow metadata. Prefer [`Repository::init`],
    /// [`Repository::open`], or [`Repository::open_with_store`] unless a
    /// cross-crate integration genuinely needs to assemble the pieces manually.
    pub fn from_parts(
        root: PathBuf,
        heddle_dir: PathBuf,
        store: Box<dyn ObjectStore>,
        refs: Box<dyn RefBackend>,
        oplog: Box<dyn OpLogBackend>,
        config: RepoConfig,
        shallow: ShallowInfo,
    ) -> Self {
        Self {
            root,
            heddle_dir,
            store,
            refs,
            oplog,
            config,
            shallow: RwLock::new(shallow),
            blob_hydrator: RwLock::new(None),
        }
    }

    fn open_raw(
        root: PathBuf,
        heddle_dir: PathBuf,
        store: Box<dyn ObjectStore>,
        config: RepoConfig,
        refs: RefManager,
    ) -> Result<Self> {
        let actor = config
            .principal
            .as_ref()
            .map(|p| objects::object::Principal::new(&p.name, &p.email))
            .unwrap_or_else(|| objects::object::Principal::new("<unknown>", ""));
        let oplog = OpLog::new(&heddle_dir, actor);
        let shallow = ShallowInfo::load(&heddle_dir)?;
        let repo = Self::from_parts(
            root,
            heddle_dir,
            store,
            Box::new(refs),
            Box::new(oplog),
            config,
            shallow,
        );
        // Run any pending declarative migrations. Idempotent:
        // re-opening a repo a second time is a no-op for the migration pass.
        // Failures here are logged but non-fatal — the inline
        // `migrate_legacy_tracks` calls before this point already handle the
        // load-bearing work, and surfacing migration errors through `open` is
        // worse than letting the repo open and warning later.
        if let Err(err) = crate::migration::apply_pending(&repo) {
            tracing::warn!("declarative migrations failed during repo open: {err}");
        }
        // Reconstruct any persisted lazy-clone blob hydrator. When
        // `.heddle/lazy-hydrator.toml` exists, look up the registered
        // factory for its `kind` and install the hydrator on the
        // freshly-opened repo so a subsequent `require_blob` against a
        // missing-blob marker can fetch transparently — without this
        // reconstruction, lazy clones would only work inside the single
        // `cmd_clone` process. See `lazy_hydrator.rs` for the shape.
        match crate::lazy_hydrator::try_reconstruct(repo.root(), repo.heddle_dir()) {
            Ok(Some(hydrator)) => repo.set_blob_hydrator(hydrator),
            Ok(None) => {}
            Err(err) => {
                // Hydrator construction failed (factory error or
                // malformed metadata). Surface as a warning rather
                // than blocking `open` — eager `heddle status` calls
                // shouldn't fail just because a stale hosted
                // endpoint is unreachable; the user will get the real
                // error on the first `require_blob` that needs it.
                tracing::warn!("lazy hydrator reconstruction failed during open: {err}");
            }
        }
        Ok(repo)
    }

    /// Build an object store from the repository configuration.
    ///
    /// Returns an [`S3Store`] when `[storage.s3]` is configured and the `s3`
    /// feature is enabled, otherwise falls back to [`FsStore`].
    fn build_store(config: &RepoConfig, heddle_dir: &Path) -> Result<Box<dyn ObjectStore>> {
        #[cfg(feature = "s3")]
        {
            if let Some(s3) = &config.storage.s3 {
                return Self::build_s3_store(s3);
            }
        }
        let _ = config; // suppress unused warning when s3 feature is off
        Ok(Box::new(FsStore::new(heddle_dir)))
    }

    /// Construct an [`S3Store`] from the repository's S3 storage configuration.
    #[cfg(feature = "s3")]
    fn build_s3_store(s3: &repo_config::S3StorageConfig) -> Result<Box<dyn ObjectStore>> {
        use objects::store::S3StoreBuilder;

        let mut builder = S3StoreBuilder::new().bucket(&s3.bucket);
        if let Some(ref region) = s3.region {
            builder = builder.region(region);
        }
        if let Some(ref prefix) = s3.prefix {
            builder = builder.prefix(prefix);
        }
        if let Some(ref url) = s3.endpoint_url {
            builder = builder.endpoint_url(url);
        }
        if let Some(ref key) = s3.access_key_id {
            builder = builder.access_key_id(key);
        }
        if let Some(ref secret) = s3.secret_access_key {
            builder = builder.secret_access_key(secret);
        }
        if let Some(ref token) = s3.session_token {
            builder = builder.session_token(token);
        }
        if s3.force_path_style {
            builder = builder.force_path_style(true);
        }

        // `S3StoreBuilder::build` is async. The previous design here was
        // `Handle::try_current().or_else(Runtime::new()).block_on(builder.build())`
        // — that nested `block_on` panicked with "Cannot start a runtime
        // from within a runtime" whenever `Repository::open` was called
        // from inside a Tokio runtime (`#[tokio::main]`, `#[tokio::test]`,
        // a daemon worker). `build_blocking` routes the async `build()`
        // through a short-lived worker-thread runtime, so the caller's
        // runtime is never re-entered — mirrors the heddle#60 fix for the
        // `ObjectStore` impl on `S3Store`.
        let store = builder
            .build_blocking()
            .map_err(|e| HeddleError::Config(format!("S3 store initialization failed: {e}")))?;
        Ok(Box::new(store))
    }

    /// Initialize a new bare repository at the given path.
    ///
    /// Creates the on-disk `.heddle` structure and an attached `main` HEAD, but
    /// does not seed any threads or states. Callers that want a ready-to-use
    /// repository (with a `main` thread pointing at an empty-tree snapshot)
    /// should use [`Repository::init_default`]. Callers that intend to populate
    /// the repository from an external source (e.g. git import) should use
    /// `init` directly so the imported refs become the sole source of truth.
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        let root = path.as_ref().to_path_buf();
        let heddle_dir = root.join(".heddle");

        if heddle_dir.exists() {
            return Err(HeddleError::RepositoryExists(root));
        }

        fs::create_dir_all(&heddle_dir)?;

        let store = FsStore::new(&heddle_dir);
        store.init()?;

        let refs = RefManager::new(&heddle_dir);
        refs.init()?;

        // `init` creates a fresh repo before any principal is configured;
        // the actor is set when the repo is later opened (which reads
        // `RepoConfig.principal`). Use the unattributed default for
        // entries written between init and first open.
        let oplog = OpLog::new_unattributed(&heddle_dir);
        oplog.init()?;

        let config = RepoConfig::default();
        config.save(&heddle_dir.join("config.toml"))?;

        refs.write_head(&Head::Attached {
            thread: "main".to_string(),
        })?;

        Ok(Self {
            root,
            heddle_dir: heddle_dir.clone(),
            store: Box::new(store),
            refs: Box::new(refs),
            oplog: Box::new(oplog),
            config,
            shallow: RwLock::new(ShallowInfo::load(&heddle_dir)?),
            blob_hydrator: RwLock::new(None),
        })
    }

    /// Initialize a new repository with a seeded `main` thread.
    ///
    /// Convenience wrapper: equivalent to [`Repository::init`] followed by
    /// [`Repository::seed_default_thread`]. This is the normal entry point for
    /// fresh, user-created repositories where `main` should exist immediately.
    pub fn init_default(path: impl AsRef<Path>) -> Result<Self> {
        let repo = Self::init(path)?;
        repo.seed_default_thread()?;
        Ok(repo)
    }

    /// Initialize Heddle sidecar storage in an existing Git repository.
    ///
    /// Unlike [`Repository::init_default`], this keeps the repo unseeded and
    /// mirrors the current Git branch attachment into Heddle's HEAD so
    /// commands like `heddle status` can immediately reflect the user's
    /// current branch and dirty worktree.
    pub fn bootstrap_git_overlay(path: impl AsRef<Path>) -> Result<Self> {
        let root = path.as_ref();
        if root.join(".heddle").exists() {
            ensure_git_overlay_exclude(root)?;
            return Self::open(root);
        }

        let repo = Self::init(root)?;
        ensure_git_overlay_exclude(root)?;
        if let Some(head) = detect_git_head(root)? {
            repo.refs.write_head(&head)?;
        }
        Ok(repo)
    }

    /// Open an existing Heddle repository using a custom object store backend.
    pub fn open_with_store(
        heddle_dir: impl AsRef<Path>,
        store: Box<dyn ObjectStore>,
    ) -> Result<Self> {
        let heddle_dir = heddle_dir.as_ref().to_path_buf();
        let root = heddle_dir
            .parent()
            .ok_or_else(|| {
                HeddleError::Config(format!(
                    "heddle_dir '{}' has no parent directory",
                    heddle_dir.display()
                ))
            })?
            .to_path_buf();
        let config = RepoConfig::load(&heddle_dir.join("config.toml"))?;
        let refs = RefManager::new(&heddle_dir);
        refs.migrate_legacy_tracks()?;
        refs.cleanup_stale_temps();
        Self::open_raw(root, heddle_dir, store, config, refs)
    }

    /// Open an existing repository.
    ///
    /// Searches for `.heddle/` in the given path and its ancestors. `.heddle/`
    /// is always a directory; its contents distinguish a main repo from a
    /// worktree pointer:
    ///
    /// - Main repo: `.heddle/objects/`, `.heddle/refs/`, `.heddle/HEAD`,
    ///   `.heddle/state/`, etc.
    /// - Worktree: `.heddle/objectstore` (text pointer to the shared
    ///   `.heddle/`), `.heddle/HEAD` (per-checkout), `.heddle/state/`
    ///   (per-checkout cached state).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let start_path = path.as_ref().canonicalize()?;
        let discovered_git_root = discover_git_root(&start_path);

        let mut current = Some(start_path.as_path());
        while let Some(dir) = current {
            let heddle_path = dir.join(".heddle");

            if heddle_path.is_dir() {
                if let Some(git_root) = discovered_git_root.as_ref()
                    && git_root != dir
                    && git_root.starts_with(dir)
                    && !git_root.join(".heddle").exists()
                {
                    ensure_git_overlay_exclude(git_root)?;
                    Self::bootstrap_git_overlay(git_root)?;
                    return Self::open(git_root);
                }
                let pointer_path = heddle_path.join("objectstore");
                let objects_dir = heddle_path.join("objects");

                if pointer_path.is_file() {
                    // Worktree mode: pointer dir at <dir>/.heddle/, shared
                    // object store at the path read from .heddle/objectstore.
                    let content = fs::read_to_string(&pointer_path)?;
                    let raw_shared = parse_objectstore_pointer(&content).ok_or_else(|| {
                        HeddleError::Config(format!(
                            "invalid .heddle/objectstore pointer at {}: expected 'objectstore: <path>'",
                            pointer_path.display()
                        ))
                    })?;

                    if raw_shared.is_relative() {
                        return Err(HeddleError::Config(format!(
                            ".heddle/objectstore pointer at {} contains a relative path '{}'; \
                             objectstore path must be absolute",
                            pointer_path.display(),
                            raw_shared.display()
                        )));
                    }

                    let shared_galeed_dir = raw_shared.canonicalize().map_err(|e| {
                        HeddleError::Config(format!(
                            ".heddle/objectstore pointer at {} points to non-existent path '{}': {}",
                            pointer_path.display(),
                            raw_shared.display(),
                            e
                        ))
                    })?;

                    if !shared_galeed_dir.join("objects").is_dir() {
                        return Err(HeddleError::Config(format!(
                            ".heddle/objectstore pointer at {} resolves to '{}' which does not \
                             contain an 'objects/' directory; not a valid Heddle store",
                            pointer_path.display(),
                            shared_galeed_dir.display()
                        )));
                    }

                    let config = RepoConfig::load(&shared_galeed_dir.join("config.toml"))?;
                    let store: Box<dyn ObjectStore> =
                        Self::build_store(&config, &shared_galeed_dir)?;
                    let local_head_path = heddle_path.join("HEAD");
                    let refs = RefManager::new(&shared_galeed_dir).with_local_head(local_head_path);
                    refs.migrate_legacy_tracks()?;
                    refs.cleanup_stale_temps();
                    return Self::open_raw(
                        dir.to_path_buf(),
                        shared_galeed_dir,
                        store,
                        config,
                        refs,
                    );
                }

                if objects_dir.is_dir() {
                    // Main repo mode.
                    let config = RepoConfig::load(&heddle_path.join("config.toml"))?;
                    let store: Box<dyn ObjectStore> = Self::build_store(&config, &heddle_path)?;
                    let refs = RefManager::new(&heddle_path);
                    refs.migrate_legacy_tracks()?;
                    refs.cleanup_stale_temps();
                    let repo = Self::open_raw(dir.to_path_buf(), heddle_path, store, config, refs)?;
                    if repo.capability() == RepositoryCapability::GitOverlay
                        && let Ok(Some(git_head)) = detect_git_head(dir)
                    {
                        // Avoid the disk write when our HEAD already matches
                        // git's. Reading the existing head is a small file
                        // read; the write that follows hits atomic-rename
                        // machinery (sync + rename) which dominates here.
                        let stale = match repo.refs.read_head() {
                            Ok(current) => current != git_head,
                            Err(_) => true,
                        };
                        if stale {
                            repo.refs.write_head(&git_head)?;
                        }
                    }
                    return Ok(repo);
                }

                // .heddle/ exists but is neither a worktree pointer nor a
                // main repo. Treat as not-found and continue walking parents.
            }

            current = dir.parent();
        }

        if let Some(git_root) = discovered_git_root {
            ensure_git_overlay_exclude(&git_root)?;
            Self::bootstrap_git_overlay(&git_root)?;
            return Self::open(git_root);
        }

        Err(HeddleError::RepositoryNotFound(path.as_ref().to_path_buf()))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn heddle_dir(&self) -> &Path {
        &self.heddle_dir
    }

    pub fn capability(&self) -> RepositoryCapability {
        if has_git_metadata(&self.root) {
            RepositoryCapability::GitOverlay
        } else {
            RepositoryCapability::NativeHeddle
        }
    }

    pub fn capability_label(&self) -> &'static str {
        match self.capability() {
            RepositoryCapability::GitOverlay => "git-overlay",
            RepositoryCapability::NativeHeddle => "native-heddle",
        }
    }

    pub fn storage_model_label(&self) -> &'static str {
        match self.capability() {
            RepositoryCapability::GitOverlay => "git+heddle-sidecar",
            RepositoryCapability::NativeHeddle => "heddle-native",
        }
    }

    pub fn hosted_enabled(&self) -> bool {
        self.config
            .hosted
            .upstream_url
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            || self
                .config
                .hosted
                .namespace
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
    }

    pub fn current_lane(&self) -> Result<Option<String>> {
        if self.current_state()?.is_none() && self.capability() == RepositoryCapability::GitOverlay
        {
            return self.git_overlay_current_branch();
        }

        match self.head_ref()? {
            Head::Attached { thread } => Ok(Some(thread)),
            Head::Detached { .. } => Ok(None),
        }
    }

    pub fn operation_status(&self) -> Result<Option<RepositoryOperationStatus>> {
        if let Some(status) = self.heddle_operation_status()? {
            return Ok(Some(status));
        }
        self.git_operation_status()
    }

    pub fn git_remote_tracking_status(&self) -> Result<Option<GitRemoteTrackingStatus>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }

        let branch = match self.git_overlay_current_branch()? {
            Some(branch) => branch,
            None => return Ok(None),
        };

        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["rev-list", "--left-right", "--count", "@{upstream}...HEAD"])
            .output()
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to inspect upstream drift at '{}': {}",
                    self.root.display(),
                    error
                ))
            })?;

        if !output.status.success() {
            return Ok(None);
        }

        let counts = String::from_utf8_lossy(&output.stdout);
        let mut parts = counts.split_whitespace();
        let behind = parts
            .next()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let ahead = parts
            .next()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        if ahead == 0 && behind == 0 {
            return Ok(None);
        }

        let upstream_output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args([
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ])
            .output()
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to inspect upstream branch at '{}': {}",
                    self.root.display(),
                    error
                ))
            })?;

        if !upstream_output.status.success() {
            return Ok(None);
        }

        let upstream = String::from_utf8_lossy(&upstream_output.stdout)
            .trim()
            .to_string();
        if upstream.is_empty() {
            return Ok(None);
        }

        let message = match (ahead, behind) {
            (0, behind) => format!(
                "Git branch '{}' is behind upstream '{}' by {} commit(s)",
                branch, upstream, behind
            ),
            (ahead, 0) => format!(
                "Git branch '{}' is ahead of upstream '{}' by {} commit(s)",
                branch, upstream, ahead
            ),
            (ahead, behind) => format!(
                "Git branch '{}' has diverged from upstream '{}' (ahead {}, behind {})",
                branch, upstream, ahead, behind
            ),
        };
        let next_action = match (ahead, behind) {
            (0, _) => "git pull --rebase".to_string(),
            (_, 0) => "git push".to_string(),
            _ => "git fetch && git rebase @{upstream}".to_string(),
        };

        Ok(Some(GitRemoteTrackingStatus {
            branch,
            upstream,
            ahead,
            behind,
            message,
            next_action,
        }))
    }

    pub fn git_overlay_import_hint(&self) -> Result<Option<GitOverlayImportHint>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }

        let current_branch = match self.git_overlay_current_branch()? {
            Some(branch) => branch,
            None => return Ok(None),
        };
        let branch_tips = self.git_overlay_branch_tips()?;
        let mut missing_branches = branch_tips
            .into_iter()
            .filter(|tip| tip.branch != current_branch && !tip.history_imported)
            .map(|tip| tip.branch)
            .collect::<Vec<_>>();
        missing_branches.sort();
        missing_branches.dedup();

        if missing_branches.is_empty() {
            return Ok(None);
        }

        let recommended_command = if missing_branches.len() == 1 {
            format!("heddle bridge git import --ref {}", missing_branches[0])
        } else {
            "heddle bridge git import".to_string()
        };

        Ok(Some(GitOverlayImportHint {
            current_branch,
            missing_branch_count: missing_branches.len(),
            missing_branches,
            recommended_command,
        }))
    }

    pub fn git_overlay_branch_tips(&self) -> Result<Vec<GitOverlayBranchTip>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(Vec::new());
        }

        let git_repo = gix::discover(&self.root).map_err(|error| {
            HeddleError::Config(format!(
                "failed to inspect git branches at '{}': {}",
                self.root.display(),
                error
            ))
        })?;

        let imported_threads: std::collections::HashSet<String> =
            self.refs().list_threads()?.into_iter().collect();
        let bridge_mapping = self.git_overlay_bridge_mapping()?;
        let mut branch_tips = Vec::new();

        for branch in git_repo
            .references()
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to read git references at '{}': {}",
                    self.root.display(),
                    error
                ))
            })?
            .local_branches()
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to enumerate git branches at '{}': {}",
                    self.root.display(),
                    error
                ))
            })?
        {
            let mut branch = branch.map_err(|error| {
                HeddleError::Config(format!(
                    "failed to inspect git branch at '{}': {}",
                    self.root.display(),
                    error
                ))
            })?;
            let name = branch.name().shorten().to_string();
            let Some(target) =
                self.git_overlay_commit_tip_oid(&git_repo, &mut branch, "branch", &name)?
            else {
                continue;
            };
            let history_imported = if imported_threads.contains(&name) {
                // Read the thread ref once; the mapped + checkpointed
                // checks each used to re-read it, which doubled the
                // ref-store hits per branch on a 60+ branch repo.
                let existing_thread = self.refs().get_thread(&name)?;
                let mapped = matches!(
                    (existing_thread.as_ref(), bridge_mapping.get(&target.to_string())),
                    (Some(existing), Some(mapped_change))
                        if existing.to_string_full() == *mapped_change
                );
                let checkpointed = if mapped {
                    false
                } else if let Some(existing) = existing_thread {
                    self.latest_git_checkpoint_for_change(&existing)?
                        .is_some_and(|record| record.git_commit == target.to_string())
                } else {
                    false
                };
                mapped || checkpointed
            } else {
                false
            };
            branch_tips.push(GitOverlayBranchTip {
                branch: name,
                git_commit: target.to_string(),
                history_imported,
            });
        }

        branch_tips.sort_by(|a, b| a.branch.cmp(&b.branch));
        Ok(branch_tips)
    }

    pub fn git_overlay_tag_tips(&self) -> Result<Vec<GitOverlayTagTip>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(Vec::new());
        }

        let git_repo = gix::discover(&self.root).map_err(|error| {
            HeddleError::Config(format!(
                "failed to inspect git tags at '{}': {}",
                self.root.display(),
                error
            ))
        })?;

        let imported_markers: std::collections::HashSet<String> =
            self.refs().list_markers()?.into_iter().collect();
        let bridge_mapping = self.git_overlay_bridge_mapping()?;
        let mut tag_tips = Vec::new();

        for tag in git_repo
            .references()
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to read git references at '{}': {}",
                    self.root.display(),
                    error
                ))
            })?
            .tags()
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to enumerate git tags at '{}': {}",
                    self.root.display(),
                    error
                ))
            })?
        {
            let mut tag = tag.map_err(|error| {
                HeddleError::Config(format!(
                    "failed to inspect git tag at '{}': {}",
                    self.root.display(),
                    error
                ))
            })?;
            let name = tag.name().shorten().to_string();
            let Some(target) =
                self.git_overlay_commit_tip_oid(&git_repo, &mut tag, "tag", &name)?
            else {
                continue;
            };
            let history_imported = if imported_markers.contains(&name) {
                matches!(
                    (self.refs().get_marker(&name)?, bridge_mapping.get(&target.to_string())),
                    (Some(existing), Some(mapped_change))
                        if existing.to_string_full() == *mapped_change
                )
            } else {
                false
            };
            tag_tips.push(GitOverlayTagTip {
                tag: name,
                git_commit: target.to_string(),
                history_imported,
            });
        }

        tag_tips.sort_by(|a, b| a.tag.cmp(&b.tag));
        Ok(tag_tips)
    }

    pub fn git_overlay_branch_tip(&self, name: &str) -> Result<Option<GitOverlayBranchTip>> {
        Ok(self
            .git_overlay_branch_tips()?
            .into_iter()
            .find(|tip| tip.branch == name))
    }

    pub fn git_overlay_tag_tip(&self, name: &str) -> Result<Option<GitOverlayTagTip>> {
        Ok(self
            .git_overlay_tag_tips()?
            .into_iter()
            .find(|tip| tip.tag == name))
    }

    pub fn git_overlay_worktree_status(&self) -> Result<Option<WorktreeStatus>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }

        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to inspect git worktree at '{}': {}",
                    self.root.display(),
                    error
                ))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(HeddleError::Config(format!(
                "git status failed at '{}': {}",
                self.root.display(),
                stderr
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut status = WorktreeStatus::default();
        for line in stdout.lines() {
            if line.len() < 3 {
                continue;
            }
            let code = &line[..2];
            let raw_path = &line[3..];
            if (code.starts_with('R') || code.ends_with('R'))
                && let Some((old_path, new_path)) = raw_path.split_once(" -> ")
            {
                let old_path = PathBuf::from(old_path);
                let new_path = PathBuf::from(new_path);
                if !(old_path == Path::new(".heddle") || old_path.starts_with(".heddle")) {
                    status.deleted.push(old_path);
                }
                if !(new_path == Path::new(".heddle") || new_path.starts_with(".heddle")) {
                    status.added.push(new_path);
                }
                continue;
            }
            let path = raw_path
                .rsplit_once(" -> ")
                .map(|(_, new_path)| new_path)
                .unwrap_or(raw_path);
            let path = PathBuf::from(path);
            if path == Path::new(".heddle") || path.starts_with(".heddle") {
                continue;
            }

            if code == "??" {
                status.added.push(path);
                continue;
            }

            let chars: Vec<char> = code.chars().collect();
            if chars.contains(&'D') {
                status.deleted.push(path);
            } else if chars.contains(&'A') {
                status.added.push(path);
            } else {
                status.modified.push(path);
            }
        }

        Ok(Some(status))
    }

    fn git_overlay_bridge_mapping(&self) -> Result<HashMap<String, String>> {
        let path = self
            .heddle_dir
            .join("git-bridge")
            .join("bridge-mapping.json");
        if !path.exists() {
            return Ok(HashMap::new());
        }

        let contents = fs::read_to_string(path)?;
        if contents.trim().is_empty() {
            return Ok(HashMap::new());
        }

        let file: GitBridgeMappingFile = serde_json::from_str(&contents)?;
        Ok(file
            .entries
            .into_iter()
            .map(|entry| (entry.git_oid, entry.change_id))
            .collect())
    }

    pub fn git_overlay_current_branch(&self) -> Result<Option<String>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }

        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
            .output()
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to inspect git HEAD at '{}': {}",
                    self.root.display(),
                    error
                ))
            })?;

        if output.status.success() {
            let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if branch.is_empty() {
                return Ok(None);
            }
            return Ok(Some(branch));
        }

        if let Some(branch) = detect_git_in_progress_branch(&self.root)? {
            return Ok(Some(branch));
        }

        Ok(None)
    }

    fn git_overlay_commit_tip_oid(
        &self,
        git_repo: &gix::Repository,
        reference: &mut gix::Reference,
        ref_kind: &str,
        ref_name: &str,
    ) -> Result<Option<gix::hash::ObjectId>> {
        if reference.target().try_id().is_none() {
            return Ok(None);
        }

        let target = match reference.peel_to_id() {
            Ok(target) => target.detach(),
            Err(_) => return Ok(None),
        };
        let object = match git_repo.find_object(target) {
            Ok(object) => object,
            Err(_) => return Ok(None),
        };
        if object.kind != gix::objs::Kind::Commit {
            return Ok(None);
        }

        let _ = (ref_kind, ref_name);
        Ok(Some(target))
    }

    fn heddle_operation_status(&self) -> Result<Option<RepositoryOperationStatus>> {
        if self.merge_state_manager().is_merge_in_progress() {
            return Ok(Some(RepositoryOperationStatus {
                scope: OperationScope::Heddle,
                kind: OperationKind::Merge,
                in_progress: true,
                state: "in-progress".to_string(),
                message: "Heddle merge is in progress".to_string(),
                next_action: "heddle continue".to_string(),
            }));
        }

        let rebase_state = self.heddle_dir.join("REBASE_STATE");
        if rebase_state.exists() {
            return Ok(Some(RepositoryOperationStatus {
                scope: OperationScope::Heddle,
                kind: OperationKind::Rebase,
                in_progress: true,
                state: "in-progress".to_string(),
                message: "Heddle rebase is in progress".to_string(),
                next_action: "heddle continue".to_string(),
            }));
        }

        let bisect_state = self.heddle_dir.join("BISECT_STATE");
        if bisect_state.exists() {
            return Ok(Some(RepositoryOperationStatus {
                scope: OperationScope::Heddle,
                kind: OperationKind::Bisect,
                in_progress: true,
                state: "in-progress".to_string(),
                message: "Heddle bisect is in progress".to_string(),
                next_action: "heddle bisect good <state> or heddle bisect bad <state>".to_string(),
            }));
        }

        Ok(None)
    }

    fn git_operation_status(&self) -> Result<Option<RepositoryOperationStatus>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }

        let git_dir = resolve_git_dir(&self.root)?;
        let candidates = [
            (
                git_dir.join("rebase-merge"),
                OperationKind::Rebase,
                "Git rebase is in progress",
                "heddle continue",
            ),
            (
                git_dir.join("rebase-apply"),
                OperationKind::Rebase,
                "Git rebase is in progress",
                "heddle continue",
            ),
            (
                git_dir.join("MERGE_HEAD"),
                OperationKind::Merge,
                "Git merge is in progress",
                "heddle continue",
            ),
            (
                git_dir.join("CHERRY_PICK_HEAD"),
                OperationKind::CherryPick,
                "Git cherry-pick is in progress",
                "heddle continue",
            ),
            (
                git_dir.join("REVERT_HEAD"),
                OperationKind::Revert,
                "Git revert is in progress",
                "heddle continue",
            ),
            (
                git_dir.join("BISECT_LOG"),
                OperationKind::Bisect,
                "Git bisect is in progress",
                "git bisect good or git bisect bad",
            ),
        ];

        for (path, kind, message, next_action) in candidates {
            if path.exists() {
                return Ok(Some(RepositoryOperationStatus {
                    scope: OperationScope::Git,
                    kind,
                    in_progress: true,
                    state: "in-progress".to_string(),
                    message: message.to_string(),
                    next_action: next_action.to_string(),
                }));
            }
        }

        Ok(None)
    }

    pub fn list_git_checkpoints(&self) -> Result<Vec<GitCheckpointRecord>> {
        let path = self.root.join(".heddle/state").join(GIT_CHECKPOINTS_FILE);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let contents = fs::read_to_string(path)?;
        if contents.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_str(&contents)?)
    }

    pub fn latest_git_checkpoint_for_change(
        &self,
        change_id: &ChangeId,
    ) -> Result<Option<GitCheckpointRecord>> {
        let full_id = change_id.to_string_full();
        Ok(self
            .list_git_checkpoints()?
            .into_iter()
            .rev()
            .find(|record| record.change_id == full_id))
    }

    pub fn record_git_checkpoint(
        &self,
        change_id: &ChangeId,
        git_commit: impl Into<String>,
        summary: impl Into<String>,
    ) -> Result<GitCheckpointRecord> {
        let mut records = self.list_git_checkpoints()?;
        let record = GitCheckpointRecord {
            change_id: change_id.to_string_full(),
            git_commit: git_commit.into(),
            summary: summary.into(),
            committed_at: Utc::now().to_rfc3339(),
        };
        let path = self.root.join(".heddle/state").join(GIT_CHECKPOINTS_FILE);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        records.push(record.clone());
        write_file_atomic(&path, serde_json::to_string_pretty(&records)?.as_bytes())?;
        Ok(record)
    }

    pub fn store(&self) -> &dyn ObjectStore {
        self.store.as_ref()
    }

    pub fn init_worktree(
        path: impl AsRef<Path>,
        shared_galeed_dir: impl AsRef<Path>,
    ) -> Result<()> {
        let path = path.as_ref();
        let shared = shared_galeed_dir.as_ref().canonicalize()?;
        fs::create_dir_all(path)?;
        let heddle_dir = path.join(".heddle");
        if heddle_dir.exists() {
            return Err(HeddleError::RepositoryExists(path.to_path_buf()));
        }
        fs::create_dir_all(&heddle_dir)?;
        write_file_atomic(
            &heddle_dir.join("objectstore"),
            format!("objectstore: {}\n", shared.display()).as_bytes(),
        )?;
        fs::create_dir_all(heddle_dir.join("state"))?;
        Ok(())
    }

    pub fn refs(&self) -> &dyn RefBackend {
        &*self.refs
    }

    pub fn oplog(&self) -> &dyn OpLogBackend {
        self.oplog.as_ref()
    }

    pub fn op_scope(&self) -> String {
        // The local HEAD pointer (`<root>/.heddle/HEAD`) is unique per
        // worktree even when several worktrees share one oplog backend
        // (via `.heddle/objectstore`). `undo`/`redo`/`--list` filter by
        // exact-match scope, so the scope must distinguish each
        // worktree's local HEAD pointer dir.
        //
        // Use a content-derived digest of the canonical pointer path:
        //   * stable across heddle invocations from the same checkout
        //   * unique per worktree (different absolute paths digest
        //     differently), so worktree-local undo keeps working in
        //     shared-oplog setups
        //   * opaque on disk — the user's home directory and username
        //     never end up serialized into oplog entries
        let local_head = self.root.join(".heddle").join("HEAD");
        let canonical = local_head.canonicalize().unwrap_or(local_head);
        let digest = blake3::hash(canonical.to_string_lossy().as_bytes());
        format!("wt-{}", &digest.to_hex().as_str()[..16])
    }

    pub fn repo_config(&self) -> &RepoConfig {
        &self.config
    }

    pub fn config(&self) -> &RepoConfig {
        self.repo_config()
    }

    pub fn get_tree_for_state(&self, state_id: &ChangeId) -> Result<Option<Tree>> {
        let state = match self.store.get_state(state_id)? {
            Some(state) => state,
            None => return Ok(None),
        };
        self.store.get_tree(&state.tree)
    }

    pub fn ignore_patterns(&self) -> Result<Vec<String>> {
        let mut patterns = self.config.worktree.ignore.clone();
        let path = self.root.join(".heddleignore");

        if path.exists() {
            let contents = std::fs::read_to_string(path)?;
            for line in contents.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if !patterns.iter().any(|pattern| pattern == trimmed) {
                    patterns.push(trimmed.to_string());
                }
            }
        }

        Ok(patterns)
    }

    /// Canonical absolute paths of *other* threads' worktrees that are
    /// strict descendants of `walk_root`. The walker uses these to
    /// avoid scanning a sibling thread's files into the current
    /// thread's tree (a common shape when an agent worktree is
    /// materialized inside the parent repo, e.g. `--path-prefix
    /// ./agents`). Computed once per scan, not once per file.
    ///
    /// Returns paths that
    ///   - are strict descendants of canonical `walk_root`, and
    ///   - are NOT equal to `walk_root` itself (each thread can scan
    ///     its own worktree without excluding itself).
    ///
    /// Threads with no recorded worktree, or worktrees that no longer
    /// exist on disk, are skipped without error.
    pub fn nested_thread_worktree_exclusions(&self, walk_root: &Path) -> Result<Vec<PathBuf>> {
        let canonical_walk_root = walk_root
            .canonicalize()
            .unwrap_or_else(|_| walk_root.to_path_buf());
        let manager = crate::thread_storage::ThreadManager::new(self.heddle_dir());
        let mut exclusions: Vec<PathBuf> = Vec::new();
        let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for thread in manager.list()? {
            for candidate in [
                Some(&thread.execution_path),
                thread.materialized_path.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                if candidate.as_os_str().is_empty() {
                    continue;
                }
                let canonical = match candidate.canonicalize() {
                    Ok(path) => path,
                    Err(_) => continue,
                };
                if canonical == canonical_walk_root {
                    continue;
                }
                if !canonical.starts_with(&canonical_walk_root) {
                    continue;
                }
                if seen.insert(canonical.clone()) {
                    exclusions.push(canonical);
                }
            }
        }
        Ok(exclusions)
    }

    pub fn head(&self) -> Result<Option<ChangeId>> {
        Ok(match self.refs.read_head()? {
            Head::Attached { thread } => self.refs.get_thread(&thread)?,
            Head::Detached { state } => Some(state),
        })
    }

    pub fn head_ref(&self) -> Result<Head> {
        self.refs.read_head()
    }

    /// Resolve the on-disk worktree path for the *active thread*.
    ///
    /// This is the canonical "where does the current thread live on disk"
    /// lookup. It reads `HEAD`, looks up the attached thread's metadata
    /// (via [`crate::ThreadManager`]), and returns the recorded
    /// `execution_path` (or `materialized_path` if unset). When no thread
    /// has a recorded path — main, threads created without a separate
    /// worktree, or `HEAD::Detached` — this falls back to [`Self::root`].
    ///
    /// Worktree-mutating commands (merge, rebase, goto, ship) should
    /// resolve their target via this helper so that
    /// `heddle thread switch X && heddle merge Y` lands the merge into
    /// thread `X`'s dedicated worktree, not into whichever directory the
    /// operator happened to invoke `heddle` from. Snapshot/capture
    /// intentionally stay CWD-based: the agent inside their worktree
    /// captures *that* worktree.
    pub fn active_worktree_path(&self) -> Result<PathBuf> {
        let head = self.refs.read_head()?;
        let Head::Attached { thread } = head else {
            return Ok(self.root.clone());
        };
        let manager = crate::thread_storage::ThreadManager::new(self.heddle_dir());
        let Some(thread_record) = manager.find_by_thread(&thread)? else {
            return Ok(self.root.clone());
        };
        if !thread_record.execution_path.as_os_str().is_empty() {
            return Ok(thread_record.execution_path);
        }
        if let Some(path) = thread_record.materialized_path {
            return Ok(path);
        }
        Ok(self.root.clone())
    }

    pub fn current_state(&self) -> Result<Option<State>> {
        match self.head()? {
            Some(id) => self.store.get_state(&id),
            None => Ok(None),
        }
    }

    pub fn get_principal(&self) -> Result<Principal> {
        if let Some(principal) = Principal::from_env() {
            return Ok(principal);
        }

        if let Some(config) = &self.config.principal {
            return Ok(Principal::new(&config.name, &config.email));
        }

        Ok(Principal::new("Unknown", "unknown@example.com"))
    }

    pub fn get_attribution(&self) -> Result<Attribution> {
        let principal = self.get_principal()?;

        if let Some(agent) = self.resolve_agent() {
            Ok(Attribution::with_agent(principal, agent))
        } else {
            Ok(Attribution::human(principal))
        }
    }

    pub fn is_shallow(&self, id: &ChangeId) -> bool {
        self.shallow.read().unwrap().is_shallow(id)
    }

    pub fn set_shallow(&self, state_id: &ChangeId, _parents: &[ChangeId]) -> Result<()> {
        self.shallow.write().unwrap().add_shallow(*state_id)?;
        Ok(())
    }

    pub fn record_missing_blob(&self, hash: ContentHash) -> Result<()> {
        self.partial_fetch_metadata().record_missing_blob(hash)?;
        Ok(())
    }

    /// Seed a `main` thread pointing at an empty-tree root state.
    ///
    /// The seeded state is written to the object store and pointed at by the
    /// `main` thread ref, but is deliberately NOT recorded in the oplog: `init`
    /// is a point-of-creation event, not user work, and should not be
    /// undoable. No-op if `main` already exists.
    ///
    /// The seed state uses a stable `Heddle <init@heddle>` attribution
    /// instead of the user's principal because the user's principal may
    /// not yet be configured at init time (e.g. the user writes
    /// `.heddle/config.toml` after `heddle init`). Falling back to
    /// `Unknown <unknown@example.com>` would surface in `heddle log` as
    /// a state owned by no one. The genesis state is also filtered out of
    /// user-facing log output (see `repository_history::is_synthetic_root`).
    pub fn seed_default_thread(&self) -> Result<()> {
        if self.refs.get_thread("main")?.is_some() {
            return Ok(());
        }

        let empty_tree = Tree::new();
        let tree_hash = self.store.put_tree(&empty_tree)?;
        let state = State::new_snapshot(tree_hash, vec![], Attribution::human(seed_principal()));
        self.store.put_state(&state)?;
        self.refs.set_thread("main", &state.change_id)?;
        Ok(())
    }

    pub fn clear_missing_blob(&self, hash: &ContentHash) -> Result<()> {
        self.partial_fetch_metadata().clear_missing_blob(hash)?;
        Ok(())
    }

    pub fn missing_blobs(&self) -> Result<Vec<ContentHash>> {
        self.partial_fetch_metadata().missing_blobs()
    }

    pub fn clear_all_missing_blobs(&self) -> Result<bool> {
        self.partial_fetch_metadata().clear_all_missing_blobs()
    }

    pub fn is_missing_blob(&self, hash: &ContentHash) -> Result<bool> {
        self.partial_fetch_metadata().is_missing_blob(hash)
    }

    pub fn require_blob(&self, hash: &ContentHash) -> Result<objects::object::Blob> {
        if let Some(blob) = self.store.get_blob(hash)? {
            if self.is_missing_blob(hash)? {
                self.clear_missing_blob(hash)?;
            }
            return Ok(blob);
        }

        if self.is_missing_blob(hash)? {
            // Lazy-clone read-time hydration (issue #50). If a hydrator
            // is registered (by `heddle clone --lazy` / `--filter`),
            // delegate; otherwise surface MissingObject as before.
            if let Some(hydrator) = self.blob_hydrator() {
                hydrator.hydrate(self, hash)?;
                if let Some(blob) = self.store.get_blob(hash)? {
                    self.clear_missing_blob(hash)?;
                    return Ok(blob);
                }
                // Hydrator returned Ok but did not actually deliver the
                // blob — defensive guard so callers never see stale
                // state. Leaves the missing marker in place so a future
                // attempt re-tries hydration.
            }
            return Err(HeddleError::MissingObject {
                object_type: "blob".to_string(),
                id: hash.to_hex(),
            });
        }

        Err(HeddleError::NotFound(hash.to_hex()))
    }

    /// Register a `BlobHydrator` to fetch blobs on demand from the
    /// upstream when `require_blob` hits a missing-blob marker. Used by
    /// the clone command after a `--lazy` / `--filter blob:none` clone.
    /// Replaces any previously registered hydrator.
    ///
    /// The trait-object handle itself is process-local, but persistence
    /// across `Repository::open` calls is handled by the
    /// [`crate::lazy_hydrator`] module: clone writes
    /// `.heddle/lazy-hydrator.toml` recording the hydrator kind +
    /// config, and `Repository::open` consults
    /// [`crate::lazy_hydrator::try_reconstruct`] to look up the
    /// registered factory and re-install the hydrator automatically.
    pub fn set_blob_hydrator(&self, hydrator: Arc<dyn BlobHydrator>) {
        *self.blob_hydrator.write().unwrap() = Some(hydrator);
    }

    /// The currently registered hydrator, if any.
    pub fn blob_hydrator(&self) -> Option<Arc<dyn BlobHydrator>> {
        self.blob_hydrator.read().unwrap().clone()
    }

    fn partial_fetch_metadata(&self) -> repository_partial_fetch::PartialFetchMetadataManager {
        repository_partial_fetch::PartialFetchMetadataManager::new(&self.heddle_dir)
    }

    pub fn shallow(&self) -> std::sync::RwLockReadGuard<'_, ShallowInfo> {
        self.shallow.read().unwrap()
    }
}

fn ensure_git_overlay_exclude(root: &Path) -> Result<()> {
    let git_dir = root.join(".git");
    if !git_dir.is_dir() {
        return Ok(());
    }

    let info_dir = git_dir.join("info");
    fs::create_dir_all(&info_dir)?;
    let exclude_path = info_dir.join("exclude");
    let existing = fs::read_to_string(&exclude_path).unwrap_or_default();
    let already_has_rule = existing
        .lines()
        .map(str::trim)
        .any(|line| line == ".heddle/" || line == "/.heddle/" || line == ".heddle");
    if already_has_rule {
        return Ok(());
    }

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude_path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, ".heddle/")?;
    Ok(())
}

/// Stable system principal stamped into the synthetic seed state created
/// at `heddle init` time, before any user principal is known. Kept
/// distinct from the `Unknown <unknown@example.com>` fallback so the
/// genesis state is never confused with an unattributed user state.
pub(crate) fn seed_principal() -> Principal {
    Principal::new("Heddle", "init@heddle")
}

/// True if `state` is the synthetic empty-tree genesis stamped by
/// [`Repository::seed_default_thread`]. These states are filtered from
/// user-facing log walks: they have no parents, no intent, and the
/// system seed principal — they represent pre-history, not user work.
pub fn is_synthetic_root(state: &State) -> bool {
    state.parents.is_empty()
        && state.intent.is_none()
        && state.attribution.principal == seed_principal()
        && state.attribution.agent.is_none()
}

/// Parse a `.heddle` pointer file and return the shared object store path.
///
/// The file must contain a line of the form `objectstore: <path>`.
fn parse_objectstore_pointer(content: &str) -> Option<PathBuf> {
    for line in content.lines() {
        if let Some(path) = line.strip_prefix("objectstore:") {
            let path = path.trim();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

fn has_git_metadata(path: &Path) -> bool {
    let dot_git = path.join(".git");
    dot_git.is_dir() || dot_git.is_file()
}

/// Read git's HEAD ref via `gix::discover` (~25ms — full repository
/// inspection). Used as a fallback when the fast path can't parse the
/// raw `.git/HEAD` file (e.g. detached HEAD, multi-worktree layouts).
fn detect_git_head_via_gix(path: &Path) -> Result<Option<Head>> {
    let repo = gix::discover(path).map_err(|error| {
        HeddleError::Config(format!(
            "failed to inspect git repository at '{}': {}",
            path.display(),
            error
        ))
    })?;
    let head = match repo.head() {
        Ok(head) => head,
        Err(_) => return Ok(None),
    };

    Ok(head.referent_name().map(|name| Head::Attached {
        thread: name.shorten().to_string(),
    }))
}

/// Detect git's current HEAD branch.
///
/// The fast path reads `.git/HEAD` directly as text. `.git/HEAD` is a
/// tiny file (~30 bytes for `ref: refs/heads/<name>\n`) and a direct
/// read is ~50us vs. `gix::discover()`'s ~25ms full repository
/// inspection. Falls back to gix only for the cases the text parser
/// can't handle: detached HEAD, multi-worktree `gitdir:` indirections,
/// and any malformed file (where we'd rather surface the right error
/// than guess).
fn detect_git_head(path: &Path) -> Result<Option<Head>> {
    if let Some(head) = detect_git_head_fast(path) {
        return Ok(Some(head));
    }
    detect_git_head_via_gix(path)
}

/// Fast path for `.git/HEAD` parsing. Returns `Some(Head::Attached)`
/// when `.git/HEAD` is the simple `ref: refs/heads/<name>` form;
/// returns `None` for any case we don't trust ourselves to parse
/// correctly (detached HEAD raw OIDs, `gitdir:` worktree pointers,
/// missing files), letting the gix fallback handle it.
fn detect_git_head_fast(path: &Path) -> Option<Head> {
    let head_path = path.join(".git").join("HEAD");
    // `.git` may also be a *file* (the gitdir: pointer used by
    // worktrees and submodules) — don't try to read it as a directory.
    if !head_path.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&head_path).ok()?;
    let trimmed = content.trim();
    let suffix = trimmed.strip_prefix("ref: ")?;
    let name = suffix.strip_prefix("refs/heads/")?.to_string();
    if name.is_empty() {
        return None;
    }
    Some(Head::Attached { thread: name })
}

fn resolve_git_dir(path: &Path) -> Result<PathBuf> {
    let repo = gix::discover(path).map_err(|error| {
        HeddleError::Config(format!(
            "failed to resolve git dir at '{}': {}",
            path.display(),
            error
        ))
    })?;
    Ok(repo.git_dir().to_path_buf())
}

fn detect_git_in_progress_branch(path: &Path) -> Result<Option<String>> {
    let git_dir = resolve_git_dir(path)?;
    for marker in ["rebase-merge/head-name", "rebase-apply/head-name"] {
        let branch_path = git_dir.join(marker);
        if !branch_path.exists() {
            continue;
        }
        let raw = fs::read_to_string(&branch_path)?;
        let value = raw.trim();
        if let Some(short) = value.strip_prefix("refs/heads/") {
            return Ok(Some(short.to_string()));
        }
        if !value.is_empty() {
            return Ok(Some(value.to_string()));
        }
    }
    Ok(None)
}

fn discover_git_root(path: &Path) -> Option<PathBuf> {
    let start = path.canonicalize().ok()?;
    let mut current = Some(start.as_path());
    while let Some(dir) = current {
        if has_git_metadata(dir) {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}
