// SPDX-License-Identifier: Apache-2.0
//! Repository discovery and bootstrap: root probing, Git-metadata
//! detection, and the `init`/`open` constructors of `Repository`.

#[cfg(feature = "git-overlay")]
use std::sync::Arc;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{OnceLock, RwLock},
};

use objects::{
    Progress,
    error::{HeddleError, Result, UntrustedRepositoryReason},
    fs_atomic::enrich_fs_error,
    object::ThreadName,
    store::{FsStore, ObjectStore, ShallowInfo},
};
use oplog::OpLog;
use refs::{Head, RefManager};
use sley::Repository as SleyRepository;

#[cfg(feature = "git-overlay")]
use super::git_overlay_object_source;
use super::{
    RepoConfig, Repository, RepositoryCapability, RepositorySourceAuthority, compute_op_scope,
    overlay::{detect_git_head, ensure_git_overlay_exclude},
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

/// Repository roots the user explicitly trusts (`[safe] repositories` in the
/// Heddle user config), canonicalized. Each process that opens repositories
/// registers them once at startup (the CLI, the mount workers); a library
/// caller that never registers any gets the strict default.
static SAFE_REPOSITORIES: OnceLock<Vec<PathBuf>> = OnceLock::new();

/// Directory inside an enclosing repository's `.heddle` that records the
/// nested repositories and checkouts Heddle itself created in its worktree.
const TRUSTED_NESTED_DIR: &str = "trusted-nested";

/// Register the user's explicitly trusted repository roots, the analogue of
/// Git's `safe.directory`. A listed root is opened even when it lies inside
/// another repository's worktree or another user owns it. The first
/// registration wins; later calls are ignored.
pub fn set_safe_repositories(roots: impl IntoIterator<Item = PathBuf>) {
    let roots = roots
        .into_iter()
        .map(|root| root.canonicalize().unwrap_or(root))
        .collect();
    let _ = SAFE_REPOSITORIES.set(roots);
}

/// Refuse repository metadata the user never vouched for (heddle#2034).
///
/// A `.heddle` directory is ordinary content below a worktree root, so a
/// checkout (Heddle's, or Git's for a vendored clone or submodule) can plant
/// `sub/.heddle/{HEAD,config.toml,hooks,objectstore}`. Opening that
/// repository from inside `sub/` would hand its config (TLS CA, upstream URL,
/// remotes), its hooks, and its store pointer control over this user's
/// commands. Mirroring Git's `safe.bareRepository=explicit` and
/// `safe.directory`, `root` is refused unless it is listed in
/// [`set_safe_repositories`], or both of these hold:
///
/// - the current user owns its `.heddle` entry, the directory it resolves to,
///   the store a checkout pointer names, and (when `.heddle` is a symlink) the
///   root directory holding that symlink;
/// - every enclosing Heddle repository whose worktree contains it vouches for
///   it with a creation record (see [`record_nested_repository_trust`]).
///   Metadata inside an enclosing `.heddle` (managed thread checkouts) is not
///   in that repository's worktree.
///
/// The creation record lives in the enclosing repository's root `.heddle`,
/// which no checkout may write, and binds the nested `.heddle`'s file
/// identity, so neither checked-out content nor a later replacement at the
/// same path can forge it. Content-based signals (a nested `.git`, its
/// `info/exclude`, its index) are all attacker-writable and are not trusted.
///
/// Refusing the whole repository rather than filtering individual config keys
/// fails closed: hooks, remotes, the hydrator config and the store pointer are
/// as dangerous as the TLS CA, and a key added later would be honoured by
/// default.
///
/// Windows has no ownership check: `std` exposes no owner SID, and a
/// per-user profile directory already carries an owner-only ACL. The embedded
/// check, which is the checked-out-content defence, applies on every platform.
pub fn ensure_repository_trusted(root: &Path) -> Result<()> {
    let safe = SAFE_REPOSITORIES
        .get()
        .map(Vec::as_slice)
        .unwrap_or_default();
    ensure_repository_trusted_with(root, safe, current_users())
}

/// The owners whose repositories this process trusts by ownership.
#[derive(Clone, Copy, Debug)]
pub(super) struct TrustedOwners {
    pub(super) euid: u32,
    /// The invoking user under `sudo` (Git's `SUDO_UID` rule): root acting
    /// for a user still trusts that user's repositories.
    pub(super) sudo_uid: Option<u32>,
}

impl TrustedOwners {
    fn accepts(self, owner: u32) -> bool {
        owner == self.euid || self.sudo_uid == Some(owner)
    }
}

#[cfg(unix)]
fn current_users() -> Option<TrustedOwners> {
    let euid = crate::daemon::peer::current_euid();
    let sudo_uid = (euid == 0)
        .then(|| std::env::var("SUDO_UID").ok()?.trim().parse::<u32>().ok())
        .flatten();
    Some(TrustedOwners { euid, sudo_uid })
}

#[cfg(not(unix))]
fn current_users() -> Option<TrustedOwners> {
    None
}

#[cfg(unix)]
fn owner_of(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;

    metadata.uid()
}

#[cfg(not(unix))]
fn owner_of(_metadata: &fs::Metadata) -> u32 {
    0
}

/// [`ensure_repository_trusted`] with the trust inputs supplied explicitly.
/// `owners` is `None` where the platform has no uid ownership model.
pub(super) fn ensure_repository_trusted_with(
    root: &Path,
    safe_repositories: &[PathBuf],
    owners: Option<TrustedOwners>,
) -> Result<()> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if safe_repositories.contains(&root) {
        return Ok(());
    }
    let heddle_dir = root.join(".heddle");
    if let Some(owners) = owners {
        ensure_owned(&root, &heddle_dir, owners)?;
    }
    for enclosing in enclosing_repository_roots(&root) {
        if !nested_record_vouches(&enclosing, &root) {
            return Err(untrusted(
                &root,
                UntrustedRepositoryReason::Embedded { enclosing },
            ));
        }
    }
    Ok(())
}

fn ensure_owned(root: &Path, heddle_dir: &Path, owners: TrustedOwners) -> Result<()> {
    let foreign = |path: &Path, metadata: &fs::Metadata| {
        let owner = owner_of(metadata);
        (!owners.accepts(owner)).then(|| {
            untrusted(
                root,
                UntrustedRepositoryReason::ForeignOwner {
                    path: path.to_path_buf(),
                    owner,
                    current: owners.euid,
                },
            )
        })
    };
    // Metadata failures are left to `Repository::open`, which reports a
    // missing or unreadable repository loudly.
    if let Ok(entry) = fs::symlink_metadata(heddle_dir) {
        if let Some(refusal) = foreign(heddle_dir, &entry) {
            return Err(refusal);
        }
        if entry.file_type().is_symlink()
            && let Ok(parent) = fs::metadata(root)
            && let Some(refusal) = foreign(root, &parent)
        {
            return Err(refusal);
        }
    }
    if let Ok(target) = fs::metadata(heddle_dir)
        && let Some(refusal) = foreign(heddle_dir, &target)
    {
        return Err(refusal);
    }
    if let Some(store) = pointed_store(heddle_dir)
        && let Ok(metadata) = fs::metadata(&store)
        && let Some(refusal) = foreign(&store, &metadata)
    {
        return Err(refusal);
    }
    Ok(())
}

/// Heddle repositories above `root` whose worktree contains it: every
/// ancestor repository root, except one whose `.heddle` holds `root`.
fn enclosing_repository_roots(root: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    bounded_ancestor_paths(root)
        .into_iter()
        .skip(1)
        .filter(move |enclosing| {
            is_heddle_repository_root(enclosing) && !root.starts_with(enclosing.join(".heddle"))
        })
}

/// Record name for the nested repository at `relative` inside an enclosing
/// worktree: a digest, so arbitrary path bytes never become file names.
fn nested_record_path(enclosing: &Path, relative: &Path) -> PathBuf {
    let digest = blake3::hash(relative.as_os_str().as_encoded_bytes());
    enclosing
        .join(".heddle")
        .join(TRUSTED_NESTED_DIR)
        .join(digest.to_hex().as_str())
}

/// Identity of the nested `.heddle` entry the record vouches for. Binding it
/// means a repository later deleted and replaced at the same path (for
/// example by a checkout) is not covered by the old record.
fn nested_record_body(root: &Path) -> Option<String> {
    let metadata = fs::symlink_metadata(root.join(".heddle")).ok()?;
    Some(format!("identity {}\n", file_identity(&metadata)))
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;

    format!("{}:{}", metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn file_identity(_metadata: &fs::Metadata) -> String {
    "unavailable".to_string()
}

fn nested_record_vouches(enclosing: &Path, root: &Path) -> bool {
    let Ok(relative) = root.strip_prefix(enclosing) else {
        return false;
    };
    let Some(expected) = nested_record_body(root) else {
        return false;
    };
    fs::read_to_string(nested_record_path(enclosing, relative))
        .is_ok_and(|recorded| recorded == expected)
}

/// Vouch for a repository or checkout Heddle just created at `root` in every
/// enclosing worktree, so implicit discovery admits it (see
/// [`ensure_repository_trusted`]). Called by the constructors that create
/// `.heddle` metadata; a standalone repository has nothing to record.
pub(super) fn record_nested_repository_trust(root: &Path) -> Result<()> {
    let root = root.canonicalize().map_err(|error| {
        HeddleError::Io(enrich_fs_error(root, "resolving new repository root", error))
    })?;
    let Some(body) = nested_record_body(&root) else {
        return Ok(());
    };
    for enclosing in enclosing_repository_roots(&root) {
        let Ok(relative) = root.strip_prefix(&enclosing) else {
            continue;
        };
        let record = nested_record_path(&enclosing, relative);
        if let Some(parent) = record.parent() {
            objects::fs_atomic::create_private_dir_all(parent)?;
        }
        objects::fs_atomic::write_file_atomic(&record, body.as_bytes())?;
    }
    Ok(())
}

fn untrusted(root: &Path, reason: UntrustedRepositoryReason) -> HeddleError {
    HeddleError::UntrustedRepository {
        root: root.to_path_buf(),
        reason,
    }
}

/// The canonical store a checkout's `.heddle/objectstore` pointer names, when
/// it is a readable absolute pointer. Malformed pointers are reported by
/// [`super::Repository::open`]; trust only needs the store they would select.
fn pointed_store(heddle_dir: &Path) -> Option<PathBuf> {
    let pointer = fs::read_to_string(heddle_dir.join("objectstore")).ok()?;
    let store = parse_objectstore_pointer(&pointer)?.objectstore;
    store
        .is_absolute()
        .then(|| store.canonicalize().ok())
        .flatten()
}

/// Refuse to operate from inside a metadata-less virtualized thread mount;
/// see [`metadataless_managed_thread_root`].
pub(super) fn refuse_metadataless_mount(start: &Path) -> Result<()> {
    match metadataless_managed_thread_root(start) {
        Some(mount_root) => Err(HeddleError::Config(format!(
            "'{}' is a Heddle-managed virtualized thread mount with no checkout \
             metadata of its own; refusing to operate on the parent repository from \
             inside it. Run heddle from the repository root, or use a solid/materialized \
             thread checkout.",
            mount_root.display()
        ))),
        None => Ok(()),
    }
}

/// The config file of the repository Heddle would open from `start`, for
/// callers that read repository config without opening it (TLS CA discovery,
/// first-screen help, the status fast path). Applies the same admission as
/// [`super::Repository::open`]: the virtualized-mount guard and
/// [`ensure_repository_trusted`]. A checkout's config is its store's, since
/// that is the config `open` loads.
pub fn discover_repository_config(start: &Path) -> Result<Option<PathBuf>> {
    let absolute = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir()?.join(start)
    };
    let start = absolute.canonicalize().unwrap_or(absolute);
    refuse_metadataless_mount(&start)?;
    let Some(root) = discover_heddle_root(&start) else {
        return Ok(None);
    };
    ensure_repository_trusted(&root)?;
    let heddle_dir = root.join(".heddle");
    if heddle_dir.join("objects").is_dir() {
        return Ok(Some(heddle_dir.join("config.toml")));
    }
    Ok(pointed_store(&heddle_dir).map(|store| store.join("config.toml")))
}

pub(super) struct WorktreePointer {
    pub(super) objectstore: PathBuf,
    pub(super) source_authority: RepositorySourceAuthority,
}

pub(super) fn parse_objectstore_pointer(content: &str) -> Option<WorktreePointer> {
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
    // Heddle binds original Git OIDs. Replacement refs neither define source
    // identity nor need enumeration during metadata discovery.
    let repo = SleyRepository::open_with(&dot_git, sley::OpenOptions::new().replace_objects(false))
        .map_err(|error| {
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
/// metadata-less mount and open the PARENT repo. The v6 encoding ends in
/// `entry`; recognize only a canonical encoded name above that terminal.
pub(super) fn metadataless_managed_thread_root(start_path: &Path) -> Option<PathBuf> {
    for dir in bounded_ancestor_paths(start_path) {
        let dir = dir.as_path();
        if let Some(thread_dir) = dir.parent()
            && thread_dir.file_name().and_then(|name| name.to_str()) == Some("entry")
            && let Some(threads) = thread_dir.ancestors().find(|ancestor| {
                ancestor.file_name().and_then(|name| name.to_str()) == Some("threads")
                    && ancestor
                        .parent()
                        .and_then(Path::file_name)
                        .and_then(|name| name.to_str())
                        == Some(".heddle")
            })
            && let Ok(encoded) = thread_dir.strip_prefix(threads)
            && (objects::name_encoding::decode_name_path(encoded).is_some()
                || objects::name_encoding::is_digest_name_path(encoded))
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

        let config_path = heddle_dir.join("config.toml");
        let mut config = match RepoConfig::load_for_repository(&config_path) {
            Ok(config) => config,
            Err(HeddleError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                RepoConfig::default()
            }
            Err(error) => return Err(error),
        };

        objects::fs_atomic::create_private_dir_all(&heddle_dir)?;
        objects::fs_atomic::create_private_dir_all(&heddle_dir.join("state"))?;
        record_nested_repository_trust(&root)?;
        // Establish recovery serialization during initialization, so later
        // observation never has to create the repository lock file.
        let _installation =
            crate::thread_replication::install_artifacts::InstallationLock::acquire(&heddle_dir)
                .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
        let store = FsStore::new(&heddle_dir);
        store.init()?;
        let refs = RefManager::new(&heddle_dir);
        refs.init()?;
        let oplog = OpLog::new_unattributed(&heddle_dir);
        oplog.init()?;

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
            pending_entry_visibility: RwLock::new(Vec::new()),
        };
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
        objects::fs_atomic::create_private_dir_all(&heddle_dir.join("state"))?;
        record_nested_repository_trust(&root)?;
        // Establish recovery serialization during initialization, so later
        // observation never has to create the repository lock file.
        let _installation =
            crate::thread_replication::install_artifacts::InstallationLock::acquire(&heddle_dir)
                .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;

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
            store,
            refs,
            oplog,
            config,
            shallow: RwLock::new(ShallowInfo::load(&heddle_dir)?),
            blob_hydrator: RwLock::new(None),
            signal_computer: RwLock::new(None),
            git_overlay_repo: RwLock::new(None),
            progress: RwLock::new(Progress::null()),
            pending_entry_visibility: RwLock::new(Vec::new()),
        };

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
