// SPDX-License-Identifier: Apache-2.0
//! Core Git bridge types and operations.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write,
    num::NonZeroU32,
    path::{Path, PathBuf},
    process::Command,
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
    object::{ChangeId, ChangeIdParseError, Tree},
    store::ObjectStore,
};
use refs::Head;
use repo::Repository as HeddleRepository;

use super::{git_export::export_all, git_import::import_all};

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

    #[error("conflict during sync: {0}")]
    Conflict(String),

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
                write!(f, "the current Heddle head is detached")
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

    /// Import Git commits into Heddle states.
    pub fn import(&mut self, git_path: Option<&Path>) -> GitResult<super::git_util::ImportStats> {
        import_all(self, git_path)
    }

    /// Push to a Git remote.
    pub fn push(&mut self, remote_name: &str) -> GitResult<()> {
        self.init_mirror()?;
        self.export()?;
        self.write_through_current_checkout()?;

        let log_message = format!("heddle: push from {}", self.heddle_repo.root().display());
        match self.resolve_remote(remote_name, gix::remote::Direction::Push)? {
            ResolvedRemote::Local(target_path) => self.copy_mirror_to_path(
                &target_path,
                &log_message,
                /* init_if_missing */ false,
            ),
            ResolvedRemote::Url(url) => {
                let mirror_repo = self.open_git_repo()?;
                push_network_remote(&mirror_repo, &url)
            }
        }
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
        let updates = collect_ref_updates(&mirror_repo)?;

        copy_reachable_objects(
            &mirror_repo,
            &target_repo,
            updates.iter().map(|update| update.target),
        )?;
        apply_ref_updates(&target_repo, &updates, log_message)?;
        Ok(())
    }

    /// Fetch Git refs and objects into the internal mirror without moving
    /// Heddle thread refs or the current worktree.
    pub fn fetch(&mut self, remote_name: &str) -> GitResult<()> {
        self.init_mirror()?;

        let mirror_repo = self.open_git_repo()?;
        match self.resolve_remote(remote_name, gix::remote::Direction::Fetch)? {
            ResolvedRemote::Local(path) => {
                let remote_repo = open_repo(&path)?;
                let updates = collect_ref_updates(&remote_repo)?;
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
            }
            ResolvedRemote::Url(url) => {
                fetch_network_remote(&mirror_repo, remote_name, &url)?;
            }
        }

        self.git_repo_path = Some(self.mirror_path());
        Ok(())
    }

    /// Pull from a Git remote.
    pub fn pull(&mut self, remote_name: &str) -> GitResult<()> {
        let head_before = self.heddle_repo.refs().read_head()?;
        let attached_before = match &head_before {
            Head::Attached { thread } => self
                .heddle_repo
                .refs()
                .get_thread(thread)?
                .map(|state| (thread.clone(), state)),
            Head::Detached { .. } => None,
        };

        self.fetch(remote_name)?;
        self.import(None)?;

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

        let mirror_guard = self.init_mirror_with_guard()?;
        // First export against a freshly-initialized mirror runs while
        // the guard is still armed; if export fails we want the
        // half-built `.heddle/git/` cleared so the next caller doesn't
        // see a corrupt bare repo.
        self.export()?;
        // Mirror is committed to disk (objects + refs) in a known-good
        // shape; remaining failures only affect the user's checkout
        // and have their own per-file rollback below.
        mirror_guard.commit();

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

            set_reference(
                &object_repo,
                &branch_ref,
                git_oid,
                PreviousValue::Any,
                "heddle: write-through current thread",
            )?;

            // Mirror the bridge's `refs/notes/heddle` ref into the
            // user's `.git/`. Without this, `git notes show <commit>`
            // from the working tree fails because the user's repo
            // has no notes ref — orchestrators have to know to poke
            // inside `.heddle/git/` with `--git-dir`. The notes ref
            // is a normal commit pointing at a tree of
            // `<commit-sha>` → `<change-id-text>` blobs, so the
            // standard reachability copy works.
            mirror_notes_ref(&mirror_repo, &object_repo)?;

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
        repo.find_fetch_remote(Some(remote_name.as_bytes().as_bstr()))
            .map(|remote| remote.url(direction).cloned())
            .map_err(git_err)
    } else if let Ok(remote) = repo.find_remote(remote_name.as_bytes().as_bstr()) {
        Ok(remote.url(direction).cloned())
    } else {
        Ok(None)
    }
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

/// Copy the bridge mirror's `refs/notes/heddle` ref into the user's
/// `.git/`. The notes ref is a regular Git commit pointing at a
/// tree of `<commit-sha>` → `<change-id-text>` files, so reachability
/// copy + a normal ref update is enough — no special notes-format
/// awareness needed.
///
/// Best-effort: if the mirror has no notes ref yet (e.g. a fresh
/// import that hasn't recorded any change_ids), we silently skip.
/// The user-visible contract is "notes ref is at-least-as-fresh-as
/// the mirror" — never "always present."
fn mirror_notes_ref(mirror_repo: &gix::Repository, object_repo: &gix::Repository) -> GitResult<()> {
    const NOTES_REF: &str = "refs/notes/heddle";
    let Ok(mut notes_ref) = mirror_repo.find_reference(NOTES_REF) else {
        // No notes in the mirror yet — nothing to mirror.
        return Ok(());
    };
    let notes_oid = notes_ref.peel_to_id().map_err(git_err)?.detach();
    copy_reachable_objects(mirror_repo, object_repo, [notes_oid])?;
    set_reference(
        object_repo,
        NOTES_REF,
        notes_oid,
        PreviousValue::Any,
        "heddle: mirror notes/heddle from bridge",
    )?;
    Ok(())
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

fn full_ref_name(update: &RefUpdate) -> String {
    match update.namespace {
        RefNamespace::Branch => format!("refs/heads/{}", update.name),
        RefNamespace::Tag => format!("refs/tags/{}", update.name),
        RefNamespace::Note => format!("refs/notes/{}", update.name),
    }
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
/// for Git-overlay URLs, where `depth` and `filter` carry through to a
/// shallow / partial clone.
///
/// * `depth` — if `Some(n)` with `n >= 1`, a shallow clone with that
///   many commits per ref (transport-v2 `deepen <n>` capability).
/// * `filter` — if `Some(spec)`, a partial-clone filter spec such as
///   `"blob:none"` is sent to the server (transport-v2 `filter`
///   capability). The resulting bare repo is also marked as a partial
///   clone (`extensions.partialClone = origin` +
///   `remote.origin.partialclonefilter = <spec>`) so subsequent fetches
///   honour the same filter.
pub fn clone_url_to_bare(
    url: &gix::Url,
    dest: &Path,
    depth: Option<u32>,
    filter: Option<&str>,
) -> GitResult<()> {
    // gix 0.80's high-level fetch builder (`Connection::prepare_fetch` →
    // `Prepare`) does not expose the v2 partial-clone `filter`
    // capability — there is no `with_filter` analogue to `with_shallow`,
    // and `gix_protocol::fetch::Arguments::filter` is only reachable
    // from inside a `Negotiate` impl whose surrounding struct gix keeps
    // private. We therefore split the path: depth-only clones stay on
    // gix (its `with_shallow(DepthAtRemote(_))` plumbs the deepen
    // capability correctly even from a fresh `init_bare`), and clones
    // that ask for a filter delegate to the user's `git` binary, which
    // speaks the full wire protocol including filter spec negotiation
    // and writes the partial-clone markers into the resulting config.
    if filter.is_some() {
        return clone_url_to_bare_via_git(url, dest, depth, filter);
    }
    clone_url_to_bare_via_gix(url, dest, depth)?;
    // gix's `init_bare` writes `.git/HEAD = ref: refs/heads/<init.defaultBranch>`
    // (typically "main" or "master") regardless of what the remote
    // advertises, and the fetch above doesn't touch HEAD. If we leave
    // that in place, downstream `select_clone_thread` and
    // `detect_git_head` will steer the user to a branch the remote may
    // not even have — observed: cloning ripgrep landed users on
    // `ag/bstr-migration` (alphabetically first imported thread) when
    // the remote's actual default is `master`. Honour the remote's
    // `HEAD` symref when we can resolve it.
    if let Some(branch) = resolve_remote_default_branch(url)
        && dest.join("refs").join("heads").join(&branch).exists()
    {
        fs::write(dest.join("HEAD"), format!("ref: refs/heads/{branch}\n"))?;
    }
    Ok(())
}

/// Resolve the remote's default branch via `git ls-remote --symref <url> HEAD`.
///
/// Returns the short branch name (e.g. `"master"`) when the remote
/// advertises `HEAD` as a symbolic ref under `refs/heads/`. Returns
/// `None` on any failure — missing `git` binary, network error,
/// detached remote HEAD, malformed output — so callers can fall back to
/// the previous heuristic without breaking.
///
/// We shell out rather than re-using the gix fetch handshake because
/// gix 0.80's high-level builder doesn't surface the symref capability
/// through the prepare/receive API: the symref metadata is parsed into
/// per-ref `Mapping`s only as a side-effect of capability negotiation,
/// and the public `Prepare` doesn't expose it on the gix versions we
/// pin. `git ls-remote --symref` is a single round trip and is
/// available wherever the `--filter` path is.
pub fn resolve_remote_default_branch(url: &gix::Url) -> Option<String> {
    let output = Command::new("git")
        .args(["ls-remote", "--symref"])
        .arg(url.to_string())
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = std::str::from_utf8(&output.stdout).ok()?;
    parse_symref_head(stdout)
}

/// Parse the first `ref: refs/heads/<branch>\tHEAD` line from
/// `git ls-remote --symref` output. Extracted so the parsing rules
/// (which catch malformed servers and detached HEAD) can be unit-tested
/// without invoking the git binary.
fn parse_symref_head(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let rest = line.strip_prefix("ref: ")?;
        let (target, label) = rest.split_once('\t')?;
        if label.trim() != "HEAD" {
            continue;
        }
        let branch = target.strip_prefix("refs/heads/")?;
        if branch.is_empty() {
            return None;
        }
        return Some(branch.to_string());
    }
    None
}

fn clone_url_to_bare_via_gix(url: &gix::Url, dest: &Path, depth: Option<u32>) -> GitResult<()> {
    fs::create_dir_all(dest)?;
    let repo = gix::init_bare(dest).map_err(git_err)?;
    let mut remote = repo.remote_at(url.clone()).map_err(git_err)?;
    remote
        .replace_refspecs(
            ["+refs/heads/*:refs/heads/*"],
            gix::remote::Direction::Fetch,
        )
        .map_err(git_err)?;
    remote = remote.with_fetch_tags(gix::remote::fetch::Tags::All);
    let connection = remote
        .connect(gix::remote::Direction::Fetch)
        .map_err(git_err)?;
    let mut prepare = connection
        .prepare_fetch(
            gix::progress::Discard,
            gix::remote::ref_map::Options::default(),
        )
        .map_err(git_err)?;
    if let Some(d) = depth.and_then(NonZeroU32::new) {
        prepare = prepare.with_shallow(gix::remote::fetch::Shallow::DepthAtRemote(d));
    }
    prepare
        .with_reflog_message(gix::remote::fetch::RefLogMessage::Override {
            message: format!("heddle: clone from {url}").into(),
        })
        .receive(gix::progress::Discard, &AtomicBool::new(false))
        .map_err(|err| GitBridgeError::Git(format!("clone failed for {url}: {err}")))?;
    Ok(())
}

fn clone_url_to_bare_via_git(
    url: &gix::Url,
    dest: &Path,
    depth: Option<u32>,
    filter: Option<&str>,
) -> GitResult<()> {
    // `git clone` refuses to write into a directory that already
    // exists and is non-empty; callers in this crate, however, often
    // pre-create the destination as an empty leaf (e.g. `ScratchDir`
    // in `bridge.rs`). Remove that empty shell so `git clone` can
    // create it itself. We deliberately only remove an *empty* dir —
    // anything else suggests the caller already wrote to the dest and
    // refusing is the safer behaviour.
    if dest.exists() {
        let is_empty = fs::read_dir(dest)?.next().is_none();
        if is_empty {
            fs::remove_dir(dest)?;
        }
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut cmd = Command::new("git");
    cmd.arg("clone").arg("--bare");
    // Force the regular wire protocol even when `url` is `file://`,
    // so the server-side `git upload-pack` advertises (and our
    // request honours) the v2 `filter` capability. Without
    // `--no-local`, git uses hardlinks or a direct pack copy and
    // skips capability negotiation entirely, which would silently
    // ignore `--filter`.
    cmd.arg("--no-local");
    if let Some(d) = depth {
        cmd.arg(format!("--depth={d}"));
    }
    if let Some(spec) = filter {
        cmd.arg(format!("--filter={spec}"));
    }
    cmd.arg(url.to_string()).arg(dest);

    let output = cmd.output().map_err(|err| {
        GitBridgeError::Git(format!("failed to spawn `git` to clone {url}: {err}"))
    })?;
    if !output.status.success() {
        return Err(GitBridgeError::Git(format!(
            "git clone failed for {url} (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

pub(crate) fn copy_reachable_objects(
    source: &gix::Repository,
    target: &gix::Repository,
    roots: impl IntoIterator<Item = ObjectId>,
) -> GitResult<()> {
    if source.object_hash() != target.object_hash() {
        return Err(GitBridgeError::Git(format!(
            "object hash mismatch: {:?} vs {:?}",
            source.object_hash(),
            target.object_hash()
        )));
    }

    for oid in collect_reachable_object_ids(source, roots)? {
        let object = source.find_object(oid).map_err(git_err)?;
        let object_ref =
            gix::objs::ObjectRef::from_bytes(object.kind, &object.data).map_err(git_err)?;
        target.write_object(object_ref).map_err(git_err)?;
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
) -> GitResult<()> {
    let mut remote = mirror_repo.remote_at(url.clone()).map_err(git_err)?;
    remote
        .replace_refspecs(
            ["+refs/heads/*:refs/heads/*"],
            gix::remote::Direction::Fetch,
        )
        .map_err(git_err)?;
    remote = remote.with_fetch_tags(gix::remote::fetch::Tags::All);

    let connection = remote
        .connect(gix::remote::Direction::Fetch)
        .map_err(git_err)?;
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

fn push_network_remote(mirror_repo: &gix::Repository, url: &gix::Url) -> GitResult<()> {
    let updates = collect_ref_updates(mirror_repo)?;
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
    for update in &updates {
        let full_name = full_ref_name(update);
        let old = remote_refs
            .get(&full_name)
            .copied()
            .unwrap_or_else(|| ObjectHashKind::Sha1.null());
        if old == update.target {
            continue;
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
    fn parse_symref_head_reads_branch_from_typical_output() {
        // Real `git ls-remote --symref <url> HEAD` shape: a `ref:`
        // symref line followed by the resolved OID/HEAD line.
        let stdout = "ref: refs/heads/master\tHEAD\n\
                      9abc123def456\tHEAD\n";
        assert_eq!(parse_symref_head(stdout), Some("master".to_string()));
    }

    #[test]
    fn parse_symref_head_handles_branch_names_with_slashes() {
        // Cloning into a non-namespaced branch like `feature/foo` is
        // exotic for a remote default, but the parser must not split
        // on the first `/` — only on the leading `refs/heads/` prefix.
        let stdout = "ref: refs/heads/release/v1\tHEAD\n";
        assert_eq!(parse_symref_head(stdout), Some("release/v1".to_string()));
    }

    #[test]
    fn parse_symref_head_returns_none_for_detached_head() {
        // A remote with detached HEAD doesn't emit a `ref:` line —
        // only a raw OID. Returning `None` keeps the caller on its
        // fallback path (alphabetical-first) rather than guessing.
        let stdout = "9abc123def456\tHEAD\n";
        assert_eq!(parse_symref_head(stdout), None);
    }

    #[test]
    fn parse_symref_head_rejects_symref_to_non_heads_namespace() {
        // A symref pointing outside `refs/heads/` (e.g. a tag) can't
        // be honoured by our flow — `select_clone_thread` only
        // imports `refs/heads/*` as threads, so we'd be pinning HEAD
        // to a name with no matching thread.
        let stdout = "ref: refs/tags/v1.0\tHEAD\n";
        assert_eq!(parse_symref_head(stdout), None);
    }

    #[test]
    fn parse_symref_head_returns_none_for_empty_branch_name() {
        // Defensive: a malformed `ref: refs/heads/\tHEAD` line
        // shouldn't crash, and shouldn't return an empty string that
        // would later become an empty thread name.
        let stdout = "ref: refs/heads/\tHEAD\n";
        assert_eq!(parse_symref_head(stdout), None);
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
