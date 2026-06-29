// SPDX-License-Identifier: Apache-2.0
use std::{collections::BTreeSet, path::Path};

use objects::{
    object::{ContentHash, EntryType, FileProvenance, State, Tree, TreeEntry},
    store::ObjectStore,
};

use super::{
    Repository, Result,
    builder::ProvenanceBuilder,
    helpers::{
        build_single_origin_provenance, expand_line_origin_sets_with_builder, lcs_line_matches,
        load_lines_for_hash, lookup_tree_entry, split_text_lines,
        synthesize_file_provenance_from_blob,
    },
};

/// Bundle of everything we need from one parent state when synthesizing
/// the current state's provenance: the state itself (for origin), its
/// tree (so we can find the parent's version of each path), and its
/// own provenance root (recursively built — `None` only when the parent
/// is itself a binary-or-empty file or has no per-line attribution).
pub(crate) struct ParentRef<'a> {
    pub state: &'a State,
    pub tree: Tree,
    pub provenance_root: Option<ContentHash>,
}

impl Repository {
    /// Synthesize provenance for `state` against zero or more parents.
    ///
    /// - **0 parents** (root commit): every line of every file is
    ///   attributed to `state`.
    /// - **1 parent** (linear commit): standard snapshot diff against
    ///   the parent — lines unchanged from the parent inherit the
    ///   parent's origins, lines new to this state are attributed
    ///   to it.
    /// - **N ≥ 2 parents** (merge commit): for each line in the
    ///   current file, walk every parent's version of that path, take
    ///   an LCS match against each, and union the parents' origin
    ///   sets for matched lines. This is what saves merge-commit
    ///   blame on imported repos: a line that came in from the
    ///   side-branch is correctly attributed to whoever wrote it on
    ///   that branch, not to the maintainer who pressed the merge
    ///   button.
    ///
    /// Lines unique to the current state (no LCS match against any
    /// parent) get the current state's origin.
    ///
    /// # Why not just use `merge.rs`?
    ///
    /// `build_merge_provenance_root` in `merge.rs` is for heddle's
    /// *explicit* three-way merge UX — it has named ours/theirs/base
    /// inputs because heddle's merge command exposes those concepts to
    /// the user. The path here covers the topological case: a state
    /// with N parents in the commit graph, no semantic distinction
    /// between them. Both paths share helpers for the line-level
    /// LCS work; they differ only in how they collect the parent
    /// inputs.
    pub(crate) fn build_provenance_from_parents(
        &self,
        state: &State,
        parents: &[ParentRef<'_>],
    ) -> Result<Option<ContentHash>> {
        let current_tree = self
            .store
            .get_tree(&state.tree)?
            .ok_or_else(|| super::HeddleError::NotFound(format!("tree {}", state.tree)))?;

        let Some(tree_hash) =
            self.build_provenance_tree_recursive(Path::new(""), &current_tree, parents, state)?
        else {
            return Ok(None);
        };
        Ok(Some(tree_hash))
    }

    fn build_provenance_tree_recursive(
        &self,
        path: &Path,
        current_tree: &Tree,
        parents: &[ParentRef<'_>],
        current_state: &State,
    ) -> Result<Option<ContentHash>> {
        let mut entries = Vec::new();

        for entry in current_tree.entries() {
            let entry_path = path.join(&entry.name);
            match entry.entry_type {
                EntryType::Tree => {
                    let Some(subtree) = self.store.get_tree(&entry.hash)? else {
                        continue;
                    };
                    // For each parent, descend into the same-name
                    // subtree if one exists. Parents that lack the
                    // subdir simply contribute nothing for this branch
                    // of the recursion.
                    let mut parent_subtrees: Vec<ParentRef<'_>> = Vec::new();
                    for parent in parents {
                        let Some(child_entry) = parent.tree.get(&entry.name) else {
                            continue;
                        };
                        if !child_entry.is_tree() {
                            continue;
                        }
                        let Some(subtree_obj) = self.store.get_tree(&child_entry.hash)? else {
                            continue;
                        };
                        parent_subtrees.push(ParentRef {
                            state: parent.state,
                            tree: subtree_obj,
                            provenance_root: parent.provenance_root,
                        });
                    }
                    if let Some(sub_hash) = self.build_provenance_tree_recursive(
                        &entry_path,
                        &subtree,
                        &parent_subtrees,
                        current_state,
                    )? {
                        entries.push(TreeEntry::directory(entry.name.clone(), sub_hash)?);
                    }
                }
                EntryType::Blob => {
                    if let Some(hash) = self.build_file_provenance_n_parents(
                        &entry_path,
                        entry,
                        parents,
                        current_state,
                    )? {
                        entries.push(TreeEntry::file(entry.name.clone(), hash, false)?);
                    }
                }
                EntryType::Symlink => {}
            }
        }

        if entries.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.store.put_tree(&Tree::from_entries(entries))?))
    }

    /// Compute one file's provenance against an arbitrary number of
    /// parents. Implements the algorithm described in
    /// [`Repository::build_provenance_from_parents`].
    fn build_file_provenance_n_parents(
        &self,
        path: &Path,
        current_entry: &TreeEntry,
        parents: &[ParentRef<'_>],
        current_state: &State,
    ) -> Result<Option<ContentHash>> {
        let Some(current_blob) = self.store.get_blob(&current_entry.hash)? else {
            return Ok(None);
        };
        let Some(current_lines) = split_text_lines(current_blob.content()) else {
            return Ok(None);
        };

        // Resolve each parent's `(entry, file_provenance)` for this
        // path. A parent that doesn't have the file at all contributes
        // nothing.
        let mut parent_provenances: Vec<FileProvenance> = Vec::with_capacity(parents.len());
        let mut parent_blob_hashes: Vec<ContentHash> = Vec::with_capacity(parents.len());
        for parent in parents {
            let Some(parent_entry) = lookup_tree_entry(self, &parent.tree, path) else {
                continue;
            };
            if !parent_entry.is_blob() {
                continue;
            }
            let parent_blob = self.store.get_blob(&parent_entry.hash)?;

            // Pull from the parent's stored provenance root if we have
            // one, otherwise synthesize a single-origin record for the
            // parent's current blob (parent had no per-line history of
            // its own — typically because it's also a fresh import).
            let provenance = match parent.provenance_root {
                Some(root) => self
                    .get_file_provenance_from_root(&root, path)?
                    .or_else(|| {
                        synthesize_file_provenance_from_blob(parent_blob.as_ref(), parent.state)
                    }),
                None => synthesize_file_provenance_from_blob(parent_blob.as_ref(), parent.state),
            };
            if let Some(p) = provenance {
                parent_blob_hashes.push(parent_entry.hash);
                parent_provenances.push(p);
            }
        }

        // Fast path: any parent has the same blob hash as the current
        // file. The content is byte-identical so the parent's
        // provenance is also the answer for this file at this state.
        // First match wins (deterministic order = parent order).
        for (i, parent_blob_hash) in parent_blob_hashes.iter().enumerate() {
            if *parent_blob_hash == current_entry.hash {
                return Ok(Some(self.put_file_provenance(&parent_provenances[i])?));
            }
        }

        // Genuinely new file (no parent had it): every line is
        // attributed to the current state.
        if parent_provenances.is_empty() {
            let current_origin = self.state_origin(current_state);
            let provenance =
                build_single_origin_provenance(current_entry.hash, &current_lines, current_origin);
            return Ok(Some(self.put_file_provenance(&provenance)?));
        }

        // General case: merge line origins across N parents. Falls
        // through cleanly to the 1-parent case (one source means
        // standard snapshot diff) and the 2+-parent case (union).
        let merged = self.merge_line_provenance_n_parents(
            current_entry.hash,
            &current_lines,
            &parent_provenances,
            self.state_origin(current_state),
        )?;
        Ok(Some(self.put_file_provenance(&merged)?))
    }

    /// Generalization of `merge::merge_file_provenance` to an arbitrary
    /// number of parent sources. For each line in `final_lines`, walk
    /// every source's LCS matching against `final_lines`; for matched
    /// lines, take the union of the source's origin set for that
    /// match. Lines that don't match any source get attributed to
    /// `final_origin`.
    fn merge_line_provenance_n_parents(
        &self,
        file_blob: ContentHash,
        final_lines: &[String],
        sources: &[FileProvenance],
        final_origin: objects::object::Origin,
    ) -> Result<FileProvenance> {
        let mut builder = ProvenanceBuilder::default();
        let mut source_lines: Vec<Vec<String>> = Vec::with_capacity(sources.len());
        let mut source_sets: Vec<Vec<u32>> = Vec::with_capacity(sources.len());

        for provenance in sources {
            source_lines.push(load_lines_for_hash(self, provenance.file_blob)?);
            source_sets.push(expand_line_origin_sets_with_builder(
                provenance,
                &mut builder,
            )?);
        }

        let final_origin_set = builder.origin_set_from_origins([final_origin]);
        let mut resolved_sets = vec![BTreeSet::<u32>::new(); final_lines.len()];

        for (lines, origin_sets) in source_lines.iter().zip(source_sets.iter()) {
            let matches = lcs_line_matches(lines, final_lines);
            for (old_index, new_index) in matches {
                if let Some(set_index) = origin_sets.get(old_index) {
                    resolved_sets[new_index].insert(*set_index);
                }
            }
        }

        let mut line_sets = Vec::with_capacity(resolved_sets.len());
        for set in resolved_sets {
            line_sets.push(if set.is_empty() {
                final_origin_set
            } else {
                builder.origin_set_from_set_indexes(set)?
            });
        }

        Ok(builder.into_file_provenance(file_blob, final_lines.len(), line_sets))
    }
}
