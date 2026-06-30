// SPDX-License-Identifier: Apache-2.0
//! Local repository synchronization.
//!
//! Direct access to local repositories without network protocol overhead.

use std::{collections::HashSet, path::Path};

use anyhow::{Result, anyhow};
use objects::{
    object::{ChangeId, ContentHash},
    store::ObjectStore,
};
use repo::Repository;
use wire::{ObjectId, ObjectTransferPlan, ObjectType, StateClosureOptions};

/// Synchronize objects from a local source repository to a target repository.
pub struct LocalSync {
    source: Repository,
}

impl LocalSync {
    /// Open a local repository for synchronization.
    pub fn open(path: &Path) -> Result<Self> {
        let source = Repository::open(path)?;
        Ok(Self { source })
    }

    /// Get the source repository.
    pub fn source(&self) -> &Repository {
        &self.source
    }

    /// List all threads in the source repository.
    pub fn list_threads(&self) -> Result<Vec<(String, ChangeId)>> {
        let mut threads = Vec::new();
        for thread in self.source.refs().list_threads()? {
            if let Some(state_id) = self.source.refs().get_thread(&thread)? {
                threads.push((thread.to_string(), state_id));
            }
        }
        Ok(threads)
    }

    /// List all markers in the source repository.
    pub fn list_markers(&self) -> Result<Vec<(String, ChangeId)>> {
        let mut markers = Vec::new();
        for marker in self.source.refs().list_markers()? {
            if let Some(state_id) = self.source.refs().get_marker(&marker)? {
                markers.push((marker.to_string(), state_id));
            }
        }
        Ok(markers)
    }

    /// Fetch a state and all its dependencies from source to target.
    pub fn fetch_state(&self, target: &Repository, state_id: &ChangeId) -> Result<usize> {
        let mut copied = 0;
        let mut visited = HashSet::new();
        self.copy_state_recursive(target, state_id, &mut visited, &mut copied, None)?;
        Ok(copied)
    }

    /// Fetch a state with limited depth (shallow clone).
    ///
    /// Depth 1 means the target state and its immediate parents.
    /// A depth of 0 should be treated by callers as "full history".
    pub fn fetch_state_with_depth(
        &self,
        target: &Repository,
        state_id: &ChangeId,
        depth: u32,
    ) -> Result<usize> {
        let mut copied = 0;
        let mut visited = HashSet::new();
        self.copy_state_recursive(target, state_id, &mut visited, &mut copied, Some(depth))?;
        Ok(copied)
    }

    fn copy_state_recursive(
        &self,
        target: &Repository,
        state_id: &ChangeId,
        visited: &mut HashSet<ChangeId>,
        copied: &mut usize,
        max_depth: Option<u32>,
    ) -> Result<()> {
        if visited.contains(state_id) {
            return Ok(());
        }
        visited.insert(*state_id);

        // Whether the target already has this state. We do NOT
        // early-return on this — even when the object graph is fully
        // present, an operator may have declared a redaction on the
        // source *after* the target previously fetched the content.
        // Subsequent syncs must still propagate the sidecar. We
        // therefore always walk the tree(s) to surface redactions,
        // and condition just the object-copy step on the
        // `state_already_present` flag.
        let target_state = target.store().get_state(state_id)?;
        let state_already_present = target_state.is_some();

        // Source-side state read drives both the object copy (when
        // needed) and sidecar propagation (always).
        // If the source no longer has the state but the target does,
        // we can't enumerate sidecars for propagation — skip with
        // no error in that case.
        let state = match self.source.store().get_state(state_id)? {
            Some(state) => state,
            None if state_already_present => return Ok(()),
            None => return Err(anyhow!("State {} not found in source", state_id)),
        };

        let transfer_plan = match wire::plan_state_transfer_with_options(
            self.source.store(),
            *state_id,
            StateClosureOptions {
                depth: Some(0),
                exclude_states: Vec::new(),
            },
        ) {
            Ok(plan) => Some(plan),
            Err(wire::ProtocolError::ObjectNotFound(_)) if state_already_present => None,
            Err(err) => return Err(err.into()),
        };
        if let Some(transfer_plan) = transfer_plan {
            self.apply_transfer_plan(target, &transfer_plan, !state_already_present, copied)?;
        } else {
            self.propagate_state_visibility_for_state(target, state_id)?;
            self.copy_state_blob_dependencies(target, &state, copied)?;
        }

        // Copy parent states recursively (if depth allows). We recurse
        // on parents even when the current state was already present —
        // a redaction declared on an ancestor blob still needs to
        // reach the target's redactions store.
        if let Some(depth) = max_depth {
            if depth > 0 {
                for parent in &state.parents {
                    self.copy_state_recursive(target, parent, visited, copied, Some(depth - 1))?;
                }
            } else {
                // Shallow state - mark parents as grafted
                if !state_already_present {
                    target.set_shallow(state_id, &state.parents)?;
                }
            }
        } else {
            for parent in &state.parents {
                self.copy_state_recursive(target, parent, visited, copied, None)?;
            }
        }

        if !state_already_present || state_metadata_roots_changed(target_state.as_ref(), &state) {
            target.store().put_state(&state)?;
            if !state_already_present {
                *copied += 1;
            }
        }

        Ok(())
    }

    fn apply_transfer_plan(
        &self,
        target: &Repository,
        plan: &ObjectTransferPlan,
        copy_objects: bool,
        copied: &mut usize,
    ) -> Result<()> {
        for object in plan.objects() {
            match (&object.id, object.obj_type) {
                (ObjectId::Hash(hash), ObjectType::Blob) if copy_objects => {
                    if self.copy_blob(target, hash)? {
                        *copied += 1;
                    }
                }
                (ObjectId::Hash(_), ObjectType::Blob) => {}
                (ObjectId::Hash(hash), ObjectType::Tree) if copy_objects => {
                    self.copy_tree(target, hash, copied)?;
                }
                (ObjectId::Hash(_), ObjectType::Tree) => {}
                (ObjectId::ChangeId(_), ObjectType::State) => {}
                (ObjectId::Hash(blob), ObjectType::Redaction) => {
                    self.propagate_redactions_for_blob(target, blob)?;
                }
                (ObjectId::ChangeId(state), ObjectType::StateVisibility) => {
                    self.propagate_state_visibility_for_state(target, state)?;
                }
                (_, ObjectType::Action) => {
                    return Err(anyhow!("Action transfer is not supported by local sync"));
                }
                _ => return Err(anyhow!("object id/type mismatch in local transfer plan")),
            }
        }
        Ok(())
    }

    fn copy_tree(&self, target: &Repository, hash: &ContentHash, copied: &mut usize) -> Result<()> {
        if target.store().has_tree(hash)? {
            return Ok(());
        }
        let tree = self
            .source
            .store()
            .get_tree(hash)?
            .ok_or_else(|| anyhow!("Tree {} not found in source", hash))?;
        target.store().put_tree(&tree)?;
        *copied += 1;
        Ok(())
    }

    fn copy_state_blob_dependencies(
        &self,
        target: &Repository,
        state: &objects::object::State,
        copied: &mut usize,
    ) -> Result<()> {
        for hash in [
            state.risk_signals,
            state.review_signatures,
            state.discussions,
            state.structured_conflicts,
        ]
        .into_iter()
        .flatten()
        {
            if self.copy_blob(target, &hash)? {
                *copied += 1;
            }
        }
        Ok(())
    }

    /// If the source repository has any redactions for `blob`, ferry
    /// the sidecar bytes through `Repository::accept_wire_redactions`
    /// on the target so signatures are verified and any `purged_at`
    /// records trigger a local purge on the target.
    ///
    /// `LocalSync` is local→local, so we use the same wire-side
    /// contract as the network path — same signature requirement,
    /// same idempotency. Operators who redact unsigned locally and
    /// expect that to propagate via a local fetch will see a clear
    /// rejection rather than a silent skip.
    fn propagate_redactions_for_blob(&self, target: &Repository, blob: &ContentHash) -> Result<()> {
        let Some(bytes) = self.source.store().get_redactions_bytes_for_blob(blob)? else {
            return Ok(());
        };
        target.accept_wire_redactions(*blob, &bytes)?;
        Ok(())
    }

    /// If the source repository has state-visibility records for `state`,
    /// ferry the sidecar bytes through the same repository boundary used by
    /// the network path.
    fn propagate_state_visibility_for_state(
        &self,
        target: &Repository,
        state: &ChangeId,
    ) -> Result<()> {
        let Some(bytes) = self.source.get_state_visibility_bytes_for_state(state)? else {
            return Ok(());
        };
        target.accept_wire_state_visibility(*state, &bytes)?;
        Ok(())
    }

    /// Copy a specific blob from source to target.
    pub fn copy_blob(&self, target: &Repository, hash: &ContentHash) -> Result<bool> {
        if target.store().has_blob(hash)? {
            return Ok(false);
        }

        let blob = self.source.require_blob(hash)?;

        target.store().put_blob(&blob)?;
        Ok(true)
    }
}

fn state_metadata_roots_changed(
    target_state: Option<&objects::object::State>,
    source_state: &objects::object::State,
) -> bool {
    let Some(target_state) = target_state else {
        return true;
    };
    target_state.risk_signals != source_state.risk_signals
        || target_state.review_signatures != source_state.review_signatures
        || target_state.discussions != source_state.discussions
        || target_state.structured_conflicts != source_state.structured_conflicts
}

#[cfg(test)]
mod tests {
    use objects::{object::Blob, store::ObjectStore};
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn fetch_state_copies_tail_metadata_blobs_from_transfer_plan() {
        let source_dir = TempDir::new().unwrap();
        let source = Repository::init_default(source_dir.path()).unwrap();
        std::fs::write(source_dir.path().join("README.md"), "hello\n").unwrap();
        let state = source.snapshot(Some("seed".to_string()), None).unwrap();
        let risk = source
            .store()
            .put_blob(&Blob::from("risk signals\n"))
            .unwrap();
        let state_with_risk = state.with_risk_signals(risk);
        let state_id = state_with_risk.change_id;
        source.store().put_state(&state_with_risk).unwrap();

        let sync = LocalSync::open(source_dir.path()).unwrap();
        let target_dir = TempDir::new().unwrap();
        let target = Repository::init_default(target_dir.path()).unwrap();

        let copied = sync.fetch_state(&target, &state_id).unwrap();

        assert!(copied > 0);
        assert!(target.store().has_blob(&risk).unwrap());
        assert_eq!(
            target
                .store()
                .get_state(&state_id)
                .unwrap()
                .unwrap()
                .risk_signals,
            Some(risk)
        );
    }
}
