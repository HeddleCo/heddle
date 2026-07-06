// SPDX-License-Identifier: Apache-2.0
//! Apply undo/redo operations to the repository.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};
use objects::{
    error::{HeddleError, Result as HeddleResult},
    lock::{RepoLock, WriteLockGuard},
    object::{ChangeId, ContentHash, MarkerName, ThreadName},
};
use oplog::{IsolationKey, OpBatch, OpEntry, OpLogBackend, OpRecord, isolation_keys_for_record};
use refs::Head;
use repo::{
    CommitGraphIndex, Repository, Thread, ThreadFreshness, ThreadIntegrationPolicy, ThreadManager,
    ThreadState, VisibilitySidecarRestore,
    atomic::{AtomicMutation, DeferredMutation, StagedCommit, Tx},
    refresh_thread_freshness,
};
use sley::{
    DeleteRef, FullName, GitObjectType, GitTime, HeadUpdateOptions, IndexWriteOptions, ObjectId,
    RefPrecondition, ReferenceTarget, Repository as SleyRepository, Signature,
    plumbing::sley_core::ByteString as GitByteString,
};

use super::{advice::RecoveryAdvice, thread_cmd::thread_not_found_advice};
use crate::git_projection_engine::git_core::{open_repo as open_git_repo, set_reference};

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
    repo.set_head_symref(format!("refs/heads/{branch}"), HeadUpdateOptions::new())
        .map_err(|error| anyhow!("failed to attach Git HEAD to branch '{branch}': {error}"))?;
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
        conflicts_resolved_manually: thread.integration_policy_result.conflicts_resolved_manually,
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
mod head_symref_tests {
    use sley::{HeadUpdateOptions, Repository as SleyRepository};

    use super::attach_git_head_to_branch;

    #[test]
    fn attach_git_head_writes_legacy_head_bytes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let git_dir = tmp.path().join(".git");
        let repo = SleyRepository::init_bare(&git_dir).expect("init bare");
        attach_git_head_to_branch(&repo, "feature").expect("attach HEAD");
        assert_eq!(
            std::fs::read_to_string(git_dir.join("HEAD")).expect("read HEAD"),
            "ref: refs/heads/feature\n"
        );
        // Same bytes as the pre-migration `fs::write` path.
        repo.set_head_symref("refs/heads/other", HeadUpdateOptions::new())
            .expect("direct symref");
        attach_git_head_to_branch(&repo, "main").expect("reattach");
        assert_eq!(
            std::fs::read_to_string(git_dir.join("HEAD")).unwrap(),
            "ref: refs/heads/main\n"
        );
    }
}

#[cfg(test)]
mod atomic_tests;
