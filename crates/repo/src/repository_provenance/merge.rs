// SPDX-License-Identifier: Apache-2.0
use std::path::Path;

use objects::object::{ContentHash, EntryType, FileProvenance, State, Tree, TreeEntry};

use super::{
    Repository, Result,
    builder::ProvenanceBuilder,
    helpers::{
        expand_line_origin_sets_with_builder, lcs_line_matches, load_lines_for_hash,
        lookup_tree_entry, split_text_lines, synthesize_file_provenance_from_blob,
    },
};

impl Repository {
    pub(crate) fn build_merge_provenance_root(
        &self,
        state: &State,
        ours: &State,
        theirs: &State,
        base: Option<&State>,
    ) -> Result<Option<ContentHash>> {
        let final_tree = self
            .store
            .get_tree(&state.tree)?
            .ok_or_else(|| super::HeddleError::NotFound(format!("tree {}", state.tree)))?;
        let ours_tree = self
            .store
            .get_tree(&ours.tree)?
            .ok_or_else(|| super::HeddleError::NotFound(format!("tree {}", ours.tree)))?;
        let theirs_tree = self
            .store
            .get_tree(&theirs.tree)?
            .ok_or_else(|| super::HeddleError::NotFound(format!("tree {}", theirs.tree)))?;
        let base_tree = match base {
            Some(base_state) => self.store.get_tree(&base_state.tree)?,
            None => None,
        };
        let ours_root = self.get_state_provenance_root(ours)?;
        let theirs_root = self.get_state_provenance_root(theirs)?;
        let base_root = match base {
            Some(base_state) => self.get_state_provenance_root(base_state)?,
            None => None,
        };

        let Some(tree_hash) = self.build_merge_provenance_tree_recursive(
            Path::new(""),
            &final_tree,
            Some(&ours_tree),
            Some(&theirs_tree),
            base_tree.as_ref(),
            ours_root.as_ref(),
            theirs_root.as_ref(),
            base_root.as_ref(),
            ours,
            theirs,
            base,
            state,
        )?
        else {
            return Ok(None);
        };

        Ok(Some(tree_hash))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_merge_provenance_tree_recursive(
        &self,
        path: &Path,
        final_tree: &Tree,
        ours_tree: Option<&Tree>,
        theirs_tree: Option<&Tree>,
        base_tree: Option<&Tree>,
        ours_root: Option<&ContentHash>,
        theirs_root: Option<&ContentHash>,
        base_root: Option<&ContentHash>,
        ours: &State,
        theirs: &State,
        base: Option<&State>,
        final_state: &State,
    ) -> Result<Option<ContentHash>> {
        let mut entries = Vec::new();

        for entry in final_tree.entries() {
            let entry_path = path.join(&entry.name);
            match entry.entry_type {
                EntryType::Tree => {
                    let Some(subtree) = self.store.get_tree(&entry.hash)? else {
                        continue;
                    };
                    let ours_subtree = ours_tree
                        .and_then(|tree| tree.get(&entry.name))
                        .filter(|te| te.is_tree())
                        .and_then(|te| self.store.get_tree(&te.hash).ok().flatten());
                    let theirs_subtree = theirs_tree
                        .and_then(|tree| tree.get(&entry.name))
                        .filter(|te| te.is_tree())
                        .and_then(|te| self.store.get_tree(&te.hash).ok().flatten());
                    let base_subtree = base_tree
                        .and_then(|tree| tree.get(&entry.name))
                        .filter(|te| te.is_tree())
                        .and_then(|te| self.store.get_tree(&te.hash).ok().flatten());
                    if let Some(sub_hash) = self.build_merge_provenance_tree_recursive(
                        &entry_path,
                        &subtree,
                        ours_subtree.as_ref(),
                        theirs_subtree.as_ref(),
                        base_subtree.as_ref(),
                        ours_root,
                        theirs_root,
                        base_root,
                        ours,
                        theirs,
                        base,
                        final_state,
                    )? {
                        entries.push(TreeEntry::directory(entry.name.clone(), sub_hash)?);
                    }
                }
                EntryType::Blob => {
                    if let Some(hash) = self.build_merge_file_provenance(
                        &entry_path,
                        entry,
                        ours_tree,
                        theirs_tree,
                        base_tree,
                        ours_root,
                        theirs_root,
                        base_root,
                        ours,
                        theirs,
                        base,
                        final_state,
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

    #[allow(clippy::too_many_arguments)]
    fn build_merge_file_provenance(
        &self,
        path: &Path,
        final_entry: &TreeEntry,
        ours_tree: Option<&Tree>,
        theirs_tree: Option<&Tree>,
        base_tree: Option<&Tree>,
        ours_root: Option<&ContentHash>,
        theirs_root: Option<&ContentHash>,
        base_root: Option<&ContentHash>,
        ours: &State,
        theirs: &State,
        base: Option<&State>,
        final_state: &State,
    ) -> Result<Option<ContentHash>> {
        let Some(final_blob) = self.store.get_blob(&final_entry.hash)? else {
            return Ok(None);
        };
        let Some(final_lines) = split_text_lines(final_blob.content()) else {
            return Ok(None);
        };

        let ours_entry = ours_tree.and_then(|tree| lookup_tree_entry(self, tree, path));
        let theirs_entry = theirs_tree.and_then(|tree| lookup_tree_entry(self, tree, path));
        let base_entry = base_tree.and_then(|tree| lookup_tree_entry(self, tree, path));

        let ours_prov = match (ours_root, ours_entry.as_ref()) {
            (Some(root), Some(entry)) if entry.is_blob() => {
                self.get_file_provenance_from_root(root, path)?.or_else(|| {
                    synthesize_file_provenance_from_blob(
                        self.store.get_blob(&entry.hash).ok().flatten().as_ref(),
                        ours,
                    )
                })
            }
            (_, Some(entry)) if entry.is_blob() => synthesize_file_provenance_from_blob(
                self.store.get_blob(&entry.hash).ok().flatten().as_ref(),
                ours,
            ),
            _ => None,
        };
        let theirs_prov = match (theirs_root, theirs_entry.as_ref()) {
            (Some(root), Some(entry)) if entry.is_blob() => {
                self.get_file_provenance_from_root(root, path)?.or_else(|| {
                    synthesize_file_provenance_from_blob(
                        self.store.get_blob(&entry.hash).ok().flatten().as_ref(),
                        theirs,
                    )
                })
            }
            (_, Some(entry)) if entry.is_blob() => synthesize_file_provenance_from_blob(
                self.store.get_blob(&entry.hash).ok().flatten().as_ref(),
                theirs,
            ),
            _ => None,
        };
        let base_prov = match (base_root, base_entry.as_ref(), base) {
            (Some(root), Some(entry), Some(base_state)) if entry.is_blob() => {
                self.get_file_provenance_from_root(root, path)?.or_else(|| {
                    synthesize_file_provenance_from_blob(
                        self.store.get_blob(&entry.hash).ok().flatten().as_ref(),
                        base_state,
                    )
                })
            }
            (_, Some(entry), Some(base_state)) if entry.is_blob() => {
                synthesize_file_provenance_from_blob(
                    self.store.get_blob(&entry.hash).ok().flatten().as_ref(),
                    base_state,
                )
            }
            _ => None,
        };

        let ours_same = ours_entry
            .map(|entry| entry.hash == final_entry.hash)
            .unwrap_or(false);
        let theirs_same = theirs_entry
            .map(|entry| entry.hash == final_entry.hash)
            .unwrap_or(false);
        let base_same = base_entry
            .map(|entry| entry.hash == final_entry.hash)
            .unwrap_or(false);

        if ours_same
            && theirs_same
            && let (Some(ours_prov), Some(theirs_prov)) = (&ours_prov, &theirs_prov)
        {
            if ours_prov == theirs_prov {
                return Ok(Some(self.put_file_provenance(ours_prov)?));
            }
            let combined = self.combine_equal_file_provenance(
                final_entry.hash,
                &final_lines,
                &[ours_prov, theirs_prov],
            )?;
            return Ok(Some(self.put_file_provenance(&combined)?));
        }
        if ours_same && let Some(ours_prov) = &ours_prov {
            return Ok(Some(self.put_file_provenance(ours_prov)?));
        }
        if theirs_same && let Some(theirs_prov) = &theirs_prov {
            return Ok(Some(self.put_file_provenance(theirs_prov)?));
        }
        if base_same && let Some(base_prov) = &base_prov {
            return Ok(Some(self.put_file_provenance(base_prov)?));
        }

        let final_origin = self.state_origin(final_state);
        let merged = self.merge_file_provenance(
            final_entry.hash,
            &final_lines,
            [ours_prov.as_ref(), theirs_prov.as_ref(), base_prov.as_ref()],
            final_origin,
        )?;
        Ok(Some(self.put_file_provenance(&merged)?))
    }

    fn merge_file_provenance(
        &self,
        file_blob: ContentHash,
        final_lines: &[String],
        sources: [Option<&FileProvenance>; 3],
        final_origin: objects::object::Origin,
    ) -> Result<FileProvenance> {
        let mut builder = ProvenanceBuilder::default();
        let mut source_lines = Vec::new();
        let mut source_sets = Vec::new();

        for provenance in sources.into_iter().flatten() {
            source_lines.push(load_lines_for_hash(self, provenance.file_blob)?);
            source_sets.push(expand_line_origin_sets_with_builder(
                provenance,
                &mut builder,
            )?);
        }

        let final_origin_set = builder.origin_set_from_origins([final_origin]);
        let mut resolved_sets = vec![std::collections::BTreeSet::<u32>::new(); final_lines.len()];

        for (lines, origin_sets) in source_lines.iter().zip(source_sets.iter()) {
            let matches = lcs_line_matches(lines, final_lines);
            for (old_index, new_index) in matches {
                resolved_sets[new_index].insert(origin_sets[old_index]);
            }
        }

        let line_sets = resolved_sets
            .into_iter()
            .map(|set| {
                if set.is_empty() {
                    final_origin_set
                } else {
                    builder.origin_set_from_indexes(set.into_iter().collect())
                }
            })
            .collect();

        Ok(builder.into_file_provenance(file_blob, final_lines.len(), line_sets))
    }

    fn combine_equal_file_provenance(
        &self,
        file_blob: ContentHash,
        final_lines: &[String],
        sources: &[&FileProvenance],
    ) -> Result<FileProvenance> {
        let mut builder = ProvenanceBuilder::default();
        let mut per_source_sets = Vec::new();
        for provenance in sources {
            per_source_sets.push(expand_line_origin_sets_with_builder(
                provenance,
                &mut builder,
            )?);
        }

        let mut line_sets = Vec::with_capacity(final_lines.len());
        for line_index in 0..final_lines.len() {
            let mut set = std::collections::BTreeSet::new();
            for source in &per_source_sets {
                if let Some(index) = source.get(line_index) {
                    set.insert(*index);
                }
            }
            line_sets.push(builder.origin_set_from_indexes(set.into_iter().collect()));
        }

        Ok(builder.into_file_provenance(file_blob, final_lines.len(), line_sets))
    }
}