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
#[path = "history_instrumentation.rs"]
mod history_instrumentation;
#[cfg(test)]
#[path = "history_perf_contract_tests.rs"]
mod history_perf_contract_tests;
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
#[path = "repository_ref_mutation.rs"]
mod repository_ref_mutation;
#[path = "repository_resolve.rs"]
mod repository_resolve;
#[path = "repository_signing.rs"]
mod repository_signing;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

pub use commit_graph::{CommitGraphIndex, find_merge_base};
#[cfg(feature = "async-source")]
pub use commit_graph::{find_merge_base_async, is_ancestor_async};
pub use context_suggestions::{
    ContextSuggestion, ContextSuggestionTier, HIGH_SUGGESTION_THRESHOLD,
    MAJOR_REWRITE_THRESHOLD_PCT, MEDIUM_SUGGESTION_THRESHOLD, SUGGESTION_WINDOW,
    compute_rewrite_pct, is_major_rewrite,
};
pub use objects::object::DiffKind;
use objects::{
    Progress,
    error::{HeddleError, Result},
    lock::{RepoLock, RepositoryLockExt},
    object::{Attribution, ContentHash, State, StateId, ThreadName, Tree},
    store::{AnyStore, ObjectStore, ShallowInfo},
    sync::RwLockExt,
};
use oplog::{OpLog, OpLogBackend, OpRecord};
pub use refs::SpoolFacet;
use refs::{Head, RefBackend, RefExpectation, RefManager, RefUpdate};
pub use repo_config::{
    HostedConfig, KeyBindingRegistryAnchor, OutputFormat, ProvenanceConfig, RepoConfig,
    RepoRemoteConfig, RepositorySourceAuthority, TrustedKey,
};
// Review-epic config types — re-exported here so the new
// `signals.rs` (and external crates wanting to construct a
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
pub use repository_tree::{TreeBuildProfile, WorktreeCompareProfile, WorktreeStateLookupProfile};
pub use repository_worktree_status::{UntrackedSet, UntrackedSubtree, WorktreeStatusDetailed};
use sley::Repository as SleyRepository;

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
pub(crate) mod repository_worktree_status;
#[cfg(test)]
#[path = "status_monitor_tests.rs"]
mod status_monitor_tests;

#[path = "discovery.rs"]
mod discovery;
#[path = "overlay.rs"]
mod overlay;
#[path = "repository_open.rs"]
mod repository_open;
use repository_open::has_git_metadata;
#[path = "repository_operation_status.rs"]
mod repository_operation_status;
pub use repository_operation_status::{OperationKind, OperationScope, RepositoryOperationStatus};
#[path = "repository_identity.rs"]
mod repository_identity;
pub use repository_identity::is_synthetic_root;
use repository_identity::seed_principal;
#[cfg(test)]
use discovery::bounded_ancestor_paths_with_device;
#[cfg(test)]
use discovery::metadataless_managed_thread_root;
pub use discovery::{discover_heddle_root, is_heddle_repository_root, open_git_repository_at_root};
pub use overlay::{
    GitCheckpointIntent, GitCheckpointIntentPhase, GitCheckpointRecord, GitImportGuidance,
    GitRemoteTrackingStatus,
};
use overlay::{GitHeadState, detect_git_head_state, detect_git_in_progress_branch};
#[cfg(feature = "git-overlay")]
pub use overlay::{
    GitOverlayBranchTip, GitOverlayOutOfBandCommits, GitOverlayShortStatus, GitOverlayTagTip,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryCapability {
    GitOverlay,
    NativeHeddle,
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
/// - Hosted clones: the CLI-owned lazy hosted hydrator
///   bridges sync `hydrate` calls to an async hosted call via a dedicated worker
///   thread + private Tokio runtime; on each call the worker invokes
///   `HostedClient::hydrate_blob` for the requested hash on the current
///   local-thread tip.
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
    signal_computer: RwLock<Option<Arc<dyn crate::signals::SignalComputer>>>,
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
    pub fn heddle_dir(&self) -> &Path {
        &self.heddle_dir
    }

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
            signal_computer: RwLock::new(None),
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

impl Repository {
    pub fn root(&self) -> &Path {
        &self.root
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
        self.set_thread_recorded(&thread, state)
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
        let scope = self.op_scope();
        let result = self
            .refs
            .commit_and_publish(&records, ref_updates, Some(&scope));
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

    /// Turn the filesystem monitor off for this handle only.
    ///
    /// Every worktree-status walk resolves its fsmonitor mode from
    /// `config.worktree.fsmonitor` — `default_worktree_status_options`,
    /// maintenance, snapshot fingerprints, and core's verification-health walk
    /// all read it — so clearing it here disables the monitor for *all* status
    /// this handle performs, including the walks that resolve their own
    /// options deep inside core rather than accepting them from the caller.
    ///
    /// This is in-memory only. Nothing persists `self.config`: every
    /// `config.toml` write reloads the file first (see
    /// [`Repository::transition_source_authority`]), so the next process to
    /// open the repository sees its configured mode again.
    ///
    /// `heddle clone` uses this. A clone materializes a worktree and exits; it
    /// has no ongoing worktree to watch, but a monitor-backed status walk would
    /// spawn the long-lived `heddle-fsmonitor-worker` daemon as a child of the
    /// clone process, leaving anything that waits on the clone's process tree
    /// blocked on the helper's idle lifetime long after the clone finished
    /// (heddle#1243). The monitor is deferred, not removed: an explicit
    /// `native`/`auto`/`watchman` setting (or `HEDDLE_FSMONITOR`) starts the
    /// helper on demand. A default install leaves it off (heddle#1411).
    #[must_use]
    pub fn without_fsmonitor(mut self) -> Self {
        self.config.worktree.fsmonitor.mode = crate::FsMonitorMode::Off;
        self
    }

    /// Apply the runtime-resolved fsmonitor mode to this repository handle.
    ///
    /// CLI configuration merges user, repository, and environment settings.
    /// Storing that resolved mode on the handle ensures nested verification
    /// and summary paths use the same decision as explicit status calls.
    #[must_use]
    pub fn with_fsmonitor_mode(mut self, mode: crate::FsMonitorMode) -> Self {
        self.config.worktree.fsmonitor.mode = mode;
        self
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
    /// includes the `heddle maintenance fsck --full` recovery hint, so call sites
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

    /// Register a capture-time risk-signal computer. Entry points opt in
    /// once at startup; see [`crate::signals`] for the seam.
    pub fn set_signal_computer(&self, computer: Arc<dyn crate::signals::SignalComputer>) {
        *self.signal_computer.write_or_poisoned() = Some(computer);
    }

    /// The currently registered signal computer, if any.
    pub fn signal_computer(&self) -> Option<Arc<dyn crate::signals::SignalComputer>> {
        self.signal_computer.read_or_poisoned().clone()
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

fn repository_capability_for_authority(
    source_authority: RepositorySourceAuthority,
) -> RepositoryCapability {
    match source_authority {
        RepositorySourceAuthority::Native => RepositoryCapability::NativeHeddle,
        RepositorySourceAuthority::GitOverlay => RepositoryCapability::GitOverlay,
    }
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
