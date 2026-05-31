// SPDX-License-Identifier: Apache-2.0
//! Transactional ref update logic for RefManager.

use std::{collections::HashSet, path::PathBuf};

use objects::{
    error::{HeddleError, Result},
    object::{ChangeId, ThreadName},
};

use super::{
    RefManager, RefUpdate, format_change_id_text,
    packed_refs::PackedRefs,
    parse_change_id_text,
    refs_storage::RefsLock,
    refs_types::{
        describe_change_id, describe_expectation_change_id, describe_expectation_head,
        describe_head, matches_expectation,
    },
};
use crate::fs_atomic::sync_directory;

enum PackedRemove {
    Thread(String),
    Marker(String),
}

struct RefUpdatePlan {
    path: PathBuf,
    new_content: Option<String>,
    previous_content: Option<String>,
    description: String,
    temp_path: Option<PathBuf>,
    packed_remove: Option<PackedRemove>,
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
        if name.contains('/') {
            let legacy_path = self.legacy_thread_path(name)?;
            if legacy_path != path {
                let legacy_raw = self.read_optional_string(&legacy_path)?;
                if let Some(ref contents) = legacy_raw {
                    match parse_change_id_text(contents) {
                        Ok(id) => return Ok((legacy_path, Some(id), legacy_raw)),
                        Err(_) => {
                            return Err(HeddleError::InvalidObject(format!(
                                "invalid thread {}: {}",
                                name,
                                contents.trim()
                            )));
                        }
                    }
                }
            }
        }
        let packed_id = PackedRefs::load(&self.packed_refs_path())?.get_thread(name);
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
        let packed_id = PackedRefs::load(&self.packed_refs_path())?.get_marker(name);
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
    /// has already linearized; return success and let reconciliation materialize
    /// the committed effect on the next read.
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
            Err(_err) if committed_for_reconcile => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// Phase 3 (heddle#330 §2.2): plan + validate every update against the
    /// current on-disk value, rejecting CAS conflicts and duplicate targets up
    /// front. Pure validation — touches no canonical ref and no temp file, so a
    /// failed expectation returns `Err` before anything is staged or committed.
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
                    let (path, current, effective_prev) =
                        self.read_track_with_packed_fallback(name)?;
                    if !seen.insert(path.clone()) {
                        return Err(HeddleError::Conflict(format!(
                            "duplicate ref update for thread {}",
                            name
                        )));
                    }

                    if !matches_expectation(expected, current.as_ref(), current.is_some()) {
                        return Err(HeddleError::Conflict(format!(
                            "thread {} expected {}, found {}",
                            name,
                            describe_expectation_change_id(expected),
                            describe_change_id(current)
                        )));
                    }

                    let new_content = new.as_ref().map(format_change_id_text);
                    let packed_remove = if new.is_none() && current.is_some() {
                        Some(PackedRemove::Thread(name.to_string()))
                    } else {
                        None
                    };
                    plans.push(RefUpdatePlan {
                        path,
                        new_content,
                        previous_content: effective_prev,
                        description: format!("thread {}", name),
                        temp_path: None,
                        packed_remove,
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

                    let (current, effective_prev) =
                        self.read_marker_with_packed_fallback(&path, name)?;

                    if !matches_expectation(expected, current.as_ref(), current.is_some()) {
                        return Err(HeddleError::Conflict(format!(
                            "marker {} expected {}, found {}",
                            name,
                            describe_expectation_change_id(expected),
                            describe_change_id(current)
                        )));
                    }

                    let new_content = new.as_ref().map(format_change_id_text);
                    let packed_remove = if new.is_none() && current.is_some() {
                        Some(PackedRemove::Marker(name.to_string()))
                    } else {
                        None
                    };
                    plans.push(RefUpdatePlan {
                        path,
                        new_content,
                        previous_content: effective_prev,
                        description: format!("marker {}", name),
                        temp_path: None,
                        packed_remove,
                    });
                }
                RefUpdate::Head { expected, new } => {
                    let state = self.read_head_state()?;
                    let current_desc = if state.exists {
                        describe_head(&state.head)
                    } else {
                        "missing".to_string()
                    };

                    if !matches_expectation(expected, Some(&state.head), state.exists) {
                        return Err(HeddleError::Conflict(format!(
                            "HEAD expected {}, found {}",
                            describe_expectation_head(expected),
                            current_desc
                        )));
                    }

                    plans.push(RefUpdatePlan {
                        path: self.head_path(),
                        new_content: Some(new.to_text()),
                        previous_content: state.raw,
                        description: "HEAD".to_string(),
                        temp_path: None,
                        packed_remove: None,
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
    fn publish_ref_plans(&self, mut plans: Vec<RefUpdatePlan>, _lock: &RefsLock) -> Result<()> {
        for plan in &mut plans {
            if let Some(ref content) = plan.new_content {
                let temp_path = self.write_string_temp(&plan.path, content)?;
                plan.temp_path = Some(temp_path.clone());
            }
        }

        let packed_snapshot = self.read_optional_string(&self.packed_refs_path())?;
        let mut applied = Vec::new();
        for (index, plan) in plans.iter().enumerate() {
            let result = if let Some(ref temp_path) = plan.temp_path {
                std::fs::rename(temp_path, &plan.path).map_err(HeddleError::from)?;
                let parent = plan
                    .path
                    .parent()
                    .ok_or_else(|| HeddleError::Config("invalid ref path".to_string()))?;
                sync_directory(parent)?;
                Ok(())
            } else if plan.path.exists() {
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

        if self.rebuild_ref_summary_index_with_lock(_lock).is_err() {
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

        let mut packed = PackedRefs::load(&pp)?;
        for removal in removals {
            match removal {
                PackedRemove::Thread(name) => packed.remove_track(name),
                PackedRemove::Marker(name) => packed.remove_marker(name),
            }
        }
        packed.save(&pp)
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

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use objects::object::MarkerName;
    use tempfile::TempDir;

    use super::super::reconcile::{LoadRequest, Loaded, ReconcileOutcome, RefReconciler};
    use super::*;

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
        fn generation(&self) -> u64 {
            self.generation.load(Ordering::Acquire)
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
