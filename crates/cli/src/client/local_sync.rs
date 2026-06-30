// SPDX-License-Identifier: Apache-2.0
//! Local repository synchronization.
//!
//! Direct access to local repositories without network protocol overhead.

use std::{
    collections::{HashSet, VecDeque},
    path::Path,
};

use anyhow::{Result, anyhow};
use objects::{
    object::{ActionId, ChangeId, ContentHash},
    store::ObjectStore,
};
use repo::Repository;
use wire::{
    GitLaneTransferIntent, ObjectId, ObjectType, PlannedObject, RepositoryTransferPlan,
    StateClosureOptions,
};

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
        let transfer_plan = self.plan_state_transfer(*state_id, None)?;
        self.copy_transfer_plan(target, &transfer_plan)
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
        let transfer_plan = self.plan_state_transfer(*state_id, Some(depth))?;
        let copied = self.copy_transfer_plan(target, &transfer_plan)?;
        self.mark_shallow_boundaries(target, *state_id, depth)?;
        Ok(copied)
    }

    fn plan_state_transfer(
        &self,
        state_id: ChangeId,
        depth: Option<u32>,
    ) -> Result<RepositoryTransferPlan> {
        // Local sync still executes through the existing dependency-preserving
        // recursive copy path. The shared plan gives local and hosted Heddle
        // object sync the same partition/stats contract without introducing a
        // second local storage executor.
        Ok(RepositoryTransferPlan::from_state_closure_plan(
            self.source.store(),
            state_id,
            StateClosureOptions {
                depth,
                exclude_states: Vec::new(),
            },
            GitLaneTransferIntent::HeddleObjectsOnly,
        )?)
    }

    fn copy_transfer_plan(
        &self,
        target: &Repository,
        transfer_plan: &RepositoryTransferPlan,
    ) -> Result<usize> {
        let mut copied = 0;
        for object in &transfer_plan.partitions.packable_objects {
            if self.copy_planned_object(target, object)? {
                copied += 1;
            }
        }
        for object in &transfer_plan.partitions.sidecar_objects {
            self.copy_planned_sidecar(target, object)?;
        }
        Ok(copied)
    }

    fn copy_planned_object(&self, target: &Repository, object: &PlannedObject) -> Result<bool> {
        match (&object.id, object.obj_type) {
            (ObjectId::Hash(hash), ObjectType::Blob) => self.copy_blob(target, hash),
            (ObjectId::Hash(hash), ObjectType::Tree) => self.copy_tree(target, hash),
            (ObjectId::Hash(hash), ObjectType::Action) => self.copy_action(target, hash),
            (ObjectId::ChangeId(state_id), ObjectType::State) => self.copy_state(target, state_id),
            (_, ObjectType::Redaction | ObjectType::StateVisibility) => Ok(false),
            (id, obj_type) => Err(anyhow!(
                "transfer plan object {id:?} has incompatible type {obj_type:?}"
            )),
        }
    }

    fn copy_tree(&self, target: &Repository, tree_hash: &ContentHash) -> Result<bool> {
        if target.store().has_tree(tree_hash)? {
            return Ok(false);
        }
        let tree = self
            .source
            .store()
            .get_tree(tree_hash)?
            .ok_or_else(|| anyhow!("Tree {} not found in source", tree_hash))?;
        target.store().put_tree(&tree)?;
        Ok(true)
    }

    fn copy_action(&self, target: &Repository, hash: &ContentHash) -> Result<bool> {
        let action_id = ActionId::from_hash(*hash);
        if target.store().get_action(&action_id)?.is_some() {
            return Ok(false);
        }
        let mut action = self
            .source
            .store()
            .get_action(&action_id)?
            .ok_or_else(|| anyhow!("Action {} not found in source", hash))?;
        target.store().put_action(&mut action)?;
        Ok(true)
    }

    fn copy_state(&self, target: &Repository, state_id: &ChangeId) -> Result<bool> {
        let target_state = target.store().get_state(state_id)?;
        let state_already_present = target_state.is_some();
        let state = self
            .source
            .store()
            .get_state(state_id)?
            .ok_or_else(|| anyhow!("State {} not found in source", state_id))?;

        if !state_already_present || state_metadata_roots_changed(target_state.as_ref(), &state) {
            target.store().put_state(&state)?;
        }
        Ok(!state_already_present)
    }

    fn copy_planned_sidecar(&self, target: &Repository, object: &PlannedObject) -> Result<()> {
        match (&object.id, object.obj_type) {
            (ObjectId::Hash(hash), ObjectType::Redaction) => {
                self.propagate_redactions_for_blob(target, hash)
            }
            (ObjectId::ChangeId(state_id), ObjectType::StateVisibility) => {
                self.propagate_state_visibility_for_state(target, state_id)
            }
            (_, ObjectType::Blob | ObjectType::Tree | ObjectType::State | ObjectType::Action) => {
                Ok(())
            }
            (id, obj_type) => Err(anyhow!(
                "transfer plan sidecar {id:?} has incompatible type {obj_type:?}"
            )),
        }
    }

    fn mark_shallow_boundaries(
        &self,
        target: &Repository,
        state_id: ChangeId,
        max_depth: u32,
    ) -> Result<()> {
        let mut seen: HashSet<ChangeId> = HashSet::new();
        let mut queue = VecDeque::from([(state_id, 0u32)]);
        while let Some((id, depth)) = queue.pop_front() {
            if !seen.insert(id) {
                continue;
            }
            let state = self
                .source
                .store()
                .get_state(&id)?
                .ok_or_else(|| anyhow!("State {} not found in source", id))?;
            if depth == max_depth {
                if !state.parents.is_empty() {
                    target.set_shallow(&id, &state.parents)?;
                }
                continue;
            }
            for parent in &state.parents {
                queue.push_back((*parent, depth + 1));
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
