// SPDX-License-Identifier: Apache-2.0
//! Core Git bridge types and operations.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use objects::{
    error::HeddleError,
    object::{ChangeId, ChangeIdParseError, ContentHash, FileMode, Principal, ThreadName, Tree},
    store::ObjectStore,
};
use refs::Head;
use repo::Repository as HeddleRepository;
use sley::{
    BString as GitBString, DeleteRef, FullName, GitObjectType, GitTime, Index, IndexEntry,
    IndexWriteOptions, ObjectFormat, ObjectId, RefPrecondition, ReferenceTarget,
    Repository as SleyRepository, Signature,
    plumbing::sley_core::ByteString as GitByteString,
    remote::{
        FetchOptions, LsRemoteFilter, NoCredentials, PushActionPlan, PushCommand, PushOptions,
        SilentProgress,
    },
};

use super::{
    git_export::{commit_is_byte_faithful, export_all, export_current_thread},
    git_ingest::import_git_history,
    git_reconstruct::{commit_object_id, reconstruct_commit_bytes, write_commit_object},
    git_util::ImportStats,
};

/// Errors specific to Git bridge operations.
#[derive(Debug, thiserror::Error)]
pub enum GitBridgeError {
    #[error("git error: {0}")]
    Git(String),

    #[error("store error: {0}")]
    Store(#[from] HeddleError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid trailer format: {0}")]
    InvalidTrailer(String),

    #[error("missing required trailer: {0}")]
    MissingTrailer(String),

    #[error("invalid mapping: {0}")]
    InvalidMapping(String),

    #[error("commit not found: {0}")]
    CommitNotFound(String),

    #[error("state not found: {0}")]
    StateNotFound(ChangeId),

    #[error("git repository not initialized")]
    GitRepoNotInitialized,

    #[error(
        "shallow Git repository at {repository} cannot be imported until full ancestry is available"
    )]
    ShallowClone {
        repository: PathBuf,
        retry_command: String,
    },

    #[error("conflict during sync: {0}")]
    Conflict(String),

    #[error("Git-overlay mapping conflict: {message}")]
    MappingConflict { message: String },

    #[error("Git branch '{branch}' cannot be imported as a Heddle thread: {message}")]
    InvalidThreadName { branch: String, message: String },

    #[error(
        "Git branch {branch} and Heddle thread {thread} diverged: thread {thread_change}, branch {branch_change}"
    )]
    GitHeddleThreadDiverged {
        thread: String,
        branch: String,
        thread_change: ChangeId,
        branch_change: ChangeId,
    },

    #[error(
        "ref update would rewrite {name}: {old} -> {new}; refusing to replace a user-visible Git commit with a Heddle export commit"
    )]
    NonFastForwardRef {
        name: String,
        old: ObjectId,
        new: ObjectId,
    },

    #[error(
        "remote branch {upstream} does not fast-forward the local Git checkpoint for {branch}: local {local}, remote {remote}"
    )]
    RemoteDiverged {
        branch: String,
        upstream: String,
        local: ObjectId,
        remote: ObjectId,
    },

    #[error("change id parse error: {0}")]
    ChangeIdParse(#[from] ChangeIdParseError),
}

/// Type alias for Git bridge results.
pub type GitResult<T> = std::result::Result<T, GitBridgeError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefNamespace {
    Branch,
    Tag,
    /// `refs/notes/<name>` — heddle uses `refs/notes/heddle` to carry
    /// per-commit metadata (change_id) without disturbing commit SHAs.
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefUpdate {
    pub name: String,
    pub target: ObjectId,
    pub namespace: RefNamespace,
}

/// Sentinel remote name for refs owned by the local repository
/// (`refs/heads/*` and `refs/tags/*`). Ported from jj's
/// `REMOTE_NAME_FOR_LOCAL_GIT_REPO` (`lib/src/git.rs`). Because a remote
/// literally named `git` would collide with this sentinel, such a name must
/// be rejected when remotes are configured.
pub const REMOTE_NAME_FOR_LOCAL_GIT_REPO: &str = "git";

/// Whether `remote` collides with [`REMOTE_NAME_FOR_LOCAL_GIT_REPO`], the
/// sentinel reserved for refs owned by the local repository. A user remote
/// with this name cannot be represented unambiguously against local refs, so
/// it must be rejected at every site that parses or accepts a remote name.
/// Single source of truth for the reserved-namespace check.
pub(crate) fn is_reserved_git_remote_name(remote: &str) -> bool {
    remote == REMOTE_NAME_FOR_LOCAL_GIT_REPO
}

/// Reject a remote name that collides with [`REMOTE_NAME_FOR_LOCAL_GIT_REPO`].
/// Surfaced at the public fetch/pull accept boundary with an actionable
/// message, and re-applied as an invariant net at every
/// `refs/remotes/{name}/...` write site, so a remote named `git` can never be
/// treated as a normal remote-tracking namespace — keeping the writers
/// consistent with [`parse_git_ref`], which already rejects such refs.
fn reject_reserved_git_remote_name(remote: &str) -> GitResult<()> {
    if is_reserved_git_remote_name(remote) {
        return Err(GitBridgeError::Git(format!(
            "a Git remote named '{remote}' collides with heddle's reserved namespace \
             (local refs are recorded under the '{REMOTE_NAME_FOR_LOCAL_GIT_REPO}' sentinel); \
             rename the remote (e.g. `git remote rename {remote} origin`) and retry"
        )));
    }
    Ok(())
}

fn remote_name_from_remote_ref(ref_name: &str) -> Option<&str> {
    let remote_and_name = ref_name.strip_prefix("refs/remotes/")?;
    let remote = remote_and_name
        .split_once('/')
        .map_or(remote_and_name, |(remote, _)| remote);
    (!remote.is_empty()).then_some(remote)
}

fn validate_refspec_ref(ref_name: &str) -> GitResult<()> {
    if let Some(remote) = remote_name_from_remote_ref(ref_name) {
        reject_reserved_git_remote_name(remote)?;
    }
    Ok(())
}

/// The kind of Git ref [`parse_git_ref`] recognizes. Ported from jj's
/// `GitRefKind` (`lib/src/git.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitRefKind {
    /// `refs/heads/<name>` or `refs/remotes/<remote>/<name>`.
    Branch,
    /// `refs/tags/<name>`.
    Tag,
}

/// A parsed Git ref name: its kind, short name, and owning remote. Borrows
/// from the input ref name. Ported from jj's `RemoteRefSymbol` shape
/// (`lib/src/git.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedGitRef<'a> {
    pub kind: GitRefKind,
    /// Short name beneath the namespace, e.g. `main` for `refs/heads/main`
    /// or `feature/x` for `refs/remotes/origin/feature/x`.
    pub name: &'a str,
    /// Owning remote. Local refs (`refs/heads/*`, `refs/tags/*`) report
    /// [`REMOTE_NAME_FOR_LOCAL_GIT_REPO`].
    pub remote: &'a str,
}

/// Parse a fully-qualified Git ref name into its [`GitRefKind`], short name,
/// and owning remote. Returns `None` for refs outside the
/// branch/remote-branch/tag namespaces (e.g. `refs/notes/*`, `HEAD`).
///
/// Ported from jj's `parse_git_ref` (`lib/src/git.rs`); like jj, the symbolic
/// `HEAD` and `refs/remotes/<remote>/HEAD` entries are not treated as refs.
pub fn parse_git_ref(ref_name: &str) -> Option<ParsedGitRef<'_>> {
    RefSpec::new(None, ref_name, false).ok()?;

    if let Some(name) = ref_name.strip_prefix("refs/heads/") {
        // Git rejects `HEAD` as a branch name.
        (name != "HEAD").then_some(ParsedGitRef {
            kind: GitRefKind::Branch,
            name,
            remote: REMOTE_NAME_FOR_LOCAL_GIT_REPO,
        })
    } else if let Some(remote_and_name) = ref_name.strip_prefix("refs/remotes/") {
        let (remote, name) = remote_and_name.split_once('/')?;
        // `refs/remotes/<remote>/HEAD` is the remote's symbolic default, not a
        // real remote-tracking branch. A remote literally named `git` collides
        // with the local sentinel ([`REMOTE_NAME_FOR_LOCAL_GIT_REPO`]); aliasing
        // it onto local refs would make remote-tracking branches
        // indistinguishable from `refs/heads/*`. Such a remote is already
        // rejected by the `RefSpec::new` validation at the top of this function
        // (`validate_refspec_ref` → `reject_reserved_git_remote_name`), so by the
        // time we reach this branch `remote` is guaranteed not to collide —
        // matching jj's parser and the sentinel ownership contract.
        (name != "HEAD").then_some(ParsedGitRef {
            kind: GitRefKind::Branch,
            name,
            remote,
        })
    } else {
        ref_name
            .strip_prefix("refs/tags/")
            .map(|name| ParsedGitRef {
                kind: GitRefKind::Tag,
                name,
                remote: REMOTE_NAME_FOR_LOCAL_GIT_REPO,
            })
    }
}

/// A Git refspec: an optional `source`, a `destination`, and a `forced` (`+`)
/// marker. Ported from jj's `RefSpec` (`lib/src/git.rs`).
mod refspec {
    use super::{GitResult, validate_refspec_ref};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RefSpec {
        forced: bool,
        /// `None` encodes a delete refspec (`:destination`).
        source: Option<String>,
        destination: String,
    }

    impl RefSpec {
        /// Construct a refspec after enforcing reserved-remote-name invariants.
        pub fn new(
            source: Option<String>,
            destination: impl Into<String>,
            forced: bool,
        ) -> GitResult<Self> {
            let destination = destination.into();
            if source.is_none() && destination.is_empty() {
                return Err(super::GitBridgeError::InvalidMapping(
                    "refspec source and destination cannot both be empty".to_string(),
                ));
            }
            if let Some(source) = source.as_deref() {
                validate_refspec_ref(source)?;
            }
            validate_refspec_ref(&destination)?;
            Ok(Self {
                forced,
                source,
                destination,
            })
        }

        /// A forced (`+`) refspec mapping `source` onto `destination`.
        pub fn forced(
            source: impl Into<String>,
            destination: impl Into<String>,
        ) -> GitResult<Self> {
            Self::new(Some(source.into()), destination, true)
        }

        /// A delete refspec (`:destination`). Not forced: deleting a destination
        /// that has no source cannot lose work.
        pub fn delete(destination: impl Into<String>) -> GitResult<Self> {
            Self::new(None, destination, false)
        }

        /// Render in `git` refspec syntax, including the leading `+` when forced.
        pub fn to_git_format(&self) -> String {
            format!(
                "{}{}",
                if self.forced { "+" } else { "" },
                self.to_git_format_not_forced()
            )
        }

        /// Render in `git` refspec syntax without the leading `+`, even when forced.
        pub fn to_git_format_not_forced(&self) -> String {
            format!(
                "{}:{}",
                self.source.as_deref().unwrap_or(""),
                self.destination
            )
        }
    }
}

pub use refspec::RefSpec;

/// A negative refspec (`^source`) excluding refs from a fetch or push. Ported
/// from jj's `NegativeRefSpec` (`lib/src/git.rs`).
mod negative_refspec {
    use super::{GitBridgeError, GitResult, validate_refspec_ref};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct NegativeRefSpec {
        source: String,
    }

    impl NegativeRefSpec {
        /// Construct a negative refspec after validating the rendered `^source`
        /// form Git will receive.
        pub fn new(source: impl Into<String>) -> GitResult<Self> {
            let source = source.into();
            validate_refspec_ref(&source)?;
            if source.contains('*') {
                return Err(GitBridgeError::InvalidMapping(format!(
                    "invalid negative refspec source '{source}': Negative glob patterns are not supported"
                )));
            }
            Ok(Self { source })
        }

        /// Render in `git` refspec syntax (`^source`).
        pub fn to_git_format(&self) -> String {
            format!("^{}", self.source)
        }
    }
}

// Keep the concrete fields in a private submodule. Callers outside this module
// cannot construct `NegativeRefSpec { ... }` directly (E0451), so all values
// pass through `NegativeRefSpec::new`.
pub use negative_refspec::NegativeRefSpec;

/// The fetch refspecs heddle uses to mirror a remote: every branch and every
/// heddle note, forced. Built through [`RefSpec`] so the wire format has a
/// single typed source of truth.
fn heddle_mirror_fetch_refspecs() -> GitResult<[String; 2]> {
    Ok([
        RefSpec::forced("refs/heads/*", "refs/heads/*")?.to_git_format(),
        RefSpec::forced("refs/notes/*", "refs/notes/*")?.to_git_format(),
    ])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitPushScope {
    CurrentThread,
    AllThreads,
}

#[derive(Debug, Clone, Default)]
pub struct GitPullOutcome {
    pub changed: bool,
    pub states_created: usize,
    pub commits_seen: usize,
    pub materialized_checkout: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PullPreflight {
    UpToDate,
    ImportRequired,
}

fn pull_outcome(stats: &ImportStats, materialized_checkout: bool) -> GitPullOutcome {
    GitPullOutcome {
        changed: materialized_checkout || stats.states_created > 0,
        states_created: stats.states_created,
        commits_seen: stats.commits_imported,
        materialized_checkout,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitFetchScope {
    BranchesAndNotes,
    AllRefs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshCheckoutAfterFetch {
    Yes,
    No,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteDirection {
    Fetch,
    Push,
}

#[derive(Debug, Clone)]
enum ResolvedRemote {
    Local(PathBuf),
    Url(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteThroughSkipReason {
    MissingDotGit,
    DetachedHead,
    NoAttachedThread,
    NoMappedCommit,
    MirrorIsWorktree,
    IndexAlreadyDirty,
}

impl std::fmt::Display for WriteThroughSkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteThroughSkipReason::MissingDotGit => {
                write!(f, "this checkout does not have a Git working tree")
            }
            WriteThroughSkipReason::DetachedHead => {
                write!(f, "Git HEAD is detached")
            }
            WriteThroughSkipReason::NoAttachedThread => {
                write!(f, "the attached Heddle thread does not resolve to a state")
            }
            WriteThroughSkipReason::NoMappedCommit => {
                write!(f, "the current Heddle state has not been exported to Git")
            }
            WriteThroughSkipReason::MirrorIsWorktree => {
                write!(f, "the Git mirror is already the active checkout")
            }
            WriteThroughSkipReason::IndexAlreadyDirty => {
                write!(f, "the Git index is already locked by another operation")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteThroughOutcome {
    Wrote(ObjectId),
    Skipped(WriteThroughSkipReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalGitIdentity {
    pub(crate) name: String,
    pub(crate) email: String,
}

impl LocalGitIdentity {
    pub(crate) fn from_principal(principal: &Principal) -> Self {
        Self {
            name: principal.name.clone(),
            email: principal.email.clone(),
        }
    }

    pub(crate) fn to_ident_line(&self, seconds: i64) -> Vec<u8> {
        format!("{} <{}> {} +0000", self.name, self.email, seconds).into_bytes()
    }

    pub(crate) fn to_signature(&self, seconds: i64) -> Signature {
        let ident = self.to_ident_line(seconds);
        Signature {
            name: GitByteString::new(self.name.as_bytes().to_vec()),
            email: GitByteString::new(self.email.as_bytes().to_vec()),
            time: GitTime::new(seconds, 0),
            raw: ident,
        }
    }
}

impl WriteThroughOutcome {
    pub fn object_id(self) -> Option<ObjectId> {
        match self {
            WriteThroughOutcome::Wrote(oid) => Some(oid),
            WriteThroughOutcome::Skipped(_) => None,
        }
    }

    pub fn skip_reason(self) -> Option<WriteThroughSkipReason> {
        match self {
            WriteThroughOutcome::Skipped(reason) => Some(reason),
            WriteThroughOutcome::Wrote(_) => None,
        }
    }
}

/// Mapping between Heddle ChangeIds and Git commit object IDs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncMapping {
    /// Maps Heddle ChangeId -> Git object id
    heddle_to_git: HashMap<ChangeId, ObjectId>,
    /// Maps Git object id -> Heddle ChangeId
    git_to_heddle: HashMap<ObjectId, ChangeId>,
}

impl SyncMapping {
    /// Create a new empty mapping.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a mapping.
    pub fn insert(&mut self, change_id: ChangeId, git_oid: ObjectId) {
        if let Some(previous_git) = self.heddle_to_git.remove(&change_id) {
            self.git_to_heddle.remove(&previous_git);
        }
        if let Some(previous_change) = self.git_to_heddle.remove(&git_oid) {
            self.heddle_to_git.remove(&previous_change);
        }
        self.heddle_to_git.insert(change_id, git_oid);
        self.git_to_heddle.insert(git_oid, change_id);
    }

    /// Insert a mapping and detect conflicts.
    pub(crate) fn insert_checked(
        &mut self,
        change_id: ChangeId,
        git_oid: ObjectId,
    ) -> GitResult<()> {
        if let Some(existing) = self.heddle_to_git.get(&change_id)
            && *existing != git_oid
        {
            return Err(GitBridgeError::MappingConflict {
                message: format!(
                    "change id {} mapped to {} (new {})",
                    change_id, existing, git_oid
                ),
            });
        }

        if let Some(existing) = self.git_to_heddle.get(&git_oid)
            && *existing != change_id
        {
            return Err(GitBridgeError::MappingConflict {
                message: format!(
                    "git oid {} mapped to {} (new {})",
                    git_oid, existing, change_id
                ),
            });
        }

        self.insert(change_id, git_oid);
        Ok(())
    }

    /// Get Git object id for a Heddle ChangeId.
    pub fn get_git(&self, change_id: &ChangeId) -> Option<ObjectId> {
        self.heddle_to_git.get(change_id).copied()
    }

    /// Get Heddle ChangeId for a Git object id.
    pub fn get_heddle(&self, git_oid: ObjectId) -> Option<ChangeId> {
        self.git_to_heddle.get(&git_oid).copied()
    }

    /// Check if a mapping exists for a ChangeId.
    pub fn has_heddle(&self, change_id: &ChangeId) -> bool {
        self.heddle_to_git.contains_key(change_id)
    }

    /// Drop the mapping for `change_id`, clearing both directions. Returns the
    /// Git OID that was mapped, if any.
    ///
    /// The export visibility purge calls this to remove a state whose
    /// effective tier is no longer served by the export audience. Without it,
    /// a stale ChangeId→OID mapping (minted while the state was public, kept
    /// alive by the notes/cache rebuild on the next export) makes the
    /// frontier walk and the tag/note sync treat a now-embargoed commit as
    /// served — leaking it via `refs/heads/<thread>` or a tag.
    pub(crate) fn remove(&mut self, change_id: &ChangeId) -> Option<ObjectId> {
        let git_oid = self.heddle_to_git.remove(change_id)?;
        self.git_to_heddle.remove(&git_oid);
        Some(git_oid)
    }

    /// Check if a mapping exists for a Git object id.
    pub fn has_git(&self, git_oid: ObjectId) -> bool {
        self.git_to_heddle.contains_key(&git_oid)
    }

    /// Iterate over mappings.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&ChangeId, &ObjectId)> {
        self.heddle_to_git.iter()
    }

    /// Whether the in-memory mapping holds no `ChangeId → git OID` entries. The
    /// checkout-materialization path (#568 P1) uses this to decide whether it must
    /// hydrate the mapping from disk (a standalone `bridge git checkout`) or trust
    /// the mapping export just built in memory (a checkpoint/push).
    pub(crate) fn is_empty(&self) -> bool {
        self.heddle_to_git.is_empty()
    }

    pub(crate) fn retain_git_objects(&mut self, repo: &SleyRepository) {
        let retained: Vec<(ChangeId, ObjectId)> = self
            .heddle_to_git
            .iter()
            .filter_map(|(change_id, git_oid)| {
                repo.read_object(git_oid)
                    .ok()
                    .map(|_| (*change_id, *git_oid))
            })
            .collect();

        self.heddle_to_git.clear();
        self.git_to_heddle.clear();
        for (change_id, git_oid) in retained {
            self.insert(change_id, git_oid);
        }
    }

    #[cfg_attr(not(feature = "git-overlay"), allow(dead_code))]
    pub(crate) fn retain_git_object_set(&mut self, reachable: &HashSet<ObjectId>) -> usize {
        let before = self.heddle_to_git.len();
        let retained: Vec<(ChangeId, ObjectId)> = self
            .heddle_to_git
            .iter()
            .filter(|(_, git_oid)| reachable.contains(*git_oid))
            .map(|(change_id, git_oid)| (*change_id, *git_oid))
            .collect();

        self.heddle_to_git.clear();
        self.git_to_heddle.clear();
        for (change_id, git_oid) in retained {
            self.insert(change_id, git_oid);
        }
        before.saturating_sub(self.heddle_to_git.len())
    }
}

/// Git bridge for Heddle repository.
pub struct GitBridge<'a> {
    pub(crate) heddle_repo: &'a HeddleRepository,
    pub(crate) git_repo_path: Option<PathBuf>,
    pub(crate) mapping: SyncMapping,
    pub(crate) commit_message_overrides: HashMap<ChangeId, String>,
    pub(crate) commit_parent_overrides: HashMap<ChangeId, Vec<ObjectId>>,
}

struct MappingFileSnapshot {
    path: PathBuf,
    contents: Option<Vec<u8>>,
}

impl MappingFileSnapshot {
    fn read(path: PathBuf) -> GitResult<Self> {
        let contents = match fs::read(&path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        Ok(Self { path, contents })
    }

    fn restore(self) -> GitResult<()> {
        match self.contents {
            Some(contents) => {
                if let Some(parent) = self.path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&self.path, contents)?;
            }
            None => match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            },
        }
        Ok(())
    }
}

impl<'a> GitBridge<'a> {
    /// Create a new Git bridge for a Heddle repository.
    pub fn new(heddle_repo: &'a HeddleRepository) -> Self {
        Self {
            heddle_repo,
            git_repo_path: None,
            mapping: SyncMapping::new(),
            commit_message_overrides: HashMap::new(),
            commit_parent_overrides: HashMap::new(),
        }
    }

    /// Initialize a Git mirror in the .heddle/git directory.
    pub fn init_mirror(&mut self) -> GitResult<()> {
        let _guard = self.init_mirror_with_guard()?;
        _guard.commit();
        Ok(())
    }

    /// Variant of `init_mirror` that returns a `MirrorInitGuard` so
    /// callers performing a multi-step bring-up (init + first export)
    /// can roll back the partially-created mirror if a later step
    /// fails. Call `guard.commit()` once the mirror is known-good.
    pub(crate) fn init_mirror_with_guard(&mut self) -> GitResult<MirrorInitGuard> {
        let git_dir = self.heddle_repo.heddle_dir().join("git");

        let did_create = if git_dir.exists() {
            let _ = open_repo(&git_dir)?;
            false
        } else {
            fs::create_dir_all(&git_dir)?;
            let _ = SleyRepository::init_bare(&git_dir).map_err(git_err)?;
            let mirror_repo = open_repo(&git_dir)?;
            seed_checkout_note_refs_into_mirror(self.heddle_repo.root(), &mirror_repo)?;
            true
        };

        self.git_repo_path = Some(git_dir.clone());
        Ok(MirrorInitGuard::new_from_init(git_dir, did_create))
    }

    /// Get the path to the Git mirror directory.
    pub fn mirror_path(&self) -> PathBuf {
        self.heddle_repo.heddle_dir().join("git")
    }

    /// Check if a Git mirror is initialized.
    pub fn is_initialized(&self) -> bool {
        self.mirror_path().exists()
    }

    /// Open the Git repository (mirror or regular).
    pub(crate) fn open_git_repo(&self) -> GitResult<SleyRepository> {
        if let Some(ref path) = self.git_repo_path {
            open_repo(path)
        } else {
            let mirror_path = self.mirror_path();
            if mirror_path.exists() {
                open_repo(&mirror_path)
            } else {
                open_repo(self.heddle_repo.root())
            }
        }
    }

    /// Sort states topologically (parents before children).
    pub(crate) fn sort_states_topologically(
        &self,
        states: &[ChangeId],
    ) -> GitResult<Vec<ChangeId>> {
        let mut sorted = Vec::new();
        let mut visited: std::collections::HashSet<ChangeId> = std::collections::HashSet::new();

        fn visit<S: ObjectStore + ?Sized>(
            state_id: &ChangeId,
            store: &S,
            visited: &mut std::collections::HashSet<ChangeId>,
            sorted: &mut Vec<ChangeId>,
        ) -> GitResult<()> {
            if visited.contains(state_id) {
                return Ok(());
            }

            if let Some(state) = store.get_state(state_id)? {
                for parent in &state.parents {
                    visit(parent, store, visited, sorted)?;
                }
            }

            visited.insert(*state_id);
            sorted.push(*state_id);

            Ok(())
        }

        for state_id in states {
            visit(
                state_id,
                self.heddle_repo.store(),
                &mut visited,
                &mut sorted,
            )?;
        }

        Ok(sorted)
    }

    /// Export all Heddle states to Git commits.
    pub fn export(&mut self) -> GitResult<super::git_util::ExportStats> {
        export_all(self)
    }

    pub(crate) fn set_commit_message_override(&mut self, state_id: ChangeId, message: String) {
        self.commit_message_overrides.insert(state_id, message);
    }

    pub(crate) fn set_commit_parent_override(
        &mut self,
        state_id: ChangeId,
        parents: Vec<ObjectId>,
    ) {
        self.commit_parent_overrides.insert(state_id, parents);
    }

    pub(crate) fn with_mapping_rollback<T>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> GitResult<T>,
    ) -> GitResult<T> {
        let mapping = self.mapping.clone();
        let commit_message_overrides = self.commit_message_overrides.clone();
        let commit_parent_overrides = self.commit_parent_overrides.clone();
        let mapping_file = MappingFileSnapshot::read(self.mapping_path())?;
        let mapping_tmp_file = MappingFileSnapshot::read(self.mapping_tmp_path())?;

        match operation(self) {
            Ok(value) => Ok(value),
            Err(error) => {
                self.mapping = mapping;
                self.commit_message_overrides = commit_message_overrides;
                self.commit_parent_overrides = commit_parent_overrides;
                if let Err(rollback_error) = mapping_file
                    .restore()
                    .and_then(|()| mapping_tmp_file.restore())
                {
                    return Err(GitBridgeError::Git(format!(
                        "operation failed ({error}); additionally failed to roll back git bridge mapping state ({rollback_error})"
                    )));
                }
                Err(error)
            }
        }
    }

    /// Push to a Git remote. Returns the full names of the refs written
    /// at the destination this invocation (see [`Self::push_with_scope_force`]).
    pub fn push(&mut self, remote_name: &str) -> GitResult<Vec<String>> {
        self.push_with_scope(remote_name, GitPushScope::AllThreads)
    }

    /// Push to a Git remote with an explicit ref scope. Returns the full
    /// names of the refs written at the destination this invocation.
    pub fn push_with_scope(
        &mut self,
        remote_name: &str,
        scope: GitPushScope,
    ) -> GitResult<Vec<String>> {
        self.push_with_scope_force(remote_name, scope, false)
    }

    /// Push to a Git remote with an explicit ref scope and optional
    /// non-fast-forward ref movement.
    ///
    /// Returns the full names (e.g. `refs/heads/<thread>`,
    /// `refs/notes/heddle`, `refs/tags/<tag>`) of the refs WRITTEN at the
    /// destination this invocation — creations, fast-forwards, and forced
    /// rewinds — sorted for deterministic output. A no-op push returns an
    /// empty list. Retraction deletes are not included.
    pub fn push_with_scope_force(
        &mut self,
        remote_name: &str,
        scope: GitPushScope,
        force: bool,
    ) -> GitResult<Vec<String>> {
        self.init_mirror()?;
        let current_branch = match scope {
            GitPushScope::CurrentThread => Some(self.current_attached_thread_for_push()?),
            GitPushScope::AllThreads => None,
        };
        match scope {
            GitPushScope::CurrentThread => {
                export_current_thread(self, current_branch.as_deref().expect("current branch"))?;
            }
            GitPushScope::AllThreads => {
                self.export()?;
                self.mirror_checkout_tags_for_push()?;
            }
        }
        self.write_current_checkout_from_existing_mirror()?;

        // The export step above (scoped or all-thread) has already reconciled the
        // mirror to the served frontier, so a scoped export materialized only the
        // requested thread yet still RECONCILED every out-of-scope sibling (rewound
        // an embargoed one). Both destination paths therefore reconcile against the
        // WHOLE-MIRROR served frontier — `collect_ref_updates(mirror)`, computed
        // inside each path — never a scope-filtered subset; the scope lives in the
        // mirror state, not in a second destination filter (heddle#316 r16).
        let log_message = format!("heddle: push from {}", self.heddle_repo.root().display());
        match self.resolve_remote(remote_name, RemoteDirection::Push)? {
            ResolvedRemote::Local(target_path) => self.copy_mirror_to_path(
                &target_path,
                &log_message,
                /* init_if_missing */ false,
                scope,
                current_branch.as_deref(),
                force,
            ),
            ResolvedRemote::Url(url) => {
                let mirror_repo = self.open_git_repo()?;
                push_network_remote(
                    &mirror_repo,
                    self.heddle_repo.heddle_dir(),
                    &url,
                    scope,
                    current_branch.as_deref(),
                    force,
                )
            }
        }
    }

    fn current_attached_thread_for_push(&self) -> GitResult<String> {
        let Head::Attached { thread } = self.heddle_repo.head_ref()? else {
            return Err(GitBridgeError::Git(
                "cannot push the current Git-overlay branch from a detached Heddle HEAD; use --all-threads to push all exported refs".to_string(),
            ));
        };
        if self.heddle_repo.refs().get_thread(&thread)?.is_none() {
            return Err(GitBridgeError::Git(format!(
                "attached thread '{thread}' has no state to push"
            )));
        }
        Ok(thread.to_string())
    }

    /// Export current Heddle state into the internal mirror, then write it out
    /// as a bare git repository at `target_path`. Auto-initializes
    /// `target_path` as a bare repo if it does not already exist.
    pub fn export_to_path(
        &mut self,
        target_path: &Path,
    ) -> GitResult<super::git_util::ExportStats> {
        self.init_mirror()?;
        let stats = self.export()?;
        self.copy_mirror_to_path(
            target_path,
            &format!("heddle: export from {}", self.heddle_repo.root().display()),
            /* init_if_missing */ true,
            GitPushScope::AllThreads,
            /* current_branch */ None,
            /* force */ false,
        )?;
        Ok(stats)
    }

    /// Shared helper: copy every reachable object from the internal mirror to
    /// `target_path`, then reconcile its branch/tag/note refs to the WHOLE-MIRROR
    /// served frontier. When `init_if_missing` is true, the destination is created
    /// as a bare repo when it does not exist. `scope`/`current_branch` gate only
    /// MATERIALIZATION (a scoped push never publishes a brand-new sibling); `force`
    /// authorizes retracting an out-of-band destination tip and forcing a true fork.
    ///
    /// Returns the sorted full names of the refs written at the destination.
    fn copy_mirror_to_path(
        &mut self,
        target_path: &Path,
        log_message: &str,
        init_if_missing: bool,
        scope: GitPushScope,
        current_branch: Option<&str>,
        force: bool,
    ) -> GitResult<Vec<String>> {
        let mirror_repo = self.open_git_repo()?;
        let target_repo = if target_path.exists() {
            open_repo(target_path)?
        } else if init_if_missing {
            fs::create_dir_all(target_path)?;
            SleyRepository::init_bare(target_path).map_err(git_err)?;
            open_repo(target_path)?
        } else {
            return Err(GitBridgeError::Git(format!(
                "destination '{}' does not exist",
                target_path.display()
            )));
        };

        // The WHOLE-MIRROR served frontier — the SAME projection the mirror
        // reconcile materialized (heddle#316 r14/r16). It drives BOTH the object
        // transfer AND the destination ref reconcile, so a scoped push reconciles
        // the destination against the whole served frontier rather than a
        // scope-filtered subset: an out-of-scope ref the mirror rewound for
        // embargo propagates to the destination by construction, never kept at its
        // old (embargoed) tip.
        //
        // Sourced from the MANAGED-filtered ref set (heddle#316): a foreign
        // branch/tag heddle never wrote — even one at a heddle-minted commit —
        // must NOT enter the served frontier nor the destination's desired set.
        // Ownership is name-keyed via the mirror's managed-refs record, the
        // mirror-side analog of the destination's exported-refs record.
        let managed_record = read_mirror_managed_refs(&mirror_repo)?;
        let served_frontier = collect_managed_ref_updates(&mirror_repo, &managed_record)?;
        copy_reachable_objects(
            &mirror_repo,
            &target_repo,
            served_frontier.iter().map(|update| update.target),
        )?;

        // The ONE served-frontier reconciliation, shared with the URL/network
        // push path (heddle#316 r11). It writes survivors — FORCING a deliberate
        // embargo rewind past the FF guard (a prior tip lagged down to its served
        // ancestor) while still rejecting a true fork — AND deletes the refs
        // heddle previously exported here that the served mirror no longer
        // carries (retraction), leaving foreign refs heddle never exported
        // untouched.
        let creatable = creatable_ref_names(&served_frontier, scope, current_branch);
        let old_at_destination = read_destination_ref_map(&target_repo)?;
        let previously_exported = read_exported_refs(&target_repo)?;
        let plan = plan_destination_reconcile(
            &mirror_repo,
            &served_frontier,
            creatable.as_ref(),
            &old_at_destination,
            &previously_exported,
            force,
        )?;
        for write in &plan.writes {
            let constraint = match write.old {
                Some(old) => RefPrecondition::MustExistAndMatch(ReferenceTarget::Direct(old)),
                None => RefPrecondition::MustNotExist,
            };
            set_reference(
                &target_repo,
                &write.full_name,
                write.new,
                constraint,
                log_message,
            )?;
        }
        for delete in &plan.deletes {
            delete_reference_matching(&target_repo, &delete.full_name, delete.old)?;
        }
        write_exported_refs(&target_repo, &plan.new_manifest)?;
        Ok(planned_write_names(&plan))
    }

    /// Fetch Git refs and objects into the internal mirror without moving
    /// Heddle thread refs or the current worktree.
    pub fn fetch(&mut self, remote_name: &str) -> GitResult<()> {
        self.fetch_with_scope(
            remote_name,
            GitFetchScope::BranchesAndNotes,
            RefreshCheckoutAfterFetch::Yes,
        )
    }

    fn fetch_with_scope(
        &mut self,
        remote_name: &str,
        scope: GitFetchScope,
        refresh_checkout: RefreshCheckoutAfterFetch,
    ) -> GitResult<()> {
        reject_reserved_git_remote_name(remote_name)?;
        self.init_mirror()?;
        let current_branch = self.heddle_repo.git_overlay_current_branch()?;
        let tracking_remote = checkout_tracking_remote_name(self.heddle_repo.root(), remote_name)?
            .or_else(|| {
                (!looks_like_remote_location(remote_name)).then(|| remote_name.to_string())
            });
        // A URL/path remote can still resolve onto a configured remote literally
        // named `git`; reject that here too so the constructed tracking refs
        // never land under the reserved namespace.
        if let Some(tracking_remote) = tracking_remote.as_deref() {
            reject_reserved_git_remote_name(tracking_remote)?;
        }

        let mirror_repo = self.open_git_repo()?;
        match self.resolve_remote(remote_name, RemoteDirection::Fetch)? {
            ResolvedRemote::Local(path) => {
                let remote_repo = open_repo(&path)?;
                let updates = collect_ref_updates_for_fetch(&remote_repo, scope)?;
                tracing::debug!(
                    remote = remote_name,
                    path = %path.display(),
                    refs = updates.len(),
                    notes = updates
                        .iter()
                        .filter(|update| update.namespace == RefNamespace::Note)
                        .count(),
                    "fetching Git refs from local remote"
                );
                copy_reachable_objects(
                    &remote_repo,
                    &mirror_repo,
                    updates.iter().map(|update| update.target),
                )?;
                apply_ref_updates(
                    &mirror_repo,
                    &updates,
                    &format!("heddle: fetch from {remote_name}"),
                )?;
                if let Some(tracking_remote) = tracking_remote.as_deref() {
                    apply_remote_tracking_ref_updates(
                        &mirror_repo,
                        tracking_remote,
                        &updates,
                        &format!("heddle: fetch from {remote_name}"),
                    )?;
                }
            }
            ResolvedRemote::Url(url) => {
                fetch_network_remote(&mirror_repo, remote_name, &url, scope)?;
                let updates = collect_ref_updates_for_fetch(&mirror_repo, scope)?;
                if let Some(tracking_remote) = tracking_remote.as_deref() {
                    apply_remote_tracking_ref_updates(
                        &mirror_repo,
                        tracking_remote,
                        &updates,
                        &format!("heddle: fetch from {remote_name}"),
                    )?;
                }
            }
        }

        self.git_repo_path = Some(self.mirror_path());
        if matches!(refresh_checkout, RefreshCheckoutAfterFetch::Yes) {
            if let Some(tracking_remote) = tracking_remote.as_deref() {
                self.refresh_checkout_remote_tracking_refs(tracking_remote)?;
            }
            if let Some(branch) = current_branch {
                self.refresh_checkout_remote_tracking_ref(remote_name, &branch)?;
            }
            self.refresh_checkout_note_refs_from_mirror()?;
        }
        Ok(())
    }

    /// Best-effort adoption preflight for the ingest-backed path.
    ///
    /// Plain Git clones do not fetch `refs/notes/heddle` by default, but
    /// Heddle-pushed overlay remotes use that ref to preserve Git commit
    /// -> Heddle state identity. Ingest reads directly from the checkout, so
    /// it only needs `refs/notes/heddle` hydrated in the checkout's own object
    /// database before `GitSource` opens the repository.
    pub(crate) fn hydrate_checkout_heddle_notes_without_mirror(root: &Path) -> bool {
        if checkout_note_ref_exists(root).unwrap_or(false) {
            return true;
        }

        let mut remotes = match checkout_remote_url_items(root) {
            Ok(remotes) => remotes
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    "skipping configured remote note hydration before ingest-backed adopt"
                );
                return false;
            }
        };
        remotes.sort_by(|left, right| {
            match (left.as_str() == "origin", right.as_str() == "origin") {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => left.cmp(right),
            }
        });
        remotes.dedup();

        for remote in remotes {
            match hydrate_checkout_notes_from_remote_without_mirror(root, &remote) {
                Ok(()) if checkout_note_ref_exists(root).unwrap_or(false) => return true,
                Ok(()) => {}
                Err(error) => {
                    tracing::debug!(
                        remote = remote.as_str(),
                        error = %error,
                        "configured remote did not provide Heddle notes during ingest-backed adopt"
                    );
                }
            }
        }

        false
    }

    /// Pull from a Git remote.
    pub fn pull(&mut self, remote_name: &str) -> GitResult<GitPullOutcome> {
        let head_before = self.heddle_repo.refs().read_head()?;
        let attached_before = match &head_before {
            Head::Attached { thread } => self
                .heddle_repo
                .refs()
                .get_thread(thread)?
                .map(|state| (thread.to_string(), state)),
            Head::Detached { .. } => None,
        };
        let attached_thread = attached_before.as_ref().map(|(thread, _)| thread.clone());

        self.fetch_with_scope(
            remote_name,
            GitFetchScope::AllRefs,
            RefreshCheckoutAfterFetch::No,
        )?;
        if self.preflight_attached_pull_fast_forward(remote_name, attached_before.as_ref())?
            == PullPreflight::UpToDate
        {
            if let Some(thread) = attached_thread {
                self.refresh_checkout_remote_tracking_ref(remote_name, &thread)?;
            }
            self.refresh_checkout_note_refs_from_mirror()?;
            return Ok(GitPullOutcome::default());
        }
        let mirror_path = self.mirror_path();
        let stats = import_git_history(self, Some(&mirror_path), &[], Default::default(), None)?;

        let mut materialized_attached_thread = false;
        if let Some((thread, old_state)) = attached_before
            && let Some(new_state) = self
                .heddle_repo
                .refs()
                .get_thread(&ThreadName::new(&thread))?
            && new_state != old_state
        {
            self.heddle_repo
                .refs()
                .set_thread(&ThreadName::new(&thread), &old_state)?;
            self.heddle_repo.refs().write_head(&Head::Attached {
                thread: ThreadName::new(&thread),
            })?;
            self.heddle_repo
                .goto_verified_clean_without_record(&new_state)?;
            self.heddle_repo
                .refs()
                .set_thread(&ThreadName::new(&thread), &new_state)?;
            self.heddle_repo.refs().write_head(&Head::Attached {
                thread: ThreadName::new(&thread),
            })?;
            materialized_attached_thread = true;
        }

        if materialized_attached_thread {
            self.write_current_checkout_from_existing_mirror()?;
        }
        if let Some(thread) = attached_thread {
            self.refresh_checkout_remote_tracking_ref(remote_name, &thread)?;
        }
        self.refresh_checkout_note_refs_from_mirror()?;
        Ok(pull_outcome(&stats, materialized_attached_thread))
    }

    fn preflight_attached_pull_fast_forward(
        &mut self,
        remote_name: &str,
        attached_before: Option<&(String, ChangeId)>,
    ) -> GitResult<PullPreflight> {
        let Some((thread, state_id)) = attached_before else {
            return Ok(PullPreflight::ImportRequired);
        };
        self.build_existing_mapping(None)?;
        let Some(local_git_oid) = self.mapping.get_git(state_id) else {
            return Ok(PullPreflight::ImportRequired);
        };
        let mirror_repo = self.open_git_repo()?;
        let branch_ref = format!("refs/heads/{thread}");
        let Some(reference) = mirror_repo.find_reference(&branch_ref).map_err(git_err)? else {
            return Ok(PullPreflight::ImportRequired);
        };
        let Some(remote_git_oid) = reference.peeled_oid(&mirror_repo).map_err(git_err)? else {
            return Ok(PullPreflight::ImportRequired);
        };
        if remote_git_oid == local_git_oid {
            return Ok(PullPreflight::UpToDate);
        }
        if commit_is_descendant_of(&mirror_repo, remote_git_oid, local_git_oid)? {
            return Ok(PullPreflight::ImportRequired);
        }
        Err(GitBridgeError::RemoteDiverged {
            branch: thread.clone(),
            upstream: format!("{remote_name}/{thread}"),
            local: local_git_oid,
            remote: remote_git_oid,
        })
    }

    fn mirror_checkout_tags_for_push(&self) -> GitResult<()> {
        if !self.heddle_repo.root().join(".git").exists() {
            return Ok(());
        }

        let mirror_repo = self.open_git_repo()?;
        let checkout_repo = SleyRepository::discover(self.heddle_repo.root()).map_err(git_err)?;
        if checkout_repo.git_dir() == mirror_repo.git_dir() {
            return Ok(());
        }
        let object_repo = common_repo_for_worktree(&checkout_repo)?;
        let tag_updates = collect_ref_updates(&object_repo)?
            .into_iter()
            .filter(|update| update.namespace == RefNamespace::Tag)
            .collect::<Vec<_>>();
        if tag_updates.is_empty() {
            return Ok(());
        }

        copy_reachable_objects(
            &object_repo,
            &mirror_repo,
            tag_updates.iter().map(|u| u.target),
        )?;
        apply_ref_updates(
            &mirror_repo,
            &tag_updates,
            "heddle: mirror checkout tags before push",
        )?;
        // Claim the raw checkout tags as heddle-managed in the mirror record so
        // the managed-filtered push frontier includes them — an all-threads push
        // publishes the user's checkout tags on their behalf. This runs AFTER the
        // export reconcile (which has no marker for a raw checkout tag and would
        // drop it), so each push re-applies + re-claims them; the net effect
        // matches the pre-record behavior where the push copied every mirror ref
        // (heddle#316).
        let mut record = read_mirror_managed_refs(&mirror_repo)?;
        for update in &tag_updates {
            record.insert(full_ref_name(update), update.target);
        }
        write_mirror_managed_refs(&mirror_repo, &record)?;
        Ok(())
    }

    pub(crate) fn seed_git_checkpoint_mappings_from_checkout(
        &mut self,
        mirror_repo: &SleyRepository,
    ) -> GitResult<()> {
        if !self.heddle_repo.root().join(".git").exists() {
            return Ok(());
        }

        let checkout_repo = match SleyRepository::discover(self.heddle_repo.root()) {
            Ok(repo) => repo,
            Err(_) => return Ok(()),
        };
        if checkout_repo.git_dir() == mirror_repo.git_dir() {
            return Ok(());
        }
        let object_repo = common_repo_for_worktree(&checkout_repo)?;

        for record in self.heddle_repo.list_git_checkpoints()? {
            let change_id = ChangeId::parse(&record.change_id)?;
            let git_oid = record
                .git_commit
                .parse::<ObjectId>()
                .map_err(|err| GitBridgeError::InvalidMapping(err.to_string()))?;

            if mirror_repo.read_object(&git_oid).is_err() {
                copy_reachable_objects(&object_repo, mirror_repo, [git_oid])?;
            }
            mirror_repo
                .read_object(&git_oid)
                .map_err(|_| GitBridgeError::CommitNotFound(record.git_commit.clone()))?;

            self.mapping.insert(change_id, git_oid);
            // Only publish a note for a state served to the public mirror.
            // `collect_ref_updates` copies `refs/notes/*`, so writing a note for
            // a now-embargoed checkpoint here would leak that commit's metadata
            // even though no branch/tag serves it. `export_scoped`'s
            // purge+retract closes this for the all-states export, but a scoped
            // export never examines an out-of-thread checkpoint — so gate the
            // note at its source, symmetric with `export_state`'s minting gate
            // (heddle#316). The Git bridge always publishes the Public mirror.
            let tier = self
                .heddle_repo
                .effective_visibility_tier(&change_id)
                .map_err(|e| {
                    GitBridgeError::Git(format!("resolve visibility for {change_id}: {e:#}"))
                })?;
            if repo::visible(&tier, &repo::AudienceTier::Public)
                && super::git_notes::read_note(mirror_repo, git_oid)?.is_none()
                && let Some(state) = self.heddle_repo.store().get_state(&change_id)?
            {
                let note = super::git_notes::HeddleNote::from_state(&state);
                super::git_notes::write_note(mirror_repo, git_oid, &note)?;
            }
        }

        Ok(())
    }

    pub(crate) fn stage_ingest_source_in_mirror(
        &mut self,
        source: &Path,
        refs: &[String],
    ) -> GitResult<()> {
        let source_repo = open_repo(source)?;
        let updates = collect_import_source_ref_updates(&source_repo, refs)?;
        if updates.is_empty() {
            return Ok(());
        }

        self.init_mirror()?;
        let mirror_repo = self.open_git_repo()?;
        copy_reachable_objects(
            &source_repo,
            &mirror_repo,
            updates.iter().map(|update| update.target),
        )?;
        apply_ref_updates(
            &mirror_repo,
            &updates,
            &format!("heddle: stage ingest source from {}", source.display()),
        )?;

        let mut record = read_or_seed_mirror_managed_refs(&mirror_repo)?;
        for update in &updates {
            record.insert(full_ref_name(update), update.target);
        }
        write_mirror_managed_refs(&mirror_repo, &record)?;
        Ok(())
    }

    /// Make the checkout's real `.git` view agree with the current Heddle
    /// thread: copy exported objects from the internal mirror, advance the
    /// matching Git branch, attach HEAD, and rebuild the Git index from the
    /// exported commit tree.
    pub fn write_through_current_checkout(&mut self) -> GitResult<WriteThroughOutcome> {
        if !self.heddle_repo.root().join(".git").exists() {
            return Ok(WriteThroughOutcome::Skipped(
                WriteThroughSkipReason::MissingDotGit,
            ));
        }
        if checkout_git_head_is_detached(self.heddle_repo.root())? {
            return Ok(WriteThroughOutcome::Skipped(
                WriteThroughSkipReason::DetachedHead,
            ));
        }
        let Head::Attached { thread } = self.heddle_repo.head_ref()? else {
            return Ok(WriteThroughOutcome::Skipped(
                WriteThroughSkipReason::DetachedHead,
            ));
        };

        let mirror_guard = self.init_mirror_with_guard()?;
        // First export against a freshly-initialized mirror runs while
        // the guard is still armed; if export fails we want the
        // half-built `.heddle/git/` cleared so the next caller doesn't
        // see a corrupt bare repo.
        //
        // Checkpoint/commit write-through is intentionally scoped to the
        // attached thread. Moving every Git branch during an everyday save
        // surprised Git users and made stale isolated threads fail while
        // checkpointing unrelated work. Full export remains explicit via
        // bridge export or push-all.
        export_current_thread(self, &thread)?;
        // Mirror is committed to disk (objects + refs) in a known-good
        // shape; remaining failures only affect the user's checkout
        // and have their own per-file rollback below.
        mirror_guard.commit();
        self.write_thread_checkout_from_existing_mirror(&thread)
    }

    pub fn write_through_current_checkout_with_message(
        &mut self,
        state_id: ChangeId,
        message: String,
    ) -> GitResult<WriteThroughOutcome> {
        self.set_commit_message_override(state_id, message);
        self.write_through_current_checkout()
    }

    /// Mark files that Heddle has captured but that Git still sees as
    /// untracked as `intent-to-add` in the colocated checkout's index,
    /// so a colocated developer's `git status` shows `AM new_file`
    /// ("Heddle knows about it; no Git blob committed yet") instead of
    /// `?? new_file` ("untracked — Git knows nothing"). The placeholder
    /// entry uses the empty-blob oid and a zeroed stat, so Git always
    /// reports the working-tree content as modified-against-index.
    ///
    /// Ported from jujutsu's `update_intent_to_add` (`lib/src/git.rs`),
    /// which diffs `old_tree` vs `new_tree` and flags paths present in
    /// the new tree but absent from the old one. Here `new_tree` is the
    /// just-captured Heddle state's tree and `old_tree` is whatever the
    /// checkout's index already tracks — paths already in the index are
    /// not `??`, so they are left untouched (no spurious marking of
    /// tracked or unchanged files).
    ///
    /// Call frequency mirrors jj: this fires at a Heddle parent/state
    /// change (`capture`), not on every command. A later `checkpoint`
    /// rebuilds the index from the committed tree via
    /// [`Self::write_through_current_checkout`], replacing these
    /// placeholder entries with real ones — so the index is never
    /// churned by read-only invocations.
    pub fn update_intent_to_add(&self, state_id: &ChangeId) -> GitResult<()> {
        let root = self.heddle_repo.root();
        if !root.join(".git").exists() {
            return Ok(());
        }
        let checkout_repo = SleyRepository::discover(root).map_err(git_err)?;
        // Skip detached HEAD: write-through only mirrors attached
        // threads, and there is no branch context to reason about here.
        if checkout_repo
            .head()
            .map(|head| head.is_detached())
            .unwrap_or(false)
        {
            return Ok(());
        }

        // `new_tree`: every file the just-captured state contains.
        let Some(state) = self.heddle_repo.store().get_state(state_id)? else {
            return Ok(());
        };
        let Some(tree) = self.heddle_repo.store().get_tree(&state.tree)? else {
            return Ok(());
        };
        let mut captured: Vec<(String, FileMode)> = Vec::new();
        collect_capture_paths(self.heddle_repo.store(), &tree, "", &mut captured)?;
        // No early return on an empty captured set: the reconcile below must
        // run on EVERY recapture path. When the recaptured state is empty,
        // `captured_paths` is empty too, so the PRUNE pass clears every prior
        // intent-to-add entry (all are now stale) and the ADD loop is a no-op.

        // Reconcile the index's intent-to-add set against the captured
        // state. Real (committed) entries are left untouched; the
        // intent-to-add set must end up equal to the captured paths that
        // are not yet real entries. So we both ADD newly-captured paths
        // and PRUNE intent-to-add entries whose path left the captured
        // set (deleted, or now committed) — otherwise a stale entry
        // surfaces as a phantom ` D path` in `git status`.
        let mut index = checkout_repo
            .open_index()
            .map_err(git_err)?
            .unwrap_or_else(|| Index {
                version: 2,
                entries: Vec::new(),
                extensions: Vec::new(),
                checksum: None,
            });

        // Partition existing entries: real tracked paths vs. the
        // intent-to-add placeholders we manage here.
        let mut real_tracked: HashSet<String> = HashSet::new();
        let mut existing_ita: HashSet<String> = HashSet::new();
        for entry in &index.entries {
            let path = String::from_utf8_lossy(entry.path.as_bytes()).into_owned();
            if entry.is_intent_to_add() {
                existing_ita.insert(path);
            } else {
                real_tracked.insert(path);
            }
        }

        // Desired intent-to-add set: captured paths not backed by a real
        // (committed) index entry.
        let captured_paths: HashSet<&str> = captured.iter().map(|(p, _)| p.as_str()).collect();

        // PRUNE: any intent-to-add entry whose path is no longer desired.
        let before_prune = index.entries.len();
        index.entries.retain(|entry| {
            !entry.is_intent_to_add()
                || captured_paths.contains(String::from_utf8_lossy(entry.path.as_bytes()).as_ref())
        });
        let mut changed = index.entries.len() != before_prune;

        // ADD: newly-captured paths not already tracked or marked.
        for (path, mode) in &captured {
            if real_tracked.contains(path) || existing_ita.contains(path) {
                continue;
            }
            // Git's index cannot hold both a blob `foo` and a blob
            // `foo/bar` — a path is either a file or a directory. An
            // added path that file↔directory-PREFIX-conflicts with a
            // still-tracked real entry is not a clean "new file": the
            // real entry wins. Writing an intent-to-add placeholder for
            // it would corrupt the index into a file/dir conflict, so
            // skip it (checked in both directions).
            if real_tracked
                .iter()
                .any(|tracked| path_prefix_conflict(path, tracked))
            {
                continue;
            }
            let mut entry = IndexEntry::intent_to_add(
                checkout_repo.object_format(),
                GitBString::from(path.as_str()),
            );
            entry.mode = match mode {
                FileMode::Executable => 0o100755,
                FileMode::Symlink => 0o120000,
                FileMode::Normal => 0o100644,
            };
            changed = true;
            index.entries.push(entry);
        }

        if changed {
            index
                .entries
                .sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
            index.upgrade_version_for_flags();
            checkout_repo
                .write_index(
                    &index,
                    IndexWriteOptions {
                        fsync: true,
                        validate_checksum: true,
                    },
                )
                .map_err(git_err)?;
        }
        Ok(())
    }

    /// Make the checkout's real `.git` view agree with a specific Heddle
    /// thread. `thread switch` uses this after writing Heddle HEAD because
    /// resolving "current" through Git-overlay discovery can still see the
    /// branch that was active before the switch.
    pub fn write_through_thread_checkout(
        &mut self,
        thread: &str,
    ) -> GitResult<WriteThroughOutcome> {
        if !self.heddle_repo.root().join(".git").exists() {
            return Ok(WriteThroughOutcome::Skipped(
                WriteThroughSkipReason::MissingDotGit,
            ));
        }

        let mirror_guard = self.init_mirror_with_guard()?;
        export_current_thread(self, thread)?;
        mirror_guard.commit();
        self.write_thread_checkout_from_existing_mirror(thread)
    }

    pub(crate) fn write_current_checkout_from_existing_mirror(
        &mut self,
    ) -> GitResult<WriteThroughOutcome> {
        if !self.heddle_repo.root().join(".git").exists() {
            return Ok(WriteThroughOutcome::Skipped(
                WriteThroughSkipReason::MissingDotGit,
            ));
        }

        let (thread, state_id) = match self.heddle_repo.head_ref()? {
            Head::Attached { thread } => {
                let Some(state_id) = self.heddle_repo.refs().get_thread(&thread)? else {
                    return Ok(WriteThroughOutcome::Skipped(
                        WriteThroughSkipReason::NoAttachedThread,
                    ));
                };
                (thread, state_id)
            }
            Head::Detached { .. } => {
                return Ok(WriteThroughOutcome::Skipped(
                    WriteThroughSkipReason::DetachedHead,
                ));
            }
        };
        self.write_thread_state_checkout_from_existing_mirror(&thread, &state_id)
    }

    fn write_thread_checkout_from_existing_mirror(
        &mut self,
        thread: &str,
    ) -> GitResult<WriteThroughOutcome> {
        let Some(state_id) = self
            .heddle_repo
            .refs()
            .get_thread(&ThreadName::new(thread))?
        else {
            return Ok(WriteThroughOutcome::Skipped(
                WriteThroughSkipReason::NoAttachedThread,
            ));
        };
        self.write_thread_state_checkout_from_existing_mirror(thread, &state_id)
    }

    fn write_thread_state_checkout_from_existing_mirror(
        &mut self,
        thread: &str,
        state_id: &ChangeId,
    ) -> GitResult<WriteThroughOutcome> {
        let mirror_repo = self.open_git_repo()?;
        // Reconstructing a faithful commit from state (#568 P1) resolves each
        // parent's git OID through the bridge mapping. A checkpoint/push runs
        // export first, which leaves the in-memory mapping populated for the
        // served set — trust it, and do NOT re-read from disk (notes vs sidecar
        // can legitimately disagree mid-operation, e.g. a `--git-commit` merge
        // checkpoint that has not yet flushed; clobbering the freshly-built
        // mapping with a disk read trips the conflict guard). Only a STANDALONE
        // checkout (`bridge git checkout`, no preceding export) starts with an
        // empty mapping; hydrate it from disk in that case alone.
        if self.mapping.is_empty() {
            self.build_existing_mapping(None)?;
        }
        let git_oid = if let Some(git_oid) = self.mapping.get_git(state_id) {
            git_oid
        } else if let Some(git_commit) = self
            .heddle_repo
            .git_overlay_mapped_git_commit_for_change(state_id)
            .map_err(|error| GitBridgeError::Git(error.to_string()))?
        {
            ObjectId::from_hex(mirror_repo.object_format(), &git_commit)
                .map_err(|error| GitBridgeError::InvalidMapping(error.to_string()))?
        } else {
            return Ok(WriteThroughOutcome::Skipped(
                WriteThroughSkipReason::NoMappedCommit,
            ));
        };

        let checkout_repo = SleyRepository::discover(self.heddle_repo.root()).map_err(git_err)?;
        if checkout_repo.git_dir() == mirror_repo.git_dir() {
            return Ok(WriteThroughOutcome::Skipped(
                WriteThroughSkipReason::MirrorIsWorktree,
            ));
        }
        let git_dir = checkout_repo.git_dir().to_path_buf();
        // sley's index writer owns `index.lock`; keep this preflight so a stale
        // or concurrent lock surfaces as a structured `IndexAlreadyDirty` skip.
        if git_dir.join("index.lock").exists() {
            return Ok(WriteThroughOutcome::Skipped(
                WriteThroughSkipReason::IndexAlreadyDirty,
            ));
        }

        let object_repo = common_repo_for_worktree(&checkout_repo)?;
        let branch_ref = format!("refs/heads/{thread}");
        let head_path = git_dir.join("HEAD");
        let index_path = git_dir.join("index");
        let previous_head = fs::read(&head_path).ok();
        let previous_index = fs::read(&index_path).ok();
        let previous_branch = object_repo
            .find_reference(&branch_ref)
            .ok()
            .flatten()
            .and_then(|reference| reference.peeled_oid(&object_repo).ok().flatten());

        let heddle_repo = self.heddle_repo;
        let mapping = &self.mapping;
        let write_result = (|| -> GitResult<()> {
            // Incremental object materialization (perf): bringing the new commit's
            // full reachable closure into the checkout re-walks the ENTIRE tree
            // every checkpoint — ~115s of the ~140s on the ~6k-object ghostty tree,
            // scaling with total history rather than the change. But the checkout
            // already holds the prior HEAD (`previous_branch`) and its whole
            // closure. So exclude that closure: only objects genuinely new since
            // the parent are reconstructed/copied. Excluding the parent COMMIT
            // alone is not enough — the new commit's tree re-reaches the parent's
            // unchanged trees/blobs, so they would not be pruned. Compute the
            // parent's FULL closure from the DESTINATION (cheap: those objects are
            // local and already packed) and exclude all of it. Byte-identical
            // result — every pruned object was already present in the checkout.
            // First checkpoint on a thread has no previous branch, so the exclude
            // set is empty (full materialization).
            let excluded: HashSet<ObjectId> = match previous_branch {
                Some(parent) => sley::plumbing::sley_odb::collect_reachable_object_ids(
                    object_repo.objects().as_ref(),
                    object_repo.object_format(),
                    [parent],
                )
                .map_err(|error| GitBridgeError::Git(error.to_string()))?,
                None => HashSet::new(),
            };
            // #568 P1: materialize the checkout from heddle state, NOT by copying
            // the eager `.heddle/git` mirror's verbatim objects. Each byte-faithful
            // commit's object closure is reconstructed directly into the checkout
            // `object_repo`; the mirror is consulted only for the lossy residual
            // (commits whose original bytes can't be re-derived). This is the
            // strategic flip — heddle-native store feeds the worktree, git is a
            // derived projection — with a per-commit fallback so nothing is lost.
            materialize_checkout_closure_from_state(
                heddle_repo,
                mapping,
                &mirror_repo,
                &object_repo,
                state_id,
                git_oid,
                &excluded,
            )?;
            // Atomic temp+rename so a torn write can't leave HEAD in a
            // self-inconsistent state mid-write-through (the rollback
            // path below restores previous_head on any later failure).
            write_head_symref(&git_dir, &branch_ref)?;

            let commit = object_repo.read_commit(&git_oid).map_err(git_err)?;
            let mut index = object_repo.index_from_tree(&commit.tree).map_err(git_err)?;
            index.upgrade_version_for_flags();
            checkout_repo
                .write_index(
                    &index,
                    IndexWriteOptions {
                        fsync: true,
                        validate_checksum: true,
                    },
                )
                .map_err(git_err)?;

            update_checkout_head_ref(
                &checkout_repo,
                git_oid,
                previous_branch,
                "heddle: write-through current thread",
            )?;

            // fsync after every durable write so a power loss between
            // `fs::write(HEAD)` and `write_index` doesn't leave the
            // checkout in a self-inconsistent state. Sync the parent
            // dir too — file-level fsync on its own doesn't durably
            // commit the dirent on most filesystems.
            fsync_path(&head_path)?;
            fsync_path(&index_path)?;
            fsync_path(&git_dir)?;
            Ok(())
        })();

        if let Err(err) = write_result {
            restore_file(head_path.clone(), previous_head.as_deref())?;
            restore_file(index_path.clone(), previous_index.as_deref())?;
            if let Some(previous_branch) = previous_branch {
                set_reference(
                    &object_repo,
                    &branch_ref,
                    previous_branch,
                    RefPrecondition::Any,
                    "heddle: rollback failed write-through",
                )?;
            } else {
                // The branch did not exist before write-through. If
                // `set_reference` (or anything after it — notes mirror,
                // fsync) created the new branch and *then* the write
                // failed, the rollback used to leave that branch
                // behind, so callers saw an error but Git still showed
                // the new ref. Delete it so the failure is actually
                // reverted. Best-effort: a missing ref here means the
                // failure happened before set_reference ran, which is
                // already the correct rolled-back state.
                let _ = delete_reference_if_present(&object_repo, &branch_ref);
            }
            // fsync the rollback so the recovered files are durable
            // even if the caller crashes immediately after.
            let _ = fsync_path(&head_path);
            let _ = fsync_path(&index_path);
            let _ = fsync_path(&git_dir);
            return Err(err);
        }

        Ok(WriteThroughOutcome::Wrote(git_oid))
    }

    fn refresh_checkout_remote_tracking_ref(
        &self,
        remote_name: &str,
        branch: &str,
    ) -> GitResult<()> {
        if !self.heddle_repo.root().join(".git").exists() {
            return Ok(());
        }
        let Some(tracking_remote) =
            checkout_tracking_remote_name(self.heddle_repo.root(), remote_name)?
        else {
            return Ok(());
        };
        reject_reserved_git_remote_name(&tracking_remote)?;

        let mirror_repo = self.open_git_repo()?;
        let branch_ref = format!("refs/heads/{branch}");
        let Some(reference) = mirror_repo.find_reference(&branch_ref).map_err(git_err)? else {
            return Ok(());
        };
        let Some(target) = reference.peeled_oid(&mirror_repo).map_err(git_err)? else {
            return Ok(());
        };

        let checkout_repo = SleyRepository::discover(self.heddle_repo.root()).map_err(git_err)?;
        if checkout_repo.git_dir() == mirror_repo.git_dir() {
            return Ok(());
        }
        let object_repo = common_repo_for_worktree(&checkout_repo)?;
        copy_reachable_objects(&mirror_repo, &object_repo, [target])?;
        set_reference(
            &object_repo,
            &format!("refs/remotes/{tracking_remote}/{branch}"),
            target,
            RefPrecondition::Any,
            "heddle: refresh remote-tracking branch after pull",
        )?;
        Ok(())
    }

    fn refresh_checkout_remote_tracking_refs(&self, remote_name: &str) -> GitResult<()> {
        if !self.heddle_repo.root().join(".git").exists() {
            return Ok(());
        }
        let Some(tracking_remote) =
            checkout_tracking_remote_name(self.heddle_repo.root(), remote_name)?
        else {
            return Ok(());
        };
        reject_reserved_git_remote_name(&tracking_remote)?;

        let mirror_repo = self.open_git_repo()?;
        let checkout_repo = SleyRepository::discover(self.heddle_repo.root()).map_err(git_err)?;
        if checkout_repo.git_dir() == mirror_repo.git_dir() {
            return Ok(());
        }
        let object_repo = common_repo_for_worktree(&checkout_repo)?;
        let prefix = format!("refs/remotes/{remote_name}/");
        for reference in mirror_repo.references().list_refs().map_err(git_err)? {
            if !reference.name.starts_with(&prefix) {
                continue;
            }
            let ReferenceTarget::Direct(target) = reference.target else {
                continue;
            };
            let full = reference.name;
            let Some(branch) = full.strip_prefix(&prefix) else {
                continue;
            };
            if branch.ends_with("/HEAD") {
                continue;
            }
            copy_reachable_objects(&mirror_repo, &object_repo, [target])?;
            set_reference(
                &object_repo,
                &format!("refs/remotes/{tracking_remote}/{branch}"),
                target,
                RefPrecondition::Any,
                "heddle: refresh remote-tracking branch after fetch",
            )?;
        }
        Ok(())
    }

    fn refresh_checkout_note_refs_from_mirror(&self) -> GitResult<()> {
        if !self.heddle_repo.root().join(".git").exists() {
            return Ok(());
        }

        let mirror_repo = self.open_git_repo()?;
        let checkout_repo = SleyRepository::discover(self.heddle_repo.root()).map_err(git_err)?;
        if checkout_repo.git_dir() == mirror_repo.git_dir() {
            return Ok(());
        }
        let object_repo = common_repo_for_worktree(&checkout_repo)?;
        let note_updates = collect_ref_updates(&mirror_repo)?
            .into_iter()
            .filter(|update| update.namespace == RefNamespace::Note)
            .collect::<Vec<_>>();
        if note_updates.is_empty() {
            return Ok(());
        }

        copy_reachable_objects(
            &mirror_repo,
            &object_repo,
            note_updates.iter().map(|u| u.target),
        )?;
        apply_ref_updates(
            &object_repo,
            &note_updates,
            "heddle: refresh Heddle note refs",
        )?;
        Ok(())
    }

    fn resolve_remote(
        &self,
        remote_name: &str,
        direction: RemoteDirection,
    ) -> GitResult<ResolvedRemote> {
        let repo = self.open_git_repo()?;
        let url = match remote_url_from_repo(&repo, remote_name, direction)? {
            Some(url) => Some(url),
            None => self.checkout_remote_url(remote_name, direction)?,
        };

        let base = repo_relative_base(&repo);
        let url = match url {
            Some(url) => url,
            None => parse_configured_remote_url(remote_name, &base)?,
        };

        if let Some(path) = local_path_from_url(&url)? {
            Ok(ResolvedRemote::Local(path))
        } else {
            Ok(ResolvedRemote::Url(url))
        }
    }

    fn checkout_remote_url(
        &self,
        remote_name: &str,
        direction: RemoteDirection,
    ) -> GitResult<Option<String>> {
        if direction == RemoteDirection::Fetch
            && let Some(url) =
                remote_fetch_url_from_checkout_config(self.heddle_repo.root(), remote_name)?
        {
            return Ok(Some(url));
        }
        let Ok(repo) = SleyRepository::discover(self.heddle_repo.root()) else {
            return Ok(None);
        };
        remote_url_from_repo(&repo, remote_name, direction)
    }
}

fn remote_url_from_repo(
    repo: &SleyRepository,
    remote_name: &str,
    direction: RemoteDirection,
) -> GitResult<Option<String>> {
    let config = repo.config_snapshot().map_err(git_err)?;
    let push = direction == RemoteDirection::Push;
    let value = if push {
        config
            .get("remote", Some(remote_name), "pushurl")
            .or_else(|| config.get("remote", Some(remote_name), "url"))
    } else {
        config.get("remote", Some(remote_name), "url")
    };
    let Some(value) = value else {
        return Ok(None);
    };
    let rewritten =
        sley::plumbing::sley_config::remotes::rewrite_url_with_config(&config, value, push);
    parse_configured_remote_url(&rewritten, &repo_relative_base(repo)).map(Some)
}

fn checkout_tracking_remote_name(root: &Path, requested: &str) -> GitResult<Option<String>> {
    let remotes = checkout_remote_url_items(root)?;
    if remotes.is_empty() {
        return Ok(None);
    }
    if let Some((name, _)) = remotes.iter().find(|(name, _)| name == requested) {
        return Ok(Some(name.clone()));
    }
    if let Some((name, _)) = remotes
        .iter()
        .find(|(_, url)| configured_remote_values_match(url, requested))
    {
        return Ok(Some(name.clone()));
    }
    if looks_like_remote_location(requested) && remotes.len() == 1 {
        return Ok(Some(remotes[0].0.clone()));
    }
    if !looks_like_remote_location(requested) {
        return Ok(Some(requested.to_string()));
    }
    Ok(None)
}

fn checkout_remote_url_items(root: &Path) -> GitResult<Vec<(String, String)>> {
    let mut remotes = Vec::new();
    for config_path in checkout_git_config_paths(root) {
        parse_remote_url_items_from_config(&config_path, &mut remotes)?;
    }
    Ok(remotes)
}

fn checkout_note_ref_exists(root: &Path) -> GitResult<bool> {
    if !root.join(".git").exists() {
        return Ok(false);
    }
    let checkout_repo = SleyRepository::discover(root).map_err(git_err)?;
    let object_repo = common_repo_for_worktree(&checkout_repo)?;
    Ok(object_repo
        .find_reference(super::git_notes::NOTES_REF)
        .map_err(git_err)?
        .is_some())
}

fn seed_checkout_note_refs_into_mirror(root: &Path, mirror_repo: &SleyRepository) -> GitResult<()> {
    if !root.join(".git").exists() {
        return Ok(());
    }

    let checkout_repo = match SleyRepository::discover(root) {
        Ok(repo) => repo,
        Err(_) => return Ok(()),
    };
    if checkout_repo.git_dir() == mirror_repo.git_dir() {
        return Ok(());
    }
    let object_repo = common_repo_for_worktree(&checkout_repo)?;
    let note_updates = collect_ref_updates(&object_repo)?
        .into_iter()
        .filter(|update| update.namespace == RefNamespace::Note)
        .collect::<Vec<_>>();
    if note_updates.is_empty() {
        return Ok(());
    }

    copy_reachable_objects(
        &object_repo,
        mirror_repo,
        note_updates.iter().map(|update| update.target),
    )?;
    apply_ref_updates(
        mirror_repo,
        &note_updates,
        "heddle: seed mirror note refs from checkout",
    )
}

fn hydrate_checkout_notes_from_remote_without_mirror(
    root: &Path,
    remote_name: &str,
) -> GitResult<()> {
    reject_reserved_git_remote_name(remote_name)?;
    let checkout_repo = SleyRepository::discover(root).map_err(git_err)?;
    let object_repo = common_repo_for_worktree(&checkout_repo)?;
    let url = remote_fetch_url_from_checkout_config(root, remote_name)?
        .ok_or_else(|| GitBridgeError::Git(format!("remote '{remote_name}' has no fetch URL")))?;

    if let Some(path) = local_path_from_url(&url)? {
        let remote_repo = open_repo(&path)?;
        let note_updates = collect_ref_updates(&remote_repo)?
            .into_iter()
            .filter(|update| update.namespace == RefNamespace::Note)
            .collect::<Vec<_>>();
        if note_updates.is_empty() {
            return Ok(());
        }
        copy_reachable_objects(
            &remote_repo,
            &object_repo,
            note_updates.iter().map(|update| update.target),
        )?;
        apply_ref_updates(
            &object_repo,
            &note_updates,
            &format!("heddle: hydrate notes from {remote_name}"),
        )?;
        return Ok(());
    }

    fetch_heddle_notes_into_repo(&object_repo, remote_name, &url)
}

fn fetch_heddle_notes_into_repo(
    repo: &SleyRepository,
    remote_name: &str,
    url: &str,
) -> GitResult<()> {
    let mut credentials = NoCredentials;
    let mut progress = SilentProgress;
    let refspec = RefSpec::forced("refs/notes/*", "refs/notes/*")?.to_git_format();
    repo.fetch(
        url,
        &[refspec],
        FetchOptions {
            quiet: true,
            auto_follow_tags: false,
            fetch_all_tags: false,
            prune: false,
            dry_run: false,
            append: false,
            write_fetch_head: true,
            tag_option_explicit: true,
            prune_option_explicit: true,
            prune_tags: false,
            prune_tags_option_explicit: false,
            refmap: None,
            refetch: false,
            record_promisor_refs: false,
            update_head_ok: false,
            ssh_options: None,
            atomic: false,
            depth: None,
            merge_srcs: Vec::new(),
            filter: None,
            cloning: false,
            update_shallow: false,
            deepen_relative: false,
            deepen_since: None,
            deepen_not: Vec::new(),
        },
        &mut credentials,
        &mut progress,
    )
    .map(|_| ())
    .map_err(|err| GitBridgeError::Git(format!("failed to fetch notes from {remote_name}: {err}")))
}

fn parse_remote_url_items_from_config(
    path: &Path,
    remotes: &mut Vec<(String, String)>,
) -> GitResult<()> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(());
    };
    let mut current_remote: Option<String> = None;
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            current_remote = line
                .strip_prefix("[remote \"")
                .and_then(|rest| rest.strip_suffix("\"]"))
                .map(str::to_string);
            continue;
        }
        let Some(name) = current_remote.as_ref() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("url") {
            remotes.push((name.clone(), git_config_value(value.trim())?));
        }
    }
    Ok(())
}

fn configured_remote_values_match(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let left_path = Path::new(left);
    let right_path = Path::new(right);
    if let (Ok(left), Ok(right)) = (left_path.canonicalize(), right_path.canonicalize()) {
        return left == right;
    }
    false
}

fn looks_like_remote_location(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with("~/")
        || value.contains("://")
        || value.contains('\\')
}

fn remote_fetch_url_from_checkout_config(
    root: &Path,
    remote_name: &str,
) -> GitResult<Option<String>> {
    for config_path in checkout_git_config_paths(root) {
        let Some(url) = parse_remote_fetch_url_from_config(&config_path, remote_name)? else {
            continue;
        };
        return parse_configured_remote_url(&url, root).map(Some);
    }
    Ok(None)
}

fn parse_configured_remote_url(value: &str, relative_base: &Path) -> GitResult<String> {
    if configured_remote_is_local_path(value) {
        let path = configured_remote_local_path(value, relative_base);
        return Ok(format!("file://{}", path.display()));
    }
    Ok(value.to_string())
}

fn configured_remote_local_path(value: &str, relative_base: &Path) -> PathBuf {
    if value == "~"
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home);
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }

    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        relative_base.join(path)
    }
}

fn configured_remote_is_local_path(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('~')
        || value.starts_with(std::path::MAIN_SEPARATOR)
}

fn checkout_git_config_paths(root: &Path) -> Vec<PathBuf> {
    let dot_git = root.join(".git");
    let mut paths = Vec::new();
    if dot_git.is_dir() {
        paths.push(dot_git.join("config"));
        if let Some(common_dir) = common_git_dir_from_git_dir(&dot_git) {
            paths.push(common_dir.join("config"));
        }
        return paths;
    }
    let Ok(contents) = fs::read_to_string(&dot_git) else {
        return paths;
    };
    let Some(target) = contents.trim().strip_prefix("gitdir:").map(str::trim) else {
        return paths;
    };
    let git_dir = {
        let path = Path::new(target);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            dot_git
                .parent()
                .map(|parent| parent.join(path))
                .unwrap_or_else(|| path.to_path_buf())
        }
    };
    paths.push(git_dir.join("config"));
    if let Some(common_dir) = common_git_dir_from_git_dir(&git_dir) {
        paths.push(common_dir.join("config"));
    }
    paths
}

fn common_git_dir_from_git_dir(git_dir: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(git_dir.join("commondir")).ok()?;
    let target = contents.trim();
    let path = Path::new(target);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        git_dir.join(path)
    })
}

fn parse_remote_fetch_url_from_config(path: &Path, remote_name: &str) -> GitResult<Option<String>> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let mut in_remote = false;
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_remote = line
                .strip_prefix("[remote \"")
                .and_then(|rest| rest.strip_suffix("\"]"))
                == Some(remote_name);
            continue;
        }
        if !in_remote {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("url") {
            return git_config_value(value.trim()).map(Some);
        }
    }
    Ok(None)
}

fn common_repo_for_worktree(repo: &SleyRepository) -> GitResult<SleyRepository> {
    let common_dir_file = repo.git_dir().join("commondir");
    let Ok(contents) = fs::read_to_string(&common_dir_file) else {
        return Ok(repo.clone());
    };
    let target = contents.trim();
    if target.is_empty() {
        return Ok(repo.clone());
    }
    let common_dir = {
        let path = Path::new(target);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            repo.git_dir().join(path)
        }
    };
    open_repo(&common_dir)
}

pub(crate) fn git_err(err: impl std::fmt::Display) -> GitBridgeError {
    GitBridgeError::Git(err.to_string())
}

fn restore_file(path: PathBuf, previous: Option<&[u8]>) -> GitResult<()> {
    if let Some(previous) = previous {
        fs::write(path, previous)?;
    } else if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// `fsync` a single file by opening it read-only and calling
/// `sync_all`. Best-effort: missing files are not an error (a Drop
/// guard might have removed them between write and fsync).
fn fsync_path(path: &Path) -> GitResult<()> {
    match std::fs::File::open(path) {
        Ok(file) => {
            file.sync_all()?;
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(GitBridgeError::Io(err)),
    }
}

/// RAII guard for `init_mirror`. When the mirror directory did not
/// exist at acquisition time, an early Drop (panic, error return)
/// removes the partially-initialized `.heddle/git/` so a future
/// `heddle bridge ...` doesn't see a half-built bare repo. Call
/// `commit()` once the mirror is known-good (e.g. after a successful
/// first export) to disarm the guard.
pub(crate) struct MirrorInitGuard {
    path: PathBuf,
    /// `Some(true)` means we created the directory in this call and
    /// own its rollback; `Some(false)` (or `None` after commit) means
    /// hands off.
    rollback: Option<bool>,
}

impl MirrorInitGuard {
    pub(crate) fn new_from_init(path: PathBuf, did_create: bool) -> Self {
        Self {
            path,
            rollback: Some(did_create),
        }
    }

    pub(crate) fn commit(mut self) {
        self.rollback = None;
    }
}

impl Drop for MirrorInitGuard {
    fn drop(&mut self) {
        if matches!(self.rollback, Some(true))
            && self.path.exists()
            && let Err(err) = std::fs::remove_dir_all(&self.path)
        {
            tracing::warn!(
                path = %self.path.display(),
                error = %err,
                "failed to roll back partial bridge mirror; manual cleanup may be required"
            );
        }
    }
}

/// Bridge policy: a thread is considered an "unclaimed bootstrap" when it
/// points at an empty-tree state with no parents. That is the exact shape of
/// the state produced by `Repository::seed_default_thread`, and it cannot
/// occur through normal user work — any snapshot advances the tip to a state
/// with either a non-empty tree or a non-empty parents list.
///
/// When a user runs `heddle init` followed by `heddle bridge pull` (or
/// `import`), the bootstrap `main` is unclaimed and the incoming git ref
/// should win. This helper lets the bridge recognize that case without
/// silently overwriting real work.
pub(crate) fn thread_is_unclaimed_bootstrap(
    heddle_repo: &HeddleRepository,
    change_id: &ChangeId,
) -> GitResult<bool> {
    let Some(state) = heddle_repo.store().get_state(change_id)? else {
        return Ok(false);
    };
    if !state.parents.is_empty() {
        return Ok(false);
    }
    let Some(tree) = heddle_repo.store().get_tree(&state.tree)? else {
        return Ok(false);
    };
    Ok(tree == Tree::new())
}

pub(crate) fn open_repo(path: &Path) -> GitResult<SleyRepository> {
    match SleyRepository::discover(path) {
        Ok(repo) => Ok(repo),
        Err(_) => SleyRepository::open(path).map_err(git_err),
    }
}

/// Delete a reference if present; missing-ref is a no-op. Used by the
/// write-through rollback path to drop a branch that was created by a
/// failed write-through but isn't reachable from any prior state. We
/// scope the deletion with `RefPrecondition::MustExist` so an unrelated
/// concurrent writer that *just* updated this ref isn't silently
/// clobbered — if the ref vanished underneath us between our read and
/// the delete, that's the rollback we wanted anyway.
pub(crate) fn delete_reference_if_present(repo: &SleyRepository, name: &str) -> GitResult<()> {
    delete_reference(repo, name, None, true)
}

fn delete_reference_matching(
    repo: &SleyRepository,
    name: &str,
    expected_old: ObjectId,
) -> GitResult<()> {
    delete_reference(repo, name, Some(expected_old), false)
}

fn delete_reference(
    repo: &SleyRepository,
    name: &str,
    expected_old: Option<ObjectId>,
    missing_ok: bool,
) -> GitResult<()> {
    let refs = repo.references();
    match refs.read_ref(name).map_err(git_err)? {
        None if missing_ok => Ok(()),
        None => Err(GitBridgeError::Git(format!(
            "failed to delete Git reference '{name}': ref is missing"
        ))),
        Some(ReferenceTarget::Direct(oid)) => repo
            .delete_ref(DeleteRef {
                name: FullName::new(name).map_err(git_err)?,
                expected_old: Some(expected_old.unwrap_or(oid)),
                expected: None,
                reflog: None,
                reflog_committer: None,
            })
            .map_err(git_err),
        Some(ReferenceTarget::Symbolic(_)) => {
            if let Some(expected_old) = expected_old {
                let current = repo
                    .find_reference(name)
                    .map_err(git_err)?
                    .and_then(|reference| reference.peeled_oid(repo).ok().flatten());
                if current != Some(expected_old) {
                    return Err(GitBridgeError::Git(format!(
                        "failed to delete Git reference '{name}': expected {expected_old}, found {}",
                        current
                            .map(|oid| oid.to_string())
                            .unwrap_or_else(|| "missing".to_string())
                    )));
                }
            }
            refs.delete_symbolic_ref(name).map(|_| ()).map_err(git_err)
        }
    }
}

pub(crate) fn set_reference(
    repo: &SleyRepository,
    name: &str,
    target: ObjectId,
    constraint: RefPrecondition,
    log_message: &str,
) -> GitResult<()> {
    let refs = repo.references();
    let old_oid = match refs.read_ref(name).map_err(git_err)? {
        Some(ReferenceTarget::Direct(oid)) => oid,
        _ => ObjectId::null(repo.object_format()),
    };
    let reflog = sley::plumbing::sley_refs::ReflogEntry {
        old_oid,
        new_oid: target,
        committer: bridge_signature(),
        message: log_message.as_bytes().to_vec(),
    };
    let mut tx = refs.transaction();
    tx.update_to(
        name.to_string(),
        ReferenceTarget::Direct(target),
        constraint,
        Some(reflog),
    );
    tx.commit().map_err(git_err)?;
    Ok(())
}

/// Whether two index paths file↔directory-PREFIX-conflict: one names a
/// blob that is a directory prefix of the other (`foo` vs `foo/bar`, in
/// either order). Git's index cannot hold both, since a path is either a
/// file or a directory. Equal paths do NOT count here — that case is an
/// exact match handled separately by the caller.
fn path_prefix_conflict(a: &str, b: &str) -> bool {
    let child_of = |parent: &str, child: &str| {
        child
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    child_of(a, b) || child_of(b, a)
}

/// Recursively collect every file path (blob and symlink) in `tree`,
/// resolving subtrees through `store`. Missing subtree objects are
/// skipped rather than treated as errors, matching the repo's other
/// tree walks. Paths use `/` separators, the form Git's index expects.
fn collect_capture_paths<S: ObjectStore + ?Sized>(
    store: &S,
    tree: &Tree,
    prefix: &str,
    out: &mut Vec<(String, FileMode)>,
) -> GitResult<()> {
    for entry in tree.iter() {
        let path = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        if entry.is_tree() {
            if let Some(subtree) = store.get_tree(&entry.hash)? {
                collect_capture_paths(store, &subtree, &path, out)?;
            }
        } else {
            out.push((path, entry.mode));
        }
    }
    Ok(())
}

fn update_checkout_head_ref(
    repo: &SleyRepository,
    target: ObjectId,
    previous_branch: Option<ObjectId>,
    log_message: &str,
) -> GitResult<()> {
    let expected = previous_branch.map_or(RefPrecondition::MustNotExist, |oid| {
        RefPrecondition::MustExistAndMatch(ReferenceTarget::Direct(oid))
    });
    let ref_name = repo
        .head()
        .ok()
        .and_then(|head| head.symbolic_target.map(|name| name.to_string()))
        .unwrap_or_else(|| "HEAD".to_string());
    let old_oid = previous_branch.unwrap_or_else(|| ObjectId::null(repo.object_format()));
    let head_reflog = sley::plumbing::sley_refs::ReflogEntry {
        old_oid,
        new_oid: target,
        committer: bridge_signature(),
        message: log_message.as_bytes().to_vec(),
    };
    set_reference(repo, &ref_name, target, expected, log_message)?;
    if ref_name != "HEAD" {
        repo.references()
            .append_reflog("HEAD", &head_reflog)
            .map_err(git_err)?;
    }
    Ok(())
}

fn checkout_git_head_is_detached(root: &Path) -> GitResult<bool> {
    let repo = SleyRepository::discover(root).map_err(git_err)?;
    Ok(repo.head().map(|head| head.is_detached()).unwrap_or(false))
}

pub(crate) fn resolve_git_commit_identity(
    repo_root: &Path,
    fallback: &Principal,
) -> GitResult<LocalGitIdentity> {
    if !principal_is_default_unknown(fallback) {
        return Ok(LocalGitIdentity::from_principal(fallback));
    }
    if let Some(identity) = git_config_identity_with_global_fallback(repo_root)? {
        return Ok(identity);
    }

    Err(GitBridgeError::Git(
        "refusing to write a Git commit with Unknown <unknown@example.com>; configure user.name/user.email, HEDDLE_PRINCIPAL_NAME/HEDDLE_PRINCIPAL_EMAIL, or .heddle principal".to_string(),
    ))
}

pub(crate) fn git_config_identity_with_global_fallback(
    repo_root: &Path,
) -> GitResult<Option<LocalGitIdentity>> {
    let name = git_config_value_with_global_fallback(repo_root, "user.name")?;
    let email = git_config_value_with_global_fallback(repo_root, "user.email")?;
    if let (Some(name), Some(email)) = (name, email)
        && !name.trim().is_empty()
        && !email.trim().is_empty()
    {
        return Ok(Some(LocalGitIdentity { name, email }));
    }
    Ok(None)
}

pub(crate) fn principal_is_default_unknown(principal: &Principal) -> bool {
    principal.name.trim().is_empty()
        || principal.email.trim().is_empty()
        || (principal.name.trim() == "Unknown" && principal.email.trim() == "unknown@example.com")
}

fn git_config_value_with_global_fallback(repo_root: &Path, key: &str) -> GitResult<Option<String>> {
    let Ok(repo) = SleyRepository::discover(repo_root) else {
        return Ok(None);
    };
    let Some((section, variable)) = key.split_once('.') else {
        return Ok(None);
    };
    Ok(repo
        .config_snapshot()
        .map_err(git_err)?
        .get(section, None, variable)
        .map(str::to_string))
}

fn git_config_value(value: &str) -> GitResult<String> {
    let Some(quoted) = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return Ok(value.to_string());
    };
    let mut out = String::new();
    let mut chars = quoted.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some(escaped) = chars.next() else {
            return Err(GitBridgeError::Git(
                "unterminated escape in repo-local Git config".to_string(),
            ));
        };
        match escaped {
            '"' | '\\' => out.push(escaped),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'b' => out.push('\u{0008}'),
            other => out.push(other),
        }
    }
    Ok(out)
}

fn bridge_signature() -> Vec<u8> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    format!("Heddle <heddle@local> {seconds} +0000").into_bytes()
}

fn repo_relative_base(repo: &SleyRepository) -> PathBuf {
    repo.workdir().unwrap_or_else(|| {
        if repo
            .git_dir()
            .file_name()
            .is_some_and(|name| name == ".git")
        {
            repo.git_dir()
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| repo.git_dir().to_path_buf())
        } else {
            repo.git_dir().to_path_buf()
        }
    })
}

fn local_path_from_url(url: &str) -> GitResult<Option<PathBuf>> {
    // Defense in depth (push-routing no-op): the git-overlay exporter speaks
    // only the local/git network transports. A `heddle://` hosted URL must
    // NEVER reach this classifier — the hosted-sync path
    // (`GrpcHostedClient`) is the only thing that can push to it. If routing
    // upstream is correct this is unreachable; making it a hard error here
    // means a `heddle://` slipping into the git exporter can never again be a
    // silent success (it would otherwise fall through as a generic network
    // URL, "reconcile" locally, and report success without contacting the
    // server).
    if url.starts_with("heddle://") {
        return Err(GitBridgeError::Git(format!(
            "remote '{url}' uses the hosted heddle:// scheme, which cannot be pushed via the git-overlay exporter; hosted pushes must go through the native hosted-sync path"
        )));
    }
    let Some(raw_path) = url.strip_prefix("file://") else {
        return Ok(None);
    };
    let path = PathBuf::from(raw_path);
    if path.as_os_str().is_empty() {
        return Err(GitBridgeError::Git(format!(
            "remote '{}' has no filesystem path",
            url
        )));
    }
    Ok(Some(path))
}

fn collect_ref_updates(repo: &SleyRepository) -> GitResult<Vec<RefUpdate>> {
    let mut updates = Vec::new();

    for reference in repo.references().list_refs().map_err(git_err)? {
        let ReferenceTarget::Direct(target) = reference.target else {
            continue;
        };
        if let Some(name) = reference.name.strip_prefix("refs/heads/") {
            updates.push(RefUpdate {
                name: name.to_string(),
                target,
                namespace: RefNamespace::Branch,
            });
        } else if let Some(name) = reference.name.strip_prefix("refs/tags/") {
            updates.push(RefUpdate {
                name: name.to_string(),
                target,
                namespace: RefNamespace::Tag,
            });
        } else if let Some(name) = reference.name.strip_prefix("refs/notes/") {
            updates.push(RefUpdate {
                name: name.to_string(),
                target,
                namespace: RefNamespace::Note,
            });
        }
    }

    Ok(updates)
}

/// A partition of the commits that land in the destination, computed over
/// the SINGLE copied ref set. `total` is every unique commit reachable from
/// the copied branch/tag tips; `newly` is the subset minted during this
/// export run. `already` is the remainder. Because `newly` is a subset of
/// the same walk that produced `total`, `newly + already == total` holds by
/// construction — the summary can never report more "newly written" than
/// "total", and no orphan/unreferenced state (minted but reachable from no
/// copied ref, hence never in the walk) can inflate any count.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ExportedCommitCounts {
    pub total: usize,
    pub newly: usize,
}

/// Count and partition the commits reachable from the branch and tag tips
/// that `collect_ref_updates` writes to a destination. Derived from the SAME
/// ref set `copy_mirror_to_path` copies, so the reported counts equal what
/// actually lands in the destination — including stale mirror refs left
/// behind by a dropped Heddle thread (export does not prune them, so the
/// commit is still copied and must still be counted; pruning would be a
/// separate behavior change). Notes refs are excluded: they carry
/// metadata, not history, so they don't count as exported commits.
///
/// `newly_minted` is the set of git OIDs freshly minted during this export
/// run; a commit in the walk that is in this set is counted as `newly`, the
/// rest as `already`. Routing both the total and the newly count through
/// this single walk guarantees they can never diverge.
pub(crate) fn count_exported_commits(
    repo: &SleyRepository,
    newly_minted: &HashSet<ObjectId>,
) -> GitResult<ExportedCommitCounts> {
    let tips: Vec<ObjectId> = collect_ref_updates(repo)?
        .into_iter()
        .filter(|update| matches!(update.namespace, RefNamespace::Branch | RefNamespace::Tag))
        .map(|update| update.target)
        .collect();

    let mut stack = tips;
    let mut seen = HashSet::new();
    let mut counts = ExportedCommitCounts::default();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let object = repo.read_object(&oid).map_err(git_err)?;
        match object.object_type {
            GitObjectType::Commit => {
                counts.total += 1;
                if newly_minted.contains(&oid) {
                    counts.newly += 1;
                }
                let commit = repo.read_commit(&oid).map_err(git_err)?;
                for parent in commit.parents {
                    stack.push(parent);
                }
            }
            // An annotated tag dereferences to its target (commit, or a
            // blob/tree for the rare blob/tree-pointing tag). Follow it;
            // only a Commit at the end increments the count.
            GitObjectType::Tag => {
                let tag = repo.read_tag(&oid).map_err(git_err)?;
                stack.push(tag.object);
            }
            GitObjectType::Tree | GitObjectType::Blob => {}
        }
    }
    Ok(counts)
}

fn collect_ref_updates_for_fetch(
    repo: &SleyRepository,
    scope: GitFetchScope,
) -> GitResult<Vec<RefUpdate>> {
    let updates = collect_ref_updates(repo)?;
    match scope {
        GitFetchScope::AllRefs => Ok(updates),
        GitFetchScope::BranchesAndNotes => Ok(updates
            .into_iter()
            .filter(|update| matches!(update.namespace, RefNamespace::Branch | RefNamespace::Note))
            .collect()),
    }
}

pub(crate) fn collect_import_source_ref_updates(
    repo: &SleyRepository,
    refs: &[String],
) -> GitResult<Vec<RefUpdate>> {
    let updates = collect_ref_updates(repo)?;
    if refs.is_empty() {
        return Ok(updates);
    }

    let wanted: HashSet<&str> = refs.iter().map(String::as_str).collect();
    Ok(updates
        .into_iter()
        .filter(|update| matches_import_ref(update, &wanted))
        .collect())
}

fn matches_import_ref(update: &RefUpdate, wanted: &HashSet<&str>) -> bool {
    let full = full_ref_name(update);
    wanted.contains(update.name.as_str()) || wanted.contains(full.as_str())
}

fn full_ref_name(update: &RefUpdate) -> String {
    match update.namespace {
        RefNamespace::Branch => format!("refs/heads/{}", update.name),
        RefNamespace::Tag => format!("refs/tags/{}", update.name),
        RefNamespace::Note => format!("refs/notes/{}", update.name),
    }
}

#[cfg(test)]
pub(crate) fn ensure_commit_update_fast_forward(
    repo: &SleyRepository,
    name: &str,
    old: ObjectId,
    new: ObjectId,
) -> GitResult<()> {
    if old == new || old == ObjectId::null(repo.object_format()) {
        return Ok(());
    }
    match commit_is_descendant_of(repo, new, old) {
        Ok(true) => Ok(()),
        Ok(false) => Err(GitBridgeError::NonFastForwardRef {
            name: name.to_string(),
            old,
            new,
        }),
        Err(err) => Err(GitBridgeError::Git(format!(
            "ref update would move {name}: {old} -> {new}, but Heddle could not verify it as a fast-forward ({err}); fetch/import first or inspect the refs explicitly"
        ))),
    }
}

fn commit_is_descendant_of(
    repo: &SleyRepository,
    descendant: ObjectId,
    ancestor: ObjectId,
) -> GitResult<bool> {
    let mut stack = vec![descendant];
    let mut seen = HashSet::new();
    while let Some(oid) = stack.pop() {
        if oid == ancestor {
            return Ok(true);
        }
        if !seen.insert(oid) {
            continue;
        }
        let commit = repo.read_commit(&oid).map_err(git_err)?;
        for parent in commit.parents {
            stack.push(parent);
        }
    }
    Ok(false)
}

/// Filename, under a destination repo's git dir, of heddle's record of which
/// full ref names it has exported to THAT destination, AND the tip OID heddle
/// last published for each. A heddle-owned sidecar (git ignores unknown files in
/// the git dir), one `<full ref name> <published tip oid>` pair per line. Lives
/// WITH the destination so the delete-set can be scoped to refs heddle actually
/// wrote here — never the raw destination namespace (heddle#316 CLASS 2) — and
/// so the force decision can prove a rewind is heddle-OWNED, not an out-of-band
/// advance, by matching the destination tip against the recorded published tip
/// (heddle#316 r12).
const HEDDLE_EXPORTED_REFS_FILE: &str = "heddle-exported-refs";

/// Directory, under heddle's OWN dir, holding the per-URL-remote exported-refs
/// records. A network remote (`git://`, `ssh://`, `https://`) has no local git
/// dir heddle can drop a sidecar into, so its record lives here instead — keyed
/// by a hash of the remote URL. This is the network sibling of
/// [`HEDDLE_EXPORTED_REFS_FILE`]: the SAME delete-set reconciliation, with the
/// only difference being WHERE the record is stored (heddle#316 r11).
const HEDDLE_NETWORK_EXPORTED_REFS_DIR: &str = "git-network-exported-refs";

fn exported_refs_manifest_path(target_repo: &SleyRepository) -> PathBuf {
    target_repo.git_dir().join(HEDDLE_EXPORTED_REFS_FILE)
}

/// On-disk location of the exported-refs record for the network remote at `url`.
/// Keyed by a hash of the URL string so an arbitrarily long / non-ASCII URL maps
/// to a fixed-length, filesystem-safe filename. Stored under heddle's own dir
/// (the remote is not local, so there is no destination git dir to host it).
fn network_exported_refs_path(heddle_dir: &Path, url: &str) -> PathBuf {
    let key = ContentHash::compute_typed("git-network-exported-refs", url.as_bytes()).to_hex();
    heddle_dir
        .join(HEDDLE_NETWORK_EXPORTED_REFS_DIR)
        .join(format!("{key}.refs"))
}

/// The full ref names heddle has previously exported to the destination whose
/// record lives at `path`, each mapped to the tip OID heddle last published for
/// it. `Ok(empty)` when no record exists yet — a first export, OR a destination
/// heddle wrote to before this record existed. Returning empty (rather than
/// assuming the destination's current heddle-namespace refs were heddle's) is the
/// conservative choice: it can never delete a foreign ref — nor force-overwrite a
/// destination tip — on the first export after this code lands.
fn read_exported_refs_at(path: &Path) -> GitResult<HashMap<String, ObjectId>> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let mut map = HashMap::new();
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                // `<full ref name> <published tip oid>`. The tip is the OID heddle
                // last published for that ref here — the ownership token the force
                // decision consults (heddle#316 r12). A pre-r12 legacy record
                // stored only the name; parse its tip when present and fall back to
                // null otherwise. A null tip can never equal a live `old`, so a
                // legacy ref is never force-rewound (the safe direction) while it
                // still participates in the delete-set.
                let mut parts = line.split_whitespace();
                let Some(name) = parts.next() else {
                    continue;
                };
                let tip = parts
                    .next()
                    .and_then(|token| token.parse::<ObjectId>().ok())
                    .unwrap_or_else(|| ObjectId::null(ObjectFormat::Sha1));
                map.insert(name.to_string(), tip);
            }
            Ok(map)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(GitBridgeError::Io(e)),
    }
}

/// Persist `refs` (full ref name → published tip OID) as heddle's exported-refs
/// record at `path`. Atomic temp+rename so a torn write can't surface a
/// half-record.
fn write_exported_refs_at(path: &Path, refs: &HashMap<String, ObjectId>) -> GitResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut sorted: Vec<(&str, &ObjectId)> = refs
        .iter()
        .map(|(name, tip)| (name.as_str(), tip))
        .collect();
    sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let body = sorted
        .iter()
        .map(|(name, tip)| format!("{name} {tip}"))
        .collect::<Vec<_>>()
        .join("\n");
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, body)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Atomically write `git_dir/HEAD` as a symbolic ref pointing at
/// `branch_ref` (e.g. `refs/heads/main`). The content is
/// `ref: <branch_ref>\n`.
///
/// A bare `fs::write(HEAD, ...)` is not crash-atomic: a power loss
/// mid-write can leave a truncated or empty `HEAD`, which a subsequent
/// `Repository::open` reads as a detached/garbage symref. We instead
/// write to `HEAD.tmp` and `fs::rename` it over `HEAD` (rename is
/// atomic within a directory), mirroring `write_exported_refs_at`.
/// Both the file and its parent directory are fsync'd so the dirent is
/// durably committed — a file-level fsync alone doesn't persist the
/// rename on most filesystems.
pub(crate) fn write_head_symref(git_dir: &Path, branch_ref: &str) -> GitResult<()> {
    let head_path = git_dir.join("HEAD");
    let tmp = head_path.with_extension("tmp");
    fs::write(&tmp, format!("ref: {branch_ref}\n"))?;
    fsync_path(&tmp)?;
    fs::rename(&tmp, &head_path)?;
    fsync_path(&head_path)?;
    fsync_path(git_dir)?;
    Ok(())
}

/// Heddle's exported-refs record for `target_repo` (full ref name → last-published
/// tip OID), the local-path destination record. See [`read_exported_refs_at`].
pub(crate) fn read_exported_refs(
    target_repo: &SleyRepository,
) -> GitResult<HashMap<String, ObjectId>> {
    read_exported_refs_at(&exported_refs_manifest_path(target_repo))
}

/// Persist the local-path destination's exported-refs record. See
/// [`write_exported_refs_at`].
pub(crate) fn write_exported_refs(
    target_repo: &SleyRepository,
    refs: &HashMap<String, ObjectId>,
) -> GitResult<()> {
    write_exported_refs_at(&exported_refs_manifest_path(target_repo), refs)
}

/// Filename, under the internal MIRROR's git dir, of heddle's record of which
/// full ref names it MANAGES in the mirror, each mapped to the tip it last
/// published for that ref. The mirror-side analog of [`HEDDLE_EXPORTED_REFS_FILE`]
/// (the destination's `heddle-exported-refs`): the mirror reconcile had no
/// persisted ownership record, so it reconstructed ownership ad-hoc from OID
/// membership — the bug that drove heddle#316 through 7 review rounds. A mirror
/// ref is MANAGED iff its full name is a key here, NEVER by OID membership: a
/// foreign branch/tag that happens to point at a heddle-minted commit is still
/// foreign because heddle never recorded WRITING it under that name. The format,
/// atomic-write, and parse contract are shared verbatim with the destination
/// record (`read_exported_refs_at`/`write_exported_refs_at`).
const HEDDLE_MIRROR_MANAGED_REFS_FILE: &str = "heddle-mirror-managed-refs";

/// On-disk path of the mirror's managed-refs record.
fn mirror_managed_refs_path(mirror_repo: &SleyRepository) -> PathBuf {
    mirror_repo.git_dir().join(HEDDLE_MIRROR_MANAGED_REFS_FILE)
}

/// Whether the mirror's managed-refs record exists on disk. Used to distinguish
/// a genuine FIRST export after this code lands (absent → seed from the current
/// mirror ref set so pre-existing heddle refs aren't all misread as foreign)
/// from a record that exists but is empty (everything was legitimately dropped —
/// do NOT re-seed).
pub(crate) fn mirror_managed_refs_recorded(mirror_repo: &SleyRepository) -> bool {
    mirror_managed_refs_path(mirror_repo).exists()
}

/// The full ref names heddle MANAGES in the mirror (full ref name → last-published
/// tip OID). `Ok(empty)` when the record is absent — callers seed a first run from
/// the current mirror ref set; see [`mirror_managed_refs_recorded`].
pub(crate) fn read_mirror_managed_refs(
    mirror_repo: &SleyRepository,
) -> GitResult<HashMap<String, ObjectId>> {
    read_exported_refs_at(&mirror_managed_refs_path(mirror_repo))
}

/// Persist the mirror's managed-refs record. Atomic temp+rename via
/// [`write_exported_refs_at`].
pub(crate) fn write_mirror_managed_refs(
    mirror_repo: &SleyRepository,
    refs: &HashMap<String, ObjectId>,
) -> GitResult<()> {
    write_exported_refs_at(&mirror_managed_refs_path(mirror_repo), refs)
}

/// Read the mirror's managed-refs record, SEEDING a genuine first run (no record
/// on disk) from the current mirror ref set so the reconcile does not misread
/// every pre-existing heddle ref as foreign.
///
/// This is the #1 first-run risk (heddle#316): an absent record on the first
/// export after this code lands must NOT make existing refs look foreign — that
/// would silently stop embargo retraction (a now-embargoed thread tip would never
/// be rewound/deleted because its branch would be treated as a foreign ref to
/// spare). Every ref currently in the mirror was put there by heddle (the mint
/// reconcile, `import`, or `fetch`), so claiming them all as managed on the first
/// run is correct. A record that EXISTS but is empty (everything was legitimately
/// dropped) is NOT re-seeded — only a truly-absent record triggers the seed.
pub(crate) fn read_or_seed_mirror_managed_refs(
    mirror_repo: &SleyRepository,
) -> GitResult<HashMap<String, ObjectId>> {
    if mirror_managed_refs_recorded(mirror_repo) {
        read_mirror_managed_refs(mirror_repo)
    } else {
        Ok(collect_ref_updates(mirror_repo)?
            .into_iter()
            .map(|update| (full_ref_name(&update), update.target))
            .collect())
    }
}

/// The mirror refs heddle MANAGES, as [`RefUpdate`]s — [`collect_ref_updates`]
/// filtered to the names in the managed-refs `record`, PLUS every `refs/notes/*`
/// ref (heddle's metadata namespace, always heddle-managed and content-rebuilt
/// rather than target-claimed through the reconcile). The export/push frontier
/// MUST source from this rather than the raw [`collect_ref_updates`] so a foreign
/// branch/tag heddle never wrote — even one pointing at a heddle-minted commit —
/// never enters the served frontier nor the destination's desired set (heddle#316).
/// The FETCH path keeps using [`collect_ref_updates`]/[`collect_ref_updates_for_fetch`]
/// (it must see every ref); only the export/push frontier is managed-filtered.
pub(crate) fn collect_managed_ref_updates(
    repo: &SleyRepository,
    record: &HashMap<String, ObjectId>,
) -> GitResult<Vec<RefUpdate>> {
    Ok(collect_ref_updates(repo)?
        .into_iter()
        .filter(|update| {
            matches!(update.namespace, RefNamespace::Note)
                || record.contains_key(&full_ref_name(update))
        })
        .collect())
}

/// How a destination ref must move from its current `old` tip to the served
/// `new` tip. The discriminator that lets EVERY push destination apply the SAME
/// served-frontier reconciliation: a deliberate backward rewind (the embargo
/// frontier lag) is FORCED past the fast-forward guard, while a true fork is
/// still caught by it (heddle#316 r11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefMove {
    /// `old == new` (or both absent) — nothing to do.
    Unchanged,
    /// No resolvable `old` at the destination — a fresh ref.
    Create,
    /// `new` descends from `old` — an ordinary fast-forward.
    FastForward,
    /// `old` descends from `new` AND `old` is the tip heddle itself last
    /// published here — a deliberate backward rewind heddle OWNS: the served
    /// frontier was lagged down to an ancestor because the prior tip (or a
    /// descendant of `new`) was embargoed/retracted. MUST be forced through at
    /// every destination, exactly as the mirror-side branch rewind forces it.
    /// Topology alone does NOT qualify: a destination tip advanced OUT OF BAND
    /// past heddle's last-published tip also descends from `new`, but is
    /// [`Diverged`](RefMove::Diverged), never force-overwritten (heddle#316 r12).
    Rewind,
    /// `old` and `new` share no ancestor line (or `old` is unresolvable here) —
    /// the divergence the fast-forward guard exists to catch.
    Diverged,
}

/// Classify how a destination ref moves from `old` to `new`, resolving the
/// topology in `repo` (the mirror, which holds every served object PLUS any
/// previously-exported-now-embargoed object the purge dropped from the mapping
/// but not from the object DB). The single place that distinguishes a deliberate
/// embargo rewind from a fork, so both push destinations force the former and
/// reject the latter identically.
///
/// `recorded_tip` is the tip heddle last published for this ref at THIS
/// destination (from its exported-refs record), or `None` when heddle has no
/// record of publishing it here. A backward rewind is FORCED only when heddle
/// owns the tip being rewound — `recorded_tip == Some(old)`. Topology alone is
/// insufficient: a destination tip advanced OUT OF BAND past heddle's
/// last-published tip (then fetched into the mirror) ALSO descends from `new`,
/// but heddle never published it, so it is [`RefMove::Diverged`] and must not be
/// force-overwritten (heddle#316 r12).
fn classify_ref_move(
    repo: &SleyRepository,
    old: Option<ObjectId>,
    new: ObjectId,
    recorded_tip: Option<ObjectId>,
) -> GitResult<RefMove> {
    let Some(old) = old else {
        return Ok(RefMove::Create);
    };
    if old == ObjectId::null(repo.object_format()) {
        return Ok(RefMove::Create);
    }
    if old == new {
        return Ok(RefMove::Unchanged);
    }
    // `new` is the served frontier we just minted/copied, so walking from it is
    // always safe. A fast-forward is `new` reaching `old`.
    if commit_is_descendant_of(repo, new, old)? {
        return Ok(RefMove::FastForward);
    }
    // A backward rewind is `old` reaching `new`. Forcing it past the FF guard is
    // authorized ONLY when heddle OWNS the rewind: `old` is exactly the tip heddle
    // itself last published for this ref here (per the exported-refs record). A
    // destination tip heddle did NOT publish — an out-of-band descendant the user
    // advanced and fetched into the mirror — is never force-overwritten; it falls
    // through to `Diverged` (FF-rejected unless the user passes `--force`), so its
    // newer commit survives. `old`'s objects survive in the mirror because heddle
    // published it (the embargo purge drops the ChangeId→OID mapping, never the
    // object); if `old` is NOT resolvable here we cannot prove a rewind anyway.
    if recorded_tip == Some(old)
        && repo.read_commit(&old).is_ok()
        && commit_is_descendant_of(repo, old, new)?
    {
        return Ok(RefMove::Rewind);
    }
    Ok(RefMove::Diverged)
}

/// Whether a destination ref in the served set may be overwritten, and on what
/// terms. The verdict EVERY namespace's overwrite funnels through, so ownership
/// is decided in exactly one place.
///
/// The reconcile invariant (heddle#316 r17): ownership — heddle owns the tip it
/// overwrites (`recorded == old`, or the move is a safe forward), OR the user
/// passes `--force` — gates EVERY namespace's overwrite AND every delete. The
/// ONLY per-namespace axis is move-classification: branch/note resolve
/// fast-forward-vs-fork topology via [`classify_ref_move`]; a tag's target may be
/// an annotated-tag-object OID (not a commit) so it cannot be FF-classified and
/// uses the free-move [`classify_tag_move`], which bakes the SAME ownership gate
/// in. A new namespace that wires an overwrite without consulting a verdict here
/// would skip the gate — the conformance matrix exists to fail that row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteVerdict {
    /// No-op — the served target already matches the destination tip.
    Skip,
    /// Safe to land unconditionally: a create, a fast-forward, or a heddle-owned
    /// overwrite/rewind (the ownership token already proved `recorded == old`).
    Write,
    /// An out-of-band overwrite heddle does NOT own — error unless `--force`.
    RequireForce,
}

/// Map a branch/note [`RefMove`] onto a [`WriteVerdict`]. `Rewind` is already
/// ownership-proven by [`classify_ref_move`] (`recorded == old`), so it is a
/// `Write`; only `Diverged` (a fork, or an out-of-band advance heddle never
/// published) demands `--force`.
fn verdict_from_move(m: RefMove) -> WriteVerdict {
    match m {
        RefMove::Unchanged => WriteVerdict::Skip,
        RefMove::Create | RefMove::FastForward | RefMove::Rewind => WriteVerdict::Write,
        RefMove::Diverged => WriteVerdict::RequireForce,
    }
}

/// Classify a TAG overwrite. Tags are free-move (never fast-forward-guarded): a
/// tag's `target` can be an annotated-tag-object OID rather than a commit, so it
/// cannot be FF-classified — [`classify_ref_move`] would resolve `find_commit`
/// on the tag object and error. The ownership gate is applied directly here
/// instead: a create or a heddle-owned overwrite (`recorded == old`) lands; an
/// out-of-band tag heddle never recorded is spared (`RequireForce`) exactly as an
/// out-of-band branch advance is — never silently clobbered (heddle#316 r17).
fn classify_tag_move(
    old: Option<ObjectId>,
    target: ObjectId,
    recorded: Option<ObjectId>,
) -> WriteVerdict {
    match old {
        // No tip at the destination — a fresh tag.
        None => WriteVerdict::Write,
        // Already at the served target — nothing to do.
        Some(o) if o == target => WriteVerdict::Skip,
        // heddle owns the tip it is overwriting — its published move lands.
        Some(o) if recorded == Some(o) => WriteVerdict::Write,
        // An out-of-band tag heddle never published — spare it unless `--force`.
        Some(_) => WriteVerdict::RequireForce,
    }
}

/// A served ref a push destination must write: its full name, the served `new`
/// tip, and whether the receive-pack command must be forced.
#[derive(Debug)]
pub(crate) struct PlannedRefWrite {
    pub(crate) full_name: String,
    pub(crate) old: Option<ObjectId>,
    pub(crate) new: ObjectId,
    pub(crate) force: bool,
}

/// A previously-exported ref the served mirror no longer carries: it must be
/// deleted at the destination.
#[derive(Debug)]
pub(crate) struct PlannedRefDelete {
    pub(crate) full_name: String,
    pub(crate) old: ObjectId,
}

/// The ONE reconciliation plan EVERY push destination applies, so its published
/// refs converge to the served frontier by construction.
#[derive(Debug)]
pub(crate) struct DestinationReconcilePlan {
    /// Survivors to write — creations, fast-forwards, and FORCED embargo rewinds.
    pub(crate) writes: Vec<PlannedRefWrite>,
    /// Previously-exported refs the mirror no longer serves AND that still exist
    /// at the destination — to delete. Scoped to heddle-owned refs (never foreign).
    pub(crate) deletes: Vec<PlannedRefDelete>,
    /// The exported-refs record to persist for this destination after the push:
    /// full ref name → the tip heddle just published, plus the previously-recorded
    /// tip for any ref left in place — a still-served ref out of this push's scope
    /// OR an out-of-band tip whose retraction was skipped (so `--force` can still
    /// retract it later). A deleted ref drops out; a foreign ref never enters.
    pub(crate) new_manifest: HashMap<String, ObjectId>,
}

/// The sorted full names of the refs a destination reconcile plan WRITES —
/// creations, fast-forwards, and forced embargo rewinds. This is the
/// `refs_written` surface `heddle push` reports so a git veteran (or agent)
/// can verify the round-trip with `git ls-remote`. Retraction deletes are
/// not included. Sorted because the plan's write order derives from hash-map
/// iteration and the reported list must be deterministic.
pub(crate) fn planned_write_names(plan: &DestinationReconcilePlan) -> Vec<String> {
    let mut names: Vec<String> = plan
        .writes
        .iter()
        .map(|write| write.full_name.clone())
        .collect();
    names.sort_unstable();
    names
}

/// The full ref names a push may MATERIALIZE (create fresh) at a destination — the
/// `creatable_names` gate for [`plan_destination_reconcile`]. `None` for an
/// all-thread push (every served ref is creatable, so the gate never fires);
/// `Some(set)` for a current-thread push (only the attached branch + the notes
/// refs). This is the destination analog of the mirror reconcile's materialization
/// gate (`git_export::export`'s `existing.is_none() && !in_scope` skip): a scoped
/// push reconciles EXISTING out-of-scope refs (the embargo rewind) but never
/// publishes a brand-new sibling the caller did not ask to export (heddle#316 r16).
fn creatable_ref_names(
    served_frontier: &[RefUpdate],
    scope: GitPushScope,
    current_branch: Option<&str>,
) -> Option<HashSet<String>> {
    match scope {
        GitPushScope::AllThreads => None,
        GitPushScope::CurrentThread => {
            let branch = current_branch.unwrap_or_default();
            Some(
                served_frontier
                    .iter()
                    .filter(|update| {
                        (matches!(update.namespace, RefNamespace::Branch) && update.name == branch)
                            || matches!(update.namespace, RefNamespace::Note)
                    })
                    .map(full_ref_name)
                    .collect(),
            )
        }
    }
}

/// Build the served-frontier reconciliation plan shared by the local-path and
/// URL/network push destinations (heddle#316 r11/r13/r16). The destination's
/// published refs are a PURE PROJECTION of the served frontier, restricted to
/// heddle-owned refs: every op — create, fast-forward, forced embargo rewind,
/// retraction delete, or skip — is DERIVED from ONE pass over the desired-vs-
/// actual diff, and the heddle-OWNERSHIP token (`recorded_tip == old`) gates
/// force AND delete UNIFORMLY. There is no separate per-operation enforcement
/// branch to forget: a destination tip heddle never published is neither
/// force-rewound NOR deleted (it survives) unless the user passes `--force`.
///
/// INVARIANT (heddle#316 r16): `served_frontier` is the WHOLE-MIRROR served
/// frontier — every heddle-managed mirror ref at its CURRENT served target — the
/// SAME projection the mirror reconcile (`git_export::export`) materialized into
/// the mirror. The destination reconcile and the mirror reconcile are therefore
/// driven by ONE source of truth, so destination and mirror cannot diverge for
/// ANY embargo transition, in-scope OR out-of-scope: an out-of-scope ref the
/// mirror rewound for embargo is present here at its NEW (rewound) target, and
/// [`classify_ref_move`] emits the rewind to the destination by construction.
/// There is NO "served but out of this push's scope, leave it untouched" arm — a
/// scoped push reconciles the destination against the whole served frontier, not
/// a scope-filtered subset that could keep serving a ref the mirror already
/// rewound (the cross-thread-embargo destination leak this round closes).
///
/// The ONE thing scope still gates is MATERIALIZATION — exactly as the mirror
/// reconcile does (`git_export::export`'s `existing.is_none() && !in_scope`
/// skip): a scoped push REWINDS/RETRACTS an EXISTING out-of-scope ref (the embargo
/// fix) but must not publish a brand-new sibling the caller did not ask to export.
/// `creatable_names` carries that gate: a ref ABSENT from the destination whose
/// name is NOT creatable is skipped (never created); one that already EXISTS is
/// always reconciled, so no target change is ever masked.
///
/// * `mirror_repo` — resolves the rewind-vs-fork topology (see
///   [`classify_ref_move`]).
/// * `served_frontier` — the WHOLE-MIRROR served frontier: every heddle-owned
///   ref that should exist at the destination, at its served target. A
///   previously-exported ref ABSENT from this set is one the mirror no longer
///   serves AT ALL (a retraction), never merely out of a push's scope.
/// * `creatable_names` — the full ref names this push may MATERIALIZE fresh:
///   `None` for an all-thread push (every served ref is creatable); `Some(set)`
///   for a current-thread push (only the attached branch + notes). Gates ONLY
///   first-time creation of an absent ref; an existing ref is always reconciled.
/// * `old_at_destination` — the destination's current ref tips (full name → oid).
/// * `previously_exported` — heddle's record of what it exported to THIS
///   destination (full ref name → last-published tip OID): the foreign-ref
///   scoping AND the single ownership token for both delete and force.
/// * `force` — the user's explicit `--force`: additionally forces a true fork
///   AND authorizes retracting an out-of-band destination tip.
pub(crate) fn plan_destination_reconcile(
    mirror_repo: &SleyRepository,
    served_frontier: &[RefUpdate],
    creatable_names: Option<&HashSet<String>>,
    old_at_destination: &HashMap<String, ObjectId>,
    previously_exported: &HashMap<String, ObjectId>,
    force: bool,
) -> GitResult<DestinationReconcilePlan> {
    // The DESIRED ref-set indexed by full name → its `RefUpdate` (served target +
    // namespace). A name is in `desired` iff the WHOLE-MIRROR served frontier
    // wants it published now — there is no scope-filtered subset (heddle#316 r16),
    // so an out-of-scope ref the mirror rewound for embargo is here at its NEW
    // target rather than silently kept at its old (embargoed) tip.
    let desired: HashMap<String, &RefUpdate> = served_frontier
        .iter()
        .map(|u| (full_ref_name(u), u))
        .collect();

    // ONE pass over the union of (desired ∪ previously-exported) names — the
    // complete desired-vs-actual diff. For each ref the op is derived from the
    // same three inputs: `desired` (does the served frontier want it, at what
    // target), `old` (the destination's current tip, out-of-band-aware), and
    // `recorded` (the tip heddle last published here = the OWNERSHIP token). The
    // ownership token gates force AND delete identically (heddle#316 r13).
    let mut names: BTreeSet<String> = desired.keys().cloned().collect();
    names.extend(previously_exported.keys().cloned());

    let mut writes = Vec::new();
    let mut deletes = Vec::new();
    let mut new_manifest: HashMap<String, ObjectId> = HashMap::new();

    for full in names {
        let old = old_at_destination.get(&full).copied();
        let recorded = previously_exported.get(&full).copied();

        if let Some(update) = desired.get(&full).copied() {
            // MATERIALIZATION gate (the mirror reconcile's `existing.is_none() &&
            // !in_scope` skip, applied to the destination): an out-of-scope ref
            // ABSENT from the destination must not be CREATED by a scoped push —
            // that would publish a brand-new sibling the caller did not ask to
            // export. An EXISTING out-of-scope ref falls through and is reconciled
            // (rewind/retract), so the embargo fix is untouched; only first-time
            // creation is suppressed. Preserve any ownership token so a later
            // all-thread push can still materialize it (heddle#316 r14/r16).
            if old.is_none() && creatable_names.is_some_and(|names| !names.contains(&full)) {
                if let Some(recorded) = recorded {
                    new_manifest.insert(full, recorded);
                }
                continue;
            }
            // In the desired set: land it at the served target. A ref this push
            // publishes is heddle-owned at its new target — record it. The
            // overwrite funnels through ONE ownership gate ([`WriteVerdict`]): the
            // only per-namespace axis is move-classification — branch/note resolve
            // fast-forward-vs-fork topology, a tag is free-move (its target may be
            // an annotated-tag-object OID, not a commit) with the SAME ownership
            // gate baked into [`classify_tag_move`]. An out-of-band destination tip
            // heddle never recorded is spared at EVERY namespace unless `--force`.
            let (verdict, force_write) = match update.namespace {
                RefNamespace::Branch | RefNamespace::Note => {
                    let movement = classify_ref_move(mirror_repo, old, update.target, recorded)?;
                    (
                        verdict_from_move(movement),
                        matches!(movement, RefMove::Rewind),
                    )
                }
                RefNamespace::Tag => {
                    let verdict = classify_tag_move(old, update.target, recorded);
                    (
                        verdict,
                        old.is_some_and(|old| old != update.target)
                            && matches!(verdict, WriteVerdict::Write),
                    )
                }
            };
            let proceed = match verdict {
                WriteVerdict::Skip => false,
                WriteVerdict::Write => true,
                WriteVerdict::RequireForce => {
                    if force {
                        true
                    } else {
                        return Err(GitBridgeError::NonFastForwardRef {
                            name: full.clone(),
                            old: old.unwrap_or_else(|| ObjectId::null(mirror_repo.object_format())),
                            new: update.target,
                        });
                    }
                }
            };
            if proceed {
                writes.push(PlannedRefWrite {
                    full_name: full.clone(),
                    old,
                    new: update.target,
                    force: force_write || matches!(verdict, WriteVerdict::RequireForce),
                });
            }
            // CLAIM ownership in the record ONLY for a ref heddle actually writes
            // this push, or one it already owned (had a record for). A pre-existing
            // destination ref already AT the served target that heddle never recorded
            // (verdict Skip, `recorded` None) is FOREIGN — recording it would let a
            // later export DELETE/rewind a ref heddle never created (heddle#316
            // destination foreign-ref over-claim). Spare it: leave it out of the
            // manifest so it stays unowned.
            if proceed || recorded.is_some() {
                new_manifest.insert(full, update.target);
            }
            continue;
        }

        // Absent from the WHOLE-MIRROR served frontier ⇒ genuinely retracted: the
        // served mirror no longer carries this previously-exported ref at all (NOT
        // merely out of a push's scope — there is no scope subset here). Delete it,
        // but ONLY through the SAME ownership gate the forced
        // rewind uses: heddle owns the destination's current tip (`recorded ==
        // old`), or the user forces. An out-of-band advance heddle never published
        // is spared (it survives) and KEEPS its ownership token, so a later
        // `--force` can still retract it (heddle#316 r13).
        match old {
            Some(old) if recorded == Some(old) || force => {
                deletes.push(PlannedRefDelete {
                    full_name: full,
                    old,
                });
                // Deleted ⇒ no longer owned ⇒ drops from the record.
            }
            Some(_) => {
                // Out-of-band tip heddle never published — skip the delete; retain
                // ownership so `--force` remains the explicit escape hatch.
                if let Some(recorded) = recorded {
                    new_manifest.insert(full, recorded);
                }
            }
            None => {
                // Already absent at the destination — no op; drops from the record.
            }
        }
    }

    Ok(DestinationReconcilePlan {
        writes,
        deletes,
        new_manifest,
    })
}

/// The destination's current ref tips (full name → oid) across the namespaces
/// heddle manages (heads, tags, notes) — the `old_at_destination` input to
/// [`plan_destination_reconcile`] for a local-path destination.
fn read_destination_ref_map(repo: &SleyRepository) -> GitResult<HashMap<String, ObjectId>> {
    Ok(collect_ref_updates(repo)?
        .iter()
        .map(|update| (full_ref_name(update), update.target))
        .collect())
}

pub(crate) fn apply_ref_updates(
    repo: &SleyRepository,
    updates: &[RefUpdate],
    log_message: &str,
) -> GitResult<()> {
    for update in updates {
        let full_name = full_ref_name(update);
        set_reference(
            repo,
            &full_name,
            update.target,
            RefPrecondition::Any,
            log_message,
        )?;
    }
    Ok(())
}

fn apply_remote_tracking_ref_updates(
    repo: &SleyRepository,
    remote_name: &str,
    updates: &[RefUpdate],
    log_message: &str,
) -> GitResult<()> {
    reject_reserved_git_remote_name(remote_name)?;
    for update in updates
        .iter()
        .filter(|update| update.namespace == RefNamespace::Branch)
    {
        set_reference(
            repo,
            &format!("refs/remotes/{remote_name}/{}", update.name),
            update.target,
            RefPrecondition::Any,
            log_message,
        )?;
    }
    Ok(())
}

/// Copy a local Git repository into a bare repository without invoking Git
/// transport helpers. This is the local-path clone fast path used by the OSS
/// Git-overlay workflow when the user does not have `git` installed.
pub fn copy_local_repo_to_bare(source_path: &Path, dest: &Path) -> GitResult<()> {
    fs::create_dir_all(dest)?;
    let source = open_repo(source_path)?;
    let target = match SleyRepository::open(dest) {
        Ok(repo) => repo,
        Err(_) => SleyRepository::init_bare(dest).map_err(git_err)?,
    };
    let updates = collect_ref_updates(&source)?;
    copy_reachable_objects(&source, &target, updates.iter().map(|update| update.target))?;
    apply_ref_updates(
        &target,
        &updates,
        &format!("heddle: clone from {}", source_path.display()),
    )?;

    // Mirror the source repo's HEAD: if the source is on `master` (or
    // `develop`, or anything non-`main`) but happens to also have a
    // `main` branch, the previous logic silently moved the user to
    // `main` on clone. Read the source's symbolic HEAD target and
    // honour it whenever it points at a branch we actually copied.
    // Fall back to `main` (then any first branch) only when the source
    // HEAD is detached or points at a branch we did not import.
    let copied_branches: HashSet<&str> = updates
        .iter()
        .filter(|update| update.namespace == RefNamespace::Branch)
        .map(|update| update.name.as_str())
        .collect();
    let source_head_branch = source
        .head()
        .ok()
        .and_then(|head| head.branch_name().map(str::to_owned))
        .filter(|branch| copied_branches.contains(branch.as_str()));
    if let Some(branch) = source_head_branch {
        write_head_symref(dest, &format!("refs/heads/{branch}"))?;
    } else if copied_branches.contains("main") {
        write_head_symref(dest, "refs/heads/main")?;
    } else if let Some(first_branch) = updates
        .iter()
        .find(|update| update.namespace == RefNamespace::Branch)
    {
        write_head_symref(dest, &format!("refs/heads/{}", first_branch.name))?;
    }
    Ok(())
}

/// Clone a remote git URL into `dest` as a bare repository, fetching all
/// branches and tags. Mirrors the sley remote fetch path used by
/// `fetch_network_remote` but starts from an empty `init_bare` rather than an
/// existing repo.
///
/// Used by `bridge import --path <URL>` (Phase F): we clone into a
/// scratch directory under the heddle repo's `.heddle/tmp/` and feed the
/// resulting bare repo into the normal import path. Also used by `clone`
/// for Git-overlay URLs, where `depth` carries through to a shallow clone.
///
/// * `depth` — if `Some(n)` with `n >= 1`, a shallow clone with that
///   many commits per ref for network transports (transport-v2
///   `deepen <n>` capability). `file://` URLs use the native local-copy
///   path so they do not spawn Git upload-pack helpers; shallow local
///   copies are rejected until Heddle has native shallow-object pruning.
/// * `filter` — currently rejected. Heddle's Git-overlay runtime is
///   intentionally Git-compatible but not Git-binary-dependent, and the
///   native transport path does not yet expose partial-clone filtering.
pub fn clone_url_to_bare(
    url: &str,
    dest: &Path,
    depth: Option<u32>,
    filter: Option<&str>,
) -> GitResult<()> {
    // Public Git-overlay workflows must run on machines with no Git executable
    // installed. Keep depth-only clones native and reject filtered clones until
    // the importer can tolerate missing objects.
    if let Some(spec) = filter {
        return Err(GitBridgeError::Git(format!(
            "partial Git clone filter `{spec}` is not supported in Heddle's native no-git runtime yet; retry without --filter/--lazy so Heddle can import a complete object graph"
        )));
    }
    if let Some(source_path) = local_path_from_url(url)? {
        if depth.is_some() {
            return Err(GitBridgeError::Git(
                "shallow file:// Git clones are not supported in Heddle's native no-git runtime yet; retry without --depth so Heddle can copy the local Git object graph without spawning Git transport helpers"
                    .to_string(),
            ));
        }
        return copy_local_repo_to_bare(&source_path, dest);
    }
    let default_branch =
        clone_url_to_bare_via_sley(url, dest, depth)?.or_else(|| default_branch_from_file_url(url));
    // `init_bare` writes `.git/HEAD = ref: refs/heads/<init.defaultBranch>`
    // (typically "main" or "master") regardless of what the remote
    // advertises, and the fetch above doesn't touch HEAD. If we leave
    // that in place, downstream `select_clone_thread` and
    // `detect_git_head` will steer the user to a branch the remote may
    // not even have — observed: cloning ripgrep landed users on
    // `ag/bstr-migration` (alphabetically first imported thread) when
    // the remote's actual default is `master`. Honour the remote's
    // `HEAD` symref when we can resolve it.
    if let Some(branch) = default_branch
        && bare_branch_exists(dest, &branch)?
    {
        write_head_symref(dest, &format!("refs/heads/{branch}"))?;
    }
    Ok(())
}

fn default_branch_from_file_url(url: &str) -> Option<String> {
    let source_path = local_path_from_url(url).ok().flatten()?;
    let head_path = if source_path.join("HEAD").is_file() {
        source_path.join("HEAD")
    } else {
        source_path.join(".git").join("HEAD")
    };
    let head = fs::read_to_string(head_path).ok()?;
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    (!branch.is_empty()).then(|| branch.to_string())
}

fn bare_branch_exists(repo_path: &Path, branch: &str) -> GitResult<bool> {
    let repo = open_repo(repo_path)?;
    Ok(repo
        .find_reference(&format!("refs/heads/{branch}"))
        .map_err(git_err)?
        .is_some())
}

fn clone_url_to_bare_via_sley(
    url: &str,
    dest: &Path,
    depth: Option<u32>,
) -> GitResult<Option<String>> {
    fs::create_dir_all(dest)?;
    let repo = SleyRepository::init_bare(dest).map_err(git_err)?;
    let mut credentials = NoCredentials;
    let mut progress = SilentProgress;
    let outcome = repo
        .fetch(
            url,
            &heddle_mirror_fetch_refspecs()?,
            FetchOptions {
                quiet: true,
                auto_follow_tags: true,
                fetch_all_tags: true,
                prune: false,
                dry_run: false,
                append: false,
                write_fetch_head: true,
                tag_option_explicit: true,
                prune_option_explicit: true,
                prune_tags: false,
                prune_tags_option_explicit: false,
                refmap: None,
                refetch: false,
                record_promisor_refs: false,
                update_head_ok: false,
                ssh_options: None,
                atomic: false,
                depth,
                merge_srcs: Vec::new(),
                filter: None,
                cloning: true,
                update_shallow: false,
                deepen_relative: false,
                deepen_since: None,
                deepen_not: Vec::new(),
            },
            &mut credentials,
            &mut progress,
        )
        .map_err(|err| GitBridgeError::Git(format!("clone failed for {url}: {err}")))?;
    Ok(outcome
        .head_symref
        .and_then(|target| target.strip_prefix("refs/heads/").map(str::to_string)))
}

/// Materialize the checkout `.git` object closure for the commit mapped to
/// `tip_state_id` (`tip_oid`) — reconstructing every byte-faithful commit from
/// heddle state, and copying only the lossy residual from the eager `.heddle/git`
/// mirror (#568 P1).
///
/// Walks the heddle state DAG from `tip_state_id`. For each visited state:
///   * its mapped git OID is already in `excluded` (the prior checkout HEAD's full
///     closure, already on disk) ⇒ skip it AND its ancestors — that subgraph is
///     present;
///   * [`commit_is_byte_faithful`] ⇒ reconstruct the commit object (and, via
///     [`reconstruct_commit_bytes`]'s [`export_tree`], its whole tree/blob closure)
///     directly into `object_repo`, then recurse into its parents;
///   * otherwise (lossy: `--lossy` import or non-UTF8 identity — the residual the
///     mirror exclusively holds) ⇒ copy that commit's full reachable closure from
///     `mirror_repo` and DO NOT recurse (the copy already brought its ancestry).
///
/// CRITICAL safety gate: every reconstructed commit's git OID MUST equal the
/// mapped `git_oid`. A mismatch means reconstruction diverged from the imported
/// bytes (an unmodeled fidelity gap), which would silently materialize a
/// wrong-OID checkout — so this HARD-ERRORS instead. This assertion is what lets
/// the reconstruction path be trusted as a mirror replacement.
///
/// Output is byte-identical to the prior `copy_reachable_objects_excluding(mirror
/// → checkout)`: git objects are content-addressed, so a faithful reconstruction
/// lands the exact same OID the mirror copy would have, and the lossy path copies
/// verbatim. The exclude set keeps it O(objects new since the parent).
#[allow(clippy::too_many_arguments)]
pub(crate) fn materialize_checkout_closure_from_state(
    heddle_repo: &HeddleRepository,
    mapping: &SyncMapping,
    mirror_repo: &SleyRepository,
    object_repo: &SleyRepository,
    tip_state_id: &ChangeId,
    tip_oid: ObjectId,
    excluded: &HashSet<ObjectId>,
) -> GitResult<()> {
    // Lossy commits whose closure is copied verbatim from the mirror. Their roots
    // are batched and copied once at the end (a single excluding pack install,
    // matching the prior single-copy perf shape) rather than per-commit.
    let mut lossy_roots: Vec<ObjectId> = Vec::new();
    let mut stack: Vec<ChangeId> = vec![*tip_state_id];
    let mut seen: HashSet<ChangeId> = HashSet::new();

    while let Some(state_id) = stack.pop() {
        if !seen.insert(state_id) {
            continue;
        }
        let Some(git_oid) = resolve_mapped_git_oid(heddle_repo, mapping, &state_id, object_repo)?
        else {
            // No mapping for this state: it was never exported (e.g. an embargoed
            // ancestor withheld from the served frontier). The tip itself always
            // resolves (`tip_oid`), and a withheld ancestor's git object is, by
            // construction, absent from both store-reconstruction and the served
            // mirror — so there is nothing to materialize. Skip without recursing.
            continue;
        };

        // Already on disk (this state's object is in the parent's excluded closure,
        // or a sibling branch already materialized it): the whole subgraph beneath
        // it is present too, so prune here.
        if excluded.contains(&git_oid) || object_repo.read_object(&git_oid).is_ok() {
            continue;
        }

        let state = heddle_repo
            .store()
            .get_state(&state_id)?
            .ok_or(GitBridgeError::StateNotFound(state_id))?;

        if commit_is_byte_faithful(&state) {
            let content = reconstruct_commit_bytes(heddle_repo, object_repo, mapping, &state)?;
            // The byte-exact gate (#568 P1): a faithful reconstruction MUST hash to
            // the mapped OID. If it does not, refuse — never write a wrong-SHA
            // object into the worktree.
            let reconstructed = commit_object_id(&content);
            if reconstructed != git_oid {
                return Err(GitBridgeError::Git(format!(
                    "checkout reconstruction OID mismatch for state {state_id}: reconstructed {reconstructed}, expected mapped {git_oid}; \
                     refusing to materialize a wrong-OID checkout (unmodeled fidelity gap)"
                )));
            }
            let written = write_commit_object(object_repo, &content)?;
            debug_assert_eq!(written, git_oid);
            stack.extend(state.parents.iter().copied());
        } else {
            // Lossy residual: the verbatim bytes live only in the mirror. Copy this
            // commit's full closure from there and stop — the copy carries its
            // ancestry, so we don't reconstruct (or re-copy) beneath it.
            lossy_roots.push(git_oid);
        }
    }

    // Ensure the requested tip is materialized even in the degenerate case where
    // the walk skipped it (e.g. an unmapped store state that nonetheless has a
    // mirror object): fall back to the mirror copy for it. The faithful path above
    // already wrote it when reconstructable, and a redundant root here is pruned
    // by the exclude set / idempotent install.
    if object_repo.read_object(&tip_oid).is_err() && !lossy_roots.contains(&tip_oid) {
        lossy_roots.push(tip_oid);
    }

    if !lossy_roots.is_empty() {
        copy_reachable_objects_excluding(mirror_repo, object_repo, lossy_roots, excluded)?;
    }

    Ok(())
}

/// Resolve the git OID a heddle state maps to, preferring the in-memory bridge
/// mapping and falling back to the git-overlay checkpoint mapping (the same
/// resolution the checkout tip uses). Returns `None` when the state has no mapped
/// git object at all.
fn resolve_mapped_git_oid(
    heddle_repo: &HeddleRepository,
    mapping: &SyncMapping,
    state_id: &ChangeId,
    object_repo: &SleyRepository,
) -> GitResult<Option<ObjectId>> {
    if let Some(git_oid) = mapping.get_git(state_id) {
        return Ok(Some(git_oid));
    }
    if let Some(git_commit) = heddle_repo
        .git_overlay_mapped_git_commit_for_change(state_id)
        .map_err(|error| GitBridgeError::Git(error.to_string()))?
    {
        let oid = ObjectId::from_hex(object_repo.object_format(), &git_commit)
            .map_err(|error| GitBridgeError::InvalidMapping(error.to_string()))?;
        return Ok(Some(oid));
    }
    Ok(None)
}

pub(crate) fn copy_reachable_objects(
    source: &SleyRepository,
    target: &SleyRepository,
    roots: impl IntoIterator<Item = ObjectId>,
) -> GitResult<()> {
    // TODO: Keep local Git-lane reachable transfer behind Sley primitives. If
    // this needs pack identity/stream planning, route it through the Sley
    // reachable-pack facade gate instead of adding a Heddle-local planner.
    let roots = roots.into_iter().collect::<Vec<_>>();
    target.copy_reachable_from(source, &roots).map_err(git_err)
}

/// Incremental variant of [`copy_reachable_objects`]: copy the closure
/// reachable from `roots`, skipping every object in `excluded`.
///
/// INVARIANT: every OID in `excluded` MUST already be present in `target` — the
/// walk neither visits nor copies an excluded object (nor anything reachable only
/// through it), so excluding an object the target is missing would silently drop
/// it. Callers satisfy this by computing `excluded` as the reachable closure of
/// something already in `target`. Used by checkpoint write-through, which passes
/// the prior checkout HEAD's full closure (already entirely in the checkout's
/// object DB): the new commit's tree re-reaches the parent's unchanged
/// trees/blobs, so excluding the whole closure — not just the parent commit —
/// prunes them all, turning per-checkpoint object transfer from O(total history)
/// into O(objects new since the parent). Output is byte-identical — the same
/// objects end up in `target`; the pruned ones were already there.
pub(crate) fn copy_reachable_objects_excluding(
    source: &SleyRepository,
    target: &SleyRepository,
    roots: impl IntoIterator<Item = ObjectId>,
    excluded: &HashSet<ObjectId>,
) -> GitResult<()> {
    if excluded.is_empty() {
        return copy_reachable_objects(source, target, roots);
    }
    if source.object_format() != target.object_format() {
        // Mismatched formats can't share objects; fall back to the plain copy so
        // its existing format-mismatch error surfaces unchanged.
        return copy_reachable_objects(source, target, roots);
    }
    // TODO: This local incremental transfer already delegates pack installation
    // to Sley. Keep future reachable-pack planning Sley-gated here too; Heddle
    // should not grow its own exclusion-aware pack planner.
    sley::plumbing::sley_odb::install_reachable_pack_excluding(
        source.objects().as_ref(),
        target.objects().as_ref(),
        target.object_format(),
        roots,
        excluded,
    )
    .map_err(|error| GitBridgeError::Git(error.to_string()))?;
    // Make the freshly-installed pack visible to subsequent reads on `target`,
    // mirroring what `copy_reachable_from` does internally.
    target.refresh_objects();
    Ok(())
}

fn fetch_network_remote(
    mirror_repo: &SleyRepository,
    remote_name: &str,
    url: &str,
    scope: GitFetchScope,
) -> GitResult<()> {
    let mut credentials = NoCredentials;
    let mut progress = SilentProgress;
    mirror_repo
        .fetch(
            url,
            &heddle_mirror_fetch_refspecs()?,
            FetchOptions {
                quiet: true,
                auto_follow_tags: matches!(scope, GitFetchScope::AllRefs),
                fetch_all_tags: matches!(scope, GitFetchScope::AllRefs),
                prune: false,
                dry_run: false,
                append: false,
                write_fetch_head: true,
                tag_option_explicit: true,
                prune_option_explicit: true,
                prune_tags: false,
                prune_tags_option_explicit: false,
                refmap: None,
                refetch: false,
                record_promisor_refs: false,
                update_head_ok: false,
                ssh_options: None,
                atomic: false,
                depth: None,
                merge_srcs: Vec::new(),
                filter: None,
                cloning: false,
                update_shallow: false,
                deepen_relative: false,
                deepen_since: None,
                deepen_not: Vec::new(),
            },
            &mut credentials,
            &mut progress,
        )
        .map_err(|err| GitBridgeError::Git(format!("failed to fetch from {url}: {err}")))?;
    let _ = remote_name;
    Ok(())
}

/// Push the served frontier to a URL/network remote. Returns the sorted
/// full names of the refs written on the wire (see [`planned_write_names`]).
fn push_network_remote(
    mirror_repo: &SleyRepository,
    heddle_dir: &Path,
    url: &str,
    scope: GitPushScope,
    current_branch: Option<&str>,
    force: bool,
) -> GitResult<Vec<String>> {
    // The network destination's exported-refs record lives in heddle's own dir,
    // keyed by the remote URL (the remote has no local git dir to host the
    // sidecar). Read it BEFORE the empty-frontier fast-path: a retraction lands
    // here with an EMPTY served set yet a non-empty record, so the delete-set —
    // not the served set — is what must still propagate (heddle#316 r11).
    let manifest_path = network_exported_refs_path(heddle_dir, url);
    let previously_exported = read_exported_refs_at(&manifest_path)?;
    // The WHOLE-MIRROR served frontier — the SAME projection the local-path
    // destination reconciles against and the mirror reconcile materialized
    // (heddle#316 r16). A scoped push reconciles the destination against this
    // whole frontier, so an out-of-scope ref the mirror rewound for embargo
    // propagates to the wire by construction, never a scope-filtered subset.
    //
    // Managed-filtered (heddle#316): the same foreign-ref exclusion the
    // local-path push applies — a foreign branch/tag heddle never wrote is kept
    // off the wire, sourced from the mirror's name-keyed managed-refs record.
    let managed_record = read_mirror_managed_refs(mirror_repo)?;
    let served_frontier = collect_managed_ref_updates(mirror_repo, &managed_record)?;
    if served_frontier.is_empty() && previously_exported.is_empty() {
        return Ok(Vec::new());
    }

    let mut credentials = NoCredentials;
    let records = mirror_repo
        .ls_remote(
            url,
            LsRemoteFilter {
                heads: false,
                tags: false,
                refs_only: true,
            },
            &|_| true,
            &mut credentials,
        )
        .map_err(|err| GitBridgeError::Git(format!("failed to list refs from {url}: {err}")))?;
    let remote_refs = records
        .into_iter()
        .filter(|record| {
            record.name.starts_with("refs/heads/")
                || record.name.starts_with("refs/tags/")
                || record.name.starts_with("refs/notes/")
        })
        .map(|record| (record.name, record.oid))
        .collect::<HashMap<_, _>>();

    // The SAME served-frontier plan the local-path destination runs: writes
    // (forcing embargo rewinds, rejecting forks), the retraction delete-set
    // (scoped to heddle-owned refs — never foreign), and the new record to
    // persist — all derived from the whole-mirror `served_frontier` above.
    let creatable = creatable_ref_names(&served_frontier, scope, current_branch);
    let plan = plan_destination_reconcile(
        mirror_repo,
        &served_frontier,
        creatable.as_ref(),
        &remote_refs,
        &previously_exported,
        force,
    )?;

    if plan.writes.is_empty() && plan.deletes.is_empty() {
        // Nothing to move on the wire, but the record may still need to drop a
        // ref that was already absent at the remote.
        write_exported_refs_at(&manifest_path, &plan.new_manifest)?;
        return Ok(Vec::new());
    }

    let mut commands = Vec::with_capacity(plan.writes.len() + plan.deletes.len());
    let mut pack_objects = Vec::with_capacity(plan.writes.len());
    let force_transport_checks = plan.writes.iter().any(|write| write.force);
    for write in &plan.writes {
        commands.push(PushCommand {
            src: Some(write.new),
            dst: write.full_name.clone(),
            expected_old: write.old,
            force: write.force,
        });
        pack_objects.push(write.new);
    }
    for delete in &plan.deletes {
        commands.push(PushCommand {
            src: None,
            dst: delete.full_name.clone(),
            expected_old: Some(delete.old),
            force: false,
        });
    }

    let mut credentials = NoCredentials;
    let mut progress = SilentProgress;
    mirror_repo
        .push_actions(
            url,
            PushActionPlan {
                commands,
                pack_objects,
                options: PushOptions {
                    quiet: true,
                    force: force || force_transport_checks,
                },
            },
            &mut credentials,
            &mut progress,
        )
        .map_err(|err| GitBridgeError::Git(format!("push failed for {url}: {err}")))?;
    // Only persist the record once the remote has acknowledged every command, so
    // a failed push never leaves a ref recorded as exported that did not land.
    write_exported_refs_at(&manifest_path, &plan.new_manifest)?;
    Ok(planned_write_names(&plan))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_git_ref_local_branch() {
        let parsed = parse_git_ref("refs/heads/main").expect("local branch parses");
        assert_eq!(parsed.kind, GitRefKind::Branch);
        assert_eq!(parsed.name, "main");
        assert_eq!(parsed.remote, REMOTE_NAME_FOR_LOCAL_GIT_REPO);
    }

    #[test]
    fn parse_git_ref_remote_branch_keeps_nested_name() {
        let parsed = parse_git_ref("refs/remotes/origin/feature/x").expect("remote branch parses");
        assert_eq!(parsed.kind, GitRefKind::Branch);
        assert_eq!(parsed.name, "feature/x");
        assert_eq!(parsed.remote, "origin");
    }

    #[test]
    fn parse_git_ref_tag() {
        let parsed = parse_git_ref("refs/tags/v1.0").expect("tag parses");
        assert_eq!(parsed.kind, GitRefKind::Tag);
        assert_eq!(parsed.name, "v1.0");
        assert_eq!(parsed.remote, REMOTE_NAME_FOR_LOCAL_GIT_REPO);
    }

    #[test]
    fn parse_git_ref_skips_head_symrefs() {
        assert_eq!(parse_git_ref("refs/heads/HEAD"), None);
        assert_eq!(parse_git_ref("refs/remotes/origin/HEAD"), None);
    }

    #[test]
    fn parse_git_ref_rejects_unknown_or_malformed() {
        assert_eq!(parse_git_ref("refs/notes/heddle"), None);
        assert_eq!(parse_git_ref("HEAD"), None);
        // A remote ref with no branch component beneath the remote name.
        assert_eq!(parse_git_ref("refs/remotes/origin"), None);
    }

    #[test]
    fn parse_git_ref_rejects_reserved_git_remote_namespace() {
        // A user remote literally named `git` collides with the local sentinel;
        // it must not be aliased onto local refs at the parse site.
        assert_eq!(parse_git_ref("refs/remotes/git/main"), None);
        assert_eq!(parse_git_ref("refs/remotes/git/feature/x"), None);
        assert!(is_reserved_git_remote_name(REMOTE_NAME_FOR_LOCAL_GIT_REPO));
        assert!(!is_reserved_git_remote_name("origin"));
    }

    #[test]
    fn local_path_from_url_rejects_hosted_heddle_scheme() {
        // Regression (push-routing no-op): a `heddle://` hosted remote that
        // reaches the git-overlay exporter must be a HARD ERROR, never a
        // silent no-op success. The git network pusher cannot speak the
        // hosted protocol, so classifying a `heddle://` URL here must fail
        // loudly rather than fall through to `ResolvedRemote::Url` (which
        // would "reconcile" locally and report success without ever
        // contacting the server).
        let err = local_path_from_url("heddle://weft.local:8421/org/repo")
            .expect_err("heddle:// must be rejected by the git exporter classifier");
        let msg = err.to_string();
        assert!(
            msg.contains("heddle://") && msg.contains("hosted"),
            "error should explain the hosted scheme cannot be pushed via the git-overlay exporter, got: {msg}"
        );
    }

    #[test]
    fn local_path_from_url_still_accepts_file_and_git_urls() {
        // The guard must not regress legitimate transports: `file://` still
        // resolves to a local path, and ordinary git URLs (https/ssh) still
        // pass through as "not local" (Ok(None)) for the network git pusher.
        assert!(
            local_path_from_url("file:///tmp/repo.git")
                .expect("file url ok")
                .is_some(),
            "file:// must still resolve to a local path"
        );
        assert!(
            local_path_from_url("https://example.com/org/repo.git")
                .expect("https url ok")
                .is_none(),
            "https git url must pass through as a network URL"
        );
        assert!(
            local_path_from_url("git@github.com:org/repo.git")
                .expect("ssh url ok")
                .is_none(),
            "ssh git url must pass through as a network URL"
        );
    }

    #[test]
    fn refspec_forced_round_trips_git_format() {
        let spec =
            RefSpec::forced("refs/heads/main", "refs/heads/main").expect("valid forced refspec");
        assert_eq!(spec.to_git_format(), "+refs/heads/main:refs/heads/main");
        assert_eq!(
            spec.to_git_format_not_forced(),
            "refs/heads/main:refs/heads/main"
        );
    }

    #[test]
    fn refspec_constructor_rejects_reserved_remote_name() {
        let err = RefSpec::new(
            Some("refs/remotes/git/main".to_string()),
            "refs/heads/main",
            false,
        )
        .expect_err("reserved remote source is rejected");
        assert!(err.to_string().contains("reserved namespace"));

        let err = RefSpec::new(
            Some("refs/heads/main".to_string()),
            "refs/remotes/git/main",
            false,
        )
        .expect_err("reserved remote destination is rejected");
        assert!(err.to_string().contains("reserved namespace"));
    }

    #[test]
    fn refspec_forced_rejects_reserved_remote_name() {
        assert!(RefSpec::forced("refs/remotes/git/main", "refs/heads/main").is_err());
        assert!(RefSpec::forced("refs/heads/main", "refs/remotes/git/main").is_err());
    }

    #[test]
    fn refspec_delete_has_empty_source() {
        let spec = RefSpec::delete("refs/heads/stale").expect("valid delete refspec");
        assert_eq!(spec.to_git_format(), ":refs/heads/stale");
        assert_eq!(spec.to_git_format_not_forced(), ":refs/heads/stale");
    }

    #[test]
    fn refspec_delete_rejects_reserved_remote_name() {
        assert!(RefSpec::delete("refs/remotes/git/stale").is_err());
    }

    #[test]
    fn refspec_constructor_rejects_empty_source_and_destination() {
        let err = RefSpec::new(None, "", false)
            .expect_err("empty source plus empty destination is rejected");
        assert!(err.to_string().contains("cannot both be empty"));
    }

    #[test]
    fn negative_refspec_prefixes_caret() {
        let spec = NegativeRefSpec::new("refs/heads/wip").expect("valid negative refspec");
        assert_eq!(spec.to_git_format(), "^refs/heads/wip");
    }

    #[test]
    fn negative_refspec_constructor_rejects_unparseable_negation() {
        let err = NegativeRefSpec::new("refs/heads/wip/*").expect_err("negative glob is rejected");
        assert!(err.to_string().contains("Negative glob patterns"));
    }

    #[test]
    fn negative_refspec_constructor_rejects_reserved_remote_name() {
        let err = NegativeRefSpec::new("refs/remotes/git/main")
            .expect_err("reserved remote negative source is rejected");
        assert!(err.to_string().contains("reserved namespace"));
    }

    #[test]
    fn mirror_fetch_refspecs_cover_branches_and_notes() {
        assert_eq!(
            heddle_mirror_fetch_refspecs().expect("mirror refspecs are valid"),
            [
                "+refs/heads/*:refs/heads/*".to_string(),
                "+refs/notes/*:refs/notes/*".to_string(),
            ]
        );
    }

    #[test]
    fn scoped_import_ref_updates_do_not_include_notes_implicitly() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = SleyRepository::init_bare(tmp.path()).expect("init bare repo");
        let main = seed_commit(&repo, "main");
        let other = seed_commit(&repo, "other");
        let notes = seed_commit(&repo, "notes");
        set_reference(
            &repo,
            "refs/heads/main",
            main,
            RefPrecondition::MustNotExist,
            "test: main",
        )
        .expect("write main");
        set_reference(
            &repo,
            "refs/heads/other",
            other,
            RefPrecondition::MustNotExist,
            "test: other",
        )
        .expect("write other");
        set_reference(
            &repo,
            "refs/notes/heddle",
            notes,
            RefPrecondition::MustNotExist,
            "test: notes",
        )
        .expect("write notes");

        let updates = collect_import_source_ref_updates(&repo, &["main".to_string()])
            .expect("collect scoped updates");
        let full_names = updates.iter().map(full_ref_name).collect::<Vec<_>>();

        assert_eq!(full_names, vec!["refs/heads/main".to_string()]);
    }

    #[test]
    fn fast_forward_guard_reports_exact_rewrite_before_after() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = SleyRepository::init_bare(tmp.path()).expect("init bare repo");
        let root = test_commit(&repo, "root", &[]);
        let old = test_commit(&repo, "old", &[root]);
        let new = test_commit(&repo, "new", &[root]);

        let err = ensure_commit_update_fast_forward(&repo, "refs/heads/main", old, new)
            .expect_err("sibling commit update should be refused");
        let message = err.to_string();
        assert!(message.contains("refs/heads/main"));
        assert!(message.contains(&old.to_string()));
        assert!(message.contains(&new.to_string()));
        assert!(message.contains("refusing to replace"));
    }

    #[test]
    fn fast_forward_guard_allows_descendant_update() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = SleyRepository::init_bare(tmp.path()).expect("init bare repo");
        let old = test_commit(&repo, "old", &[]);
        let new = test_commit(&repo, "new", &[old]);

        ensure_commit_update_fast_forward(&repo, "refs/heads/main", old, new)
            .expect("descendant update should be allowed");
    }

    fn test_commit(repo: &SleyRepository, message: &str, parents: &[ObjectId]) -> ObjectId {
        let empty_tree_oid = ObjectId::empty_tree(repo.object_format());
        let sig = Signature {
            name: GitByteString::new(b"Heddle Test".to_vec()),
            email: GitByteString::new(b"heddle@test".to_vec()),
            time: GitTime::new(0, 0),
            raw: b"Heddle Test <heddle@test> 0 +0000".to_vec(),
        };
        let commit = sley::CommitObject {
            tree: empty_tree_oid,
            parents: parents.to_vec(),
            author: sig.to_ident_bytes(),
            committer: sig.to_ident_bytes(),
            encoding: None,
            message: message.as_bytes().to_vec(),
        };
        repo.write_object(sley::plumbing::sley_object::EncodedObject::new(
            GitObjectType::Commit,
            commit.write(),
        ))
        .expect("write test commit")
    }

    fn seed_commit(repo: &SleyRepository, message: &str) -> ObjectId {
        test_commit(repo, message, &[])
    }

    /// heddle#141 regression: when the URL-fetch path of
    /// `clone_url_to_bare` runs against a bare repo whose `HEAD`
    /// points at a branch that is *not* alphabetically first (and
    /// crucially, not what sley's `init_bare` defaults to), the
    /// resulting dest bare must have `HEAD` pointing at the remote
    /// default — not sley's init-time guess.
    #[test]
    fn clone_url_to_bare_via_sley_honours_remote_head_symref() {
        let tmp = tempfile::TempDir::new().unwrap();
        let source = tmp.path().join("source.git");
        let dest = tmp.path().join("dest.git");

        // Build a bare source with two branches under
        // deliberately-non-default names: `trunk` (will be the
        // remote default — neither sley's `init.defaultBranch` nor
        // the alphabetically-first imported ref would land here by
        // accident) and `abc-feature` (alphabetically first — what
        // the buggy fallback used to pick).
        let src = SleyRepository::init_bare(&source).expect("init bare source");
        let seed = seed_commit(&src, "seed");
        for name in ["refs/heads/trunk", "refs/heads/abc-feature"] {
            set_reference(&src, name, seed, RefPrecondition::Any, "test: seed branch")
                .expect("set ref");
        }
        // Make sure HEAD on the source points at trunk so
        // `git ls-remote --symref` reports trunk.
        std::fs::write(source.join("HEAD"), b"ref: refs/heads/trunk\n").unwrap();

        let url = format!("file://{}", source.display());
        clone_url_to_bare(&url, &dest, None, None).expect("clone url to bare");

        let dest_head = std::fs::read_to_string(dest.join("HEAD")).expect("read dest HEAD");
        assert_eq!(
            dest_head.trim(),
            "ref: refs/heads/trunk",
            "dest HEAD must mirror the remote's symref (trunk), not sley's \
             init-time default and not the alphabetically-first branch \
             (abc-feature) — see heddle#141"
        );
    }

    #[test]
    fn write_head_symref_is_atomic_and_round_trips() {
        let tmp = tempfile::TempDir::new().unwrap();
        let git_dir = tmp.path();

        write_head_symref(git_dir, "refs/heads/feature/x").expect("write HEAD symref");

        // (a) No leftover temp file — the rename consumed it.
        assert!(
            !git_dir.join("HEAD.tmp").exists(),
            "atomic writer must not leave HEAD.tmp behind"
        );

        // (b) Exact content, including the trailing newline.
        let contents = std::fs::read_to_string(git_dir.join("HEAD")).expect("read HEAD");
        assert_eq!(contents, "ref: refs/heads/feature/x\n");

        // (c) Round-trips through the same symref parse `read_git_head_branch`
        // (clone.rs) and `detect_git_head` use.
        let branch = contents
            .trim()
            .strip_prefix("ref: ")
            .and_then(|s| s.strip_prefix("refs/heads/"))
            .expect("HEAD parses as a branch symref");
        assert_eq!(branch, "feature/x");

        // Overwriting an existing HEAD is also clean.
        write_head_symref(git_dir, "refs/heads/main").expect("rewrite HEAD symref");
        assert!(!git_dir.join("HEAD.tmp").exists());
        assert_eq!(
            std::fs::read_to_string(git_dir.join("HEAD")).unwrap(),
            "ref: refs/heads/main\n"
        );
    }
}
