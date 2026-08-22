// SPDX-License-Identifier: Apache-2.0
//! Repository discovery and bootstrap: root probing, Git-metadata
//! detection, and the `init`/`open` constructors of `Repository`.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
};

use objects::{
    Progress,
    error::{HeddleError, Result},
    fs_atomic::enrich_fs_error,
    object::ThreadName,
    store::{AnyStore, FsStore, ObjectStore, ShallowInfo},
};
use oplog::OpLog;
use refs::{Head, RefManager};
use sley::Repository as SleyRepository;

#[cfg(feature = "git-overlay")]
use std::sync::Arc;

#[cfg(feature = "git-overlay")]
use super::git_overlay_object_source;
use super::overlay::{detect_git_head, ensure_git_overlay_exclude};
use super::{
    RepoConfig, Repository, RepositoryCapability, RepositorySourceAuthority, compute_op_scope,
    repository_capability_for_authority,
};
const HEDDLE_REPOSITORY_MEMBERS: &[&str] = &["HEAD", "objects", "objectstore", "oplog", "refs"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RepositoryOpenMode {
    Normal,
    OplogRecovery,
}

fn git_discovery_across_filesystem() -> bool {
    std::env::var("GIT_DISCOVERY_ACROSS_FILESYSTEM")
        .is_ok_and(|value| !matches!(value.as_str(), "" | "0" | "false" | "no" | "off"))
}

fn filesystem_device(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        fs::metadata(path).ok().map(|metadata| metadata.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

pub(super) fn bounded_ancestor_paths(start: &Path) -> Vec<PathBuf> {
    bounded_ancestor_paths_with_device(start, git_discovery_across_filesystem(), filesystem_device)
}

pub(super) fn bounded_ancestor_paths_with_device(
    start: &Path,
    across_filesystem: bool,
    device_of: impl Fn(&Path) -> Option<u64>,
) -> Vec<PathBuf> {
    let start_device = if across_filesystem {
        None
    } else {
        device_of(start)
    };
    let mut ancestors = Vec::new();
    let mut current = Some(start);
    while let Some(path) = current {
        ancestors.push(path.to_path_buf());
        let Some(parent) = path.parent() else {
            break;
        };
        if let (Some(start_device), Some(parent_device)) = (start_device, device_of(parent))
            && parent_device != start_device
        {
            break;
        }
        // A metadata failure for an unreadable parent yields no device. Keep
        // discovery fault-tolerant: marker probes on that path will simply
        // miss, while a later readable ancestor may still provide a boundary.
        current = Some(parent);
    }
    ancestors
}

/// Return whether `root/.heddle` contains repository-specific metadata.
///
/// The user configuration directory is also named `.heddle`, so the directory
/// name alone is not a repository marker. This is only a discovery probe, not
/// full validation: once a candidate is found, [`Repository::open`] still
/// validates it and reports malformed repository metadata loudly.
pub fn is_heddle_repository_root(root: &Path) -> bool {
    let heddle_dir = root.join(".heddle");
    heddle_dir.is_dir()
        && HEDDLE_REPOSITORY_MEMBERS
            .iter()
            .any(|member| fs::symlink_metadata(heddle_dir.join(member)).is_ok())
}

/// Find the nearest Heddle repository sidecar without allowing Git discovery
/// to claim the path first. The walk follows Git's filesystem-boundary policy
/// and honors `GIT_DISCOVERY_ACROSS_FILESYSTEM`.
pub fn discover_heddle_root(start: &Path) -> Option<PathBuf> {
    let absolute = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };
    let start = absolute.canonicalize().unwrap_or(absolute);
    bounded_ancestor_paths(&start)
        .into_iter()
        .find(|path| is_heddle_repository_root(path))
}

/// Open only the Git repository rooted at `root`; never inherit an ancestor.
/// This accepts both a normal worktree and Heddle's embedded bare `.git`
/// layout, while rejecting a worktree resolved to any other root.
pub fn open_git_repository_at_root(root: &Path) -> Result<Option<SleyRepository>> {
    let dot_git = root.join(".git");
    let metadata = match fs::metadata(&dot_git) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(HeddleError::Io(enrich_fs_error(
                &dot_git,
                "inspecting Git metadata",
                error,
            )));
        }
    };
    if !(metadata.is_dir() || metadata.is_file()) {
        return Ok(None);
    }
    let repo = SleyRepository::open(&dot_git).map_err(|error| {
        HeddleError::Config(format!(
            "failed to open Git metadata at '{}': {error}",
            dot_git.display()
        ))
    })?;
    if let Some(workdir) = repo.workdir() {
        let resolved_root = root.canonicalize().map_err(|error| {
            HeddleError::Io(enrich_fs_error(root, "resolving Git worktree root", error))
        })?;
        let resolved_workdir = workdir.canonicalize().map_err(|error| {
            HeddleError::Io(enrich_fs_error(
                &workdir,
                "resolving Git metadata worktree",
                error,
            ))
        })?;
        if resolved_workdir != resolved_root {
            return Err(HeddleError::Config(format!(
                "Git metadata at '{}' resolves to worktree '{}', not repository root '{}'",
                dot_git.display(),
                resolved_workdir.display(),
                resolved_root.display()
            )));
        }
    }
    Ok(Some(repo))
}

pub(super) fn has_git_repository_at_root(root: &Path) -> bool {
    open_git_repository_at_root(root).ok().flatten().is_some()
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
pub(super) fn metadataless_managed_thread_root(start_path: &Path) -> Option<PathBuf> {
    for dir in bounded_ancestor_paths(start_path) {
        let dir = dir.as_path();
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
    }
    None
}

impl<S: ObjectStore> Repository<RefManager, OpLog, S> {
    pub(super) fn open_raw(
        root: PathBuf,
        heddle_dir: PathBuf,
        store: S,
        config: RepoConfig,
        refs: RefManager,
        mode: RepositoryOpenMode,
    ) -> Result<Self> {
        let actor = config
            .principal
            .as_ref()
            .map(|p| objects::object::Principal::new(&p.name, &p.email))
            .unwrap_or_else(|| objects::object::Principal::new("<unknown>", ""));
        let oplog = OpLog::new(&heddle_dir, actor.clone());
        if mode == RepositoryOpenMode::Normal {
            oplog.validate_structural_health()?;
        }
        let shallow = ShallowInfo::load(&heddle_dir)?;
        if mode == RepositoryOpenMode::OplogRecovery {
            return Ok(Self::from_parts(
                root, heddle_dir, store, refs, oplog, config, shallow,
            ));
        }
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

    /// Build or resume the unpublished local skeleton for a hosted clone.
    ///
    /// A durable [`crate::clone_intent::CloneIntent`] must already exist. This
    /// initializer persists the source authority selected from the server's
    /// bootstrap refs, but deliberately creates no HEAD or thread ref: those
    /// are the publication gate and are written only after the fetched closure
    /// passes hash verification and its clone durability batch commits.
    pub fn init_clone(
        path: impl AsRef<Path>,
        source_authority: RepositorySourceAuthority,
    ) -> Result<Self> {
        let root = path.as_ref().to_path_buf();
        let heddle_dir = root.join(".heddle");
        if crate::clone_intent::CloneIntent::load(&root)?.is_none() {
            return Err(HeddleError::Config(format!(
                "clone initialization at {} requires a durable clone intent",
                root.display()
            )));
        }

        objects::fs_atomic::create_private_dir_all(&heddle_dir)?;
        let store = FsStore::new(&heddle_dir);
        store.init()?;
        let refs = RefManager::new(&heddle_dir);
        refs.init()?;
        let oplog = OpLog::new_unattributed(&heddle_dir);
        oplog.init()?;

        let config_path = heddle_dir.join("config.toml");
        let mut config = match RepoConfig::load_for_repository(&config_path) {
            Ok(config) => config,
            Err(HeddleError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                RepoConfig::default()
            }
            Err(error) => return Err(error),
        };
        config.repository.source_authority = source_authority;
        config.save(&config_path)?;
        let store = Self::build_store(&config, &root, &heddle_dir, None)?;

        let reconciler = std::sync::Arc::new(crate::atomic::OplogRefReconciler::new(
            &heddle_dir,
            compute_op_scope(&root),
        ));
        let committer = std::sync::Arc::new(crate::atomic::OplogRefCommitter::new(
            &heddle_dir,
            objects::object::Principal::new("<unknown>", ""),
        ));
        let refs = refs.with_reconciler(reconciler).with_committer(committer);
        refs.init_reconcile_watermark()?;
        let repo = Self {
            root,
            heddle_dir: heddle_dir.clone(),
            capability: repository_capability_for_authority(config.repository.source_authority),
            store,
            refs,
            oplog,
            config,
            shallow: RwLock::new(ShallowInfo::load(&heddle_dir)?),
            blob_hydrator: RwLock::new(None),
            signal_computer: RwLock::new(None),
            git_overlay_repo: RwLock::new(None),
            progress: RwLock::new(Progress::null()),
        };
        crate::migration::apply_pending(&repo)?;
        Ok(repo)
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
            signal_computer: RwLock::new(None),
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
}
