// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BinaryHeap, HashMap, HashSet},
    fs::File,
};

use heddle_object_model::compact::{
    decode_state_frame, decode_tree_frame, encode_state_frame, encode_tree_frame, encoded_tree_size,
};

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

pub(super) fn add_compact_metadata(
    store: &FsStore,
    builder: &mut StreamingPackBuilder<File>,
    state_ids: &[StateId],
    tree_hashes: &[ContentHash],
    blob_hashes: &[ContentHash],
    context: &RepackContext,
    corrupt_first: &mut bool,
) -> Result<u64, BuildError> {
    let states = load_states(store, state_ids)?;
    let state_order = newest_first_topology(&states)?;
    let tree_order = tree_path_order(store, tree_hashes, &states, &state_order)?;
    let blob_order = blob_lineage_order(store, &states, &state_order, blob_hashes)?;
    let blob_bytes = add_blob_frames(store, builder, &blob_order, context, corrupt_first)?;
    let state_bytes = add_state_frames(builder, &states, &state_order, context, corrupt_first)?;
    let tree_bytes = add_tree_frames(store, builder, &tree_order, context, corrupt_first)?;
    Ok(blob_bytes
        .saturating_add(state_bytes)
        .saturating_add(tree_bytes))
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

fn add_tree_frames(
    store: &FsStore,
    builder: &mut StreamingPackBuilder<File>,
    order: &[ContentHash],
    context: &RepackContext,
    corrupt_first: &mut bool,
) -> Result<u64, BuildError> {
    let mut trees = Vec::new();
    let mut ids = Vec::new();
    let mut tree_bytes = 0usize;
    let mut logical_bytes = 0u64;
    for hash in order {
        let tree = load_tree(store, *hash)?;
        let tree_size = encoded_tree_size(&tree);
        let proposed_size = 4usize
            .saturating_add(unsigned_varint_len(trees.len() + 1))
            .saturating_add(tree_bytes)
            .saturating_add(tree_size)
            .saturating_add(32);
        if !trees.is_empty() && proposed_size > FRAME_LIMIT {
            write_tree_frame(builder, &ids, &trees, corrupt_first)?;
            trees.clear();
            ids.clear();
            tree_bytes = 0;
        }
        let source = tree.encode_canonical().map_err(HeddleError::from)?;
        logical_bytes = logical_bytes.saturating_add(source.len() as u64);
        tree_bytes = tree_bytes.saturating_add(tree_size);
        trees.push(tree);
        ids.push(PackObjectId::Hash(*hash));
        context
            .checkpoint(tree_size as u64)
            .map_err(BuildError::Cancelled)?;
    }
    if !trees.is_empty() {
        write_tree_frame(builder, &ids, &trees, corrupt_first)?;
    }
    Ok(logical_bytes)
}

fn unsigned_varint_len(mut value: usize) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

fn write_tree_frame(
    builder: &mut StreamingPackBuilder<File>,
    ids: &[PackObjectId],
    trees: &[Tree],
    corrupt_first: &mut bool,
) -> Result<(), BuildError> {
    let mut frame = encode_tree_frame(trees).map_err(compact_error)?;
    verify_tree_frame(ids, trees, &frame)?;
    corrupt_if_requested(&mut frame, corrupt_first);
    let stored = compress_compact_frame(&frame)?;
    builder.add_shared_frame(ids, ObjectType::Tree, frame.len(), &stored)?;
    Ok(())
}

fn verify_tree_frame(
    ids: &[PackObjectId],
    expected: &[Tree],
    frame: &[u8],
) -> Result<(), BuildError> {
    let decoded = decode_tree_frame(frame).map_err(compact_error)?;
    if decoded != expected || decoded.len() != ids.len() {
        return Err(
            HeddleError::InvalidObject("compact tree frame changed object values".into()).into(),
        );
    }
    for (id, tree) in ids.iter().zip(decoded) {
        if *id != PackObjectId::Hash(tree.hash()) {
            return Err(
                HeddleError::InvalidObject("compact tree frame changed a typed id".into()).into(),
            );
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
