// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use super::{blob_renames::similarity_renames, staging::BuildError};
use crate::{
    object::{ContentHash, DiffKind, State, StateId, Tree, diff_trees},
    store::{HeddleError, ObjectStore, fs::FsStore},
};

pub(super) fn blob_lineage_order(
    store: &FsStore,
    states: &HashMap<StateId, State>,
    state_order: &[StateId],
    blob_hashes: &[ContentHash],
) -> Result<Vec<ContentHash>, BuildError> {
    let allowed = blob_hashes.iter().copied().collect::<HashSet<_>>();
    let mut walk = BlobWalk::default();
    for child_id in state_order {
        let child = &states[child_id];
        if child.parents.is_empty() {
            for (path, hash) in leaf_paths(store, child.tree)? {
                if allowed.contains(&hash) {
                    walk.record(&path, hash);
                }
            }
        }
        for parent_id in child.parents.iter().filter(|id| states.contains_key(id)) {
            walk.diff_edge(store, child, &states[parent_id], &allowed)?;
        }
    }
    let order = walk.finish(&allowed);
    if order.len() != allowed.len() {
        return Err(HeddleError::InvalidObject(format!(
            "blob lineage contains {} objects, snapshot contains {}",
            order.len(),
            allowed.len()
        ))
        .into());
    }
    Ok(order)
}

#[derive(Default)]
struct BlobWalk {
    aliases: HashMap<String, String>,
    histories: HashMap<String, Vec<ContentHash>>,
}

impl BlobWalk {
    fn diff_edge(
        &mut self,
        store: &FsStore,
        child: &State,
        parent: &State,
        allowed: &HashSet<ContentHash>,
    ) -> Result<(), BuildError> {
        let changes = diff_trees(store, &parent.tree, &child.tree)
            .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
        let mut added = Vec::new();
        let mut deleted = Vec::new();
        for change in changes.iter() {
            match change.kind {
                DiffKind::Modified => {
                    if let Some(hash) = leaf_hash(store, child.tree, &change.path)?
                        && allowed.contains(&hash)
                    {
                        self.record(&change.path, hash);
                    }
                    if let Some(hash) = leaf_hash(store, parent.tree, &change.path)?
                        && allowed.contains(&hash)
                    {
                        self.record(&change.path, hash);
                    }
                }
                DiffKind::Added => added.push(change.path.clone()),
                DiffKind::Deleted => deleted.push(change.path.clone()),
                DiffKind::Unchanged => {}
            }
        }
        self.record_add_delete(store, child.tree, parent.tree, &added, &deleted, allowed)
    }

    fn record_add_delete(
        &mut self,
        store: &FsStore,
        child_tree: ContentHash,
        parent_tree: ContentHash,
        added_paths: &[String],
        deleted_paths: &[String],
        allowed: &HashSet<ContentHash>,
    ) -> Result<(), BuildError> {
        let added = path_hashes(store, child_tree, added_paths, allowed)?;
        let deleted = path_hashes(store, parent_tree, deleted_paths, allowed)?;
        let mut used_added = HashSet::new();
        let mut used_deleted = HashSet::new();
        for old_path in deleted_paths {
            let Some(old_hash) = deleted.get(old_path) else {
                continue;
            };
            if let Some(new_path) = added_paths
                .iter()
                .find(|path| !used_added.contains(*path) && added.get(*path) == Some(old_hash))
            {
                self.record_rename(old_path, new_path, *old_hash, *old_hash);
                used_deleted.insert(old_path.clone());
                used_added.insert(new_path.clone());
            }
        }
        let deleted_remaining = remaining_text(store, &deleted, &used_deleted)?;
        let added_remaining = remaining_text(store, &added, &used_added)?;
        for (old_index, new_index) in similarity_renames(&deleted_remaining, &added_remaining) {
            let old_path = &deleted_remaining[old_index].0;
            let new_path = &added_remaining[new_index].0;
            self.record_rename(old_path, new_path, deleted[old_path], added[new_path]);
            used_deleted.insert(old_path.clone());
            used_added.insert(new_path.clone());
        }
        for (path, hash) in added {
            if !used_added.contains(&path) {
                self.record(&path, hash);
            }
        }
        for (path, hash) in deleted {
            if !used_deleted.contains(&path) {
                self.record(&path, hash);
            }
        }
        Ok(())
    }

    fn record_rename(
        &mut self,
        old_path: &str,
        new_path: &str,
        old_hash: ContentHash,
        new_hash: ContentHash,
    ) {
        self.rename(old_path, new_path);
        self.record(new_path, new_hash);
        self.record(old_path, old_hash);
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

    fn finish(&self, all_blobs: &HashSet<ContentHash>) -> Vec<ContentHash> {
        let mut paths = self.histories.keys().collect::<Vec<_>>();
        paths.sort_by_key(|path| (extension(path), path.as_str()));
        let mut seen = HashSet::new();
        let mut order = Vec::with_capacity(all_blobs.len());
        for path in paths {
            for hash in &self.histories[path] {
                if all_blobs.contains(hash) && seen.insert(*hash) {
                    order.push(*hash);
                }
            }
        }
        let mut leftovers = all_blobs.difference(&seen).copied().collect::<Vec<_>>();
        leftovers.sort();
        order.extend(leftovers);
        order
    }
}

fn path_hashes(
    store: &FsStore,
    root: ContentHash,
    paths: &[String],
    allowed: &HashSet<ContentHash>,
) -> Result<HashMap<String, ContentHash>, BuildError> {
    paths
        .iter()
        .filter_map(|path| match leaf_hash(store, root, path) {
            Ok(Some(hash)) if allowed.contains(&hash) => Some(Ok((path.clone(), hash))),
            Ok(Some(_)) => None,
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn remaining_text(
    store: &FsStore,
    paths: &HashMap<String, ContentHash>,
    used: &HashSet<String>,
) -> Result<Vec<(String, String)>, BuildError> {
    let mut remaining = paths
        .iter()
        .filter(|(path, _)| !used.contains(*path))
        .collect::<Vec<_>>();
    remaining.sort_by_key(|(path, _)| path.as_str());
    remaining
        .into_iter()
        .map(|(path, hash)| {
            load_blob(store, *hash)
                .map(|body| (path.clone(), String::from_utf8_lossy(&body).into_owned()))
        })
        .collect()
}

fn leaf_hash(
    store: &FsStore,
    root: ContentHash,
    path: &str,
) -> Result<Option<ContentHash>, BuildError> {
    let mut tree = load_tree(store, root)?;
    let mut components = Path::new(path).components().peekable();
    while let Some(component) = components.next() {
        let name = component.as_os_str().to_string_lossy();
        let Some(entry) = tree.get(&name) else {
            return Ok(None);
        };
        if components.peek().is_none() {
            return Ok(entry.leaf_content_hash());
        }
        let Some(hash) = entry.tree_hash() else {
            return Ok(None);
        };
        tree = load_tree(store, hash)?;
    }
    Ok(None)
}

fn leaf_paths(
    store: &FsStore,
    root: ContentHash,
) -> Result<Vec<(String, ContentHash)>, BuildError> {
    let mut output = Vec::new();
    let mut stack = vec![(String::new(), root)];
    while let Some((prefix, hash)) = stack.pop() {
        for entry in load_tree(store, hash)?.entries().iter().rev() {
            let path = if prefix.is_empty() {
                entry.name().to_string()
            } else {
                format!("{prefix}/{}", entry.name())
            };
            if let Some(hash) = entry.tree_hash() {
                stack.push((path, hash));
            } else if let Some(hash) = entry.leaf_content_hash() {
                output.push((path, hash));
            }
        }
    }
    Ok(output)
}

fn load_tree(store: &FsStore, hash: ContentHash) -> Result<Tree, BuildError> {
    ObjectStore::get_tree(store, &hash)?
        .ok_or_else(|| HeddleError::InvalidObject(format!("repack tree disappeared: {hash}")))
        .map_err(BuildError::from)
}

fn load_blob(store: &FsStore, hash: ContentHash) -> Result<Vec<u8>, BuildError> {
    ObjectStore::get_blob(store, &hash)?
        .map(|blob| blob.into_content())
        .ok_or_else(|| HeddleError::InvalidObject(format!("repack blob disappeared: {hash}")))
        .map_err(BuildError::from)
}

fn extension(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| Path::new(name).extension())
        .and_then(|value| value.to_str())
        .unwrap_or("")
}
