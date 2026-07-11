// SPDX-License-Identifier: Apache-2.0
//! `thread start` on the `AtomicMutation` primitive (heddle#356, impl-c).
//!
//! `heddle start` materializes a thread out of several independently-failable
//! filesystem + ref + oplog effects: a thread-ref write, an isolated-checkout
//! tree, a manifest sidecar, an optional `.cargo/config.toml` redirect, the
//! optional `--hydrate` dependency symlinks, and a `ThreadManager` record. Any
//! one of them can fail partway — most visibly `--hydrate` on a host that
//! rejects directory symlinks — and a non-atomic `start` would leave a
//! half-materialized checkout / dangling thread ref behind.
//!
//! [`StartThread`] maps the whole write-path onto the merged primitive
//! ([`repo::atomic`]) so it is all-or-nothing: each effect registers its own
//! inverse through the right combinator, and a failure anywhere rewinds every
//! applied effect — with **precise directory rewind** — back to the exact
//! pre-start state. The structure mirrors the impl-b undo/redo migration:
//!
//!   * **Atomic single writes** (the thread-ref CAS) → [`Tx::step`]
//!     (forward-first; the restore inverse is registered only after the write
//!     lands, so a failed write leaves a pre-existing ref untouched).
//!   * **Non-atomic / partial-failure-prone effects** (checkout materialize,
//!     manifest sidecar, cargo-config write, each hydrate symlink) →
//!     [`Tx::step_nonatomic`] (capture-restore, registered BEFORE the forward)
//!     or, for the genuinely all-or-nothing per-symlink create, [`Tx::step`].
//!   * **Thread-record writes** route through [`ThreadManager::converge_records`]
//!     — the same lock-atomic record-set chokepoint undo/redo uses — captured
//!     via [`ThreadManager::snapshot_records`] for the converge-back inverse.
//!   * **The oplog `ThreadCreate`** is the staged domain record handed to the
//!     executor's single commit point (it is NOT appended inside `apply`); the
//!     commit marker dedups on the stable `transaction_id`.
//!
//! Precise directory rewind is the load-bearing property (heddle#302 r4 / #324):
//! the checkout inverse removes EXACTLY what this invocation created — a
//! self-created target dir is removed wholesale, but a user-supplied
//! pre-existing empty `--path` dir is only cleared of the contents we wrote, never
//! deleted. Each hydrate symlink gets its own unlink inverse, so a partial
//! hydrate (k of N links, the (k+1)-th fails) unwinds all k links AND the
//! checkout. No thread/ref/git domain knowledge leaks into the primitive — it
//! all lives here, exactly like undo/redo's `EntrySteps`.

#[cfg(unix)]
use std::fs::File;
use std::{
    cell::Cell,
    collections::BTreeSet,
    path::{Path, PathBuf},
    rc::Rc,
};

use heddle_core::{
    CheckoutRewindPlan, SelfCreatedDirRewindPlan, TargetDirClaimKind, classify_materialize_error,
    plan_checkout_rewind, plan_self_created_dir_rewind,
};
use objects::{
    error::{HeddleError, Result as HeddleResult},
    object::{ChangeId, ThreadName},
};
use oplog::{IsolationKey, OpRecord};
use refs::RefExpectation;
use repo::{
    CheckoutMaterialization, Thread, ThreadManager, ThreadMode,
    atomic::{AtomicMutation, StagedCommit, Tx},
};

use super::{
    mount_lifecycle::{self, MountOwnership},
    worktree_cmd::{helpers::write_isolated_checkout, hydrate, shared_target::write_cargo_config},
};

/// Classify an `anyhow` error from a materialize/hydrate helper into the
/// `HeddleError` the primitive's `Result` requires.
///
/// Pure classification lives in [`heddle_core::classify_materialize_error`]
/// (heddle#571); this thin adapter keeps the local `apply_error` call sites.
fn apply_error(err: anyhow::Error) -> HeddleError {
    classify_materialize_error(err)
}

// ---- In-process fault seam (unit tests only) ----
//
// The binary integration tests drive rollback through the env-var fault
// points inside `write_isolated_checkout` (`start_materialize_checkout`) and
// `hydrate::create_symlink` (`hydrate_symlink_dir`). For in-process unit-test
// coverage of the rewind closures — which a separate-process binary run can't
// always attribute to patch coverage — this thread-local seam trips a chosen
// forward without the env-var `OnceLock` caching hazard.
#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum StartFault {
    /// Fail the checkout-materialize forward.
    Materialize,
    /// Fail the (n+1)-th hydrate-symlink forward (0 = the first link).
    HydrateNth(usize),
}

#[cfg(test)]
thread_local! {
    static START_FAULT: std::cell::Cell<Option<StartFault>> = const { std::cell::Cell::new(None) };
}

/// Arm `fault` for the duration of `body`, clearing it afterwards so it never
/// leaks into another test on the same thread.
#[cfg(test)]
pub(crate) fn with_start_fault<T>(fault: StartFault, body: impl FnOnce() -> T) -> T {
    START_FAULT.with(|f| f.set(Some(fault)));
    let out = body();
    START_FAULT.with(|f| f.set(None));
    out
}

#[cfg(test)]
fn materialize_fault_trips() -> bool {
    START_FAULT.with(|f| match f.get() {
        Some(StartFault::Materialize) => {
            f.set(None);
            true
        }
        _ => false,
    })
}

#[cfg(test)]
fn hydrate_fault_trips() -> bool {
    START_FAULT.with(|f| match f.get() {
        Some(StartFault::HydrateNth(0)) => {
            f.set(None);
            true
        }
        Some(StartFault::HydrateNth(n)) => {
            f.set(Some(StartFault::HydrateNth(n - 1)));
            false
        }
        _ => false,
    })
}

#[cfg(all(test, unix))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetSwapPoint {
    BeforeManifest,
    BeforePreserveIgnores,
}

#[cfg(all(test, unix))]
#[derive(Clone)]
struct TargetSwapFault {
    point: TargetSwapPoint,
    symlink_target: PathBuf,
}

#[cfg(all(test, unix))]
thread_local! {
    static START_TARGET_SWAP: std::cell::RefCell<Option<TargetSwapFault>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, unix))]
fn with_start_target_swap<T>(
    point: TargetSwapPoint,
    symlink_target: PathBuf,
    body: impl FnOnce() -> T,
) -> T {
    START_TARGET_SWAP.with(|f| {
        *f.borrow_mut() = Some(TargetSwapFault {
            point,
            symlink_target,
        })
    });
    let out = body();
    START_TARGET_SWAP.with(|f| *f.borrow_mut() = None);
    out
}

#[cfg(all(test, unix))]
fn maybe_swap_target_leaf(point: TargetSwapPoint, abs_path: &Path) -> HeddleResult<()> {
    let fault = START_TARGET_SWAP.with(|f| {
        let mut fault = f.borrow_mut();
        if fault.as_ref().is_some_and(|fault| fault.point == point) {
            fault.take()
        } else {
            None
        }
    });
    let Some(fault) = fault else {
        return Ok(());
    };
    let moved_name = abs_path
        .file_name()
        .map(|name| format!("{}.claimed-original", name.to_string_lossy()))
        .unwrap_or_else(|| "claimed-original".to_string());
    let moved = abs_path.with_file_name(moved_name);
    std::fs::rename(abs_path, moved).map_err(HeddleError::from)?;
    std::os::unix::fs::symlink(&fault.symlink_target, abs_path).map_err(HeddleError::from)
}

/// What the target-dir claim ([`create_target_dir`]) established about the
/// worktree leaf, and how writers/rewinds may treat it. This is the SINGLE
/// determination both the checkout writer and BOTH rewinds key on, and it carries
/// the opened directory handle captured at claim time. Later writes use that
/// handle-backed path instead of re-resolving `abs_path`, so a post-claim path
/// swap cannot redirect checkout bytes into a symlink target (heddle#356 cid
/// 3336120590).
///
/// The pure claim kind is [`TargetDirClaimKind`] in `heddle-core`; rewinds
/// consult [`plan_checkout_rewind`] / [`plan_self_created_dir_rewind`]. A leaf
/// that is anything else — a symlink, a non-directory file, or a non-empty
/// directory — is NOT representable here: `create_target_dir` refuses the start
/// with an `Err` instead, so there is no "owned" value that can stand for a
/// foreign object.
#[derive(Clone, Debug)]
pub(crate) enum TargetDir {
    /// THIS invocation created the leaf as a fresh, real, empty directory →
    /// rollback removes it wholesale (restoring "didn't exist"). Its dep symlinks
    /// go with it: `remove_dir_all` unlinks symlinks without following them.
    Created(TargetDirHandle),
    /// A real, EMPTY directory already existed at the leaf that we may safely
    /// write into — a user-supplied `--path`, or a concurrent process that
    /// created a *real empty dir* between plan time and the transaction. The
    /// checkout writes into it; rollback clears ONLY the contents we wrote and
    /// never removes the directory itself (it is not ours to delete).
    AdoptedEmpty(TargetDirHandle),
}

impl TargetDir {
    fn created(handle: TargetDirHandle) -> Self {
        Self::Created(handle)
    }

    fn adopted_empty(handle: TargetDirHandle) -> Self {
        Self::AdoptedEmpty(handle)
    }

    fn kind(&self) -> TargetDirClaimKind {
        match self {
            Self::Created(_) => TargetDirClaimKind::Created,
            Self::AdoptedEmpty(_) => TargetDirClaimKind::AdoptedEmpty,
        }
    }

    fn handle(&self) -> &TargetDirHandle {
        match self {
            Self::Created(handle) | Self::AdoptedEmpty(handle) => handle,
        }
    }
}

/// The open directory identity captured by `create_target_dir`. The handle is
/// the root of trust: checkout writes and rollback cleanup use `io_path()`, a
/// path that names this open directory (`/proc/self/fd/N` on Linux) rather than
/// the mutable user-facing leaf path.
#[derive(Clone)]
pub(crate) struct TargetDirHandle {
    #[cfg(unix)]
    dir: Rc<File>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    path: PathBuf,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl std::fmt::Debug for TargetDirHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(unix)]
        {
            f.debug_struct("TargetDirHandle")
                .field("dev", &self.dev)
                .field("ino", &self.ino)
                .finish_non_exhaustive()
        }
        #[cfg(not(unix))]
        {
            f.debug_struct("TargetDirHandle")
                .field("path", &self.path)
                .finish()
        }
    }
}

impl TargetDirHandle {
    fn open(abs_path: &Path) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

            let dir = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(abs_path)?;
            let metadata = dir.metadata()?;
            if !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "target is not a directory",
                ));
            }
            Ok(Self {
                dir: Rc::new(dir),
                dev: metadata.dev(),
                ino: metadata.ino(),
                path: abs_path.to_path_buf(),
            })
        }
        #[cfg(not(unix))]
        {
            let metadata = std::fs::symlink_metadata(abs_path)?;
            if !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "target is not a directory",
                ));
            }
            Ok(Self {
                path: abs_path.to_path_buf(),
            })
        }
    }

    #[cfg(unix)]
    fn fd_traversal_path(&self) -> Option<PathBuf> {
        use std::os::fd::AsRawFd;

        Path::new("/proc/self/fd").is_dir().then(|| {
            let fd = self.dir.as_raw_fd();
            PathBuf::from(format!("/proc/self/fd/{fd}"))
        })
    }

    fn io_path_if_current(&self, abs_path: &Path) -> std::io::Result<Option<PathBuf>> {
        #[cfg(unix)]
        {
            if let Some(path) = self.fd_traversal_path() {
                Ok(Some(path))
            } else {
                match self.same_identity_at_path(abs_path) {
                    Ok(true) => Ok(Some(self.path.clone())),
                    Ok(false) => Ok(None),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(err) if err.kind() == std::io::ErrorKind::NotADirectory => Ok(None),
                    Err(err) => Err(err),
                }
            }
        }
        #[cfg(not(unix))]
        {
            Ok((abs_path == self.path).then(|| self.path.clone()))
        }
    }

    fn same_identity_at_path(&self, abs_path: &Path) -> std::io::Result<bool> {
        #[cfg(unix)]
        {
            let current = Self::open(abs_path)?;
            Ok(current.dev == self.dev && current.ino == self.ino)
        }
        #[cfg(not(unix))]
        {
            Ok(abs_path == self.path)
        }
    }
}

fn claim_from_cell(target_claim: &Cell<Option<TargetDir>>) -> Option<TargetDir> {
    let claim = target_claim.take();
    let snapshot = claim.clone();
    target_claim.set(claim);
    snapshot
}

/// Read the checkout-materialization outcome the checkout step recorded, without
/// consuming it from the cell (take → clone → set back), so a later step (the
/// manifest stage) can observe whether the checkout was withheld. Mirrors
/// [`claim_from_cell`]. The cell is set by `stage_checkout`'s forward, which the
/// executor runs before `stage_manifest`'s forward.
fn outcome_from_cell(
    cell: &Cell<Option<CheckoutMaterialization>>,
) -> Option<CheckoutMaterialization> {
    let outcome = cell.take();
    let snapshot = outcome.clone();
    cell.set(outcome);
    snapshot
}

/// Validate that `abs_path` is, on disk RIGHT NOW, a real (non-symlink) EMPTY
/// directory we may write the isolated checkout into and clear-on-rollback, and
/// retain the directory handle that later writes/rewinds must use. Opening uses
/// `O_NOFOLLOW | O_DIRECTORY` on Unix, so a symlink leaf is refused before it can
/// become an owned claim.
fn adopt_existing_empty_dir(abs_path: &Path) -> HeddleResult<TargetDirHandle> {
    let handle = TargetDirHandle::open(abs_path)
        .map_err(|_| target_dir_shape_refusal(abs_path, &target_dir_shape_reason(abs_path)))?;
    let io_path = handle
        .io_path_if_current(abs_path)
        .map_err(HeddleError::from)?
        .ok_or_else(|| target_dir_shape_refusal(abs_path, "changed since it was claimed"))?;
    if std::fs::read_dir(io_path)
        .map_err(HeddleError::from)?
        .next()
        .transpose()
        .map_err(HeddleError::from)?
        .is_some()
    {
        return Err(target_dir_shape_refusal(abs_path, "is not empty"));
    }
    Ok(handle)
}

fn target_dir_shape_reason(abs_path: &Path) -> String {
    match std::fs::symlink_metadata(abs_path) {
        Ok(meta) if meta.file_type().is_symlink() => "is a symlink".to_string(),
        Ok(meta) if !meta.is_dir() => "is not a directory".to_string(),
        Ok(_) => "could not be opened as a real directory".to_string(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => "does not exist".to_string(),
        Err(err) => format!("could not be inspected ({err})"),
    }
}

/// The refusal raised when the target leaf is not a real, empty, ownable
/// directory at claim time — a symlink, a non-directory file, or a non-empty
/// directory a concurrent process dropped in after `plan_worktree_target` saw
/// the leaf absent. Refusing here (rather than returning a "not ours" signal the
/// writer proceeds on) is what closes the data-loss class.
fn target_dir_shape_refusal(abs_path: &Path, reason: &str) -> HeddleError {
    HeddleError::Conflict(format!(
        "refusing to start: worktree target '{}' {} — not a real empty directory heddle can \
         own. A concurrent process or a pre-existing filesystem object occupies the target, and \
         heddle will not write the isolated checkout through it. Choose an empty real directory \
         for `--path`, or let heddle create a managed materialized checkout.",
        abs_path.display(),
        reason,
    ))
}

/// Remove every entry inside `dir` without removing `dir` itself, so a
/// pre-existing user-provided directory survives a rollback while the contents
/// this invocation materialized are cleared. Symlinks are unlinked directly
/// (never followed): `file_type()` does not traverse symlinks, so a symlinked
/// dep dir takes the `remove_file` branch and the origin's deps stay untouched.
fn clear_dir_contents(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

fn clear_claimed_dir_contents(abs_path: &Path, claim: &TargetDir) -> HeddleResult<()> {
    let Some(io_path) = claim
        .handle()
        .io_path_if_current(abs_path)
        .map_err(HeddleError::from)?
    else {
        return Ok(());
    };
    match clear_dir_contents(&io_path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotADirectory => Ok(()),
        Err(err) => Err(HeddleError::from(err)),
    }
}

/// The precise checkout-rewind inverse, keyed on the runtime [`TargetDir`] claim
/// from [`create_target_dir`] (NOT a stale plan-time bool — cid 3335052857 /
/// 3335586962). Pure action selection is
/// [`heddle_core::plan_checkout_rewind`]; this applies the FS side:
///   * [`CheckoutRewindPlan::ClearAndRemoveDir`] → clear contents + remove leaf
///   * [`CheckoutRewindPlan::ClearContentsOnly`] → clear contents, keep leaf
///   * [`CheckoutRewindPlan::TouchNothing`] → claim unestablished; touch nothing
///
/// Tolerant of an already-absent target so it composes with other rewind steps.
fn rewind_checkout(abs_path: &Path, claim: Option<TargetDir>) -> HeddleResult<()> {
    match plan_checkout_rewind(claim.as_ref().map(TargetDir::kind)) {
        CheckoutRewindPlan::TouchNothing => Ok(()),
        CheckoutRewindPlan::ClearContentsOnly => {
            let claim = claim.expect("ClearContentsOnly requires an adopted claim");
            clear_claimed_dir_contents(abs_path, &claim)
        }
        CheckoutRewindPlan::ClearAndRemoveDir => {
            let claim = claim.expect("ClearAndRemoveDir requires a created claim");
            clear_claimed_dir_contents(abs_path, &claim)?;
            remove_claimed_created_dir_if_still_at_path(abs_path, &claim)
        }
    }
}

fn remove_claimed_created_dir_if_still_at_path(
    abs_path: &Path,
    claim: &TargetDir,
) -> HeddleResult<()> {
    match claim.handle().same_identity_at_path(abs_path) {
        Ok(true) => {}
        Ok(false) => return Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotADirectory => return Ok(()),
        Err(err) => return Err(HeddleError::from(err)),
    }
    match std::fs::remove_dir(abs_path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(err) => Err(HeddleError::from(err)),
    }
}

fn claimed_worktree_path(claim: Option<TargetDir>, abs_path: &Path) -> HeddleResult<PathBuf> {
    match claim {
        Some(claim) => match claim.handle().same_identity_at_path(abs_path) {
            Ok(true) => claim
                .handle()
                .io_path_if_current(abs_path)
                .map_err(HeddleError::from)?
                .ok_or_else(|| target_dir_shape_refusal(abs_path, "changed since it was claimed")),
            Ok(false) => Err(target_dir_shape_refusal(
                abs_path,
                "changed since it was claimed",
            )),
            Err(err) => Err(target_dir_shape_refusal(
                abs_path,
                &format!("changed since it was claimed ({err})"),
            )),
        },
        None => Err(target_dir_shape_refusal(abs_path, "was not established")),
    }
}

/// Atomically establish that the materialization target is a real, empty, owned
/// directory, returning the [`TargetDir`] claim both directory rewinds key on —
/// or REFUSING the start (`Err`) when the leaf is anything else. This single
/// determination drives both whether the checkout writer may proceed (a refusal
/// aborts the transaction before its step runs) and how rollback treats the leaf
/// (heddle#356 cid 3335586962 / 3335052857).
///
/// `plan_created` is the plan-time observation from `plan_worktree_target` (the
/// dir was absent then). It is NOT trusted: a concurrent process can create — or
/// drop a symlink/file at — the target between plan time and now, so the shape is
/// re-established atomically HERE:
///   * `plan_created == false` → `plan_worktree_target` accepted a pre-existing,
///     validated-empty, non-symlink user `--path` dir. Re-validate the shape now
///     (TOCTOU) and adopt it ([`TargetDir::AdoptedEmpty`]) — never created, never
///     removed by us — or refuse if it is no longer a real empty dir.
///   * `plan_created == true` → `create_dir` (NOT `create_dir_all`) on the leaf:
///     - `Ok` → this start created the leaf → [`TargetDir::Created`].
///     - `AlreadyExists` → a concurrent process won the race. It may have created
///       a real empty dir (safe to adopt) OR dropped a symlink / file / non-empty
///       dir — `adopt_existing_empty_dir` adopts the former and REFUSES the
///       latter, so the checkout is never written through a foreign object and
///       rollback never clears/deletes through it (cid 3335586962, data loss).
///
/// Parents are created with `create_dir_all` (shared infrastructure, left in
/// place on rollback — `remove_self_created_dir` only targets the leaf).
fn create_target_dir(abs_path: &Path, plan_created: bool) -> HeddleResult<TargetDir> {
    if !plan_created {
        return adopt_existing_empty_dir(abs_path).map(TargetDir::adopted_empty);
    }
    if let Some(parent) = abs_path.parent() {
        std::fs::create_dir_all(parent).map_err(HeddleError::from)?;
    }
    match std::fs::create_dir(abs_path) {
        Ok(()) => TargetDirHandle::open(abs_path)
            .map(TargetDir::created)
            .map_err(HeddleError::from),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            adopt_existing_empty_dir(abs_path).map(TargetDir::adopted_empty)
        }
        Err(err) => Err(HeddleError::from(err)),
    }
}

/// Inverse of the target-dir creation step: remove the worktree directory ONLY
/// when this invocation created it. Pure action selection is
/// [`heddle_core::plan_self_created_dir_rewind`]; this applies the FS side.
/// `claim` is the runtime [`TargetDir`] signal from [`create_target_dir`], NOT
/// a stale plan-time bool — an adopted (concurrent or user-supplied) dir, or a
/// leaf this step never established (`None`), is never removed here. Tolerant
/// of an already-absent dir so it composes after the checkout rewind (which
/// removes a self-created dir first; this then no-ops on `NotFound`).
fn remove_self_created_dir(abs_path: &Path, claim: Option<TargetDir>) -> HeddleResult<()> {
    match plan_self_created_dir_rewind(claim.as_ref().map(TargetDir::kind)) {
        SelfCreatedDirRewindPlan::TouchNothing => Ok(()),
        SelfCreatedDirRewindPlan::RemoveIfStillAtPath => {
            let claim = claim.expect("RemoveIfStillAtPath requires a created claim");
            remove_claimed_created_dir_if_still_at_path(abs_path, &claim)
        }
    }
}

/// A fully all-or-nothing `thread start` (the bytes-on-disk + virtualized
/// write-path). Holds owned inputs the surrounding `start_thread` precomputed
/// from reads; `apply` performs the staged writes and registers each effect's
/// inverse. The executor reaches the single commit point exactly once.
pub(crate) struct StartThread {
    /// Retry-stable idempotency key (derived in `start_thread` from scope +
    /// name + base state + a per-start epoch — NOT the advancing oplog head).
    /// Identical across retries of the same logical start so a crash-retry dedups
    /// instead of double-applying (heddle#356 cid 3333881568), yet distinct for a
    /// genuinely-new start after a prior committed-then-dropped one whose ref
    /// still points at the same base (cid 3335052848). See
    /// `start_thread::start_transaction_id` / `resolve_start_epoch`.
    pub transaction_id: String,
    /// The thread name (ref name + record key).
    pub name: String,
    /// The base state the checkout materializes / the ref points at.
    pub base_state: ChangeId,
    /// `Some(prior)` when a thread ref already exists (re-start reuses it via a
    /// CAS against `prior`); `None` for a brand-new thread (CAS-against-missing
    /// + a staged `ThreadCreate` commit record).
    pub existing_thread_state: Option<ChangeId>,
    pub thread_mode: ThreadMode,
    /// The materialization target (mount point for virtualized).
    pub abs_path: PathBuf,
    /// The plan-time observation (`plan_worktree_target`) that `abs_path` was
    /// absent, so this start expects to create it. It is only the INPUT to the
    /// at-creation ownership recheck ([`create_target_dir`]); the rewind keys on
    /// that runtime result, not this stale bool, so a concurrent create between
    /// plan and the transaction is never deleted (cid 3335052857).
    pub target_dir_created: bool,
    /// The candidate `--shared-target` redirect dir, or `None`. The cargo-config
    /// write reports whether it actually applied; if not, the record's
    /// `shared_target_dir` is cleared before it is persisted.
    pub shared_target_dir: Option<PathBuf>,
    pub hydrate: bool,
    /// Mount ownership for the virtualized path (unused for solid/materialized).
    pub mount_ownership: MountOwnership,
    /// The fully-built thread record; `shared_target_dir` is reconciled with the
    /// cargo-config outcome inside `apply` before it is converged onto disk.
    pub record: Thread,
}

pub(crate) struct StartThreadOutput {
    pub linked: Vec<String>,
    pub fskit_readiness: Option<mount_lifecycle::FskitReadinessReport>,
}

impl StartThread {
    /// Create the materialization target directory as the FIRST transaction
    /// step, so it lands on the rewind ledger and a self-created dir is removed
    /// on any later failure. The command resolves + validates the target before
    /// `execute` but defers the actual `create_dir_all` to here — otherwise a
    /// failure in the remaining pre-transaction work would orphan a dir created
    /// before the executor had a ledger (heddle#356 cid 3333881552).
    ///
    /// The inverse removes the directory ONLY when this invocation created it
    /// ([`TargetDir::Created`]); an adopted pre-existing/concurrent dir, or a leaf
    /// the forward refused (a symlink/foreign object — the cell stays `None`), is
    /// never removed (cid 3335052857 / 3335586962). The [`TargetDir`] claim is
    /// decided at the moment of creation by [`create_target_dir`] and stored in
    /// `target_claim`, a cell BOTH this inverse and the checkout rewind read so
    /// neither trusts the stale plan-time bool. Forward-first (`Tx::step`): the
    /// inverse is registered only after the create succeeds, and a forward that
    /// refused (or never ran) leaves the cell `None`, so rollback touches nothing.
    fn stage_target_dir(
        &self,
        tx: &mut Tx<'_>,
        target_claim: Rc<Cell<Option<TargetDir>>>,
    ) -> HeddleResult<()> {
        let abs = self.abs_path.clone();
        let rewind_abs = self.abs_path.clone();
        let plan_created = self.target_dir_created;
        let fwd_claim = Rc::clone(&target_claim);
        tx.step(
            move || {
                let established = create_target_dir(&abs, plan_created)?;
                fwd_claim.set(Some(established));
                Ok(())
            },
            move || remove_self_created_dir(&rewind_abs, claim_from_cell(&target_claim)),
        )
    }

    /// Stage the thread-ref write (atomic, forward-first), pushing the staged
    /// `ThreadCreate` commit record for a brand-new thread.
    fn stage_ref(&self, tx: &mut Tx<'_>, oplog: &mut Vec<OpRecord>) -> HeddleResult<()> {
        let repo = tx.repo();
        let base_state = self.base_state;
        match self.existing_thread_state {
            Some(existing) => {
                let fwd_name = ThreadName::new(&self.name);
                let inv_name = ThreadName::new(&self.name);
                tx.step(
                    move || {
                        repo.refs().set_thread_cas(
                            &fwd_name,
                            RefExpectation::Value(existing),
                            &base_state,
                        )
                    },
                    move || {
                        repo.cas_guarded_thread_ref_rollback(&inv_name, base_state, Some(existing))
                    },
                )?;
            }
            None => {
                let fwd_name = ThreadName::new(&self.name);
                let inv_name = ThreadName::new(&self.name);
                tx.step(
                    move || {
                        repo.refs()
                            .set_thread_cas(&fwd_name, RefExpectation::Missing, &base_state)
                    },
                    move || repo.cas_guarded_thread_ref_rollback(&inv_name, base_state, None),
                )?;
                // The domain commit record (shape owned by the repo). The record
                // is written below by the converge step, so there is nothing to
                // snapshot at construction time (heddle#23 r2).
                oplog.push(repo.thread_create_op_record(&self.name, base_state));
            }
        }
        Ok(())
    }

    /// Materialize the isolated checkout tree (`.heddle` metadata + worktree
    /// bytes) under a capture-restore step whose inverse precisely rewinds the
    /// created directory.
    fn stage_checkout(
        &self,
        tx: &mut Tx<'_>,
        target_claim: Rc<Cell<Option<TargetDir>>>,
        checkout_outcome: Rc<Cell<Option<CheckoutMaterialization>>>,
    ) -> HeddleResult<()> {
        let repo = tx.repo();
        let abs = self.abs_path.clone();
        let rewind_abs = self.abs_path.clone();
        let base_state = self.base_state;
        let name = self.name.clone();
        let rewind_claim = Rc::clone(&target_claim);
        // The [`TargetDir`] claim set by `stage_target_dir`'s forward (which ran
        // first), read at rewind time when it is settled — never the stale
        // plan-time bool. An adopted dir is cleared (not deleted), a self-created
        // dir is removed wholesale, and a refused/unestablished leaf (`None`) is
        // left untouched (cid 3335052857 / 3335586962).
        let rewind_repo = tx.repo();
        tx.step_nonatomic(
            move || Ok(()),
            move |()| {
                let claim = claim_from_cell(&rewind_claim);
                // Drop the per-root `.leaves` / withheld-marker sidecars the
                // checkout chokepoint wrote (keyed by canonical root in the
                // SHARED heddle dir, so `rewind_checkout` — which only touches
                // the checkout DIRECTORY — never reaches them). Clear BEFORE the
                // rewind removes the dir: the canonical key is derived by
                // canonicalizing the still-present root. A rolled-back start must
                // leave no orphaned sidecar (heddle#316 r11 P2).
                if let Some(claim) = claim.as_ref()
                    && let Some(io_path) = claim
                        .handle()
                        .io_path_if_current(&rewind_abs)
                        .map_err(HeddleError::from)?
                {
                    rewind_repo.clear_materialized_root_records(&io_path)?;
                }
                rewind_checkout(&rewind_abs, claim)
            },
            move || {
                #[cfg(test)]
                if materialize_fault_trips() {
                    return Err(HeddleError::Conflict(
                        "injected materialize fault".to_string(),
                    ));
                }
                let checkout_root = claimed_worktree_path(claim_from_cell(&target_claim), &abs)?;
                // Record the gate outcome for the manifest stage: a withheld base
                // means only the courtesy stub is on disk, so the manifest stage
                // must NOT stat the unmaterialized real tree (heddle#316 r9
                // Finding 3).
                let outcome =
                    write_isolated_checkout(repo, &checkout_root, &base_state, Some(&name))
                        .map_err(apply_error)?;
                checkout_outcome.set(Some(outcome));
                Ok(())
            },
        )
    }

    /// Write the materialized-thread manifest sidecar (it lives under
    /// `.heddle/threads/<name>/`, OUTSIDE the checkout, so it needs its own
    /// inverse — the checkout rewind won't reach it).
    fn stage_manifest(
        &self,
        tx: &mut Tx<'_>,
        target_claim: Rc<Cell<Option<TargetDir>>>,
        checkout_outcome: Rc<Cell<Option<CheckoutMaterialization>>>,
    ) -> HeddleResult<()> {
        let repo = tx.repo();
        let abs = self.abs_path.clone();
        let base_state = self.base_state;
        let fwd_name = self.name.clone();
        let inv_name = self.name.clone();
        let cap_name = self.name.clone();
        // Capture the prior manifest bytes (or absence) so the inverse restores
        // it to its pre-start snapshot. A re-start that reuses an existing
        // thread ref may have an OLD materialized manifest under
        // `.heddle/threads/<name>`; blind-deleting it on rollback would lose the
        // prior manifest (heddle#356 cid 3333881561).
        tx.step_nonatomic(
            move || {
                let path = repo::thread_manifest::manifest_path(repo.heddle_dir(), &cap_name);
                Ok(std::fs::read(&path).ok())
            },
            move |prior| repo.restore_thread_manifest(&inv_name, prior),
            move || {
                #[cfg(all(test, unix))]
                maybe_swap_target_leaf(TargetSwapPoint::BeforeManifest, &abs)?;
                let checkout_root = claimed_worktree_path(claim_from_cell(&target_claim), &abs)?;
                // Branch on the checkout outcome recorded by `stage_checkout`. A
                // WITHHELD base materialized only the courtesy stub, so record a
                // withheld-consistent manifest (stub-only, no tracked-leaf stat
                // entries) rather than stat-ing the unmaterialized real tree —
                // which is what made `heddle start` on a Private base error
                // (heddle#316 r9 Finding 3). A visible/absent outcome takes the
                // normal real-tree manifest path.
                match outcome_from_cell(&checkout_outcome) {
                    Some(CheckoutMaterialization::Withheld { .. }) => repo
                        .record_withheld_thread_manifest(&fwd_name, &base_state, &checkout_root)
                        .map(|_| ()),
                    _ => repo
                        .record_thread_manifest(&fwd_name, &base_state, &checkout_root)
                        .map(|_| ()),
                }
            },
        )
    }

    /// Apply the `--shared-target` cargo-config redirect (inside the checkout),
    /// returning whether it landed (a pre-staged `.cargo/config.toml` is left
    /// untouched). Capture-restore on the config file so a later failure
    /// restores the pre-write state precisely even though the checkout rewind
    /// would also reach it.
    fn stage_cargo_config(
        &self,
        tx: &mut Tx<'_>,
        dir: &Path,
        target_claim: Rc<Cell<Option<TargetDir>>>,
    ) -> HeddleResult<bool> {
        let cap_abs = self.abs_path.clone();
        let restore_abs = self.abs_path.clone();
        let fwd_abs = self.abs_path.clone();
        let dir = dir.to_path_buf();
        let cap_claim = Rc::clone(&target_claim);
        let restore_claim = Rc::clone(&target_claim);
        tx.step_nonatomic(
            move || {
                let checkout_root = claimed_worktree_path(claim_from_cell(&cap_claim), &cap_abs)?;
                Ok(std::fs::read(checkout_root.join(".cargo").join("config.toml")).ok())
            },
            move |prior| match prior {
                Some(bytes) => {
                    let checkout_root =
                        claimed_worktree_path(claim_from_cell(&restore_claim), &restore_abs)?;
                    std::fs::write(checkout_root.join(".cargo").join("config.toml"), bytes)
                        .map_err(HeddleError::from)
                }
                None => {
                    let checkout_root =
                        claimed_worktree_path(claim_from_cell(&restore_claim), &restore_abs)?;
                    match std::fs::remove_file(checkout_root.join(".cargo").join("config.toml")) {
                        Ok(()) => Ok(()),
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(err) => Err(HeddleError::from(err)),
                    }
                }
            },
            move || {
                let checkout_root =
                    claimed_worktree_path(claim_from_cell(&target_claim), &fwd_abs)?;
                write_cargo_config(&checkout_root, &dir).map_err(apply_error)
            },
        )
    }

    /// `--hydrate`: symlink the origin's top-level ignored dependency dirs into
    /// the checkout. Each link is one forward-first `Tx::step` (a single
    /// all-or-nothing `symlink`), so its unlink inverse is registered only after
    /// the link is created and a partial hydrate unwinds every created link.
    /// Returns the names linked (for the post-commit note).
    fn stage_hydrate(
        &self,
        tx: &mut Tx<'_>,
        target_claim: Rc<Cell<Option<TargetDir>>>,
    ) -> HeddleResult<Vec<String>> {
        let repo = tx.repo();
        let sources = hydrate::hydratable_ignored_dirs(repo).map_err(apply_error)?;
        let mut linked: Vec<String> = Vec::new();
        let checkout_root = claimed_worktree_path(claim_from_cell(&target_claim), &self.abs_path)?;
        for source in &sources {
            let Some((dest, link_name)) = hydrate::plan_link(&checkout_root, source) else {
                continue;
            };
            let src = source.clone();
            let dest_fwd = dest.clone();
            let inv_checkout = checkout_root.clone();
            let inv_name = link_name.clone();
            let err_name = link_name.clone();
            tx.step(
                move || {
                    #[cfg(test)]
                    if hydrate_fault_trips() {
                        return Err(symlink_unsupported_error(&err_name));
                    }
                    hydrate::create_symlink(&src, &dest_fwd)
                        .map_err(|e| symlink_unsupported_error_from(&err_name, e))
                },
                move || {
                    hydrate::unlink_hydrated(&inv_checkout, &inv_name).map_err(HeddleError::from)
                },
            )?;
            linked.push(link_name);
        }
        // Preserve the hydrated deps' ignore rule in the checkout's
        // worktree-local, never-captured exclude file (capture-restore so a
        // later failure restores it). Writing to the exclude file — not the
        // possibly-tracked `.heddleignore` — keeps a successful `start
        // --hydrate` from dirtying tracked state (heddle#356 cid 3333881577).
        if !linked.is_empty() {
            let cap_abs = self.abs_path.clone();
            let restore_abs = self.abs_path.clone();
            let fwd_abs = self.abs_path.clone();
            let cap_claim = Rc::clone(&target_claim);
            let restore_claim = Rc::clone(&target_claim);
            let linked_fwd = linked.clone();
            tx.step_nonatomic(
                move || {
                    #[cfg(all(test, unix))]
                    maybe_swap_target_leaf(TargetSwapPoint::BeforePreserveIgnores, &cap_abs)?;
                    let checkout_root =
                        claimed_worktree_path(claim_from_cell(&cap_claim), &cap_abs)?;
                    let exclude_path = hydrate::hydrate_exclude_path(&checkout_root);
                    Ok(std::fs::read(&exclude_path).ok())
                },
                move |prior| {
                    let checkout_root =
                        claimed_worktree_path(claim_from_cell(&restore_claim), &restore_abs)?;
                    let restore_path = hydrate::hydrate_exclude_path(&checkout_root);
                    restore_ignore_file(&restore_path, prior)
                },
                move || {
                    let checkout_root =
                        claimed_worktree_path(claim_from_cell(&target_claim), &fwd_abs)?;
                    hydrate::preserve_hydrated_ignores(&checkout_root, &linked_fwd)
                        .map_err(apply_error)
                },
            )?;
        }
        Ok(linked)
    }

    /// Establish the FUSE mount for a virtualized thread, registering an unmount
    /// inverse so an outer failure tears the mount down (it would otherwise
    /// outlive the failed start — the daemon owns it across process exit).
    fn stage_mount(
        &self,
        tx: &mut Tx<'_>,
    ) -> HeddleResult<mount_lifecycle::VirtualizedMountOutcome> {
        let repo = tx.repo();
        let root = repo.root().to_path_buf();
        let abs = self.abs_path.clone();
        let fwd_name = self.name.clone();
        let inv_name = self.name.clone();
        let ownership = self.mount_ownership;
        let mounted_owner = Rc::new(Cell::new(None));
        let fwd_owner = Rc::clone(&mounted_owner);
        let inv_owner = Rc::clone(&mounted_owner);
        let inv_root = root.clone();
        tx.step(
            move || {
                let outcome =
                    mount_lifecycle::establish_virtualized_mount(&root, &fwd_name, &abs, ownership)
                        .map_err(apply_error)?;
                fwd_owner.set(Some(outcome.owner));
                Ok(outcome)
            },
            move || {
                let Some(owner) = inv_owner.get() else {
                    return Ok(());
                };
                mount_lifecycle::cleanup_virtualized_mount(&inv_root, &inv_name, owner)
                    .map_err(apply_error)?;
                Ok(())
            },
        )
    }

    /// Persist the thread record as the SOLE record under its name, via the
    /// lock-atomic [`ThreadManager::converge_records`] chokepoint. Capture the
    /// full prior same-name set so the inverse converges back to it.
    fn stage_record(&self, tx: &mut Tx<'_>) -> HeddleResult<()> {
        let repo = tx.repo();
        let record = self.record.clone();
        let fwd_name = self.name.clone();
        let inv_name = self.name.clone();
        let prior = ThreadManager::new(repo.heddle_dir()).snapshot_records(&self.name)?;
        tx.step_nonatomic(
            || Ok(prior),
            move |prior| ThreadManager::new(repo.heddle_dir()).converge_records(&inv_name, &prior),
            move || {
                ThreadManager::new(repo.heddle_dir())
                    .converge_records(&fwd_name, std::slice::from_ref(&record))
            },
        )
    }
}

impl AtomicMutation for StartThread {
    type Output = StartThreadOutput;

    fn transaction_id(&self) -> String {
        self.transaction_id.clone()
    }

    fn isolation_keys(&self, _repo: &repo::Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
        let mut keys = BTreeSet::new();
        keys.insert(IsolationKey::Thread(self.name.clone()));
        if let Some(thread) = &self.record.target_thread {
            keys.insert(IsolationKey::Thread(thread.clone()));
        }
        if let Some(thread) = &self.record.parent_thread {
            keys.insert(IsolationKey::Thread(thread.clone()));
        }
        Ok(keys)
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<StartThreadOutput>> {
        let mut oplog: Vec<OpRecord> = Vec::new();

        // The single [`TargetDir`] claim, decided atomically by
        // `stage_target_dir`'s forward and consumed by BOTH directory rewinds
        // (the target-dir inverse and the checkout rewind). It starts `None` — an
        // unestablished claim — so if the forward refuses on a symlink/foreign
        // object (or never runs), neither rewind clears or deletes through the
        // leaf (heddle#356 cid 3335052857 / 3335586962).
        let target_claim = Rc::new(Cell::new(None));

        // The checkout-materialization outcome `stage_checkout`'s forward records
        // and `stage_manifest`'s forward reads (they run in that order). It lets
        // the manifest stage record a withheld-consistent manifest when the base
        // state was withheld, instead of stat-ing the unmaterialized real tree
        // (heddle#316 r9 Finding 3). `None` until the checkout forward runs.
        let checkout_outcome = Rc::new(Cell::new(None));

        // 0. Create the target dir inside the transaction (first, so its removal
        //    inverse runs last and a self-created dir never leaks).
        self.stage_target_dir(tx, Rc::clone(&target_claim))?;

        // 1. Thread ref (+ staged ThreadCreate for a brand-new thread).
        self.stage_ref(tx, &mut oplog)?;

        // 2. Mode-specific materialization.
        let mut fskit_readiness = None;
        let linked = match self.thread_mode {
            ThreadMode::Solid | ThreadMode::Materialized => {
                self.stage_checkout(tx, Rc::clone(&target_claim), Rc::clone(&checkout_outcome))?;
                if matches!(self.thread_mode, ThreadMode::Materialized) {
                    self.stage_manifest(
                        tx,
                        Rc::clone(&target_claim),
                        Rc::clone(&checkout_outcome),
                    )?;
                }
                if let Some(dir) = self.shared_target_dir.clone() {
                    let applied = self.stage_cargo_config(tx, &dir, Rc::clone(&target_claim))?;
                    // The writer no-ops on a pre-staged config; don't advertise a
                    // redirect that isn't in effect (`thread show` would lie).
                    if !applied {
                        self.record.shared_target_dir = None;
                    }
                }
                if self.hydrate {
                    self.stage_hydrate(tx, Rc::clone(&target_claim))?
                } else {
                    Vec::new()
                }
            }
            ThreadMode::Virtualized => {
                fskit_readiness = self.stage_mount(tx)?.fskit_readiness;
                Vec::new()
            }
        };

        // 3. Thread record (sole record under the name), via converge_records.
        self.stage_record(tx)?;

        Ok(StagedCommit::new(
            StartThreadOutput {
                linked,
                fskit_readiness,
            },
            oplog,
        ))
    }
}

/// Restore the checkout's hydrate exclude file to its captured pre-hydrate
/// state: rewrite the prior bytes, or remove the file we created.
fn restore_ignore_file(path: &Path, prior: Option<Vec<u8>>) -> HeddleResult<()> {
    match prior {
        Some(bytes) => std::fs::write(path, bytes).map_err(HeddleError::from),
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(HeddleError::from(err)),
        },
    }
}

/// The user-facing error a rejected directory symlink raises. Names the
/// host/FS limitation (the integration test asserts on "directory symlink")
/// and notes the start was rolled back.
fn symlink_unsupported_error_from(link: &str, err: anyhow::Error) -> HeddleError {
    HeddleError::Conflict(format!(
        "--hydrate could not create a directory symlink for '{link}' in the new checkout. \
         This host or filesystem appears to reject directory symlinks (e.g. Windows without \
         Developer Mode / the SeCreateSymbolicLink privilege, or a filesystem that doesn't \
         support them). The partially-created thread has been rolled back — re-run \
         `heddle start` without --hydrate, or enable directory-symlink support on this host \
         and retry. (cause: {err:#})"
    ))
}

#[cfg(test)]
fn symlink_unsupported_error(link: &str) -> HeddleError {
    symlink_unsupported_error_from(link, anyhow::anyhow!("injected hydrate symlink fault"))
}

#[cfg(test)]
mod tests {
    use repo::Repository;
    use tempfile::TempDir;

    use super::{
        super::{
            thread::{
                find_active_thread_entry, resolve_start_epoch, start_thread, start_transaction_id,
            },
            thread_cmd::drop_thread_silent,
            worktree_cmd::helpers::plan_worktree_target,
        },
        *,
    };
    use crate::cli::{ThreadStartArgs, WorkspaceModeArg};

    /// heddle#571 (Bug 1): a non-conflict failure on the start path must NOT be
    /// reported as `conflict:`. The macOS regression was a `clonefile` ENOENT
    /// (an `io::Error`) bubbling through the materialize helper's
    /// `anyhow::Result` and getting blanket-wrapped as `HeddleError::Conflict`,
    /// surfacing to the user as `conflict: No such file or directory (os error
    /// 2)` — wrong, and it blocked diagnosis. `apply_error` is the classifier on
    /// that path; assert it preserves the real variant.
    #[test]
    fn apply_error_preserves_io_and_does_not_mislabel_as_conflict() {
        // A bare io::Error (what `clonefile`/`FICLONE` produce on ENOENT).
        let bare_io = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory (os error 2)",
        ));
        let mapped = apply_error(bare_io);
        assert!(
            matches!(mapped, HeddleError::Io(_)),
            "a bare io error must surface as Io, got {mapped:?}"
        );
        assert!(
            !format!("{mapped}").starts_with("conflict:"),
            "io error must not be reported as a conflict: {mapped}"
        );

        // The real shape from the materialize path: an io error already
        // converted to `HeddleError::Io`, then propagated through the helper's
        // `anyhow::Result` via `?`. `apply_error` must recover the Io variant.
        let structured_io = anyhow::Error::new(HeddleError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory (os error 2)",
        )));
        assert!(
            matches!(apply_error(structured_io), HeddleError::Io(_)),
            "a propagated HeddleError::Io must keep its variant"
        );

        // A genuine merge/visibility conflict still maps to Conflict.
        let conflict = anyhow::Error::new(HeddleError::Conflict("real merge conflict".to_string()));
        assert!(
            matches!(apply_error(conflict), HeddleError::Conflict(_)),
            "a genuine conflict must remain a conflict"
        );
    }

    /// heddle#571 (round 2, finding 1): reclassifying an io failure must NOT
    /// drop the `anyhow` context. The real shape is `write_cargo_config` doing
    /// `fs::write(..).with_context(|| "writing .cargo/config.toml to {path}")?` —
    /// the chain's outer layer is the context string, its source the io::Error.
    /// `apply_error` must surface it as `Io` (kind preserved) WHILE keeping the
    /// path/action in the message, so `heddle start --shared-target` failing to
    /// write the cargo config still tells the user which file/action failed.
    #[test]
    fn apply_error_preserves_context_when_reclassifying_io() {
        use anyhow::Context as _;

        let with_ctx = Err::<(), _>(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "os error 13",
        ))
        .context("writing .cargo/config.toml to /work/.cargo/config.toml")
        .unwrap_err();

        let mapped = apply_error(with_ctx);
        // Kind preserved so `exit::from_error` still classifies it correctly.
        assert!(
            matches!(&mapped, HeddleError::Io(io) if io.kind() == std::io::ErrorKind::PermissionDenied),
            "io kind must survive reclassification, got {mapped:?}"
        );
        // The path/action context must NOT be flattened away.
        let msg = format!("{mapped}");
        assert!(
            msg.contains(".cargo/config.toml") && msg.contains("writing"),
            "reclassified io error must retain the path/action context: {msg}"
        );
    }

    /// A `--path` solid-thread start that pins its base on `from` (no current-
    /// state bootstrap) and never spawns a daemon — minimal machinery for the
    /// in-process rollback assertions.
    fn solid_args(
        name: &str,
        path: &std::path::Path,
        from: &ChangeId,
        hydrate: bool,
    ) -> ThreadStartArgs {
        ThreadStartArgs {
            name: name.to_string(),
            from: Some(from.to_string()),
            path: Some(path.to_path_buf()),
            workspace: WorkspaceModeArg::Solid,
            agent_provider: None,
            agent_model: None,
            task: None,
            parent_thread: None,
            automated: true,
            print_cd_path: false,
            daemon: false,
            no_daemon: true,
            shared_target: false,
            hydrate,
        }
    }

    fn materialized_args(
        name: &str,
        path: &std::path::Path,
        from: &ChangeId,
        hydrate: bool,
    ) -> ThreadStartArgs {
        let mut args = solid_args(name, path, from, hydrate);
        args.workspace = WorkspaceModeArg::Materialized;
        args
    }

    fn has_thread_ref(repo: &Repository, name: &str) -> bool {
        repo.refs()
            .get_thread(&ThreadName::new(name))
            .unwrap()
            .is_some()
    }

    fn has_thread_record(repo: &Repository, name: &str) -> bool {
        ThreadManager::new(repo.heddle_dir())
            .find_by_thread(name)
            .unwrap()
            .is_some()
    }

    /// Repo with one captured state holding a tracked `a.txt` (+ optional
    /// ignored dep dirs). Returns the temp dir, repo, and base state id.
    fn repo_with_state(deps: &[&str]) -> (TempDir, Repository, ChangeId) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("a.txt"), "a").unwrap();
        if !deps.is_empty() {
            let ignore = deps.iter().map(|d| format!("{d}/\n")).collect::<String>();
            std::fs::write(temp.path().join(".heddleignore"), ignore).unwrap();
            for dep in deps {
                std::fs::create_dir_all(temp.path().join(dep)).unwrap();
            }
        }
        let state = repo.snapshot(Some("s1".to_string()), None).unwrap();
        (temp, repo, state.change_id)
    }

    #[test]
    fn start_happy_path_materializes_records_and_hydrates() {
        let (temp, repo, state) = repo_with_state(&["dep_a", "dep_b"]);
        let checkout = temp.path().join("iso");
        let out = start_thread(&repo, solid_args("iso", &checkout, &state, true));
        assert!(
            out.is_ok(),
            "happy-path start should succeed: {:?}",
            out.err()
        );

        assert!(
            checkout.join(".heddle").is_dir(),
            "checkout .heddle should exist"
        );
        assert!(
            checkout.join("a.txt").is_file(),
            "tracked file should materialize"
        );
        // Both ignored dep dirs hydrated as symlinks.
        for dep in ["dep_a", "dep_b"] {
            assert!(
                std::fs::symlink_metadata(checkout.join(dep))
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false),
                "{dep} should be hydrated as a symlink"
            );
        }
        assert!(has_thread_ref(&repo, "iso"), "thread ref should be created");
        assert!(
            has_thread_record(&repo, "iso"),
            "thread record should be persisted"
        );
    }

    /// #316 / PR #528 r9 Finding 3: `heddle start` on a Private base_state must
    /// yield a WITHHELD checkout, not error. `write_isolated_checkout` used to
    /// discard the `CheckoutMaterialization::Withheld` outcome, so the start path
    /// went on to `record_thread_manifest`, which stats the REAL state tree — but
    /// those files were intentionally not materialized (only the courtesy stub is
    /// on disk). With the outcome propagated, the manifest stage records a
    /// withheld-consistent manifest instead, the start succeeds, and a later
    /// capture of the withheld checkout is a no-op.
    #[test]
    fn start_on_private_base_yields_withheld_checkout_not_error() {
        use objects::object::{Principal, StateVisibility, VisibilityTier};

        // Mirror the gate's operator-local stub filename (the const is
        // repo-crate-private).
        const COURTESY_STUB_FILENAME: &str = "HEDDLE-EMBARGO.txt";

        let (temp, repo, state) = repo_with_state(&[]);
        // Embargo the base state Private — withheld even from the all-seeing
        // `Internal` audience the start path materializes under.
        repo.put_state_visibility(StateVisibility {
            state,
            tier: VisibilityTier::Private {
                scope_label: "sec-embargo".into(),
            },
            embargo_until: None,
            declarer: Principal {
                name: "Grace Hopper".into(),
                email: "grace@example.com".into(),
            },
            declared_at: chrono::Utc::now(),
            signature: None,
            supersedes: None,
        })
        .expect("put visibility");

        let checkout = temp.path().join("iso");
        // Materialized start (so a manifest sidecar is recorded) of a Private
        // base. Pre-fix this errored; now it must succeed with a withheld
        // checkout.
        let out = start_thread(&repo, materialized_args("iso", &checkout, &state, false));
        assert!(
            out.is_ok(),
            "start on a Private base must succeed (withheld checkout), got {:?}",
            out.err()
        );

        // The worktree holds the courtesy stub and NONE of the base's tracked
        // bytes.
        assert!(
            checkout.join(COURTESY_STUB_FILENAME).exists(),
            "a withheld start must write the courtesy stub"
        );
        assert!(
            !checkout.join("a.txt").exists(),
            "the Private base's tracked bytes must NOT be materialized"
        );

        // The recorded manifest reflects the withheld checkout: marked withheld,
        // with NO real-tree stat-cache entries.
        let manifest = repo::thread_manifest::read_manifest(repo.heddle_dir(), "iso")
            .unwrap()
            .expect("manifest must be recorded");
        assert!(
            manifest.withheld,
            "manifest must mark the checkout withheld"
        );
        assert!(
            manifest.files.is_empty(),
            "withheld manifest must record NO tracked-leaf stat entries, got {:?}",
            manifest.files.keys().collect::<Vec<_>>()
        );

        // A capture of the withheld checkout is a no-op (non-capturable).
        let outcome = repo
            .capture_thread_from_disk("iso", &checkout)
            .expect("capture of a withheld checkout must not error");
        assert_eq!(
            outcome,
            repo::ThreadCaptureOutcome::NoOp,
            "a withheld checkout is non-capturable"
        );
    }

    #[test]
    fn start_shared_target_writes_cargo_config_through_claimed_dir() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
        let state = repo
            .snapshot(Some("rust workspace".to_string()), None)
            .unwrap();
        let checkout = temp.path().join("iso");
        let mut args = solid_args("iso", &checkout, &state.change_id, false);
        args.shared_target = true;

        start_thread(&repo, args).expect("shared-target start should succeed");

        let config = std::fs::read_to_string(checkout.join(".cargo").join("config.toml"))
            .expect("cargo config should be written inside the checkout");
        assert!(
            config.contains(".heddle/targets"),
            "cargo config should point at the shared target dir: {config}"
        );
        let record = ThreadManager::new(repo.heddle_dir())
            .load("iso")
            .unwrap()
            .expect("thread record should persist");
        assert!(
            record.shared_target_dir.is_some(),
            "record should advertise the applied shared target"
        );
    }

    #[test]
    fn start_materialize_fault_rolls_back_self_created_dir() {
        let (temp, repo, state) = repo_with_state(&[]);
        let checkout = temp.path().join("iso");
        let out = with_start_fault(StartFault::Materialize, || {
            start_thread(&repo, solid_args("iso", &checkout, &state, false))
        });
        assert!(out.is_err(), "a materialize fault must fail the start");
        assert!(
            std::fs::symlink_metadata(&checkout).is_err(),
            "the self-created checkout must be removed on rollback"
        );
        assert!(
            !has_thread_ref(&repo, "iso"),
            "the thread ref must be rolled back"
        );
        assert!(
            !has_thread_record(&repo, "iso"),
            "no record must survive the rollback"
        );
    }

    #[test]
    fn start_materialize_fault_preserves_preexisting_dir() {
        let (temp, repo, state) = repo_with_state(&[]);
        let checkout = temp.path().join("iso");
        std::fs::create_dir(&checkout).unwrap();
        let out = with_start_fault(StartFault::Materialize, || {
            start_thread(&repo, solid_args("iso", &checkout, &state, false))
        });
        assert!(out.is_err(), "a materialize fault must fail the start");
        // The user-supplied dir survives, emptied of what we materialized.
        assert!(
            checkout.is_dir(),
            "a pre-existing user dir must not be deleted"
        );
        let remaining: Vec<_> = std::fs::read_dir(&checkout)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            remaining.is_empty(),
            "rollback must clear materialized contents: {remaining:?}"
        );
        assert!(
            !has_thread_ref(&repo, "iso"),
            "the thread ref must be rolled back"
        );
    }

    #[test]
    fn start_partial_hydrate_unwinds_links_and_checkout() {
        let (temp, repo, state) = repo_with_state(&["dep_a", "dep_b"]);
        let checkout = temp.path().join("iso");
        // Fail the SECOND hydrate link (dep_b, sorted) → the first (dep_a) must
        // still be unwound, along with the whole checkout.
        let out = with_start_fault(StartFault::HydrateNth(1), || {
            start_thread(&repo, solid_args("iso", &checkout, &state, true))
        });
        assert!(out.is_err(), "a partial hydrate must fail the start");
        assert!(
            std::fs::symlink_metadata(&checkout).is_err(),
            "a partial hydrate must remove the checkout (and every created link)"
        );
        assert!(
            !has_thread_ref(&repo, "iso"),
            "the thread ref must be rolled back"
        );
        // The origin's dep dirs are untouched (we unlink, never follow).
        assert!(temp.path().join("dep_a").is_dir());
        assert!(temp.path().join("dep_b").is_dir());
    }

    /// Sorted file names directly under `dir` (empty when the dir is absent) —
    /// used to detect whether a failed start orphaned a per-root sidecar.
    fn sidecar_entries(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// #316 / PR #528 r11 P2: `checkout_state_gated` (reached by atomic
    /// `start --path`) writes a per-root `.leaves` materialized-leaves record (and,
    /// for a withheld base, a withheld marker) under the SHARED heddle dir, keyed
    /// by the canonical worktree root. Those sidecars sit OUTSIDE the checkout
    /// directory, so the checkout-dir rewind never reaches them. A later start
    /// step that fails and rolls the transaction back must therefore drop them
    /// explicitly, or they orphan. The `stage_checkout` inverse now clears the
    /// per-root records before rewinding the dir.
    #[test]
    fn failed_atomic_start_rolls_back_leaves_sidecar() {
        let (temp, repo, state) = repo_with_state(&["dep_a", "dep_b"]);
        let checkout = temp.path().join("iso");

        let roots_dir = repo.heddle_dir().join("materialized-roots");
        let withheld_dir = repo.heddle_dir().join("withheld-checkouts");
        let before_roots = sidecar_entries(&roots_dir);
        let before_withheld = sidecar_entries(&withheld_dir);

        // Fail the SECOND hydrate link — AFTER the checkout (and its per-root
        // `.leaves` sidecar) is fully written — so the rollback must reach the
        // root-keyed sidecars the checkout-dir rewind cannot.
        let out = with_start_fault(StartFault::HydrateNth(1), || {
            start_thread(&repo, solid_args("iso", &checkout, &state, true))
        });
        assert!(out.is_err(), "a partial hydrate must fail the start");
        assert!(
            std::fs::symlink_metadata(&checkout).is_err(),
            "the checkout dir must be removed on rollback"
        );

        let after_roots = sidecar_entries(&roots_dir);
        let after_withheld = sidecar_entries(&withheld_dir);
        assert_eq!(
            before_roots, after_roots,
            "a rolled-back start must not orphan a per-root .leaves sidecar"
        );
        assert_eq!(
            before_withheld, after_withheld,
            "a rolled-back start must not orphan a withheld marker"
        );
    }

    #[cfg(unix)]
    #[test]
    fn start_manifest_refuses_swapped_target_and_spares_symlink_target() {
        let (temp, repo, state) = repo_with_state(&[]);
        let checkout = temp.path().join("iso");
        let victim = temp.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("a.txt"), b"victim").unwrap();
        std::fs::write(victim.join("precious.txt"), b"precious").unwrap();

        let out = with_start_target_swap(TargetSwapPoint::BeforeManifest, victim.clone(), || {
            start_thread(&repo, materialized_args("iso", &checkout, &state, false))
        });
        assert!(
            out.is_err(),
            "manifest staging must refuse when the claimed target leaf was swapped"
        );
        assert!(
            std::fs::symlink_metadata(&checkout)
                .unwrap()
                .file_type()
                .is_symlink(),
            "rollback must leave the substituted symlink itself alone"
        );
        assert_eq!(
            std::fs::read(victim.join("a.txt")).unwrap(),
            b"victim",
            "manifest staging must not read/stat through the swapped symlink target"
        );
        assert_eq!(
            std::fs::read(victim.join("precious.txt")).unwrap(),
            b"precious",
            "rollback must not touch the symlink target"
        );
        assert!(
            repo::thread_manifest::manifest_path(repo.heddle_dir(), "iso")
                .symlink_metadata()
                .is_err(),
            "a refused manifest stage must not leave a sidecar behind"
        );
        assert!(
            !has_thread_ref(&repo, "iso"),
            "the thread ref must be rolled back"
        );
    }

    #[cfg(unix)]
    #[test]
    fn preserve_hydrated_ignores_refuses_swapped_target_and_spares_symlink_target() {
        let (temp, repo, state) = repo_with_state(&["dep_a"]);
        let checkout = temp.path().join("iso");
        let victim = temp.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("precious.txt"), b"precious").unwrap();

        let out = with_start_target_swap(
            TargetSwapPoint::BeforePreserveIgnores,
            victim.clone(),
            || start_thread(&repo, solid_args("iso", &checkout, &state, true)),
        );
        assert!(
            out.is_err(),
            "hydrate ignore preservation must refuse when the claimed target leaf was swapped"
        );
        assert!(
            std::fs::symlink_metadata(&checkout)
                .unwrap()
                .file_type()
                .is_symlink(),
            "rollback must leave the substituted symlink itself alone"
        );
        assert!(
            !victim.join(".heddle").exists(),
            "hydrate ignore preservation must not create an exclude file through the symlink target"
        );
        assert_eq!(
            std::fs::read(victim.join("precious.txt")).unwrap(),
            b"precious",
            "rollback must not touch the symlink target"
        );
        assert!(
            !has_thread_ref(&repo, "iso"),
            "the thread ref must be rolled back"
        );
    }

    // ---- heddle#356 r2 fixes ----

    /// cid 3333881552: the target dir must be created INSIDE the transaction.
    /// `plan_worktree_target` resolves + validates but defers creation, so a
    /// failure in the remaining pre-transaction work can't orphan a directory.
    #[test]
    fn plan_worktree_target_defers_dir_creation() {
        let (temp, repo, _state) = repo_with_state(&[]);
        let target = temp.path().join("iso-deferred");
        let prepared = plan_worktree_target(&repo, &target, None).unwrap();
        assert!(
            prepared.target_dir_created,
            "a non-existent target is flagged as one this start will create"
        );
        assert!(
            std::fs::symlink_metadata(&target).is_err(),
            "plan must NOT create the target dir — creation is deferred into the \
             transaction so a pre-execute failure can't orphan it"
        );
    }

    /// cid 3333881568: the transaction key must not fold in the live oplog head,
    /// which advances when the commit marker appends. A re-derivation after an
    /// unrelated oplog advance must yield the identical key.
    #[test]
    fn start_transaction_id_is_stable_across_oplog_advance() {
        let (temp, repo, state) = repo_with_state(&[]);
        let scope = repo.op_scope();
        // A fixed epoch isolates the property under test (oplog-head independence)
        // from the epoch's own freshness.
        let epoch = chrono::Utc::now();
        let id1 = start_transaction_id(&scope, "iso", &state, epoch);
        // Advance the oplog head with an unrelated capture.
        std::fs::write(temp.path().join("b.txt"), "b").unwrap();
        repo.snapshot(Some("s2".to_string()), None).unwrap();
        let id2 = start_transaction_id(&scope, "iso", &state, epoch);
        assert_eq!(
            id1, id2,
            "the start transaction key must be independent of the advancing oplog head"
        );
    }

    /// cid 3333881568 + 3335052848: a crash-retry of the SAME start dedups
    /// exactly-once. After a start commits, its thread record is still Active, so
    /// `resolve_start_epoch` returns that record's creation instant and the key
    /// folds back to the committed one — the executor's `transaction_id` dedup
    /// makes the retry a no-op instead of double-applying.
    #[test]
    fn crash_retry_of_same_start_rederives_committed_key() {
        let (temp, repo, state) = repo_with_state(&[]);
        let checkout = temp.path().join("iso");
        let scope = repo.op_scope();
        start_thread(&repo, solid_args("iso", &checkout, &state, false))
            .expect("start should succeed");
        // A crash-retry re-derives the epoch from the still-Active record, so the
        // key matches the committed batch and the executor dedups it.
        let epoch = resolve_start_epoch(&repo, "iso").unwrap();
        let retry_id = start_transaction_id(&scope, "iso", &state, epoch);
        assert!(
            !repo
                .oplog()
                .committed_batch_records(&retry_id)
                .unwrap()
                .is_empty(),
            "a crash-retry must re-derive the committed key (the Active record's epoch) so \
             the executor dedups it instead of re-applying the committed start"
        );
    }

    /// cid 3335052848: a genuinely-new start after a SILENT drop (which keeps the
    /// ref at the same base) must NOT be deduped into the dropped start's
    /// committed marker — it must actually materialize a fresh checkout + record.
    /// Pre-fix the base-only key collided, so the second start dedup-rewound to a
    /// no-op (empty checkout, Abandoned record).
    #[test]
    fn start_after_silent_drop_at_same_base_actually_starts() {
        let (temp, repo, state) = repo_with_state(&[]);
        let checkout = temp.path().join("iso");

        start_thread(&repo, solid_args("iso", &checkout, &state, false))
            .expect("first start should succeed");
        // Silent drop (delete_thread = false) keeps the ref at `state` and leaves
        // an Abandoned record; force skips the worktree-clean guard.
        drop_thread_silent(&repo, "iso", false, true).expect("silent drop should succeed");
        assert!(
            has_thread_ref(&repo, "iso"),
            "a silent drop keeps the ref at the same base (the collision premise)"
        );
        assert!(
            std::fs::symlink_metadata(&checkout).is_err(),
            "the drop removes the prior checkout"
        );

        // The new start mints a fresh epoch (the prior record is Abandoned), so
        // its key differs from the dropped start's committed marker and it runs.
        start_thread(&repo, solid_args("iso", &checkout, &state, false))
            .expect("start after a silent drop must actually start, not dedup to a no-op");
        assert!(
            checkout.join(".heddle").is_dir(),
            "the restart must re-materialize the checkout (not dedup into a no-op)"
        );
        let record = ThreadManager::new(repo.heddle_dir())
            .load("iso")
            .unwrap()
            .expect("the restart must persist a record");
        assert_eq!(
            record.state,
            repo::ThreadState::Active,
            "the restart must leave an Active record, not the dropped Abandoned one"
        );
    }

    // ---- heddle#356 r4 close-the-class: Class A commit-detection gate ----

    /// cid 3335586969 (the new sibling): a committed-before-bookkeeping retry.
    /// The start transaction committed (checkout + ref + record + commit marker),
    /// then a crash interrupted the post-commit `AgentRegistry` reservation — so
    /// there is NO live reservation, yet the checkout is now NON-EMPTY. The retry
    /// must be RECOGNIZED as a committed retry (the epoch re-derived from the
    /// durable Active record yields the SAME key) and short-circuit exactly-once,
    /// NOT rejected by `plan_worktree_target`'s `worktree_target_not_empty` (which
    /// fires before `execute` could ever see the matching `TransactionCommit`).
    #[test]
    fn committed_before_bookkeeping_retry_short_circuits() {
        let (temp, repo, state) = repo_with_state(&[]);
        let checkout = temp.path().join("iso");

        // A first start fully succeeds: checkout + ref + Active record + commit
        // marker + the post-commit AgentRegistry reservation.
        start_thread(&repo, solid_args("iso", &checkout, &state, false))
            .expect("first start should succeed");
        assert!(
            checkout.join(".heddle").is_dir(),
            "the first start materialized the checkout"
        );

        // Recreate the crash window: the write-path committed, but the post-commit
        // bookkeeping never landed — delete the reservation so no live owner
        // exists (find_active_thread_entry → None, the committed-before-bookkeeping
        // state). The checkout, ref, record, and commit marker all remain.
        let entry = find_active_thread_entry(&repo, "iso")
            .unwrap()
            .expect("the first start created a reservation");
        objects::store::AgentRegistry::new(repo.heddle_dir())
            .delete(&entry.session_id)
            .unwrap();
        assert!(
            find_active_thread_entry(&repo, "iso").unwrap().is_none(),
            "the reservation is gone — exactly the committed-before-bookkeeping window"
        );

        // The retry must NOT fail with worktree_target_not_empty: commit-detection
        // gates the preflight, so the committed marker short-circuits the start and
        // completes the interrupted bookkeeping instead.
        start_thread(&repo, solid_args("iso", &checkout, &state, false)).expect(
            "a committed-before-bookkeeping retry must short-circuit, not be rejected by \
             worktree_target_not_empty",
        );

        // The committed checkout is left exactly as it was — neither re-created nor
        // cleared (the short-circuit never ran the write-path or its rewind).
        assert!(
            checkout.join(".heddle").is_dir(),
            "the committed checkout survives the retry"
        );
        assert!(
            checkout.join("a.txt").is_file(),
            "the committed tree survives the retry"
        );

        // The record stays the single committed Active record (no duplicate, no
        // Abandoned). And the interrupted reservation is now completed.
        let record = ThreadManager::new(repo.heddle_dir())
            .load("iso")
            .unwrap()
            .expect("the committed record persists");
        assert_eq!(record.state, repo::ThreadState::Active);
        assert!(
            find_active_thread_entry(&repo, "iso").unwrap().is_some(),
            "the retry completes the interrupted reservation exactly-once"
        );
    }

    /// cid 3333881561: the manifest rollback must restore the prior manifest
    /// snapshot (a stale manifest from a reused thread ref), not blind-delete.
    #[test]
    fn restore_thread_manifest_restores_prior_and_removes_when_absent() {
        let (temp, repo, _state) = repo_with_state(&[]);
        let _ = &temp;
        let heddle_dir = repo.heddle_dir();

        // Prior = Some: an OLD manifest existed. The forward overwrote it; the
        // inverse must restore the OLD bytes, not the forward's, and not delete.
        let path = repo::thread_manifest::manifest_path(heddle_dir, "foo");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"OLD").unwrap();
        let prior = std::fs::read(&path).ok();
        std::fs::write(&path, b"NEW").unwrap();
        repo.restore_thread_manifest("foo", prior).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"OLD",
            "rollback must restore the prior manifest snapshot, not delete it"
        );

        // Prior = None: no manifest existed. The inverse removes what we created.
        let path2 = repo::thread_manifest::manifest_path(heddle_dir, "bar");
        std::fs::create_dir_all(path2.parent().unwrap()).unwrap();
        std::fs::write(&path2, b"NEW").unwrap();
        repo.restore_thread_manifest("bar", None).unwrap();
        assert!(
            std::fs::symlink_metadata(&path2).is_err(),
            "rollback of a freshly-created manifest must remove it"
        );
    }

    /// cid 3333881583: the thread-ref rollback is CAS-guarded — it must NOT
    /// clobber a ref a concurrent process advanced past this transaction's
    /// forward value, but must still undo a ref that still holds it.
    #[test]
    fn cas_guarded_ref_rollback_does_not_clobber_concurrent_advance() {
        let (temp, repo, base) = repo_with_state(&[]);
        let _ = &temp;
        let name = ThreadName::new("foo");
        let advanced = ChangeId::generate();
        let prior = ChangeId::generate();

        // Brand-new case (restore_to = None → would otherwise delete). A
        // concurrent writer advanced the ref past our forward value → leave it.
        repo.refs().set_thread(&name, &advanced).unwrap();
        repo.cas_guarded_thread_ref_rollback(&name, base, None)
            .unwrap();
        assert_eq!(
            repo.refs().get_thread(&name).unwrap(),
            Some(advanced),
            "rollback must not delete a ref a concurrent process advanced"
        );

        // Brand-new case, ref still holds our forward value → delete it.
        repo.refs().set_thread(&name, &base).unwrap();
        repo.cas_guarded_thread_ref_rollback(&name, base, None)
            .unwrap();
        assert_eq!(
            repo.refs().get_thread(&name).unwrap(),
            None,
            "rollback must delete a ref still holding our forward value"
        );

        // Re-start case (restore_to = Some(prior)). Concurrent advance → leave.
        repo.refs().set_thread(&name, &advanced).unwrap();
        repo.cas_guarded_thread_ref_rollback(&name, base, Some(prior))
            .unwrap();
        assert_eq!(
            repo.refs().get_thread(&name).unwrap(),
            Some(advanced),
            "rollback must not reset a ref a concurrent process advanced"
        );

        // Re-start case, ref still holds our forward value → restore prior.
        repo.refs().set_thread(&name, &base).unwrap();
        repo.cas_guarded_thread_ref_rollback(&name, base, Some(prior))
            .unwrap();
        assert_eq!(
            repo.refs().get_thread(&name).unwrap(),
            Some(prior),
            "rollback must restore the prior value when the ref still holds our forward value"
        );
    }

    // ---- heddle#356 r3 fixes (updated to the TargetDir claim — r4) ----

    /// cid 3335052857: target-dir ownership is re-established atomically at
    /// creation, so rollback never deletes a directory a concurrent process
    /// created between `plan_worktree_target` time and the transaction. Here the
    /// plan saw the target absent (`plan_created = true`) but a concurrent
    /// process created a REAL EMPTY dir first — `create_target_dir` adopts it
    /// (does NOT claim it as created), and the claim-keyed rewind leaves it intact.
    #[test]
    fn target_dir_rollback_leaves_concurrently_created_dir_intact() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("iso");

        // The concurrent creation that races our forward (a real empty dir).
        std::fs::create_dir(&target).unwrap();
        let claim = create_target_dir(&target, true).unwrap();
        assert_eq!(
            claim.kind(),
            TargetDirClaimKind::AdoptedEmpty,
            "a real empty dir a concurrent process created is ADOPTED, not claimed as created \
             (a stale plan-time bool would have claimed it)"
        );
        // The rewind keys on the runtime claim, so it must spare their dir.
        remove_self_created_dir(&target, Some(claim.clone())).unwrap();
        rewind_checkout(&target, Some(claim)).unwrap();
        assert!(
            target.is_dir(),
            "rollback must not delete a directory this start did not create"
        );
    }

    /// cid 3335052857: the genuinely-created case still rewinds — a target absent
    /// at the forward is owned by this start and removed wholesale on rollback.
    #[test]
    fn target_dir_rollback_removes_genuinely_created_dir() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("nested").join("iso");

        let claim = create_target_dir(&target, true).unwrap();
        assert_eq!(
            claim.kind(),
            TargetDirClaimKind::Created,
            "an absent target this start creates is owned by us"
        );
        assert!(
            target.is_dir(),
            "the forward must create the leaf (and its parents)"
        );
        remove_self_created_dir(&target, Some(claim)).unwrap();
        assert!(
            std::fs::symlink_metadata(&target).is_err(),
            "rollback must remove a directory this start genuinely created"
        );
    }

    /// cid 3335052857: a pre-existing user `--path` dir (`plan_created = false`)
    /// is adopted (never created or removed by us); the claim is `AdoptedEmpty`.
    #[test]
    fn target_dir_user_supplied_dir_is_never_removed() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("iso");
        std::fs::create_dir(&target).unwrap();

        let claim = create_target_dir(&target, false).unwrap();
        assert_eq!(
            claim.kind(),
            TargetDirClaimKind::AdoptedEmpty,
            "a user-supplied pre-existing empty dir is adopted, not ours to own/remove"
        );
        remove_self_created_dir(&target, Some(claim)).unwrap();
        assert!(target.is_dir(), "a user-supplied dir must survive rollback");
    }

    // ---- heddle#356 r5 close-the-class: write-time target-dir identity (cid 3336120590) ----

    /// cid 3336120590: after the target claim is established, a concurrent
    /// process swaps the leaf for a symlink to an outside dir before checkout
    /// materialization. The writer must refuse from the claim's handle identity
    /// check (not follow the symlink), and rollback must clear/delete only
    /// through the retained handle, never through the substituted path.
    #[cfg(unix)]
    #[test]
    fn claimed_target_swap_refuses_write_and_spares_symlink_target() {
        let (temp, repo, state) = repo_with_state(&[]);
        let checkout = temp.path().join("iso");
        let victim = temp.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("precious.txt"), b"precious").unwrap();

        let claim = create_target_dir(&checkout, true).unwrap();
        assert_eq!(claim.kind(), TargetDirClaimKind::Created);

        std::fs::remove_dir(&checkout).unwrap();
        std::os::unix::fs::symlink(&victim, &checkout).unwrap();

        let checkout_root = claimed_worktree_path(Some(claim.clone()), &checkout);
        assert!(
            checkout_root.is_err(),
            "a post-claim leaf swap must refuse before any checkout write"
        );

        rewind_checkout(&checkout, Some(claim.clone())).unwrap();
        remove_self_created_dir(&checkout, Some(claim)).unwrap();

        assert!(
            std::fs::symlink_metadata(&checkout)
                .unwrap()
                .file_type()
                .is_symlink(),
            "rollback must not remove the substituted symlink"
        );
        assert!(
            !victim.join("a.txt").exists() && !victim.join(".heddle").exists(),
            "checkout materialization must not write into the symlink target"
        );
        assert_eq!(
            std::fs::read(victim.join("precious.txt")).unwrap(),
            b"precious",
            "rollback must not clear the symlink target's existing data"
        );

        let _ = (repo, state);
    }

    #[test]
    fn claimed_created_and_adopted_dirs_still_write_and_rewind() {
        let (temp, repo, state) = repo_with_state(&[]);

        let created = temp.path().join("created");
        let created_claim = create_target_dir(&created, true).unwrap();
        let created_root = claimed_worktree_path(Some(created_claim.clone()), &created).unwrap();
        write_isolated_checkout(&repo, &created_root, &state, Some("created")).unwrap();
        assert!(
            created.join("a.txt").is_file(),
            "created dir still receives checkout bytes"
        );
        rewind_checkout(&created, Some(created_claim)).unwrap();
        assert!(
            std::fs::symlink_metadata(&created).is_err(),
            "created dir is removed on rollback"
        );

        let adopted = temp.path().join("adopted");
        std::fs::create_dir(&adopted).unwrap();
        let adopted_claim = create_target_dir(&adopted, false).unwrap();
        let adopted_root = claimed_worktree_path(Some(adopted_claim.clone()), &adopted).unwrap();
        write_isolated_checkout(&repo, &adopted_root, &state, Some("adopted")).unwrap();
        assert!(
            adopted.join("a.txt").is_file(),
            "adopted dir still receives checkout bytes"
        );
        rewind_checkout(&adopted, Some(adopted_claim)).unwrap();
        assert!(adopted.is_dir(), "adopted dir itself survives rollback");
        let remaining: Vec<_> = std::fs::read_dir(&adopted)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(
            remaining.is_empty(),
            "adopted dir contents are cleared: {remaining:?}"
        );
    }

    // ---- heddle#356 r4 close-the-class: target-dir SHAPE validation (cid 3335586962) ----

    /// cid 3335586962 (P1, the new sibling): `plan_created = true` but a
    /// concurrent process dropped a SYMLINK-to-a-dir at the leaf between plan time
    /// and the transaction. `create_target_dir` must REFUSE (not return a
    /// "not-ours" value the checkout writer proceeds on), and because the forward
    /// refuses, the claim is never established (`None`) so rollback NEVER clears
    /// or deletes through the symlink — the symlink target's contents survive.
    #[test]
    fn create_target_dir_refuses_symlink_leaf_and_spares_target_contents() {
        let temp = TempDir::new().unwrap();
        // The symlink target with precious contents that MUST survive.
        let victim = temp.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("precious.txt"), b"precious").unwrap();
        // A concurrent process dropped a symlink-to-dir at the leaf.
        let leaf = temp.path().join("iso");
        std::os::unix::fs::symlink(&victim, &leaf).unwrap();

        // The plan saw the leaf absent (`plan_created = true`); the claim must refuse.
        let claim = create_target_dir(&leaf, true);
        assert!(
            claim.is_err(),
            "a symlink at the leaf must REFUSE the start, never be adopted/written through"
        );

        // The forward refused, so no claim was established → rollback (driven by a
        // `None` cell) must touch nothing.
        rewind_checkout(&leaf, None).unwrap();
        remove_self_created_dir(&leaf, None).unwrap();
        assert!(
            std::fs::symlink_metadata(&leaf)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink itself must be left untouched"
        );
        assert!(
            victim.join("precious.txt").is_file(),
            "rollback must NOT delete through the symlink into its target's contents"
        );
        assert_eq!(
            std::fs::read(victim.join("precious.txt")).unwrap(),
            b"precious",
            "the symlink target's data must survive intact"
        );
    }

    /// cid 3335586962: a plain FILE (not a dir) occupying the leaf is likewise
    /// refused — the checkout is never written over a non-directory object.
    #[test]
    fn create_target_dir_refuses_non_directory_leaf() {
        let temp = TempDir::new().unwrap();
        let leaf = temp.path().join("iso");
        std::fs::write(&leaf, b"i am a file, not a dir").unwrap();

        let claim = create_target_dir(&leaf, true);
        assert!(claim.is_err(), "a non-directory leaf must refuse the start");
        assert!(
            leaf.is_file(),
            "the pre-existing file must be left untouched"
        );
        assert_eq!(std::fs::read(&leaf).unwrap(), b"i am a file, not a dir");
    }

    /// cid 3335586962: a NON-EMPTY directory at the leaf is refused — adopting it
    /// would let rollback `clear_dir_contents` someone else's populated dir.
    #[test]
    fn create_target_dir_refuses_non_empty_dir_leaf() {
        let temp = TempDir::new().unwrap();
        let leaf = temp.path().join("iso");
        std::fs::create_dir(&leaf).unwrap();
        std::fs::write(leaf.join("someone-elses-work.txt"), b"keep me").unwrap();

        let claim = create_target_dir(&leaf, true);
        assert!(
            claim.is_err(),
            "a non-empty dir at the leaf must refuse the start"
        );
        assert!(
            leaf.join("someone-elses-work.txt").is_file(),
            "the pre-existing contents must be left untouched"
        );
    }
}
