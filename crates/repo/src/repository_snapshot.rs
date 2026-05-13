// SPDX-License-Identifier: Apache-2.0
//! Snapshot operations for Repository.

use objects::{
    lock::RepositoryLockExt,
    object::{Attribution, ChangeId, State, Tree},
};
use refs::Head;
use tracing::{debug, instrument};

use super::{HeddleError, Repository, Result, repository_tree::TreeBuildProfile};

#[derive(Debug, Clone, Default)]
pub struct SnapshotProfile {
    pub tree_walk_ms: u128,
    pub blob_prep_ms: u128,
    pub blob_write_ms: u128,
    pub tree_write_ms: u128,
    pub state_ref_oplog_ms: u128,
}

#[derive(Debug, Clone)]
pub struct SnapshotExecution {
    pub state: State,
    pub tree: Tree,
    pub profile: SnapshotProfile,
}

impl Repository {
    /// Create a snapshot of the current worktree.
    #[instrument(skip(self), fields(intent = ?intent))]
    pub fn snapshot(&self, intent: Option<String>, confidence: Option<f32>) -> Result<State> {
        let attribution = self.get_attribution()?;
        self.snapshot_with_attribution(intent, confidence, attribution)
    }

    /// Create a snapshot with explicit attribution.
    #[instrument(skip(self, attribution), fields(intent = ?intent, confidence))]
    pub fn snapshot_with_attribution(
        &self,
        intent: Option<String>,
        confidence: Option<f32>,
        attribution: Attribution,
    ) -> Result<State> {
        self.snapshot_with_attribution_profiled(intent, confidence, attribution)
            .map(|execution| execution.state)
    }

    /// Create a snapshot with profiling details for the hot path.
    #[instrument(skip(self, attribution), fields(intent = ?intent, confidence))]
    pub fn snapshot_with_attribution_profiled(
        &self,
        intent: Option<String>,
        confidence: Option<f32>,
        attribution: Attribution,
    ) -> Result<SnapshotExecution> {
        let _lock = self
            .locker()
            .write()
            .map_err(|e| HeddleError::Io(std::io::Error::other(e.to_string())))?;

        if let Some(merge_state) = self.merge_state_manager().load()? {
            let unresolved: Vec<_> = merge_state
                .conflicts
                .iter()
                .filter(|path| !merge_state.resolved.contains(*path))
                .collect();
            if !unresolved.is_empty() {
                return Err(HeddleError::Conflict(format!(
                    "Unresolved conflicts: {}",
                    unresolved
                        .into_iter()
                        .map(|path| path.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            let theirs = merge_state.theirs;
            let base = merge_state.base;
            let intent = intent.or(Some(format!("Merge {}", theirs.short())));
            let state = self.snapshot_merge_with_attribution(
                &theirs,
                intent,
                confidence,
                attribution,
                base,
            )?;
            self.merge_state_manager().finish()?;
            let tree = self
                .store
                .get_tree(&state.tree)?
                .ok_or_else(|| HeddleError::NotFound("merge snapshot tree missing".to_string()))?;
            return Ok(SnapshotExecution {
                state,
                tree,
                profile: SnapshotProfile::default(),
            });
        }

        self.store.begin_snapshot_write_batch()?;
        let snapshot = (|| -> Result<SnapshotExecution> {
            debug!("Building tree from worktree");
            let (tree, tree_profile) = self.build_tree_profiled(&self.root)?;
            debug!(duration_ms = tree_profile.tree_walk_ms, "Tree built");

            debug!("Storing tree");
            let root_tree_write_start = std::time::Instant::now();
            let tree_hash = self.store.put_tree(&tree)?;
            let root_tree_write_ms = root_tree_write_start.elapsed().as_millis();

            let prev_head = self.head()?;
            let parents = match prev_head {
                Some(id) => vec![id],
                None => vec![],
            };

            let mut state = State::new_snapshot(tree_hash, parents, attribution);

            if let Some(intent) = intent {
                state = state.with_intent(intent);
            }

            if let Some(confidence) = confidence {
                state = state.with_confidence(confidence);
            }

            // Carry the parent's context tree forward so annotations attached
            // upstream remain active at this state. The tree is content-
            // addressed, so this is a pointer copy. The on-demand staleness
            // check (compares stored `source_hash` against current bytes at
            // the anchor) reports drift caused by the new tree without us
            // re-stamping anything here.
            if let Some(parent_id) = prev_head
                && let Some(parent_state) = self.store.get_state(&parent_id)?
                && let Some(inherited) = Repository::inherit_parent_context(&parent_state)
            {
                state = state.with_context(inherited);
            }

            // Risk-signal computation. Runs the `state_review`
            // registry against the freshly-built `(prior, new)` pair,
            // persists the resulting `RiskSignalBlob`, and attaches its
            // hash to the new state. The function returns `None` when
            // either no signals fire (avoid an empty blob) or anything
            // goes wrong below the line — capture must not fail because
            // of a signal hiccup. Gated behind `tree-sitter-symbols` to
            // match anchor-travel; the registry's tree-sitter modules
            // would otherwise sit idle anyway.
            #[cfg(feature = "tree-sitter-symbols")]
            {
                let prior_state = match prev_head {
                    Some(id) => self.store.get_state(&id).ok().flatten(),
                    None => None,
                };
                match self.compute_and_persist_signals(prior_state.as_ref(), &state) {
                    Ok(Some(hash)) => {
                        state = state.with_risk_signals(hash);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::warn!(error = %err, "risk signal computation failed; continuing without signals");
                    }
                }
            }

            let state_ref_oplog_start = std::time::Instant::now();
            self.store.put_state(&state)?;
            self.store.flush_snapshot_write_batch()?;

            // Fault-injection checkpoint: a crash here leaves the
            // state object durable on disk but no ref pointing at
            // it. The next heddle invocation re-reads HEAD and sees
            // the prior tip — captured work is effectively dropped
            // (no corruption). Tested by
            // `agent_capture_atomicity_under_sigkill`.
            objects::fault_inject::maybe_panic_at("snapshot_after_state_before_ref");

            let head = self.refs.read_head()?;
            let thread = match &head {
                Head::Attached { thread } => Some(thread.clone()),
                Head::Detached { .. } => None,
            };

            match head {
                Head::Attached { thread } => {
                    self.refs.set_thread(&thread, &state.change_id)?;
                }
                Head::Detached { .. } => {
                    self.refs.write_head(&Head::Detached {
                        state: state.change_id,
                    })?;
                }
            }

            self.oplog.record_snapshot(
                &state.change_id,
                prev_head.as_ref(),
                thread.as_deref(),
                Some(&self.op_scope()),
            )?;

            Ok(SnapshotExecution {
                state,
                tree,
                profile: snapshot_profile_from_tree(
                    tree_profile,
                    root_tree_write_ms,
                    state_ref_oplog_start.elapsed().as_millis(),
                ),
            })
        })();
        if snapshot.is_err() {
            self.store.abort_snapshot_write_batch();
        }
        snapshot
    }

    /// Create a merge state with two parents.
    pub fn snapshot_merge_with_attribution(
        &self,
        merge_parent: &ChangeId,
        intent: Option<String>,
        confidence: Option<f32>,
        attribution: Attribution,
        merge_base: Option<ChangeId>,
    ) -> Result<State> {
        let tree = self.build_tree(&self.root)?;
        let tree_hash = self.store.put_tree(&tree)?;

        let first_parent = self
            .head()?
            .ok_or_else(|| HeddleError::NotFound("No current state".to_string()))?;
        let parents = vec![first_parent, *merge_parent];

        let mut state = State::new_merge(tree_hash, parents, attribution);

        if let Some(intent) = intent {
            state = state.with_intent(intent);
        }

        if let Some(confidence) = confidence {
            state = state.with_confidence(confidence);
        }

        let ours_state = self
            .store
            .get_state(&first_parent)?
            .ok_or(HeddleError::StateNotFound(first_parent))?;
        let theirs_state = self
            .store
            .get_state(merge_parent)?
            .ok_or(HeddleError::StateNotFound(*merge_parent))?;
        let base_state = match merge_base {
            Some(base_id) => self.store.get_state(&base_id)?,
            None => None,
        };
        if let Some(provenance) = self.build_merge_provenance_root(
            &state,
            &ours_state,
            &theirs_state,
            base_state.as_ref(),
        )? {
            state = state.with_provenance(provenance);
        }

        // Union the parents' context trees so annotations from either side
        // ride forward. Same-id collisions resolve to the newest revision;
        // see `union_parent_contexts` for the merge rules.
        if let Some(merged_context) = self.union_parent_contexts(&[&ours_state, &theirs_state])? {
            state = state.with_context(merged_context);
        }

        self.store.put_state(&state)?;

        let head = self.refs.read_head()?;
        let thread = match &head {
            Head::Attached { thread } => Some(thread.clone()),
            Head::Detached { .. } => None,
        };

        match head {
            Head::Attached { thread } => {
                self.refs.set_thread(&thread, &state.change_id)?;
            }
            Head::Detached { .. } => {
                self.refs.write_head(&Head::Detached {
                    state: state.change_id,
                })?;
            }
        }

        self.oplog.record_snapshot(
            &state.change_id,
            Some(&first_parent),
            thread.as_deref(),
            Some(&self.op_scope()),
        )?;

        Ok(state)
    }
}

fn snapshot_profile_from_tree(
    tree_profile: TreeBuildProfile,
    root_tree_write_ms: u128,
    state_ref_oplog_ms: u128,
) -> SnapshotProfile {
    SnapshotProfile {
        tree_walk_ms: tree_profile.tree_walk_ms,
        blob_prep_ms: tree_profile.blob_prep_ms,
        blob_write_ms: tree_profile.blob_write_ms,
        tree_write_ms: tree_profile.tree_write_ms + root_tree_write_ms,
        state_ref_oplog_ms,
    }
}