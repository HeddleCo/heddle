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
#[cfg(feature = "git-overlay")]
#[path = "git_overlay_object_source.rs"]
mod git_overlay_object_source;
#[path = "repo_config.rs"]
pub(crate) mod repo_config;
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
#[path = "repository_recovery.rs"]
mod repository_recovery;
#[path = "repository_resolve.rs"]
mod repository_resolve;
#[path = "repository_signing.rs"]
mod repository_signing;
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use chrono::Utc;
pub use commit_graph::{CommitGraphIndex, find_merge_base};
#[cfg(feature = "async-source")]
pub use commit_graph::{find_merge_base_async, is_ancestor_async};
pub use context_suggestions::{
    ContextSuggestion, ContextSuggestionTier, HIGH_SUGGESTION_THRESHOLD,
    MAJOR_REWRITE_THRESHOLD_PCT, MEDIUM_SUGGESTION_THRESHOLD, SUGGESTION_WINDOW,
    compute_rewrite_pct, is_major_rewrite,
};
pub use objects::object::DiffKind;
#[cfg(feature = "git-overlay")]
use objects::object::MarkerName;
use objects::{
    Progress,
    error::{HeddleError, Result},
    fs_atomic::write_file_atomic,
    lock::{RepoLock, RepositoryLockExt},
    object::{Attribution, ContentHash, Principal, State, StateId, ThreadName, Tree},
    store::{AnyStore, FsStore, ObjectStore, ShallowInfo},
    sync::RwLockExt,
    worktree::WorktreeStatus,
};
use oplog::{ConditionalCommitOutcome, IsolationPrecondition, OpLog, OpLogBackend, OpRecord};
use refs::{Head, RefBackend, RefExpectation, RefManager, RefUpdate};
pub use refs::{RefSummaryIndexInspection, SpoolFacet};
pub use repo_config::{
    HostedConfig, OutputFormat, RedactConfig, RepoConfig, RepositorySourceAuthority, TrustedKey,
};
// Review-epic config types — re-exported here so the new
// `repository_signals.rs` (and external crates wanting to construct a
// custom signals config) don't need to reach into a private module path.
#[allow(unused_imports)]
pub use repo_config::{
    PatternDeviationToml, ReviewConfig, ReviewSignalsToml, SelfFlaggedToml, SignalEnableToml,
    SignalModuleToml, TestReachabilityToml,
};
#[cfg(feature = "async-source")]
pub use repository_history::query_history_async;
pub use repository_history::{
    ChangedPathFilter, ChangedPathFilters, HistoryQuery, query_history_from_source,
};
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
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sley::{
    ObjectId as SleyObjectId, Reference as SleyReference, ReferenceTarget as SleyRefTarget,
    Repository as SleyRepository,
};
#[cfg(feature = "git-overlay")]
use sley::{
    ShortStatusOptions as SleyShortStatusOptions, StatusUntrackedMode as SleyStatusUntrackedMode,
    StreamControl as SleyStreamControl,
};

use crate::{GitRefContentNamespace, GitRefName};
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
pub(crate) mod repository_worktree_apply;
#[path = "repository_worktree_status.rs"]
mod repository_worktree_status;
#[path = "status_tracked_refresh.rs"]
mod status_tracked_refresh;
#[path = "status_untracked_scan.rs"]
mod status_untracked_scan;

const GIT_CHECKPOINTS_FILE: &str = "git-checkpoints.json";
const GIT_CHECKPOINT_INTENT_FILE: &str = "git-checkpoint-intent.json";
const GIT_OVERLAY_LOCAL_EXCLUDE_PATTERNS: &[&str] = &[".heddle/"];

#[derive(Debug)]
pub struct GitOverlayShortStatus {
    pub worktree: WorktreeStatus,
    pub index_staged_paths: Vec<String>,
    pub index_extra_paths: Vec<String>,
    pub index_plan_applicable: bool,
}

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
    pub state_id: String,
    pub git_commit: String,
    pub summary: String,
    pub committed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitCheckpointIntentPhase {
    Prepared,
    Published,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitCheckpointIntent {
    pub version: u32,
    pub state_id: String,
    pub branch: String,
    pub previous_git_oid: Option<String>,
    pub new_git_oid: String,
    pub summary: String,
    pub phase: GitCheckpointIntentPhase,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitImportGuidance {
    pub current_branch: String,
    pub missing_branch_count: usize,
    pub missing_branches: Vec<String>,
    pub recommended_command: String,
}

#[cfg(feature = "git-overlay")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitOverlayBranchTip {
    pub branch: String,
    pub git_commit: String,
    pub history_imported: bool,
    #[serde(skip)]
    pub mapped_state: Option<StateId>,
}

#[cfg(feature = "git-overlay")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitOverlayTagTip {
    pub tag: String,
    pub git_commit: String,
    pub history_imported: bool,
    #[serde(skip)]
    pub mapped_state: Option<StateId>,
}

/// How many Git commits reachable from a branch tip have no Heddle mapping
/// (neither imported/projection-mapped nor checkpointed). Used to report
/// how far a Git branch moved out-of-band before `heddle import git --ref`
/// reconciles it.
#[cfg(feature = "git-overlay")]
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
#[cfg(feature = "git-overlay")]
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
struct GitProjectionMappingEntry {
    state_id: String,
    git_oid: String,
}

#[derive(Debug, Deserialize, Default)]
struct GitProjectionMappingFile {
    entries: Vec<GitProjectionMappingEntry>,
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
/// - Hosted clones: `heddle_client::hosted::LazyHostedHydrator`
///   bridges sync `hydrate` calls to an async hosted call via a dedicated worker
///   thread + private Tokio runtime; on each call the worker invokes
///   `HostedClient::hydrate_pulled_state` for the current local-thread
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
/// wraps the local [`FsStore`] in a concrete enum variant rather than a
/// `Box<dyn>`, so every object access is static-dispatched through the enum
/// to the inner store — no vtable (heddle#283). `S` goes last so existing
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
    /// Live progress handle driven by long-running operations (tree
    /// materialization, and future streaming seams). Defaults to
    /// [`Progress::null`] — a no-op that costs one relaxed atomic add per
    /// update — so the common "no one is watching" path (piped output,
    /// `--output json`, library use) pays nothing. A CLI command installs a
    /// real, TTY-rendering handle via [`Repository::set_progress`] before
    /// driving an operation. Set-after-construction like `blob_hydrator`.
    progress: RwLock<Progress>,
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
    /// the on-disk shallow metadata. Prefer [`Repository::init`] or
    /// [`Repository::open`] unless a cross-crate integration genuinely needs to
    /// assemble the pieces manually.
    pub fn from_parts(
        root: PathBuf,
        heddle_dir: PathBuf,
        store: S,
        refs: R,
        oplog: O,
        config: RepoConfig,
        shallow: ShallowInfo,
    ) -> Self {
        let capability = repository_capability_for_authority(config.repository.source_authority);
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
            progress: RwLock::new(Progress::null()),
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
    if found < supported {
        return Err(HeddleError::RepositoryFormatMigrationRequired {
            path: config_path.to_path_buf(),
            found,
            required: supported,
        });
    }
    Ok(())
}

fn validate_snapshot_artifact_records(
    artifact: &objects::store::SnapshotCommitArtifact,
    records: &[OpRecord],
) -> Result<()> {
    let expected_op_count = records.len().saturating_sub(1) as u32;
    match records.last() {
        Some(OpRecord::TransactionCommit {
            transaction_id,
            op_count,
        }) if transaction_id == &artifact.transaction_id && *op_count == expected_op_count => {}
        _ => {
            return Err(HeddleError::InvalidObject(
                "snapshot artifact has an invalid transaction marker".to_string(),
            ));
        }
    }
    let snapshots = records
        .iter()
        .filter_map(|record| match record {
            OpRecord::Snapshot { new_state, .. } => Some(*new_state),
            _ => None,
        })
        .collect::<Vec<_>>();
    if snapshots.as_slice() != [artifact.state] {
        return Err(HeddleError::InvalidObject(
            "snapshot artifact records do not identify exactly its embedded state".to_string(),
        ));
    }
    if records[..records.len().saturating_sub(1)]
        .iter()
        .any(|record| matches!(record, OpRecord::TransactionCommit { .. }))
    {
        return Err(HeddleError::InvalidObject(
            "snapshot artifact contains an interior transaction marker".to_string(),
        ));
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
        oplog.validate_current_format()?;
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
}

impl Repository {
    /// Run the local-only hooks that follow a config-driven [`Repository::open`]:
    /// declarative migrations + lazy-clone hydrator reconstruction. Both are
    /// bound to the default `AnyStore` flavor (`apply_pending` and
    /// `BlobHydrator` operate on the bare `Repository`), so they live here
    /// rather than in the generic `open_raw`.
    fn run_open_hooks(&self) -> Result<()> {
        self.recover_snapshot_artifact_views()?;

        // Hot-path skip: when the schema ledger already records every
        // registered migration *and* there is no lazy-hydrator metadata,
        // both probes below are pure no-ops. Avoid the ledger parse /
        // hydrator path.exists work on every warm open.
        // See docs/perf/cli-core-loop-todo.md ("Reduce repo-open work by
        // skipping migration/hydrator probes when a repo has a clean
        // schema ledger and no lazy-hydrator file").
        let hydrator_path = crate::lazy_hydrator::LazyHydratorConfig::path_in(self.heddle_dir());
        let schema_clean = crate::migration::is_schema_ledger_complete(self.heddle_dir());
        let no_lazy_hydrator = !hydrator_path.exists();
        if schema_clean && no_lazy_hydrator {
            return Ok(());
        }

        // Run any pending declarative migrations. Idempotent:
        // re-opening a repo a second time is a no-op for the migration pass.
        // Hard schema migrations are part of the open contract: if they cannot
        // complete, continuing with a partially-upgraded repo would make later
        // strict readers fail at arbitrary call sites.
        //
        // Only skip the call when the ledger is complete: a missing or
        // incomplete ledger still needs `apply_pending` (which also reports
        // malformed ledger files).
        if !schema_clean {
            crate::migration::apply_pending(self)?;
        }
        // Reconstruct any persisted lazy-clone blob hydrator. When
        // `.heddle/lazy-hydrator.toml` exists, look up the registered
        // factory for its `kind` and install the hydrator on the
        // freshly-opened repo so a subsequent `require_blob` against a
        // missing-blob marker can fetch transparently — without this
        // reconstruction, lazy clones would only work inside the single
        // `cmd_clone` process. See `lazy_hydrator.rs` for the shape.
        if !no_lazy_hydrator {
            match crate::lazy_hydrator::try_reconstruct(self.root(), self.heddle_dir()) {
                Ok(Some(hydrator)) => self.set_blob_hydrator(hydrator),
                Ok(None) => {}
                Err(err) => {
                    // Hydrator construction failed (factory error or
                    // malformed metadata). Surface as a warning rather
                    // than blocking `open` — eager `heddle verify` calls
                    // shouldn't fail just because a stale hosted
                    // endpoint is unreachable; the user will get the real
                    // error on the first `require_blob` that needs it.
                    tracing::warn!("lazy hydrator reconstruction failed during open: {err}");
                }
            }
        }
        Ok(())
    }

    /// Replay authoritative structured-snapshot artifacts whose oplog/ref
    /// materialized views were lost before they reached stable storage.
    fn recover_snapshot_artifact_views(&self) -> Result<()> {
        let mut pending = self.store.snapshot_commit_descriptors()?;
        pending.sort_by(|left, right| {
            left.artifact
                .base_oplog_head_id
                .cmp(&right.artifact.base_oplog_head_id)
                .then_with(|| left.pack_name.cmp(&right.pack_name))
        });

        while !pending.is_empty() {
            let mut progressed = false;
            let mut remaining = Vec::new();
            for descriptor in pending {
                let artifact = &descriptor.artifact;
                if !self
                    .oplog
                    .committed_batch_records(&artifact.transaction_id)?
                    .is_empty()
                {
                    progressed = true;
                    continue;
                }

                let current_head = self.oplog.head_id()?;
                if artifact.base_oplog_head_id > current_head {
                    remaining.push(descriptor);
                    continue;
                }
                if artifact.base_oplog_head_id < current_head {
                    return Err(HeddleError::InvalidObject(format!(
                        "snapshot artifact {} starts at oplog head {}, behind current head {}",
                        descriptor.pack_name, artifact.base_oplog_head_id, current_head
                    )));
                }

                let records = artifact
                    .encoded_records
                    .iter()
                    .map(|bytes| rmp_serde::from_slice::<OpRecord>(bytes))
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                validate_snapshot_artifact_records(artifact, &records)?;
                let outcome = self.oplog.record_batch_exactly_once_if_unchanged(
                    records,
                    Some(&artifact.scope),
                    &artifact.transaction_id,
                    &IsolationPrecondition {
                        since_head_id: current_head,
                        keys: BTreeSet::new(),
                    },
                )?;
                match outcome {
                    ConditionalCommitOutcome::Committed(_)
                    | ConditionalCommitOutcome::AlreadyCommitted(_) => progressed = true,
                    ConditionalCommitOutcome::IsolationConflict { .. } => {
                        unreachable!("empty recovery isolation set cannot produce a conflict")
                    }
                }
            }
            if !progressed {
                let next = remaining
                    .first()
                    .map(|descriptor| descriptor.artifact.base_oplog_head_id)
                    .unwrap_or_default();
                return Err(HeddleError::InvalidObject(format!(
                    "snapshot artifact chain has a gap before oplog head {next}"
                )));
            }
            pending = remaining;
        }
        Ok(())
    }

    /// Build an object store from the repository configuration.
    ///
    /// Returns the local [`FsStore`] wrapped in the [`AnyStore`] enum so object
    /// access stays statically dispatched.
    fn build_store(
        config: &RepoConfig,
        root: &Path,
        heddle_dir: &Path,
        shared_overlay_source_root: Option<&Path>,
    ) -> Result<AnyStore> {
        let store = AnyStore::Fs(FsStore::new(heddle_dir));
        #[cfg(feature = "git-overlay")]
        let mut store = store;
        #[cfg(not(feature = "git-overlay"))]
        let _ = (config, root, shared_overlay_source_root);
        #[cfg(feature = "git-overlay")]
        let overlay_source_root = shared_overlay_source_root
            .map(Path::to_path_buf)
            .or_else(|| {
                (config.repository.source_authority == RepositorySourceAuthority::GitOverlay)
                    .then(|| root.to_path_buf())
            });
        #[cfg(feature = "git-overlay")]
        if let Some(source_root) = overlay_source_root {
            store.set_external_source(Arc::new(
                git_overlay_object_source::GitOverlayObjectSource::new(
                    source_root,
                    heddle_dir.to_path_buf(),
                ),
            ));
        }
        Ok(store)
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
        Self::init_with_source_authority(path, RepositorySourceAuthority::Native)
    }

    fn init_with_source_authority(
        path: impl AsRef<Path>,
        source_authority: RepositorySourceAuthority,
    ) -> Result<Self> {
        let root = path.as_ref().to_path_buf();
        let heddle_dir = root.join(".heddle");

        if heddle_dir.exists() {
            return Err(HeddleError::RepositoryExists(root));
        }

        // Owner-only `.heddle` tree: holds keys, credentials, and object store.
        objects::fs_atomic::create_private_dir_all(&heddle_dir)?;

        let store = FsStore::new(&heddle_dir);
        #[cfg(feature = "git-overlay")]
        let mut store = store;
        store.init()?;

        let refs = RefManager::new(&heddle_dir);
        refs.init()?;

        // `init` creates a fresh repo before any principal is configured;
        // the actor is set when the repo is later opened (which reads
        // `RepoConfig.principal`). Use the unattributed default for
        // entries written between init and first open.
        let oplog = OpLog::new_unattributed(&heddle_dir);
        oplog.init()?;

        let mut config = RepoConfig::default();
        config.repository.source_authority = source_authority;
        config.save(&heddle_dir.join("config.toml"))?;

        #[cfg(feature = "git-overlay")]
        if source_authority == RepositorySourceAuthority::GitOverlay {
            store.set_external_source(Arc::new(
                git_overlay_object_source::GitOverlayObjectSource::new(
                    root.clone(),
                    heddle_dir.clone(),
                ),
            ));
        }

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

        let repo = Self {
            root,
            heddle_dir: heddle_dir.clone(),
            capability: repository_capability_for_authority(source_authority),
            store: AnyStore::Fs(store),
            refs,
            oplog,
            config,
            shallow: RwLock::new(ShallowInfo::load(&heddle_dir)?),
            blob_hydrator: RwLock::new(None),
            git_overlay_repo: RwLock::new(None),
            progress: RwLock::new(Progress::null()),
        };

        // A freshly initialized repository is already in the current format.
        // Record that fact during the mutating init operation so the first
        // observe-only command does not have to create the migration ledger.
        crate::migration::apply_pending(&repo)?;
        Ok(repo)
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
    /// commands like `heddle verify` can immediately reflect the user's
    /// current branch and dirty worktree.
    pub fn bootstrap_git_overlay(path: impl AsRef<Path>) -> Result<Self> {
        let root = path.as_ref();
        if root.join(".heddle").exists() {
            let repo = Self::open(root)?;
            if repo.capability() == RepositoryCapability::GitOverlay {
                ensure_git_overlay_exclude(root)?;
            }
            return Ok(repo);
        }

        let repo = Self::init_git_overlay_sidecar(root)?;
        ensure_git_overlay_exclude(root)?;
        Ok(repo)
    }

    pub fn init_git_overlay_sidecar(path: impl AsRef<Path>) -> Result<Self> {
        let root = path.as_ref();
        let repo = Self::init_with_source_authority(root, RepositorySourceAuthority::GitOverlay)?;
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
    /// - Worktree: `.heddle/objectstore` (shared store path and checkout
    ///   authority), `.heddle/HEAD` (per-checkout), `.heddle/state/`
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
                    let pointer = parse_objectstore_pointer(&content).ok_or_else(|| {
                        HeddleError::Config(format!(
                            "invalid .heddle/objectstore pointer at {}: expected objectstore and source-authority entries",
                            pointer_path.display()
                        ))
                    })?;
                    let raw_shared = pointer.objectstore;

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
                    let mut config = RepoConfig::load_for_repository(&config_path)?;
                    ensure_supported_repo_format(&config_path, &config)?;
                    let shared_overlay_source_root = (config.repository.source_authority
                        == RepositorySourceAuthority::GitOverlay)
                        .then(|| shared_galeed_dir.parent().map(Path::to_path_buf))
                        .flatten();
                    config.repository.source_authority = pointer.source_authority;
                    let store = Self::build_store(
                        &config,
                        dir,
                        &shared_galeed_dir,
                        shared_overlay_source_root.as_deref(),
                    )?;
                    let local_head_path = heddle_path.join("HEAD");
                    let refs = RefManager::new(&shared_galeed_dir).with_local_head(local_head_path);
                    let repo =
                        Self::open_raw(dir.to_path_buf(), shared_galeed_dir, store, config, refs)?;
                    repo.run_open_hooks()?;
                    return Ok(repo);
                }

                if objects_dir.is_dir() {
                    // Main repo mode.
                    let config_path = heddle_path.join("config.toml");
                    let config = RepoConfig::load_for_repository(&config_path)?;
                    ensure_supported_repo_format(&config_path, &config)?;
                    let store = Self::build_store(&config, dir, &heddle_path, None)?;
                    let refs = RefManager::new(&heddle_path);
                    let repo = Self::open_raw(dir.to_path_buf(), heddle_path, store, config, refs)?;
                    repo.run_open_hooks()?;
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
                                    repo.git_overlay_mapped_state_for_git_oid(git_oid)
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

        // Mutating commands historically rely on open() bootstrapping a plain
        // Git tree into a Git-overlay sidecar (import/thread/start/marker…).
        // Observe-only commands (status/verify/doctor) must NOT call open on
        // plain Git — they take the plain-Git probe path so they never create
        // `.heddle`. See `verify_execution_context_from_cli`.
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

    pub fn source_authority(&self) -> RepositorySourceAuthority {
        match self.capability {
            RepositoryCapability::GitOverlay => RepositorySourceAuthority::GitOverlay,
            RepositoryCapability::NativeHeddle => RepositorySourceAuthority::Native,
        }
    }

    pub fn transition_source_authority(
        &self,
        expected: RepositorySourceAuthority,
        next: RepositorySourceAuthority,
    ) -> Result<()> {
        let _write_lock = self.locker().write().map_err(|error| {
            HeddleError::Config(format!(
                "failed to lock repository for source-authority transition: {error}"
            ))
        })?;
        let config_path = self.heddle_dir.join("config.toml");
        let mut config = RepoConfig::load_for_repository(&config_path)?;
        if config.repository.source_authority != expected {
            return Err(HeddleError::Config(format!(
                "repository source authority changed before transition: expected {expected:?}, found {:?}",
                config.repository.source_authority
            )));
        }
        config.repository.source_authority = next;
        config.save(&config_path)
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
        if self.capability() == RepositoryCapability::GitOverlay && has_git_metadata(&self.root) {
            if self.git_overlay_head_is_detached()?
                && detect_git_in_progress_branch(&self.root)?.is_none()
            {
                return Ok(None);
            }

            if self.current_state()?.is_none() {
                return self.git_overlay_current_branch();
            }
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

        let local_ref_name = GitRefName::branch_full_name(&branch);
        if git_find_reference(&git, &local_ref_name)?.is_some()
            && let Some(tracking_name) = git_configured_tracking_ref(&git, &branch)?
            && let Some(upstream_head) = git_resolve_oid(&git, &tracking_name)?
        {
            let (ahead, behind) = git_ahead_behind_counts(&git, head, upstream_head)?;
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
            let remote_ref = GitRefName::remote_branch_full_name(remote, &branch);
            if let Some(remote_head) = git_resolve_oid(&git, &remote_ref)? {
                if remote_head == head {
                    return Ok(None);
                }
                let (ahead, behind) = git_ahead_behind_counts(&git, head, remote_head)?;
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
        let batches = match self.oplog().redo_batches_scoped(64, Some(&scope)) {
            Ok(batches) => batches,
            Err(error) => {
                tracing::warn!(
                    branch,
                    local_oid,
                    upstream_oid,
                    error = %error,
                    "could not inspect redo oplog for undone Git checkpoint status"
                );
                return Ok(false);
            }
        };
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

    pub fn git_import_guidance(&self) -> Result<Option<GitImportGuidance>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }
        // Git-overlay treats Git refs and commits as Git-owned storage that
        // Heddle reads directly. Missing Git->Heddle state mappings are not an
        // everyday "needs adopt" condition; `adopt` is reserved for explicit
        // transition to native source authority.
        Ok(None)
    }

    /// Enumerate Git branch tips with Heddle mapping status.
    ///
    /// Gated behind `git-overlay`: native-only builds do not expose overlay
    /// branch enumeration on `Repository`.
    #[cfg(feature = "git-overlay")]
    pub fn git_overlay_branch_tips(&self) -> Result<Vec<GitOverlayBranchTip>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(Vec::new());
        }

        let Some(git_repo) = self.git_overlay_sley_repository()? else {
            return Ok(Vec::new());
        };

        let imported_threads: std::collections::HashSet<ThreadName> =
            self.refs().list_threads()?.into_iter().collect();
        let projection_mapping = self.git_projection_mapping()?;
        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        let mut branch_tips = Vec::new();

        for branch in git_repo.references().list_refs().map_err(|error| {
            HeddleError::Config(format!(
                "failed to enumerate git branches at '{}': {}",
                self.root.display(),
                error
            ))
        })? {
            let ref_name = GitRefName::new(&branch.name);
            if ref_name.content_namespace() != Some(GitRefContentNamespace::Branch) {
                continue;
            };
            let Some(name) = ref_name.short_name().map(str::to_string) else {
                continue;
            };
            let Some(target) =
                self.git_overlay_commit_tip_oid(&git_repo, &branch, "branch", &name)?
            else {
                continue;
            };
            let git_commit = target.to_string();
            let mapped_state = self.git_overlay_mapped_state_for_commit(
                &git_commit,
                &projection_mapping,
                &ingest_mapping,
                &checkpoint_mapping,
            )?;
            let thread_name = ThreadName::from(name.as_str());
            let history_imported = if imported_threads.contains(&thread_name) {
                // Read the thread ref once; the mapped + checkpointed
                // checks each used to re-read it, which doubled the
                // ref-store hits per branch on a 60+ branch repo.
                let existing_thread = self.refs().get_thread(&thread_name)?;
                let mapped = matches!(
                    (existing_thread.as_ref(), mapped_state.as_ref()),
                    (Some(existing), Some(mapped_state))
                        if existing == mapped_state
                );
                let checkpointed = if mapped {
                    false
                } else if let Some(existing) = existing_thread {
                    self.latest_git_checkpoint_for_state(&existing)?
                        .is_some_and(|record| record.git_commit == git_commit)
                        || mapped_state.as_ref().is_some_and(|mapped_state| {
                            self.state_is_ancestor(mapped_state, &existing)
                        })
                } else {
                    false
                };
                mapped || checkpointed
            } else {
                mapped_state.is_some()
            };
            branch_tips.push(GitOverlayBranchTip {
                branch: name,
                git_commit,
                history_imported,
                mapped_state,
            });
        }
        branch_tips.sort_by(|a, b| a.branch.cmp(&b.branch));
        Ok(branch_tips)
    }

    #[cfg(feature = "git-overlay")]
    pub fn git_overlay_tag_tips(&self) -> Result<Vec<GitOverlayTagTip>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(Vec::new());
        }

        let Some(git_repo) = self.git_overlay_sley_repository()? else {
            return Ok(Vec::new());
        };

        let imported_markers: std::collections::HashSet<MarkerName> =
            self.refs().list_markers()?.into_iter().collect();
        let projection_mapping = self.git_projection_mapping()?;
        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        let mut tag_tips = Vec::new();

        for tag in git_repo.references().list_refs().map_err(|error| {
            HeddleError::Config(format!(
                "failed to enumerate git tags at '{}': {}",
                self.root.display(),
                error
            ))
        })? {
            let ref_name = GitRefName::new(&tag.name);
            if ref_name.content_namespace() != Some(GitRefContentNamespace::Tag) {
                continue;
            };
            let Some(name) = ref_name.short_name().map(str::to_string) else {
                continue;
            };
            let Some(target) = self.git_overlay_commit_tip_oid(&git_repo, &tag, "tag", &name)?
            else {
                continue;
            };
            let git_commit = target.to_string();
            let mapped_state = self.git_overlay_mapped_state_for_commit(
                &git_commit,
                &projection_mapping,
                &ingest_mapping,
                &checkpoint_mapping,
            )?;
            let marker_name = MarkerName::from(name.as_str());
            let history_imported = if imported_markers.contains(&marker_name) {
                matches!(
                    (self.refs().get_marker(&marker_name)?, mapped_state.as_ref()),
                    (Some(existing), Some(mapped_state)) if existing == *mapped_state
                )
            } else {
                false
            };
            tag_tips.push(GitOverlayTagTip {
                tag: name,
                git_commit,
                history_imported,
                mapped_state,
            });
        }

        tag_tips.sort_by(|a, b| a.tag.cmp(&b.tag));
        Ok(tag_tips)
    }

    #[cfg(feature = "git-overlay")]
    pub fn git_overlay_branch_tip(&self, name: &str) -> Result<Option<GitOverlayBranchTip>> {
        Ok(self
            .git_overlay_branch_tips()?
            .into_iter()
            .find(|tip| tip.branch == name))
    }

    #[cfg(feature = "git-overlay")]
    pub fn git_overlay_tag_tip(&self, name: &str) -> Result<Option<GitOverlayTagTip>> {
        Ok(self
            .git_overlay_tag_tips()?
            .into_iter()
            .find(|tip| tip.tag == name))
    }

    /// Map a Git branch name to a Heddle state id when known.
    ///
    /// Kept available without `git-overlay` feature so open/HEAD reconciliation
    /// can compile under native-only builds (it no-ops when capability is not
    /// Git Overlay). Tip enumeration (`git_overlay_branch_tips`) remains gated.
    pub fn git_overlay_mapped_state_for_branch(&self, name: &str) -> Result<Option<StateId>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }
        let Some(git_repo) = self.git_overlay_sley_repository()? else {
            return Ok(None);
        };
        let full_name = format!("refs/heads/{name}");
        let projection_mapping = self.git_projection_mapping()?;
        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        for reference in git_repo.references().list_refs().map_err(|error| {
            HeddleError::Config(format!(
                "failed to enumerate git branches at '{}': {}",
                self.root.display(),
                error
            ))
        })? {
            if reference.name != full_name {
                continue;
            }
            let Some(target) =
                self.git_overlay_commit_tip_oid(&git_repo, &reference, "branch", name)?
            else {
                return Ok(None);
            };
            return self.git_overlay_mapped_state_for_commit(
                &target.to_string(),
                &projection_mapping,
                &ingest_mapping,
                &checkpoint_mapping,
            );
        }
        Ok(None)
    }

    #[cfg(feature = "git-overlay")]
    pub fn git_overlay_mapped_state_for_remote_tracking_ref(
        &self,
        name: &str,
    ) -> Result<Option<StateId>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }
        let Some(git_repo) = self.git_overlay_sley_repository()? else {
            return Ok(None);
        };
        let full_name = GitRefName::remote_tracking_full_name(name);
        let projection_mapping = self.git_projection_mapping()?;
        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        for reference in git_repo.references().list_refs().map_err(|error| {
            HeddleError::Config(format!(
                "failed to enumerate git remote-tracking refs at '{}': {}",
                self.root.display(),
                error
            ))
        })? {
            if reference.name != full_name {
                continue;
            }
            let Some(target) =
                self.git_overlay_commit_tip_oid(&git_repo, &reference, "remote branch", name)?
            else {
                return Ok(None);
            };
            return self.git_overlay_mapped_state_for_commit(
                &target.to_string(),
                &projection_mapping,
                &ingest_mapping,
                &checkpoint_mapping,
            );
        }
        Ok(None)
    }

    pub fn git_overlay_mapped_state_for_tag(&self, name: &str) -> Result<Option<StateId>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }
        let Some(git_repo) = self.git_overlay_sley_repository()? else {
            return Ok(None);
        };
        let full_name = format!("refs/tags/{name}");
        let projection_mapping = self.git_projection_mapping()?;
        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        for reference in git_repo.references().list_refs().map_err(|error| {
            HeddleError::Config(format!(
                "failed to enumerate git tags at '{}': {}",
                self.root.display(),
                error
            ))
        })? {
            if reference.name != full_name {
                continue;
            }
            let Some(target) =
                self.git_overlay_commit_tip_oid(&git_repo, &reference, "tag", name)?
            else {
                return Ok(None);
            };
            return self.git_overlay_mapped_state_for_commit(
                &target.to_string(),
                &projection_mapping,
                &ingest_mapping,
                &checkpoint_mapping,
            );
        }
        Ok(None)
    }

    #[cfg(feature = "git-overlay")]
    fn state_is_ancestor(&self, ancestor: &StateId, descendant: &StateId) -> bool {
        let mut graph = CommitGraphIndex::new(self);
        graph.is_ancestor(ancestor, descendant).unwrap_or(false)
    }

    /// Git-overlay worktree status, compared against the **Git index** (distinct
    /// from `compare_worktree_cached*`, which compares against heddle's own index).
    ///
    /// The expensive part — deciding whether each tracked file changed since it
    /// was staged — is handled by sley's `stream_short_status_with_options`, which
    /// honors git's racy-clean stat cache: when a file's mode + size + mtime match
    /// its Git index entry (and the entry is not racily clean), sley reuses the
    /// staged OID and SKIPS re-reading + SHA-1ing the file (`reuse_tracked_entry`),
    /// falling back to a full content hash whenever the stat is ambiguous. On a
    /// warm worktree this turns the walk from "hash every file" into "stat every
    /// file" (~0.35s vs minutes on the ~6k-file ghostty tree). This stat-cache
    /// MUST be preserved across sley bumps — a sley that re-hashes unconditionally
    /// would silently reintroduce the pathological checkpoint cost.
    #[cfg(feature = "git-overlay")]
    pub fn git_overlay_worktree_status(&self) -> Result<Option<WorktreeStatus>> {
        Ok(self
            .git_overlay_short_status()?
            .map(|status| status.worktree))
    }

    /// Build worktree status and Git-index intent from one Sley status stream.
    #[cfg(feature = "git-overlay")]
    pub fn git_overlay_short_status(&self) -> Result<Option<GitOverlayShortStatus>> {
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

        let mut added = BTreeSet::new();
        let mut modified = BTreeSet::new();
        let mut deleted = BTreeSet::new();
        let ignore_patterns = self.ignore_patterns()?;
        let worktree_ignore = crate::worktree_ignore::WorktreeIgnoreMatcher::new(&ignore_patterns);
        let index_ignore = objects::worktree::build_worktree_ignore(&ignore_patterns);
        let index_plan_applicable = git_worktree_matches_repo_root(&git_repo, self.root());
        let mut index_staged_paths = Vec::new();
        let mut index_extra_paths = Vec::new();

        git_repo
            .stream_short_status_with_options(
                SleyShortStatusOptions {
                    untracked_mode: SleyStatusUntrackedMode::All,
                    ..SleyShortStatusOptions::default()
                },
                |entry| {
                    let path = git_path(entry.path);
                    if path.is_empty() {
                        return Ok(SleyStreamControl::Continue);
                    }
                    if index_plan_applicable {
                        append_short_status_to_index_intent(
                            &mut index_staged_paths,
                            &mut index_extra_paths,
                            &index_ignore,
                            entry,
                            &path,
                        );
                    }
                    if ignored_git_overlay_status_path(&path) {
                        return Ok(SleyStreamControl::Continue);
                    }
                    let path = PathBuf::from(path);

                    if entry.index == b'?' && entry.worktree == b'?' {
                        if git_overlay_untracked_path_ignored(&worktree_ignore, &path) {
                            return Ok(SleyStreamControl::Continue);
                        }
                        added.insert(path);
                    } else if entry.index == b'D' || entry.worktree == b'D' {
                        deleted.insert(path);
                    } else if entry.index == b'A'
                        || entry.index == b'R'
                        || entry.index == b'C'
                        || entry.head_oid.is_none()
                    {
                        added.insert(path);
                    } else {
                        modified.insert(path);
                    }

                    Ok(SleyStreamControl::Continue)
                },
            )
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to inspect Git worktree status at '{}': {}",
                    self.root.display(),
                    error
                ))
            })?;

        Ok(Some(GitOverlayShortStatus {
            worktree: WorktreeStatus {
                modified: modified.into_iter().collect(),
                added: added.into_iter().collect(),
                deleted: deleted.into_iter().collect(),
            },
            index_staged_paths,
            index_extra_paths,
            index_plan_applicable,
        }))
    }

    /// Native-only builds have no Git status stream.
    #[cfg(not(feature = "git-overlay"))]
    pub fn git_overlay_short_status(&self) -> Result<Option<GitOverlayShortStatus>> {
        Ok(None)
    }

    fn git_projection_mapping(&self) -> Result<HashMap<String, String>> {
        let path = self
            .heddle_dir
            .join("git-projection")
            .join("git-projection-mapping.json");
        if !path.exists() {
            return Ok(HashMap::new());
        }

        let contents = fs::read_to_string(path)?;
        if contents.trim().is_empty() {
            return Ok(HashMap::new());
        }

        let file: GitProjectionMappingFile = serde_json::from_str(&contents)?;
        Ok(file
            .entries
            .into_iter()
            .map(|entry| (entry.git_oid, entry.state_id))
            .collect())
    }

    pub fn git_overlay_ingest_commit_mapping(&self) -> Result<HashMap<String, String>> {
        let path = self.heddle_dir.join("ingest").join("sha_map.sqlite");
        if !path.exists() {
            return Ok(HashMap::new());
        }

        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| {
            HeddleError::Config(format!(
                "failed to open ingest SHA map at '{}': {}",
                path.display(),
                error
            ))
        })?;
        let mut stmt = conn
            .prepare_cached("SELECT git_sha, heddle_repr FROM sha_map WHERE kind = 0")
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to read ingest SHA map at '{}': {}",
                    path.display(),
                    error
                ))
            })?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to enumerate ingest SHA map at '{}': {}",
                    path.display(),
                    error
                ))
            })?;

        let mut mapping = HashMap::new();
        for row in rows {
            let (git_sha, state_id) = row.map_err(|error| {
                HeddleError::Config(format!(
                    "failed to read ingest SHA map row at '{}': {}",
                    path.display(),
                    error
                ))
            })?;
            mapping.insert(git_sha, state_id);
        }
        Ok(mapping)
    }

    fn git_overlay_checkpoint_mapping(&self) -> Result<HashMap<String, String>> {
        Ok(self
            .list_git_checkpoints()?
            .into_iter()
            .map(|record| (record.git_commit, record.state_id))
            .collect())
    }

    fn git_overlay_mapped_state_for_commit(
        &self,
        git_commit: &str,
        projection_mapping: &HashMap<String, String>,
        ingest_mapping: &HashMap<String, String>,
        checkpoint_mapping: &HashMap<String, String>,
    ) -> Result<Option<StateId>> {
        let Some(change) = projection_mapping
            .get(git_commit)
            .or_else(|| ingest_mapping.get(git_commit))
            .or_else(|| checkpoint_mapping.get(git_commit))
        else {
            return Ok(None);
        };
        let state_id = StateId::parse(change).map_err(|error| {
            HeddleError::Config(format!(
                "git commit {git_commit} maps to invalid Heddle state id '{change}': {error}"
            ))
        })?;
        if self.store.get_state(&state_id)?.is_some() {
            Ok(Some(state_id))
        } else {
            Ok(None)
        }
    }

    fn git_overlay_mapped_git_commit_for_state_in(
        &self,
        state_id: &StateId,
        mapping: &HashMap<String, String>,
    ) -> Result<Option<String>> {
        for (git_commit, mapped_state) in mapping {
            let mapped_state_id = StateId::parse(mapped_state).map_err(|error| {
                HeddleError::Config(format!(
                    "git commit {git_commit} maps to invalid Heddle state id '{mapped_state}': {error}"
                ))
            })?;
            if mapped_state_id == *state_id {
                return Ok(Some(git_commit.clone()));
            }
        }
        Ok(None)
    }

    pub fn git_overlay_mapped_git_commit_for_state(
        &self,
        state_id: &StateId,
    ) -> Result<Option<String>> {
        let projection_mapping = self.git_projection_mapping()?;
        if let Some(git_commit) =
            self.git_overlay_mapped_git_commit_for_state_in(state_id, &projection_mapping)?
        {
            return Ok(Some(git_commit));
        }

        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
        if let Some(git_commit) =
            self.git_overlay_mapped_git_commit_for_state_in(state_id, &ingest_mapping)?
        {
            return Ok(Some(git_commit));
        }

        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        self.git_overlay_mapped_git_commit_for_state_in(state_id, &checkpoint_mapping)
    }

    pub fn git_overlay_mapped_state_for_git_commit(
        &self,
        git_commit: &str,
    ) -> Result<Option<StateId>> {
        let projection_mapping = self.git_projection_mapping()?;
        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        self.git_overlay_mapped_state_for_commit(
            git_commit,
            &projection_mapping,
            &ingest_mapping,
            &checkpoint_mapping,
        )
    }

    fn git_overlay_mapped_state_for_git_oid(
        &self,
        git_oid: SleyObjectId,
    ) -> Result<Option<StateId>> {
        self.git_overlay_mapped_state_for_git_commit(&git_oid.to_string())
    }

    /// Count the Git commits reachable from `tip_git_commit` that are not
    /// represented in Heddle state (no Git Projection Mapping, ingest identity
    /// mapping, or checkpoint mapping). The walk prunes at the first mapped
    /// commit on each lineage, so the cost is proportional to the out-of-band
    /// suffix, capped at `GIT_OVERLAY_OUT_OF_BAND_SCAN_LIMIT`.
    ///
    /// Returns `Ok(None)` when the repository is not a Git overlay or the tip
    /// cannot be resolved; callers should degrade to a countless report.
    #[cfg(feature = "git-overlay")]
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

        let projection_mapping = self.git_projection_mapping()?;
        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
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
                .git_overlay_mapped_state_for_commit(
                    &git_commit,
                    &projection_mapping,
                    &ingest_mapping,
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
        let raw_git_next_action = "heddle verify";
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

    pub fn latest_git_checkpoint_for_state(
        &self,
        state_id: &StateId,
    ) -> Result<Option<GitCheckpointRecord>> {
        let full_id = state_id.to_string_full();
        Ok(self
            .list_git_checkpoints()?
            .into_iter()
            .rev()
            .find(|record| record.state_id == full_id))
    }

    pub fn record_git_checkpoint(
        &self,
        state_id: &StateId,
        git_commit: impl Into<String>,
        summary: impl Into<String>,
    ) -> Result<GitCheckpointRecord> {
        let mut records = self.list_git_checkpoints()?;
        let git_commit = git_commit.into();
        if let Some(existing) = records.iter().rev().find(|record| {
            record.state_id == state_id.to_string_full() && record.git_commit == git_commit
        }) {
            return Ok(existing.clone());
        }
        let record = GitCheckpointRecord {
            state_id: state_id.to_string_full(),
            git_commit,
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

    fn git_checkpoint_intent_path(&self) -> PathBuf {
        self.root
            .join(".heddle/state")
            .join(GIT_CHECKPOINT_INTENT_FILE)
    }

    pub fn pending_git_checkpoint_intent(&self) -> Result<Option<GitCheckpointIntent>> {
        let path = self.git_checkpoint_intent_path();
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(serde_json::from_str(&contents)?))
    }

    pub fn begin_git_checkpoint_intent(
        &self,
        intent: &GitCheckpointIntent,
    ) -> Result<GitCheckpointIntent> {
        if intent.version != 1 || intent.phase != GitCheckpointIntentPhase::Prepared {
            return Err(HeddleError::InvalidObject(
                "new Git checkpoint intent must be prepared v1".to_string(),
            ));
        }
        if let Some(existing) = self.pending_git_checkpoint_intent()? {
            let same_operation = existing.version == intent.version
                && existing.state_id == intent.state_id
                && existing.branch == intent.branch
                && existing.previous_git_oid == intent.previous_git_oid
                && existing.new_git_oid == intent.new_git_oid
                && existing.summary == intent.summary;
            if same_operation {
                return Ok(existing);
            }
            return Err(HeddleError::Config(format!(
                "Git checkpoint {} -> {} is still pending on branch '{}'; retry that checkpoint before starting another",
                existing.previous_git_oid.as_deref().unwrap_or("<unborn>"),
                existing.new_git_oid,
                existing.branch
            )));
        }
        let path = self.git_checkpoint_intent_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_file_atomic(&path, serde_json::to_string_pretty(intent)?.as_bytes())?;
        Ok(intent.clone())
    }

    pub fn mark_git_checkpoint_published(
        &self,
        state_id: &StateId,
        git_oid: &str,
    ) -> Result<GitCheckpointIntent> {
        let mut intent = self.pending_git_checkpoint_intent()?.ok_or_else(|| {
            HeddleError::Config("Git checkpoint intent disappeared before publish".to_string())
        })?;
        if intent.state_id != state_id.to_string_full() || intent.new_git_oid != git_oid {
            return Err(HeddleError::Config(
                "Git checkpoint publish does not match the durable intent".to_string(),
            ));
        }
        intent.phase = GitCheckpointIntentPhase::Published;
        write_file_atomic(
            &self.git_checkpoint_intent_path(),
            serde_json::to_string_pretty(&intent)?.as_bytes(),
        )?;
        Ok(intent)
    }

    pub fn finish_git_checkpoint_intent(&self, state_id: &StateId, git_oid: &str) -> Result<()> {
        let Some(intent) = self.pending_git_checkpoint_intent()? else {
            return Ok(());
        };
        if intent.state_id != state_id.to_string_full() || intent.new_git_oid != git_oid {
            return Err(HeddleError::Config(
                "cannot finalize a Git checkpoint that does not match the durable intent"
                    .to_string(),
            ));
        }
        let path = self.git_checkpoint_intent_path();
        fs::remove_file(&path)?;
        if let Some(parent) = path.parent() {
            objects::fs_atomic::sync_directory(parent)?;
        }
        Ok(())
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
            format!(
                "objectstore: {}\nsource-authority: native\n",
                shared.display()
            )
            .as_bytes(),
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

    /// The oplog scope token for a named facet lineage.
    ///
    /// Generalizes [`op_scope`](Self::op_scope) to the open facet set (Spool
    /// epic P2). The default (content) facet returns the **unchanged** base
    /// scope, so existing content/Git/Heddle oplog batches, undo records, and
    /// `IsolationKey::LocalHead` are byte-identical to today. Every other facet
    /// (`governance`, `membership`, …) gets its own suffixed scope
    /// (`wt-<digest>/<facet>`), so that facet's batches, undo/redo view, and
    /// isolation key are fully independent of every other facet's.
    ///
    /// Thread this into `record_batch_scoped` / `recent_batches_scoped` /
    /// `undo_batches_scoped` to run the same `Repository` operations against a
    /// different facet lineage.
    pub fn op_scope_for_facet(&self, facet: &SpoolFacet) -> String {
        facet.scope_token(&self.op_scope())
    }

    /// Read a named facet's HEAD.
    ///
    /// A facet's HEAD is modeled with the existing [`Head`] enum unchanged: it
    /// attaches to the facet's canonical thread ref
    /// (`refs/spool/<facet>/threads/<main_thread>`) when that thread exists
    /// ([`Head::Attached`]), or resolves to a detached state otherwise. The
    /// **content** facet is the physical `.heddle/HEAD` (delegates to
    /// [`head_ref`](Self::head_ref)), preserving today's behavior exactly.
    ///
    /// This is the heddle-side per-`(repo, facet)` HEAD the Spool model needs;
    /// the weft `heads` PK change is a later weft phase.
    pub fn facet_head(&self, facet: &SpoolFacet, main_thread: &str) -> Result<Option<Head>> {
        if facet.is_default() {
            return Ok(Some(self.head_ref()?));
        }
        let thread = ThreadName::from(facet.thread_ref(main_thread).as_str());
        match self.refs.get_thread(&thread)? {
            Some(_) => Ok(Some(Head::Attached { thread })),
            None => Ok(None),
        }
    }

    /// Resolve a named facet's HEAD to a concrete state, if any.
    pub fn facet_head_state(
        &self,
        facet: &SpoolFacet,
        main_thread: &str,
    ) -> Result<Option<StateId>> {
        if facet.is_default() {
            return self.head();
        }
        let thread = ThreadName::from(facet.thread_ref(main_thread).as_str());
        self.refs.get_thread(&thread)
    }

    /// Advance a named facet's HEAD thread to `state`.
    ///
    /// Moves the facet's canonical thread ref
    /// (`refs/spool/<facet>/threads/<main_thread>`) under the facet's own ref
    /// prefix — it does **not** touch any other facet's refs. Rejected for the
    /// default (content) facet, whose HEAD is the physical `.heddle/HEAD` and is
    /// moved through the existing snapshot/goto write paths.
    pub fn set_facet_head(
        &self,
        facet: &SpoolFacet,
        main_thread: &str,
        state: &StateId,
    ) -> Result<()> {
        if facet.is_default() {
            return Err(HeddleError::InvalidObject(
                "set_facet_head is for named facets; the content facet HEAD moves via snapshot/goto"
                    .to_string(),
            ));
        }
        let thread = ThreadName::from(facet.thread_ref(main_thread).as_str());
        self.refs.set_thread(&thread, state)
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
        new_state: &StateId,
        prev_head: Option<StateId>,
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
        new_state: &StateId,
        prev_head: Option<StateId>,
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
        new_state: &StateId,
        prev_head: Option<StateId>,
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

    pub fn get_tree_for_state(&self, state_id: &StateId) -> Result<Option<Tree>> {
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
        // Root Git metadata is repository-engine state, never source content.
        patterns.push("/.git/".to_string());
        if self.capability() == RepositoryCapability::GitOverlay {
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

    pub fn head(&self) -> Result<Option<StateId>> {
        Ok(match self.head_ref()? {
            Head::Attached { thread } => match self.refs.get_thread(&thread)? {
                Some(state_id) => Some(state_id),
                None if self.capability() == RepositoryCapability::GitOverlay => {
                    self.git_overlay_mapped_state_for_branch(&thread)?
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
            && let Some(state) = self.git_overlay_mapped_state_for_git_oid(git_oid)?
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
            || self.git_overlay_mapped_state_for_branch(&branch)?.is_some()
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
    /// `heddle thread switch X && heddle sync --thread Y` lands the merge into
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

    pub fn is_shallow(&self, id: &StateId) -> bool {
        self.shallow.read_or_poisoned().is_shallow(id)
    }

    pub fn set_shallow(&self, state_id: &StateId, _parents: &[StateId]) -> Result<()> {
        self.shallow.write_or_poisoned().add_shallow(*state_id)?;
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
        self.refs.set_thread(&main_thread, &state.id())?;
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
    /// paths (`heddle verify`, `heddle ready`, `heddle stash show`),
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
        *self.blob_hydrator.write_or_poisoned() = Some(hydrator);
    }

    /// The currently registered hydrator, if any.
    pub fn blob_hydrator(&self) -> Option<Arc<dyn BlobHydrator>> {
        self.blob_hydrator.read_or_poisoned().clone()
    }

    /// Install a live [`Progress`] handle. Long-running operations on this
    /// repository (tree materialization today) drive it; the caller — the CLI —
    /// installs a TTY-rendering handle here before the operation and reads the
    /// same handle back to paint a completion line. Passing [`Progress::null`]
    /// (the default) disables rendering. The handle is a cheap `Arc` clone, so
    /// it can be shared across the parallel-materialization worker threads.
    pub fn set_progress(&self, progress: Progress) {
        *self.progress.write_or_poisoned() = progress;
    }

    /// The currently installed progress handle (a cheap clone). Defaults to
    /// [`Progress::null`] until [`Repository::set_progress`] is called.
    pub fn progress(&self) -> Progress {
        self.progress.read_or_poisoned().clone()
    }

    fn partial_fetch_metadata(&self) -> repository_partial_fetch::PartialFetchMetadataManager {
        repository_partial_fetch::PartialFetchMetadataManager::new(&self.heddle_dir)
    }

    pub fn shallow(&self) -> std::sync::RwLockReadGuard<'_, ShallowInfo> {
        self.shallow.read_or_poisoned()
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

struct WorktreePointer {
    objectstore: PathBuf,
    source_authority: RepositorySourceAuthority,
}

fn parse_objectstore_pointer(content: &str) -> Option<WorktreePointer> {
    let mut objectstore = None;
    let mut source_authority = None;
    for line in content.lines() {
        if let Some(path) = line.strip_prefix("objectstore:") {
            let path = path.trim();
            if !path.is_empty() {
                objectstore = Some(PathBuf::from(path));
            }
        } else if let Some(authority) = line.strip_prefix("source-authority:") {
            source_authority = match authority.trim() {
                "native" => Some(RepositorySourceAuthority::Native),
                "git-overlay" => Some(RepositorySourceAuthority::GitOverlay),
                _ => return None,
            };
        }
    }
    Some(WorktreePointer {
        objectstore: objectstore?,
        source_authority: source_authority?,
    })
}

pub(crate) fn has_git_metadata(path: &Path) -> bool {
    let dot_git = path.join(".git");
    if !(dot_git.is_dir() || dot_git.is_file()) {
        return false;
    }

    SleyRepository::discover(path).is_ok()
}

fn repository_capability_for_authority(
    source_authority: RepositorySourceAuthority,
) -> RepositoryCapability {
    match source_authority {
        RepositorySourceAuthority::Native => RepositoryCapability::NativeHeddle,
        RepositorySourceAuthority::GitOverlay => RepositoryCapability::GitOverlay,
    }
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

#[cfg(feature = "git-overlay")]
fn git_path(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

#[cfg(feature = "git-overlay")]
fn ignored_git_overlay_status_path(path: &str) -> bool {
    path == ".heddle" || path.starts_with(".heddle/")
}

#[cfg(feature = "git-overlay")]
const GIT_MODE_COMMIT: u32 = 0o160000;

#[cfg(feature = "git-overlay")]
fn git_worktree_matches_repo_root(git: &SleyRepository, root: &Path) -> bool {
    git.workdir().is_some_and(
        |workdir| match (workdir.canonicalize(), root.canonicalize()) {
            (Ok(workdir), Ok(root)) => workdir == root,
            _ => false,
        },
    )
}

#[cfg(feature = "git-overlay")]
fn append_short_status_to_index_intent(
    staged_paths: &mut Vec<String>,
    extra_paths: &mut Vec<String>,
    ignore_matcher: &objects::worktree::WorktreeIgnoreMatcher,
    entry: sley::ShortStatusRow<'_>,
    path: &str,
) {
    if entry.index == b'?' && entry.worktree == b'?' {
        if !ignore_matcher.is_ignored(Path::new(path)) {
            extra_paths.push(format!("untracked: {path}"));
        }
        return;
    }
    if entry.index != b' ' && entry.index != b'!' {
        staged_paths.push(path.to_string());
    }
    if entry.worktree != b' '
        && entry.worktree != b'!'
        && !status_row_is_gitlink_worktree_only(entry)
    {
        extra_paths.push(format!("unstaged: {path}"));
    }
}

#[cfg(feature = "git-overlay")]
fn status_row_is_gitlink_worktree_only(entry: sley::ShortStatusRow<'_>) -> bool {
    entry.index == b' '
        && (entry.index_mode == Some(GIT_MODE_COMMIT)
            || entry.head_mode == Some(GIT_MODE_COMMIT)
            || entry.worktree_mode == Some(GIT_MODE_COMMIT))
}

#[cfg(feature = "git-overlay")]
fn git_overlay_untracked_path_ignored(
    ignore_matcher: &crate::worktree_ignore::WorktreeIgnoreMatcher,
    path: &Path,
) -> bool {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    ignore_matcher.should_prune_directory_child(parent, name)
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
    let merge_ref = GitRefName::new(merge);
    if merge_ref.content_namespace() != Some(GitRefContentNamespace::Branch) {
        return Ok(None);
    };
    let Some(short) = merge_ref.short_name() else {
        return Ok(None);
    };
    Ok(Some(GitRefName::remote_branch_full_name(remote, short)))
}

fn git_ahead_behind_counts(
    git: &SleyRepository,
    head: SleyObjectId,
    upstream: SleyObjectId,
) -> Result<(usize, usize)> {
    if upstream == head {
        return Ok((0, 0));
    }
    let (ahead, behind) = git
        .rev_graph()
        .ahead_behind(head, upstream)
        .map_err(|error| HeddleError::Config(error.to_string()))?;
    Ok((ahead, behind))
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

/// Read git's HEAD via sley's [`SleyRepository::head_state`], including
/// worktree `gitdir:` indirections and detached HEAD.
fn detect_git_head_state(path: &Path) -> Result<Option<GitHeadState>> {
    let repo = SleyRepository::discover(path).map_err(|error| {
        HeddleError::Config(format!(
            "failed to inspect git repository at '{}': {}",
            path.display(),
            error
        ))
    })?;
    let head = match repo.head_state() {
        Ok(head) => head,
        Err(_) => return Ok(None),
    };

    if head.is_missing() {
        return Ok(None);
    }
    if let Some(name) = head.branch_name() {
        if name.is_empty() {
            return Ok(None);
        }
        return Ok(Some(GitHeadState::Attached(name.to_string())));
    }
    if head.is_detached()
        && let Some(id) = head.oid()
    {
        return Ok(Some(GitHeadState::Detached(id)));
    }
    Ok(None)
}

/// Detect git's current HEAD branch.
fn detect_git_head(path: &Path) -> Result<Option<Head>> {
    if let Some(GitHeadState::Attached(thread)) = detect_git_head_state(path)? {
        return Ok(Some(Head::Attached {
            thread: ThreadName::from(thread),
        }));
    }
    Ok(None)
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
        let ref_name = GitRefName::new(value);
        if ref_name.content_namespace() == Some(GitRefContentNamespace::Branch)
            && let Some(short) = ref_name.short_name()
        {
            return Ok(Some(short.to_string()));
        }
        if !value.is_empty() {
            return Ok(Some(value.to_string()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::{path::Path, process::Command};

    use tempfile::TempDir;

    use super::Repository;
    use crate::RepositoryCapability;

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(root)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(
            status.success(),
            "git {:?} failed in {}",
            args,
            root.display()
        );
    }

    fn git_output(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn init_git_with_identity(root: &Path) {
        sley::Repository::init(root).expect("init git repository");
        git(root, &["config", "user.email", "test@heddle.local"]);
        git(root, &["config", "user.name", "Heddle Test"]);
    }

    fn configure_main_tracks_origin(root: &Path) {
        git(root, &["config", "branch.main.remote", "origin"]);
        git(root, &["config", "branch.main.merge", "refs/heads/main"]);
    }

    /// Diverged history (2 ahead / 1 behind) from the pre-sley hand-walk on this fixture:
    ///
    /// ```text
    ///        base
    ///       /    \
    ///      u1    l1
    ///           l2  <- HEAD
    /// ```
    ///
    /// `refs/remotes/origin/main` points at `u1`.
    fn diverged_two_ahead_one_behind_fixture() -> TempDir {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        init_git_with_identity(root);
        git(root, &["commit", "--allow-empty", "-m", "base"]);
        let base = git_output(root, &["rev-parse", "HEAD"]);
        git(root, &["commit", "--allow-empty", "-m", "u1"]);
        let upstream_tip = git_output(root, &["rev-parse", "HEAD"]);
        git(root, &["reset", "--hard", &base]);
        git(root, &["commit", "--allow-empty", "-m", "l1"]);
        git(root, &["commit", "--allow-empty", "-m", "l2"]);
        git(
            root,
            &["update-ref", "refs/remotes/origin/main", &upstream_tip],
        );
        configure_main_tracks_origin(root);
        temp
    }

    #[test]
    fn git_remote_tracking_reports_diverged_ahead_behind() {
        let temp = diverged_two_ahead_one_behind_fixture();
        let repo = Repository::init_git_overlay_sidecar(temp.path()).unwrap();
        assert_eq!(repo.capability(), RepositoryCapability::GitOverlay);

        let status = repo
            .git_remote_tracking_status()
            .unwrap()
            .expect("configured upstream with drift should return status");
        assert_eq!(status.ahead, 2);
        assert_eq!(status.behind, 1);
        assert_eq!(status.upstream, "origin/main");
    }

    #[test]
    fn git_remote_tracking_in_sync_returns_none() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        init_git_with_identity(root);
        git(root, &["commit", "--allow-empty", "-m", "only"]);
        let tip = git_output(root, &["rev-parse", "HEAD"]);
        git(root, &["update-ref", "refs/remotes/origin/main", &tip]);
        configure_main_tracks_origin(root);

        let repo = Repository::init_git_overlay_sidecar(root).unwrap();
        assert!(repo.git_remote_tracking_status().unwrap().is_none());
    }

    #[test]
    fn git_remote_tracking_without_upstream_config() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        init_git_with_identity(root);
        git(root, &["commit", "--allow-empty", "-m", "only"]);
        git(root, &["remote", "add", "origin", root.to_str().unwrap()]);

        let repo = Repository::init_git_overlay_sidecar(root).unwrap();
        let status = repo
            .git_remote_tracking_status()
            .unwrap()
            .expect("no upstream config still reports actionable status");
        assert_eq!(status.ahead, 0);
        assert_eq!(status.behind, 0);
        assert!(status.upstream.is_empty());
        assert!(status.message.contains("has no upstream tracking branch"));
    }
}
