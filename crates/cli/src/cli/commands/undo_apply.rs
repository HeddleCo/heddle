// SPDX-License-Identifier: Apache-2.0
//! Apply undo/redo operations to the repository.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};
use objects::error::{HeddleError, Result as HeddleResult};
use objects::lock::{RepoLock, WriteLockGuard};
use objects::object::{ChangeId, ContentHash, MarkerName, ThreadName};
use oplog::{IsolationKey, OpBatch, OpEntry, OpLogBackend, OpRecord, isolation_keys_for_record};
use refs::Head;
use repo::{
    CommitGraphIndex, Repository, Thread, ThreadFreshness, ThreadIntegrationPolicy, ThreadManager,
    ThreadState, VisibilitySidecarRestore,
    atomic::{AtomicMutation, DeferredMutation, StagedCommit, Tx},
    refresh_thread_freshness,
};
use sley::{
    DeleteRef, FullName, GitObjectType, GitTime, IndexWriteOptions, ObjectId, RefPrecondition,
    ReferenceTarget, Repository as SleyRepository, Signature,
    plumbing::sley_core::ByteString as GitByteString,
};

use super::{advice::RecoveryAdvice, thread_cmd::thread_not_found_advice};
use crate::bridge::git_core::{open_repo as open_git_repo, set_reference};

pub(super) fn preflight_undo_batches(repo: &Repository, batches: &[OpBatch]) -> Result<()> {
    if !batches_have_git_checkpoint(batches) {
        return Ok(());
    }
    let mut simulated_git_head = current_git_head(repo)?;
    for batch in batches {
        for entry in batch.entries.iter().rev() {
            if let OpRecord::GitCheckpoint {
                new_git_oid,
                previous_git_oid,
                ..
            } = &entry.operation
            {
                ensure_simulated_git_head_is(
                    repo,
                    &simulated_git_head,
                    new_git_oid,
                    "undo git checkpoint",
                )?;
                if let Some(previous) = previous_git_oid {
                    simulated_git_head = previous.clone();
                }
            }
        }
    }
    ensure_git_worktree_clean(repo, "undo git checkpoint")?;
    Ok(())
}

pub(super) fn preflight_redo_batches(repo: &Repository, batches: &[OpBatch]) -> Result<()> {
    if !batches_have_git_checkpoint(batches) {
        return Ok(());
    }
    let mut simulated_git_head = current_git_head(repo)?;
    for batch in batches {
        for entry in &batch.entries {
            if let OpRecord::GitCheckpoint {
                previous_git_oid,
                new_git_oid,
                ..
            } = &entry.operation
            {
                if let Some(previous) = previous_git_oid {
                    ensure_simulated_git_head_is(
                        repo,
                        &simulated_git_head,
                        previous,
                        "redo git checkpoint",
                    )?;
                }
                simulated_git_head = new_git_oid.clone();
            }
        }
    }
    ensure_git_worktree_clean(repo, "redo git checkpoint")?;
    Ok(())
}

fn batches_have_git_checkpoint(batches: &[OpBatch]) -> bool {
    batches.iter().any(|batch| {
        batch
            .entries
            .iter()
            .any(|entry| matches!(&entry.operation, OpRecord::GitCheckpoint { .. }))
    })
}

fn current_git_head(repo: &Repository) -> Result<String> {
    let git = git_checkout_repo(repo)?;
    git.head()
        .map_err(|error| anyhow!("failed to inspect Git HEAD: {error}"))?
        .oid
        .map(|id| id.to_string())
        .ok_or_else(|| anyhow!("failed to inspect Git HEAD: HEAD is unborn"))
}

fn ensure_simulated_git_head_is(
    repo: &Repository,
    actual: &str,
    expected: &str,
    action: &str,
) -> Result<()> {
    if actual == expected {
        return Ok(());
    }
    Err(anyhow!(RecoveryAdvice::git_head_mismatch(
        action,
        actual,
        expected,
        repo.git_overlay_current_branch()?
            .unwrap_or_else(|| "HEAD".to_string()),
        git_dirty_paths(repo),
    )))
}

// ---- The per-effect undo/redo applier (heddle#355 r3/r4) ----
//
// `EntrySteps` owns the `&mut Tx` for the lifetime of one undo/redo apply and
// exposes the domain's reversible writes as NAMED per-effect operations
// (`goto`, `set_thread`, `save_thread_record`, `remove_redaction_sidecar`, …).
// Each wraps the correct ledger combinator ONCE, so the entry appliers read as
// `steps.set_thread(name, state)?` instead of inlining a forward/inverse closure
// pair at every call site. The domain knowledge (refs, git, thread records,
// redaction sidecars) lives HERE in the CLI/undo layer, never in the atomic
// primitive (`crates/repo/src/atomic/`).
//
// GRANULARITY. Every externally-visible write runs through its OWN combinator
// call, so a failure on the Nth write leaves the prior N-1 inverses on the
// rewind ledger and the rollback restores the EXACT pre-entry state. The two
// combinators guard opposite hazards:
//
//   * `step` (forward-first) — for a single all-or-nothing write. Registers its
//     capture-before inverse only AFTER the forward returns `Ok`, so a forward
//     that fails registers no inverse for an effect that did not happen (the
//     register-then-forward footgun, cid 3330867774 / 3330867775).
//   * `step_nonatomic` (capture-restore) — for a NON-atomic forward (several
//     internal writes, or a materialization that can fail partway): a `goto`
//     (worktree materialize + HEAD write), `ThreadManager::save` (record file +
//     workspace file), or the redaction-sidecar removal (rewrite/delete). It
//     captures the prior state and registers a restore-to-snapshot inverse
//     BEFORE running the forward, so a partially-applied (or applied-then-later-
//     failed) forward is still fully unwound. A plain `step` here would leak the
//     partial effect, because it registers nothing when the forward returns
//     `Err`.
//
// Inverses (rollback, best-effort) MAY touch more than one ref; only `step`
// forwards are held to the one-write rule. `step_nonatomic` forwards may be
// non-atomic by definition — that is the whole point of the second combinator.

#[cfg(test)]
thread_local! {
    /// Test seam: when armed with `Some(n)`, the (n+1)-th per-effect step within
    /// an entry application returns an injected error WITHOUT running its forward
    /// — modeling "the (n+1)-th visible write fails before it does anything", so
    /// the mid-ENTRY rollback path can be asserted. Counts across every `step` /
    /// `step_nonatomic` of the current entry; disarms on trip.
    static ENTRY_WRITE_FAULT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// Test seam for the NON-atomic forwards: when armed with `Some(n)`, the
    /// (n+1)-th `step_nonatomic` RUNS its forward (applying its possibly-partial
    /// effect) and THEN returns an injected error — modeling a composite forward
    /// that mutated state and then failed. The restore registered BEFORE the
    /// forward must unwind it. Counts only `step_nonatomic` calls; disarms on
    /// trip.
    static NONATOMIC_FORWARD_FAULT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Arm the trip-before mid-entry write-fault seam for the duration of `body`,
/// clearing it afterwards so it never leaks into another test on the same thread.
#[cfg(test)]
fn with_entry_write_fault<T>(skip_then_fail_at: usize, body: impl FnOnce() -> T) -> T {
    ENTRY_WRITE_FAULT.with(|f| f.set(Some(skip_then_fail_at)));
    let out = body();
    ENTRY_WRITE_FAULT.with(|f| f.set(None));
    out
}

/// Arm the non-atomic forward-partial-failure seam for the duration of `body`:
/// the `skip_then_fail_at`-th-after `step_nonatomic` forward runs (applying its
/// effect) then fails, exercising the restore-to-snapshot rollback the second
/// combinator exists for.
#[cfg(test)]
fn with_nonatomic_forward_fault<T>(skip_then_fail_at: usize, body: impl FnOnce() -> T) -> T {
    NONATOMIC_FORWARD_FAULT.with(|f| f.set(Some(skip_then_fail_at)));
    let out = body();
    NONATOMIC_FORWARD_FAULT.with(|f| f.set(None));
    out
}

/// Decrement-and-test one of the per-entry fault counters; `true` iff this call
/// is the armed trip point (the counter disarms on trip).
#[cfg(test)]
fn fault_counter_trips(
    cell: &'static std::thread::LocalKey<std::cell::Cell<Option<usize>>>,
) -> bool {
    cell.with(|f| match f.get() {
        Some(0) => {
            f.set(None);
            true
        }
        Some(n) => {
            f.set(Some(n - 1));
            false
        }
        None => false,
    })
}

/// Restore HEAD to a captured worktree `state` + ref attachment: re-materialize
/// the worktree (if a state was captured), then restore the exact `Head` ref.
fn restore_head(repo: &Repository, state: Option<ChangeId>, head_ref: &Head) -> HeddleResult<()> {
    if let Some(state) = state {
        repo.goto_without_record_discard_local(&state)?;
    }
    repo.refs().write_head(head_ref)
}

/// Owns the `&mut Tx` for one undo/redo apply and exposes the domain's reversible
/// writes as named per-effect operations. See the module comment above.
struct EntrySteps<'tx, 'a> {
    tx: &'tx mut Tx<'a>,
}

impl<'a> EntrySteps<'_, 'a> {
    fn new<'tx>(tx: &'tx mut Tx<'a>) -> EntrySteps<'tx, 'a> {
        EntrySteps { tx }
    }

    fn repo(&self) -> &'a Repository {
        self.tx.repo()
    }

    /// One reversible leaf write that is a single all-or-nothing operation:
    /// forward-first via [`Tx::step`]. Honors the [`ENTRY_WRITE_FAULT`] seam.
    fn step<T>(
        &mut self,
        forward: impl FnOnce() -> HeddleResult<T>,
        inverse: impl FnOnce() -> HeddleResult<()> + 'a,
    ) -> HeddleResult<T> {
        #[cfg(test)]
        if fault_counter_trips(&ENTRY_WRITE_FAULT) {
            return Err(HeddleError::Conflict(
                "injected mid-entry write fault".to_string(),
            ));
        }
        self.tx.step(forward, inverse)
    }

    /// One reversible write whose `forward` is NOT a single all-or-nothing
    /// operation: capture-restore via [`Tx::step_nonatomic`], which registers the
    /// restore-to-snapshot inverse BEFORE running the forward. Honors both the
    /// trip-before [`ENTRY_WRITE_FAULT`] seam (shared per-entry write counter) and
    /// the [`NONATOMIC_FORWARD_FAULT`] seam (run-forward-then-fail).
    fn step_nonatomic<T, S: 'a>(
        &mut self,
        capture: impl FnOnce() -> HeddleResult<S>,
        restore: impl FnOnce(S) -> HeddleResult<()> + 'a,
        forward: impl FnOnce() -> HeddleResult<T>,
    ) -> HeddleResult<T> {
        #[cfg(test)]
        if fault_counter_trips(&ENTRY_WRITE_FAULT) {
            return Err(HeddleError::Conflict(
                "injected mid-entry write fault".to_string(),
            ));
        }
        #[cfg(test)]
        if fault_counter_trips(&NONATOMIC_FORWARD_FAULT) {
            // Run the real forward (applying its possibly-partial effect), then
            // fail — the restore registered BEFORE the forward unwinds it.
            return self.tx.step_nonatomic(capture, restore, move || {
                forward()?;
                Err(HeddleError::Conflict(
                    "injected non-atomic forward fault".to_string(),
                ))
            });
        }
        self.tx.step_nonatomic(capture, restore, forward)
    }

    /// Navigate HEAD + worktree to `target`. NON-atomic: `goto_without_record`
    /// materializes the worktree and then re-detaches HEAD, so it can fail
    /// partway. The capture snapshots the FULL pre-step `Head` (worktree state
    /// AND ref attachment) and the restore re-materializes it, so a partial
    /// (or later-failed) goto is fully unwound.
    fn goto(&mut self, target: ChangeId) -> HeddleResult<()> {
        let repo = self.repo();
        self.step_nonatomic(
            move || Ok((repo.head()?, repo.head_ref()?)),
            move |(prev_state, prev_head_ref)| restore_head(repo, prev_state, &prev_head_ref),
            move || repo.goto_without_record_discard_local(&target),
        )
    }

    /// Re-materialize the currently attached thread at `target`, preserving its
    /// attached HEAD. No-op when undo/redo is replaying a different thread's ref.
    fn restore_active_thread_worktree(&mut self, name: &str, target: ChangeId) -> HeddleResult<()> {
        let repo = self.repo();
        let head_ref = repo.head_ref()?;
        let Head::Attached { thread } = &head_ref else {
            return Ok(());
        };
        if thread != name {
            return Ok(());
        }
        self.step_nonatomic(
            move || Ok((repo.head()?, repo.head_ref()?)),
            move |(prev_state, prev_head_ref)| restore_head(repo, prev_state, &prev_head_ref),
            move || repo.fast_forward_attached_without_record_discard_local(&target),
        )
    }

    /// Write HEAD to `head`; inverse restores the prior HEAD ref. A single ref
    /// write — genuinely all-or-nothing.
    fn write_head(&mut self, head: Head) -> HeddleResult<()> {
        let repo = self.repo();
        let prev = repo.head_ref()?;
        self.step(
            move || repo.refs().write_head(&head),
            move || repo.refs().write_head(&prev),
        )
    }

    /// Set thread ref `name` to `state`; inverse restores its prior value (or
    /// deletes it if it had none). A single ref write.
    fn set_thread(&mut self, name: &str, state: ChangeId) -> HeddleResult<()> {
        let repo = self.repo();
        let forward_name = ThreadName::new(name);
        let prev = repo.refs().get_thread(&forward_name)?;
        let restore_name = name.to_string();
        self.step(
            move || repo.refs().set_thread(&forward_name, &state),
            move || {
                let name = ThreadName::new(restore_name);
                match prev {
                    Some(prev) => repo.refs().set_thread(&name, &prev),
                    None => repo.refs().delete_thread(&name).map(|_| ()),
                }
            },
        )
    }

    /// Delete thread ref `name`; inverse restores its prior value. A single ref
    /// write.
    fn delete_thread(&mut self, name: &str) -> HeddleResult<()> {
        let repo = self.repo();
        let forward_name = ThreadName::new(name);
        let prev = repo.refs().get_thread(&forward_name)?;
        let restore_name = name.to_string();
        self.step(
            move || repo.refs().delete_thread(&forward_name).map(|_| ()),
            move || match prev {
                Some(prev) => repo
                    .refs()
                    .set_thread(&ThreadName::new(restore_name), &prev),
                None => Ok(()),
            },
        )
    }

    /// Create marker `name` at `state`; inverse restores its prior value (delete
    /// if it didn't exist). A single ref write — on a name collision the forward
    /// fails, so no inverse is registered and a pre-existing marker survives the
    /// rollback.
    fn create_marker(&mut self, name: &str, state: ChangeId) -> HeddleResult<()> {
        let repo = self.repo();
        let forward_name = MarkerName::new(name);
        let prev = repo.refs().get_marker(&forward_name)?;
        let restore_name = name.to_string();
        self.step(
            move || repo.refs().create_marker(&forward_name, &state),
            move || {
                let name = MarkerName::new(restore_name);
                match prev {
                    Some(prev) => repo.refs().create_marker(&name, &prev),
                    None => repo.refs().delete_marker(&name).map(|_| ()),
                }
            },
        )
    }

    /// Delete marker `name`; inverse recreates it at its prior value. A single
    /// ref write.
    fn delete_marker(&mut self, name: &str) -> HeddleResult<()> {
        let repo = self.repo();
        let forward_name = MarkerName::new(name);
        let prev = repo.refs().get_marker(&forward_name)?;
        let restore_name = name.to_string();
        self.step(
            move || repo.refs().delete_marker(&forward_name).map(|_| ()),
            move || match prev {
                Some(prev) => repo
                    .refs()
                    .create_marker(&MarkerName::new(restore_name), &prev),
                None => Ok(()),
            },
        )
    }

    /// The SOLE way the undo/redo applier mutates a thread's persisted record
    /// set: converge the records filed under `name` to exactly `desired`, as ONE
    /// `step_nonatomic`. Capture snapshots the FULL prior same-name set
    /// ([`ThreadManager::snapshot_records`]); the inverse converges back to it;
    /// the forward converges to `desired`. Both directions run through
    /// [`ThreadManager::converge_records`] — the lone lock-atomic record-set
    /// mutation point — so no entry-apply path can mutate a thread's record set
    /// except through it.
    ///
    /// GRANULARITY. One whole-set capture→converge inverse REPLACES the prior
    /// per-record `step_nonatomic` inverses. Correct, not a regression: per-record
    /// granularity existed to unwind a NON-atomic forward that could fail partway
    /// (the r3/r4 `save`/`delete` loop). `converge_records` is lock-atomic /
    /// all-or-nothing, so a single converge-back-to-prior fully reverses it.
    fn converge_thread_records(&mut self, name: &str, desired: Vec<Thread>) -> HeddleResult<()> {
        let repo = self.repo();
        let forward_name = name.to_string();
        let restore_name = name.to_string();
        let prior = ThreadManager::new(repo.heddle_dir()).snapshot_records(name)?;
        self.step_nonatomic(
            || Ok(prior),
            move |prior| {
                ThreadManager::new(repo.heddle_dir()).converge_records(&restore_name, &prior)
            },
            move || ThreadManager::new(repo.heddle_dir()).converge_records(&forward_name, &desired),
        )
    }

    /// Persist a mutated ThreadManager record as the SOLE record under its thread
    /// name (single-record postcondition). Routes through
    /// [`converge_thread_records`](Self::converge_thread_records) with
    /// `desired = [record]`, so a replacement save that would write a NEW-id,
    /// newer-timestamped record cannot leave a duplicate behind, and rollback
    /// converges back to the captured prior set regardless of any leaked id.
    fn save_thread_record(&mut self, record: Thread) -> HeddleResult<()> {
        let name = record.thread.clone();
        self.converge_thread_records(&name, vec![record])
    }

    /// Restore a ThreadManager record from a redo snapshot as the SOLE record
    /// under its thread name. Capture = the full prior same-name set; inverse =
    /// converge back to it; forward = DECODE the opaque snapshot bytes and
    /// [`converge_records`](ThreadManager::converge_records) the name to the single
    /// restored record — NOT a raw save, so a pre-existing duplicate cannot
    /// survive (cid 3331603135) and the success path has the same single-record
    /// postcondition as the rollback converge. A snapshot that fails to decode is
    /// a non-fatal warning that writes nothing — the on-disk set stays the
    /// captured prior set (a no-op converge), preserving the pre-migration
    /// ref-only fallback.
    fn restore_thread_record(
        &mut self,
        name: &str,
        bytes: &[u8],
        op_label: &'static str,
    ) -> HeddleResult<()> {
        let repo = self.repo();
        let forward_name = name.to_string();
        let restore_name = name.to_string();
        let warn_name = name.to_string();
        let bytes = bytes.to_vec();
        let prior = ThreadManager::new(repo.heddle_dir()).snapshot_records(name)?;
        self.step_nonatomic(
            || Ok(prior),
            move |prior| {
                ThreadManager::new(repo.heddle_dir()).converge_records(&restore_name, &prior)
            },
            move || {
                let manager = ThreadManager::new(repo.heddle_dir());
                match manager.decode_thread_record_snapshot(&bytes) {
                    Ok(restored) => {
                        manager.converge_records(&forward_name, std::slice::from_ref(&restored))
                    }
                    Err(e) => {
                        eprintln!(
                            "warning: replay of `{}` for '{}' restored the ref but failed \
                             to decode the ThreadManager record snapshot ({}). Record-backed \
                             commands (`thread cd`, delegate) may degrade on this thread — run \
                             `heddle thread start {}` to recreate the record.",
                            op_label, warn_name, e, warn_name
                        );
                        // Write nothing: the on-disk set is unchanged (== the
                        // captured prior set), i.e. a no-op converge.
                        Ok(())
                    }
                }
            },
        )
    }

    fn restore_thread_record_set(
        &mut self,
        name: &str,
        snapshots: &[Vec<u8>],
        op_label: &'static str,
    ) -> HeddleResult<()> {
        let manager = ThreadManager::new(self.repo().heddle_dir());
        let mut restored = Vec::with_capacity(snapshots.len());
        for bytes in snapshots {
            match manager.decode_thread_record_snapshot(bytes) {
                Ok(record) => restored.push(record),
                Err(e) => {
                    eprintln!(
                        "warning: replay of `{}` for '{}' skipped ThreadManager record-set \
                         restore because one snapshot failed to decode ({}).",
                        op_label, name, e
                    );
                    return Ok(());
                }
            }
        }
        self.converge_thread_records(name, restored)
    }

    /// Remove a redaction record from its per-blob sidecar. NON-atomic and
    /// re-exposing: `remove_redaction` rewrites or deletes the sidecar file, so
    /// the capture snapshots the whole sidecar (bytes or absence) and registers a
    /// restore-to-snapshot BEFORE the removal — a later transaction failure then
    /// restores the sidecar and the redacted blob is NOT re-exposed.
    fn remove_redaction_sidecar(
        &mut self,
        blob: ContentHash,
        state: ChangeId,
        path: String,
        redaction_id: ContentHash,
    ) -> HeddleResult<()> {
        let repo = self.repo();
        self.step_nonatomic(
            move || repo.capture_redaction_sidecar(&blob).map_err(apply_error),
            move |snapshot| {
                repo.restore_redaction_sidecar(&blob, snapshot)
                    .map_err(apply_error)
            },
            move || {
                repo.remove_redaction(&blob, &state, &path, &redaction_id)
                    .map(|_| ())
                    .map_err(apply_error)
            },
        )
    }

    /// Restore the per-state visibility sidecar from `expected_current` to
    /// `target` as part of the undo/redo transaction (heddle#317 r7). Undo
    /// passes `expected_current = new_sidecar` (the op's after-image, what should
    /// be on disk) and `target = prior_sidecar`; redo passes them swapped.
    ///
    /// Unlike the other reversible writes, a visibility-sidecar mutation has a
    /// concurrent counterpart — `heddle visibility set`/`promote`'s
    /// `commit_state_visibility` — that writes the SAME existing state's sidecar
    /// under the repo write lock. So both directions route through
    /// [`Repository::restore_state_visibility_sidecar_if_unchanged`], which takes
    /// that same lock and re-checks the current sidecar before writing:
    ///
    ///   * FORWARD (`step`, all-or-nothing under the lock): write `target` only if
    ///     the current sidecar still equals `expected_current`. If a concurrent
    ///     commit already superseded it, ABORT the transaction (a `Conflict`)
    ///     rather than clobbering the newer record — the forward writes nothing, so
    ///     `step` registers no inverse for an effect that did not happen.
    ///   * INVERSE (rollback): restore back to `expected_current`, but again only
    ///     if OUR write (`target`) is still the current sidecar. If a concurrent
    ///     commit superseded our write between the forward and the rollback, the
    ///     re-check leaves that newer record in place — closing the TOCTOU where the
    ///     transaction's own rewind (running after the isolation conflict was
    ///     detected) overwrote a concurrently-committed record.
    fn restore_visibility_sidecar(
        &mut self,
        state: ChangeId,
        expected_current: Option<Vec<u8>>,
        target: Option<Vec<u8>>,
    ) -> HeddleResult<()> {
        let repo = self.repo();
        // The inverse undoes the forward's write: after the forward wrote
        // `target`, the current sidecar should be `target`, and the rollback
        // restores it back to `expected_current` — but only if our write still
        // stands (re-checked under the lock, same as the forward).
        let inverse_expected = target.clone();
        let inverse_target = expected_current.clone();
        self.step(
            move || match repo
                .restore_state_visibility_sidecar_if_unchanged(&state, &expected_current, target)
                .map_err(apply_error)?
            {
                VisibilitySidecarRestore::Applied => Ok(()),
                VisibilitySidecarRestore::Superseded => Err(visibility_superseded_conflict(&state)),
            },
            move || {
                repo.restore_state_visibility_sidecar_if_unchanged(
                    &state,
                    &inverse_expected,
                    inverse_target,
                )
                .map(|_| ())
                .map_err(apply_error)
            },
        )
    }

    /// Run one checkout-repo Git write as a capture-restore step against the
    /// pre-entry [`GitState`] `snapshot`. Git checkpoint entries make several
    /// internal writes; restoring to the absolute pre-entry snapshot is
    /// idempotent across the entry's steps, and registering the restore BEFORE
    /// each write covers a write that fails partway. A fresh checkout handle is
    /// opened per step (cold path).
    fn git_restore_snapshot(
        &mut self,
        repo: &'a Repository,
        branch: &str,
        snapshot: &GitState,
        forward: impl FnOnce() -> Result<()>,
    ) -> HeddleResult<()> {
        let snapshot = snapshot.clone();
        let branch = branch.to_string();
        self.step_nonatomic(
            move || Ok(snapshot),
            move |snapshot| restore_git_state(repo, &branch, &snapshot),
            move || forward().map_err(apply_error),
        )
    }

    /// Flip the batch's persisted undone flag; inverse re-marks it redone. A
    /// single oplog write. Returns the updated batch for the command output.
    fn mark_batch_undone(&mut self, batch: &OpBatch) -> HeddleResult<OpBatch> {
        let repo = self.repo();
        let forward_batch = batch.clone();
        let inverse_batch = batch.clone();
        self.step(
            move || repo.oplog().mark_batch_undone(&forward_batch),
            move || repo.oplog().mark_batch_redone(&inverse_batch).map(|_| ()),
        )
    }

    /// Flip the batch's persisted redone flag; inverse re-marks it undone. The
    /// mirror of [`mark_batch_undone`](Self::mark_batch_undone).
    fn mark_batch_redone(&mut self, batch: &OpBatch) -> HeddleResult<OpBatch> {
        let repo = self.repo();
        let forward_batch = batch.clone();
        let inverse_batch = batch.clone();
        self.step(
            move || repo.oplog().mark_batch_redone(&forward_batch),
            move || repo.oplog().mark_batch_undone(&inverse_batch).map(|_| ()),
        )
    }
}

fn apply_undo_entry(steps: &mut EntrySteps, entry: &OpEntry) -> HeddleResult<()> {
    match &entry.operation {
        OpRecord::Snapshot {
            prev_head: Some(prev),
            thread,
            new_state,
            ..
        } => {
            steps.goto(*prev)?;
            if let Some(thread) = thread {
                steps.set_thread(thread.as_str(), *prev)?;
                steps.write_head(Head::Attached {
                    thread: ThreadName::new(thread.as_str()),
                })?;
                sync_thread_record_state(steps, thread, *prev)?;
                mark_merged_threads_unintegrated_for_target(steps, thread, new_state, prev)?;
            }
        }
        OpRecord::Goto {
            prev_head: Some(prev),
            ..
        } => {
            steps.goto(*prev)?;
        }
        OpRecord::Snapshot {
            prev_head: None, ..
        }
        | OpRecord::Goto {
            prev_head: None, ..
        } => {}
        OpRecord::ThreadCreate { name, .. } => {
            delete_thread_safely(steps, &ThreadName::new(name.as_str()))?;
            // Cross-thread contract rule 4 (docs/design/cross-thread-undo.md):
            // the inverse of `ThreadCreate` must also remove the matching
            // ThreadManager record so `heddle thread show` and the record-
            // store readers don't surface a phantom entry for a thread
            // whose ref no longer exists. The worktree-attached refusal in
            // `ensure_thread_worktree_undo_safe` already gated us, so any
            // record we hit here has `materialized_path = None` or a path
            // that no longer exists — either way, dropping the record is
            // safe. Missing record is fine: not every `ThreadCreate` path
            // writes one.
            //
            // `manager_snapshot` is recorded for redo, so undo can still
            // destroy the live record without losing the data needed to put it
            // back.
            remove_thread_manager_record(steps, name)?;
        }
        OpRecord::ThreadDelete { name, state } => {
            steps.set_thread(name.as_str(), *state)?;
        }
        OpRecord::ThreadUpdate {
            name,
            old_state,
            manager_snapshots,
            ..
        } => {
            if manager_snapshots
                .as_ref()
                .is_some_and(|snapshots| snapshots.old_ref_absent)
            {
                steps.delete_thread(name.as_str())?;
            } else {
                steps.set_thread(name.as_str(), *old_state)?;
                steps.restore_active_thread_worktree(name.as_str(), *old_state)?;
            }
            if let Some(snapshots) = manager_snapshots.as_ref() {
                if !snapshots.old_records.is_empty() || !snapshots.new_records.is_empty() {
                    steps.restore_thread_record_set(
                        name,
                        &snapshots.old_records,
                        "ThreadUpdate",
                    )?;
                } else if let Some(bytes) = snapshots.old.as_ref() {
                    steps.restore_thread_record(name, bytes, "ThreadUpdate")?;
                }
            }
        }
        OpRecord::MarkerCreate { name, .. } => {
            steps.delete_marker(name.as_str())?;
        }
        OpRecord::MarkerDelete { name, state } => {
            steps.create_marker(name.as_str(), *state)?;
        }
        OpRecord::Collapse {
            thread: Some(thread),
            pre_thread_state: Some(pre_thread_state),
            ..
        } => {
            steps.set_thread(thread.as_str(), *pre_thread_state)?;
            sync_thread_record_state(steps, thread, *pre_thread_state)?;
        }
        OpRecord::Collapse { .. } => {}
        // Redaction inverse: drop the specific redaction record so
        // subsequent materialize calls restore the original blob
        // bytes. The opt-in flag + purged-bytes check are enforced in
        // `cmd_undo::ensure_redaction_undo_safe` before this point;
        // `remove_redaction` re-checks `purged_at` defensively so a
        // future caller that bypasses the CLI gate can't lose the
        // audit trail of destroyed bytes.
        //
        // Pass the oplog-recorded `redaction_id` through so a
        // refinement pass (multiple records sharing the same
        // `(blob, state, path)` with different reasons or signatures)
        // undoes the exact record this op references rather than the
        // first match in sidecar order. `remove_redaction` falls
        // back to `(state, path)` only for the purge-id-shift case
        // and refuses in that branch.
        OpRecord::Redact {
            redaction_id,
            blob,
            state,
            path,
        } => {
            // NON-atomic, re-exposing forward: `remove_redaction` rewrites or
            // deletes the per-blob sidecar, so it runs through `step_nonatomic`
            // with a capture-restore of the whole sidecar. If a LATER batch in
            // the same undo transaction fails, the rollback restores the sidecar
            // and the redacted blob is NOT re-exposed — the hazard a plain,
            // unregistered removal left open.
            steps.remove_redaction_sidecar(*blob, *state, path.clone(), *redaction_id)?;
        }
        // Fast-forward merge inverse: restore both HEAD and the target
        // thread ref to the pre-FF tip. The source thread never moved
        // during the FF, so it's untouched. Closes heddle#99 r1 — the
        // bug where recording an FF as `OpRecord::Goto` left the target
        // thread ref stranded at the FF target after undo.
        OpRecord::FastForward {
            source_thread,
            target_thread,
            pre_target_id,
            ..
        } => {
            apply_ff_undo(steps, source_thread, target_thread, pre_target_id)?;
        }
        OpRecord::GitCheckpoint {
            branch,
            previous_git_oid,
            new_git_oid,
            ..
        } => {
            apply_git_checkpoint_undo(steps, branch, previous_git_oid.as_deref(), new_git_oid)?;
        }
        // Visibility set/promote undo: restore the per-state sidecar from the
        // op's after-image (`new_sidecar`, what should currently be on disk)
        // back to the before-image (`prior_sidecar`). Both set and promote
        // restore the same way — the whole sidecar is snapshotted around the
        // put, so the inverse is "put the prior bytes back" whether the forward
        // appended a first record or a superseding one. `None` means the state
        // was public-by-absence before the op, so the sidecar is removed and
        // `has_visibility_for_state` reports false again. The restore is
        // conflict-rechecked under the repo write lock (heddle#317 r7): a
        // concurrent `visibility set`/`promote` that already superseded
        // `new_sidecar` aborts the undo instead of being clobbered. Closes PR
        // #529 P1: the old no-op left the oplog and sidecar divergent.
        OpRecord::StateVisibilitySet {
            state,
            prior_sidecar,
            new_sidecar,
            ..
        }
        | OpRecord::StateVisibilityPromote {
            state,
            prior_sidecar,
            new_sidecar,
            ..
        } => {
            steps.restore_visibility_sidecar(*state, new_sidecar.clone(), prior_sidecar.clone())?;
        }
        // No undo inverse: these records don't move a ref the undo chain
        // restores, or their reversal is irreversible / handled outside the
        // oplog replay. Enumerated explicitly (no wildcard) so a new
        // `OpRecord` variant is a COMPILE error here until its undo behavior
        // is decided (heddle#354 r9):
        //   - Fork: structural op; HEAD/thread restoration is driven by
        //     surrounding records in the same batch.
        //   - Checkpoint: addressable save, goto-reachable; nothing to invert.
        //   - TransactionAbort / TransactionCommit / ConflictResolved: forensic
        //     / audit records, no ref to restore.
        //   - EphemeralThreadCollapse: TTL retirement of a thread pointer; the
        //     states stay addressable and the pointer is not resurrected here.
        //   - Purge: irreversible by design (bytes physically removed) — the
        //     undo preflight (`ensure_redaction_undo_safe`) refuses earlier.
        //   - RemoteThreadUpdate / RemoteThreadDelete / UndoRecoveryUpdate:
        //     reconcile-class bookkeeping refs, outside the user undo chain.
        OpRecord::Fork { .. }
        | OpRecord::Checkpoint { .. }
        | OpRecord::TransactionAbort { .. }
        | OpRecord::TransactionCommit { .. }
        | OpRecord::ConflictResolved { .. }
        | OpRecord::EphemeralThreadCollapse { .. }
        | OpRecord::Purge { .. }
        | OpRecord::RemoteThreadUpdate { .. }
        | OpRecord::RemoteThreadDelete { .. }
        | OpRecord::UndoRecoveryUpdate { .. } => {}
    }

    Ok(())
}

fn apply_ff_undo(
    steps: &mut EntrySteps,
    source_thread: &str,
    target_thread: &str,
    pre_target_id: &ChangeId,
) -> HeddleResult<()> {
    steps.goto(*pre_target_id)?;
    steps.set_thread(target_thread, *pre_target_id)?;
    steps.write_head(Head::Attached {
        thread: ThreadName::new(target_thread),
    })?;
    sync_thread_record_state(steps, target_thread, *pre_target_id)?;
    mark_source_thread_unintegrated(steps, source_thread, pre_target_id)
}

fn apply_redo_entry(steps: &mut EntrySteps, entry: &OpEntry) -> HeddleResult<()> {
    match &entry.operation {
        OpRecord::Snapshot {
            new_state,
            prev_head,
            thread,
            ..
        } => {
            steps.goto(*new_state)?;
            if let Some(thread) = thread {
                steps.set_thread(thread.as_str(), *new_state)?;
                steps.write_head(Head::Attached {
                    thread: ThreadName::new(thread.as_str()),
                })?;
                sync_thread_record_state(steps, thread, *new_state)?;
                mark_ready_threads_integrated_for_target(steps, thread, new_state, prev_head)?;
            }
        }
        OpRecord::Goto { target, .. } => {
            steps.goto(*target)?;
        }
        // Restore both the thread ref and the ThreadManager record body
        // from the snapshot captured at recording time.
        //
        // `manager_snapshot = None` means the forward path didn't have
        // a record to snapshot (cmd_start before materialization, the
        // rename batch's new-name arm, ingest, harness/agent stubs).
        // Restore the ref only in that case — no record to put back.
        OpRecord::ThreadCreate {
            name,
            state,
            manager_snapshot,
        } => {
            steps.set_thread(name.as_str(), *state)?;
            if let Some(bytes) = manager_snapshot {
                steps.restore_thread_record(name, bytes, "ThreadCreate")?;
            }
        }
        OpRecord::ThreadDelete { name, .. } => {
            delete_thread_safely(steps, &ThreadName::new(name.as_str()))?;
        }
        OpRecord::ThreadUpdate {
            name,
            new_state,
            manager_snapshots,
            ..
        } => {
            steps.set_thread(name.as_str(), *new_state)?;
            steps.restore_active_thread_worktree(name.as_str(), *new_state)?;
            if let Some(snapshots) = manager_snapshots.as_ref() {
                if !snapshots.old_records.is_empty() || !snapshots.new_records.is_empty() {
                    steps.restore_thread_record_set(
                        name,
                        &snapshots.new_records,
                        "ThreadUpdate",
                    )?;
                } else if let Some(bytes) = snapshots.new.as_ref() {
                    steps.restore_thread_record(name, bytes, "ThreadUpdate")?;
                }
            }
        }
        OpRecord::MarkerCreate { name, state } => {
            steps.create_marker(name.as_str(), *state)?;
        }
        OpRecord::MarkerDelete { name, .. } => {
            steps.delete_marker(name.as_str())?;
        }
        OpRecord::Collapse {
            thread: Some(thread),
            result,
            pre_thread_state: Some(_),
            ..
        } => {
            steps.set_thread(thread.as_str(), *result)?;
            sync_thread_record_state(steps, thread, *result)?;
        }
        OpRecord::Collapse { .. } => {}
        // FF merge redo: replay the *recorded* FF target. We do
        // not re-read `source_thread` — the recorded `post_target_id`
        // is the exact state the target advanced to at the original
        // FF, so redo is deterministic regardless of what the source
        // thread did between undo and redo (advanced, was deleted,
        // etc.). Closes heddle#99 r2 — Codex's non-determinism finding
        // on the r1 implementation.
        OpRecord::FastForward {
            source_thread,
            target_thread,
            post_target_id,
            ..
        } => {
            apply_ff_redo(steps, source_thread, target_thread, post_target_id)?;
        }
        OpRecord::GitCheckpoint {
            branch,
            previous_git_oid,
            new_git_oid,
            ..
        } => {
            apply_git_checkpoint_redo(steps, branch, previous_git_oid.as_deref(), new_git_oid)?;
        }
        // No redo replay: these records don't re-advance a ref redo touches, or
        // they are refused upstream. Enumerated explicitly (no wildcard) so a
        // new `OpRecord` variant is a COMPILE error here until its redo
        // behavior is decided (heddle#354 r9):
        //   - Fork / Collapse: structural ops; redo is driven by the
        //     surrounding records in the same batch.
        //   - Checkpoint: addressable save, goto-reachable; nothing to replay.
        //   - Redact: redo is refused upstream by `ensure_redaction_redo_supported`
        //     (the OpRecord doesn't carry the full Redaction needed to recreate
        //     it); reaching here is a no-op.
        //   - Purge: irreversible by design; also refused upstream.
        //   - TransactionAbort / TransactionCommit / ConflictResolved /
        //     EphemeralThreadCollapse: forensic / TTL records, no ref to replay.
        //   - RemoteThreadUpdate / RemoteThreadDelete / UndoRecoveryUpdate:
        //     reconcile-class bookkeeping refs, outside the user redo chain.
        // Visibility set/promote redo: re-apply the after-image captured on the
        // op (`new_sidecar`). Undo restored the sidecar to `prior_sidecar`; redo
        // writes it back to exactly the post-put bytes (or removes it when the op
        // resolved to public-by-absence). Symmetric with the undo arm: it expects
        // the current sidecar to be `prior_sidecar` (what the undo restored) and
        // conflict-rechecks under the repo write lock before writing
        // `new_sidecar`, so a concurrent `visibility set`/`promote` that
        // superseded `prior_sidecar` aborts the redo instead of being clobbered.
        OpRecord::StateVisibilitySet {
            state,
            prior_sidecar,
            new_sidecar,
            ..
        }
        | OpRecord::StateVisibilityPromote {
            state,
            prior_sidecar,
            new_sidecar,
            ..
        } => {
            steps.restore_visibility_sidecar(*state, prior_sidecar.clone(), new_sidecar.clone())?;
        }
        OpRecord::Fork { .. }
        | OpRecord::Checkpoint { .. }
        | OpRecord::TransactionAbort { .. }
        | OpRecord::TransactionCommit { .. }
        | OpRecord::ConflictResolved { .. }
        | OpRecord::EphemeralThreadCollapse { .. }
        | OpRecord::Redact { .. }
        | OpRecord::Purge { .. }
        | OpRecord::RemoteThreadUpdate { .. }
        | OpRecord::RemoteThreadDelete { .. }
        | OpRecord::UndoRecoveryUpdate { .. } => {}
    }

    Ok(())
}

fn apply_ff_redo(
    steps: &mut EntrySteps,
    source_thread: &str,
    target_thread: &str,
    post_target_id: &ChangeId,
) -> HeddleResult<()> {
    steps.goto(*post_target_id)?;
    steps.set_thread(target_thread, *post_target_id)?;
    steps.write_head(Head::Attached {
        thread: ThreadName::new(target_thread),
    })?;
    sync_thread_record_state(steps, target_thread, *post_target_id)?;
    mark_source_thread_integrated(steps, source_thread, post_target_id)
}

/// The pre-entry Git state a checkpoint undo/redo entry overwrites, captured
/// before any write so a partial failure restores it exactly. `git`-side writes
/// span the checkout repo (HEAD file, branch ref, index) and the heddle mirror
/// (branch ref), so all four are snapshotted.
#[derive(Clone)]
struct GitState {
    /// Raw `.git/HEAD` of the checkout repo (symref or detached oid line).
    head_file: Option<String>,
    /// `refs/heads/{branch}` in the checkout repo.
    checkout_branch_oid: Option<ObjectId>,
    /// Resolved checkout HEAD commit — the index is reset back to this.
    checkout_head_oid: Option<String>,
    /// `refs/heads/{branch}` in the heddle mirror.
    mirror_branch_oid: Option<ObjectId>,
}

fn capture_git_state(repo: &Repository, branch: &str) -> HeddleResult<GitState> {
    let git = git_checkout_repo(repo).map_err(apply_error)?;
    let checkout_branch_oid = if branch == "HEAD" {
        None
    } else {
        ref_target_oid(&git, &format!("refs/heads/{branch}")).map_err(apply_error)?
    };
    let head_file = fs::read_to_string(git.git_dir().join("HEAD")).ok();
    let checkout_head_oid = git
        .head()
        .ok()
        .and_then(|head| head.oid.map(|id| id.to_string()));
    let mirror_branch_oid = capture_mirror_oid(repo, branch).map_err(apply_error)?;
    Ok(GitState {
        head_file,
        checkout_branch_oid,
        checkout_head_oid,
        mirror_branch_oid,
    })
}

/// Restore the checkout + mirror Git state captured in [`GitState`]. Runs only on
/// the rollback path; absolute SET/DELETE ops, so re-running it (LIFO across the
/// entry's steps) is idempotent.
fn restore_git_state(repo: &Repository, branch: &str, state: &GitState) -> HeddleResult<()> {
    let git = git_checkout_repo(repo).map_err(apply_error)?;
    if branch != "HEAD" {
        let ref_name = format!("refs/heads/{branch}");
        match state.checkout_branch_oid {
            Some(oid) => set_reference(
                &git,
                &ref_name,
                oid,
                RefPrecondition::Any,
                "heddle: rollback git checkpoint",
            )
            .map_err(|error| apply_error(anyhow!(error)))?,
            None => delete_ref_if_present(&git, &ref_name).map_err(apply_error)?,
        }
    }
    if let Some(head) = &state.head_file {
        let head_path = git.git_dir().join("HEAD");
        fs::write(&head_path, head)?;
        fsync_file_and_parent(&head_path).map_err(apply_error)?;
    }
    if let Some(oid) = &state.checkout_head_oid {
        let oid = parse_git_oid(oid).map_err(apply_error)?;
        reset_git_index_to_commit(&git, oid).map_err(apply_error)?;
    }
    restore_mirror_oid(repo, branch, state.mirror_branch_oid).map_err(apply_error)?;
    Ok(())
}

fn capture_mirror_oid(repo: &Repository, branch: &str) -> Result<Option<ObjectId>> {
    if branch == "HEAD" {
        return Ok(None);
    }
    let mirror = repo.heddle_dir().join("git");
    if !mirror.exists() {
        return Ok(None);
    }
    let git = open_git_repo(&mirror)?;
    ref_target_oid(&git, &format!("refs/heads/{branch}"))
}

fn restore_mirror_oid(repo: &Repository, branch: &str, oid: Option<ObjectId>) -> Result<()> {
    if branch == "HEAD" {
        return Ok(());
    }
    let mirror = repo.heddle_dir().join("git");
    if !mirror.exists() {
        return Ok(());
    }
    let git = open_git_repo(&mirror)?;
    let ref_name = format!("refs/heads/{branch}");
    match oid {
        Some(oid) => set_reference(
            &git,
            &ref_name,
            oid,
            RefPrecondition::Any,
            "heddle: rollback mirror checkpoint ref",
        )
        .map_err(|error| anyhow!(error)),
        None => delete_ref_if_present(&git, &ref_name),
    }
}

fn delete_ref_if_present(git: &SleyRepository, ref_name: &str) -> Result<()> {
    if ref_target_oid(git, ref_name)?.is_some() {
        delete_reference_matching(git, ref_name, None)?;
    }
    Ok(())
}

fn apply_git_checkpoint_undo(
    steps: &mut EntrySteps,
    branch: &str,
    previous_git_oid: Option<&str>,
    new_git_oid: &str,
) -> HeddleResult<()> {
    let repo = steps.repo();
    ensure_git_head_is(repo, new_git_oid, "undo git checkpoint").map_err(apply_error)?;
    ensure_git_worktree_clean(repo, "undo git checkpoint").map_err(apply_error)?;
    let snapshot = capture_git_state(repo, branch)?;
    let new_oid = parse_git_oid(new_git_oid).map_err(apply_error)?;
    match previous_git_oid {
        Some(previous) => {
            let previous_oid = parse_git_oid(previous).map_err(apply_error)?;
            if branch != "HEAD" {
                let git = git_checkout_repo(repo).map_err(apply_error)?;
                if ref_target_oid(&git, &format!("refs/heads/{branch}")).map_err(apply_error)?
                    != Some(previous_oid)
                {
                    steps.git_restore_snapshot(repo, branch, &snapshot, || {
                        attach_git_head_to_branch(&git_checkout_repo(repo)?, branch)
                    })?;
                    steps.git_restore_snapshot(repo, branch, &snapshot, || {
                        set_attached_git_head(
                            &git_checkout_repo(repo)?,
                            branch,
                            previous_oid,
                            new_oid,
                            "heddle: undo git checkpoint",
                        )
                    })?;
                }
                steps.git_restore_snapshot(repo, branch, &snapshot, || {
                    attach_git_head_to_branch(&git_checkout_repo(repo)?, branch)
                })?;
            }
            steps.git_restore_snapshot(repo, branch, &snapshot, || {
                reset_git_index_to_commit(&git_checkout_repo(repo)?, previous_oid)
            })?;
            let previous = previous.to_string();
            let new_git_oid = new_git_oid.to_string();
            steps.git_restore_snapshot(repo, branch, &snapshot, || {
                update_mirror_branch_ref(repo, branch, Some(&previous), Some(&new_git_oid))
            })?;
        }
        None => {
            if branch != "HEAD" {
                steps.git_restore_snapshot(repo, branch, &snapshot, || {
                    delete_reference_matching(
                        &git_checkout_repo(repo)?,
                        &format!("refs/heads/{branch}"),
                        Some(new_oid),
                    )
                })?;
            }
            let new_git_oid = new_git_oid.to_string();
            steps.git_restore_snapshot(repo, branch, &snapshot, || {
                update_mirror_branch_ref(repo, branch, None, Some(&new_git_oid))
            })?;
        }
    }
    Ok(())
}

fn apply_git_checkpoint_redo(
    steps: &mut EntrySteps,
    branch: &str,
    previous_git_oid: Option<&str>,
    new_git_oid: &str,
) -> HeddleResult<()> {
    let repo = steps.repo();
    if let Some(previous) = previous_git_oid {
        ensure_git_head_is(repo, previous, "redo git checkpoint").map_err(apply_error)?;
    }
    let snapshot = capture_git_state(repo, branch)?;
    let new_oid = parse_git_oid(new_git_oid).map_err(apply_error)?;
    if branch != "HEAD" {
        match previous_git_oid {
            Some(previous) => {
                let previous_oid = parse_git_oid(previous).map_err(apply_error)?;
                steps.git_restore_snapshot(repo, branch, &snapshot, || {
                    attach_git_head_to_branch(&git_checkout_repo(repo)?, branch)
                })?;
                steps.git_restore_snapshot(repo, branch, &snapshot, || {
                    set_attached_git_head(
                        &git_checkout_repo(repo)?,
                        branch,
                        new_oid,
                        previous_oid,
                        "heddle: redo git checkpoint",
                    )
                })?;
            }
            None => {
                steps.git_restore_snapshot(repo, branch, &snapshot, || {
                    set_reference(
                        &git_checkout_repo(repo)?,
                        &format!("refs/heads/{branch}"),
                        new_oid,
                        RefPrecondition::Any,
                        "heddle: redo git checkpoint",
                    )
                    .map_err(|error| anyhow!(error))
                })?;
                steps.git_restore_snapshot(repo, branch, &snapshot, || {
                    attach_git_head_to_branch(&git_checkout_repo(repo)?, branch)
                })?;
            }
        }
    }
    steps.git_restore_snapshot(repo, branch, &snapshot, || {
        reset_git_index_to_commit(&git_checkout_repo(repo)?, new_oid)
    })?;
    let previous_git_oid = previous_git_oid.map(|previous| previous.to_string());
    let new_git_oid = new_git_oid.to_string();
    steps.git_restore_snapshot(repo, branch, &snapshot, || {
        update_mirror_branch_ref(
            repo,
            branch,
            Some(&new_git_oid),
            previous_git_oid.as_deref(),
        )
    })?;
    Ok(())
}

fn update_mirror_branch_ref(
    repo: &Repository,
    branch: &str,
    target_oid: Option<&str>,
    expected_old_oid: Option<&str>,
) -> Result<()> {
    if branch == "HEAD" {
        return Ok(());
    }
    let mirror = repo.heddle_dir().join("git");
    if !mirror.exists() {
        return Ok(());
    }
    let git = open_git_repo(&mirror)?;
    let ref_name = format!("refs/heads/{branch}");
    if let Some(target) = target_oid
        && ref_target_oid(&git, &ref_name)? == Some(parse_git_oid(target)?)
    {
        return Ok(());
    }
    match (target_oid, expected_old_oid) {
        (Some(target), Some(expected)) => set_reference(
            &git,
            &ref_name,
            parse_git_oid(target)?,
            RefPrecondition::MustExistAndMatch(ReferenceTarget::Direct(parse_git_oid(expected)?)),
            "heddle: update mirror checkpoint ref",
        )
        .map_err(|error| anyhow!(error)),
        (Some(target), None) => set_reference(
            &git,
            &ref_name,
            parse_git_oid(target)?,
            RefPrecondition::Any,
            "heddle: update mirror checkpoint ref",
        )
        .map_err(|error| anyhow!(error)),
        (None, Some(expected)) => {
            delete_reference_matching(&git, &ref_name, Some(parse_git_oid(expected)?))
        }
        (None, None) => delete_reference_matching(&git, &ref_name, None),
    }
}

fn ensure_git_head_is(repo: &Repository, expected: &str, action: &str) -> Result<()> {
    let actual = current_git_head(repo)?;
    if actual == expected {
        return Ok(());
    }
    Err(anyhow!(RecoveryAdvice::git_head_mismatch(
        action,
        &actual,
        expected,
        repo.git_overlay_current_branch()?
            .unwrap_or_else(|| "HEAD".to_string()),
        git_dirty_paths(repo),
    )))
}

fn ensure_git_worktree_clean(repo: &Repository, action: &str) -> Result<()> {
    let Some(status) = repo.git_overlay_worktree_status()? else {
        return Ok(());
    };
    if status.is_clean() {
        return Ok(());
    }
    Err(anyhow!(RecoveryAdvice::dirty_worktree(
        action,
        git_status_paths(&status),
        "the Heddle undo batch has not been applied",
    )))
}

fn git_dirty_paths(repo: &Repository) -> Vec<String> {
    repo.git_overlay_worktree_status()
        .ok()
        .flatten()
        .map(|status| git_status_paths(&status))
        .unwrap_or_default()
}

fn git_status_paths(status: &objects::worktree::WorktreeStatus) -> Vec<String> {
    let mut paths = Vec::new();
    paths.extend(format_status_paths("modified", &status.modified));
    paths.extend(format_status_paths("added", &status.added));
    paths.extend(format_status_paths("deleted", &status.deleted));
    paths
}

fn format_status_paths(kind: &str, paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| format!("{kind}: {}", path.display()))
        .collect()
}

fn git_checkout_repo(repo: &Repository) -> Result<SleyRepository> {
    open_git_repo(repo.root()).map_err(|error| anyhow!(error))
}

fn parse_git_oid(oid: &str) -> Result<ObjectId> {
    oid.parse::<ObjectId>()
        .map_err(|error| anyhow!("invalid Git object id '{oid}': {error}"))
}

fn ref_target_oid(repo: &SleyRepository, name: &str) -> Result<Option<ObjectId>> {
    let Some(reference) = repo
        .find_reference(name)
        .map_err(|error| anyhow!("failed to inspect Git reference '{name}': {error}"))?
    else {
        return Ok(None);
    };
    reference
        .peeled_oid(repo)
        .map_err(|error| anyhow!("failed to resolve Git reference '{name}': {error}"))
}

fn attach_git_head_to_branch(repo: &SleyRepository, branch: &str) -> Result<()> {
    if branch == "HEAD" {
        return Ok(());
    }
    let head_path = repo.git_dir().join("HEAD");
    fs::write(&head_path, format!("ref: refs/heads/{branch}\n"))
        .map_err(|error| anyhow!("failed to attach Git HEAD to branch '{branch}': {error}"))?;
    fsync_file_and_parent(&head_path)?;
    Ok(())
}

fn set_attached_git_head(
    repo: &SleyRepository,
    branch: &str,
    target: ObjectId,
    expected: ObjectId,
    log_message: &str,
) -> Result<()> {
    let ref_name = if branch == "HEAD" {
        "HEAD".to_string()
    } else {
        format!("refs/heads/{branch}")
    };
    set_reference_with_reflog(
        repo,
        &ref_name,
        target,
        RefPrecondition::MustExistAndMatch(ReferenceTarget::Direct(expected)),
        log_message,
    )
    .map_err(|error| anyhow!("failed to update Git HEAD for branch '{branch}': {error}"))
}

fn reset_git_index_to_commit(repo: &SleyRepository, oid: ObjectId) -> Result<()> {
    let object = repo
        .read_object(&oid)
        .map_err(|error| anyhow!("failed to inspect Git commit {oid}: {error}"))?;
    if object.object_type != GitObjectType::Commit {
        return Err(anyhow!("failed to inspect Git commit {oid}: not a commit"));
    }
    let commit = repo
        .read_commit(&oid)
        .map_err(|error| anyhow!("failed to inspect Git commit {oid}: {error}"))?;
    let index = repo
        .index_from_tree(&commit.tree)
        .map_err(|error| anyhow!("failed to build Git index for commit {oid}: {error}"))?;
    repo.write_index(
        &index,
        IndexWriteOptions {
            fsync: true,
            validate_checksum: true,
        },
    )
    .map_err(|error| anyhow!("failed to write Git index for commit {oid}: {error}"))?;
    Ok(())
}

fn delete_reference_matching(
    repo: &SleyRepository,
    name: &str,
    expected: Option<ObjectId>,
) -> Result<()> {
    let current = ref_target_oid(repo, name)?;
    if current.is_none() {
        return Err(anyhow!(
            "failed to delete Git reference '{name}': ref is missing"
        ));
    }
    if let Some(expected) = expected
        && current != Some(expected)
    {
        return Err(anyhow!(
            "failed to delete Git reference '{name}': expected {expected}, found {}",
            current
                .map(|oid| oid.to_string())
                .unwrap_or_else(|| "missing".to_string())
        ));
    }
    let refs = repo.references();
    match refs
        .read_ref(name)
        .map_err(|error| anyhow!("failed to inspect Git reference '{name}': {error}"))?
    {
        Some(ReferenceTarget::Direct(oid)) => repo
            .delete_ref(DeleteRef {
                name: FullName::new(name)
                    .map_err(|error| anyhow!("invalid Git reference '{name}': {error}"))?,
                expected_old: Some(expected.unwrap_or(oid)),
                expected: None,
                reflog: None,
                reflog_committer: None,
            })
            .map_err(|error| anyhow!("failed to delete Git reference '{name}': {error}")),
        Some(ReferenceTarget::Symbolic(_)) => refs
            .delete_symbolic_ref(name)
            .map(|_| ())
            .map_err(|error| anyhow!("failed to delete Git reference '{name}': {error}")),
        None => Err(anyhow!(
            "failed to delete Git reference '{name}': ref is missing"
        )),
    }
}

fn git_signature() -> Signature {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let name = "Heddle";
    let email = "heddle@local";
    Signature {
        name: GitByteString::new(name.as_bytes().to_vec()),
        email: GitByteString::new(email.as_bytes().to_vec()),
        time: GitTime::new(seconds, 0),
        raw: format!("{name} <{email}> {seconds} +0000").into_bytes(),
    }
}

fn git_reflog_entry(
    old_oid: ObjectId,
    new_oid: ObjectId,
    message: &str,
) -> sley::plumbing::sley_refs::ReflogEntry {
    sley::plumbing::sley_refs::ReflogEntry {
        old_oid,
        new_oid,
        committer: git_signature().to_ident_bytes(),
        message: message.as_bytes().to_vec(),
    }
}

fn set_reference_with_reflog(
    repo: &SleyRepository,
    name: &str,
    target: ObjectId,
    constraint: RefPrecondition,
    log_message: &str,
) -> Result<()> {
    let refs = repo.references();
    let old_oid = match refs
        .read_ref(name)
        .map_err(|error| anyhow!("failed to inspect Git reference '{name}': {error}"))?
    {
        Some(ReferenceTarget::Direct(oid)) => oid,
        _ => ObjectId::null(repo.object_format()),
    };
    let reflog = git_reflog_entry(old_oid, target, log_message);
    let should_append_head_reflog = name != "HEAD"
        && repo
            .head()
            .ok()
            .and_then(|head| head.symbolic_target.map(|target| target.to_string()))
            .as_deref()
            == Some(name);
    let mut tx = refs.transaction();
    tx.update_to(
        name.to_string(),
        ReferenceTarget::Direct(target),
        constraint,
        Some(reflog.clone()),
    );
    tx.commit()
        .map_err(|error| anyhow!("failed to update Git reference '{name}': {error}"))?;
    if should_append_head_reflog {
        refs.append_reflog("HEAD", &reflog)
            .map_err(|error| anyhow!("failed to append Git HEAD reflog: {error}"))?;
    }
    Ok(())
}

fn fsync_file_and_parent(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| anyhow!("failed to sync '{}': {error}", path.display()))?;
    if let Some(parent) = path.parent() {
        fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|error| anyhow!("failed to sync '{}': {error}", parent.display()))?;
    }
    Ok(())
}

fn delete_thread_safely(steps: &mut EntrySteps, name: &ThreadName) -> HeddleResult<()> {
    let repo = steps.repo();
    if let Head::Attached { thread } = repo.head_ref()?
        && thread == *name
    {
        let state = repo.refs().get_thread(name)?.ok_or_else(|| {
            HeddleError::Conflict(
                thread_not_found_advice(name.as_str(), "delete thread").to_string(),
            )
        })?;
        // Detaching HEAD is its own effect (inverse restores the prior attached
        // HEAD), separate from the ref deletion below.
        steps.write_head(Head::Detached { state })?;
    }

    steps.delete_thread(name.as_str())?;
    Ok(())
}

fn sync_thread_record_state(
    steps: &mut EntrySteps,
    thread_name: &str,
    state: objects::object::ChangeId,
) -> HeddleResult<()> {
    let manager = ThreadManager::new(steps.repo().heddle_dir());
    if let Some(mut thread) = manager.find_by_thread(thread_name)? {
        thread.current_state = Some(state.short());
        thread.updated_at = chrono::Utc::now();
        steps.save_thread_record(thread)?;
    }
    Ok(())
}

fn mark_source_thread_unintegrated(
    steps: &mut EntrySteps,
    source_thread: &str,
    target_after_undo: &ChangeId,
) -> HeddleResult<()> {
    let repo = steps.repo();
    let manager = ThreadManager::new(repo.heddle_dir());
    let Some(mut thread) = manager.find_by_thread(source_thread)? else {
        return Ok(());
    };
    let source_tip = repo.refs().get_thread(&ThreadName::new(source_thread))?;
    let still_integrated = source_tip
        .as_ref()
        .is_some_and(|source_tip| change_contains(repo, source_tip, target_after_undo));
    if still_integrated {
        return Ok(());
    }

    if matches!(thread.state, ThreadState::Merged) {
        thread.state = ThreadState::Ready;
    }
    if let Some(source_tip) = source_tip {
        thread.current_state = Some(source_tip.short());
    }
    thread.merged_state = None;
    if matches!(
        thread.integration_policy_result.status.as_deref(),
        Some("auto_integrated")
    ) {
        thread.integration_policy_result = ThreadIntegrationPolicy::default();
    }
    refresh_thread_freshness(repo, &mut thread)?;
    if matches!(thread.freshness, ThreadFreshness::Unknown) {
        thread.freshness = ThreadFreshness::Current;
    }
    thread.updated_at = chrono::Utc::now();
    steps.save_thread_record(thread)?;
    Ok(())
}

fn mark_merged_threads_unintegrated_for_target(
    steps: &mut EntrySteps,
    target_thread: &str,
    integrated_state: &ChangeId,
    target_after_undo: &ChangeId,
) -> HeddleResult<()> {
    let repo = steps.repo();
    let manager = ThreadManager::new(repo.heddle_dir());
    for thread in manager.list()? {
        if thread.thread == target_thread
            || thread.target_thread.as_deref() != Some(target_thread)
            || thread.state != ThreadState::Merged
        {
            continue;
        }
        let points_at_integrated_state = thread
            .merged_state
            .as_deref()
            .or(thread.current_state.as_deref())
            .and_then(|state| repo.resolve_state(state).ok().flatten())
            .is_some_and(|state| state == *integrated_state);
        if points_at_integrated_state {
            // Each affected record is saved through its own per-effect step, so a
            // mid-loop failure rolls each one back independently.
            mark_source_thread_unintegrated(steps, &thread.thread, target_after_undo)?;
        }
    }
    Ok(())
}

fn mark_source_thread_integrated(
    steps: &mut EntrySteps,
    source_thread: &str,
    target_after_redo: &ChangeId,
) -> HeddleResult<()> {
    let repo = steps.repo();
    let manager = ThreadManager::new(repo.heddle_dir());
    let Some(mut thread) = manager.find_by_thread(source_thread)? else {
        return Ok(());
    };
    let source_tip = repo.refs().get_thread(&ThreadName::new(source_thread))?;
    let integrated = source_tip
        .as_ref()
        .is_some_and(|source_tip| change_contains(repo, source_tip, target_after_redo));
    if !integrated {
        return Ok(());
    }

    thread.state = ThreadState::Merged;
    thread.merged_state = Some(target_after_redo.short());
    thread.current_state = source_tip
        .map(|source_tip| source_tip.short())
        .or_else(|| Some(target_after_redo.short()));
    thread.integration_policy_result = ThreadIntegrationPolicy {
        status: Some("auto_integrated".to_string()),
        reason: Some("redo restored integrated target state".to_string()),
        manual_resolution_state: thread.integration_policy_result.manual_resolution_state,
    };
    thread.freshness = ThreadFreshness::Current;
    thread.updated_at = chrono::Utc::now();
    steps.save_thread_record(thread)?;
    Ok(())
}

fn mark_ready_threads_integrated_for_target(
    steps: &mut EntrySteps,
    target_thread: &str,
    integrated_state: &ChangeId,
    target_before_redo: &Option<ChangeId>,
) -> HeddleResult<()> {
    let repo = steps.repo();
    let manager = ThreadManager::new(repo.heddle_dir());
    for thread in manager.list()? {
        if thread.thread == target_thread
            || thread.target_thread.as_deref() != Some(target_thread)
            || thread.state != ThreadState::Ready
        {
            continue;
        }
        let Some(source_tip) = repo.refs().get_thread(&ThreadName::new(&thread.thread))? else {
            continue;
        };
        let newly_integrated = change_contains(repo, &source_tip, integrated_state)
            && !target_before_redo
                .as_ref()
                .is_some_and(|before| change_contains(repo, &source_tip, before));
        if newly_integrated {
            mark_source_thread_integrated(steps, &thread.thread, integrated_state)?;
        }
    }
    Ok(())
}

fn change_contains(repo: &Repository, ancestor: &ChangeId, descendant: &ChangeId) -> bool {
    let mut graph = CommitGraphIndex::new(repo);
    graph.is_ancestor(ancestor, descendant).unwrap_or(false)
}

/// Remove EVERY ThreadManager record filed under `thread_name`, converging the
/// name to EMPTY as ONE lock-atomic `step_nonatomic` (cid 3331603131). Used by
/// the `ThreadCreate` inverse to keep refs and record-store state in lockstep
/// (cross-thread undo contract rule 4).
///
/// Converging to empty under a SINGLE write lock via
/// [`ThreadManager::converge_records`] — rather than taking an unlocked `list()`
/// snapshot and deleting each record separately — is what closes the duplicate
/// class: a same-name writer that lands between the snapshot and the deletes can
/// no longer survive the converge (the old per-record loop had exactly that
/// window). Deleting the FULL same-name set (not just the `find_by_thread`
/// winner) also stops an older duplicate from being left as a phantom after the
/// thread ref is gone. The inverse re-converges to the full captured prior set,
/// so a later transaction failure restores ALL same-name records, not just the
/// winner.
fn remove_thread_manager_record(steps: &mut EntrySteps, thread_name: &str) -> HeddleResult<()> {
    steps.converge_thread_records(thread_name, Vec::new())
}

// ---- Atomic undo/redo (heddle#355 impl-b) ----
//
// `undo`/`redo` are migrated to the `AtomicMutation` primitive so the whole
// operation is all-or-nothing: a failure anywhere mid-apply rewinds every
// already-applied step back to the exact pre-operation state instead of
// leaving the repo half-rewound (the spike §5.1 hazard — batch N fails after
// batches `0..N` were applied AND marked undone, with no rollback).
//
// SHAPE. `undo`/`redo` perform direct, immediately-visible, **idempotent**
// canonical mutations (ref writes, `goto` worktree material, thread-record and
// git-mirror state, and the in-place `mark_batch_undone` flag flip) and append
// NO new domain oplog record — they navigate states that already exist. So:
//   * Each sub-op stages its effect through the forward-first `Tx::step`
//     combinator, which registers the inverse on the granular ledger ONLY after
//     the forward succeeds (a forward that fails registers no inverse at all).
//     Crucially this is done PER EFFECT through the `EntrySteps` applier:
//     `apply_undo_entry` / `apply_redo_entry` wrap EACH individual write
//     (`goto`, each ref/marker/thread-record write, each git write) in its own
//     `step` (single all-or-nothing write, capture-before inverse) or
//     `step_nonatomic` (composite/partial-failure forward, capture-restore
//     inverse registered BEFORE the forward). A failure on the Nth write of an
//     entry leaves the prior N-1 inverses on the ledger and the rollback
//     restores the exact pre-entry state. A whole-entry `step` (one forward =
//     many writes) would leave a write half-applied when a LATER write in the
//     same entry failed (heddle#355 cid 3330966930 / 3330966931) — the
//     granularity bug this migration must NOT reintroduce. A plain `step` on a
//     NON-atomic forward (goto, `ThreadManager::save`, redaction-sidecar
//     removal) would leak a partially-applied effect — those use `step_nonatomic`.
//   * The parent NESTS the sub-ops via `Tx::enroll` (deferred enrollment) —
//     the recovery-ref child then one child per batch — so a child that stages
//     then fails rewinds the child AND unwinds the parent through the shared
//     ledger. This is the nesting path the migration exists to validate.
//   * The commit point is the executor's lone `TransactionCommit` marker over
//     an EMPTY domain batch (`StagedCommit::pure`). `OpBatch::is_transaction_
//     marker_only` keeps that record-less commit sentinel out of the undo/redo
//     eligibility scans and the `undo --list` view.
//
// TWO VALIDATION NOTES (see the PR description) — neither blocks the migration,
// both are properties of mapping a self-mutating, immediately-visible op onto
// the primitive:
//   1. IDEMPOTENCY KEY. `undo`/`redo` have no unique content identity (they
//      revisit existing states), so a key derived from "operation identity"
//      (batch ids + head) COLLIDES on the legitimate `undo → redo → undo`
//      toggle, and the primitive's dedup-then-`rewind_all` on a hit would
//      silently REVERT the second undo. The key is therefore derived from the
//      oplog GENERATION (`head_id`) at command start — unique per committed
//      transaction (every undo/redo appends a marker that bumps the
//      generation), so the dedup branch is never taken. The crash-retry dedup
//      the trait optimizes for is both unreachable (a committed undo marks its
//      batches undone, so a retry re-derives a different batch set) and
//      unnecessary (re-applying an undo is idempotent) for this op.
//   2. DEFERRED-COMMIT SEMANTICS. The children use the deferred ENROLLMENT
//      mechanism (`enroll` + `step`) for its apply+ledger-rewind shape: each
//      defers its commit marker to the outer transaction and is unwound by the
//      shared ledger on failure. Their effects are direct canonical writes
//      (visible immediately) — `DeferredMutation` makes NO invisibility claim;
//      it names exactly this "defer the commit, be ledger-unwound" contract. So
//      this adds FAILURE atomicity (rewind), not concurrent-reader isolation —
//      matching the pre-migration concurrency semantics, where undo already
//      published refs batch-by-batch.
//
// EXACTNESS SCOPE. The rewind restores the exact pre-operation state for `undo`
// of any batch count and for single-batch `redo` (the `-n 1` default). A
// MULTI-batch `redo -n N>1` replays batches newest-first with absolute `goto`s
// (pre-existing forward behavior this migration preserves), so the per-entry
// inverses do not compose back to the exact pre-redo head — a mid-redo fault
// there rewinds to a consistent intermediate state, not the precise pre-redo
// tip. Still strictly safer than the pre-migration path, which had no rollback
// at all. Fixing it would mean reordering redo replay (a forward-behavior
// change) — out of scope for the atomicity migration.

/// Convert an `anyhow` error raised by an undo/redo apply helper into the
/// `HeddleError` the primitive's `Result` requires. The structured
/// `RecoveryAdvice` refusals are produced by the command-level preflights
/// (which run BEFORE `execute`), so a wrapped message here only ever surfaces a
/// genuinely-unexpected mid-apply failure — one the preflights could not
/// foresee — whose rewind has already restored the pre-operation state.
fn apply_error(err: anyhow::Error) -> HeddleError {
    HeddleError::Conflict(format!("{err:#}"))
}

/// The conflict surfaced when an undo/redo visibility-sidecar restore finds the
/// sidecar already superseded by a concurrent `visibility set`/`promote`
/// (heddle#317 r7). Aborting the transaction here — rather than overwriting the
/// newer record — is what makes the lock-serialized restore safe: the rewind
/// then leaves the concurrently-committed record intact.
fn visibility_superseded_conflict(state: &ChangeId) -> HeddleError {
    HeddleError::Conflict(format!(
        "cannot undo/redo visibility on state {}: a concurrent `visibility set`/`promote` \
         superseded the sidecar. The newer record is preserved; re-run undo/redo after \
         refreshing.",
        state.to_string_full()
    ))
}

/// Acquire the per-repository undo/redo serialization lock, held across the
/// whole `select batches → preflight → apply → commit` critical section.
///
/// Two concurrent `heddle undo` (or `redo`) invocations that both read the same
/// oplog generation derive the SAME generation-based [`undo_redo_transaction_id`];
/// the second then dedup-hits `AlreadyCommitted` inside the executor and
/// `rewind_all`s its own replay — silently reverting the first invocation while
/// returning success (heddle#355 cid 3330867776). Serializing the critical
/// section forces the second invocation to (re-)select its batches only AFTER
/// the first has committed, so it observes the already-undone batch and cleanly
/// undoes the next op (or finds nothing to do) — never colliding on the key.
/// Crash-retry of the same logical op is unaffected: a retry re-selects against
/// the post-commit generation and idempotently re-derives the correct batch set.
///
/// Uses a dedicated `locks/undo-redo.lock`, NOT the repo-wide `repo.lock`: the
/// apply path calls `goto`, which itself takes `repo.lock`, so reusing it here
/// would self-deadlock on the non-reentrant exclusive `flock`.
pub(super) fn acquire_undo_redo_lock(repo: &Repository) -> Result<WriteLockGuard> {
    RepoLock::at(repo.heddle_dir().join("locks/undo-redo.lock"))
        .write()
        .map_err(|e| anyhow!("failed to acquire undo/redo serialization lock: {e}"))
}

/// Build the stable-per-transaction idempotency key. Derived from the oplog
/// `generation` (read at command start) rather than the batch contents — see
/// the "IDEMPOTENCY KEY" note above for why a content-derived key is unsafe for
/// a self-mutating op.
pub(super) fn undo_redo_transaction_id(
    action: &str,
    scope: &str,
    generation: u64,
    batches: &[OpBatch],
) -> String {
    let ids: Vec<String> = batches.iter().map(|batch| batch.id.to_string()).collect();
    format!("{action}:{scope}:gen{generation}:[{}]", ids.join(","))
}

/// Deferred child: preserve the pre-undo HEAD into the heddle-internal
/// recovery pointer (the heddle#305 `ORIG_HEAD`-style ref), registering its
/// restore as the inverse so an outer failure puts the prior pointer back
/// (or clears it, on the first-ever undo).
struct StageUndoRecovery {
    head: Option<ChangeId>,
}

impl AtomicMutation for StageUndoRecovery {
    type Output = ();

    fn transaction_id(&self) -> String {
        // Enrolled children never reach the commit point; only the root's id is
        // used. A constant is sufficient and never minted fresh.
        "undo:stage-recovery".to_string()
    }

    fn isolation_keys(&self, repo: &Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
        let mut keys = BTreeSet::new();
        keys.insert(IsolationKey::LocalHead {
            scope: repo.op_scope(),
        });
        Ok(keys)
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<()>> {
        let Some(state) = self.head else {
            return Ok(StagedCommit::pure(()));
        };
        let repo = tx.repo();
        // Reconciled read of the prior pointer so the inverse restores exactly
        // what was there (never a raw-ref bypass).
        let prior = repo.refs().get_undo_recovery()?;
        // Forward-first via `Tx::step`: the restore inverse is registered ONLY
        // after the pointer is actually overwritten, so a failed write leaves
        // the pre-existing recovery pointer untouched on rollback.
        tx.step(
            || repo.refs().set_undo_recovery(&state),
            move || match prior {
                Some(prior) => repo.refs().set_undo_recovery(&prior),
                None => repo.refs().clear_undo_recovery(),
            },
        )?;
        Ok(StagedCommit::pure(()))
    }
}

impl DeferredMutation for StageUndoRecovery {}

/// Deferred child: undo one batch. `apply_undo_entry` registers a per-EFFECT
/// inverse for every individual write it performs, so a mid-ENTRY failure rewinds
/// exactly the writes already applied (not just whole entries); the
/// `mark_batch_undone` flip is paired with its `mark_batch_redone` inverse.
struct ApplyUndoBatch {
    batch: OpBatch,
    /// Test seam: when `Some(n)`, fail immediately after undoing `n` entries,
    /// to exercise the mid-batch rewind path. Always `None` in production.
    #[cfg(test)]
    fail_after_entries: Option<usize>,
}

impl ApplyUndoBatch {
    fn new(batch: OpBatch) -> Self {
        Self {
            batch,
            #[cfg(test)]
            fail_after_entries: None,
        }
    }

    #[cfg(test)]
    fn failing_after(batch: OpBatch, entries: usize) -> Self {
        Self {
            batch,
            fail_after_entries: Some(entries),
        }
    }
}

impl AtomicMutation for ApplyUndoBatch {
    type Output = OpBatch;

    fn transaction_id(&self) -> String {
        format!("undo:batch:{}", self.batch.id)
    }

    fn isolation_keys(&self, repo: &Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
        Ok(isolation_keys_for_batches(
            std::slice::from_ref(&self.batch),
            &repo.op_scope(),
        ))
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<OpBatch>> {
        let mut steps = EntrySteps::new(tx);
        for (applied, entry) in self.batch.entries.iter().rev().enumerate() {
            // `apply_undo_entry` drives the per-effect `EntrySteps` applier, which
            // wraps EACH write in its own `step` / `step_nonatomic`, so a failure
            // on the Nth write of the entry leaves the prior N-1 inverses on the
            // ledger and the rollback restores the exact pre-entry state (heddle#355
            // cid 3330966930). No outer per-entry `step` is needed — and a
            // whole-entry one would reintroduce the granularity bug it once
            // papered over.
            apply_undo_entry(&mut steps, entry)?;
            #[cfg(test)]
            if self.fail_after_entries == Some(applied + 1) {
                return Err(HeddleError::Conflict("injected mid-undo fault".to_string()));
            }
            #[cfg(not(test))]
            let _ = applied;
        }
        let updated = steps.mark_batch_undone(&self.batch)?;
        Ok(StagedCommit::pure(updated))
    }
}

impl DeferredMutation for ApplyUndoBatch {}

/// Deferred child: redo one batch — the mirror of [`ApplyUndoBatch`]. Entries
/// replay in forward order; `apply_redo_entry` registers a per-effect inverse for
/// each write, and the `mark_batch_redone` flip pairs with `mark_batch_undone`.
struct ApplyRedoBatch {
    batch: OpBatch,
    #[cfg(test)]
    fail_after_entries: Option<usize>,
}

impl ApplyRedoBatch {
    fn new(batch: OpBatch) -> Self {
        Self {
            batch,
            #[cfg(test)]
            fail_after_entries: None,
        }
    }

    #[cfg(test)]
    fn failing_after(batch: OpBatch, entries: usize) -> Self {
        Self {
            batch,
            fail_after_entries: Some(entries),
        }
    }
}

impl AtomicMutation for ApplyRedoBatch {
    type Output = OpBatch;

    fn transaction_id(&self) -> String {
        format!("redo:batch:{}", self.batch.id)
    }

    fn isolation_keys(&self, repo: &Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
        Ok(isolation_keys_for_batches(
            std::slice::from_ref(&self.batch),
            &repo.op_scope(),
        ))
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<OpBatch>> {
        let mut steps = EntrySteps::new(tx);
        for (applied, entry) in self.batch.entries.iter().enumerate() {
            // Mirror of `ApplyUndoBatch`: `apply_redo_entry` drives the per-effect
            // `EntrySteps` applier, so a mid-entry redo failure rolls back
            // per-write to the exact pre-entry state (heddle#355 cid 3330966931).
            apply_redo_entry(&mut steps, entry)?;
            #[cfg(test)]
            if self.fail_after_entries == Some(applied + 1) {
                return Err(HeddleError::Conflict("injected mid-redo fault".to_string()));
            }
            #[cfg(not(test))]
            let _ = applied;
        }
        let updated = steps.mark_batch_redone(&self.batch)?;
        Ok(StagedCommit::pure(updated))
    }
}

impl DeferredMutation for ApplyRedoBatch {}

fn isolation_keys_for_batches(batches: &[OpBatch], scope: &str) -> BTreeSet<IsolationKey> {
    let mut keys = BTreeSet::new();
    for batch in batches {
        for entry in &batch.entries {
            keys.extend(isolation_keys_for_record(
                &entry.operation,
                entry.scope.as_deref(),
            ));
        }
    }
    keys.insert(IsolationKey::LocalHead {
        scope: scope.to_string(),
    });
    keys
}

/// Root composite for `heddle undo`: stage the recovery pointer, then nest one
/// [`ApplyUndoBatch`] per batch. Returns the updated (undone) batches for the
/// command's output. Appends no domain record — the executor's commit marker
/// is the sole commit point.
pub(super) struct UndoOp {
    batches: Vec<OpBatch>,
    recovery_head: Option<ChangeId>,
    transaction_id: String,
}

impl UndoOp {
    pub(super) fn new(
        batches: Vec<OpBatch>,
        recovery_head: Option<ChangeId>,
        transaction_id: String,
    ) -> Self {
        Self {
            batches,
            recovery_head,
            transaction_id,
        }
    }
}

impl AtomicMutation for UndoOp {
    type Output = Vec<OpBatch>;

    fn transaction_id(&self) -> String {
        self.transaction_id.clone()
    }

    fn isolation_keys(&self, repo: &Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
        Ok(isolation_keys_for_batches(&self.batches, &repo.op_scope()))
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<Vec<OpBatch>>> {
        tx.enroll(StageUndoRecovery {
            head: self.recovery_head,
        })?;
        let mut updated = Vec::with_capacity(self.batches.len());
        for batch in &self.batches {
            let staged = tx.enroll(ApplyUndoBatch::new(batch.clone()))?;
            updated.push(staged.output);
        }
        Ok(StagedCommit::pure(updated))
    }
}

/// Root composite for `heddle undo --redo`: nest one [`ApplyRedoBatch`] per batch. No
/// recovery child (redo restores the pre-undo state the recovery pointer was
/// captured against).
pub(super) struct RedoOp {
    batches: Vec<OpBatch>,
    transaction_id: String,
}

impl RedoOp {
    pub(super) fn new(batches: Vec<OpBatch>, transaction_id: String) -> Self {
        Self {
            batches,
            transaction_id,
        }
    }
}

impl AtomicMutation for RedoOp {
    type Output = Vec<OpBatch>;

    fn transaction_id(&self) -> String {
        self.transaction_id.clone()
    }

    fn isolation_keys(&self, repo: &Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
        Ok(isolation_keys_for_batches(&self.batches, &repo.op_scope()))
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<Vec<OpBatch>>> {
        let mut updated = Vec::with_capacity(self.batches.len());
        for batch in &self.batches {
            let staged = tx.enroll(ApplyRedoBatch::new(batch.clone()))?;
            updated.push(staged.output);
        }
        Ok(StagedCommit::pure(updated))
    }
}

#[cfg(test)]
mod atomic_tests {
    use super::*;
    use oplog::ThreadUpdateSnapshots;
    use tempfile::TempDir;

    /// Init a repo and create two snapshots on `main`. The worktree at `s2`
    /// holds both `a.txt` (from `s1`) and `b.txt` (from `s2`); `s1` holds only
    /// `a.txt`; the initial state holds neither. Returns the repo + temp dir +
    /// the two states.
    fn repo_with_two_snapshots() -> (TempDir, Repository, ChangeId, ChangeId) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("a.txt"), "a").unwrap();
        let s1 = repo.snapshot(Some("s1".to_string()), None).unwrap();
        std::fs::write(temp.path().join("b.txt"), "b").unwrap();
        let s2 = repo.snapshot(Some("s2".to_string()), None).unwrap();
        (temp, repo, s1.change_id, s2.change_id)
    }

    #[test]
    fn apply_error_wraps_anyhow_into_conflict() {
        let wrapped = apply_error(anyhow!("boom"));
        assert!(
            matches!(&wrapped, HeddleError::Conflict(message) if message.contains("boom")),
            "an apply-helper error must surface as a HeddleError::Conflict carrying the message"
        );
    }

    fn commit_marker_count(repo: &Repository) -> usize {
        repo.oplog()
            .recent(256)
            .unwrap()
            .iter()
            .filter(|entry| matches!(entry.operation, OpRecord::TransactionCommit { .. }))
            .count()
    }

    fn commit_marker_count_for(repo: &Repository, txid: &str) -> usize {
        repo.oplog()
            .recent(256)
            .unwrap()
            .iter()
            .filter(|entry| {
                matches!(
                    &entry.operation,
                    OpRecord::TransactionCommit { transaction_id, .. } if transaction_id == txid
                )
            })
            .count()
    }

    fn main_thread(repo: &Repository) -> Option<ChangeId> {
        repo.refs().get_thread(&ThreadName::new("main")).unwrap()
    }

    /// Test-only parent mirroring [`UndoOp`] but injecting a fault: the LAST
    /// enrolled batch child fails after undoing `fail_after` of its entries.
    /// Reuses the REAL [`StageUndoRecovery`] + [`ApplyUndoBatch`] children, so
    /// it exercises the real compensators + nesting + rewind path.
    struct FaultyUndo {
        batches: Vec<OpBatch>,
        recovery_head: Option<ChangeId>,
        fail_after: usize,
    }

    impl AtomicMutation for FaultyUndo {
        type Output = ();

        fn transaction_id(&self) -> String {
            "test-undo-fault".to_string()
        }

        fn isolation_keys(&self, repo: &Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
            Ok(isolation_keys_for_batches(&self.batches, &repo.op_scope()))
        }

        fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<()>> {
            tx.enroll(StageUndoRecovery {
                head: self.recovery_head,
            })?;
            let last = self.batches.len() - 1;
            for (i, batch) in self.batches.iter().enumerate() {
                if i == last {
                    tx.enroll(ApplyUndoBatch::failing_after(
                        batch.clone(),
                        self.fail_after,
                    ))?;
                } else {
                    tx.enroll(ApplyUndoBatch::new(batch.clone()))?;
                }
            }
            Ok(StagedCommit::pure(()))
        }
    }

    /// Test-only parent mirroring [`RedoOp`] with an injected fault on the last
    /// enrolled batch child.
    struct FaultyRedo {
        batches: Vec<OpBatch>,
        fail_after: usize,
    }

    impl AtomicMutation for FaultyRedo {
        type Output = ();

        fn transaction_id(&self) -> String {
            "test-redo-fault".to_string()
        }

        fn isolation_keys(&self, repo: &Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
            Ok(isolation_keys_for_batches(&self.batches, &repo.op_scope()))
        }

        fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<()>> {
            let last = self.batches.len() - 1;
            for (i, batch) in self.batches.iter().enumerate() {
                if i == last {
                    tx.enroll(ApplyRedoBatch::failing_after(
                        batch.clone(),
                        self.fail_after,
                    ))?;
                } else {
                    tx.enroll(ApplyRedoBatch::new(batch.clone()))?;
                }
            }
            Ok(StagedCommit::pure(()))
        }
    }

    /// Behavioral parity: a clean atomic `UndoOp` reverts the worktree, HEAD,
    /// and thread ref, marks the batch undone, captures the recovery pointer,
    /// and commits exactly one marker — same observable result as the
    /// pre-migration sequential path.
    #[test]
    fn atomic_undo_success_reverts_and_records_recovery() {
        let (temp, repo, s1, s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();

        let recovery_head = repo.head().unwrap();
        let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
        let updated =
            repo::atomic::execute(&repo, UndoOp::new(batches, recovery_head, txid.clone()))
                .unwrap();

        assert_eq!(updated.len(), 1);
        assert!(updated[0].entries.iter().all(|e| e.undone));
        assert_eq!(repo.head().unwrap(), Some(s1), "HEAD reverted to s1");
        assert_eq!(main_thread(&repo), Some(s1));
        assert!(temp.path().join("a.txt").exists(), "s1 file kept");
        assert!(!temp.path().join("b.txt").exists(), "s2 file reverted");
        assert_eq!(
            repo.refs().get_undo_recovery().unwrap(),
            Some(s2),
            "recovery pointer pins the pre-undo tip"
        );
        assert_eq!(
            commit_marker_count_for(&repo, &txid),
            1,
            "exactly one undo commit marker"
        );
    }

    /// Fault-injection: a failure mid-undo (after the first batch is fully
    /// applied, partway into the second) rewinds EVERY applied step back to the
    /// exact pre-operation state — no partial ref / oplog / worktree leak.
    #[test]
    fn fault_mid_undo_rewinds_to_pre_operation_state() {
        let (temp, repo, _s1, s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();

        let pre_head = repo.head().unwrap();
        assert_eq!(pre_head, Some(s2));
        let pre_main = main_thread(&repo);
        assert_eq!(repo.refs().get_undo_recovery().unwrap(), None);
        let pre_markers = commit_marker_count(&repo);

        let batches = repo.oplog().undo_batches_scoped(2, Some(&scope)).unwrap();
        assert_eq!(batches.len(), 2, "two snapshots are undoable");
        let result = repo::atomic::execute(
            &repo,
            FaultyUndo {
                batches,
                recovery_head: pre_head,
                fail_after: 1,
            },
        );
        assert!(result.is_err(), "the injected fault must fail the undo");

        // Exact pre-operation state restored across every dimension.
        assert_eq!(
            repo.head().unwrap(),
            Some(s2),
            "HEAD rewound to pre-undo tip"
        );
        assert_eq!(main_thread(&repo), pre_main, "main ref rewound");
        assert!(temp.path().join("a.txt").exists(), "s1 file restored");
        assert!(temp.path().join("b.txt").exists(), "s2 file restored");
        assert_eq!(
            repo.oplog()
                .undo_batches_scoped(2, Some(&scope))
                .unwrap()
                .len(),
            2,
            "no batch left marked undone"
        );
        assert_eq!(
            repo.refs().get_undo_recovery().unwrap(),
            None,
            "recovery pointer cleared by rewind (it had no prior value)"
        );
        assert_eq!(
            commit_marker_count(&repo),
            pre_markers,
            "a failed transaction commits no marker"
        );
    }

    /// Fault-injection: a failure mid-redo rewinds the replay back to the
    /// fully-undone pre-redo state — no partial effect leaks.
    #[test]
    fn fault_mid_redo_rewinds_to_pre_operation_state() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("a.txt"), "a").unwrap();
        let _s1 = repo.snapshot(Some("s1".to_string()), None).unwrap();
        let scope = repo.op_scope();

        // Cleanly undo the single snapshot through the real atomic UndoOp.
        let recovery_head = repo.head().unwrap();
        let undo_batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &undo_batches);
        repo::atomic::execute(&repo, UndoOp::new(undo_batches, recovery_head, txid)).unwrap();

        // Pre-redo state: the initial (pre-s1) state — a.txt gone, one batch
        // redoable.
        assert!(!temp.path().join("a.txt").exists(), "undone: a.txt gone");
        let pre_redo_head = repo.head().unwrap();
        let pre_redo_main = main_thread(&repo);
        assert_eq!(
            repo.oplog()
                .redo_batches_scoped(1, Some(&scope))
                .unwrap()
                .len(),
            1,
            "one batch is redoable"
        );
        let pre_markers = commit_marker_count(&repo);

        let redo_batches = repo.oplog().redo_batches_scoped(1, Some(&scope)).unwrap();
        let result = repo::atomic::execute(
            &repo,
            FaultyRedo {
                batches: redo_batches,
                fail_after: 1,
            },
        );
        assert!(result.is_err(), "the injected fault must fail the redo");

        // Rewound to the fully-undone pre-redo state.
        assert_eq!(repo.head().unwrap(), pre_redo_head, "HEAD rewound");
        assert_eq!(main_thread(&repo), pre_redo_main, "main ref rewound");
        assert!(
            !temp.path().join("a.txt").exists(),
            "s1 file not resurrected"
        );
        assert_eq!(
            repo.oplog()
                .redo_batches_scoped(1, Some(&scope))
                .unwrap()
                .len(),
            1,
            "batch still redoable"
        );
        assert_eq!(
            commit_marker_count(&repo),
            pre_markers,
            "a failed transaction commits no marker"
        );
    }

    /// Per-effect rollback, UNDO direction (heddle#355 cid 3330966930). A threaded
    /// `Snapshot` undo performs several writes — `goto` (moves HEAD + worktree),
    /// then the thread-ref / HEAD / record updates. Injecting a failure on the
    /// SECOND write, after the goto already moved HEAD + worktree, must roll the
    /// goto back too, restoring the EXACT pre-entry state. Under the old
    /// whole-entry `step`, the goto leaked: a forward that failed partway had no
    /// inverse registered, leaving HEAD/worktree half-rewound.
    #[test]
    fn per_effect_rollback_threaded_snapshot_undo() {
        let (temp, repo, _s1, s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();

        let pre_head = repo.head().unwrap();
        assert_eq!(pre_head, Some(s2));
        let pre_main = main_thread(&repo);
        let pre_markers = commit_marker_count(&repo);
        assert_eq!(repo.refs().get_undo_recovery().unwrap(), None);

        let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
        // Fail the entry's 2nd per-effect write: the goto (write 1) succeeds and
        // moves HEAD/worktree, then the thread-ref update (write 2) errors.
        let result = with_entry_write_fault(1, || {
            repo::atomic::execute(&repo, UndoOp::new(batches, pre_head, txid))
        });
        assert!(
            result.is_err(),
            "the injected 2nd-write fault must fail the undo"
        );

        // The goto was rolled back along with everything else.
        assert_eq!(
            repo.head().unwrap(),
            Some(s2),
            "HEAD goto rolled back to the pre-undo tip"
        );
        assert_eq!(main_thread(&repo), pre_main, "main ref unchanged");
        assert!(temp.path().join("a.txt").exists(), "s1 file present");
        assert!(
            temp.path().join("b.txt").exists(),
            "s2 file restored by the goto rollback (the per-effect inverse ran)"
        );
        assert_eq!(
            repo.oplog()
                .undo_batches_scoped(1, Some(&scope))
                .unwrap()
                .len(),
            1,
            "no batch left marked undone"
        );
        assert_eq!(
            repo.refs().get_undo_recovery().unwrap(),
            None,
            "recovery pointer cleared by rewind"
        );
        assert_eq!(
            commit_marker_count(&repo),
            pre_markers,
            "no marker committed"
        );
    }

    /// Per-effect rollback, REDO direction (heddle#355 cid 3330966931). Mirror of
    /// the undo case: a threaded `Snapshot` redo's `goto` moves HEAD + worktree,
    /// then the 2nd write fails — the goto must roll back to the fully-undone
    /// pre-redo state, not leave the s2 worktree material resurrected. The
    /// `GitCheckpoint` redo Codex named is the same multi-write class, now routed
    /// through the identical per-effect `entry_step` machinery this exercises.
    #[test]
    fn per_effect_rollback_threaded_snapshot_redo() {
        let (temp, repo, s1, _s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();

        // Cleanly undo s2 so it becomes redoable; pre-redo state is the s1 tip.
        let recovery_head = repo.head().unwrap();
        let undo_batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &undo_batches);
        repo::atomic::execute(&repo, UndoOp::new(undo_batches, recovery_head, txid)).unwrap();
        assert_eq!(repo.head().unwrap(), Some(s1), "undone to s1");
        assert!(!temp.path().join("b.txt").exists(), "b.txt gone after undo");

        let pre_redo_head = repo.head().unwrap();
        let pre_redo_main = main_thread(&repo);
        let pre_markers = commit_marker_count(&repo);

        let redo_batches = repo.oplog().redo_batches_scoped(1, Some(&scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("redo", &scope, generation, &redo_batches);
        let result = with_entry_write_fault(1, || {
            repo::atomic::execute(&repo, RedoOp::new(redo_batches, txid))
        });
        assert!(
            result.is_err(),
            "the injected 2nd-write fault must fail the redo"
        );

        assert_eq!(
            repo.head().unwrap(),
            pre_redo_head,
            "HEAD goto rolled back to the pre-redo (fully-undone) state"
        );
        assert_eq!(main_thread(&repo), pre_redo_main, "main ref unchanged");
        assert!(temp.path().join("a.txt").exists(), "s1 file present");
        assert!(
            !temp.path().join("b.txt").exists(),
            "s2 file NOT resurrected — the goto's per-effect inverse rolled it back"
        );
        assert_eq!(
            repo.oplog()
                .redo_batches_scoped(1, Some(&scope))
                .unwrap()
                .len(),
            1,
            "batch still redoable"
        );
        assert_eq!(
            commit_marker_count(&repo),
            pre_markers,
            "no marker committed"
        );
    }

    /// Per-effect rollback of marker writes. Undoing a batch whose entries delete
    /// one marker (`mc`) and re-create another (`md`) registers a per-effect
    /// inverse for each; a later write failing must restore both markers to their
    /// exact pre-undo presence (`mc` back, `md` gone again).
    #[test]
    fn per_effect_rollback_restores_marker_writes() {
        let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();
        // `mc` exists (its MarkerCreate undo = delete_marker, inverse recreate);
        // `md` does not (its MarkerDelete undo = create_marker, inverse delete).
        repo.refs()
            .create_marker(&MarkerName::new("mc"), &s1)
            .unwrap();
        let main_state = main_thread(&repo).unwrap();

        repo.oplog()
            .record_batch_scoped(
                vec![
                    OpRecord::ThreadUpdate {
                        name: "main".to_string(),
                        old_state: main_state,
                        new_state: main_state,
                        manager_snapshots: None,
                    },
                    OpRecord::MarkerCreate {
                        name: "mc".to_string(),
                        state: s1,
                    },
                    OpRecord::MarkerDelete {
                        name: "md".to_string(),
                        state: s1,
                    },
                ],
                Some(&scope),
            )
            .unwrap();

        let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        let recovery_head = repo.head().unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
        // Undo order (entries.rev()): create `md` [w1], delete `mc` [w2], then the
        // ThreadUpdate undo's set_thread [w3] — trip at w3 so both marker inverses
        // are on the ledger.
        let result = with_entry_write_fault(2, || {
            repo::atomic::execute(&repo, UndoOp::new(batches, recovery_head, txid))
        });
        assert!(result.is_err(), "the injected fault must fail the undo");

        assert_eq!(
            repo.refs().get_marker(&MarkerName::new("mc")).unwrap(),
            Some(s1),
            "mc restored by the delete_marker inverse"
        );
        assert_eq!(
            repo.refs().get_marker(&MarkerName::new("md")).unwrap(),
            None,
            "md removed again by the create_marker inverse"
        );
    }

    /// Per-effect rollback of thread-ref writes. Undoing a batch that re-creates a
    /// deleted thread (`new`) and deletes a created thread (`old`) registers a
    /// per-effect inverse for each; a later write failing must restore both refs.
    #[test]
    fn per_effect_rollback_restores_thread_ref_writes() {
        let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();
        // `old` exists (its ThreadCreate undo = delete_thread, inverse re-set);
        // `new` does not (its ThreadDelete undo = set_thread, inverse delete).
        repo.refs()
            .set_thread(&ThreadName::new("old"), &s1)
            .unwrap();
        let main_state = main_thread(&repo).unwrap();

        repo.oplog()
            .record_batch_scoped(
                vec![
                    OpRecord::ThreadUpdate {
                        name: "main".to_string(),
                        old_state: main_state,
                        new_state: main_state,
                        manager_snapshots: None,
                    },
                    OpRecord::ThreadCreate {
                        name: "old".to_string(),
                        state: s1,
                        manager_snapshot: None,
                    },
                    OpRecord::ThreadDelete {
                        name: "new".to_string(),
                        state: s1,
                    },
                ],
                Some(&scope),
            )
            .unwrap();

        let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        let recovery_head = repo.head().unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
        // Undo order (entries.rev()): set `new` [w1], delete `old` [w2], then the
        // ThreadUpdate undo's set_thread [w3] — trip at w3.
        let result = with_entry_write_fault(2, || {
            repo::atomic::execute(&repo, UndoOp::new(batches, recovery_head, txid))
        });
        assert!(result.is_err(), "the injected fault must fail the undo");

        assert_eq!(
            repo.refs().get_thread(&ThreadName::new("old")).unwrap(),
            Some(s1),
            "old restored by the delete_thread inverse"
        );
        assert_eq!(
            repo.refs().get_thread(&ThreadName::new("new")).unwrap(),
            None,
            "new removed again by the set_thread inverse"
        );
    }

    /// A successful round trip via the atomic ops: undo then redo restores the
    /// original tip, and the marker-only commit batches are excluded from the
    /// undo/redo eligibility scans (so the round trip terminates instead of
    /// chasing its own commit sentinels).
    #[test]
    fn atomic_undo_redo_round_trip_ignores_commit_markers() {
        let (temp, repo, s1, s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();

        // Undo s2.
        let recovery_head = repo.head().unwrap();
        let undo_batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &undo_batches);
        repo::atomic::execute(&repo, UndoOp::new(undo_batches, recovery_head, txid)).unwrap();
        assert_eq!(repo.head().unwrap(), Some(s1));

        // The undo's commit marker is a record-less batch — not itself undoable.
        let still_undoable = repo.oplog().undo_batches_scoped(2, Some(&scope)).unwrap();
        assert_eq!(
            still_undoable.len(),
            1,
            "only the s1 snapshot remains undoable; the commit marker is excluded"
        );

        // Redo s2.
        let redo_batches = repo.oplog().redo_batches_scoped(1, Some(&scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("redo", &scope, generation, &redo_batches);
        repo::atomic::execute(&repo, RedoOp::new(redo_batches, txid)).unwrap();
        assert_eq!(repo.head().unwrap(), Some(s2), "redo restored the s2 tip");
        assert!(
            temp.path().join("b.txt").exists(),
            "s2 file restored by redo"
        );
    }

    /// The undo/redo serialization lock is mutually exclusive: while one holder
    /// has it, a second writer on the same lock file is blocked; once released it
    /// is acquirable again (heddle#355 cid 3330867776).
    #[test]
    fn undo_redo_lock_is_exclusive() {
        let (_temp, repo, _s1, _s2) = repo_with_two_snapshots();
        let lock_path = repo.heddle_dir().join("locks/undo-redo.lock");

        let guard = acquire_undo_redo_lock(&repo).unwrap();
        let contended = RepoLock::at(lock_path.clone()).try_write().unwrap();
        assert!(
            contended.is_none(),
            "a second writer must be blocked while the lock is held"
        );

        drop(guard);
        let reacquired = RepoLock::at(lock_path).try_write().unwrap();
        assert!(
            reacquired.is_some(),
            "the lock is acquirable again after the holder releases it"
        );
    }

    /// The serialized outcome the lock guarantees: a second undo invocation that
    /// (re-)selects its batches only AFTER the first has committed sees the
    /// already-undone batch and targets the PRECEDING op instead — it never
    /// re-selects the batch the first undid, so the two can't derive the same
    /// generation-keyed transaction id and the second can't dedup-hit and
    /// self-revert the first (heddle#355 cid 3330867776).
    #[test]
    fn serialized_second_undo_selects_a_different_batch() {
        let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();

        // Invocation 1, under the lock: undo the newest batch (s2).
        let first_ids: Vec<u64> = {
            let _lock = acquire_undo_redo_lock(&repo).unwrap();
            let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
            let ids = batches.iter().map(|b| b.id).collect();
            let recovery = repo.head().unwrap();
            let generation = repo.oplog().head_id().unwrap();
            let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
            repo::atomic::execute(&repo, UndoOp::new(batches, recovery, txid)).unwrap();
            ids
        };
        assert_eq!(repo.head().unwrap(), Some(s1), "first undo reverted to s1");

        // Invocation 2, under the lock (after 1 released + committed): the s2
        // batch is now undone, so re-selection returns the preceding s1 batch.
        let _lock = acquire_undo_redo_lock(&repo).unwrap();
        let second = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        let second_ids: Vec<u64> = second.iter().map(|b| b.id).collect();
        assert!(!second_ids.is_empty(), "a preceding op is still undoable");
        assert_ne!(
            second_ids, first_ids,
            "the serialized second undo must not re-select the batch the first already undid"
        );
    }

    /// `undo --list --depth N` returns N *user-facing* batches even when the
    /// newest batch is an undo/redo's record-less commit marker (heddle#355 cid
    /// 3330867777). After undoing s2, `recent_batches_scoped(1)` surfaces only
    /// the marker sentinel; `recent_user_batches_scoped(1)` skips it and returns
    /// the preceding real op (the s1 snapshot).
    #[test]
    fn list_depth_one_returns_preceding_user_op_past_commit_marker() {
        let (_temp, repo, _s1, _s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();

        // Undo s2 — this appends a marker-only `TransactionCommit` batch that is
        // now the newest batch in the log.
        let recovery_head = repo.head().unwrap();
        let undo_batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &undo_batches);
        repo::atomic::execute(&repo, UndoOp::new(undo_batches, recovery_head, txid)).unwrap();

        // The fixed-count fetch surfaces only the commit marker for depth 1...
        let raw = repo.oplog().recent_batches_scoped(1, Some(&scope)).unwrap();
        assert_eq!(raw.len(), 1);
        assert!(
            raw[0].is_transaction_marker_only(),
            "the newest batch is the undo's commit marker"
        );

        // ...while the user-facing query skips it and returns the real op.
        let user = repo
            .oplog()
            .recent_user_batches_scoped(1, Some(&scope))
            .unwrap();
        assert_eq!(
            user.len(),
            1,
            "depth 1 returns exactly one user-facing batch"
        );
        assert!(
            !user[0].is_transaction_marker_only(),
            "the returned batch is a real op, not the marker sentinel"
        );
        assert!(
            user[0]
                .entries
                .iter()
                .any(|e| matches!(e.operation, OpRecord::Snapshot { .. })),
            "it is the preceding s1 snapshot"
        );
    }

    /// Compensator class, undo direction (heddle#355 cid 3330867774). Undoing a
    /// `MarkerDelete` recreates the marker (`create_marker`). When that forward
    /// FAILS because a marker of the same name already exists (a pre-existing
    /// ref), the migration onto `Tx::step` guarantees NO `delete_marker` inverse
    /// was registered — so the rollback leaves the pre-existing marker intact.
    /// Pre-`step` (register-then-forward) the inverse ran on rollback and deleted
    /// the pre-existing marker.
    #[test]
    fn undo_marker_delete_forward_failure_keeps_preexisting_marker() {
        let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();
        let marker = MarkerName::new("keep");

        // A `MarkerDelete` batch becomes the newest undoable op; undoing it will
        // attempt `create_marker("keep", s1)`.
        repo.oplog()
            .record_batch_scoped(
                vec![OpRecord::MarkerDelete {
                    name: "keep".to_string(),
                    state: s1,
                }],
                Some(&scope),
            )
            .unwrap();

        // Plant a pre-existing marker of the same name — the undo's
        // `create_marker` will now collide and fail.
        repo.refs().create_marker(&marker, &s1).unwrap();

        let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        assert!(
            matches!(
                batches[0].entries[0].operation,
                OpRecord::MarkerDelete { .. }
            ),
            "the newest undoable batch is the MarkerDelete"
        );
        let recovery_head = repo.head().unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
        let result = repo::atomic::execute(&repo, UndoOp::new(batches, recovery_head, txid));

        assert!(
            result.is_err(),
            "the colliding create_marker must fail the undo"
        );
        assert_eq!(
            repo.refs().get_marker(&marker).unwrap(),
            Some(s1),
            "the pre-existing marker survives the rolled-back undo (no delete inverse ran)"
        );
    }

    /// Compensator class, redo direction (heddle#355 cid 3330867775). Redoing a
    /// `MarkerCreate` re-runs `create_marker`. A collision with a pre-existing
    /// marker must NOT delete it on rollback — the mirror of the undo case.
    #[test]
    fn redo_marker_create_forward_failure_keeps_preexisting_marker() {
        let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();
        let marker = MarkerName::new("keep");

        // Record a `MarkerCreate`, then mark it undone so it is REDOABLE; redoing
        // it will attempt `create_marker("keep", s1)`.
        repo.oplog()
            .record_batch_scoped(
                vec![OpRecord::MarkerCreate {
                    name: "keep".to_string(),
                    state: s1,
                }],
                Some(&scope),
            )
            .unwrap();
        let created = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        repo.oplog().mark_batch_undone(&created[0]).unwrap();

        // Plant the pre-existing colliding marker.
        repo.refs().create_marker(&marker, &s1).unwrap();

        let redo_batches = repo.oplog().redo_batches_scoped(1, Some(&scope)).unwrap();
        assert!(
            matches!(
                redo_batches[0].entries[0].operation,
                OpRecord::MarkerCreate { .. }
            ),
            "the redoable batch is the MarkerCreate"
        );
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("redo", &scope, generation, &redo_batches);
        let result = repo::atomic::execute(&repo, RedoOp::new(redo_batches, txid));

        assert!(
            result.is_err(),
            "the colliding create_marker must fail the redo"
        );
        assert_eq!(
            repo.refs().get_marker(&marker).unwrap(),
            Some(s1),
            "the pre-existing marker survives the rolled-back redo (no delete inverse ran)"
        );
    }

    // ---- step_nonatomic: forward-internal partial-failure rollback (r4 §A) ----

    /// `goto` is a NON-atomic forward (worktree materialize + HEAD write). When it
    /// applies its effect (moves HEAD + worktree) and then fails, the
    /// restore-to-snapshot inverse `step_nonatomic` registered BEFORE the forward
    /// must unwind it. A plain `step` would register NOTHING on the `Err` return
    /// and leak the moved HEAD/worktree (the hazard this combinator closes).
    #[test]
    fn step_nonatomic_rolls_back_partially_applied_goto() {
        let (temp, repo, _s1, s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();

        let pre_head = repo.head().unwrap();
        assert_eq!(pre_head, Some(s2));
        let pre_main = main_thread(&repo);
        let pre_markers = commit_marker_count(&repo);
        assert_eq!(repo.refs().get_undo_recovery().unwrap(), None);

        let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
        // The goto is the entry's FIRST non-atomic step: it runs (materializing
        // the worktree + moving HEAD) and then fails.
        let result = with_nonatomic_forward_fault(0, || {
            repo::atomic::execute(&repo, UndoOp::new(batches, pre_head, txid))
        });
        assert!(
            result.is_err(),
            "the injected partial-goto fault must fail the undo"
        );

        assert_eq!(
            repo.head().unwrap(),
            Some(s2),
            "HEAD restored to the pre-undo tip after a partially-applied goto"
        );
        assert_eq!(main_thread(&repo), pre_main, "main ref unchanged");
        assert!(temp.path().join("a.txt").exists(), "s1 file present");
        assert!(
            temp.path().join("b.txt").exists(),
            "s2 worktree material restored by the goto's restore-before inverse"
        );
        assert_eq!(
            repo.refs().get_undo_recovery().unwrap(),
            None,
            "recovery pointer cleared by rewind"
        );
        assert_eq!(
            commit_marker_count(&repo),
            pre_markers,
            "no marker committed"
        );
    }

    fn sample_main_thread(current_state: &str, materialized: &str) -> Thread {
        Thread {
            id: "thread-main".to_string(),
            thread: "main".to_string(),
            target_thread: None,
            parent_thread: None,
            mode: repo::ThreadMode::Solid,
            state: ThreadState::Active,
            base_state: "base".to_string(),
            base_root: "root".to_string(),
            current_state: Some(current_state.to_string()),
            merged_state: None,
            task: None,
            execution_path: PathBuf::from("/work/exec"),
            materialized_path: Some(PathBuf::from(materialized)),
            changed_paths: vec![],
            impact_categories: vec![],
            heavy_impact_paths: vec![],
            promotion_suggested: false,
            freshness: ThreadFreshness::Current,
            verification_summary: repo::ThreadVerificationSummary::default(),
            confidence_summary: repo::ThreadConfidenceSummary::default(),
            integration_policy_result: ThreadIntegrationPolicy::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            ephemeral: None,
            auto: false,
            shared_target_dir: None,
        }
    }

    fn encode_thread_record_set(manager: &ThreadManager, records: &[Thread]) -> Vec<Vec<u8>> {
        records
            .iter()
            .map(|record| manager.encode_thread_record_snapshot(record).unwrap())
            .collect()
    }

    fn apply_undo_once(repo: &Repository, scope: &str) {
        let batches = repo.oplog().undo_batches_scoped(1, Some(scope)).unwrap();
        let recovery_head = repo.head().unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", scope, generation, &batches);
        repo::atomic::execute(repo, UndoOp::new(batches, recovery_head, txid)).unwrap();
    }

    fn apply_redo_once(repo: &Repository, scope: &str) {
        let batches = repo.oplog().redo_batches_scoped(1, Some(scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("redo", scope, generation, &batches);
        repo::atomic::execute(repo, RedoOp::new(batches, txid)).unwrap();
    }

    #[test]
    fn thread_update_undo_preserves_missing_ref_fallback_absence() {
        let (_temp, repo, s1, s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();
        let manager = ThreadManager::new(repo.heddle_dir());
        let mut old_record = sample_main_thread(&s1.short(), "/work/missing-ref");
        old_record.id = "missing-ref".to_string();
        old_record.thread = "missing-ref".to_string();
        old_record.base_state = s1.short();
        old_record.current_state = Some(s1.short());
        let mut new_record = old_record.clone();
        new_record.current_state = Some(s2.short());
        new_record.updated_at = old_record.updated_at + chrono::Duration::seconds(1);
        manager.save(&new_record).unwrap();
        repo.refs()
            .delete_thread(&ThreadName::new("missing-ref"))
            .unwrap();

        repo.oplog()
            .record_batch_scoped(
                vec![OpRecord::ThreadUpdate {
                    name: "missing-ref".to_string(),
                    old_state: s1,
                    new_state: s2,
                    manager_snapshots: ThreadUpdateSnapshots::from_record_sets(
                        Some(manager.encode_thread_record_snapshot(&old_record).unwrap()),
                        Some(manager.encode_thread_record_snapshot(&new_record).unwrap()),
                        encode_thread_record_set(&manager, std::slice::from_ref(&old_record)),
                        encode_thread_record_set(&manager, std::slice::from_ref(&new_record)),
                        true,
                    ),
                }],
                Some(&scope),
            )
            .unwrap();

        apply_undo_once(&repo, &scope);
        assert_eq!(
            repo.refs()
                .get_thread(&ThreadName::new("missing-ref"))
                .unwrap(),
            None,
            "undo restores the pre-update absence instead of fabricating a ref"
        );
        assert_eq!(
            manager
                .find_by_thread("missing-ref")
                .unwrap()
                .unwrap()
                .current_state
                .as_deref(),
            Some(s1.short().as_str()),
            "undo restores the old ThreadManager record"
        );

        apply_redo_once(&repo, &scope);
        assert_eq!(
            repo.refs()
                .get_thread(&ThreadName::new("missing-ref"))
                .unwrap(),
            Some(s2),
            "redo recreates the post-update thread ref"
        );
        assert_eq!(
            manager
                .find_by_thread("missing-ref")
                .unwrap()
                .unwrap()
                .current_state
                .as_deref(),
            Some(s2.short().as_str()),
            "redo restores the new ThreadManager record"
        );
    }

    #[test]
    fn thread_update_undo_redo_restores_duplicate_same_name_record_sets() {
        let (_temp, repo, s1, s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();
        let manager = ThreadManager::new(repo.heddle_dir());
        let mut winner_old = sample_main_thread(&s1.short(), "/work/winner-old");
        winner_old.id = "rec-winner".to_string();
        winner_old.updated_at = chrono::Utc::now();
        let mut duplicate = sample_main_thread(&s1.short(), "/work/duplicate");
        duplicate.id = "rec-duplicate".to_string();
        duplicate.updated_at = winner_old.updated_at - chrono::Duration::seconds(30);
        let mut winner_new = winner_old.clone();
        winner_new.current_state = Some(s2.short());
        winner_new.materialized_path = Some(PathBuf::from("/work/winner-new"));
        winner_new.updated_at = winner_old.updated_at + chrono::Duration::seconds(30);
        manager.save(&winner_new).unwrap();
        manager.save(&duplicate).unwrap();
        repo.refs()
            .set_thread(&ThreadName::new("main"), &s2)
            .unwrap();

        let old_records = vec![winner_old.clone(), duplicate.clone()];
        let new_records = vec![winner_new.clone(), duplicate.clone()];
        repo.oplog()
            .record_batch_scoped(
                vec![OpRecord::ThreadUpdate {
                    name: "main".to_string(),
                    old_state: s1,
                    new_state: s2,
                    manager_snapshots: ThreadUpdateSnapshots::from_record_sets(
                        Some(manager.encode_thread_record_snapshot(&winner_old).unwrap()),
                        Some(manager.encode_thread_record_snapshot(&winner_new).unwrap()),
                        encode_thread_record_set(&manager, &old_records),
                        encode_thread_record_set(&manager, &new_records),
                        false,
                    ),
                }],
                Some(&scope),
            )
            .unwrap();

        apply_undo_once(&repo, &scope);
        let undone = manager.snapshot_records("main").unwrap();
        let undone_ids: std::collections::HashSet<_> =
            undone.iter().map(|record| record.id.as_str()).collect();
        assert_eq!(
            undone_ids,
            std::collections::HashSet::from(["rec-winner", "rec-duplicate"]),
            "undo preserves every same-name record"
        );
        assert_eq!(
            manager
                .load("rec-winner")
                .unwrap()
                .unwrap()
                .current_state
                .as_deref(),
            Some(s1.short().as_str()),
            "undo restores the winner's old body"
        );
        assert_eq!(
            manager
                .load("rec-duplicate")
                .unwrap()
                .unwrap()
                .materialized_path,
            Some(PathBuf::from("/work/duplicate")),
            "undo keeps the non-winner duplicate worktree metadata"
        );

        apply_redo_once(&repo, &scope);
        let redone = manager.snapshot_records("main").unwrap();
        let redone_ids: std::collections::HashSet<_> =
            redone.iter().map(|record| record.id.as_str()).collect();
        assert_eq!(
            redone_ids,
            std::collections::HashSet::from(["rec-winner", "rec-duplicate"]),
            "redo preserves every same-name record"
        );
        assert_eq!(
            manager
                .load("rec-winner")
                .unwrap()
                .unwrap()
                .current_state
                .as_deref(),
            Some(s2.short().as_str()),
            "redo restores the winner's new body"
        );
        assert_eq!(
            manager
                .load("rec-duplicate")
                .unwrap()
                .unwrap()
                .materialized_path,
            Some(PathBuf::from("/work/duplicate")),
            "redo keeps the non-winner duplicate worktree metadata"
        );
    }

    /// Test-only deferred mutation that saves ONE thread record through the
    /// `EntrySteps` applier — exercises the real `save_thread_record`
    /// (`step_nonatomic`) capture-restore path.
    struct SaveOnly {
        record: Thread,
    }

    impl AtomicMutation for SaveOnly {
        type Output = ();

        fn transaction_id(&self) -> String {
            "test-save-only".to_string()
        }

        fn isolation_keys(&self, _repo: &Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
            let mut keys = BTreeSet::new();
            keys.insert(IsolationKey::Thread(self.record.thread.clone()));
            Ok(keys)
        }

        fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<()>> {
            let mut steps = EntrySteps::new(tx);
            steps.save_thread_record(self.record.clone())?;
            Ok(StagedCommit::pure(()))
        }
    }

    impl DeferredMutation for SaveOnly {}

    /// `ThreadManager::save` is a NON-atomic forward — it writes the record file
    /// AND the workspace file. When the save applies (both halves) and then a
    /// failure occurs, the `step_nonatomic` capture-restore must rewrite BOTH
    /// halves back to the prior record. A plain `step` would leak the saved
    /// record/workspace on the `Err` return.
    #[test]
    fn step_nonatomic_restores_record_and_workspace_on_save_failure() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let manager = ThreadManager::new(repo.heddle_dir());

        // R0: the prior persisted record (record half: current_state; workspace
        // half: materialized_path).
        let r0 = sample_main_thread("current-A", "/work/A");
        manager.save(&r0).unwrap();

        // R1: what the (faulted) save writes — different in BOTH halves.
        let mut r1 = r0.clone();
        r1.current_state = Some("current-B".to_string());
        r1.materialized_path = Some(PathBuf::from("/work/B"));

        let result = with_nonatomic_forward_fault(0, || {
            repo::atomic::execute(&repo, SaveOnly { record: r1 })
        });
        assert!(result.is_err(), "the injected save fault must fail the op");

        let restored = manager.find_by_thread("main").unwrap().unwrap();
        assert_eq!(
            restored.current_state.as_deref(),
            Some("current-A"),
            "the record half (current_state) was restored to R0"
        );
        assert_eq!(
            restored.materialized_path,
            Some(PathBuf::from("/work/A")),
            "the workspace half (materialized_path) was restored to R0"
        );
    }

    /// A "replacement save" persists the thread under a NEW record id (the prior
    /// record had a different id). `find_by_thread` selects among ALL records with
    /// that thread name, so if a later failure rolls the save back, the restore
    /// must delete the newly-written `new_id` record + its workspace file — not
    /// just re-save `prev`. Otherwise the leaked newer record stays visible and
    /// record-backed commands observe the rolled-back-away state. A re-save-only
    /// restore leaves two records for "main" and `find_by_thread` returns the leak.
    #[test]
    fn step_nonatomic_restores_replacement_save_deleting_leaked_new_record() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let manager = ThreadManager::new(repo.heddle_dir());

        // R0: the prior persisted record for thread "main".
        let mut r0 = sample_main_thread("current-A", "/work/A");
        r0.id = "thread-main-v1".to_string();
        r0.updated_at = chrono::Utc::now();
        manager.save(&r0).unwrap();

        // R1: the replacement save — SAME thread, DIFFERENT record id, and a later
        // `updated_at` so a leaked R1 would win `find_by_thread`'s max-by-updated.
        let mut r1 = r0.clone();
        r1.id = "thread-main-v2".to_string();
        r1.current_state = Some("current-B".to_string());
        r1.updated_at = r0.updated_at + chrono::Duration::seconds(60);

        let result = with_nonatomic_forward_fault(0, || {
            repo::atomic::execute(&repo, SaveOnly { record: r1 })
        });
        assert!(result.is_err(), "the injected save fault must fail the op");

        assert!(
            manager.load("thread-main-v2").unwrap().is_none(),
            "the leaked new_id record must be deleted on rollback"
        );
        let remaining = manager.list().unwrap();
        assert_eq!(
            remaining.len(),
            1,
            "only the prior record survives for the thread, no leaked newer record"
        );
        let restored = manager.find_by_thread("main").unwrap().unwrap();
        assert_eq!(
            restored.id, "thread-main-v1",
            "find_by_thread returns ONLY prev"
        );
        assert_eq!(restored.current_state.as_deref(), Some("current-A"));
    }

    /// A "create save" persists a thread with NO prior record. On rollback the
    /// restore must delete the created record + its workspace file so nothing is
    /// left for the thread.
    #[test]
    fn step_nonatomic_create_save_rollback_removes_created_record() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let manager = ThreadManager::new(repo.heddle_dir());

        let mut created = sample_main_thread("current-A", "/work/A");
        created.id = "thread-main-new".to_string();

        let result = with_nonatomic_forward_fault(0, || {
            repo::atomic::execute(&repo, SaveOnly { record: created })
        });
        assert!(result.is_err(), "the injected save fault must fail the op");

        assert!(
            manager.load("thread-main-new").unwrap().is_none(),
            "the created record must be removed on rollback"
        );
        assert!(
            manager.find_by_thread("main").unwrap().is_none(),
            "no record survives for a rolled-back create save"
        );
    }

    /// Test-only deferred mutation that restores ONE thread record from a redo
    /// snapshot through the `EntrySteps` applier — exercises the real
    /// `restore_thread_record` (`step_nonatomic`) capture-restore path, the redo
    /// arm whose forward writes a record under a snapshot-buried id.
    struct RestoreSnapshotOnly {
        name: String,
        bytes: Vec<u8>,
    }

    impl AtomicMutation for RestoreSnapshotOnly {
        type Output = ();

        fn transaction_id(&self) -> String {
            "test-restore-snapshot-only".to_string()
        }

        fn isolation_keys(&self, _repo: &Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
            let mut keys = BTreeSet::new();
            keys.insert(IsolationKey::Thread(self.name.clone()));
            Ok(keys)
        }

        fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<()>> {
            let mut steps = EntrySteps::new(tx);
            steps.restore_thread_record(&self.name, &self.bytes, "ThreadCreate")?;
            Ok(StagedCommit::pure(()))
        }
    }

    impl DeferredMutation for RestoreSnapshotOnly {}

    /// The redo-snapshot sibling of `..._restores_replacement_save_...`: the redo
    /// of a `ThreadCreate` restores the record from an opaque snapshot whose
    /// record id is NOT known to the applier. The forward writes that snapshot-id
    /// record (newer timestamp); on rollback the converge must drop it so
    /// `find_by_thread` returns ONLY the prior record. A re-save-only restore
    /// (the pre-r6 redo arm) left the snapshot-id record and `find_by_thread`
    /// returned the leak — this test fails against that arm and passes against the
    /// `converge_records` restore.
    #[test]
    fn step_nonatomic_restores_redo_snapshot_deleting_leaked_record() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let manager = ThreadManager::new(repo.heddle_dir());

        // R0: the prior persisted record for thread "main".
        let mut r0 = sample_main_thread("current-A", "/work/A");
        r0.id = "thread-main-v1".to_string();
        r0.updated_at = chrono::Utc::now();
        manager.save(&r0).unwrap();

        // Build a redo snapshot of a DIFFERENT-id, NEWER record (what the redo
        // forward writes). Save it, snapshot it, then remove it so only the prior
        // record remains at capture time.
        let mut snap_rec = r0.clone();
        snap_rec.id = "thread-main-v2".to_string();
        snap_rec.current_state = Some("current-B".to_string());
        snap_rec.updated_at = r0.updated_at + chrono::Duration::seconds(60);
        manager.save(&snap_rec).unwrap();
        let snapshot = manager.snapshot_thread_record("main").unwrap().unwrap();
        manager.delete("thread-main-v2").unwrap();
        assert_eq!(
            manager.list().unwrap().len(),
            1,
            "precondition: only the prior record exists at capture time"
        );

        let result = with_nonatomic_forward_fault(0, || {
            repo::atomic::execute(
                &repo,
                RestoreSnapshotOnly {
                    name: "main".to_string(),
                    bytes: snapshot,
                },
            )
        });
        assert!(
            result.is_err(),
            "the injected restore fault must fail the op"
        );

        assert!(
            manager.load("thread-main-v2").unwrap().is_none(),
            "the leaked snapshot-id record must be deleted on rollback"
        );
        assert_eq!(
            manager.list().unwrap().len(),
            1,
            "only the prior record survives, no leaked newer record"
        );
        let restored = manager.find_by_thread("main").unwrap().unwrap();
        assert_eq!(
            restored.id, "thread-main-v1",
            "find_by_thread returns ONLY prev"
        );
        assert_eq!(restored.current_state.as_deref(), Some("current-A"));
    }

    /// SUCCESS-path postcondition of the redo `ThreadCreate` restore (cid
    /// 3331603135): when a pre-existing DUPLICATE is already filed under the name,
    /// redoing the create restores the snapshot AND leaves ONLY the restored
    /// record — the success path has the same single-record postcondition as the
    /// rollback converge. The pre-r8 arm `save`d the decoded record without
    /// removing the duplicate, so two records survived and `find_by_thread`
    /// (max-by-updated) returned the newer duplicate, not the restored record —
    /// this test fails against that arm and passes against decode→converge.
    #[test]
    fn redo_restore_thread_record_converges_away_preexisting_duplicate() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let manager = ThreadManager::new(repo.heddle_dir());

        // The record the redo snapshot will restore (older timestamp).
        let mut to_restore = sample_main_thread("current-restored", "/work/R");
        to_restore.id = "rec-restored".to_string();
        to_restore.updated_at = chrono::Utc::now();
        manager.save(&to_restore).unwrap();
        let snapshot = manager.snapshot_thread_record("main").unwrap().unwrap();
        manager.delete("rec-restored").unwrap();

        // A pre-existing DUPLICATE under the same name with a NEWER timestamp, so
        // a raw-save redo would leave it winning `find_by_thread`.
        let mut dup = sample_main_thread("current-dup", "/work/D");
        dup.id = "rec-dup".to_string();
        dup.updated_at = to_restore.updated_at + chrono::Duration::seconds(60);
        manager.save(&dup).unwrap();
        assert_eq!(
            manager
                .list()
                .unwrap()
                .iter()
                .filter(|t| t.thread == "main")
                .count(),
            1,
            "precondition: only the duplicate is filed at redo time"
        );

        // Redo restores the snapshot — SUCCESS path (no fault).
        repo::atomic::execute(
            &repo,
            RestoreSnapshotOnly {
                name: "main".to_string(),
                bytes: snapshot,
            },
        )
        .unwrap();

        let under_name: Vec<_> = manager
            .list()
            .unwrap()
            .into_iter()
            .filter(|t| t.thread == "main")
            .collect();
        assert_eq!(
            under_name.len(),
            1,
            "ONLY the restored record remains — the duplicate is converged away"
        );
        assert_eq!(under_name[0].id, "rec-restored");
        assert_eq!(
            manager.find_by_thread("main").unwrap().unwrap().id,
            "rec-restored",
            "find_by_thread returns the restored record, not the leaked duplicate"
        );
        assert!(
            manager.load("rec-dup").unwrap().is_none(),
            "the pre-existing duplicate record file is gone"
        );
    }

    /// Test-only deferred mutation that runs `remove_thread_manager_record` — the
    /// `ThreadCreate` inverse — through the `EntrySteps` applier, so its single
    /// lock-atomic `converge_records`-to-empty step and the converge-back-to-prior
    /// rollback can be exercised in isolation.
    struct RemoveRecordOnly {
        name: String,
    }

    impl AtomicMutation for RemoveRecordOnly {
        type Output = ();

        fn transaction_id(&self) -> String {
            "test-remove-record-only".to_string()
        }

        fn isolation_keys(&self, _repo: &Repository) -> HeddleResult<BTreeSet<IsolationKey>> {
            let mut keys = BTreeSet::new();
            keys.insert(IsolationKey::Thread(self.name.clone()));
            Ok(keys)
        }

        fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<()>> {
            let mut steps = EntrySteps::new(tx);
            remove_thread_manager_record(&mut steps, &self.name)?;
            Ok(StagedCommit::pure(()))
        }
    }

    impl DeferredMutation for RemoveRecordOnly {}

    /// The created-thread inverse converges the name to EMPTY: when the store holds
    /// MULTIPLE records under the name (the duplicate class the converge tolerates),
    /// undoing the `ThreadCreate` must drop EVERY same-name record, not just the
    /// `find_by_thread` winner. The pre-fix arm deleted only the winner, leaving the
    /// older duplicate as a phantom whose thread ref is gone — this test fails
    /// against that arm (the older record survives) and passes against converge-to-
    /// empty.
    #[test]
    fn remove_thread_manager_record_converges_name_to_empty() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let manager = ThreadManager::new(repo.heddle_dir());

        // Two records under "main": a winner (newer) + an older duplicate.
        let mut winner = sample_main_thread("current-A", "/work/A");
        winner.id = "rec-winner".to_string();
        winner.updated_at = chrono::Utc::now();
        manager.save(&winner).unwrap();
        let mut older = sample_main_thread("current-B", "/work/B");
        older.id = "rec-older".to_string();
        older.updated_at = winner.updated_at - chrono::Duration::seconds(60);
        manager.save(&older).unwrap();
        assert_eq!(
            manager.list().unwrap().len(),
            2,
            "precondition: two records"
        );

        repo::atomic::execute(
            &repo,
            RemoveRecordOnly {
                name: "main".to_string(),
            },
        )
        .unwrap();

        assert!(
            manager.find_by_thread("main").unwrap().is_none(),
            "converge-to-empty: no record survives under the name"
        );
        assert!(
            manager.list().unwrap().iter().all(|t| t.thread != "main"),
            "EVERY same-name record removed, not just the find_by_thread winner"
        );
    }

    /// Rollback of the converge-to-empty inverse: the single `step_nonatomic`
    /// converge runs its forward (deleting BOTH same-name records under one write
    /// lock) and then fails — the converge-back-to-prior inverse, registered
    /// BEFORE the forward, must restore the FULL captured prior set (both
    /// records), not just the `find_by_thread` winner. Arming the fault at the
    /// converge step (index 0) proves the whole-set capture-restore reverses a
    /// lock-atomic all-or-nothing forward in one inverse.
    #[test]
    fn remove_thread_manager_record_rollback_resaves_all_records() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let manager = ThreadManager::new(repo.heddle_dir());

        let mut winner = sample_main_thread("current-A", "/work/A");
        winner.id = "rec-winner".to_string();
        winner.updated_at = chrono::Utc::now();
        manager.save(&winner).unwrap();
        let mut older = sample_main_thread("current-B", "/work/B");
        older.id = "rec-older".to_string();
        older.updated_at = winner.updated_at - chrono::Duration::seconds(60);
        manager.save(&older).unwrap();

        // Fault the converge forward: it empties the name (both records deleted
        // under one lock), then the op fails — the inverse must re-converge to the
        // full captured prior set, restoring BOTH records.
        let result = with_nonatomic_forward_fault(0, || {
            repo::atomic::execute(
                &repo,
                RemoveRecordOnly {
                    name: "main".to_string(),
                },
            )
        });
        assert!(
            result.is_err(),
            "the injected forward fault must fail the op"
        );

        let remaining = manager.list().unwrap();
        assert_eq!(
            remaining.len(),
            2,
            "rollback re-converged to ALL same-name records, not just the winner"
        );
        let ids: std::collections::HashSet<_> = remaining.iter().map(|t| t.id.clone()).collect();
        assert!(
            ids.contains("rec-winner") && ids.contains("rec-older"),
            "both the winner and the older duplicate were restored"
        );
        assert_eq!(
            manager.find_by_thread("main").unwrap().unwrap().id,
            "rec-winner",
            "find_by_thread still selects the newer winner after rollback"
        );
    }

    /// Undoing a `Redact` removes the per-blob sidecar (re-exposing the blob). If
    /// a LATER batch in the same undo transaction fails, the `step_nonatomic`
    /// capture-restore — registered BEFORE the removal — must restore the sidecar
    /// so the redacted blob is NOT re-exposed. The pre-migration unregistered
    /// removal left the blob exposed on a rolled-back undo. (`--allow-redact-undo`
    /// gates this at the command level; the apply path is exercised directly.)
    #[test]
    fn step_nonatomic_restores_redaction_sidecar_when_a_later_batch_fails() {
        use objects::object::{Principal, Redaction};

        let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();
        let main_state = main_thread(&repo).unwrap();

        // A real redaction on disk.
        let blob = ContentHash::from_bytes([7u8; 32]);
        let redaction = Redaction {
            redacted_blob: blob,
            state: s1,
            path: "config/secrets.toml".to_string(),
            reason: "leaked credential".to_string(),
            redactor: Principal {
                name: "Grace Hopper".to_string(),
                email: "grace@example.com".to_string(),
            },
            redacted_at: chrono::Utc::now(),
            signature: None,
            purged_at: None,
            supersedes: None,
        };
        let redaction_id = repo.put_redaction(redaction).unwrap();
        assert_eq!(
            repo.get_redactions_for_blob(&blob)
                .unwrap()
                .redactions
                .len(),
            1,
            "redaction planted on disk"
        );

        // Older batch (a no-op thread update) recorded first; newer Redact batch
        // recorded second, so the Redact is undone FIRST and the older batch —
        // which the injected fault fails — is undone after it.
        repo.oplog()
            .record_batch_scoped(
                vec![OpRecord::ThreadUpdate {
                    name: "main".to_string(),
                    old_state: main_state,
                    new_state: main_state,
                    manager_snapshots: None,
                }],
                Some(&scope),
            )
            .unwrap();
        repo.oplog()
            .record_batch_scoped(
                vec![OpRecord::Redact {
                    redaction_id,
                    blob,
                    state: s1,
                    path: "config/secrets.toml".to_string(),
                }],
                Some(&scope),
            )
            .unwrap();

        let batches = repo.oplog().undo_batches_scoped(2, Some(&scope)).unwrap();
        assert_eq!(batches.len(), 2);
        assert!(
            matches!(batches[0].entries[0].operation, OpRecord::Redact { .. }),
            "the newest undoable batch is the Redact (undone first)"
        );

        let recovery_head = repo.head().unwrap();
        // FaultyUndo fails the LAST enrolled batch (the older ThreadUpdate) after
        // its first entry — by then the Redact's sidecar removal already ran.
        let result = repo::atomic::execute(
            &repo,
            FaultyUndo {
                batches,
                recovery_head,
                fail_after: 1,
            },
        );
        assert!(
            result.is_err(),
            "the injected fault on the later batch must fail the undo"
        );

        let restored = repo.get_redactions_for_blob(&blob).unwrap();
        assert_eq!(
            restored.redactions.len(),
            1,
            "redaction sidecar restored by the rollback — the blob is NOT re-exposed"
        );
        assert!(
            repo.get_redaction(&redaction_id).unwrap().is_some(),
            "the exact redaction record is back on disk"
        );
    }

    /// Build a `StateVisibility` record for an existing state with an explicit
    /// timestamp (so distinct records on the same state get distinct content
    /// hashes and accrete rather than dedup).
    fn visibility_record(
        state: ChangeId,
        tier: objects::object::VisibilityTier,
        ts: i64,
    ) -> objects::object::StateVisibility {
        objects::object::StateVisibility {
            state,
            tier,
            embargo_until: None,
            declarer: objects::object::Principal {
                name: "Grace Hopper".to_string(),
                email: "grace@example.com".to_string(),
            },
            declared_at: chrono::DateTime::from_timestamp(ts, 0).unwrap(),
            signature: None,
            supersedes: None,
        }
    }

    /// heddle#317 r7 — the undo/redo restore must be serialized with a concurrent
    /// `visibility set`/`promote` so it can never clobber a newer committed
    /// record. A visibility set A on state S is committed and selected for undo;
    /// then a concurrent set C commits on S (through the same locked transaction),
    /// superseding A. Running the undo of A drives its restore through the repo
    /// write lock, which re-checks the current sidecar: C no longer matches A's
    /// recorded after-image, so the undo ABORTS instead of restoring A's stale
    /// before-image over C. C survives.
    #[test]
    fn concurrent_set_during_undo_is_not_clobbered() {
        use objects::object::VisibilityTier;

        let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();
        let state = s1;

        // Commit visibility A on the state — the op the undo will target.
        repo.commit_state_visibility(
            visibility_record(state, VisibilityTier::Internal, 1_700_000_000),
            repo::VisibilityCommitKind::Set,
        )
        .expect("commit A")
        .expect("a set always commits");

        // Select A's undo batch (its StateVisibilitySet op) BEFORE C lands.
        let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        assert!(
            batches[0]
                .entries
                .iter()
                .any(|e| matches!(e.operation, OpRecord::StateVisibilitySet { .. })),
            "the newest undoable batch is the visibility set"
        );

        // A concurrent `visibility set` C commits FIRST (through the locked
        // transaction), superseding A on disk.
        repo.commit_state_visibility(
            visibility_record(
                state,
                VisibilityTier::TeamScoped {
                    team_id: "infra".to_string(),
                },
                1_700_000_060,
            ),
            repo::VisibilityCommitKind::Set,
        )
        .expect("commit C")
        .expect("a set always commits");
        let after_c = repo
            .get_state_visibility_bytes_for_state(&state)
            .expect("read sidecar after C");
        assert!(after_c.is_some(), "C is on disk");

        // Undo A: the restore takes the repo write lock, re-checks, sees C
        // superseded A's after-image, and aborts rather than clobbering C.
        let recovery = repo.head().unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
        let result = repo::atomic::execute(&repo, UndoOp::new(batches, recovery, txid));
        assert!(
            result.is_err(),
            "the undo must abort on the superseding concurrent visibility commit"
        );

        // C survives untouched — the undo did NOT restore A's stale before-image.
        assert_eq!(
            repo.get_state_visibility_bytes_for_state(&state).unwrap(),
            after_c,
            "the newer concurrent visibility record C must survive the aborted undo"
        );
        assert!(
            repo.has_visibility_for_state(&state).unwrap(),
            "the state stays non-public (C's tier), not dropped to public-by-absence"
        );
    }

    /// heddle#317 r7 — with NO concurrent writer, an undo→redo of a visibility op
    /// still round-trips through the locked, conflict-rechecked restore: undo
    /// drops the state back to public-by-absence and redo restores exactly the
    /// op's after-image. Guards against the lock/re-check regressing normal
    /// undo/redo.
    #[test]
    fn undo_redo_visibility_roundtrip_still_works() {
        use objects::object::VisibilityTier;

        let (_temp, repo, s1, _s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();
        let state = s1;
        assert!(
            !repo.has_visibility_for_state(&state).unwrap(),
            "the state starts public-by-absence"
        );

        // Commit visibility A.
        repo.commit_state_visibility(
            visibility_record(state, VisibilityTier::Internal, 1_700_000_000),
            repo::VisibilityCommitKind::Set,
        )
        .expect("commit A")
        .expect("a set always commits");
        let after_set = repo
            .get_state_visibility_bytes_for_state(&state)
            .expect("read A");
        assert!(after_set.is_some(), "A is on disk");

        // Undo A: the sidecar drops back to public-by-absence.
        let recovery = repo.head().unwrap();
        let undo_batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &undo_batches);
        repo::atomic::execute(&repo, UndoOp::new(undo_batches, recovery, txid))
            .expect("undo succeeds with no concurrent writer");
        assert!(
            !repo.has_visibility_for_state(&state).unwrap(),
            "undo restored public-by-absence"
        );
        assert!(
            repo.get_state_visibility_bytes_for_state(&state)
                .unwrap()
                .is_none(),
            "the sidecar was removed by the undo"
        );

        // Redo A: the sidecar comes back to exactly A's bytes.
        let redo_batches = repo.oplog().redo_batches_scoped(1, Some(&scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("redo", &scope, generation, &redo_batches);
        repo::atomic::execute(&repo, RedoOp::new(redo_batches, txid)).expect("redo succeeds");
        assert_eq!(
            repo.get_state_visibility_bytes_for_state(&state).unwrap(),
            after_set,
            "redo restored exactly A's sidecar bytes"
        );
        assert!(
            repo.has_visibility_for_state(&state).unwrap(),
            "the state is non-public again after redo"
        );
    }
}
