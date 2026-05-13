// SPDX-License-Identifier: Apache-2.0
//! Provenance helpers for preserving line-level blame across rewrites.

mod blame_file;
mod builder;
mod helpers;
mod merge;
mod snapshot;
mod storage;
#[cfg(test)]
mod tests;

use std::{collections::HashMap, path::Path};

use objects::object::{ChangeId, ContentHash, FileProvenance, State};

use super::{HeddleError, Repository, Result};

/// Memoization cache for [`Repository::get_state_provenance_root`] and
/// its recursive walk through `state.parents`.
///
/// One blame query against a state at depth N in the commit graph would
/// otherwise re-derive every ancestor's provenance root once per
/// reachability path. With merges in the history that's super-linear
/// — the same ancestor is reached via multiple sibling branches and
/// recomputed each time. The cache turns it back into a single walk.
///
/// Lifetime is one query: `get_state_provenance_root` builds a fresh
/// cache and threads it through the recursion. We deliberately don't
/// keep a process-wide cache yet — Repository handles can outlive any
/// single query, and a bounded per-query lifetime keeps memory usage
/// proportional to "states reachable from the asked-about state" rather
/// than "states ever asked about." A longer-lived cache is a follow-up
/// when we have measured workloads to size it against.
#[derive(Debug, Default)]
pub(crate) struct ProvenanceCache {
    roots: HashMap<ChangeId, Option<ContentHash>>,
}

impl Repository {
    pub fn state_origin(&self, state: &State) -> objects::object::Origin {
        objects::object::Origin {
            state_id: state.change_id,
            attribution: state.attribution.clone(),
            created_at: state.created_at,
            // Forward `authored_at` from the state into the
            // origin record. For native heddle commits this is `None`;
            // the git-ingest importer populates it so blame can show
            // the original author time even when committer time
            // (created_at) was overwritten by a rebase / squash /
            // amend / cherry-pick.
            authored_at: state.authored_at,
        }
    }

    pub fn get_state_provenance_root(&self, state: &State) -> Result<Option<ContentHash>> {
        // Top-level entry point: build a fresh cache and let the
        // recursion reuse it. The cache lives only for the duration
        // of this call, so memory is bounded by the ancestry size of
        // `state` rather than by repository-lifetime accumulation.
        let mut cache = ProvenanceCache::default();
        self.get_state_provenance_root_cached(state, &mut cache)
    }

    /// Recursive variant that reuses an explicit memoization cache
    /// across parent walks. Sibling branches that share an ancestor
    /// (every merge does) hit the cache instead of re-deriving the
    /// ancestor's provenance.
    pub(crate) fn get_state_provenance_root_cached(
        &self,
        state: &State,
        cache: &mut ProvenanceCache,
    ) -> Result<Option<ContentHash>> {
        if let Some(cached) = cache.roots.get(&state.change_id) {
            return Ok(*cached);
        }

        // The state's stored `provenance` (when set by the explicit
        // merge UX) short-circuits the walk entirely. Cache it so we
        // don't pay the field read again on a sibling-branch query.
        if let Some(root) = state.provenance {
            cache.roots.insert(state.change_id, Some(root));
            return Ok(Some(root));
        }

        // Walk every parent (not just first). For non-merge states this
        // is a slice of length one; for merge commits — including the
        // imported variants from `bridge git ingest` — it's the full
        // parent set so a line introduced by the side branch gets
        // attributed to *its* author rather than to whoever pressed
        // the merge button. Octopus merges (3+ parents) fall out for
        // free.
        let parent_states: Vec<State> = state
            .parents
            .iter()
            .filter_map(|id| self.store.get_state(id).unwrap_or_default())
            .collect();
        if state.parents.len() != parent_states.len() {
            // A parent we couldn't load shouldn't break the build —
            // happens with incremental imports where a parent predates
            // the imported window. The walk continues with whatever we
            // could find; lines that came in via the missing parent
            // fall through to the current-state attribution rather
            // than crediting an arbitrary surviving parent.
            tracing::debug!(
                state = %state.change_id,
                missing = state.parents.len() - parent_states.len(),
                "some parent states unavailable while building provenance"
            );
        }

        // Resolve each parent's tree + recursive provenance root
        // before we hand the slice to the snapshot builder. The
        // recursive call reuses `cache`, so a diamond ancestor
        // (reached via two parents that themselves share a parent)
        // collapses to one walk.
        let mut parent_refs: Vec<snapshot::ParentRef<'_>> = Vec::with_capacity(parent_states.len());
        for parent_state in &parent_states {
            let Some(tree) = self.store.get_tree(&parent_state.tree)? else {
                continue;
            };
            let provenance_root = self.get_state_provenance_root_cached(parent_state, cache)?;
            parent_refs.push(snapshot::ParentRef {
                state: parent_state,
                tree,
                provenance_root,
            });
        }

        let result = self.build_provenance_from_parents(state, &parent_refs)?;
        cache.roots.insert(state.change_id, result);
        Ok(result)
    }

    pub fn get_file_provenance_for_state(
        &self,
        state: &State,
        path: &Path,
    ) -> Result<Option<FileProvenance>> {
        // Two cases:
        //
        // 1. `state.provenance` is set — the explicit merge UX (or a
        //    future precompute path) already pinned a provenance tree
        //    root. Look up the file inside it. O(1) blob fetch.
        //
        // 2. `state.provenance` is None — the typical imported case.
        //    The tree-oriented `build_provenance_from_parents` walk
        //    is structurally O(ancestors × files-per-tree), which
        //    runs into a wall on deep imported histories like a
        //    git→heddle of ripgrep. Use the path-targeted blame walk
        //    instead: same output shape, complexity bounded by the
        //    file's *change* history rather than the repo's commit
        //    history. Mirrors what `git blame` does internally.
        if let Some(root) = state.provenance {
            return self.get_file_provenance_from_root(&root, path);
        }
        self.blame_file_via_path_walk(state, path)
    }

    pub(crate) fn get_file_provenance_from_root(
        &self,
        root: &ContentHash,
        path: &Path,
    ) -> Result<Option<FileProvenance>> {
        let Some(blob_hash) = self.lookup_tree_leaf(root, path)? else {
            return Ok(None);
        };
        let Some(blob) = self.store.get_blob(&blob_hash)? else {
            return Ok(None);
        };
        let provenance: FileProvenance =
            rmp_serde::from_slice(blob.content()).map_err(|error| {
                HeddleError::InvalidObject(format!("invalid provenance blob: {error}"))
            })?;
        provenance.validate().map_err(|error| {
            HeddleError::InvalidObject(format!("invalid provenance blob: {error}"))
        })?;
        Ok(Some(provenance))
    }
}