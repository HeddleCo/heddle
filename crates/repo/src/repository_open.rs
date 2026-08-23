// SPDX-License-Identifier: Apache-2.0
//! The config-driven open path: locating `.heddle` state on disk, building
//! the object store, replaying snapshot artifacts, and running the local-only
//! open hooks (migrations + lazy-hydrator reconstruction).

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use objects::{
    error::{HeddleError, Result},
    fs_atomic::{enrich_fs_error, write_file_atomic},
    object::ThreadName,
    store::{AnyStore, FsStore},
};
use oplog::{ConditionalCommitOutcome, IsolationPrecondition, OpLogBackend, OpRecord};
use refs::{Head, RefManager};

#[cfg(feature = "git-overlay")]
use super::git_overlay_object_source;
use super::{RepoConfig, Repository, RepositoryCapability, RepositorySourceAuthority};
use super::discovery::{
    RepositoryOpenMode, bounded_ancestor_paths, discover_heddle_root,
    has_git_repository_at_root, is_heddle_repository_root, metadataless_managed_thread_root,
};
use super::overlay::{GitHeadState, detect_git_head_state, ensure_git_overlay_exclude};

pub(super) fn has_git_metadata(path: &Path) -> bool {
    has_git_repository_at_root(path)
}

fn ensure_supported_repo_format(config_path: &Path, config: &RepoConfig) -> Result<()> {
    let found = config.repository.version;
    let supported = super::repo_config::SUPPORTED_REPO_FORMAT;
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
            OpRecord::Goto { .. }
            | OpRecord::ThreadCreate { .. }
            | OpRecord::ThreadDelete { .. }
            | OpRecord::ThreadUpdate { .. }
            | OpRecord::Fork { .. }
            | OpRecord::Collapse { .. }
            | OpRecord::MarkerCreate { .. }
            | OpRecord::MarkerDelete { .. }
            | OpRecord::Checkpoint { .. }
            | OpRecord::TransactionAbort { .. }
            | OpRecord::EphemeralThreadCollapse { .. }
            | OpRecord::ConflictResolved { .. }
            | OpRecord::TransactionCommit { .. }
            | OpRecord::Redact { .. }
            | OpRecord::Purge { .. }
            | OpRecord::FastForward { .. }
            | OpRecord::GitCheckpoint { .. }
            | OpRecord::RemoteThreadUpdate { .. }
            | OpRecord::RemoteThreadDelete { .. }
            | OpRecord::UndoRecoveryUpdate { .. }
            | OpRecord::StateVisibilitySet { .. }
            | OpRecord::StateVisibilityPromote { .. }
            | OpRecord::HeadUpdate { .. } => None,
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

impl Repository {
    /// Run the local-only hooks that follow a config-driven [`Repository::open`]:
    /// declarative migrations + lazy-clone hydrator reconstruction. Both are
    /// bound to the default `AnyStore` flavor (`apply_pending` and
    /// `BlobHydrator` operate on the bare `Repository`), so they live here
    /// rather than in the generic `open_raw`.
    pub(super) fn run_open_hooks(&self) -> Result<()> {
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
        let mut pending = self.store.snapshot_commit_recovery_descriptors()?;
        if pending.is_empty() {
            return Ok(());
        }
        // Classify every artifact against one fresh, validated transaction
        // index. Reopening the complete packed oplog once per descriptor made
        // repository open O(snapshot artifacts × oplog bytes). Exact-once
        // recovery below remains the serialized authority if another process
        // commits after this snapshot was taken.
        let mut committed_transactions = self.oplog.committed_transaction_ids(
            pending
                .iter()
                .map(|descriptor| descriptor.artifact.transaction_id.as_str()),
        )?;
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
                if committed_transactions.contains(&artifact.transaction_id) {
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
                    | ConditionalCommitOutcome::AlreadyCommitted(_) => {
                        committed_transactions.insert(artifact.transaction_id.clone());
                        progressed = true;
                    }
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
    pub(super) fn build_store(
        config: &RepoConfig,
        root: &Path,
        heddle_dir: &Path,
        shared_overlay_source_root: Option<&Path>,
    ) -> Result<AnyStore> {
        let mut fs_store = FsStore::new(heddle_dir);
        fs_store.set_snapshot_delta_search(config.storage.delta_search.snapshot);
        let store = AnyStore::Fs(fs_store);
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

    /// Open an existing repository.
    ///
    /// Searches for `.heddle` starting at `path` and walking its ancestors.
    /// `.heddle` is always a directory; its contents distinguish a main repo
    /// from a worktree checkout:
    ///
    /// - Main repo: `.heddle/objects`, `.heddle/refs`, `.heddle/HEAD`,
    ///   etc.
    /// - Worktree: `.heddle/objectstore` (shared store path and checkout
    ///   authority), `.heddle/HEAD` (per-checkout), `.heddle/state/`
    ///   (per-checkout cached state).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_mode(path.as_ref(), RepositoryOpenMode::Normal)
    }

    /// Open a Heddle store only when one already exists at `path` or an ancestor.
    ///
    /// [`Repository::open`] bootstraps a `.heddle` sidecar and Git excludes
    /// for a plain Git checkout. Observe-only commands (whoami, status,
    /// verify, doctor) must probe with [`discover_heddle_root`] first so a
    /// read-only query never adopts the tree.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Option<Self>> {
        let path = path.as_ref();
        let Some(root) = discover_heddle_root(path) else {
            return Ok(None);
        };
        Ok(Some(Self::open(&root)?))
    }

    /// Open only enough repository state to run explicit oplog recovery.
    ///
    /// This deliberately bypasses oplog validation, open hooks, ref-tail
    /// reconciliation, and Git-overlay synchronization so an operator can
    /// authorize salvage even when normal repository open correctly refuses a
    /// damaged or non-contiguous generation.
    pub fn open_for_oplog_recovery(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_mode(path.as_ref(), RepositoryOpenMode::OplogRecovery)
    }

    fn open_with_mode(path: &Path, mode: RepositoryOpenMode) -> Result<Self> {
        heddle_perf_contract::record_repository_open();
        let requested_path = path;
        let start_path = requested_path.canonicalize().map_err(|error| {
            HeddleError::Io(enrich_fs_error(
                requested_path,
                "resolving repository path",
                error,
            ))
        })?;
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

        for dir in bounded_ancestor_paths(&start_path) {
            let dir = dir.as_path();
            if discovered_git_root.is_none() && has_git_metadata(dir) {
                discovered_git_root = Some(dir.to_path_buf());
            }
            let heddle_path = dir.join(".heddle");

            if crate::clone_intent::CloneIntent::path(dir).is_file() {
                return Err(HeddleError::IncompleteClone(dir.to_path_buf()));
            }

            if is_heddle_repository_root(dir) {
                let pointer_path = heddle_path.join("objectstore");
                let objects_dir = heddle_path.join("objects");
                if !pointer_path.is_file() && !objects_dir.is_dir() {
                    return Err(HeddleError::RepositoryNotFound(dir.to_path_buf()));
                }

                if let Some(git_root) = discovered_git_root.as_ref()
                    && git_root != dir
                    && git_root.starts_with(dir)
                    && !git_root.join(".heddle").exists()
                {
                    if mode == RepositoryOpenMode::OplogRecovery {
                        return Err(HeddleError::RepositoryNotFound(git_root.clone()));
                    }
                    ensure_git_overlay_exclude(git_root)?;
                    Self::bootstrap_git_overlay(git_root)?;
                    return Self::open_with_mode(git_root, mode);
                }

                if pointer_path.is_file() {
                    // Worktree mode: pointer dir at <dir>/.heddle/, shared
                    // object store at the path read from .heddle/objectstore.
                    let content = fs::read_to_string(&pointer_path).map_err(|error| {
                        HeddleError::Io(enrich_fs_error(
                            &pointer_path,
                            "reading worktree pointer",
                            error,
                        ))
                    })?;
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
                             contain an 'objects' directory; not a valid Heddle store",
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
                    let repo = Self::open_raw(
                        dir.to_path_buf(),
                        shared_galeed_dir,
                        store,
                        config,
                        refs,
                        mode,
                    )?;
                    if mode == RepositoryOpenMode::Normal {
                        repo.run_open_hooks()?;
                    }
                    return Ok(repo);
                }

                if objects_dir.is_dir() {
                    // Main repo mode.
                    let config_path = heddle_path.join("config.toml");
                    let config = RepoConfig::load_for_repository(&config_path)?;
                    ensure_supported_repo_format(&config_path, &config)?;
                    let store = Self::build_store(&config, dir, &heddle_path, None)?;
                    let refs = RefManager::new(&heddle_path);
                    let repo =
                        Self::open_raw(dir.to_path_buf(), heddle_path, store, config, refs, mode)?;
                    if mode == RepositoryOpenMode::Normal {
                        repo.run_open_hooks()?;
                    }
                    if mode == RepositoryOpenMode::OplogRecovery {
                        return Ok(repo);
                    }
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
                                    repo.write_head_recorded(&git_head)?;
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
                                        repo.write_head_recorded(&git_head)?;
                                    }
                                }
                            }
                            Ok(None) | Err(_) => {}
                        }
                    }
                    return Ok(repo);
                }
            }
        }

        // Mutating commands historically rely on open() bootstrapping a plain
        // Git tree into a Git-overlay sidecar (import/thread/start/marker…).
        // Observe-only commands (status/verify/doctor) must NOT call open on
        // plain Git — they take the plain-Git probe path so they never create
        // `.heddle`. See `verify_execution_context_from_cli`.
        if mode == RepositoryOpenMode::Normal
            && let Some(git_root) = discovered_git_root
        {
            ensure_git_overlay_exclude(&git_root)?;
            Self::bootstrap_git_overlay(&git_root)?;
            return Self::open_with_mode(&git_root, mode);
        }

        Err(HeddleError::RepositoryNotFound(path.to_path_buf()))
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
