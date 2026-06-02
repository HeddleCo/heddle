// SPDX-License-Identifier: Apache-2.0
//! The oplog-backed [`RefReconciler`] (heddle#330 read chokepoint).
//!
//! Defined here, in the `repo`/`oplog` layer that sees both crates, and injected
//! into `RefManager` via `with_reconciler` — so `refs` names no `oplog` type
//! (dependency inversion). It folds the committed oplog tail (scoped by ref
//! class) to re-derive the authoritative ref value when the canonical cache
//! lags a committed-but-unpublished write.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use objects::{
    error::Result,
    object::{ChangeId, MarkerName, ThreadName},
};
use oplog::{OpLog, OpRecord};
use refs::{
    Head, LoadRequest, Loaded, ReconcileOutcome, RefClass, RefExpectation, RefManager,
    RefReconciler, RefUpdate,
};

/// Reads the file oplog from disk (path-based, so a long-held handle observes
/// concurrent commits) and folds it into ref values.
///
/// Holds the heddle dir rather than a cached `OpLog` so each reconcile reads a
/// **fresh** view — the writes that produced the lag came through a *different*
/// `OpLog` handle (the repository's), so a cached reader would miss them.
pub struct OplogRefReconciler {
    heddle_dir: PathBuf,
    op_scope: String,
}

impl OplogRefReconciler {
    pub fn new(heddle_dir: &Path, op_scope: String) -> Self {
        Self {
            heddle_dir: heddle_dir.to_path_buf(),
            op_scope,
        }
    }

    fn oplog(&self) -> OpLog {
        OpLog::new_unattributed(&self.heddle_dir)
    }
}

/// What the folded window implies for canonical HEAD (the `Local` class).
///
/// HEAD reconciliation folds the **latest HEAD-moving record of ANY shape** in
/// the window — a per-shape allowlist (Fork/Collapse only) re-opens the
/// stale-HEAD clobber (heddle#354 r4): a `goto B` recorded *after* a `Fork A`
/// would be ignored, so a reconcile would resurrect the stale `A` over the live
/// `B`. The last HEAD-mover in id order wins; every earlier mover is masked.
///
/// The two republish modes differ by the latest mover's commit ordering:
/// - [`HeadFold::Republish`] — the HEAD-moving records emitted by the atomic
///   write chokepoint, which commits the oplog record (phase 4) BEFORE
///   publishing HEAD (phase 5). A crash in between leaves the record committed
///   but HEAD unpublished, so reconstruct from the record and republish to
///   recover it (the `crash_replay_reconstructs_committed_head_update` case).
///   This set is the *HEAD-moving* shapes only: a named `Fork` (creates a
///   thread and attaches HEAD to it), a detached `Fork`/`Collapse`/`Snapshot`,
///   and `Goto` (publishes a detached HEAD).
/// - [`HeadFold::Canonical`] — detached `Checkpoint` and `FastForward` write
///   HEAD DIRECTLY *before* recording. Canonical therefore already reflects the
///   move; worse, an unrecorded follow-up write (e.g.
///   `fast_forward_attached`'s re-attach to a thread) may have superseded it.
///   Defer to canonical — republishing the record's intermediate target here
///   would clobber a newer, legitimately-published HEAD. Reaching this mode
///   still MASKS an earlier `Republish`, which is what stops the stale Fork from
///   resurrecting.
///
/// Invariant: HEAD reconciliation considers EVERY HEAD-moving record shape, not
/// a Fork/Collapse allowlist. A publish-first mover defers to canonical (never
/// republishing its possibly-stale target over a newer HEAD); a record-first
/// mover reconstructs. Reintroducing a per-shape allowlist re-opens the
/// stale-HEAD clobber class.
#[derive(Default)]
enum HeadFold {
    /// No HEAD-moving record in the window — canonical is authoritative.
    #[default]
    Untouched,
    /// Latest mover is a record-first atomic publish (Fork / Collapse /
    /// Snapshot / Goto): reconstruct and republish to recover a
    /// crash-lost HEAD publish.
    Republish(Head),
    /// Latest mover is a publish-first direct write (detached Checkpoint /
    /// FastForward): canonical wins.
    Canonical,
}

/// The folded state of one ref class, replayed from committed entries.
#[derive(Default)]
struct Fold {
    head: HeadFold,
    threads: BTreeMap<String, Option<ChangeId>>,
    markers: BTreeMap<String, Option<ChangeId>>,
    remotes: BTreeMap<(String, String), Option<ChangeId>>,
    undo_recovery: Option<ChangeId>,
}

impl Fold {
    fn apply(&mut self, op: &OpRecord) {
        match op {
            OpRecord::Snapshot {
                new_state,
                thread,
                head,
                ..
            } => match thread {
                Some(name) => {
                    // Attached snapshot advances the thread and leaves HEAD
                    // attached to that thread. Treat it as the latest
                    // reconstructable HEAD state so it masks any earlier
                    // HEAD-mover and materializes the paired thread target
                    // during Local recovery.
                    self.threads.insert(name.clone(), Some(*new_state));
                    self.head = HeadFold::Republish(Head::Attached {
                        thread: ThreadName::new(name),
                    });
                }
                // Detached snapshot is record-first. Use the record's published
                // HEAD field directly so the replay target is explicit.
                None => {
                    if let Some(head) = head {
                        self.head = HeadFold::Republish(Head::Detached { state: *head });
                    }
                }
            },
            OpRecord::ThreadCreate { name, state }
            | OpRecord::ThreadUpdate {
                name,
                new_state: state,
                ..
            } => {
                self.threads.insert(name.clone(), Some(*state));
            }
            OpRecord::ThreadCreateV2 { name, state, .. } => {
                self.threads.insert(name.clone(), Some(*state));
            }
            OpRecord::ThreadDelete { name, .. } => {
                self.threads.insert(name.clone(), None);
            }
            OpRecord::Fork {
                new_state, thread, ..
            } => {
                // Record-first atomic publish: reconstruct the published HEAD so
                // a phase-4-committed/phase-5-unpublished fork is recovered. A
                // fork that publishes neither a thread nor a detached HEAD
                // (`thread = None, head = None`) moves no ref, so it leaves the
                // fold's HEAD untouched (it must not mask a later/earlier mover).
                if let Some(name) = thread {
                    self.threads.insert(name.clone(), Some(*new_state));
                    self.head = HeadFold::Republish(Head::Attached {
                        thread: ThreadName::new(name),
                    });
                }
                if let OpRecord::Fork {
                    head: Some(head), ..
                } = op
                {
                    self.head = HeadFold::Republish(Head::Detached { state: *head });
                }
            }
            OpRecord::Collapse { result, thread, .. } => match thread {
                // Attached collapse advances the thread; HEAD stays
                // `Attached{name}` (identity unchanged), so it is NOT a
                // HEAD-mover — mirror the attached `Snapshot` arm. The collapse
                // command publishes ONLY the thread ref when HEAD is attached
                // (it never re-attaches HEAD), so canonical HEAD is already
                // correct and republishing `Attached` here moved HEAD when it
                // should stay attached (heddle#354 r9, cid 3330304665). The
                // thread ref is recovered via the Shared-class fold below.
                Some(name) => {
                    self.threads.insert(name.clone(), Some(*result));
                }
                // Detached collapse publishes HEAD record-first (the command
                // emits `RefUpdate::Head` Detached): reconstruct it so a
                // phase-4-committed / phase-5-unpublished collapse is recovered
                // (symmetric with the detached `Snapshot` / `Fork` arms).
                None => {
                    self.head = HeadFold::Republish(Head::Detached { state: *result });
                }
            },
            OpRecord::MarkerCreate { name, state } => {
                self.markers.insert(name.clone(), Some(*state));
            }
            OpRecord::MarkerDelete { name, .. } => {
                self.markers.insert(name.clone(), None);
            }
            OpRecord::Checkpoint { state, thread, .. } => match thread {
                Some(name) => {
                    self.threads.insert(name.clone(), Some(*state));
                }
                // Detached checkpoint moves HEAD via a publish-first direct
                // write (symmetric with the detached `Snapshot` arm).
                None => self.head = HeadFold::Canonical,
            },
            OpRecord::FastForwardV2 {
                target_thread,
                post_target_id,
                ..
            } => {
                // The forward FF (`fast_forward_attached_without_record`) writes
                // HEAD directly before recording — publish-first, so canonical
                // wins for HEAD. The target thread ref also advances and IS
                // reconstructable from the record.
                self.threads
                    .insert(target_thread.clone(), Some(*post_target_id));
                self.head = HeadFold::Canonical;
            }
            OpRecord::EphemeralThreadCollapse { thread, .. } => {
                self.threads.insert(thread.clone(), None);
            }
            OpRecord::RemoteThreadUpdate {
                remote,
                thread,
                state,
            } => {
                self.remotes
                    .insert((remote.clone(), thread.clone()), Some(*state));
            }
            OpRecord::RemoteThreadDelete { remote, thread, .. } => {
                self.remotes.insert((remote.clone(), thread.clone()), None);
            }
            OpRecord::UndoRecoveryUpdate { state } => {
                self.undo_recovery = Some(*state);
            }
            OpRecord::Goto { head, .. } => {
                self.head = HeadFold::Republish(Head::Detached { state: *head });
            }
            // Publish-first HEAD move: legacy V1 `FastForward` moved HEAD via a
            // direct write. Canonical already reflects it (and may carry a
            // newer unrecorded re-attach), so defer — while masking any earlier
            // `Republish` so a stale Fork cannot resurrect.
            OpRecord::FastForward { .. } => {
                self.head = HeadFold::Canonical;
            }
            // Records that do not move canonical HEAD: transaction markers,
            // conflict resolution, redaction bookkeeping, and the Git-overlay
            // checkpoint leave HEAD where it is.
            OpRecord::TransactionAbort { .. }
            | OpRecord::TransactionCommit { .. }
            | OpRecord::ConflictResolved { .. }
            | OpRecord::Redact { .. }
            | OpRecord::Purge { .. }
            | OpRecord::GitCheckpoint { .. } => {}
        }
    }
}

/// The re-materialization set a folded window implies for one ref class:
/// (`republish` thread/marker/HEAD updates, `remote_updates`, `undo_recovery`).
type ClassMaterialization = (
    Vec<RefUpdate>,
    Vec<(String, ThreadName, Option<ChangeId>)>,
    Option<ChangeId>,
);

impl OplogRefReconciler {
    /// Fold the committed entries of `batches` newer than `since` into a
    /// [`Fold`], in id order so the newest committed target per ref wins.
    fn fold_batches(&self, batches: Vec<oplog::OpBatch>, since: u64) -> Fold {
        let mut entries: Vec<_> = batches
            .into_iter()
            .flat_map(|b| b.entries)
            .filter(|e| !e.undone && e.id > since)
            .collect();
        entries.sort_by_key(|e| e.id);

        let mut fold = Fold::default();
        for entry in &entries {
            fold.apply(&entry.operation);
        }
        fold
    }

    /// The re-materialization set a folded window implies for `class`: thread /
    /// marker / remote-thread for shared; reconstructed HEAD + undo-recovery for
    /// local.
    fn class_materialization(class: RefClass, fold: &Fold) -> ClassMaterialization {
        let mut republish = Vec::new();
        let mut remote_updates = Vec::new();
        let mut undo_recovery = None;

        match class {
            RefClass::Shared => {
                for (name, value) in &fold.threads {
                    republish.push(RefUpdate::Thread {
                        name: ThreadName::new(name),
                        expected: RefExpectation::Any,
                        new: *value,
                    });
                }
                for (name, value) in &fold.markers {
                    republish.push(RefUpdate::Marker {
                        name: MarkerName::new(name),
                        expected: RefExpectation::Any,
                        new: *value,
                    });
                }
                for ((remote, thread), value) in &fold.remotes {
                    remote_updates.push((remote.clone(), ThreadName::new(thread), *value));
                }
            }
            RefClass::Local => {
                // Only a record-first mover (Fork / Collapse / Snapshot /
                // Goto) republishes a reconstructed HEAD; a publish-first
                // mover defers to canonical (`HeadFold::Canonical`) so it never
                // clobbers a newer HEAD.
                if let HeadFold::Republish(head) = &fold.head {
                    republish.push(RefUpdate::Head {
                        expected: RefExpectation::Any,
                        new: head.clone(),
                    });
                    // Cross-class recovery atomicity (heddle#354 r8, cid
                    // 3330183592): a record-first HEAD mover that ATTACHES to a
                    // thread (named-fork / collapse-into-thread) created or
                    // advanced that thread in the SAME committed record. The
                    // thread lives in the Shared class with its own watermark,
                    // so a Local-only reconcile would republish HEAD =
                    // Attached(topic) yet leave `topic` unmaterialized —
                    // advancing the Local watermark over a HEAD that points at a
                    // missing ref. Materialize the paired thread HERE, under the
                    // same lock and before the Local watermark advances, so HEAD
                    // and its target land atomically. (A detached-HEAD mover has
                    // no paired thread, so this is a no-op for it.)
                    if let Head::Attached { thread } = head
                        && let Some(value) = fold.threads.get(thread.as_str())
                    {
                        republish.push(RefUpdate::Thread {
                            name: thread.clone(),
                            expected: RefExpectation::Any,
                            new: *value,
                        });
                    }
                }
                undo_recovery = fold.undo_recovery;
            }
        }

        (republish, remote_updates, undo_recovery)
    }
}

impl RefReconciler for OplogRefReconciler {
    fn generation(&self) -> Result<u64> {
        // Propagate a header read error (cid 3329631081): never report a
        // truncated/corrupt/unreadable header as generation 0, which would make
        // logical reads silently skip committed records. `head_id` itself maps a
        // not-yet-created oplog to 0; only a genuine read failure surfaces here.
        self.oplog().head_id()
    }

    fn reconcile(&self, req: &LoadRequest, raw: Loaded, since: u64) -> Result<ReconcileOutcome> {
        let class = req.ref_class();
        let scope = match class {
            RefClass::Local => Some(self.op_scope.as_str()),
            RefClass::Shared => None,
        };

        // Replay committed (non-undone) entries newer than the watermark, in id
        // order, so the fold reflects the newest committed target per ref.
        let batches = self
            .oplog()
            .recent_batches_after_scoped(since, usize::MAX, scope)?;
        let fold = self.fold_batches(batches, since);

        let (republish, remote_updates, undo_recovery) = Self::class_materialization(class, &fold);
        let loaded = reconciled_value(req, &raw, &fold, &self.heddle_dir)?;

        Ok(ReconcileOutcome {
            loaded,
            republish,
            remote_updates,
            undo_recovery,
        })
    }
}

/// Project the authoritative value for the specific request out of the fold.
/// A committed record past the class watermark is **authoritative** over the
/// live canonical, so a folded value wins whether it CREATES a missing ref or
/// UPDATES a stale present one (cid 3329490981) — not fill-if-absent, which
/// silently dropped a committed update to an already-existing ref (the
/// crash-replayed `cmd_collapse` update-to-existing-`main` case). The fold only
/// holds refs touched by commits newer than the watermark, so a ref with no
/// recent committed record keeps its canonical value untouched.
fn reconciled_value(
    req: &LoadRequest,
    raw: &Loaded,
    fold: &Fold,
    heddle_dir: &Path,
) -> Result<Loaded> {
    Ok(match req {
        LoadRequest::Head => match &fold.head {
            HeadFold::Republish(head) => Loaded::Head(head.clone()),
            // A publish-first mover (or no mover at all) leaves canonical
            // authoritative — return the raw HEAD, never a stale reconstruction.
            HeadFold::Canonical | HeadFold::Untouched => raw.clone(),
        },
        LoadRequest::UndoRecovery => fill_point(raw, fold.undo_recovery.map(Some)),
        LoadRequest::Thread(name) => fill_point(raw, fold.threads.get(name.as_str()).copied()),
        LoadRequest::Marker(name) => fill_point(raw, fold.markers.get(name.as_str()).copied()),
        LoadRequest::RemoteThread { remote, thread } => fill_point(
            raw,
            fold.remotes
                .get(&(remote.clone(), thread.to_string()))
                .copied(),
        ),
        LoadRequest::ThreadList => Loaded::ThreadList(
            merge_list(raw_thread_names(raw), &fold.threads)
                .into_iter()
                .map(|n| ThreadName::new(&n))
                .collect(),
        ),
        LoadRequest::MarkerList => Loaded::MarkerList(
            merge_list(raw_marker_names(raw), &fold.markers)
                .into_iter()
                .map(|n| MarkerName::new(&n))
                .collect(),
        ),
        LoadRequest::RemoteList => remote_list_value(raw, fold, heddle_dir)?,
        LoadRequest::RemoteThreadList { remote } => {
            let base: Vec<ThreadName> = match raw {
                Loaded::RemoteThreadList(names) => names.clone(),
                _ => Vec::new(),
            };
            let mut set: BTreeMap<String, ()> =
                base.into_iter().map(|n| (n.to_string(), ())).collect();
            for ((r, thread), value) in &fold.remotes {
                if r == remote {
                    if value.is_some() {
                        set.insert(thread.clone(), ());
                    } else {
                        set.remove(thread);
                    }
                }
            }
            Loaded::RemoteThreadList(set.into_keys().map(|n| ThreadName::new(&n)).collect())
        }
    })
}

/// Authoritative point read: a committed record past the watermark wins over
/// the live canonical, so whenever the lagged batches *touched* this ref we
/// adopt the folded target — set or delete, present canonical or not (cid
/// 3329490981). `folded` is `Some(Some(state))` / `Some(None)` (touched:
/// set/deleted) or `None` (untouched by the lagged batches ⇒ keep canonical).
fn fill_point(raw: &Loaded, folded: Option<Option<ChangeId>>) -> Loaded {
    match folded {
        Some(value) => Loaded::Point(value),
        None => raw.clone(),
    }
}

fn raw_thread_names(raw: &Loaded) -> Vec<String> {
    match raw {
        Loaded::ThreadList(names) => names.iter().map(|n| n.to_string()).collect(),
        _ => Vec::new(),
    }
}

fn raw_marker_names(raw: &Loaded) -> Vec<String> {
    match raw {
        Loaded::MarkerList(names) => names.iter().map(|n| n.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Apply the fold's committed effect to a raw list: set inserts the name and
/// delete removes it. The fold only contains records newer than the class
/// watermark, so its touched refs are authoritative over the canonical view.
fn merge_list(base: Vec<String>, changes: &BTreeMap<String, Option<ChangeId>>) -> Vec<String> {
    let mut set: BTreeMap<String, ()> = base.into_iter().map(|n| (n, ())).collect();
    for (name, value) in changes {
        if value.is_some() {
            set.insert(name.clone(), ());
        } else {
            set.remove(name);
        }
    }
    set.into_keys().collect()
}

fn remote_list_value(raw: &Loaded, fold: &Fold, heddle_dir: &Path) -> Result<Loaded> {
    let mut remotes: BTreeMap<String, ()> = match raw {
        Loaded::RemoteList(names) => names.iter().cloned().map(|n| (n, ())).collect(),
        _ => BTreeMap::new(),
    };
    for ((remote, _thread), value) in &fold.remotes {
        if value.is_some() {
            remotes.insert(remote.clone(), ());
        } else {
            remotes.entry(remote.clone()).or_insert(());
        }
    }

    let raw_refs = RefManager::new(heddle_dir);
    let mut present = Vec::new();
    for remote in remotes.into_keys() {
        let mut threads: BTreeMap<String, ()> = raw_refs
            .list_remote_threads(&remote)?
            .into_iter()
            .map(|name| (name.to_string(), ()))
            .collect();
        for ((r, thread), value) in &fold.remotes {
            if r == &remote {
                if value.is_some() {
                    threads.insert(thread.clone(), ());
                } else {
                    threads.remove(thread);
                }
            }
        }
        if !threads.is_empty() {
            present.push(remote);
        }
    }
    Ok(Loaded::RemoteList(present))
}
