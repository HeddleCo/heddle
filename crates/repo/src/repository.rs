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
pub use repository_signing::ResignOutcome;
#[path = "repository_snapshot.rs"]
mod repository_snapshot;
#[cfg(test)]
#[path = "repository_tests.rs"]
mod repository_tests;
#[path = "repository_thread_materialize.rs"]
mod repository_thread_materialize;
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
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
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
    object::{Attribution, ChangeId, ContentHash, MarkerName, Principal, State, ThreadName, Tree},
    store::{AnyStore, FsStore, ObjectStore, ShallowInfo},
    worktree::{WorktreeStatus, should_ignore as should_ignore_path},
};
use oplog::{OpLog, OpLogBackend, OpRecord};
pub use refs::RefSummaryIndexInspection;
use refs::{Head, RefBackend, RefExpectation, RefManager, RefUpdate};
use sley::{
    ObjectId as SleyObjectId, Reference as SleyReference, ReferenceTarget as SleyRefTarget,
    Repository as SleyRepository,
};

use crate::git_worktree_status::GitWorktreeEntryState;
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
pub use repository_thread_materialize::{CheckoutMaterialization, ThreadCaptureOutcome};
pub use repository_tree::{TreeBuildProfile, WorktreeCompareProfile};
pub use repository_worktree_status::{UntrackedSet, UntrackedSubtree, WorktreeStatusDetailed};
use serde::{Deserialize, Serialize};

const GIT_CHECKPOINTS_FILE: &str = "git-checkpoints.json";
const GIT_OVERLAY_LOCAL_EXCLUDE_PATTERNS: &[&str] = &[".heddle/"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryCapability {
    GitOverlay,
    NativeHeddle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GitHeadState {
    Attached(String),
    Detached(SleyObjectId),
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
    #[serde(skip)]
    pub mapped_change: Option<ChangeId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitOverlayTagTip {
    pub tag: String,
    pub git_commit: String,
    pub history_imported: bool,
    #[serde(skip)]
    pub mapped_change: Option<ChangeId>,
}

/// How many Git commits reachable from a branch tip have no Heddle mapping
/// (neither bridge-imported nor checkpointed). Used to report how far a Git
/// branch moved out-of-band before `heddle adopt --ref` reconciles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitOverlayOutOfBandCommits {
    pub count: usize,
    /// True when the walk stopped at the scan limit before exhausting the
    /// unmapped history; `count` is then a lower bound.
    pub truncated: bool,
}

/// Cap for the out-of-band commit walk so a read path (status/verify/health)
/// never pays an O(full-history) traversal when external history was rewritten
/// and no mapped ancestor exists.
const GIT_OVERLAY_OUT_OF_BAND_SCAN_LIMIT: usize = 1000;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_oid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_oid: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub upstream_is_undone_checkpoint: bool,
    pub message: String,
    pub next_action: String,
}

fn is_false(value: &bool) -> bool {
    !*value
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
///   uses sley promisor-fetch semantics against the bare `.git/` repo.
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
///
/// Generic over its reference, operation-log, and object-store backends.
/// The CLI uses the defaults — `Repository<RefManager, OpLog, AnyStore>`
/// (the on-disk local backends) — so the bare name `Repository` resolves to
/// the local flavor everywhere. The hosted server instantiates
/// `Repository<PgRefBackend, PgOpLogBackend, …>` via [`Repository::from_parts`].
///
/// The object store is the [`AnyStore`] enum by default: [`Repository::open`]
/// selects `FsStore` vs `S3Store` at *runtime* from config (`build_store`),
/// but the choice is a concrete enum variant rather than a `Box<dyn>`, so
/// every object access is static-dispatched through the enum to the inner
/// store — no vtable (heddle#283). `S` goes last so existing
/// `Repository<R, O>` references keep resolving with `S = AnyStore`.
pub struct Repository<R = RefManager, O = OpLog, S = AnyStore>
where
    R: RefBackend,
    O: OpLogBackend,
    S: ObjectStore,
{
    root: PathBuf,
    heddle_dir: PathBuf,
    capability: RepositoryCapability,
    store: S,
    refs: R,
    oplog: O,
    config: RepoConfig,
    shallow: RwLock<ShallowInfo>,
    blob_hydrator: RwLock<Option<Arc<dyn BlobHydrator>>>,
    git_overlay_repo: RwLock<Option<SleyRepository>>,
}

impl<R: RefBackend, O: OpLogBackend, S: ObjectStore> RepositoryLockExt for Repository<R, O, S> {
    fn locker(&self) -> RepoLock {
        let lock_root = self.heddle_dir.parent().expect(
            "heddle_dir has no parent component; cannot determine lock root. This indicates a misconfigured repository.",
        );
        RepoLock::new(lock_root)
    }
}

impl<R: RefBackend, O: OpLogBackend, S: ObjectStore> Repository<R, O, S> {
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
        store: S,
        refs: R,
        oplog: O,
        config: RepoConfig,
        shallow: ShallowInfo,
    ) -> Self {
        let capability = repository_capability_for_root(&root);
        Self {
            root,
            heddle_dir,
            capability,
            store,
            refs,
            oplog,
            config,
            shallow: RwLock::new(shallow),
            blob_hydrator: RwLock::new(None),
            git_overlay_repo: RwLock::new(None),
        }
    }

    /// The object store backing this repository.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The reference backend (threads, markers, HEAD).
    pub fn refs(&self) -> &R {
        &self.refs
    }

    /// The operation-log backend.
    pub fn oplog(&self) -> &O {
        &self.oplog
    }
}

/// Local-flavor opens generic over the object store `S`.
///
/// `open_raw` assembles a repository from already-resolved pieces and runs
/// none of the local-only open hooks (migrations, hydrator reconstruction) —
/// those are bound to the default `AnyStore` flavor and live in
/// [`Repository::run_open_hooks`], which the config-driven [`Repository::open`]
/// invokes after `open_raw`.
/// The per-worktree checkout lane (heddle#330 §1.5). Free function so the
/// reconciler can be wired at construction (before a `Repository` exists)
/// using the same computation as [`Repository::op_scope`].
pub(crate) fn compute_op_scope(root: &Path) -> String {
    let local_head = root.join(".heddle").join("HEAD");
    let canonical = local_head.canonicalize().unwrap_or(local_head);
    let digest = blake3::hash(canonical.to_string_lossy().as_bytes());
    format!("wt-{}", &digest.to_hex().as_str()[..16])
}

fn ensure_supported_repo_format(config_path: &Path, config: &RepoConfig) -> Result<()> {
    let found = config.repository.version;
    let supported = repo_config::SUPPORTED_REPO_FORMAT;
    if found > supported {
        return Err(HeddleError::RepositoryFormatTooNew {
            path: config_path.to_path_buf(),
            found,
            supported,
        });
    }
    Ok(())
}

impl<S: ObjectStore> Repository<RefManager, OpLog, S> {
    fn open_raw(
        root: PathBuf,
        heddle_dir: PathBuf,
        store: S,
        config: RepoConfig,
        refs: RefManager,
    ) -> Result<Self> {
        let actor = config
            .principal
            .as_ref()
            .map(|p| objects::object::Principal::new(&p.name, &p.email))
            .unwrap_or_else(|| objects::object::Principal::new("<unknown>", ""));
        let oplog = OpLog::new(&heddle_dir, actor.clone());
        let shallow = ShallowInfo::load(&heddle_dir)?;
        // Inject the oplog-backed read + write chokepoints (heddle#330 §2.2):
        // every logical read reconciles against the committed oplog tail, and
        // `commit_and_publish` appends a ref-carrying record before publishing.
        let reconciler = std::sync::Arc::new(crate::atomic::OplogRefReconciler::new(
            &heddle_dir,
            compute_op_scope(&root),
        ));
        let committer =
            std::sync::Arc::new(crate::atomic::OplogRefCommitter::new(&heddle_dir, actor));
        let refs = refs.with_reconciler(reconciler).with_committer(committer);
        // Seed the per-read watermark from the persisted last-clean point
        // (heddle#354 r5, cid 3329631074) so a fresh handle folds — and recovers
        // — a prior process's committed-but-unpublished crash tail on its next
        // read, without re-deriving long-since-deleted refs from ancient records.
        refs.init_reconcile_watermark()?;
        Ok(Self::from_parts(
            root, heddle_dir, store, refs, oplog, config, shallow,
        ))
    }

    /// Open an existing Heddle repository using a custom object store backend.
    ///
    /// Expert/test injection point: takes the store by value (any
    /// [`ObjectStore`]) and skips the local-only open hooks (declarative
    /// migrations, lazy-clone hydrator reconstruction) that [`Repository::open`]
    /// runs for the default `AnyStore` flavor.
    pub fn open_with_store(heddle_dir: impl AsRef<Path>, store: S) -> Result<Self> {
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
        let config_path = heddle_dir.join("config.toml");
        let config = RepoConfig::load(&config_path)?;
        ensure_supported_repo_format(&config_path, &config)?;
        let refs = RefManager::new(&heddle_dir);
        Self::open_raw(root, heddle_dir, store, config, refs)
    }
}

impl Repository {
    /// Run the local-only hooks that follow a config-driven [`Repository::open`]:
    /// declarative migrations + lazy-clone hydrator reconstruction. Both are
    /// bound to the default `AnyStore` flavor (`apply_pending` and
    /// `BlobHydrator` operate on the bare `Repository`), so they live here
    /// rather than in the generic `open_raw`.
    fn run_open_hooks(&self) {
        // Run any pending declarative migrations. Idempotent:
        // re-opening a repo a second time is a no-op for the migration pass.
        // Failures here are logged but non-fatal; surfacing migration errors
        // through `open` is worse than letting the repo open and warning later.
        if let Err(err) = crate::migration::apply_pending(self) {
            tracing::warn!("declarative migrations failed during repo open: {err}");
        }
        // Reconstruct any persisted lazy-clone blob hydrator. When
        // `.heddle/lazy-hydrator.toml` exists, look up the registered
        // factory for its `kind` and install the hydrator on the
        // freshly-opened repo so a subsequent `require_blob` against a
        // missing-blob marker can fetch transparently — without this
        // reconstruction, lazy clones would only work inside the single
        // `cmd_clone` process. See `lazy_hydrator.rs` for the shape.
        match crate::lazy_hydrator::try_reconstruct(self.root(), self.heddle_dir()) {
            Ok(Some(hydrator)) => self.set_blob_hydrator(hydrator),
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
    }

    /// Build an object store from the repository configuration.
    ///
    /// Returns an [`S3Store`] when `[storage.s3]` is configured and the `s3`
    /// feature is enabled, otherwise falls back to [`FsStore`] — wrapped in
    /// the [`AnyStore`] enum so the runtime choice stays statically dispatched.
    fn build_store(config: &RepoConfig, heddle_dir: &Path) -> Result<AnyStore> {
        #[cfg(feature = "s3")]
        {
            if let Some(s3) = &config.storage.s3 {
                return Self::build_s3_store(s3);
            }
        }
        let _ = config; // suppress unused warning when s3 feature is off
        Ok(AnyStore::Fs(FsStore::new(heddle_dir)))
    }

    /// Construct an [`S3Store`] from the repository's S3 storage configuration.
    #[cfg(feature = "s3")]
    fn build_s3_store(s3: &repo_config::S3StorageConfig) -> Result<AnyStore> {
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
        Ok(AnyStore::S3(store))
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
            thread: ThreadName::from("main"),
        })?;

        // Inject the oplog-backed read + write chokepoints (heddle#330 §2.2) —
        // same as `open_raw`, so a freshly-init'd handle reconciles and
        // record-commits too.
        let reconciler = std::sync::Arc::new(crate::atomic::OplogRefReconciler::new(
            &heddle_dir,
            compute_op_scope(&root),
        ));
        let committer = std::sync::Arc::new(crate::atomic::OplogRefCommitter::new(
            &heddle_dir,
            objects::object::Principal::new("<unknown>", ""),
        ));
        let refs = refs.with_reconciler(reconciler).with_committer(committer);
        // Establish the persisted reconcile watermark at init (heddle#354 r5,
        // cid 3329631074) so subsequent processes seed from a real last-clean
        // point — parity with `open_raw`.
        refs.init_reconcile_watermark()?;

        let capability = repository_capability_for_root(&root);
        Ok(Self {
            root,
            heddle_dir: heddle_dir.clone(),
            capability,
            store: AnyStore::Fs(store),
            refs,
            oplog,
            config,
            shallow: RwLock::new(ShallowInfo::load(&heddle_dir)?),
            blob_hydrator: RwLock::new(None),
            git_overlay_repo: RwLock::new(None),
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

    /// Install local, untracked Git exclude rules Heddle needs for Git-overlay
    /// repos. Only Heddle's sidecar is excluded automatically; project
    /// artifacts must be covered by `.gitignore` or `.heddleignore`.
    pub fn ensure_git_overlay_local_excludes(path: impl AsRef<Path>) -> Result<()> {
        ensure_git_overlay_exclude(path.as_ref())
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
        // A virtualized thread mounts at
        // `.heddle/threads/<encoded>/<repo-name>` and writes no checkout
        // metadata of its own. Without this guard, the upward walk below would
        // sail past the metadata-less mount and open the PARENT repo, so
        // status/capture/thread operations would silently hit the wrong
        // checkout. Refuse rather than resolve to the parent (heddle#572 r2).
        // Solid/materialized checkouts have their own `.heddle` pointer and
        // are handled by the worktree branch below, so this only fires for a
        // virtualized (or torn-down) mount root.
        if let Some(mount_root) = metadataless_managed_thread_root(&start_path) {
            return Err(HeddleError::Config(format!(
                "'{}' is a Heddle-managed virtualized thread mount with no checkout \
                 metadata of its own; refusing to operate on the parent repository from \
                 inside it. Run heddle from the repository root, or use a solid/materialized \
                 thread checkout.",
                mount_root.display()
            )));
        }
        let mut discovered_git_root = None;

        let mut current = Some(start_path.as_path());
        while let Some(dir) = current {
            if discovered_git_root.is_none() && has_git_metadata(dir) {
                discovered_git_root = Some(dir.to_path_buf());
            }
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

                    let config_path = shared_galeed_dir.join("config.toml");
                    let config = RepoConfig::load(&config_path)?;
                    ensure_supported_repo_format(&config_path, &config)?;
                    let store = Self::build_store(&config, &shared_galeed_dir)?;
                    let local_head_path = heddle_path.join("HEAD");
                    let refs = RefManager::new(&shared_galeed_dir).with_local_head(local_head_path);
                    let repo =
                        Self::open_raw(dir.to_path_buf(), shared_galeed_dir, store, config, refs)?;
                    repo.run_open_hooks();
                    return Ok(repo);
                }

                if objects_dir.is_dir() {
                    // Main repo mode.
                    let config_path = heddle_path.join("config.toml");
                    let config = RepoConfig::load(&config_path)?;
                    ensure_supported_repo_format(&config_path, &config)?;
                    let store = Self::build_store(&config, &heddle_path)?;
                    let refs = RefManager::new(&heddle_path);
                    let repo = Self::open_raw(dir.to_path_buf(), heddle_path, store, config, refs)?;
                    repo.run_open_hooks();
                    if repo.capability() == RepositoryCapability::GitOverlay {
                        match detect_git_head_state(dir) {
                            Ok(Some(GitHeadState::Attached(thread))) => {
                                let git_head = Head::Attached {
                                    thread: ThreadName::from(thread),
                                };
                                // Avoid the disk write when our HEAD already matches
                                // git's. Reading the existing head is a small file
                                // read; the write that follows hits atomic-rename
                                // machinery (sync + rename) which dominates here.
                                //
                                // Detached Heddle HEAD only counts as an explicit user
                                // override (e.g. `heddle goto`) when the detached
                                // state diverges from git's current branch tip.
                                // `cmd_clone` writes Head::Attached then calls
                                // repo.goto() — which unconditionally detaches —
                                // and relies on this reopen path to re-attach;
                                // when the detached state still matches the branch
                                // tip we treat that as a bootstrap leftover and
                                // sync. A user `heddle goto <other>` lands on a
                                // state that does *not* match the branch tip, so
                                // it survives (heddle#146).
                                let stale = match (repo.refs.read_head(), &git_head) {
                                    (Ok(Head::Detached { state }), Head::Attached { thread }) => {
                                        match repo.refs.get_thread(thread) {
                                            Ok(Some(tip)) => tip == state,
                                            _ => false,
                                        }
                                    }
                                    (Ok(Head::Detached { .. }), _) => false,
                                    (Ok(current), _) => current != git_head,
                                    (Err(_), _) => true,
                                };
                                if stale {
                                    repo.refs.write_head(&git_head)?;
                                }
                            }
                            Ok(Some(GitHeadState::Detached(git_oid))) => {
                                if let Ok(Some(state)) =
                                    repo.git_overlay_mapped_change_for_git_oid(git_oid)
                                {
                                    let git_head = Head::Detached { state };
                                    let stale = match repo.refs.read_head() {
                                        Ok(current) => current != git_head,
                                        Err(_) => true,
                                    };
                                    if stale {
                                        repo.refs.write_head(&git_head)?;
                                    }
                                }
                            }
                            Ok(None) | Err(_) => {}
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

    /// Root whose directory name should be used for managed thread checkout
    /// leaves.
    ///
    /// For the main checkout this is `repo.root()`. For an isolated checkout,
    /// `repo.root()` is the checkout's own directory (possibly custom-named),
    /// while `heddle_dir` points back at the shared source repository's
    /// `.heddle`; use that shared parent so child threads keep the original
    /// repo name.
    pub fn managed_checkout_source_root(&self) -> &Path {
        self.heddle_dir.parent().unwrap_or(self.root.as_path())
    }

    /// Default managed checkout path for `thread`.
    pub fn managed_checkout_path(&self, thread: &str) -> PathBuf {
        crate::thread_manifest::managed_checkout_path(
            &self.heddle_dir,
            thread,
            self.managed_checkout_source_root(),
        )
    }

    pub fn capability(&self) -> RepositoryCapability {
        self.capability
    }

    pub fn git_overlay_sley_repository(&self) -> Result<Option<SleyRepository>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }

        if let Some(repo) = self
            .git_overlay_repo
            .read()
            .map_err(|_| HeddleError::Config("git overlay repo cache lock poisoned".into()))?
            .clone()
        {
            return Ok(Some(repo));
        }

        let mut cached = self
            .git_overlay_repo
            .write()
            .map_err(|_| HeddleError::Config("git overlay repo cache lock poisoned".into()))?;
        if let Some(repo) = cached.clone() {
            return Ok(Some(repo));
        }

        let repo = SleyRepository::discover(&self.root).map_err(|error| {
            HeddleError::Config(format!(
                "failed to inspect Git repository at '{}': {}",
                self.root.display(),
                error
            ))
        })?;
        *cached = Some(repo.clone());
        Ok(Some(repo))
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
        if self.capability() == RepositoryCapability::GitOverlay
            && self.git_overlay_head_is_detached()?
            && detect_git_in_progress_branch(&self.root)?.is_none()
        {
            return Ok(None);
        }

        if self.current_state()?.is_none() && self.capability() == RepositoryCapability::GitOverlay
        {
            return self.git_overlay_current_branch();
        }

        match self.head_ref()? {
            Head::Attached { thread } => Ok(Some(thread.to_string())),
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

        let Some(git) = self.git_overlay_sley_repository()? else {
            return Ok(None);
        };
        let Some(head) = git_resolve_oid(&git, "HEAD")? else {
            return Ok(None);
        };

        let local_ref_name = format!("refs/heads/{branch}");
        if git_find_reference(&git, &local_ref_name)?.is_some()
            && let Some(tracking_name) = git_configured_tracking_ref(&git, &branch)?
            && let Some(upstream_head) = git_resolve_oid(&git, &tracking_name)?
        {
            let (ahead, behind) = git_ahead_behind(&self.root, &git, upstream_head, head)?;
            if ahead == 0 && behind == 0 {
                return Ok(None);
            }
            let upstream = git_remote_tracking_display_name(&tracking_name);
            let local_oid = head.to_string();
            let upstream_oid = upstream_head.to_string();
            let upstream_is_undone_checkpoint =
                self.remote_tracks_undone_git_checkpoint(&branch, &local_oid, &upstream_oid)?;
            return Ok(Some(GitRemoteTrackingStatus {
                branch: branch.clone(),
                upstream: upstream.clone(),
                ahead,
                behind,
                local_oid: Some(local_oid),
                upstream_oid: Some(upstream_oid),
                upstream_is_undone_checkpoint,
                message: git_remote_tracking_message(
                    &branch,
                    &upstream,
                    ahead,
                    behind,
                    upstream_is_undone_checkpoint,
                ),
                next_action: git_remote_tracking_next_action(
                    ahead,
                    behind,
                    upstream_is_undone_checkpoint,
                ),
            }));
        }

        let remotes = git_remote_names(&self.root)?;
        if remotes.is_empty() {
            return Ok(None);
        }
        for remote in &remotes {
            let remote_ref = format!("refs/remotes/{remote}/{branch}");
            if let Some(remote_head) = git_resolve_oid(&git, &remote_ref)? {
                if remote_head == head {
                    return Ok(None);
                }
                let (ahead, behind) = git_ahead_behind(&self.root, &git, remote_head, head)?;
                if behind > 0 {
                    let upstream = format!("{remote}/{branch}");
                    let local_oid = head.to_string();
                    let upstream_oid = remote_head.to_string();
                    let upstream_is_undone_checkpoint = self.remote_tracks_undone_git_checkpoint(
                        &branch,
                        &local_oid,
                        &upstream_oid,
                    )?;
                    return Ok(Some(GitRemoteTrackingStatus {
                        branch: branch.clone(),
                        upstream: upstream.clone(),
                        ahead,
                        behind,
                        local_oid: Some(local_oid),
                        upstream_oid: Some(upstream_oid),
                        upstream_is_undone_checkpoint,
                        message: git_remote_tracking_message(
                            &branch,
                            &upstream,
                            ahead,
                            behind,
                            upstream_is_undone_checkpoint,
                        ),
                        next_action: git_remote_tracking_next_action(
                            ahead,
                            behind,
                            upstream_is_undone_checkpoint,
                        ),
                    }));
                }
            }
        }

        Ok(Some(GitRemoteTrackingStatus {
            branch: branch.clone(),
            upstream: String::new(),
            ahead: 0,
            behind: 0,
            local_oid: Some(head.to_string()),
            upstream_oid: None,
            upstream_is_undone_checkpoint: false,
            message: format!("Git branch '{branch}' has no upstream tracking branch"),
            next_action: "heddle push".to_string(),
        }))
    }

    fn remote_tracks_undone_git_checkpoint(
        &self,
        branch: &str,
        local_oid: &str,
        upstream_oid: &str,
    ) -> Result<bool> {
        let scope = self.op_scope();
        let batches = self.oplog().redo_batches_scoped(64, Some(&scope))?;
        Ok(batches.iter().any(|batch| {
            batch.entries.iter().any(|entry| {
                if !entry.undone {
                    return false;
                }
                matches!(
                    &entry.operation,
                    OpRecord::GitCheckpoint {
                        branch: checkpoint_branch,
                        previous_git_oid: Some(previous_git_oid),
                        new_git_oid,
                        ..
                    } if checkpoint_branch == branch
                        && previous_git_oid == local_oid
                        && new_git_oid == upstream_oid
                )
            })
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
        let imported_threads: std::collections::HashSet<ThreadName> =
            self.refs().list_threads()?.into_iter().collect();
        let threads_with_real_history: std::collections::HashSet<String> = imported_threads
            .iter()
            .filter_map(|thread| {
                self.refs()
                    .get_thread(thread)
                    .ok()
                    .flatten()
                    .and_then(|change| self.store.get_state(&change).ok())
                    .flatten()
                    .filter(|state| !is_synthetic_root(state))
                    .map(|_| thread.to_string())
            })
            .collect();
        let mut missing_branches = branch_tips
            .into_iter()
            .filter(|tip| {
                !(tip.history_imported
                    || threads_with_real_history.contains(&tip.branch)
                        && tip.mapped_change.is_some())
            })
            .map(|tip| tip.branch)
            .collect::<Vec<_>>();
        missing_branches.sort_by(|left, right| {
            match (left == &current_branch, right == &current_branch) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => left.cmp(right),
            }
        });
        missing_branches.dedup();

        if missing_branches.is_empty() {
            return Ok(None);
        }

        let missing_tags = self
            .git_overlay_tag_tips()?
            .into_iter()
            .any(|tip| !tip.history_imported);
        let recommended_command = if missing_branches.len() > 1 || missing_tags {
            "heddle adopt".to_string()
        } else if missing_branches
            .iter()
            .any(|branch| branch == &current_branch)
        {
            format!("heddle adopt --ref {current_branch}")
        } else if missing_branches.len() == 1 {
            format!("heddle adopt --ref {}", missing_branches[0])
        } else {
            "heddle adopt".to_string()
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

        let Some(git_repo) = self.git_overlay_sley_repository()? else {
            return Ok(Vec::new());
        };

        let imported_threads: std::collections::HashSet<ThreadName> =
            self.refs().list_threads()?.into_iter().collect();
        let bridge_mapping = self.git_overlay_bridge_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        let mut branch_tips = Vec::new();

        for branch in git_repo.references().list_refs().map_err(|error| {
            HeddleError::Config(format!(
                "failed to enumerate git branches at '{}': {}",
                self.root.display(),
                error
            ))
        })? {
            let Some(name) = branch.name.strip_prefix("refs/heads/") else {
                continue;
            };
            let name = name.to_string();
            let Some(target) =
                self.git_overlay_commit_tip_oid(&git_repo, &branch, "branch", &name)?
            else {
                continue;
            };
            let git_commit = target.to_string();
            let mapped_change = self.git_overlay_mapped_change_for_commit(
                &git_commit,
                &bridge_mapping,
                &checkpoint_mapping,
            )?;
            let thread_name = ThreadName::from(name.as_str());
            let history_imported = if imported_threads.contains(&thread_name) {
                // Read the thread ref once; the mapped + checkpointed
                // checks each used to re-read it, which doubled the
                // ref-store hits per branch on a 60+ branch repo.
                let existing_thread = self.refs().get_thread(&thread_name)?;
                let mapped = matches!(
                    (existing_thread.as_ref(), mapped_change.as_ref()),
                    (Some(existing), Some(mapped_change))
                        if existing == mapped_change
                );
                let checkpointed = if mapped {
                    false
                } else if let Some(existing) = existing_thread {
                    self.latest_git_checkpoint_for_change(&existing)?
                        .is_some_and(|record| record.git_commit == git_commit)
                        || mapped_change.as_ref().is_some_and(|mapped_change| {
                            self.change_is_ancestor(mapped_change, &existing)
                        })
                } else {
                    false
                };
                mapped || checkpointed
            } else {
                mapped_change.is_some()
            };
            branch_tips.push(GitOverlayBranchTip {
                branch: name,
                git_commit,
                history_imported,
                mapped_change,
            });
        }
        branch_tips.sort_by(|a, b| a.branch.cmp(&b.branch));
        Ok(branch_tips)
    }

    pub fn git_overlay_tag_tips(&self) -> Result<Vec<GitOverlayTagTip>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(Vec::new());
        }

        let Some(git_repo) = self.git_overlay_sley_repository()? else {
            return Ok(Vec::new());
        };

        let imported_markers: std::collections::HashSet<MarkerName> =
            self.refs().list_markers()?.into_iter().collect();
        let bridge_mapping = self.git_overlay_bridge_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        let mut tag_tips = Vec::new();

        for tag in git_repo.references().list_refs().map_err(|error| {
            HeddleError::Config(format!(
                "failed to enumerate git tags at '{}': {}",
                self.root.display(),
                error
            ))
        })? {
            let Some(name) = tag.name.strip_prefix("refs/tags/") else {
                continue;
            };
            let name = name.to_string();
            let Some(target) = self.git_overlay_commit_tip_oid(&git_repo, &tag, "tag", &name)?
            else {
                continue;
            };
            let git_commit = target.to_string();
            let mapped_change = self.git_overlay_mapped_change_for_commit(
                &git_commit,
                &bridge_mapping,
                &checkpoint_mapping,
            )?;
            let marker_name = MarkerName::from(name.as_str());
            let history_imported = if imported_markers.contains(&marker_name) {
                matches!(
                    (self.refs().get_marker(&marker_name)?, mapped_change.as_ref()),
                    (Some(existing), Some(mapped_change)) if existing == *mapped_change
                )
            } else {
                false
            };
            tag_tips.push(GitOverlayTagTip {
                tag: name,
                git_commit,
                history_imported,
                mapped_change,
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

    pub fn git_overlay_mapped_change_for_branch(&self, name: &str) -> Result<Option<ChangeId>> {
        Ok(self
            .git_overlay_branch_tip(name)?
            .and_then(|tip| tip.mapped_change))
    }

    pub fn git_overlay_mapped_change_for_tag(&self, name: &str) -> Result<Option<ChangeId>> {
        Ok(self
            .git_overlay_tag_tip(name)?
            .and_then(|tip| tip.mapped_change))
    }

    fn change_is_ancestor(&self, ancestor: &ChangeId, descendant: &ChangeId) -> bool {
        let mut graph = CommitGraphIndex::new(self);
        graph.is_ancestor(ancestor, descendant).unwrap_or(false)
    }

    pub fn git_overlay_worktree_status(&self) -> Result<Option<WorktreeStatus>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }
        let git_repo = match self.git_overlay_sley_repository() {
            Ok(Some(repo)) => repo,
            Ok(None) | Err(_) => return Ok(None),
        };
        if git_repo.workdir().is_none() {
            return Ok(None);
        }

        let index = git_repo.open_index().map_err(|error| {
            HeddleError::Config(format!(
                "failed to inspect Git index at '{}': {}",
                self.root.display(),
                error
            ))
        })?;
        let index = index.unwrap_or_else(|| sley::Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        });
        let head_tree = match git_repo
            .head()
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to inspect Git HEAD tree at '{}': {}",
                    self.root.display(),
                    error
                ))
            })?
            .oid
        {
            Some(head) => {
                git_repo
                    .read_commit(&head)
                    .map_err(|error| {
                        HeddleError::Config(format!(
                            "failed to inspect Git HEAD commit at '{}': {}",
                            self.root.display(),
                            error
                        ))
                    })?
                    .tree
            }
            None => sley::ObjectId::empty_tree(git_repo.object_format()),
        };
        let head_index = git_repo.index_from_tree(&head_tree).map_err(|error| {
            HeddleError::Config(format!(
                "failed to inspect Git HEAD tree at '{}': {}",
                self.root.display(),
                error
            ))
        })?;

        let mut head_entries = BTreeMap::new();
        for entry in head_index.entries {
            let path = git_path(entry.path.as_bytes());
            head_entries.insert(path, (entry.oid, entry.mode));
        }
        let mut index_entries = BTreeMap::new();
        let index_path = sley::plumbing::sley_worktree::repository_index_path(git_repo.git_dir());
        for entry in index.entries {
            let path = git_path(entry.path.as_bytes());
            let probe = crate::git_worktree_status::IndexStatProbe::from_index_entry_and_index_path(
                entry.clone(),
                &index_path,
            );
            index_entries.insert(path, (entry.oid, entry.mode, probe));
        }

        let mut added = BTreeSet::new();
        let mut modified = BTreeSet::new();
        let mut deleted = BTreeSet::new();

        for (path, (oid, mode, _probe)) in &index_entries {
            if ignored_git_overlay_status_path(path) {
                continue;
            }
            match head_entries.get(path) {
                None => {
                    added.insert(PathBuf::from(path));
                }
                Some((head_oid, head_mode)) if (head_oid, head_mode) != (oid, mode) => {
                    modified.insert(PathBuf::from(path));
                }
                Some(_) => {}
            }
        }
        for path in head_entries.keys() {
            if !ignored_git_overlay_status_path(path) && !index_entries.contains_key(path) {
                deleted.insert(PathBuf::from(path));
            }
        }

        for (path, (oid, mode, probe)) in &index_entries {
            if ignored_git_overlay_status_path(path) {
                continue;
            }
            match crate::git_worktree_status::git_worktree_entry_state_in_repo(
                &git_repo,
                &self.root,
                path,
                *oid,
                *mode,
                Some(probe.clone()),
            )? {
                GitWorktreeEntryState::Clean => {}
                GitWorktreeEntryState::Deleted => {
                    deleted.insert(PathBuf::from(path));
                }
                GitWorktreeEntryState::Modified => {
                    modified.insert(PathBuf::from(path));
                }
            }
        }

        let ignore_patterns = self.ignore_patterns()?;
        let tracked_paths: BTreeSet<&str> = index_entries.keys().map(String::as_str).collect();
        for path in git_overlay_untracked_paths(&self.root, &tracked_paths, &ignore_patterns)? {
            added.insert(PathBuf::from(path));
        }

        Ok(Some(WorktreeStatus {
            modified: modified.into_iter().collect(),
            added: added.into_iter().collect(),
            deleted: deleted.into_iter().collect(),
        }))
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

    fn git_overlay_checkpoint_mapping(&self) -> Result<HashMap<String, String>> {
        Ok(self
            .list_git_checkpoints()?
            .into_iter()
            .map(|record| (record.git_commit, record.change_id))
            .collect())
    }

    fn git_overlay_mapped_change_for_commit(
        &self,
        git_commit: &str,
        bridge_mapping: &HashMap<String, String>,
        checkpoint_mapping: &HashMap<String, String>,
    ) -> Result<Option<ChangeId>> {
        let Some(change) = bridge_mapping
            .get(git_commit)
            .or_else(|| checkpoint_mapping.get(git_commit))
        else {
            return Ok(None);
        };
        let change_id = ChangeId::parse(change).map_err(|error| {
            HeddleError::Config(format!(
                "git commit {git_commit} maps to invalid Heddle change id '{change}': {error}"
            ))
        })?;
        if self.store.get_state(&change_id)?.is_some() {
            Ok(Some(change_id))
        } else {
            Ok(None)
        }
    }

    fn git_overlay_mapped_change_for_git_oid(
        &self,
        git_oid: SleyObjectId,
    ) -> Result<Option<ChangeId>> {
        let git_commit = git_oid.to_string();
        let bridge_mapping = self.git_overlay_bridge_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        self.git_overlay_mapped_change_for_commit(&git_commit, &bridge_mapping, &checkpoint_mapping)
    }

    /// Count the Git commits reachable from `tip_git_commit` that are not
    /// represented in Heddle state (no bridge mapping and no checkpoint
    /// mapping). The walk prunes at the first mapped commit on each lineage,
    /// so the cost is proportional to the out-of-band suffix, capped at
    /// `GIT_OVERLAY_OUT_OF_BAND_SCAN_LIMIT`.
    ///
    /// Returns `Ok(None)` when the repository is not a Git overlay or the tip
    /// cannot be resolved; callers should degrade to a countless report.
    pub fn git_overlay_out_of_band_commits(
        &self,
        tip_git_commit: &str,
    ) -> Result<Option<GitOverlayOutOfBandCommits>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }
        let git_repo = match self.git_overlay_sley_repository() {
            Ok(Some(repo)) => repo,
            Ok(None) | Err(_) => return Ok(None),
        };
        let Ok(tip) = SleyObjectId::from_hex(git_repo.object_format(), tip_git_commit) else {
            return Ok(None);
        };

        let bridge_mapping = self.git_overlay_bridge_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;

        let mut pending = vec![tip];
        let mut visited = std::collections::HashSet::new();
        let mut count = 0usize;
        while let Some(oid) = pending.pop() {
            if !visited.insert(oid) {
                continue;
            }
            let git_commit = oid.to_string();
            if self
                .git_overlay_mapped_change_for_commit(
                    &git_commit,
                    &bridge_mapping,
                    &checkpoint_mapping,
                )?
                .is_some()
            {
                // Mapped into Heddle: this lineage is reconciled; stop here.
                continue;
            }
            count += 1;
            if count >= GIT_OVERLAY_OUT_OF_BAND_SCAN_LIMIT {
                return Ok(Some(GitOverlayOutOfBandCommits {
                    count,
                    truncated: true,
                }));
            }
            let Ok(commit) = git_repo.read_commit(&oid) else {
                continue;
            };
            for parent in commit.parents {
                pending.push(parent);
            }
        }
        Ok(Some(GitOverlayOutOfBandCommits {
            count,
            truncated: false,
        }))
    }

    pub fn git_overlay_current_branch(&self) -> Result<Option<String>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }

        match detect_git_head_state(&self.root)? {
            Some(GitHeadState::Attached(branch)) => return Ok(Some(branch)),
            Some(GitHeadState::Detached(_)) | None => {}
        }

        detect_git_in_progress_branch(&self.root)
    }

    pub fn git_overlay_head_is_detached(&self) -> Result<bool> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(false);
        }

        Ok(matches!(
            detect_git_head_state(&self.root)?,
            Some(GitHeadState::Detached(_))
        ))
    }

    pub fn git_overlay_detached_head_commit(&self) -> Result<Option<String>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }

        Ok(match detect_git_head_state(&self.root)? {
            Some(GitHeadState::Detached(git_oid)) => Some(git_oid.to_string()),
            Some(GitHeadState::Attached(_)) | None => None,
        })
    }

    fn git_overlay_commit_tip_oid(
        &self,
        git_repo: &SleyRepository,
        reference: &sley::plumbing::sley_refs::Ref,
        ref_kind: &str,
        ref_name: &str,
    ) -> Result<Option<SleyObjectId>> {
        let target = match &reference.target {
            SleyRefTarget::Direct(oid) => *oid,
            SleyRefTarget::Symbolic(_) => return Ok(None),
        };
        let target = match sley::plumbing::sley_rev::peel_to_commit(
            git_repo.objects().as_ref(),
            git_repo.object_format(),
            &target,
        ) {
            Ok(target) => target,
            Err(_) => return Ok(None),
        };

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
                // The `bisect` verb was removed in the whole-CLI consolidation
                // (heddle#473); a lingering BISECT_STATE can only come from an
                // older binary, and the only valid recovery now is to abort.
                message: "Heddle bisect is in progress".to_string(),
                next_action: "heddle abort".to_string(),
            }));
        }

        Ok(None)
    }

    fn git_operation_status(&self) -> Result<Option<RepositoryOperationStatus>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }

        let git_dir = resolve_git_dir(&self.root)?;
        let raw_git_next_action = "heddle bridge git status";
        let candidates = [
            (
                git_dir.join("rebase-merge"),
                OperationKind::Rebase,
                "Git rebase is in progress",
                raw_git_next_action,
            ),
            (
                git_dir.join("rebase-apply"),
                OperationKind::Rebase,
                "Git rebase is in progress",
                raw_git_next_action,
            ),
            (
                git_dir.join("MERGE_HEAD"),
                OperationKind::Merge,
                "Git merge is in progress",
                raw_git_next_action,
            ),
            (
                git_dir.join("CHERRY_PICK_HEAD"),
                OperationKind::CherryPick,
                "Git cherry-pick is in progress",
                raw_git_next_action,
            ),
            (
                git_dir.join("REVERT_HEAD"),
                OperationKind::Revert,
                "Git revert is in progress",
                raw_git_next_action,
            ),
            (
                git_dir.join("BISECT_LOG"),
                OperationKind::Bisect,
                "Git bisect is in progress",
                raw_git_next_action,
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
        compute_op_scope(&self.root)
    }

    /// The write chokepoint (heddle#330 §2.2): commit the ref-carrying
    /// `OpRecord` batch (phase 4) **before** publishing the atomic `ref_updates`
    /// batch (phase 5), record-before-publish. Encodes the records opaquely and
    /// routes through [`RefBackend::commit_and_publish`] so the backend's seam
    /// enforces the ordering — the file backend appends-then-publishes, a
    /// Postgres backend would co-commit in one SQL transaction. Replaces the
    /// publish-then-record order that left a reader-visible ref with no undo
    /// record (the fork/collapse bug).
    pub fn commit_and_publish(
        &self,
        records: Vec<OpRecord>,
        ref_updates: &[RefUpdate],
    ) -> Result<()> {
        let encoded = records
            .iter()
            .map(|record| {
                rmp_serde::to_vec(record).map_err(|e| HeddleError::Serialization(e.to_string()))
            })
            .collect::<Result<Vec<_>>>()?;
        let scope = self.op_scope();
        let result = self
            .refs
            .commit_and_publish(&encoded, ref_updates, Some(&scope));
        // The committer appended through a fresh `OpLog` handle (the `refs`→`repo`
        // seam), so this repository's own cached oplog handle is now stale.
        // Refresh it so a same-process read via `self.oplog()` observes the
        // just-committed records — the long-lived mount/daemon handle would
        // otherwise miss them (heddle#354 r8). Best-effort: a refresh failure
        // only costs a stale cache until the next disk reload, never correctness.
        let _ = self.oplog.refresh_cache();
        result
    }

    /// Atomically commit a snapshot's `OpRecord::Snapshot` and its paired ref
    /// publish through the write chokepoint, **record-first** (heddle#354 r8).
    ///
    /// The pre-r8 snapshot path published the ref FIRST (`refs.set_thread` /
    /// `refs.write_head`) and recorded SECOND. Because the reconciler folds a
    /// `Snapshot` record authoritatively (newest committed record wins), a late
    /// snapshot record carrying a stale thread value could clobber a newer
    /// concurrent write that had already recorded. Routing every snapshot ref
    /// write through this single chokepoint makes the record the unit of
    /// ordering: the newest committed record IS the newest write, so the
    /// authoritative fold can no longer resurrect a stale snapshot.
    ///
    /// `thread = Some(name)` advances that thread (HEAD stays attached);
    /// `thread = None` republishes a detached HEAD. The detached case is now
    /// record-first too, so a phase-4-committed / phase-5-unpublished crash is
    /// recovered by the reconciler reconstructing `Head::Detached{new_state}`
    /// (see `atomic::reconciler`'s detached-`Snapshot` arm).
    pub fn commit_snapshot_atomic(
        &self,
        new_state: &ChangeId,
        prev_head: Option<ChangeId>,
        thread: Option<&ThreadName>,
    ) -> Result<()> {
        self.commit_snapshot_atomic_with_records(new_state, prev_head, thread, Vec::new())
    }

    /// [`commit_snapshot_atomic`](Self::commit_snapshot_atomic) plus `extra`
    /// records folded into the SAME batch as the `OpRecord::Snapshot`.
    ///
    /// Used by the snapshot creators that commit through this chokepoint rather
    /// than the `SnapshotMutation` transaction (the in-progress merge branch and
    /// the mount capture path) to fold the automatic capture-time
    /// default-visibility binding's `OpRecord::StateVisibilitySet` into the
    /// snapshot's batch, so one `heddle undo` reverts the snapshot and its
    /// auto-applied default tier together (heddle#317 / PR #529 P1).
    pub fn commit_snapshot_atomic_with_records(
        &self,
        new_state: &ChangeId,
        prev_head: Option<ChangeId>,
        thread: Option<&ThreadName>,
        extra: Vec<OpRecord>,
    ) -> Result<()> {
        let record = OpRecord::Snapshot {
            new_state: *new_state,
            prev_head,
            head: thread.is_none().then_some(*new_state),
            thread: thread.map(|name| name.to_string()),
        };
        let mut records = vec![record];
        records.extend(extra);
        let ref_update = match thread {
            Some(name) => RefUpdate::Thread {
                name: name.clone(),
                expected: RefExpectation::Any,
                new: Some(*new_state),
            },
            None => RefUpdate::Head {
                expected: RefExpectation::Any,
                new: Head::Detached { state: *new_state },
            },
        };
        self.commit_and_publish(records, &[ref_update])
    }

    /// Commit a snapshot batch that folds the automatic capture-time
    /// default-visibility binding, **rewinding the staged sidecar if the commit
    /// fails** (heddle#317 invariant 2).
    ///
    /// This is THE single fold-and-rewind chokepoint for snapshot creators that
    /// commit *outside* the [`SnapshotMutation`](crate::repository_snapshot)
    /// executor — the mount capture path and the in-progress-merge branch. Those
    /// paths cannot lean on the executor's `rewind`, so the rollback guarantee
    /// lives here, by construction: the binding's sidecar is written by
    /// [`stage_default_visibility_binding`](Self::stage_default_visibility_binding)
    /// *before* the batch commits, and if the commit errors the sidecar is
    /// rewound to its pre-binding image so no orphaned non-public sidecar is left
    /// for a state whose snapshot batch never committed.
    ///
    /// `lock_held` is forwarded to `stage_default_visibility_binding`: the merge
    /// branch already holds the snapshot write lock (`true`); the mount path
    /// holds none (`false`). A public default stages nothing (absence ≡ public)
    /// and the commit runs with no folded record.
    pub fn commit_snapshot_atomic_with_capture_visibility(
        &self,
        new_state: &ChangeId,
        prev_head: Option<ChangeId>,
        thread: Option<&ThreadName>,
        lock_held: bool,
    ) -> Result<()> {
        let binding = self
            .stage_default_visibility_binding(new_state, lock_held)
            .map_err(|e| HeddleError::Io(std::io::Error::other(format!("{e:#}"))))?;
        let (extra, rewind_to): (Vec<OpRecord>, Option<Option<Vec<u8>>>) = match binding {
            Some(binding) => (vec![binding.record], Some(binding.prior_sidecar)),
            None => (Vec::new(), None),
        };

        // Test seam (heddle#317 inv 2): fail the commit AFTER the binding's
        // sidecar is staged, so the rewind path is exercised deterministically.
        #[cfg(test)]
        let commit_result = if crate::repository_state_visibility::take_visibility_commit_fault(
            crate::repository_state_visibility::VisibilityCommitFault::SnapshotCommit,
        ) {
            Err(HeddleError::Io(std::io::Error::other(
                "injected snapshot-commit failure after staging visibility binding",
            )))
        } else {
            self.commit_snapshot_atomic_with_records(new_state, prev_head, thread, extra)
        };
        #[cfg(not(test))]
        let commit_result =
            self.commit_snapshot_atomic_with_records(new_state, prev_head, thread, extra);

        match commit_result {
            Ok(()) => Ok(()),
            Err(commit_err) => {
                if let Some(prior) = rewind_to {
                    // Best-effort rewind to the pre-binding sidecar; the commit
                    // error is what the caller acts on. A rewind failure is
                    // logged, never masking the original error.
                    if let Err(rewind_err) = self.restore_state_visibility_sidecar(new_state, prior)
                    {
                        tracing::warn!(
                            state = %new_state,
                            error = %rewind_err,
                            "rewind of staged visibility binding after a failed snapshot commit also failed"
                        );
                    }
                }
                Err(commit_err)
            }
        }
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
        // Reserve the operator-local courtesy-stub filename. It is a Heddle
        // artifact written for under-tier checkouts, never tracked content.
        // Excluding it here is the single tree-build chokepoint every capture path
        // consults (`build_tree`, `build_tree_with_stat_cache`, and the stat-cache
        // no-op predicate), so the stub can never be pulled into a captured thread
        // by any of them — including a plain `snapshot`/`capture` taken from inside
        // a withheld worktree, which does not go through the withheld-manifest guard
        // (heddle#316). ROOT-ANCHORED (`/HEDDLE-EMBARGO.txt`): the stub is only ever
        // written at the worktree root, so the bare filename — which gitignore
        // matches at ANY depth — would silently drop a user's own
        // `sub/HEDDLE-EMBARGO.txt` from capture (heddle#316 #9).
        patterns.push(format!(
            "/{}",
            repository_thread_materialize::COURTESY_STUB_FILENAME
        ));
        if self.capability() == RepositoryCapability::GitOverlay {
            patterns.push(".git".to_string());
            append_ignore_file_patterns(&mut patterns, &self.root.join(".gitignore"))?;
        }
        // Worktree-local, never-captured excludes (heddle's analogue of
        // `.git/info/exclude`). Lives under THIS worktree's own `.heddle/`
        // (`root/.heddle`, which is local even for a shared-store checkout), so
        // it is never captured. Lets `start --hydrate` ignore symlinked deps
        // without dirtying a tracked `.heddleignore` (heddle#356 cid 3333881577).
        // `append_ignore_file_patterns` no-ops when the file is absent — the
        // common case for a plain repo.
        append_ignore_file_patterns(
            &mut patterns,
            &self.root.join(".heddle").join("info").join("exclude"),
        )?;
        let path = self.root.join(".heddleignore");

        if path.exists() {
            append_ignore_file_patterns(&mut patterns, &path)?;
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
        Ok(match self.head_ref()? {
            Head::Attached { thread } => match self.refs.get_thread(&thread)? {
                Some(change_id) => Some(change_id),
                None if self.capability() == RepositoryCapability::GitOverlay => {
                    self.git_overlay_mapped_change_for_branch(&thread)?
                }
                None => None,
            },
            Head::Detached { state } => Some(state),
        })
    }

    pub fn head_ref(&self) -> Result<Head> {
        let raw = self.refs.read_head()?;
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(raw);
        }
        if matches!(raw, Head::Detached { .. }) {
            return Ok(raw);
        }
        if let Some(GitHeadState::Detached(git_oid)) = detect_git_head_state(&self.root)?
            && let Some(state) = self.git_overlay_mapped_change_for_git_oid(git_oid)?
        {
            return Ok(Head::Detached { state });
        }
        let Some(branch) = self.git_overlay_current_branch()? else {
            return Ok(raw);
        };
        if matches!(&raw, Head::Attached { thread } if *thread == branch) {
            return Ok(raw);
        }
        let branch_thread = ThreadName::from(branch.as_str());
        if self.refs.get_thread(&branch_thread)?.is_some()
            || self
                .git_overlay_mapped_change_for_branch(&branch)?
                .is_some()
        {
            return Ok(Head::Attached {
                thread: branch_thread,
            });
        }
        Ok(raw)
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

        if self.capability() == RepositoryCapability::GitOverlay
            && let Some(principal) = git_config_principal(&self.root)
        {
            return Ok(principal);
        }

        if let Some(principal) = self.shared_checkout_parent_git_principal() {
            return Ok(principal);
        }

        Ok(Principal::new("Unknown", "unknown@example.com"))
    }

    fn shared_checkout_parent_git_principal(&self) -> Option<Principal> {
        let local_heddle_dir = self.root.join(".heddle");
        if local_heddle_dir == self.heddle_dir || !local_heddle_dir.join("objectstore").is_file() {
            return None;
        }
        let parent_root = self.heddle_dir.parent()?;
        if parent_root == self.root {
            return None;
        }
        git_config_principal(parent_root)
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
        let main_thread = ThreadName::from("main");
        if self.refs.get_thread(&main_thread)?.is_some() {
            return Ok(());
        }

        let empty_tree = Tree::new();
        let tree_hash = self.store.put_tree(&empty_tree)?;
        let state = State::new_snapshot(tree_hash, vec![], Attribution::human(seed_principal()));
        self.store.put_state(&state)?;
        self.refs.set_thread(&main_thread, &state.change_id)?;
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

    /// Load a tree by hash from the object store, surfacing a clear
    /// error when the hash resolves to nothing.
    ///
    /// Use this whenever a hash recorded in a `State.tree` field or as
    /// a subtree `TreeEntry` MUST resolve to an object: presentation
    /// paths (`heddle status`, `heddle ready`, `heddle stash show`),
    /// mutation paths (`heddle revert`, `heddle cherry-pick`,
    /// `heddle goto`, `heddle resolve`), and inspection paths
    /// (semantic diff, harness baseline) all qualify.
    ///
    /// Replaces the legacy `get_tree(...)?.unwrap_or_default()`
    /// pattern. That pattern silently substituted `Tree::default()`
    /// for a missing object, so presentation paths rendered "no
    /// content" and mutation paths committed subtree-erasure merges
    /// (see heddle#90 for the merge-path lock and heddle#93 for the
    /// non-merge sweep that motivated this method).
    ///
    /// Returns [`HeddleError::MissingObject`] with `object_type =
    /// "tree"` so callers and the top-level error printer can
    /// recognize the bug class. The `Display` impl on `MissingObject`
    /// includes the `heddle fsck --full` recovery hint, so call sites
    /// don't need to wrap with anyhow context to give the operator a
    /// next step.
    ///
    /// Pair with [`Repository::require_blob`] for the blob side of the
    /// same contract.
    pub fn require_tree(&self, hash: &ContentHash) -> Result<Tree> {
        self.store
            .get_tree(hash)?
            .ok_or_else(|| HeddleError::MissingObject {
                object_type: "tree".to_string(),
                id: hash.to_hex(),
            })
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
    let git_dir = match SleyRepository::discover(root) {
        Ok(repo) if repo.workdir().is_some() => repo.git_dir().to_path_buf(),
        _ => root.join(".git"),
    };
    if !git_dir.is_dir() {
        return Ok(());
    }

    let info_dir = git_dir.join("info");
    fs::create_dir_all(&info_dir)?;
    let exclude_path = info_dir.join("exclude");
    let mut contents = fs::read_to_string(&exclude_path).unwrap_or_default();
    let existing_lines = contents.lines().map(str::trim).collect::<BTreeSet<_>>();
    let mut missing = Vec::new();
    for pattern in GIT_OVERLAY_LOCAL_EXCLUDE_PATTERNS {
        if !existing_lines
            .iter()
            .any(|line| git_overlay_exclude_line_matches(line, pattern))
        {
            missing.push(*pattern);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str("# Heddle local metadata\n");
    for pattern in missing {
        contents.push_str(pattern);
        contents.push('\n');
    }
    fs::write(exclude_path, contents)?;
    Ok(())
}

fn git_overlay_exclude_line_matches(line: &str, pattern: &str) -> bool {
    line == pattern
        || matches!(
            (line, pattern),
            (".heddle", ".heddle/") | ("/.heddle/", ".heddle/") | ("/.heddle", ".heddle/")
        )
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
    if !(dot_git.is_dir() || dot_git.is_file()) {
        return false;
    }

    SleyRepository::discover(path).is_ok()
}

/// If `start_path` lies inside a *managed virtualized thread root*
/// (`<repo>/.heddle/threads/<encoded>/<repo-name>`) that carries NO
/// checkout metadata of its own, return that mount root.
///
/// Solid and materialized thread checkouts write their own `.heddle`
/// objectstore pointer at the checkout root, so [`Repository::open`]
/// resolves them as a worktree before it climbs to the parent. A
/// *virtualized* thread mounts a content-addressed projection there and
/// writes no such pointer, so a bare upward walk would sail past the
/// metadata-less mount and open the PARENT repo. The flat
/// `thread_manifest::thread_dir` encoding guarantees `<encoded>` is exactly
/// one path component, so any direct checkout leaf below it has the
/// unambiguous `<leaf> → <encoded> → threads → .heddle` shape (heddle#572 r2).
fn metadataless_managed_thread_root(start_path: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start_path);
    while let Some(dir) = cur {
        if let Some(thread_dir) = dir.parent()
            && let Some(threads) = thread_dir.parent()
            && threads.file_name().and_then(|n| n.to_str()) == Some("threads")
            && let Some(heddle) = threads.parent()
            && heddle.file_name().and_then(|n| n.to_str()) == Some(".heddle")
            && heddle.join("objects").is_dir()
            && !dir.join(".heddle").exists()
        {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

fn git_config_principal(root: &Path) -> Option<Principal> {
    let git_repo = SleyRepository::discover(root).ok()?;
    let config = git_repo.config_snapshot().ok()?;
    let name = config.get("user", None, "name")?.to_string();
    let email = config.get("user", None, "email")?.to_string();
    if name.trim().is_empty() || email.trim().is_empty() {
        return None;
    }
    Some(Principal::new(&name, &email))
}

fn git_overlay_untracked_paths(
    root: &Path,
    tracked_paths: &BTreeSet<&str>,
    ignore_patterns: &[String],
) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    let filter_root = root.to_path_buf();
    let filter_ignore_patterns = ignore_patterns.to_vec();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .filter_entry(move |entry| {
            should_descend_for_git_overlay_status(
                &filter_root,
                entry.path(),
                &filter_ignore_patterns,
            )
        })
        .build();
    for entry in walker {
        let entry = entry.map_err(|error| HeddleError::Config(error.to_string()))?;
        let file_type = entry.file_type();
        if !file_type.is_some_and(|file_type| file_type.is_file() || file_type.is_symlink()) {
            continue;
        }
        let path = repo_relative_git_path(root, entry.path())?;
        if !tracked_paths.contains(path.as_str())
            && !ignored_git_overlay_status_path(&path)
            && !should_ignore_path(Path::new(&path), ignore_patterns)
        {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn should_descend_for_git_overlay_status(
    root: &Path,
    path: &Path,
    ignore_patterns: &[String],
) -> bool {
    if is_git_or_heddle_dir(path) {
        return false;
    }
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    if relative.as_os_str().is_empty() {
        return true;
    }
    !should_ignore_path(relative, ignore_patterns)
}

fn is_git_or_heddle_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".git" || name == ".heddle")
}

fn repo_relative_git_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(root).map_err(|error| {
        HeddleError::Config(format!(
            "failed to relativize Git worktree path '{}': {}",
            path.display(),
            error
        ))
    })?;
    Ok(path_to_git_path(relative))
}

fn path_to_git_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn git_path(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

fn ignored_git_overlay_status_path(path: &str) -> bool {
    path == ".heddle" || path.starts_with(".heddle/")
}

fn git_remote_names(root: &Path) -> Result<Vec<String>> {
    let repo = match SleyRepository::discover(root) {
        Ok(repo) => repo,
        Err(_) => return Ok(Vec::new()),
    };
    repo.remote_names()
        .map(|names| {
            names
                .into_iter()
                .filter(|name| !name.trim().is_empty())
                .collect()
        })
        .map_err(|error| HeddleError::Config(error.to_string()))
}

fn git_find_reference(repo: &SleyRepository, name: &str) -> Result<Option<SleyReference>> {
    repo.find_reference(name).map_err(|error| {
        HeddleError::Config(format!("failed to inspect Git reference '{name}': {error}"))
    })
}

fn git_resolve_oid(repo: &SleyRepository, rev: &str) -> Result<Option<SleyObjectId>> {
    match repo.rev_parse(rev) {
        Ok(id) => Ok(Some(id)),
        Err(_) => Ok(None),
    }
}

fn git_configured_tracking_ref(repo: &SleyRepository, branch: &str) -> Result<Option<String>> {
    let config = repo
        .config_snapshot()
        .map_err(|error| HeddleError::Config(error.to_string()))?;
    let Some(remote) = config.get("branch", Some(branch), "remote") else {
        return Ok(None);
    };
    let Some(merge) = config.get("branch", Some(branch), "merge") else {
        return Ok(None);
    };
    if remote == "." {
        return Ok(Some(merge.to_string()));
    }
    let Some(short) = merge.strip_prefix("refs/heads/") else {
        return Ok(None);
    };
    Ok(Some(format!("refs/remotes/{remote}/{short}")))
}

fn git_ahead_behind(
    root: &Path,
    repo: &SleyRepository,
    upstream: SleyObjectId,
    head: SleyObjectId,
) -> Result<(usize, usize)> {
    if upstream == head {
        return Ok((0, 0));
    }
    let ahead = git_reachable_count(root, repo, head, upstream)?;
    let behind = git_reachable_count(root, repo, upstream, head)?;
    Ok((ahead, behind))
}

fn git_reachable_count(
    root: &Path,
    repo: &SleyRepository,
    tip: SleyObjectId,
    hidden: SleyObjectId,
) -> Result<usize> {
    let hidden = git_ancestor_set(root, repo, hidden)?;
    let mut seen = std::collections::HashSet::new();
    let mut pending = vec![tip];
    let mut count = 0;
    while let Some(oid) = pending.pop() {
        if hidden.contains(&oid) || !seen.insert(oid) {
            continue;
        }
        count += 1;
        let commit = repo.read_commit(&oid).map_err(|error| {
            HeddleError::Config(format!(
                "failed to inspect Git upstream drift at '{}': {error}",
                root.display()
            ))
        })?;
        pending.extend(commit.parents);
    }
    Ok(count)
}

fn git_ancestor_set(
    root: &Path,
    repo: &SleyRepository,
    start: SleyObjectId,
) -> Result<std::collections::HashSet<SleyObjectId>> {
    let mut seen = std::collections::HashSet::new();
    let mut pending = vec![start];
    while let Some(oid) = pending.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let commit = repo.read_commit(&oid).map_err(|error| {
            HeddleError::Config(format!(
                "failed to inspect Git upstream drift at '{}': {error}",
                root.display()
            ))
        })?;
        pending.extend(commit.parents);
    }
    Ok(seen)
}

fn git_remote_tracking_display_name(name: &str) -> String {
    name.strip_prefix("refs/remotes/")
        .unwrap_or(name)
        .to_string()
}

fn git_remote_tracking_message(
    branch: &str,
    upstream: &str,
    ahead: usize,
    behind: usize,
    upstream_is_undone_checkpoint: bool,
) -> String {
    if upstream_is_undone_checkpoint && ahead == 0 && behind > 0 {
        return format!(
            "Upstream '{upstream}' still points at a Git commit that was undone locally on branch '{branch}'"
        );
    }
    match (ahead, behind) {
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
    }
}

fn git_remote_tracking_next_action(
    ahead: usize,
    behind: usize,
    upstream_is_undone_checkpoint: bool,
) -> String {
    if upstream_is_undone_checkpoint && ahead == 0 && behind > 0 {
        return "heddle push --force".to_string();
    }
    match (ahead, behind) {
        (0, _) => "heddle pull".to_string(),
        (_, 0) => "heddle push".to_string(),
        _ => "heddle pull".to_string(),
    }
}

fn repository_capability_for_root(root: &Path) -> RepositoryCapability {
    if has_git_metadata(root) {
        RepositoryCapability::GitOverlay
    } else {
        RepositoryCapability::NativeHeddle
    }
}

fn append_ignore_file_patterns(patterns: &mut Vec<String>, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
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
    Ok(())
}

/// Read git's HEAD ref via `sley::Repository::discover` (~25ms — full repository
/// inspection). Used as a fallback when the fast path can't parse the
/// raw `.git/HEAD` file (e.g. detached HEAD, multi-worktree layouts).
fn detect_git_head_state_via_sley(path: &Path) -> Result<Option<GitHeadState>> {
    let repo = SleyRepository::discover(path).map_err(|error| {
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

    if let Some(name) = head.branch_name() {
        return Ok(Some(GitHeadState::Attached(name.to_string())));
    }
    if head.is_detached()
        && let Some(id) = head.oid
    {
        return Ok(Some(GitHeadState::Detached(id)));
    }
    Ok(None)
}

fn detect_git_head_state(path: &Path) -> Result<Option<GitHeadState>> {
    if let Some(head) = detect_git_head_fast(path) {
        return Ok(Some(head));
    }
    detect_git_head_state_via_sley(path)
}

/// Detect git's current HEAD branch.
///
/// The fast path reads `.git/HEAD` directly as text. `.git/HEAD` is a
/// tiny file (~30 bytes for `ref: refs/heads/<name>\n`) and a direct
/// read is ~50us vs. repository discovery's ~25ms full repository
/// inspection. Falls back to sley only for the cases the text parser
/// can't handle: detached HEAD, multi-worktree `gitdir:` indirections,
/// and any malformed file (where we'd rather surface the right error
/// than guess).
fn detect_git_head(path: &Path) -> Result<Option<Head>> {
    if let Some(GitHeadState::Attached(thread)) = detect_git_head_state(path)? {
        return Ok(Some(Head::Attached {
            thread: ThreadName::from(thread),
        }));
    }
    Ok(None)
}

/// Fast path for `.git/HEAD` parsing. Returns `Some(GitHeadState::Attached)`
/// when `.git/HEAD` is the simple `ref: refs/heads/<name>` form;
/// returns `None` for any case we don't trust ourselves to parse
/// correctly (detached HEAD raw OIDs, `gitdir:` worktree pointers,
/// missing files), letting the sley fallback handle it.
fn detect_git_head_fast(path: &Path) -> Option<GitHeadState> {
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
    Some(GitHeadState::Attached(name))
}

fn resolve_git_dir(path: &Path) -> Result<PathBuf> {
    let repo = SleyRepository::discover(path).map_err(|error| {
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
