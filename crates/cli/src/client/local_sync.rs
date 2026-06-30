// SPDX-License-Identifier: Apache-2.0
//! Local repository synchronization.
//!
//! Direct access to local repositories without network protocol overhead.

use std::{collections::HashSet, path::Path};

use anyhow::{Result, anyhow};
use objects::{
    object::{ChangeId, ContentHash, EntryType, Tree},
    store::ObjectStore,
};
use repo::Repository;

/// Synchronize objects from a local source repository to a target repository.
pub struct LocalSync {
    source: Repository,
}

struct TreeTransferPlan {
    trees: Vec<PlannedTree>,
    blobs: Vec<ContentHash>,
}

struct PlannedTree {
    hash: ContentHash,
    tree: Tree,
}

impl TreeTransferPlan {
    fn gather(
        source: &Repository,
        roots: impl IntoIterator<Item = ContentHash>,
        require_complete: bool,
    ) -> Result<Self> {
        let mut visitor = TreeTransferVisitor::new(source, require_complete);
        for root in roots {
            visitor.visit_tree(&root)?;
        }
        Ok(visitor.finish())
    }

    fn propagate_redactions(&self, sync: &LocalSync, target: &Repository) -> Result<()> {
        for blob in &self.blobs {
            sync.propagate_redactions_for_blob(target, blob)?;
        }
        Ok(())
    }

    fn copy_objects(
        &self,
        sync: &LocalSync,
        target: &Repository,
        copied: &mut usize,
    ) -> Result<()> {
        for hash in &self.blobs {
            if !target.store().has_blob(hash)? {
                let blob = sync.source.require_blob(hash)?;
                target.store().put_blob(&blob)?;
                *copied += 1;
            }
        }
        for planned in &self.trees {
            if !target.store().has_tree(&planned.hash)? {
                target.store().put_tree(&planned.tree)?;
                *copied += 1;
            }
        }
        Ok(())
    }
}

struct TreeTransferVisitor<'source> {
    source: &'source Repository,
    require_complete: bool,
    visited_trees: HashSet<ContentHash>,
    visited_blobs: HashSet<ContentHash>,
    plan: TreeTransferPlan,
}

impl<'source> TreeTransferVisitor<'source> {
    fn new(source: &'source Repository, require_complete: bool) -> Self {
        Self {
            source,
            require_complete,
            visited_trees: HashSet::new(),
            visited_blobs: HashSet::new(),
            plan: TreeTransferPlan {
                trees: Vec::new(),
                blobs: Vec::new(),
            },
        }
    }

    fn visit_tree(&mut self, tree_hash: &ContentHash) -> Result<()> {
        if !self.visited_trees.insert(*tree_hash) {
            return Ok(());
        }

        let Some(tree) = self.source.store().get_tree(tree_hash)? else {
            if self.require_complete {
                return Err(anyhow!("Tree {} not found in source", tree_hash));
            }
            return Ok(());
        };

        for entry in tree.entries() {
            match entry.entry_type {
                EntryType::Blob | EntryType::Symlink => {
                    if self.visited_blobs.insert(entry.hash) {
                        self.plan.blobs.push(entry.hash);
                    }
                }
                EntryType::Tree => {
                    self.visit_tree(&entry.hash)?;
                }
            }
        }
        self.plan.trees.push(PlannedTree {
            hash: *tree_hash,
            tree,
        });
        Ok(())
    }

    fn finish(self) -> TreeTransferPlan {
        self.plan
    }
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

        // Always propagate per-state visibility and per-blob redactions,
        // regardless of whether the objects themselves need copying.
        self.propagate_state_visibility_for_state(target, state_id)?;
        let transfer_plan = TreeTransferPlan::gather(
            &self.source,
            [Some(state.tree), state.provenance, state.context]
                .into_iter()
                .flatten(),
            !state_already_present,
        )?;
        transfer_plan.propagate_redactions(self, target)?;

        if !state_already_present {
            transfer_plan.copy_objects(self, target, copied)?;
        }
        self.copy_state_blob_dependencies(target, &state, copied)?;

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
            self.copy_blob_dependency(target, &hash, copied)?;
        }
        Ok(())
    }

    fn copy_blob_dependency(
        &self,
        target: &Repository,
        hash: &ContentHash,
        copied: &mut usize,
    ) -> Result<()> {
        if target.store().has_blob(hash)? {
            return Ok(());
        }
        let blob = self.source.require_blob(hash)?;
        target.store().put_blob(&blob)?;
        *copied += 1;
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
