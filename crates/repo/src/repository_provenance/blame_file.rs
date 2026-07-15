// SPDX-License-Identifier: Apache-2.0
//! Path-targeted blame, mirroring git blame's algorithm.
//!
//! # Why a separate path
//!
//! The tree-oriented `build_provenance_from_parents` is the right
//! primitive for "compute everything for state S" use cases (the
//! explicit merge UX, state-summary surfaces). But for `heddle blame
//! <one-file>` the access pattern is wildly different:
//!
//! - We care about **one path**, not the whole tree.
//! - Most ancestors didn't change that path (its tree-entry blob
//!   hash is identical to the parent's), so the per-ancestor work
//!   should be O(1) blob-hash compare, not O(F) tree walk.
//! - We can stop as soon as every line has been finalized — no need
//!   to traverse the full ancestry.
//!
//! Empirically this is the difference between sub-second and minutes
//! on the imported ripgrep history (2 249 commits). The previous
//! tree-oriented path read every ancestor's whole tree from disk;
//! this one reads one tree-entry per ancestor.
//!
//! # Algorithm
//!
//! Mirrors git's `blame.c` walk, simplified:
//!
//! 1. **Working set as line ranges with state pointers.** Each entry
//!    is `(state, blob_hash, lines_at_state, state_lines → target_lines)`.
//!    Initially one entry covering all target lines, attributed to the
//!    target state.
//! 2. **For each entry's parents, see if the file moved through.**
//!    - Same blob hash as entry: the whole entry walks back to the
//!      parent unchanged. Reassign origins, push the new entry, stop
//!      checking other parents.
//!    - Different blob hash but file exists: LCS line-match between
//!      parent's version and entry's. Lines that match → push new
//!      entry for the parent (those lines walk back). Lines that
//!      don't match → stay attributed to entry's state.
//!    - File doesn't exist in parent: the entry's lines didn't
//!      come from this parent.
//! 3. **Merge-aware via the `moved` bitvec.** When a state has
//!    multiple parents, each parent gets a turn at the entry's
//!    not-yet-moved lines. First parent to claim a line wins
//!    (matches git's default merge-aware blame).
//! 4. **Stop early.** When every working-set line has been
//!    finalized (every parent of every reachable entry has been
//!    processed), the walk terminates. For files with stable history
//!    that's a few ancestors deep; for files modified at every
//!    commit it's still bounded by the file's change history, not
//!    the repo's.
//!
//! # Output
//!
//! [`FileProvenance`] keyed by the target state's blob hash. Same
//! shape `get_file_provenance_for_state` returned in the tree-oriented
//! path, so every caller — `heddle blame`, the gRPC `GetBlame` RPC,
//! the web app — benefits without any client-side change.

use std::{collections::HashMap, path::Path};

use objects::{
    object::{ContentHash, FileProvenance, Origin, State, StateId},
    store::ObjectStore,
};

use super::{
    Repository, Result,
    builder::ProvenanceBuilder,
    helpers::{lcs_line_matches, lookup_tree_entry, split_text_lines},
};

impl Repository {
    /// Path-targeted blame: walk ancestry only along `path`'s
    /// blob-hash boundary and synthesize a [`FileProvenance`] for the
    /// resulting per-line attribution.
    ///
    /// Returns `Ok(None)` if `path` doesn't exist at `state` or if
    /// the file is binary (lines can't be split). Otherwise produces
    /// the same shape `get_file_provenance_for_state` would have
    /// produced under the tree-oriented path, but typically orders of
    /// magnitude faster on deep imported histories.
    pub(crate) fn blame_file_via_path_walk(
        &self,
        state: &State,
        path: &Path,
    ) -> Result<Option<FileProvenance>> {
        // 1. Resolve the target file at `state`.
        let Some(target_blob_hash) = self.lookup_blob_at_path(&state.tree, path)? else {
            return Ok(None);
        };
        let target_blob = match self.store.get_blob(&target_blob_hash)? {
            Some(b) => b,
            None => return Ok(None),
        };
        let target_lines = match split_text_lines(target_blob.content()) {
            Some(lines) => lines,
            None => return Ok(None),
        };

        let n = target_lines.len();
        if n == 0 {
            // Empty file — single empty provenance with current origin.
            let origin = self.state_origin(state);
            let mut builder = ProvenanceBuilder::default();
            let _ = builder.origin_set_from_origins([origin]);
            return Ok(Some(builder.into_file_provenance(
                target_blob_hash,
                0,
                Vec::new(),
            )));
        }

        // 2. Working state: per-target-line origin (mutated as we walk).
        let mut origins: Vec<Origin> = vec![self.state_origin(state); n];

        // Cache state objects so the worklist doesn't re-fetch the
        // same parents repeatedly. Bounded by ancestry size of the
        // target state, same shape as the provenance memo cache.
        let mut state_cache: HashMap<StateId, State> = HashMap::new();
        state_cache.insert(state.id(), state.clone());

        // 3. Worklist of frontiers we still need to walk back from.
        let mut worklist: Vec<BlameFrontier> = vec![BlameFrontier {
            state_id: state.id(),
            blob_hash: target_blob_hash,
            // Clone target_lines once. Subsequent frontiers will own
            // the parent's lines after each LCS step.
            state_lines: target_lines.clone(),
            state_to_target: (0..n).map(Some).collect(),
        }];

        while let Some(entry) = worklist.pop() {
            let entry_state = match state_cache.get(&entry.state_id) {
                Some(s) => s.clone(),
                None => match self.store.get_state(&entry.state_id)? {
                    Some(s) => {
                        state_cache.insert(s.id(), s.clone());
                        s
                    }
                    None => continue,
                },
            };

            if entry_state.parents.is_empty() {
                // Root commit — every still-tracked line stays
                // attributed to entry_state. (Their `origins[i]` was
                // already set when this entry was created, so nothing
                // to do.)
                continue;
            }

            // `moved[i]` is true when an earlier parent (in this
            // state's parent list) already claimed line `i` of the
            // entry. Subsequent parents skip that line — first match
            // wins, matching git's default merge-aware behavior.
            let mut moved: Vec<bool> = vec![false; entry.state_lines.len()];

            for parent_id in &entry_state.parents {
                let parent_state = match state_cache.get(parent_id) {
                    Some(s) => s.clone(),
                    None => match self.store.get_state(parent_id)? {
                        Some(s) => {
                            state_cache.insert(s.id(), s.clone());
                            s
                        }
                        None => continue,
                    },
                };

                let Some(parent_blob_hash) = self.lookup_blob_at_path(&parent_state.tree, path)?
                else {
                    // Parent doesn't have the file. None of the
                    // entry's lines came from this parent.
                    continue;
                };

                if parent_blob_hash == entry.blob_hash {
                    // **Fast path.** Same blob hash means the file is
                    // bit-identical between entry's state and this
                    // parent. Every not-yet-moved line walks back to
                    // the parent without LCS work.
                    let mut new_state_to_target = vec![None; entry.state_lines.len()];
                    let mut any_moved = false;
                    for (i, target_i_opt) in entry.state_to_target.iter().enumerate() {
                        if moved[i] {
                            continue;
                        }
                        if let Some(target_i) = target_i_opt {
                            origins[*target_i] = self.state_origin(&parent_state);
                            new_state_to_target[i] = Some(*target_i);
                            moved[i] = true;
                            any_moved = true;
                        }
                    }
                    if any_moved {
                        worklist.push(BlameFrontier {
                            state_id: parent_state.id(),
                            blob_hash: parent_blob_hash,
                            // Reuse entry.state_lines because the
                            // parent has byte-identical content.
                            state_lines: entry.state_lines.clone(),
                            state_to_target: new_state_to_target,
                        });
                    }
                    // Same-blob short-circuit: don't bother checking
                    // other parents for this entry. Their content
                    // would diverge from the line we just matched.
                    break;
                }

                // **Slow path.** Different blob — LCS to find which
                // lines walked back unchanged.
                let parent_blob = match self.store.get_blob(&parent_blob_hash)? {
                    Some(b) => b,
                    None => continue,
                };
                let parent_lines = match split_text_lines(parent_blob.content()) {
                    Some(lines) => lines,
                    None => continue,
                };

                let matches = lcs_line_matches(&parent_lines, &entry.state_lines);
                let mut parent_to_target: Vec<Option<usize>> = vec![None; parent_lines.len()];
                let mut any_moved = false;
                for (p_idx, e_idx) in matches {
                    if moved[e_idx] {
                        continue;
                    }
                    if let Some(target_i) = entry.state_to_target[e_idx] {
                        origins[target_i] = self.state_origin(&parent_state);
                        parent_to_target[p_idx] = Some(target_i);
                        moved[e_idx] = true;
                        any_moved = true;
                    }
                }
                if any_moved {
                    worklist.push(BlameFrontier {
                        state_id: parent_state.id(),
                        blob_hash: parent_blob_hash,
                        state_lines: parent_lines,
                        state_to_target: parent_to_target,
                    });
                }
            }

            // Lines not moved by *any* parent of this entry are
            // attributed to entry_state — which is already what
            // `origins[*]` says (set when this frontier was pushed).
            // No bookkeeping needed.
        }

        // 4. Build the FileProvenance from per-line origins.
        let mut builder = ProvenanceBuilder::default();
        let line_origin_sets: Vec<u32> = origins
            .into_iter()
            .map(|origin| builder.origin_set_from_origins([origin]))
            .collect();
        Ok(Some(builder.into_file_provenance(
            target_blob_hash,
            n,
            line_origin_sets,
        )))
    }

    /// Resolve `path` inside a tree (recursively for slashed paths)
    /// and return the file's blob hash if the leaf is a blob.
    /// Returns `None` if the path is missing or terminates at a tree
    /// or symlink rather than a blob.
    fn lookup_blob_at_path(
        &self,
        tree_hash: &ContentHash,
        path: &Path,
    ) -> Result<Option<ContentHash>> {
        let Some(tree) = self.store.get_tree(tree_hash)? else {
            return Ok(None);
        };
        let Some(entry) = lookup_tree_entry(self, &tree, path) else {
            return Ok(None);
        };
        Ok(entry.blob_hash())
    }
}

/// One frontier to walk back from.
///
/// `state_to_target` maps line indexes in `state_lines` (the file's
/// content at `state_id`) to the corresponding target-state line
/// indexes. `None` entries are lines that aren't (or aren't yet) being
/// tracked through this branch — they may have been claimed by a
/// sibling parent or finalized at an earlier step.
struct BlameFrontier {
    state_id: StateId,
    blob_hash: ContentHash,
    state_lines: Vec<String>,
    state_to_target: Vec<Option<usize>>,
}
