// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use objects::{
    object::{ContentHash, DiffKind, State, StateId, diff_trees},
    store::{FsStore, ObjectStore},
};
use semantic::{SimilarityMethod, detect_file_renames};

use crate::{
    lineage::{LineageStats, RenameRecord},
    model::ObjectSet,
    tree_access::{leaf_hash, leaf_paths},
};

const RENAME_THRESHOLD: f64 = 0.6;

pub fn blob_lineage_order(
    store: &FsStore,
    states: &HashMap<StateId, State>,
    state_order: &[StateId],
    objects: &ObjectSet,
) -> Result<(Vec<ContentHash>, Vec<RenameRecord>, LineageStats)> {
    let mut walk = BlobWalk::default();
    for (index, child_id) in state_order.iter().enumerate() {
        let child = &states[child_id];
        if child.parents.is_empty() {
            walk.root_states += 1;
            for (path, hash) in leaf_paths(store, child.tree)? {
                walk.record(&path, hash);
            }
        }
        walk.merge_states += usize::from(child.parents.len() > 1);
        for parent_id in child.parents.iter().filter(|id| states.contains_key(id)) {
            walk.state_edges += 1;
            walk.diff_edge(store, *child_id, child, *parent_id, &states[parent_id])?;
        }
        if (index + 1) % 500 == 0 || index + 1 == state_order.len() {
            eprintln!("lineage walk: {}/{} states", index + 1, state_order.len());
        }
    }
    let (order, lineage_blobs, leftovers) = walk.finish(&objects.blobs);
    let stats = LineageStats {
        state_edges_walked: walk.state_edges,
        root_states: walk.root_states,
        merge_states: walk.merge_states,
        file_changes: walk.file_changes,
        exact_renames: walk.exact_renames,
        similarity_renames: walk.similarity_renames,
        lineage_paths: walk.histories.len(),
        lineage_blobs,
        leftover_blobs: leftovers,
        missing_parents: 0,
    };
    Ok((order, walk.renames, stats))
}

#[derive(Default)]
struct BlobWalk {
    aliases: HashMap<String, String>,
    histories: HashMap<String, Vec<ContentHash>>,
    renames: Vec<RenameRecord>,
    state_edges: usize,
    root_states: usize,
    merge_states: usize,
    file_changes: usize,
    exact_renames: usize,
    similarity_renames: usize,
}

impl BlobWalk {
    fn diff_edge(
        &mut self,
        store: &FsStore,
        child_id: StateId,
        child: &State,
        parent_id: StateId,
        parent: &State,
    ) -> Result<()> {
        let changes = diff_trees(store, &parent.tree, &child.tree)?;
        self.file_changes += changes.len();
        let mut added = Vec::new();
        let mut deleted = Vec::new();
        for change in &changes {
            match change.kind {
                DiffKind::Modified => {
                    if let Some(hash) = leaf_hash(store, child.tree, &change.path)? {
                        self.record(&change.path, hash);
                    }
                    if let Some(hash) = leaf_hash(store, parent.tree, &change.path)? {
                        self.record(&change.path, hash);
                    }
                }
                DiffKind::Added => added.push(change.path.clone()),
                DiffKind::Deleted => deleted.push(change.path.clone()),
                DiffKind::Unchanged => {}
            }
        }
        self.record_add_delete(
            store,
            (child_id, parent_id),
            child,
            parent,
            &added,
            &deleted,
        )
    }

    fn record_add_delete(
        &mut self,
        store: &FsStore,
        state_ids: (StateId, StateId),
        child: &State,
        parent: &State,
        added: &[String],
        deleted: &[String],
    ) -> Result<()> {
        let added_hashes = path_hashes(store, child.tree, added)?;
        let deleted_hashes = path_hashes(store, parent.tree, deleted)?;
        let mut renamed_added = HashSet::new();
        let mut renamed_deleted = HashSet::new();
        for (from, to, exact) in detect_renames(store, &added_hashes, &deleted_hashes)? {
            let Some(old_hash) = deleted_hashes.get(&from).copied() else {
                continue;
            };
            let Some(new_hash) = added_hashes.get(&to).copied() else {
                continue;
            };
            self.exact_renames += usize::from(exact);
            self.similarity_renames += usize::from(!exact);
            self.rename(&from, &to);
            self.record(&to, new_hash);
            self.record(&from, old_hash);
            renamed_added.insert(to.clone());
            renamed_deleted.insert(from.clone());
            self.renames.push(RenameRecord {
                child: state_ids.0,
                parent: state_ids.1,
                from,
                to,
                exact,
            });
        }
        for (path, hash) in added_hashes {
            if !renamed_added.contains(&path) {
                self.record(&path, hash);
            }
        }
        for (path, hash) in deleted_hashes {
            if !renamed_deleted.contains(&path) {
                self.record(&path, hash);
            }
        }
        Ok(())
    }

    fn rename(&mut self, old: &str, new: &str) {
        let old_key = self.canonical(old);
        let new_key = self.canonical(new);
        if old_key == new_key {
            return;
        }
        self.aliases.insert(old_key.clone(), new_key.clone());
        if let Some(old_history) = self.histories.remove(&old_key) {
            self.histories
                .entry(new_key)
                .or_default()
                .extend(old_history);
        }
    }

    fn record(&mut self, path: &str, hash: ContentHash) {
        let key = self.canonical(path);
        self.histories.entry(key).or_default().push(hash);
    }

    fn canonical(&self, path: &str) -> String {
        let mut current = path;
        let mut seen = HashSet::new();
        while let Some(next) = self.aliases.get(current) {
            if !seen.insert(current) {
                break;
            }
            current = next;
        }
        current.to_string()
    }

    fn finish(&self, all_blobs: &[ContentHash]) -> (Vec<ContentHash>, usize, usize) {
        let mut paths = self.histories.keys().collect::<Vec<_>>();
        paths.sort_by_key(|path| (extension(path), path.as_str()));
        let mut seen = HashSet::new();
        let mut order = Vec::with_capacity(all_blobs.len());
        for path in paths {
            for hash in &self.histories[path] {
                if seen.insert(*hash) {
                    order.push(*hash);
                }
            }
        }
        let lineage_blobs = order.len();
        for hash in all_blobs {
            if seen.insert(*hash) {
                order.push(*hash);
            }
        }
        let leftovers = order.len() - lineage_blobs;
        (order, lineage_blobs, leftovers)
    }
}

fn detect_renames(
    store: &FsStore,
    added: &HashMap<String, ContentHash>,
    deleted: &HashMap<String, ContentHash>,
) -> Result<Vec<(String, String, bool)>> {
    let mut added_paths = added.keys().cloned().collect::<Vec<_>>();
    let mut deleted_paths = deleted.keys().cloned().collect::<Vec<_>>();
    added_paths.sort();
    deleted_paths.sort();
    let mut used_added = HashSet::new();
    let mut used_deleted = HashSet::new();
    let mut output = Vec::new();
    for from in &deleted_paths {
        if let Some(to) = added_paths
            .iter()
            .find(|to| !used_added.contains(*to) && deleted[from] == added[*to])
        {
            used_deleted.insert(from.clone());
            used_added.insert(to.clone());
            output.push((from.clone(), to.clone(), true));
        }
    }
    let added_remaining = remaining_paths(added, &used_added);
    let deleted_remaining = remaining_paths(deleted, &used_deleted);
    let semantic = detect_file_renames(
        &path_text(store, &deleted_remaining)?,
        &path_text(store, &added_remaining)?,
        RENAME_THRESHOLD,
        SimilarityMethod::Lines,
    );
    output.extend(semantic.into_iter().map(|(from, to)| {
        (
            from.to_string_lossy().into_owned(),
            to.to_string_lossy().into_owned(),
            false,
        )
    }));
    Ok(output)
}

fn remaining_paths(
    paths: &HashMap<String, ContentHash>,
    used: &HashSet<String>,
) -> HashMap<String, ContentHash> {
    paths
        .iter()
        .filter(|(path, _)| !used.contains(*path))
        .map(|(path, hash)| (path.clone(), *hash))
        .collect()
}

fn path_hashes(
    store: &FsStore,
    root: ContentHash,
    paths: &[String],
) -> Result<HashMap<String, ContentHash>> {
    paths
        .iter()
        .filter_map(|path| match leaf_hash(store, root, path) {
            Ok(Some(hash)) => Some(Ok((path.clone(), hash))),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn path_text(
    store: &FsStore,
    paths: &HashMap<String, ContentHash>,
) -> Result<Vec<(PathBuf, String)>> {
    paths
        .iter()
        .map(|(path, hash)| {
            let bytes = store
                .get_blob(hash)?
                .with_context(|| format!("missing blob {hash}"))?;
            Ok((
                PathBuf::from(path),
                String::from_utf8_lossy(bytes.content()).into_owned(),
            ))
        })
        .collect()
}

fn extension(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| Path::new(name).extension())
        .and_then(|value| value.to_str())
        .unwrap_or("")
}
