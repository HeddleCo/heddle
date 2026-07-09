// SPDX-License-Identifier: Apache-2.0
//! Transactional ref update logic for RefManager.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use objects::{
    error::{HeddleError, Result},
    object::{ChangeId, ThreadName},
};

use super::{
    RefManager, RefUpdate, format_change_id_text, parse_change_id_text,
    reconcile::{LoadRequest, Loaded},
    ref_summary_index::SummaryDelta,
    refs_storage::RefsLock,
    refs_types::{
        describe_change_id, describe_expectation_change_id, describe_expectation_head,
        describe_head, matches_expectation,
    },
};

#[cfg(test)]
use super::packed_refs::PackedRefs;
use crate::fs_atomic::{stage_temp_files_durable, sync_directory};

enum PackedRemove {
    Thread(String),
    Marker(String),
}

/// Map a planned thread write to its summary-index delta: a set when `new` is
/// `Some`, a delete (loose + packed are both purged) when `None`.
fn thread_summary_delta(name: &str, new: Option<&ChangeId>) -> SummaryDelta {
    match new {
        Some(change_id) => SummaryDelta::SetThread {
            name: name.to_string(),
            change_id: *change_id,
        },
        None => SummaryDelta::DeleteThread {
            name: name.to_string(),
        },
    }
}

/// Map a planned marker write to its summary-index delta (see
/// [`thread_summary_delta`]).
fn marker_summary_delta(name: &str, new: Option<&ChangeId>) -> SummaryDelta {
    match new {
        Some(change_id) => SummaryDelta::SetMarker {
            name: name.to_string(),
            change_id: *change_id,
        },
        None => SummaryDelta::DeleteMarker {
            name: name.to_string(),
        },
    }
}

pub(super) struct RefUpdatePlan {
    path: PathBuf,
    new_content: Option<String>,
    previous_content: Option<String>,
    description: String,
    temp_path: Option<PathBuf>,
    packed_remove: Option<PackedRemove>,
    /// How this plan changes the summary index, so the post-publish index update
    /// is an `O(1)` edit instead of a full-dir rescan. `None` for HEAD plans
    /// (HEAD is not part of the summary index).
    summary_delta: Option<SummaryDelta>,
}

impl RefManager {
    fn read_track_with_packed_fallback(
        &self,
        name: &ThreadName,
    ) -> Result<(PathBuf, Option<ChangeId>, Option<String>)> {
        let path = self.thread_path(name)?;
        let raw = self.read_optional_string(&path)?;
        if let Some(ref contents) = raw {
            match parse_change_id_text(contents) {
                Ok(id) => return Ok((path, Some(id), raw)),
                Err(_) => {
                    return Err(HeddleError::InvalidObject(format!(
                        "invalid thread {}: {}",
                        name,
                        contents.trim()
                    )));
                }
            }
        }
        let packed_id = self.load_packed_refs_cached()?.get_thread(name);
        let effective_prev = packed_id.map(|id| format_change_id_text(&id));
        Ok((path, packed_id, effective_prev))
    }

    fn read_marker_with_packed_fallback(
        &self,
        path: &std::path::Path,
        name: &str,
    ) -> Result<(Option<ChangeId>, Option<String>)> {
        let raw = self.read_optional_string(path)?;
        if let Some(ref contents) = raw {
            match parse_change_id_text(contents) {
                Ok(id) => return Ok((Some(id), raw)),
                Err(_) => {
                    return Err(HeddleError::InvalidObject(format!(
                        "invalid marker {}: {}",
                        name,
                        contents.trim()
                    )));
                }
            }
        }
        let packed_id = self.load_packed_refs_cached()?.get_marker(name);
        let effective_prev = packed_id.map(|id| format_change_id_text(&id));
        Ok((packed_id, effective_prev))
    }

    pub(super) fn update_refs_with_lock(
        &self,
        updates: &[RefUpdate],
        lock: &RefsLock,
    ) -> Result<()> {
        let plans = self.plan_ref_updates(updates)?;
        self.publish_ref_plans(plans, lock)
    }

    /// Validate + commit + publish under the held refs lock (heddle#330 §2.2
    /// write chokepoint, cid 3329490978 / 3329490984).
    ///
    /// Phase 3 plans and validates every update against the on-disk value
    /// **first** (writing nothing), so a CAS-expectation failure returns `Err`
    /// before `commit` runs — the oplog record is never appended for a mutation
    /// that will not publish (no validation-failure leak). `commit` (phase 4)
    /// then runs, immediately followed by the phase-5 publish. If phase 5
    /// fails after a ref-carrying record was durably committed, the operation
    /// has already linearized; log the swallowed publish error (warn) for
    /// operator visibility, then return success and let reconciliation
    /// materialize the committed effect on the next read.
    pub(super) fn validate_commit_publish(
        &self,
        updates: &[RefUpdate],
        lock: &RefsLock,
        commit: impl FnOnce() -> Result<bool>,
    ) -> Result<()> {
        let plans = self.plan_ref_updates(updates)?;
        let committed_for_reconcile = commit()?;
        match self.publish_ref_plans(plans, lock) {
            Ok(()) => Ok(()),
            Err(err) if committed_for_reconcile => {
                tracing::warn!(
                    error = %err,
                    "ref publish failed after the record committed; the operation \
                     linearized and reconciliation will materialize it on the next read"
                );
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    /// Phase 3 (heddle#330 §2.2): plan + validate every update against the
    /// **reconciled** current value, rejecting CAS conflicts and duplicate
    /// targets up front. Pure validation — touches no canonical ref and no temp
    /// file, so a failed expectation returns `Err` before anything is staged or
    /// committed.
    ///
    /// **Validation + publish base come from the under-lock reconciled state, not
    /// a pre-lock raw disk read (heddle#354 r5, cid 3329631079).** Because a
    /// committed-but-unpublished record can leave the on-disk ref stale (a crash
    /// between phase 4 and phase 5, or a co-tenant lane's lagging publish), a
    /// `Missing`/CAS expectation checked against raw disk would validate against
    /// the wrong value. The caller already holds the refs lock, so
    /// [`reconciled_value_under_lock`](RefManager::reconciled_value_under_lock)
    /// folds the committed tail without re-locking; the fold→validate→publish
    /// sequence is therefore one atomic unit under the single held lock.
    fn plan_ref_updates(&self, updates: &[RefUpdate]) -> Result<Vec<RefUpdatePlan>> {
        let mut seen = HashSet::new();
        let mut plans = Vec::new();

        for update in updates {
            match update {
                RefUpdate::Thread {
                    name,
                    expected,
                    new,
                } => {
                    let (path, _raw_current, _raw_prev) =
                        self.read_track_with_packed_fallback(name)?;
                    if !seen.insert(path.clone()) {
                        return Err(HeddleError::Conflict(format!(
                            "duplicate ref update for thread {}",
                            name
                        )));
                    }

                    let current = match self
                        .reconciled_value_under_lock(&LoadRequest::Thread(name.clone()))?
                    {
                        Loaded::Point(id) => id,
                        _ => unreachable!("Thread request yields Point"),
                    };
                    if !matches_expectation(expected, current.as_ref(), current.is_some()) {
                        return Err(HeddleError::Conflict(format!(
                            "thread {} expected {}, found {}",
                            name,
                            describe_expectation_change_id(expected),
                            describe_change_id(current)
                        )));
                    }

                    let new_content = new.as_ref().map(format_change_id_text);
                    let previous_content = current.as_ref().map(format_change_id_text);
                    let packed_remove = if new.is_none() && current.is_some() {
                        Some(PackedRemove::Thread(name.to_string()))
                    } else {
                        None
                    };
                    let summary_delta = Some(thread_summary_delta(name.as_str(), new.as_ref()));
                    plans.push(RefUpdatePlan {
                        path,
                        new_content,
                        previous_content,
                        description: format!("thread {}", name),
                        temp_path: None,
                        packed_remove,
                        summary_delta,
                    });
                }
                RefUpdate::Marker {
                    name,
                    expected,
                    new,
                } => {
                    let path = self.marker_path(name)?;
                    if !seen.insert(path.clone()) {
                        return Err(HeddleError::Conflict(format!(
                            "duplicate ref update for marker {}",
                            name
                        )));
                    }

                    let current = match self
                        .reconciled_value_under_lock(&LoadRequest::Marker(name.clone()))?
                    {
                        Loaded::Point(id) => id,
                        _ => unreachable!("Marker request yields Point"),
                    };
                    if !matches_expectation(expected, current.as_ref(), current.is_some()) {
                        return Err(HeddleError::Conflict(format!(
                            "marker {} expected {}, found {}",
                            name,
                            describe_expectation_change_id(expected),
                            describe_change_id(current)
                        )));
                    }

                    let new_content = new.as_ref().map(format_change_id_text);
                    let previous_content = current.as_ref().map(format_change_id_text);
                    let packed_remove = if new.is_none() && current.is_some() {
                        Some(PackedRemove::Marker(name.to_string()))
                    } else {
                        None
                    };
                    let summary_delta = Some(marker_summary_delta(name, new.as_ref()));
                    plans.push(RefUpdatePlan {
                        path,
                        new_content,
                        previous_content,
                        description: format!("marker {}", name),
                        temp_path: None,
                        packed_remove,
                        summary_delta,
                    });
                }
                RefUpdate::Head { expected, new } => {
                    let raw_state = self.read_head_state()?;
                    let reconciled_head =
                        match self.reconciled_value_under_lock(&LoadRequest::Head)? {
                            Loaded::Head(head) => head,
                            _ => unreachable!("Head request yields Head"),
                        };
                    // HEAD "exists" if its file is present OR a committed record
                    // reconstructs a value the stale on-disk HEAD does not reflect.
                    let exists = raw_state.exists || reconciled_head != raw_state.head;
                    let current_desc = if exists {
                        describe_head(&reconciled_head)
                    } else {
                        "missing".to_string()
                    };

                    if !matches_expectation(expected, Some(&reconciled_head), exists) {
                        return Err(HeddleError::Conflict(format!(
                            "HEAD expected {}, found {}",
                            describe_expectation_head(expected),
                            current_desc
                        )));
                    }

                    // Publish base from the reconciled HEAD: when a committed
                    // record reconstructs a value the raw HEAD lags, a rollback
                    // restores that authoritative value, not the stale disk one.
                    let previous_content = if reconciled_head == raw_state.head {
                        raw_state.raw
                    } else {
                        Some(reconciled_head.to_text())
                    };

                    plans.push(RefUpdatePlan {
                        path: self.head_path(),
                        new_content: Some(new.to_text()),
                        previous_content,
                        description: "HEAD".to_string(),
                        temp_path: None,
                        packed_remove: None,
                        summary_delta: None,
                    });
                }
            }
        }

        Ok(plans)
    }

    /// Build the publish plans for the reconciler's lazy re-materialization set
    /// (heddle#354 r5). Unlike [`plan_ref_updates`](Self::plan_ref_updates) this
    /// does NOT re-reconcile or validate: the `republish` values are already the
    /// authoritative under-lock fold (computed by the caller before the lock was
    /// taken stale-free), so re-folding here would be redundant and could double-
    /// count the reconciler's call budget. Each entry is skipped when the current
    /// canonical already equals the folded value (no-op), and the publish base is
    /// the current canonical (so a failed publish rolls back to exactly what was
    /// on disk).
    pub(super) fn plan_materialization(
        &self,
        republish: &[RefUpdate],
    ) -> Result<Vec<RefUpdatePlan>> {
        let mut plans = Vec::new();
        for update in republish {
            match update {
                RefUpdate::Thread { name, new, .. } => {
                    let (path, current, effective_prev) =
                        self.read_track_with_packed_fallback(name)?;
                    if current == *new {
                        continue;
                    }
                    let packed_remove = if new.is_none() && current.is_some() {
                        Some(PackedRemove::Thread(name.to_string()))
                    } else {
                        None
                    };
                    let summary_delta = Some(thread_summary_delta(name.as_str(), new.as_ref()));
                    plans.push(RefUpdatePlan {
                        path,
                        new_content: new.as_ref().map(format_change_id_text),
                        previous_content: effective_prev,
                        description: format!("thread {}", name),
                        temp_path: None,
                        packed_remove,
                        summary_delta,
                    });
                }
                RefUpdate::Marker { name, new, .. } => {
                    let path = self.marker_path(name)?;
                    let (current, effective_prev) =
                        self.read_marker_with_packed_fallback(&path, name)?;
                    if current == *new {
                        continue;
                    }
                    let packed_remove = if new.is_none() && current.is_some() {
                        Some(PackedRemove::Marker(name.to_string()))
                    } else {
                        None
                    };
                    let summary_delta = Some(marker_summary_delta(name, new.as_ref()));
                    plans.push(RefUpdatePlan {
                        path,
                        new_content: new.as_ref().map(format_change_id_text),
                        previous_content: effective_prev,
                        description: format!("marker {}", name),
                        temp_path: None,
                        packed_remove,
                        summary_delta,
                    });
                }
                RefUpdate::Head { new, .. } => {
                    let state = self.read_head_state()?;
                    if state.exists && state.head == *new {
                        continue;
                    }
                    plans.push(RefUpdatePlan {
                        path: self.head_path(),
                        new_content: Some(new.to_text()),
                        previous_content: state.raw,
                        description: "HEAD".to_string(),
                        temp_path: None,
                        packed_remove: None,
                        summary_delta: None,
                    });
                }
            }
        }
        Ok(plans)
    }

    /// Phase 5 (heddle#330 §2.2): stage each update into a temp file, rename the
    /// temps into their canonical paths (the publish), apply packed-ref removals,
    /// and rebuild the summary index. On any apply error the reverse-order
    /// `rollback_updates` restores prior contents. Called only after
    /// [`plan_ref_updates`](Self::plan_ref_updates) has validated the batch.
    pub(super) fn publish_ref_plans(
        &self,
        mut plans: Vec<RefUpdatePlan>,
        _lock: &RefsLock,
    ) -> Result<()> {
        // Stage every new-content plan into a temp file, then make them all
        // durable in ONE overlapped-writeback pass. The previous per-plan
        // `write → fsync` loop paid one serial fsync barrier per ref, so a batch
        // publishing N refs (the `heddle adopt` hot path: N branches → one
        // `update_refs`) sat ~N fsyncs deep (~2.3s for 800 refs). Kicking every
        // temp file's writeback up front and fsyncing them as a batch keeps the
        // identical per-file durability guarantee — each temp is fsync'd before
        // its rename — while overlapping the writeback I/O.
        let mut temp_writes: Vec<(PathBuf, Vec<u8>)> = Vec::new();
        for plan in &mut plans {
            if let Some(ref content) = plan.new_content {
                let temp_path = self.alloc_temp_path(&plan.path)?;
                temp_writes.push((temp_path.clone(), content.clone().into_bytes()));
                plan.temp_path = Some(temp_path);
            }
        }
        stage_temp_files_durable(&temp_writes)?;

        let packed_snapshot = self.read_optional_string(&self.packed_refs_path())?;
        let mut applied = Vec::new();
        // Directories whose entries changed via rename. Their fsync is hoisted
        // out of the per-plan loop: a batch that publishes N refs into the same
        // `refs/threads/` directory (the adopt hot path — N branches → one
        // `update_refs`) shares one parent, so the old per-rename `sync_directory`
        // fsync'd that directory N times (2 fsyncs/ref on adopt: the temp file +
        // its parent dir). We instead fsync each *distinct* parent once, after
        // every rename lands. The post-batch durability is identical — on success
        // every rename's directory entry is durable — and the batch was never
        // crash-atomic across refs in either version (there is no journal spanning
        // all N renames; `rollback_updates` handles in-process errors, not power
        // loss mid-loop).
        let mut dirty_parents: Vec<PathBuf> = Vec::new();
        for (index, plan) in plans.iter().enumerate() {
            let result = if let Some(ref temp_path) = plan.temp_path {
                std::fs::rename(temp_path, &plan.path)
                    .map_err(HeddleError::from)
                    .and_then(|()| note_dirty_parent(&mut dirty_parents, &plan.path))
            } else if plan.path.exists() {
                // Matches the pre-hoist behavior: only renames drove a directory
                // fsync (a loose-ref delete published via `remove_file` did not).
                std::fs::remove_file(&plan.path).map_err(HeddleError::from)
            } else {
                Ok(())
            };

            if let Err(err) = result {
                let rollback_result =
                    self.rollback_updates(&plans, &applied, packed_snapshot.clone());
                if let Err(rollback_err) = rollback_result {
                    return Err(HeddleError::Conflict(format!(
                        "refs update failed for {}: {}; rollback failed: {}",
                        plan.description, err, rollback_err
                    )));
                }
                return Err(err);
            }

            applied.push(index);
        }

        // One directory fsync per distinct parent, making every rename in this
        // batch durable. On adopt this collapses ~N dir fsyncs into 1.
        for parent in &dirty_parents {
            sync_directory(parent)?;
        }

        if let Err(err) = self.apply_packed_removals(&plans) {
            let rollback_result = self.rollback_updates(&plans, &applied, packed_snapshot);
            if let Err(rollback_err) = rollback_result {
                return Err(HeddleError::Conflict(format!(
                    "packed refs update failed: {}; rollback failed: {}",
                    err, rollback_err
                )));
            }
            return Err(err);
        }

        // Fold only the just-published loose-ref deltas into the summary index
        // (heddle perf/adopt: the old per-publish `rebuild_ref_summary_index`
        // rescanned the entire refs dir, making any many-ref operation
        // O(refs²)). The plans already carry each changed ref's new value, so
        // this is O(deltas) edits + one packed-refs load. On any failure we drop
        // the sidecar so the next read rebuilds it from storage.
        let deltas: Vec<SummaryDelta> = plans
            .iter()
            .filter_map(|plan| plan.summary_delta.clone())
            .collect();
        if self
            .update_ref_summary_index_with_deltas(_lock, &deltas)
            .is_err()
        {
            self.invalidate_ref_summary_index();
        }

        Ok(())
    }

    fn apply_packed_removals(&self, plans: &[RefUpdatePlan]) -> Result<()> {
        let removals: Vec<&PackedRemove> = plans
            .iter()
            .filter_map(|p| p.packed_remove.as_ref())
            .collect();
        if removals.is_empty() {
            return Ok(());
        }

        let pp = self.packed_refs_path();
        if !pp.exists() {
            return Ok(());
        }

        let mut packed = self.load_packed_refs_cached()?;
        for removal in removals {
            match removal {
                PackedRemove::Thread(name) => packed.remove_track(name),
                PackedRemove::Marker(name) => packed.remove_marker(name),
            }
        }
        packed.save(&pp)?;
        self.invalidate_packed_refs_cache();
        Ok(())
    }

    fn rollback_updates(
        &self,
        plans: &[RefUpdatePlan],
        applied: &[usize],
        packed_snapshot: Option<String>,
    ) -> Result<()> {
        for index in applied.iter().rev().copied() {
            let plan = &plans[index];
            if let Some(ref previous) = plan.previous_content {
                self.write_string(&plan.path, previous)?;
            } else if plan.path.exists() {
                std::fs::remove_file(&plan.path)?;
            }
        }

        let packed_path = self.packed_refs_path();
        match packed_snapshot {
            Some(snapshot) => self.write_string(&packed_path, &snapshot)?,
            None if packed_path.exists() => std::fs::remove_file(packed_path)?,
            None => {}
        }
        self.invalidate_packed_refs_cache();

        Ok(())
    }
}

/// Record `path`'s parent as a directory whose entries changed, so the batch can
/// fsync each distinct parent exactly once after every rename/remove lands.
fn note_dirty_parent(dirty_parents: &mut Vec<PathBuf>, path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| HeddleError::Config("invalid ref path".to_string()))?;
    if !dirty_parents.iter().any(|p| p == parent) {
        dirty_parents.push(parent.to_path_buf());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use objects::object::MarkerName;
    use tempfile::TempDir;

    use super::{
        super::reconcile::{LoadRequest, Loaded, ReconcileOutcome, RefReconciler},
        *,
    };

    fn create_ref_manager() -> (TempDir, RefManager) {
        let temp_dir = TempDir::new().unwrap();
        let heddle_dir = temp_dir.path().join(".heddle");
        std::fs::create_dir_all(&heddle_dir).unwrap();
        let refs = RefManager::new(&heddle_dir);
        refs.init().unwrap();
        (temp_dir, refs)
    }

    #[test]
    fn rollback_restores_packed_refs_snapshot() {
        let (_temp, refs) = create_ref_manager();
        let change_id = ChangeId::generate();
        refs.set_thread(&ThreadName::new("packed-only"), &change_id)
            .unwrap();
        refs.pack_refs().unwrap();

        let packed_path = refs.packed_refs_path();
        let packed_snapshot = std::fs::read_to_string(&packed_path).unwrap();
        let thread_path = refs.thread_path(&ThreadName::new("packed-only")).unwrap();

        let mut packed = PackedRefs::load(&packed_path).unwrap();
        packed.remove_track("packed-only");
        packed.save(&packed_path).unwrap();

        let plans = vec![RefUpdatePlan {
            path: thread_path.clone(),
            new_content: None,
            previous_content: Some(format!("{}\n", change_id.to_string_full())),
            description: "thread packed-only".to_string(),
            temp_path: None,
            packed_remove: Some(PackedRemove::Thread("packed-only".to_string())),
            summary_delta: Some(SummaryDelta::DeleteThread {
                name: "packed-only".to_string(),
            }),
        }];

        refs.rollback_updates(&plans, &[], Some(packed_snapshot.clone()))
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(&packed_path).unwrap(),
            packed_snapshot
        );
        assert!(
            !thread_path.exists(),
            "rollback should restore packed refs, not leave a loose recovery ref"
        );
    }

    struct OneMarkerReconciler {
        generation: Arc<AtomicU64>,
        name: MarkerName,
        state: ChangeId,
    }

    impl RefReconciler for OneMarkerReconciler {
        fn generation(&self) -> Result<u64> {
            Ok(self.generation.load(Ordering::Acquire))
        }

        fn reconcile(
            &self,
            req: &LoadRequest,
            raw: Loaded,
            _since: u64,
        ) -> Result<ReconcileOutcome> {
            let loaded = match req {
                LoadRequest::Marker(name) if name == &self.name => Loaded::Point(Some(self.state)),
                _ => raw,
            };
            Ok(ReconcileOutcome {
                loaded,
                republish: vec![RefUpdate::Marker {
                    name: self.name.clone(),
                    expected: super::super::RefExpectation::Any,
                    new: Some(self.state),
                }],
                remote_updates: Vec::new(),
                undo_recovery: None,
            })
        }
    }

    #[test]
    fn post_commit_publish_failure_is_deferred_success() {
        let (temp, plain_refs) = create_ref_manager();
        let generation = Arc::new(AtomicU64::new(0));
        let good = MarkerName::new("good");
        let bad = MarkerName::new("bad");
        let committed_state = ChangeId::generate();
        let refs = RefManager::new(temp.path().join(".heddle")).with_reconciler(Arc::new(
            OneMarkerReconciler {
                generation: Arc::clone(&generation),
                name: good.clone(),
                state: committed_state,
            },
        ));

        let updates = vec![
            RefUpdate::Marker {
                name: good.clone(),
                expected: super::super::RefExpectation::Missing,
                new: Some(committed_state),
            },
            RefUpdate::Marker {
                name: bad.clone(),
                expected: super::super::RefExpectation::Missing,
                new: Some(ChangeId::generate()),
            },
        ];
        let lock = refs.lock_refs().unwrap();
        let result = refs.validate_commit_publish(&updates, &lock, || {
            generation.store(1, Ordering::Release);
            std::fs::create_dir(plain_refs.marker_path(bad.as_str()).unwrap()).unwrap();
            Ok(true)
        });
        drop(lock);

        assert!(
            result.is_ok(),
            "phase-5 failure after durable commit must not report mutation failure"
        );
        std::fs::remove_dir_all(plain_refs.marker_path(bad.as_str()).unwrap()).unwrap();
        assert_eq!(
            refs.get_marker(&good).unwrap(),
            Some(committed_state),
            "the next read must materialize the committed effect"
        );
    }
}
