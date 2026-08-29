// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BinaryHeap, HashMap, HashSet},
    fs::File,
};

use heddle_object_model::compact::{decode_state_frame, encode_state_frame};

use super::{
    super::FsStore, blob_lineage::blob_lineage_order, blob_writer::add_blob_frames,
    staging::BuildError,
};
use crate::{
    object::{ContentHash, State, StateId, Tree},
    store::{
        HeddleError, ObjectStore,
        pack::{
            ObjectType, PackObjectId, RepackContext, StreamingPackBuilder, compress_compact_frame,
        },
    },
};

#[path = "compact_state_writer.rs"]
mod state_writer;

use state_writer::add_state_frames;

pub(super) const FRAME_LIMIT: usize = 12 * 1024 * 1024;

pub(super) struct CompactMetadata {
    pub(super) logical_bytes: u64,
    pub(super) tree_order: Vec<ContentHash>,
    pub(super) tree_parents: HashMap<ContentHash, ContentHash>,
}

pub(super) fn add_compact_metadata(
    store: &FsStore,
    builder: &mut StreamingPackBuilder<File>,
    state_ids: &[StateId],
    tree_hashes: &[ContentHash],
    blob_hashes: &[ContentHash],
    context: &RepackContext,
    corrupt_first: &mut bool,
) -> Result<CompactMetadata, BuildError> {
    let states = load_states(store, state_ids)?;
    let state_order = newest_first_topology(&states)?;
    let tree_order = tree_path_order(store, tree_hashes, &states, &state_order)?;
    let tree_parents = historical_tree_parents(store, tree_hashes, &states, &state_order)?;
    let blob_order = blob_lineage_order(store, &states, &state_order, blob_hashes)?;
    let blob_bytes = add_blob_frames(store, builder, &blob_order, context, corrupt_first)?;
    let state_bytes = add_state_frames(builder, &states, &state_order, context, corrupt_first)?;
    Ok(CompactMetadata {
        logical_bytes: blob_bytes.saturating_add(state_bytes),
        tree_order,
        tree_parents,
    })
}

fn load_states(store: &FsStore, ids: &[StateId]) -> Result<HashMap<StateId, State>, BuildError> {
    ids.iter()
        .map(|id| {
            ObjectStore::get_state(store, id)?
                .ok_or_else(|| {
                    HeddleError::InvalidObject(format!(
                        "repack state disappeared: {}",
                        id.to_string_full()
                    ))
                })
                .map(|state| (*id, state))
        })
        .collect::<crate::store::Result<_>>()
        .map_err(BuildError::from)
}

fn newest_first_topology(states: &HashMap<StateId, State>) -> Result<Vec<StateId>, BuildError> {
    let mut children = HashMap::<StateId, usize>::new();
    for state in states.values() {
        children.entry(state.state_id).or_default();
        for parent in &state.parents {
            if states.contains_key(parent) {
                *children.entry(*parent).or_default() += 1;
            }
        }
    }
    let mut ready = BinaryHeap::new();
    for (id, state) in states {
        if children[id] == 0 {
            ready.push((state.created_at, *id));
        }
    }
    let mut order = Vec::with_capacity(states.len());
    while let Some((_, id)) = ready.pop() {
        order.push(id);
        for parent in &states[&id].parents {
            let Some(count) = children.get_mut(parent) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                ready.push((states[parent].created_at, *parent));
            }
        }
    }
    if order.len() != states.len() {
        return Err(HeddleError::InvalidObject(
            "repack state graph is cyclic or incomplete".to_string(),
        )
        .into());
    }
    Ok(order)
}

fn tree_path_order(
    store: &FsStore,
    hashes: &[ContentHash],
    states: &HashMap<StateId, State>,
    state_order: &[StateId],
) -> Result<Vec<ContentHash>, BuildError> {
    let allowed = hashes.iter().copied().collect::<HashSet<_>>();
    let mut seen = HashSet::with_capacity(allowed.len());
    let mut group_indices = HashMap::<String, usize>::new();
    let mut groups = Vec::<Vec<ContentHash>>::new();
    for state_id in state_order {
        visit_tree(
            store,
            states[state_id].tree,
            &allowed,
            &mut seen,
            &mut group_indices,
            &mut groups,
        )?;
    }
    let mut order = groups.into_iter().flatten().collect::<Vec<_>>();
    let mut leftovers = allowed.difference(&seen).copied().collect::<Vec<_>>();
    leftovers.sort();
    order.extend(leftovers);
    Ok(order)
}

fn historical_tree_parents(
    store: &FsStore,
    hashes: &[ContentHash],
    states: &HashMap<StateId, State>,
    state_order: &[StateId],
) -> Result<HashMap<ContentHash, ContentHash>, BuildError> {
    let allowed = hashes.iter().copied().collect::<HashSet<_>>();
    let mut parents = HashMap::new();
    let mut seen_pairs = HashSet::new();
    for state_id in state_order {
        let state = &states[state_id];
        for parent_id in &state.parents {
            let Some(parent) = states.get(parent_id) else {
                continue;
            };
            visit_tree_pairs(
                store,
                state.tree,
                parent.tree,
                &allowed,
                &mut seen_pairs,
                &mut parents,
            )?;
        }
    }
    Ok(parents)
}

fn visit_tree_pairs(
    store: &FsStore,
    current: ContentHash,
    parent: ContentHash,
    allowed: &HashSet<ContentHash>,
    seen_pairs: &mut HashSet<(ContentHash, ContentHash)>,
    parents: &mut HashMap<ContentHash, ContentHash>,
) -> Result<(), BuildError> {
    let mut stack = vec![(current, parent)];
    while let Some((current, parent)) = stack.pop() {
        if current == parent || !seen_pairs.insert((current, parent)) {
            continue;
        }
        if !allowed.contains(&current) || !allowed.contains(&parent) {
            continue;
        }
        parents.entry(current).or_insert(parent);
        let current_tree = load_tree(store, current)?;
        let parent_tree = load_tree(store, parent)?;
        for entry in current_tree.entries().iter().rev() {
            let Some(current_child) = entry.tree_hash() else {
                continue;
            };
            let Some(parent_child) = parent_tree
                .get(entry.name())
                .and_then(|entry| entry.tree_hash())
            else {
                continue;
            };
            stack.push((current_child, parent_child));
        }
    }
    Ok(())
}

fn visit_tree(
    store: &FsStore,
    root: ContentHash,
    allowed: &HashSet<ContentHash>,
    seen: &mut HashSet<ContentHash>,
    group_indices: &mut HashMap<String, usize>,
    groups: &mut Vec<Vec<ContentHash>>,
) -> Result<(), BuildError> {
    let mut stack = vec![(root, String::new())];
    while let Some((hash, path)) = stack.pop() {
        if !allowed.contains(&hash) {
            return Err(HeddleError::InvalidObject(format!(
                "state references tree outside repack snapshot: {hash}"
            ))
            .into());
        }
        if !seen.insert(hash) {
            continue;
        }
        let index = *group_indices.entry(path.clone()).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[index].push(hash);
        let tree = load_tree(store, hash)?;
        for entry in tree.entries().iter().rev() {
            if let Some(child) = entry.tree_hash() {
                let child_path = if path.is_empty() {
                    entry.name().to_string()
                } else {
                    format!("{path}/{}", entry.name())
                };
                stack.push((child, child_path));
            }
        }
    }
    Ok(())
}

fn load_tree(store: &FsStore, hash: ContentHash) -> Result<Tree, BuildError> {
    ObjectStore::get_tree(store, &hash)?
        .ok_or_else(|| HeddleError::InvalidObject(format!("repack tree disappeared: {hash}")))
        .map_err(BuildError::from)
}

fn compact_error(error: heddle_object_model::compact::CompactError) -> BuildError {
    HeddleError::InvalidObject(error.to_string()).into()
}

fn corrupt_if_requested(frame: &mut [u8], corrupt_first: &mut bool) {
    if *corrupt_first {
        let index = frame.len() / 2;
        frame[index] ^= 0x01;
        *corrupt_first = false;
    }
}
