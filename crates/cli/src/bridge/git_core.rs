// SPDX-License-Identifier: Apache-2.0
//! Core Git bridge types and operations.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write,
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{SystemTime, UNIX_EPOCH},
};

use gix::{
    bstr::ByteSlice,
    hash::{Kind as ObjectHashKind, ObjectId},
    refs::{
        Target,
        transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog},
    },
};
use gix_transport::{
    Protocol, Service,
    client::{MessageKind, WriteMode, blocking_io::Transport},
};
use objects::{
    error::HeddleError,
    object::{ChangeId, ChangeIdParseError, Principal, Tree},
    store::ObjectStore,
};
use refs::Head;
use repo::Repository as HeddleRepository;

use super::{
    git_export::{export_all, export_current_thread},
    git_import::import_all,
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

#[derive(Debug, Clone)]
enum ResolvedRemote {
    Local(PathBuf),
    Url(gix::Url),
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

    pub(crate) fn to_signature(&self, seconds: i64) -> gix::actor::Signature {
        gix::actor::Signature {
            name: self.name.as_str().into(),
            email: self.email.as_str().into(),
            time: gix::date::Time { seconds, offset: 0 },
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
#[derive(Debug, Default)]
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
            return Err(GitBridgeError::Conflict(format!(
                "change id {} mapped to {} (new {})",
                change_id, existing, git_oid
            )));
        }

        if let Some(existing) = self.git_to_heddle.get(&git_oid)
            && *existing != change_id
        {
            return Err(GitBridgeError::Conflict(format!(
                "git oid {} mapped to {} (new {})",
                git_oid, existing, change_id
            )));
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

    /// Check if a mapping exists for a Git object id.
    pub fn has_git(&self, git_oid: ObjectId) -> bool {
        self.git_to_heddle.contains_key(&git_oid)
    }

    /// Iterate over mappings.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&ChangeId, &ObjectId)> {
        self.heddle_to_git.iter()
    }

    pub(crate) fn retain_git_objects(&mut self, repo: &gix::Repository) {
        let retained: Vec<(ChangeId, ObjectId)> = self
            .heddle_to_git
            .iter()
            .filter_map(|(change_id, git_oid)| {
                repo.find_object(*git_oid)
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
            .filter_map(|(change_id, git_oid)| {
                reachable
                    .contains(git_oid)
                    .then_some((*change_id, *git_oid))
            })
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
}

impl<'a> GitBridge<'a> {
    /// Trailer keys used in Git commit messages for Heddle metadata.
    pub(crate) const TRAILER_CHANGE_ID: &'static str = "Heddle-Change-Id";
    pub(crate) const TRAILER_AGENT: &'static str = "Heddle-Agent";
    pub(crate) const TRAILER_CONFIDENCE: &'static str = "Heddle-Confidence";
    pub(crate) const TRAILER_STATUS: &'static str = "Heddle-Status";

    /// Create a new Git bridge for a Heddle repository.
    pub fn new(heddle_repo: &'a HeddleRepository) -> Self {
        Self {
            heddle_repo,
            git_repo_path: None,
            mapping: SyncMapping::new(),
            commit_message_overrides: HashMap::new(),
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
            let _ = gix::init_bare(&git_dir).map_err(git_err)?;
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
    pub(crate) fn open_git_repo(&self) -> GitResult<gix::Repository> {
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

    /// Import Git commits into Heddle states.
    pub fn import(&mut self, git_path: Option<&Path>) -> GitResult<super::git_util::ImportStats> {
        import_all(self, git_path)
    }

    /// Push to a Git remote.
    pub fn push(&mut self, remote_name: &str) -> GitResult<()> {
        self.push_with_scope(remote_name, GitPushScope::AllThreads)
    }

    /// Push to a Git remote with an explicit ref scope.
    pub fn push_with_scope(&mut self, remote_name: &str, scope: GitPushScope) -> GitResult<()> {
        self.push_with_scope_force(remote_name, scope, false)
    }

    /// Push to a Git remote with an explicit ref scope and optional
    /// non-fast-forward ref movement.
    pub fn push_with_scope_force(
        &mut self,
        remote_name: &str,
        scope: GitPushScope,
        force: bool,
    ) -> GitResult<()> {
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

        let log_message = format!("heddle: push from {}", self.heddle_repo.root().display());
        match self.resolve_remote(remote_name, gix::remote::Direction::Push)? {
            ResolvedRemote::Local(target_path) => {
                let mirror_repo = self.open_git_repo()?;
                let updates =
                    collect_ref_updates_for_push(&mirror_repo, scope, current_branch.as_deref())?;
                self.copy_mirror_to_path_with_updates(
                    &target_path,
                    &log_message,
                    /* init_if_missing */ false,
                    &updates,
                    force,
                )
            }
            ResolvedRemote::Url(url) => {
                let mirror_repo = self.open_git_repo()?;
                let updates =
                    collect_ref_updates_for_push(&mirror_repo, scope, current_branch.as_deref())?;
                push_network_remote_with_updates(&mirror_repo, &url, &updates, force)
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
        Ok(thread)
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
        )?;
        Ok(stats)
    }

    /// Shared helper: copy every reachable object from the internal mirror to
    /// `target_path`, then mirror branch/tag refs onto it. When
    /// `init_if_missing` is true, the destination is created as a bare repo
    /// when it does not exist.
    fn copy_mirror_to_path(
        &mut self,
        target_path: &Path,
        log_message: &str,
        init_if_missing: bool,
    ) -> GitResult<()> {
        let mirror_repo = self.open_git_repo()?;
        let updates = collect_ref_updates(&mirror_repo)?;
        self.copy_mirror_to_path_with_updates(
            target_path,
            log_message,
            init_if_missing,
            &updates,
            false,
        )
    }

    fn copy_mirror_to_path_with_updates(
        &mut self,
        target_path: &Path,
        log_message: &str,
        init_if_missing: bool,
        updates: &[RefUpdate],
        force: bool,
    ) -> GitResult<()> {
        let mirror_repo = self.open_git_repo()?;
        let target_repo = if target_path.exists() {
            open_repo(target_path)?
        } else if init_if_missing {
            fs::create_dir_all(target_path)?;
            gix::init_bare(target_path).map_err(git_err)?;
            open_repo(target_path)?
        } else {
            return Err(GitBridgeError::Git(format!(
                "destination '{}' does not exist",
                target_path.display()
            )));
        };

        copy_reachable_objects(
            &mirror_repo,
            &target_repo,
            updates.iter().map(|update| update.target),
        )?;
        if !force {
            validate_ref_updates_fast_forward(&target_repo, updates)?;
        }
        apply_ref_updates(&target_repo, &updates, log_message)?;
        Ok(())
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
        self.init_mirror()?;
        let current_branch = self.heddle_repo.git_overlay_current_branch()?;
        let tracking_remote = checkout_tracking_remote_name(self.heddle_repo.root(), remote_name)?
            .or_else(|| {
                (!looks_like_remote_location(remote_name)).then(|| remote_name.to_string())
            });

        let mirror_repo = self.open_git_repo()?;
        match self.resolve_remote(remote_name, gix::remote::Direction::Fetch)? {
            ResolvedRemote::Local(path) => {
                let remote_repo = open_repo(&path)?;
                let updates = collect_ref_updates_for_fetch(&remote_repo, scope)?;
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

    /// Best-effort adoption preflight for raw `git clone` checkouts.
    ///
    /// Plain Git clones do not fetch `refs/notes/heddle` by default, but
    /// Heddle-pushed overlay remotes use that ref to preserve Git commit
    /// -> Heddle state identity. Before import, try each checkout-configured
    /// remote and mirror any available Heddle notes into both the internal
    /// mirror and the working checkout. Remote failures are deliberately
    /// non-fatal: offline Git history can still be adopted, and push
    /// fast-forward guards prevent a missing notes ref from overwriting
    /// one that exists upstream.
    pub(crate) fn hydrate_checkout_heddle_notes_from_configured_remotes(&mut self) -> bool {
        if checkout_note_ref_exists(self.heddle_repo.root()).unwrap_or(false) {
            return true;
        }

        let mut remotes = match checkout_remote_url_items(self.heddle_repo.root()) {
            Ok(remotes) => remotes,
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    "skipping configured remote note hydration before git-overlay adopt"
                );
                return false;
            }
        };
        remotes.sort_by(|left, right| {
            match (left.0.as_str() == "origin", right.0.as_str() == "origin") {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => left.0.cmp(&right.0),
            }
        });
        remotes.dedup_by(|a, b| a.0 == b.0);

        for (remote_name, remote_url_str) in remotes {
            let remote_url = match gix::url::parse(remote_url_str.as_str().into()) {
                Ok(url) => url,
                Err(err) => {
                    tracing::debug!(
                        remote = remote_name.as_str(),
                        error = %err,
                        "could not parse configured remote url for hydrate check"
                    );
                    continue;
                }
            };
            // Cheap discovery first: ls-refs the remote and check
            // whether `refs/notes/heddle` is even advertised. On a
            // single-user repo or a vanilla Git remote the answer is
            // No — and the full fetch we'd otherwise issue (which
            // walks the entire object graph for branches *and* notes)
            // burns 9+ seconds on a 76-commit repo. Skipping it cuts
            // first-time adopt time by ~70%. See PR 6.
            let span = tracing::info_span!(
                "hydrate.ls_refs",
                remote = remote_name.as_str(),
            )
            .entered();
            let advertised = self
                .remote_advertises_ref(&remote_url, "refs/notes/heddle")
                .unwrap_or(false);
            drop(span);
            if !advertised {
                tracing::debug!(
                    remote = remote_name.as_str(),
                    "remote does not advertise refs/notes/heddle; skipping hydrate fetch"
                );
                continue;
            }
            match self.fetch_with_scope(
                &remote_name,
                GitFetchScope::BranchesAndNotes,
                RefreshCheckoutAfterFetch::Yes,
            ) {
                Ok(()) if checkout_note_ref_exists(self.heddle_repo.root()).unwrap_or(false) => {
                    return true;
                }
                Ok(()) => {}
                Err(error) => {
                    tracing::debug!(
                        remote = remote_name.as_str(),
                        error = %error,
                        "configured remote did not provide Heddle notes during git-overlay adopt"
                    );
                }
            }
        }

        false
    }

    /// Ask a remote what refs it advertises (no pack download). Returns
    /// `Ok(true)` if the remote's ref advertisement includes the named
    /// ref. Used to skip the full hydrate fetch when the remote has no
    /// Heddle notes.
    ///
    /// Local file remotes are checked by opening the on-disk repo —
    /// cheaper still than a process roundtrip.
    fn remote_advertises_ref(&self, url: &gix::Url, ref_name: &str) -> GitResult<bool> {
        if url.scheme == gix::url::Scheme::File {
            let local = local_path_from_url(url)?;
            let repo = open_repo(&local)?;
            return Ok(repo.find_reference(ref_name).is_ok());
        }
        let mirror_repo = self.open_git_repo()?;
        let mut remote = mirror_repo.remote_at(url.clone()).map_err(git_err)?;
        // gix needs *some* refspec to negotiate the ls-refs; the spec
        // we pick doesn't constrain what the server advertises — only
        // what would actually be downloaded if we called `receive`.
        remote
            .replace_refspecs(
                [format!("+{ref_name}:{ref_name}").as_str()],
                gix::remote::Direction::Fetch,
            )
            .map_err(git_err)?;
        let mut connection = remote
            .connect(gix::remote::Direction::Fetch)
            .map_err(git_err)?;
        connection.set_credentials(|_| Ok(None));
        let (ref_map, _handshake) = connection
            .ref_map(
                gix::progress::Discard,
                gix::remote::ref_map::Options::default(),
            )
            .map_err(git_err)?;
        Ok(ref_map.remote_refs.iter().any(|r| {
            let full = match r {
                gix_protocol::handshake::Ref::Direct { full_ref_name, .. }
                | gix_protocol::handshake::Ref::Symbolic { full_ref_name, .. }
                | gix_protocol::handshake::Ref::Peeled { full_ref_name, .. }
                | gix_protocol::handshake::Ref::Unborn { full_ref_name, .. } => full_ref_name,
            };
            full.as_bstr() == ref_name.as_bytes()
        }))
    }

    /// Pull from a Git remote.
    pub fn pull(&mut self, remote_name: &str) -> GitResult<GitPullOutcome> {
        let head_before = self.heddle_repo.refs().read_head()?;
        let attached_before = match &head_before {
            Head::Attached { thread } => self
                .heddle_repo
                .refs()
                .get_thread(thread)?
                .map(|state| (thread.clone(), state)),
            Head::Detached { .. } => None,
        };
        let attached_thread = attached_before.as_ref().map(|(thread, _)| thread.clone());

        self.fetch_with_scope(
            remote_name,
            GitFetchScope::AllRefs,
            RefreshCheckoutAfterFetch::No,
        )?;
        self.preflight_attached_pull_fast_forward(remote_name, attached_before.as_ref())?;
        let stats = self.import(None)?;

        let mut materialized_attached_thread = false;
        if let Some((thread, old_state)) = attached_before
            && let Some(new_state) = self.heddle_repo.refs().get_thread(&thread)?
            && new_state != old_state
        {
            self.heddle_repo.refs().set_thread(&thread, &old_state)?;
            self.heddle_repo.refs().write_head(&Head::Attached {
                thread: thread.clone(),
            })?;
            self.heddle_repo
                .goto_verified_clean_without_record(&new_state)?;
            self.heddle_repo.refs().set_thread(&thread, &new_state)?;
            self.heddle_repo
                .refs()
                .write_head(&Head::Attached { thread })?;
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
    ) -> GitResult<()> {
        let Some((thread, state_id)) = attached_before else {
            return Ok(());
        };
        self.load_mapping_from_disk()?;
        let Some(local_git_oid) = self.mapping.get_git(state_id) else {
            return Ok(());
        };
        let mirror_repo = self.open_git_repo()?;
        let branch_ref = format!("refs/heads/{thread}");
        let Ok(mut reference) = mirror_repo.find_reference(&branch_ref) else {
            return Ok(());
        };
        let remote_git_oid = reference.peel_to_id().map_err(git_err)?.detach();
        if remote_git_oid == local_git_oid
            || commit_is_descendant_of(&mirror_repo, remote_git_oid, local_git_oid)?
        {
            return Ok(());
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
        let checkout_repo = gix::discover(self.heddle_repo.root()).map_err(git_err)?;
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
        Ok(())
    }

    pub(crate) fn seed_git_checkpoint_mappings_from_checkout(
        &mut self,
        mirror_repo: &gix::Repository,
    ) -> GitResult<()> {
        if !self.heddle_repo.root().join(".git").exists() {
            return Ok(());
        }

        let checkout_repo = match gix::discover(self.heddle_repo.root()) {
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

            if mirror_repo.find_object(git_oid).is_err() {
                copy_reachable_objects(&object_repo, mirror_repo, [git_oid])?;
            }
            mirror_repo
                .find_object(git_oid)
                .map_err(|_| GitBridgeError::CommitNotFound(record.git_commit.clone()))?;

            self.mapping.insert(change_id, git_oid);
            if super::git_notes::read_note(mirror_repo, git_oid)?.is_none()
                && let Some(state) = self.heddle_repo.store().get_state(&change_id)?
            {
                let note = super::git_notes::HeddleNote::from_state(&state);
                super::git_notes::write_note(mirror_repo, git_oid, &note)?;
            }
        }

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

    fn write_current_checkout_from_existing_mirror(&mut self) -> GitResult<WriteThroughOutcome> {
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
        let Some(state_id) = self.heddle_repo.refs().get_thread(thread)? else {
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
        let Some(git_oid) = self.mapping.get_git(&state_id) else {
            return Ok(WriteThroughOutcome::Skipped(
                WriteThroughSkipReason::NoMappedCommit,
            ));
        };

        let mirror_repo = self.open_git_repo()?;
        let checkout_repo = gix::discover(self.heddle_repo.root()).map_err(git_err)?;
        if checkout_repo.git_dir() == mirror_repo.git_dir() {
            return Ok(WriteThroughOutcome::Skipped(
                WriteThroughSkipReason::MirrorIsWorktree,
            ));
        }
        let git_dir = checkout_repo.git_dir().to_path_buf();
        // gix-index manages its own `index.lock` (atomic `O_CREAT |
        // O_EXCL`) inside `index.write`, so we don't create a parallel
        // lock here — that would deadlock with gix's writer. The
        // existence check below is a UX nicety so a stale or
        // concurrent lock surfaces as a structured `IndexAlreadyDirty`
        // skip rather than a raw "Could not acquire lock" error from
        // gix.
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
            .and_then(|mut reference| reference.peel_to_id().ok())
            .map(|id| id.detach());

        let write_result = (|| -> GitResult<()> {
            copy_reachable_objects(&mirror_repo, &object_repo, [git_oid])?;
            fs::write(&head_path, format!("ref: {branch_ref}\n"))?;

            let commit = checkout_repo.find_commit(git_oid).map_err(git_err)?;
            let tree_id = commit.tree_id().map_err(git_err)?;
            let mut index = checkout_repo.index_from_tree(&tree_id).map_err(git_err)?;
            index
                .write(gix_index::write::Options::default())
                .map_err(git_err)?;

            update_checkout_head_ref(
                &checkout_repo,
                git_oid,
                previous_branch,
                "heddle: write-through current thread",
            )?;

            // fsync after every durable write so a power loss between
            // `fs::write(HEAD)` and `index.write` doesn't leave the
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
                    PreviousValue::Any,
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

        let mirror_repo = self.open_git_repo()?;
        let branch_ref = format!("refs/heads/{branch}");
        let Ok(mut reference) = mirror_repo.find_reference(&branch_ref) else {
            return Ok(());
        };
        let target = reference.peel_to_id().map_err(git_err)?.detach();

        let checkout_repo = gix::discover(self.heddle_repo.root()).map_err(git_err)?;
        if checkout_repo.git_dir() == mirror_repo.git_dir() {
            return Ok(());
        }
        let object_repo = common_repo_for_worktree(&checkout_repo)?;
        copy_reachable_objects(&mirror_repo, &object_repo, [target])?;
        set_reference(
            &object_repo,
            &format!("refs/remotes/{tracking_remote}/{branch}"),
            target,
            PreviousValue::Any,
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

        let mirror_repo = self.open_git_repo()?;
        let checkout_repo = gix::discover(self.heddle_repo.root()).map_err(git_err)?;
        if checkout_repo.git_dir() == mirror_repo.git_dir() {
            return Ok(());
        }
        let object_repo = common_repo_for_worktree(&checkout_repo)?;
        let prefix = format!("refs/remotes/{remote_name}/");
        for reference in mirror_repo
            .references()
            .map_err(git_err)?
            .prefixed(prefix.as_str())
            .map_err(git_err)?
        {
            let reference = reference.map_err(git_err)?;
            let Some(target) = reference.target().try_id().map(ToOwned::to_owned) else {
                continue;
            };
            let full = reference.name().as_bstr().to_string();
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
                PreviousValue::Any,
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
        let checkout_repo = gix::discover(self.heddle_repo.root()).map_err(git_err)?;
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
        direction: gix::remote::Direction,
    ) -> GitResult<ResolvedRemote> {
        let repo = self.open_git_repo()?;
        let url = match remote_url_from_repo(&repo, remote_name, direction)? {
            Some(url) => Some(url),
            None => self.checkout_remote_url(remote_name, direction)?,
        };

        let url = match url {
            Some(url) => url,
            None => gix::url::parse(remote_name.as_bytes().as_bstr()).map_err(git_err)?,
        };

        match url.scheme {
            gix::url::Scheme::File => Ok(ResolvedRemote::Local(local_path_from_url(&url)?)),
            _ => Ok(ResolvedRemote::Url(url)),
        }
    }

    fn checkout_remote_url(
        &self,
        remote_name: &str,
        direction: gix::remote::Direction,
    ) -> GitResult<Option<gix::Url>> {
        if direction == gix::remote::Direction::Fetch
            && let Some(url) =
                remote_fetch_url_from_checkout_config(self.heddle_repo.root(), remote_name)?
        {
            return Ok(Some(url));
        }
        let Ok(repo) = gix::discover(self.heddle_repo.root()) else {
            return Ok(None);
        };
        remote_url_from_repo(&repo, remote_name, direction)
    }
}

fn remote_url_from_repo(
    repo: &gix::Repository,
    remote_name: &str,
    direction: gix::remote::Direction,
) -> GitResult<Option<gix::Url>> {
    if direction == gix::remote::Direction::Fetch {
        if let Some(url) = remote_fetch_url_from_config(repo, remote_name)? {
            return Ok(Some(url));
        }
        if let Ok(remote) = repo.find_remote(remote_name.as_bytes().as_bstr()) {
            return Ok(remote.url(direction).cloned());
        }
        Ok(None)
    } else if let Ok(remote) = repo.find_remote(remote_name.as_bytes().as_bstr()) {
        Ok(remote.url(direction).cloned())
    } else {
        Ok(None)
    }
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
    let checkout_repo = gix::discover(root).map_err(git_err)?;
    let object_repo = common_repo_for_worktree(&checkout_repo)?;
    Ok(object_repo
        .find_reference(super::git_notes::NOTES_REF)
        .is_ok())
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

fn remote_fetch_url_from_config(
    repo: &gix::Repository,
    remote_name: &str,
) -> GitResult<Option<gix::Url>> {
    for config_path in repo_config_paths(repo)? {
        let Some(url) = parse_remote_fetch_url_from_config(&config_path, remote_name)? else {
            continue;
        };
        let base = repo.workdir().unwrap_or_else(|| repo.git_dir());
        return parse_configured_remote_url(&url, base).map(Some);
    }
    Ok(None)
}

fn remote_fetch_url_from_checkout_config(
    root: &Path,
    remote_name: &str,
) -> GitResult<Option<gix::Url>> {
    for config_path in checkout_git_config_paths(root) {
        let Some(url) = parse_remote_fetch_url_from_config(&config_path, remote_name)? else {
            continue;
        };
        return parse_configured_remote_url(&url, root).map(Some);
    }
    Ok(None)
}

fn parse_configured_remote_url(value: &str, relative_base: &Path) -> GitResult<gix::Url> {
    if configured_remote_is_local_path(value) {
        let path = configured_remote_local_path(value, relative_base);
        return gix::url::parse(format!("file://{}", path.display()).as_bytes().as_bstr())
            .map_err(git_err);
    }
    gix::url::parse(value.as_bytes().as_bstr()).map_err(git_err)
}

fn configured_remote_local_path(value: &str, relative_base: &Path) -> PathBuf {
    if value == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
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

fn repo_config_paths(repo: &gix::Repository) -> GitResult<Vec<PathBuf>> {
    let mut paths = vec![repo.git_dir().join("config")];
    let common_repo = common_repo_for_worktree(repo)?;
    let common_config = common_repo.git_dir().join("config");
    if !paths.iter().any(|path| path == &common_config) {
        paths.push(common_config);
    }
    Ok(paths)
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

fn common_repo_for_worktree(repo: &gix::Repository) -> GitResult<gix::Repository> {
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

pub(crate) fn open_repo(path: &Path) -> GitResult<gix::Repository> {
    match gix::discover(path) {
        Ok(repo) => Ok(repo),
        Err(_) => gix::open(path).map_err(git_err),
    }
}

/// Delete a reference if present; missing-ref is a no-op. Used by the
/// write-through rollback path to drop a branch that was created by a
/// failed write-through but isn't reachable from any prior state. We
/// scope the deletion with `PreviousValue::MustExist` so an unrelated
/// concurrent writer that *just* updated this ref isn't silently
/// clobbered — if the ref vanished underneath us between our read and
/// the delete, that's the rollback we wanted anyway.
pub(crate) fn delete_reference_if_present(repo: &gix::Repository, name: &str) -> GitResult<()> {
    let signature = bridge_signature();
    let mut time_buf = gix::date::parse::TimeBuf::default();
    let edit = RefEdit {
        change: Change::Delete {
            log: RefLog::AndReference,
            expected: PreviousValue::MustExist,
        },
        name: name
            .try_into()
            .map_err(|err| GitBridgeError::Git(format!("invalid ref {name}: {err}")))?,
        deref: false,
    };
    match repo.edit_references_as([edit], Some(signature.to_ref(&mut time_buf))) {
        Ok(_) => Ok(()),
        // Missing ref → already rolled back; treat as success. gix's
        // error message on an absent ref reads "for deletion did not
        // exist or could not be parsed".
        Err(err) if err.to_string().contains("did not exist") => Ok(()),
        Err(err) => Err(git_err(err)),
    }
}

pub(crate) fn set_reference(
    repo: &gix::Repository,
    name: &str,
    target: ObjectId,
    constraint: PreviousValue,
    log_message: &str,
) -> GitResult<()> {
    let signature = bridge_signature();
    let mut time_buf = gix::date::parse::TimeBuf::default();
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: log_message.into(),
            },
            expected: constraint,
            new: Target::Object(target),
        },
        name: name
            .try_into()
            .map_err(|err| GitBridgeError::Git(format!("invalid ref {name}: {err}")))?,
        deref: false,
    };
    repo.edit_references_as([edit], Some(signature.to_ref(&mut time_buf)))
        .map_err(git_err)?;
    Ok(())
}

fn update_checkout_head_ref(
    repo: &gix::Repository,
    target: ObjectId,
    previous_branch: Option<ObjectId>,
    log_message: &str,
) -> GitResult<()> {
    let signature = bridge_signature();
    let mut time_buf = gix::date::parse::TimeBuf::default();
    let expected = previous_branch.map_or(PreviousValue::MustNotExist, |oid| {
        PreviousValue::MustExistAndMatch(Target::Object(oid))
    });
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: log_message.into(),
            },
            expected,
            new: Target::Object(target),
        },
        name: "HEAD"
            .try_into()
            .map_err(|err| GitBridgeError::Git(format!("invalid ref HEAD: {err}")))?,
        deref: true,
    };
    repo.edit_references_as([edit], Some(signature.to_ref(&mut time_buf)))
        .map_err(git_err)?;
    Ok(())
}

fn checkout_git_head_is_detached(root: &Path) -> GitResult<bool> {
    let repo = gix::discover(root).map_err(git_err)?;
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
    let Ok(repo) = gix::discover(repo_root) else {
        return Ok(None);
    };
    Ok(repo
        .config_snapshot()
        .string(key)
        .map(|value| value.to_str_lossy().into_owned()))
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

fn bridge_signature() -> gix::actor::Signature {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    gix::actor::Signature {
        name: "Heddle".into(),
        email: "heddle@local".into(),
        time: gix::date::Time { seconds, offset: 0 },
    }
}

fn local_path_from_url(url: &gix::Url) -> GitResult<PathBuf> {
    if url.scheme != gix::url::Scheme::File {
        return Err(GitBridgeError::Git(format!(
            "remote '{}' uses unsupported scheme {:?}; only local path and file:// remotes are supported",
            url, url.scheme
        )));
    }

    let path = PathBuf::from(String::from_utf8_lossy(url.path.as_ref()).into_owned());
    if path.as_os_str().is_empty() {
        return Err(GitBridgeError::Git(format!(
            "remote '{}' has no filesystem path",
            url
        )));
    }
    Ok(path)
}

fn collect_ref_updates(repo: &gix::Repository) -> GitResult<Vec<RefUpdate>> {
    let mut updates = Vec::new();

    for branch in repo
        .references()
        .map_err(git_err)?
        .local_branches()
        .map_err(git_err)?
    {
        let branch = branch.map_err(git_err)?;
        let Some(target) = branch.try_id() else {
            continue;
        };
        updates.push(RefUpdate {
            name: branch.name().shorten().to_string(),
            target: target.detach(),
            namespace: RefNamespace::Branch,
        });
    }

    for tag in repo
        .references()
        .map_err(git_err)?
        .tags()
        .map_err(git_err)?
    {
        let tag = tag.map_err(git_err)?;
        let Some(target) = tag.try_id() else {
            continue;
        };
        updates.push(RefUpdate {
            name: tag.name().shorten().to_string(),
            target: target.detach(),
            namespace: RefNamespace::Tag,
        });
    }

    // Pick up refs/notes/* (currently just refs/notes/heddle) so the
    // change_id metadata travels alongside branches/tags on every push.
    for note_ref in repo
        .references()
        .map_err(git_err)?
        .prefixed("refs/notes/")
        .map_err(git_err)?
    {
        let note_ref = note_ref.map_err(git_err)?;
        let Some(target) = note_ref.try_id() else {
            continue;
        };
        // shorten() on refs/notes/<n> returns "<n>" (the path beneath the
        // notes/ prefix). We want to round-trip "<n>", not "notes/<n>",
        // since apply_ref_updates rebuilds the full name.
        let full = note_ref.name().as_bstr().to_string();
        let short = full
            .strip_prefix("refs/notes/")
            .unwrap_or(&full)
            .to_string();
        updates.push(RefUpdate {
            name: short,
            target: target.detach(),
            namespace: RefNamespace::Note,
        });
    }

    Ok(updates)
}

fn collect_ref_updates_for_push(
    repo: &gix::Repository,
    scope: GitPushScope,
    current_branch: Option<&str>,
) -> GitResult<Vec<RefUpdate>> {
    let updates = collect_ref_updates(repo)?;
    match scope {
        GitPushScope::AllThreads => Ok(updates),
        GitPushScope::CurrentThread => {
            let branch = current_branch.ok_or_else(|| {
                GitBridgeError::Git("missing current branch for scoped push".to_string())
            })?;
            Ok(updates
                .into_iter()
                .filter(|update| {
                    (matches!(update.namespace, RefNamespace::Branch) && update.name == branch)
                        || matches!(update.namespace, RefNamespace::Note)
                })
                .collect())
        }
    }
}

fn collect_ref_updates_for_fetch(
    repo: &gix::Repository,
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

fn full_ref_name(update: &RefUpdate) -> String {
    match update.namespace {
        RefNamespace::Branch => format!("refs/heads/{}", update.name),
        RefNamespace::Tag => format!("refs/tags/{}", update.name),
        RefNamespace::Note => format!("refs/notes/{}", update.name),
    }
}

fn validate_ref_updates_fast_forward(
    repo: &gix::Repository,
    updates: &[RefUpdate],
) -> GitResult<()> {
    for update in updates {
        if !matches!(update.namespace, RefNamespace::Branch | RefNamespace::Note) {
            continue;
        }
        let full_name = full_ref_name(update);
        if let Ok(mut reference) = repo.find_reference(&full_name) {
            let old = reference.peel_to_id().map_err(git_err)?.detach();
            ensure_commit_update_fast_forward(repo, &full_name, old, update.target)?;
        }
    }
    Ok(())
}

pub(crate) fn ensure_commit_update_fast_forward(
    repo: &gix::Repository,
    name: &str,
    old: ObjectId,
    new: ObjectId,
) -> GitResult<()> {
    if old == new || old == repo.object_hash().null() {
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
    repo: &gix::Repository,
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
        let commit = repo.find_commit(oid).map_err(git_err)?;
        for parent in commit.parent_ids() {
            stack.push(parent.detach());
        }
    }
    Ok(false)
}

pub(crate) fn apply_ref_updates(
    repo: &gix::Repository,
    updates: &[RefUpdate],
    log_message: &str,
) -> GitResult<()> {
    for update in updates {
        let full_name = full_ref_name(update);
        set_reference(
            repo,
            &full_name,
            update.target,
            PreviousValue::Any,
            log_message,
        )?;
    }
    Ok(())
}

fn apply_remote_tracking_ref_updates(
    repo: &gix::Repository,
    remote_name: &str,
    updates: &[RefUpdate],
    log_message: &str,
) -> GitResult<()> {
    for update in updates
        .iter()
        .filter(|update| update.namespace == RefNamespace::Branch)
    {
        set_reference(
            repo,
            &format!("refs/remotes/{remote_name}/{}", update.name),
            update.target,
            PreviousValue::Any,
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
    let target = match open_repo(dest) {
        Ok(repo) => repo,
        Err(_) => gix::init_bare(dest).map_err(git_err)?,
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
        .head_name()
        .ok()
        .flatten()
        .and_then(|full_name| {
            full_name
                .as_bstr()
                .to_str()
                .ok()
                .and_then(|s| s.strip_prefix("refs/heads/").map(str::to_owned))
        })
        .filter(|branch| copied_branches.contains(branch.as_str()));
    if let Some(branch) = source_head_branch {
        fs::write(dest.join("HEAD"), format!("ref: refs/heads/{branch}\n"))?;
    } else if copied_branches.contains("main") {
        fs::write(dest.join("HEAD"), b"ref: refs/heads/main\n")?;
    } else if let Some(first_branch) = updates
        .iter()
        .find(|update| update.namespace == RefNamespace::Branch)
    {
        fs::write(
            dest.join("HEAD"),
            format!("ref: refs/heads/{}\n", first_branch.name),
        )?;
    }
    Ok(())
}

/// Clone a remote git URL into `dest` as a bare repository, fetching all
/// branches and tags. Mirrors the gix recipe used by `fetch_network_remote`
/// but starts from an empty `init_bare` rather than an existing repo.
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
    url: &gix::Url,
    dest: &Path,
    depth: Option<u32>,
    filter: Option<&str>,
) -> GitResult<()> {
    // gix 0.80's high-level fetch builder (`Connection::prepare_fetch` →
    // `Prepare`) does not expose the v2 partial-clone `filter`
    // capability. Older code delegated that case to `git clone`, but
    // public Git-overlay workflows must run on machines with no Git
    // executable installed. Keep depth-only clones native and reject
    // filtered clones until we have a native implementation.
    if let Some(spec) = filter {
        return Err(GitBridgeError::Git(format!(
            "partial Git clone filter `{spec}` is not supported in Heddle's native no-git runtime yet; retry without --filter/--lazy so Heddle can import a complete object graph"
        )));
    }
    if url.scheme == gix::url::Scheme::File {
        let source_path = local_path_from_url(url)?;
        if depth.is_some() {
            return Err(GitBridgeError::Git(
                "shallow file:// Git clones are not supported in Heddle's native no-git runtime yet; retry without --depth so Heddle can copy the local Git object graph without spawning Git transport helpers"
                    .to_string(),
            ));
        }
        return copy_local_repo_to_bare(&source_path, dest);
    }
    let default_branch =
        clone_url_to_bare_via_gix(url, dest, depth)?.or_else(|| default_branch_from_file_url(url));
    // gix's `init_bare` writes `.git/HEAD = ref: refs/heads/<init.defaultBranch>`
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
        fs::write(dest.join("HEAD"), format!("ref: refs/heads/{branch}\n"))?;
        // Also persist `refs/remotes/origin/HEAD` so `git symbolic-ref
        // refs/remotes/origin/HEAD` works, and so any code path that
        // reads the remote-tracking symref (e.g. `select_clone_thread`'s
        // fallback chain) finds the same answer the fetch handshake
        // gave us. gix doesn't write this by default — only `.git/HEAD`.
        let remotes_dir = dest.join("refs").join("remotes").join("origin");
        fs::create_dir_all(&remotes_dir)?;
        fs::write(
            remotes_dir.join("HEAD"),
            format!("ref: refs/remotes/origin/{branch}\n"),
        )?;
    }
    Ok(())
}

fn default_branch_from_file_url(url: &gix::Url) -> Option<String> {
    let source_path = local_path_from_url(url).ok()?;
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
    Ok(repo.find_reference(&format!("refs/heads/{branch}")).is_ok())
}

fn clone_url_to_bare_via_gix(
    url: &gix::Url,
    dest: &Path,
    depth: Option<u32>,
) -> GitResult<Option<String>> {
    fs::create_dir_all(dest)?;
    let repo = gix::init_bare(dest).map_err(git_err)?;
    let mut remote = repo.remote_at(url.clone()).map_err(git_err)?;
    remote
        .replace_refspecs(
            [
                // HEAD must be in the refspec list for gix to include
                // the remote's HEAD advertisement in `RefMap.remote_refs`
                // — without it, `default_branch_from_ref_map` can't see
                // which branch the remote points HEAD at and clone
                // attaches to the alphabetically-first ref instead of
                // the repo's default.
                "+HEAD:refs/remotes/origin/HEAD",
                "+refs/heads/*:refs/heads/*",
                "+refs/notes/*:refs/notes/*",
            ],
            gix::remote::Direction::Fetch,
        )
        .map_err(git_err)?;
    remote = remote.with_fetch_tags(gix::remote::fetch::Tags::All);
    let mut connection = remote
        .connect(gix::remote::Direction::Fetch)
        .map_err(git_err)?;
    connection.set_credentials(|_| Ok(None));
    let mut prepare = connection
        .prepare_fetch(
            gix::progress::Discard,
            gix::remote::ref_map::Options::default(),
        )
        .map_err(git_err)?;
    if let Some(d) = depth.and_then(NonZeroU32::new) {
        prepare = prepare.with_shallow(gix::remote::fetch::Shallow::DepthAtRemote(d));
    }
    let outcome = prepare
        .with_reflog_message(gix::remote::fetch::RefLogMessage::Override {
            message: format!("heddle: clone from {url}").into(),
        })
        .receive(gix::progress::Discard, &AtomicBool::new(false))
        .map_err(|err| GitBridgeError::Git(format!("clone failed for {url}: {err}")))?;
    Ok(default_branch_from_ref_map(&outcome.ref_map))
}

fn default_branch_from_ref_map(ref_map: &gix::remote::fetch::RefMap) -> Option<String> {
    // Pass 1: prefer an explicit symref (protocol v2 + the
    // `symref=HEAD:refs/heads/<branch>` capability). This is the only
    // unambiguous answer.
    for remote_ref in &ref_map.remote_refs {
        let target = match remote_ref {
            gix_protocol::handshake::Ref::Symbolic {
                full_ref_name,
                target,
                ..
            } if full_ref_name.as_bstr() == "HEAD" => target,
            gix_protocol::handshake::Ref::Unborn {
                full_ref_name,
                target,
            } if full_ref_name.as_bstr() == "HEAD" => target,
            _ => continue,
        };
        if let Some(branch) = target.as_bstr().strip_prefix(b"refs/heads/")
            && !branch.is_empty()
        {
            return Some(branch.to_str_lossy().into_owned());
        }
    }
    // Pass 2: fall back to ref-by-OID match. Some servers (or older
    // protocol versions) advertise HEAD as a Direct ref carrying just
    // the commit OID, with no symref annotation. In that case the
    // default branch is the one whose tip matches HEAD's OID.
    let head_oid = ref_map.remote_refs.iter().find_map(|r| match r {
        gix_protocol::handshake::Ref::Direct {
            full_ref_name,
            object,
        } if full_ref_name.as_bstr() == "HEAD" => Some(*object),
        gix_protocol::handshake::Ref::Peeled {
            full_ref_name,
            object,
            ..
        } if full_ref_name.as_bstr() == "HEAD" => Some(*object),
        _ => None,
    })?;
    // Prefer the conventional defaults when several branches share the
    // tip OID (e.g. a fresh repo where `main` was created from
    // `master`). Otherwise return the first match in advertisement
    // order, which on github.com matches the repo's "Default branch"
    // setting in practice.
    let mut first_match: Option<String> = None;
    for remote_ref in &ref_map.remote_refs {
        let (full_name, oid) = match remote_ref {
            gix_protocol::handshake::Ref::Direct {
                full_ref_name,
                object,
            } => (full_ref_name, object),
            gix_protocol::handshake::Ref::Peeled {
                full_ref_name,
                object,
                ..
            } => (full_ref_name, object),
            _ => continue,
        };
        if *oid != head_oid {
            continue;
        }
        let Some(branch) = full_name.as_bstr().strip_prefix(b"refs/heads/") else {
            continue;
        };
        if branch.is_empty() {
            continue;
        }
        let branch = branch.to_str_lossy().into_owned();
        if matches!(branch.as_str(), "main" | "master" | "trunk") {
            return Some(branch);
        }
        if first_match.is_none() {
            first_match = Some(branch);
        }
    }
    first_match
}

pub(crate) fn copy_reachable_objects(
    source: &gix::Repository,
    target: &gix::Repository,
    roots: impl IntoIterator<Item = ObjectId>,
) -> GitResult<()> {
    use gix::objs::Exists;
    use gix::prelude::Write;
    if source.object_hash() != target.object_hash() {
        return Err(GitBridgeError::Git(format!(
            "object hash mismatch: {:?} vs {:?}",
            source.object_hash(),
            target.object_hash()
        )));
    }

    // Fastest path: copy source's packfiles directly into target's
    // `objects/pack/` directory. A clone-style remote ships its
    // history as a single delta-compressed pack; rewriting each
    // contained object as a loose file (the original code path)
    // makes us decode every delta and re-zlib every object, which
    // on bine cost 4.27s for 593 objects. A `fs::copy` of the same
    // pack is ~5ms. Only when there is no pack on disk (every
    // object is loose), or after the pack copy we still have
    // reachable OIDs the target can't see, do we fall back to the
    // per-object walk.
    try_copy_packs(source, target)?;

    // gix's odb has a refresh-on-miss policy by default, so any
    // packs we just copied get picked up the first time an OID isn't
    // already cached. Walk reachable OIDs to fill in any loose
    // objects the pack didn't cover (typically none after a fresh
    // clone, but possible on adopted dev checkouts with unpacked
    // commits since the last gc).
    for oid in collect_reachable_object_ids(source, roots)? {
        if target.objects.exists(&oid) {
            continue;
        }
        let object = source.find_object(oid).map_err(git_err)?;
        target
            .objects
            .write_buf(object.kind, &object.data)
            .map_err(|err| GitBridgeError::Git(format!("write_buf for {oid}: {err}")))?;
    }

    Ok(())
}

/// Copy all `.pack` + `.idx` (+ `.rev` + `.mtimes` if present) files
/// from source's `objects/pack/` into target's `objects/pack/`. Best
/// effort: missing source dir is a no-op, already-present pack at
/// target is skipped. Errors propagate so the caller can fall back to
/// per-object copy.
fn try_copy_packs(source: &gix::Repository, target: &gix::Repository) -> GitResult<()> {
    let source_pack = source.git_dir().join("objects").join("pack");
    let target_pack = target.git_dir().join("objects").join("pack");
    if !source_pack.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(&target_pack)?;
    for entry in fs::read_dir(&source_pack)? {
        let entry = entry?;
        let name = entry.file_name();
        let dest = target_pack.join(&name);
        if dest.exists() {
            continue;
        }
        let name_str = name.to_string_lossy();
        if !(name_str.starts_with("pack-")
            && (name_str.ends_with(".pack")
                || name_str.ends_with(".idx")
                || name_str.ends_with(".rev")
                || name_str.ends_with(".mtimes")
                || name_str.ends_with(".keep")))
        {
            continue;
        }
        // Write to a tempname + rename so a crashed copy doesn't
        // leave a half-written .idx that gix would try to mmap.
        let temp = target_pack.join(format!(".{name_str}.tmp"));
        fs::copy(entry.path(), &temp)?;
        fs::rename(&temp, &dest)?;
    }
    Ok(())
}

fn collect_reachable_object_ids(
    source: &gix::Repository,
    roots: impl IntoIterator<Item = ObjectId>,
) -> GitResult<Vec<ObjectId>> {
    let mut stack: Vec<ObjectId> = roots.into_iter().collect();
    let mut seen = HashSet::new();
    let mut ordered = Vec::new();

    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        ordered.push(oid);

        let object = source.find_object(oid).map_err(git_err)?;
        match object.kind {
            gix::objs::Kind::Commit => {
                let commit = source.find_commit(oid).map_err(git_err)?;
                stack.push(commit.tree_id().map_err(git_err)?.detach());
                for parent in commit.parent_ids() {
                    stack.push(parent.detach());
                }
            }
            gix::objs::Kind::Tree => {
                let tree = source.find_tree(oid).map_err(git_err)?;
                for entry in tree.iter() {
                    let entry = entry.map_err(git_err)?;
                    // Gitlink (mode 160000) entries point at a commit
                    // in the *submodule's* repository, not this one —
                    // by Git's design, that commit is never stored
                    // locally. Pushing its OID onto the walk would
                    // make the next `find_object` fail with
                    // "object … could not be found", which is what
                    // happens on a normal clone of any repo with
                    // submodules (e.g. git/git's
                    // sha1collisiondetection). The bridge import
                    // path (`import_gitlink`) records the foreign OID
                    // as a `heddle-submodule:` blob, which is what
                    // round-trips on export — so skipping it here is
                    // safe: we still emit the parent tree, just
                    // without trying to resolve a foreign-repo
                    // commit we cannot read.
                    if entry.mode().kind() == gix::object::tree::EntryKind::Commit {
                        continue;
                    }
                    stack.push(entry.object_id());
                }
            }
            gix::objs::Kind::Tag => {
                let tag = source.find_tag(oid).map_err(git_err)?;
                stack.push(tag.target_id().map_err(git_err)?.detach());
            }
            gix::objs::Kind::Blob => {}
        }
    }

    Ok(ordered)
}

fn fetch_network_remote(
    mirror_repo: &gix::Repository,
    remote_name: &str,
    url: &gix::Url,
    scope: GitFetchScope,
) -> GitResult<()> {
    let mut remote = mirror_repo.remote_at(url.clone()).map_err(git_err)?;
    remote
        .replace_refspecs(
            ["+refs/heads/*:refs/heads/*", "+refs/notes/*:refs/notes/*"],
            gix::remote::Direction::Fetch,
        )
        .map_err(git_err)?;
    remote = remote.with_fetch_tags(match scope {
        GitFetchScope::BranchesAndNotes => gix::remote::fetch::Tags::None,
        GitFetchScope::AllRefs => gix::remote::fetch::Tags::All,
    });

    let mut connection = remote
        .connect(gix::remote::Direction::Fetch)
        .map_err(git_err)?;
    connection.set_credentials(|_| Ok(None));
    let progress = gix::progress::Discard;
    let prepare = connection
        .prepare_fetch(progress, gix::remote::ref_map::Options::default())
        .map_err(git_err)?;
    let progress = gix::progress::Discard;
    prepare
        .with_reflog_message(gix::remote::fetch::RefLogMessage::Override {
            message: format!("heddle: fetch from {remote_name}").into(),
        })
        .receive(progress, &AtomicBool::new(false))
        .map_err(|err| GitBridgeError::Git(format!("failed to fetch from {url}: {err}")))?;
    Ok(())
}

fn push_network_remote_with_updates(
    mirror_repo: &gix::Repository,
    url: &gix::Url,
    updates: &[RefUpdate],
    force: bool,
) -> GitResult<()> {
    if updates.is_empty() {
        return Ok(());
    }

    let mut transport = gix_transport::client::blocking_io::connect::connect(
        url.clone(),
        gix_transport::client::blocking_io::connect::Options {
            version: Protocol::V1,
            ..Default::default()
        },
    )
    .map_err(|err| GitBridgeError::Git(format!("failed to connect to {url}: {err}")))?;

    let remote_refs = {
        let mut handshake = transport
            .handshake(Service::ReceivePack, &[])
            .map_err(|err| {
                GitBridgeError::Git(format!("receive-pack handshake failed for {url}: {err}"))
            })?;
        if !handshake.capabilities.contains("report-status") {
            return Err(GitBridgeError::Git(format!(
                "remote {url} does not support report-status; refusing to push without server acknowledgement"
            )));
        }
        remote_refs_from_receive_pack_handshake(&mut handshake)?
    };
    let mut commands = Vec::new();
    for update in updates {
        let full_name = full_ref_name(update);
        let old = remote_refs
            .get(&full_name)
            .copied()
            .unwrap_or_else(|| ObjectHashKind::Sha1.null());
        if old == update.target {
            continue;
        }
        if !force && matches!(update.namespace, RefNamespace::Branch | RefNamespace::Note) {
            ensure_commit_update_fast_forward(mirror_repo, &full_name, old, update.target)?;
        }
        commands.push((full_name, old, update.target));
    }

    if commands.is_empty() {
        return Ok(());
    }

    let pack =
        pack_reachable_objects(mirror_repo, commands.iter().map(|(_, _, new_oid)| *new_oid))?;
    let mut request = transport
        .request(
            WriteMode::OneLfTerminatedLinePerWriteCall,
            MessageKind::Flush,
            false,
        )
        .map_err(git_err)?;
    for (idx, (name, old, new_oid)) in commands.iter().enumerate() {
        let mut line = format!("{old} {new_oid} {name}");
        if idx == 0 {
            line.push('\0');
            line.push_str("report-status");
        }
        request.write_all(line.as_bytes()).map_err(git_err)?;
    }
    request.write_message(MessageKind::Flush).map_err(git_err)?;

    let (mut raw_writer, mut reader) = request.into_parts();
    raw_writer.write_all(&pack).map_err(git_err)?;
    raw_writer.flush().map_err(git_err)?;
    drop(raw_writer);

    read_receive_pack_status(&mut reader, &commands, url)
}

fn remote_refs_from_receive_pack_handshake(
    handshake: &mut gix_transport::client::blocking_io::SetServiceResponse<'_>,
) -> GitResult<HashMap<String, ObjectId>> {
    let mut remote_refs = HashMap::new();
    let Some(refs) = handshake.refs.as_mut() else {
        return Ok(remote_refs);
    };
    let (parsed, _) =
        gix_protocol::handshake::refs::from_v1_refs_received_as_part_of_handshake_and_capabilities(
            refs,
            handshake.capabilities.iter(),
        )
        .map_err(git_err)?;

    for remote_ref in parsed {
        let (name, target, _) = remote_ref.unpack();
        let Some(target) = target else {
            continue;
        };
        remote_refs.insert(name.to_string(), target.to_owned());
    }
    Ok(remote_refs)
}

fn pack_reachable_objects(
    repo: &gix::Repository,
    roots: impl IntoIterator<Item = ObjectId>,
) -> GitResult<Vec<u8>> {
    let oids = collect_reachable_object_ids(repo, roots)?;
    let mut entries = Vec::with_capacity(oids.len());
    for oid in &oids {
        let object = repo.find_object(*oid).map_err(git_err)?;
        let data = gix::objs::Data {
            kind: object.kind,
            data: &object.data,
        };
        let count = gix_pack::data::output::Count::from_data(*oid, None);
        let entry = gix_pack::data::output::Entry::from_data(&count, &data).map_err(git_err)?;
        entries.push(entry);
    }

    let mut pack = Vec::new();
    let input = std::iter::once(Ok::<_, GitBridgeError>(entries));
    let mut writer = gix_pack::data::output::bytes::FromEntriesIter::new(
        input,
        &mut pack,
        oids.len().try_into().map_err(|_| {
            GitBridgeError::Git(format!(
                "push pack has too many objects to encode: {}",
                oids.len()
            ))
        })?,
        gix_pack::data::Version::V2,
        ObjectHashKind::Sha1,
    );
    for result in writer.by_ref() {
        result.map_err(git_err)?;
    }
    drop(writer);
    Ok(pack)
}

fn read_receive_pack_status(
    reader: &mut (dyn gix_transport::client::blocking_io::ExtendedBufRead<'_> + Unpin),
    commands: &[(String, ObjectId, ObjectId)],
    url: &gix::Url,
) -> GitResult<()> {
    let mut line = String::new();
    let mut saw_unpack_ok = false;
    let mut acknowledged = HashSet::new();

    loop {
        line.clear();
        let read = reader.readline_str(&mut line).map_err(git_err)?;
        if read == 0 {
            break;
        }
        let status = line.trim_end_matches(['\r', '\n']);
        if status == "unpack ok" {
            saw_unpack_ok = true;
            continue;
        }
        if let Some(name) = status.strip_prefix("ok ") {
            acknowledged.insert(name.to_string());
            continue;
        }
        if let Some(rest) = status.strip_prefix("ng ") {
            return Err(GitBridgeError::Git(format!(
                "push rejected by {url}: {rest}"
            )));
        }
        if let Some(rest) = status.strip_prefix("unpack ") {
            return Err(GitBridgeError::Git(format!(
                "push pack rejected by {url}: {rest}"
            )));
        }
    }

    if !saw_unpack_ok {
        return Err(GitBridgeError::Git(format!(
            "push to {url} did not return an unpack acknowledgement"
        )));
    }
    for (name, _, _) in commands {
        if !acknowledged.contains(name) {
            return Err(GitBridgeError::Git(format!(
                "push to {url} did not acknowledge ref {name}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_forward_guard_reports_exact_rewrite_before_after() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = gix::init_bare(tmp.path()).expect("init bare repo");
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
        let repo = gix::init_bare(tmp.path()).expect("init bare repo");
        let old = test_commit(&repo, "old", &[]);
        let new = test_commit(&repo, "new", &[old]);

        ensure_commit_update_fast_forward(&repo, "refs/heads/main", old, new)
            .expect("descendant update should be allowed");
    }

    fn test_commit(
        repo: &gix::Repository,
        message: &str,
        parents: &[gix::ObjectId],
    ) -> gix::ObjectId {
        let empty_tree_oid: gix::ObjectId = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
            .parse()
            .expect("parse empty tree oid");
        let sig = gix::actor::Signature {
            name: "Heddle Test".into(),
            email: "heddle@test".into(),
            time: gix::date::Time {
                seconds: 0,
                offset: 0,
            },
        };
        let mut committer_buf = gix::date::parse::TimeBuf::default();
        let mut author_buf = gix::date::parse::TimeBuf::default();
        repo.new_commit_as(
            sig.to_ref(&mut committer_buf),
            sig.to_ref(&mut author_buf),
            message,
            empty_tree_oid,
            parents.iter().copied(),
        )
        .expect("write test commit")
        .id
    }

    /// heddle#141 regression: when the URL-fetch path of
    /// `clone_url_to_bare` runs against a bare repo whose `HEAD`
    /// points at a branch that is *not* alphabetically first (and
    /// crucially, not what gix's `init_bare` defaults to), the
    /// resulting dest bare must have `HEAD` pointing at the remote
    /// default — not gix's init-time guess.
    #[test]
    fn clone_url_to_bare_via_gix_honours_remote_head_symref() {
        let tmp = tempfile::TempDir::new().unwrap();
        let source = tmp.path().join("source.git");
        let dest = tmp.path().join("dest.git");

        // Build a bare source with two branches under
        // deliberately-non-default names: `trunk` (will be the
        // remote default — neither gix's `init.defaultBranch` nor
        // the alphabetically-first imported ref would land here by
        // accident) and `abc-feature` (alphabetically first — what
        // the buggy fallback used to pick).
        let src = gix::init_bare(&source).expect("init bare source");
        // Empty tree (well-known OID) so we don't have to build a
        // tree object explicitly.
        let empty_tree_oid: gix::ObjectId = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
            .parse()
            .expect("parse empty tree oid");
        // Use an explicit signature via `new_commit_as` rather than
        // `Repository::commit`. The latter reads `user.name`/`user.email`
        // from git config, which CI runners don't set — leading to
        // `AuthorMissing` errors. The clone path under test doesn't care
        // who authored these seed commits.
        let sig = gix::actor::Signature {
            name: "Heddle Test".into(),
            email: "heddle@test".into(),
            time: gix::date::Time {
                seconds: 0,
                offset: 0,
            },
        };
        let mut committer_buf = gix::date::parse::TimeBuf::default();
        let mut author_buf = gix::date::parse::TimeBuf::default();
        let seed = src
            .new_commit_as(
                sig.to_ref(&mut committer_buf),
                sig.to_ref(&mut author_buf),
                "seed",
                empty_tree_oid,
                gix::commit::NO_PARENT_IDS,
            )
            .expect("seed commit")
            .id;
        for name in ["refs/heads/trunk", "refs/heads/abc-feature"] {
            set_reference(&src, name, seed, PreviousValue::Any, "test: seed branch")
                .expect("set ref");
        }
        // Make sure HEAD on the source points at trunk so
        // `git ls-remote --symref` reports trunk.
        std::fs::write(source.join("HEAD"), b"ref: refs/heads/trunk\n").unwrap();

        let url = gix::url::parse(format!("file://{}", source.display()).as_bytes().into())
            .expect("parse file:// url");
        clone_url_to_bare(&url, &dest, None, None).expect("clone url to bare");

        let dest_head = std::fs::read_to_string(dest.join("HEAD")).expect("read dest HEAD");
        assert_eq!(
            dest_head.trim(),
            "ref: refs/heads/trunk",
            "dest HEAD must mirror the remote's symref (trunk), not gix's \
             init-time default and not the alphabetically-first branch \
             (abc-feature) — see heddle#141"
        );
    }
}
