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
//!   * **The oplog `ThreadCreateV2`** is the staged domain record handed to the
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

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use objects::error::{HeddleError, Result as HeddleResult};
use objects::object::{ChangeId, ThreadName};
use oplog::OpRecord;
use refs::RefExpectation;
use repo::{
    Repository, Thread, ThreadManager, ThreadMode,
    atomic::{AtomicMutation, StagedCommit, Tx},
};

use super::mount_lifecycle::{self, MountOwnership};
use super::worktree_cmd::{
    helpers::write_isolated_checkout,
    hydrate,
    shared_target::write_cargo_config,
};

/// Wrap an `anyhow` error from a materialize/hydrate helper into the
/// `HeddleError` the primitive's `Result` requires (mirrors undo/redo's
/// `apply_error`). The command-level preflights produce the structured
/// refusals; a message wrapped here only ever surfaces a genuinely
/// unexpected mid-apply failure whose rewind has already run.
fn apply_error(err: anyhow::Error) -> HeddleError {
    HeddleError::Conflict(format!("{err:#}"))
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

/// What the target-dir claim ([`create_target_dir`]) established about the
/// worktree leaf, and how the two directory rewinds may treat it. This is the
/// SINGLE determination both the checkout writer (implicitly — a refusal aborts
/// the transaction before the writer's step runs) and BOTH rewinds key on, so
/// nothing writes through, clears, or deletes a path that was not established as
/// a real, empty directory we own (heddle#356 cid 3335586962 / 3335052857).
///
/// A leaf that is anything else — a symlink, a non-directory file, or a
/// non-empty directory — is NOT representable here: `create_target_dir` refuses
/// the start with an `Err` instead, so there is no "owned" value that can stand
/// for a foreign object.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TargetDir {
    /// THIS invocation created the leaf as a fresh, real, empty directory →
    /// rollback removes it wholesale (restoring "didn't exist"). Its dep symlinks
    /// go with it: `remove_dir_all` unlinks symlinks without following them.
    Created,
    /// A real, EMPTY directory already existed at the leaf that we may safely
    /// write into — a user-supplied `--path`, or a concurrent process that
    /// created a *real empty dir* between plan time and the transaction. The
    /// checkout writes into it; rollback clears ONLY the contents we wrote and
    /// never removes the directory itself (it is not ours to delete).
    AdoptedEmpty,
}

/// Validate that `abs_path` is, on disk RIGHT NOW, a real (non-symlink) EMPTY
/// directory we may write the isolated checkout into and clear-on-rollback —
/// refuse the start otherwise. Uses `symlink_metadata` so a symlink leaf is
/// detected AS a symlink and never followed: writing the checkout through it (or
/// `clear_dir_contents`-ing it on rollback) would mutate/delete data in the
/// symlink's target (heddle#356 cid 3335586962, data loss).
fn adopt_existing_empty_dir(abs_path: &Path) -> HeddleResult<TargetDir> {
    let meta = std::fs::symlink_metadata(abs_path).map_err(HeddleError::from)?;
    if meta.file_type().is_symlink() {
        return Err(target_dir_shape_refusal(abs_path, "is a symlink"));
    }
    if !meta.is_dir() {
        return Err(target_dir_shape_refusal(abs_path, "is not a directory"));
    }
    if std::fs::read_dir(abs_path)
        .map_err(HeddleError::from)?
        .next()
        .transpose()
        .map_err(HeddleError::from)?
        .is_some()
    {
        return Err(target_dir_shape_refusal(abs_path, "is not empty"));
    }
    Ok(TargetDir::AdoptedEmpty)
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

/// The precise checkout-rewind inverse, keyed on the runtime [`TargetDir`] claim
/// from [`create_target_dir`] (NOT a stale plan-time bool — cid 3335052857 /
/// 3335586962):
///   * [`TargetDir::Created`] → this invocation made the worktree directory, so
///     remove it entirely (restoring "didn't exist"); its dep symlinks go with
///     it, since `remove_dir_all` unlinks symlinks without following them, so the
///     origin's deps are never touched.
///   * [`TargetDir::AdoptedEmpty`] → we adopted a real, empty pre-existing dir
///     (user `--path`, or a concurrent real-empty-dir) — not ours to delete, so
///     preserve the directory and clear only the contents we materialized inside.
///   * `None` → the claim never established an owned empty dir here (the forward
///     refused on a symlink/foreign object, or never ran). Touch NOTHING — the
///     whole point of the close-the-class fix is that rollback never clears or
///     deletes through a path it did not itself establish as a real empty dir.
///
/// Tolerant of an already-absent target so it composes with other rewind steps.
fn rewind_checkout(abs_path: &Path, claim: Option<TargetDir>) -> HeddleResult<()> {
    let result = match claim {
        Some(TargetDir::Created) => std::fs::remove_dir_all(abs_path),
        Some(TargetDir::AdoptedEmpty) => clear_dir_contents(abs_path),
        None => return Ok(()),
    };
    match result {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(HeddleError::from(err)),
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
        return adopt_existing_empty_dir(abs_path);
    }
    if let Some(parent) = abs_path.parent() {
        std::fs::create_dir_all(parent).map_err(HeddleError::from)?;
    }
    match std::fs::create_dir(abs_path) {
        Ok(()) => Ok(TargetDir::Created),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            adopt_existing_empty_dir(abs_path)
        }
        Err(err) => Err(HeddleError::from(err)),
    }
}

/// Inverse of the target-dir creation step: remove the worktree directory ONLY
/// when this invocation created it ([`TargetDir::Created`], restoring "didn't
/// exist"). `claim` is the runtime [`TargetDir`] signal from
/// [`create_target_dir`], NOT a stale plan-time bool — an adopted (concurrent or
/// user-supplied) dir, or a leaf this step never established (`None`), is never
/// removed here. Tolerant of an already-absent dir so it composes after the
/// checkout rewind (which removes a self-created dir first; this then no-ops on
/// `NotFound`).
fn remove_self_created_dir(abs_path: &Path, claim: Option<TargetDir>) -> HeddleResult<()> {
    if claim != Some(TargetDir::Created) {
        return Ok(());
    }
    match std::fs::remove_dir_all(abs_path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(HeddleError::from(err)),
    }
}

/// CAS-guarded rollback of the thread-ref forward (heddle#356 cid 3333881583).
///
/// The forward set the ref to `set_value` (the start's base state). Undo it
/// ONLY if the ref STILL points there: restore the prior value, or delete a ref
/// this start created. If a concurrent process advanced/changed the ref after
/// our forward (a concurrent start or crash-recovery), leave their write in
/// place — an unconditional reset/delete would clobber it.
fn cas_guarded_ref_rollback(
    repo: &Repository,
    name: &ThreadName,
    set_value: ChangeId,
    restore_to: Option<ChangeId>,
) -> HeddleResult<()> {
    // Compare-before-write: bail without touching the ref if it no longer holds
    // the value our forward set.
    if repo.refs().get_thread(name)? != Some(set_value) {
        return Ok(());
    }
    let result = match restore_to {
        Some(prior) => repo
            .refs()
            .set_thread_cas(name, RefExpectation::Value(set_value), &prior),
        None => repo
            .refs()
            .delete_thread_cas(name, RefExpectation::Value(set_value)),
    };
    match result {
        Ok(()) => Ok(()),
        // Lost the race between the read above and this CAS: a concurrent writer
        // advanced the ref. The expectation guard means we wrote nothing — leave
        // their advance intact (the whole point of the guard).
        Err(HeddleError::Conflict(_)) => Ok(()),
        Err(other) => Err(other),
    }
}

/// Restore the thread manifest sidecar to its captured pre-start snapshot:
/// rewrite the prior `manifest.toml` bytes if one existed, or remove the
/// directory this start created. Restoring (not blind-deleting) preserves an
/// OLD manifest left by a prior materialization of a reused thread ref
/// (heddle#356 cid 3333881561).
fn restore_thread_manifest(
    heddle_dir: &Path,
    thread: &str,
    prior: Option<Vec<u8>>,
) -> HeddleResult<()> {
    match prior {
        Some(bytes) => {
            let path = repo::thread_manifest::manifest_path(heddle_dir, thread);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(HeddleError::from)?;
            }
            std::fs::write(&path, bytes).map_err(HeddleError::from)
        }
        None => repo::thread_manifest::remove_thread_manifest_dir(heddle_dir, thread)
            .map(|_| ())
            .map_err(HeddleError::from),
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
    /// + a staged `ThreadCreateV2` commit record).
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
            move || remove_self_created_dir(&rewind_abs, target_claim.get()),
        )
    }

    /// Stage the thread-ref write (atomic, forward-first), pushing the staged
    /// `ThreadCreateV2` commit record for a brand-new thread.
    fn stage_ref(&self, tx: &mut Tx<'_>, oplog: &mut Vec<OpRecord>) -> HeddleResult<()> {
        let repo = tx.repo();
        let base_state = self.base_state;
        match self.existing_thread_state {
            Some(existing) => {
                let fwd_name = ThreadName::new(&self.name);
                let inv_name = ThreadName::new(&self.name);
                tx.step(
                    move || repo.refs().set_thread_cas(&fwd_name, RefExpectation::Value(existing), &base_state),
                    move || cas_guarded_ref_rollback(repo, &inv_name, base_state, Some(existing)),
                )?;
            }
            None => {
                let fwd_name = ThreadName::new(&self.name);
                let inv_name = ThreadName::new(&self.name);
                tx.step(
                    move || repo.refs().set_thread_cas(&fwd_name, RefExpectation::Missing, &base_state),
                    move || cas_guarded_ref_rollback(repo, &inv_name, base_state, None),
                )?;
                // The domain commit record. `manager_snapshot = None` matches the
                // pre-migration `cmd_start` (the record is written below, so
                // there is nothing to snapshot yet — heddle#23 r2).
                oplog.push(OpRecord::ThreadCreateV2 {
                    name: self.name.clone(),
                    state: base_state,
                    manager_snapshot: None,
                });
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
    ) -> HeddleResult<()> {
        let repo = tx.repo();
        let abs = self.abs_path.clone();
        let rewind_abs = self.abs_path.clone();
        let base_state = self.base_state;
        let name = self.name.clone();
        // The [`TargetDir`] claim set by `stage_target_dir`'s forward (which ran
        // first), read at rewind time when it is settled — never the stale
        // plan-time bool. An adopted dir is cleared (not deleted), a self-created
        // dir is removed wholesale, and a refused/unestablished leaf (`None`) is
        // left untouched (cid 3335052857 / 3335586962).
        tx.step_nonatomic(
            move || Ok(()),
            move |()| rewind_checkout(&rewind_abs, target_claim.get()),
            move || {
                #[cfg(test)]
                if materialize_fault_trips() {
                    return Err(HeddleError::Conflict(
                        "injected materialize fault".to_string(),
                    ));
                }
                write_isolated_checkout(repo, &abs, &base_state, Some(&name)).map_err(apply_error)
            },
        )
    }

    /// Write the materialized-thread manifest sidecar (it lives under
    /// `.heddle/threads/<name>/`, OUTSIDE the checkout, so it needs its own
    /// inverse — the checkout rewind won't reach it).
    fn stage_manifest(&self, tx: &mut Tx<'_>) -> HeddleResult<()> {
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
            move |prior| restore_thread_manifest(repo.heddle_dir(), &inv_name, prior),
            move || {
                repo.record_thread_manifest(&fwd_name, &base_state, &abs)
                    .map(|_| ())
            },
        )
    }

    /// Apply the `--shared-target` cargo-config redirect (inside the checkout),
    /// returning whether it landed (a pre-staged `.cargo/config.toml` is left
    /// untouched). Capture-restore on the config file so a later failure
    /// restores the pre-write state precisely even though the checkout rewind
    /// would also reach it.
    fn stage_cargo_config(&self, tx: &mut Tx<'_>, dir: &Path) -> HeddleResult<bool> {
        let cfg = self.abs_path.join(".cargo").join("config.toml");
        let restore_cfg = cfg.clone();
        let abs = self.abs_path.clone();
        let dir = dir.to_path_buf();
        tx.step_nonatomic(
            move || Ok(std::fs::read(&cfg).ok()),
            move |prior| match prior {
                Some(bytes) => std::fs::write(&restore_cfg, bytes).map_err(HeddleError::from),
                None => match std::fs::remove_file(&restore_cfg) {
                    Ok(()) => Ok(()),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(err) => Err(HeddleError::from(err)),
                },
            },
            move || write_cargo_config(&abs, &dir).map_err(apply_error),
        )
    }

    /// `--hydrate`: symlink the origin's top-level ignored dependency dirs into
    /// the checkout. Each link is one forward-first `Tx::step` (a single
    /// all-or-nothing `symlink`), so its unlink inverse is registered only after
    /// the link is created and a partial hydrate unwinds every created link.
    /// Returns the names linked (for the post-commit note).
    fn stage_hydrate(&self, tx: &mut Tx<'_>) -> HeddleResult<Vec<String>> {
        let repo = tx.repo();
        let sources = hydrate::hydratable_ignored_dirs(repo).map_err(apply_error)?;
        let mut linked: Vec<String> = Vec::new();
        for source in &sources {
            let Some((dest, link_name)) = hydrate::plan_link(&self.abs_path, source) else {
                continue;
            };
            let src = source.clone();
            let dest_fwd = dest.clone();
            let inv_checkout = self.abs_path.clone();
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
            let exclude_path = hydrate::hydrate_exclude_path(&self.abs_path);
            let restore_path = exclude_path.clone();
            let abs = self.abs_path.clone();
            let linked_fwd = linked.clone();
            tx.step_nonatomic(
                move || Ok(std::fs::read(&exclude_path).ok()),
                move |prior| restore_ignore_file(&restore_path, prior),
                move || hydrate::preserve_hydrated_ignores(&abs, &linked_fwd).map_err(apply_error),
            )?;
        }
        Ok(linked)
    }

    /// Establish the FUSE mount for a virtualized thread, registering an unmount
    /// inverse so an outer failure tears the mount down (it would otherwise
    /// outlive the failed start — the daemon owns it across process exit).
    fn stage_mount(&self, tx: &mut Tx<'_>) -> HeddleResult<()> {
        let repo = tx.repo();
        let root = repo.root().to_path_buf();
        let abs = self.abs_path.clone();
        let fwd_name = self.name.clone();
        let inv_name = self.name.clone();
        let ownership = self.mount_ownership;
        tx.step(
            move || {
                mount_lifecycle::establish_virtualized_mount(&root, &fwd_name, &abs, ownership)
                    .map_err(apply_error)
            },
            move || {
                mount_lifecycle::unmount_thread_if_mounted(&inv_name);
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
    type Output = Vec<String>;

    fn transaction_id(&self) -> String {
        self.transaction_id.clone()
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<Vec<String>>> {
        let mut oplog: Vec<OpRecord> = Vec::new();

        // The single [`TargetDir`] claim, decided atomically by
        // `stage_target_dir`'s forward and consumed by BOTH directory rewinds
        // (the target-dir inverse and the checkout rewind). It starts `None` — an
        // unestablished claim — so if the forward refuses on a symlink/foreign
        // object (or never runs), neither rewind clears or deletes through the
        // leaf (heddle#356 cid 3335052857 / 3335586962).
        let target_claim = Rc::new(Cell::new(None));

        // 0. Create the target dir inside the transaction (first, so its removal
        //    inverse runs last and a self-created dir never leaks).
        self.stage_target_dir(tx, Rc::clone(&target_claim))?;

        // 1. Thread ref (+ staged ThreadCreateV2 for a brand-new thread).
        self.stage_ref(tx, &mut oplog)?;

        // 2. Mode-specific materialization.
        let linked = match self.thread_mode {
            ThreadMode::Solid | ThreadMode::Materialized => {
                self.stage_checkout(tx, Rc::clone(&target_claim))?;
                if matches!(self.thread_mode, ThreadMode::Materialized) {
                    self.stage_manifest(tx)?;
                }
                if let Some(dir) = self.shared_target_dir.clone() {
                    let applied = self.stage_cargo_config(tx, &dir)?;
                    // The writer no-ops on a pre-staged config; don't advertise a
                    // redirect that isn't in effect (`thread show` would lie).
                    if !applied {
                        self.record.shared_target_dir = None;
                    }
                }
                if self.hydrate {
                    self.stage_hydrate(tx)?
                } else {
                    Vec::new()
                }
            }
            ThreadMode::Virtualized => {
                self.stage_mount(tx)?;
                Vec::new()
            }
        };

        // 3. Thread record (sole record under the name), via converge_records.
        self.stage_record(tx)?;

        Ok(StagedCommit::new(linked, oplog))
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
    use super::super::thread::{
        find_active_thread_entry, resolve_start_epoch, start_thread, start_transaction_id,
    };
    use super::super::thread_cmd::drop_thread_silent;
    use super::super::worktree_cmd::helpers::plan_worktree_target;
    use super::*;
    use crate::cli::{ThreadStartArgs, WorkspaceModeArg};
    use repo::Repository;
    use tempfile::TempDir;

    /// A `--path` solid-thread start that pins its base on `from` (no current-
    /// state bootstrap) and never spawns a daemon — minimal machinery for the
    /// in-process rollback assertions.
    fn solid_args(name: &str, path: &std::path::Path, from: &ChangeId, hydrate: bool) -> ThreadStartArgs {
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
            let ignore = deps
                .iter()
                .map(|d| format!("{d}/\n"))
                .collect::<String>();
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
        assert!(out.is_ok(), "happy-path start should succeed: {:?}", out.err());

        assert!(checkout.join(".heddle").is_dir(), "checkout .heddle should exist");
        assert!(checkout.join("a.txt").is_file(), "tracked file should materialize");
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
        assert!(has_thread_record(&repo, "iso"), "thread record should be persisted");
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
        assert!(!has_thread_ref(&repo, "iso"), "the thread ref must be rolled back");
        assert!(!has_thread_record(&repo, "iso"), "no record must survive the rollback");
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
        assert!(checkout.is_dir(), "a pre-existing user dir must not be deleted");
        let remaining: Vec<_> = std::fs::read_dir(&checkout)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(remaining.is_empty(), "rollback must clear materialized contents: {remaining:?}");
        assert!(!has_thread_ref(&repo, "iso"), "the thread ref must be rolled back");
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
        assert!(!has_thread_ref(&repo, "iso"), "the thread ref must be rolled back");
        // The origin's dep dirs are untouched (we unlink, never follow).
        assert!(temp.path().join("dep_a").is_dir());
        assert!(temp.path().join("dep_b").is_dir());
    }

    // ---- heddle#356 r2 fixes ----

    /// cid 3333881552: the target dir must be created INSIDE the transaction.
    /// `plan_worktree_target` resolves + validates but defers creation, so a
    /// failure in the remaining pre-transaction work can't orphan a directory.
    #[test]
    fn plan_worktree_target_defers_dir_creation() {
        let (temp, repo, _state) = repo_with_state(&[]);
        let target = temp.path().join("iso-deferred");
        let prepared = plan_worktree_target(&repo, &target).unwrap();
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
            !repo.oplog().committed_batch_records(&retry_id).unwrap().is_empty(),
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
        assert!(checkout.join(".heddle").is_dir(), "the first start materialized the checkout");

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
        assert!(checkout.join(".heddle").is_dir(), "the committed checkout survives the retry");
        assert!(checkout.join("a.txt").is_file(), "the committed tree survives the retry");

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
        let temp = TempDir::new().unwrap();
        let heddle_dir = temp.path();

        // Prior = Some: an OLD manifest existed. The forward overwrote it; the
        // inverse must restore the OLD bytes, not the forward's, and not delete.
        let path = repo::thread_manifest::manifest_path(heddle_dir, "foo");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"OLD").unwrap();
        let prior = std::fs::read(&path).ok();
        std::fs::write(&path, b"NEW").unwrap();
        restore_thread_manifest(heddle_dir, "foo", prior).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"OLD",
            "rollback must restore the prior manifest snapshot, not delete it"
        );

        // Prior = None: no manifest existed. The inverse removes what we created.
        let path2 = repo::thread_manifest::manifest_path(heddle_dir, "bar");
        std::fs::create_dir_all(path2.parent().unwrap()).unwrap();
        std::fs::write(&path2, b"NEW").unwrap();
        restore_thread_manifest(heddle_dir, "bar", None).unwrap();
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
        cas_guarded_ref_rollback(&repo, &name, base, None).unwrap();
        assert_eq!(
            repo.refs().get_thread(&name).unwrap(),
            Some(advanced),
            "rollback must not delete a ref a concurrent process advanced"
        );

        // Brand-new case, ref still holds our forward value → delete it.
        repo.refs().set_thread(&name, &base).unwrap();
        cas_guarded_ref_rollback(&repo, &name, base, None).unwrap();
        assert_eq!(
            repo.refs().get_thread(&name).unwrap(),
            None,
            "rollback must delete a ref still holding our forward value"
        );

        // Re-start case (restore_to = Some(prior)). Concurrent advance → leave.
        repo.refs().set_thread(&name, &advanced).unwrap();
        cas_guarded_ref_rollback(&repo, &name, base, Some(prior)).unwrap();
        assert_eq!(
            repo.refs().get_thread(&name).unwrap(),
            Some(advanced),
            "rollback must not reset a ref a concurrent process advanced"
        );

        // Re-start case, ref still holds our forward value → restore prior.
        repo.refs().set_thread(&name, &base).unwrap();
        cas_guarded_ref_rollback(&repo, &name, base, Some(prior)).unwrap();
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
            claim,
            TargetDir::AdoptedEmpty,
            "a real empty dir a concurrent process created is ADOPTED, not claimed as created \
             (a stale plan-time bool would have claimed it)"
        );
        // The rewind keys on the runtime claim, so it must spare their dir.
        remove_self_created_dir(&target, Some(claim)).unwrap();
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
        assert_eq!(claim, TargetDir::Created, "an absent target this start creates is owned by us");
        assert!(target.is_dir(), "the forward must create the leaf (and its parents)");
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
            claim,
            TargetDir::AdoptedEmpty,
            "a user-supplied pre-existing empty dir is adopted, not ours to own/remove"
        );
        remove_self_created_dir(&target, Some(claim)).unwrap();
        assert!(target.is_dir(), "a user-supplied dir must survive rollback");
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
            std::fs::symlink_metadata(&leaf).unwrap().file_type().is_symlink(),
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
        assert!(leaf.is_file(), "the pre-existing file must be left untouched");
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
        assert!(claim.is_err(), "a non-empty dir at the leaf must refuse the start");
        assert!(
            leaf.join("someone-elses-work.txt").is_file(),
            "the pre-existing contents must be left untouched"
        );
    }
}
