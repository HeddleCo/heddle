// SPDX-License-Identifier: Apache-2.0
//! The oplog-backed [`RefReconciler`] (heddle#330 read chokepoint).
//!
//! Defined here, in the `repo`/`oplog` layer that sees both crates, and injected
//! into `RefManager` via `with_reconciler` — so `refs` names no `oplog` type
//! (dependency inversion). It folds the committed oplog tail (scoped by ref
//! class) to re-derive the authoritative ref value when the canonical cache
//! lags a committed-but-unpublished write.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use objects::error::Result;
use objects::object::{ChangeId, MarkerName, ThreadName};
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

/// The folded state of one ref class, replayed from committed entries.
///
/// HEAD is reconstructed only from records that now carry the published HEAD
/// identity (`Fork` and `Collapse`). Older records that do not identify the
/// post-HEAD shape remain non-authoritative for HEAD.
#[derive(Default)]
struct Fold {
    head: Option<Head>,
    threads: BTreeMap<String, Option<ChangeId>>,
    markers: BTreeMap<String, Option<ChangeId>>,
    remotes: BTreeMap<(String, String), Option<ChangeId>>,
    undo_recovery: Option<ChangeId>,
}

impl Fold {
    fn apply(&mut self, op: &OpRecord) {
        match op {
            OpRecord::Snapshot {
                new_state, thread, ..
            } => {
                if let Some(name) = thread {
                    self.threads.insert(name.clone(), Some(*new_state));
                }
            }
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
                if let Some(name) = thread {
                    self.threads.insert(name.clone(), Some(*new_state));
                    self.head = Some(Head::Attached {
                        thread: ThreadName::new(name),
                    });
                }
                if let OpRecord::Fork {
                    head: Some(head), ..
                } = op
                {
                    self.head = Some(Head::Detached { state: *head });
                }
            }
            OpRecord::Collapse { result, thread, .. } => {
                if let Some(name) = thread {
                    self.threads.insert(name.clone(), Some(*result));
                    self.head = Some(Head::Attached {
                        thread: ThreadName::new(name),
                    });
                } else {
                    self.head = Some(Head::Detached { state: *result });
                }
            }
            OpRecord::MarkerCreate { name, state } => {
                self.markers.insert(name.clone(), Some(*state));
            }
            OpRecord::MarkerDelete { name, .. } => {
                self.markers.insert(name.clone(), None);
            }
            OpRecord::Checkpoint { state, thread, .. } => {
                if let Some(name) = thread {
                    self.threads.insert(name.clone(), Some(*state));
                }
            }
            OpRecord::FastForwardV2 {
                target_thread,
                post_target_id,
                ..
            } => {
                self.threads
                    .insert(target_thread.clone(), Some(*post_target_id));
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
            // HEAD-only records that do not encode the post-HEAD shape
            // contribute nothing to materialization.
            OpRecord::Goto { .. }
            | OpRecord::FastForward { .. }
            | OpRecord::TransactionAbort { .. }
            | OpRecord::TransactionCommit { .. }
            | OpRecord::ConflictResolved { .. }
            | OpRecord::Redact { .. }
            | OpRecord::Purge { .. }
            | OpRecord::GitCheckpoint { .. } => {}
        }
    }
}

impl RefReconciler for OplogRefReconciler {
    fn generation(&self) -> u64 {
        self.oplog().head_id().unwrap_or(0)
    }

    fn reconcile(&self, req: &LoadRequest, raw: Loaded, since: u64) -> Result<ReconcileOutcome> {
        let class = req.ref_class();
        let scope = match class {
            RefClass::Local => Some(self.op_scope.as_str()),
            RefClass::Shared => None,
        };

        // Replay committed (non-undone) entries newer than the watermark, in id
        // order, so the fold reflects the newest committed target per ref.
        let batches = self.oplog().recent_batches_scoped(usize::MAX, scope)?;
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
                if let Some(head) = &fold.head {
                    republish.push(RefUpdate::Head {
                        expected: RefExpectation::Any,
                        new: head.clone(),
                    });
                }
                undo_recovery = fold.undo_recovery;
            }
        }

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
            Some(head) => Loaded::Head(head.clone()),
            None => raw.clone(),
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
