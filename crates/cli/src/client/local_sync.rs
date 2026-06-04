// SPDX-License-Identifier: Apache-2.0
//! Local repository synchronization.
//!
//! Direct access to local repositories without network protocol overhead.

use objects::store::ObjectStore;
use std::{collections::HashSet, path::Path};

use anyhow::{anyhow, Result};
use objects::object::{ChangeId, ContentHash};
use repo::Repository;

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
        let state_already_present = target.store().has_state(state_id)?;

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
        let mut propagated_trees: HashSet<ContentHash> = HashSet::new();
        self.propagate_redactions_in_tree(target, &state.tree, &mut propagated_trees)?;
        if let Some(provenance_root) = state.provenance {
            self.propagate_redactions_in_tree(target, &provenance_root, &mut propagated_trees)?;
        }
        if let Some(context_root) = state.context {
            self.propagate_redactions_in_tree(target, &context_root, &mut propagated_trees)?;
        }

        if !state_already_present {
            // Copy tree recursively
            self.copy_tree_recursive(target, &state.tree, copied)?;
            if let Some(provenance_root) = state.provenance {
                self.copy_tree_recursive(target, &provenance_root, copied)?;
            }
            if let Some(context_root) = state.context {
                self.copy_tree_recursive(target, &context_root, copied)?;
            }
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

        if !state_already_present {
            target.store().put_state(&state)?;
            *copied += 1;
        }

        Ok(())
    }

    fn copy_tree_recursive(
        &self,
        target: &Repository,
        tree_hash: &ContentHash,
        copied: &mut usize,
    ) -> Result<()> {
        // Check if target already has this tree
        if target.store().has_tree(tree_hash)? {
            return Ok(());
        }

        // Get the tree from source
        let tree = self
            .source
            .store()
            .get_tree(tree_hash)?
            .ok_or_else(|| anyhow!("Tree {} not found in source", tree_hash))?;

        // Copy all blobs and sub-trees. Redaction propagation lives
        // in `propagate_redactions_in_tree`, called by
        // `copy_state_recursive` regardless of whether the tree was
        // already present — so it's intentionally absent here.
        for entry in tree.entries() {
            match entry.entry_type {
                objects::object::EntryType::Blob => {
                    if !target.store().has_blob(&entry.hash)? {
                        let blob = self.source.require_blob(&entry.hash)?;
                        target.store().put_blob(&blob)?;
                        *copied += 1;
                    }
                }
                objects::object::EntryType::Tree => {
                    self.copy_tree_recursive(target, &entry.hash, copied)?;
                }
                objects::object::EntryType::Symlink => {
                    if !target.store().has_blob(&entry.hash)? {
                        let blob = self.source.require_blob(&entry.hash)?;
                        target.store().put_blob(&blob)?;
                        *copied += 1;
                    }
                }
            }
        }

        // Store the tree in target
        target.store().put_tree(&tree)?;
        *copied += 1;

        Ok(())
    }

    /// Walk a source-side tree and propagate any redaction sidecars
    /// found for the blobs it references. Runs regardless of whether
    /// the tree (or its parent state) is already present on the
    /// target — the whole point is to recover from the "redact-after-
    /// peer-fetched" flow where the object graph is unchanged but a
    /// new sidecar exists upstream.
    ///
    /// `propagated_trees` dedups within a single sync so we don't
    /// re-walk the same subtree across `state.tree`, `provenance`,
    /// and `context` roots that happen to share content.
    fn propagate_redactions_in_tree(
        &self,
        target: &Repository,
        tree_hash: &ContentHash,
        propagated_trees: &mut HashSet<ContentHash>,
    ) -> Result<()> {
        if !propagated_trees.insert(*tree_hash) {
            return Ok(());
        }

        // Tree must come from the source — if it's missing there, we
        // can't enumerate blob hashes for sidecar lookup. Treat as a
        // gap in propagation (best-effort), not a hard failure.
        let Some(tree) = self.source.store().get_tree(tree_hash)? else {
            return Ok(());
        };

        for entry in tree.entries() {
            match entry.entry_type {
                objects::object::EntryType::Blob | objects::object::EntryType::Symlink => {
                    self.propagate_redactions_for_blob(target, &entry.hash)?;
                }
                objects::object::EntryType::Tree => {
                    self.propagate_redactions_in_tree(target, &entry.hash, propagated_trees)?;
                }
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
