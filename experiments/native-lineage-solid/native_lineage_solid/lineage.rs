// SPDX-License-Identifier: Apache-2.0

use std::collections::{BinaryHeap, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use objects::{
    object::{ContentHash, State, StateId},
    store::{FsStore, ObjectStore},
};
use serde::Serialize;

use crate::{
    blob_lineage::blob_lineage_order,
    model::{ObjectRef, ObjectSet},
    tree_access::get_tree,
};

#[derive(Serialize)]
pub struct LineageStats {
    pub state_edges_walked: usize,
    pub root_states: usize,
    pub merge_states: usize,
    pub file_changes: usize,
    pub exact_renames: usize,
    pub similarity_renames: usize,
    pub lineage_paths: usize,
    pub lineage_blobs: usize,
    pub leftover_blobs: usize,
    pub missing_parents: usize,
}

pub struct RenameRecord {
    pub child: StateId,
    pub parent: StateId,
    pub from: String,
    pub to: String,
    pub exact: bool,
}

pub struct LineageOrder {
    pub order: Vec<ObjectRef>,
    pub renames: Vec<RenameRecord>,
    pub stats: LineageStats,
}

pub fn build_lineage_order(store: &FsStore, objects: &ObjectSet) -> Result<LineageOrder> {
    let states = load_states(store, &objects.states)?;
    let (state_order, missing_parents) = newest_first_topology(&states)?;
    let tree_order = trees_in_history_order(store, &states, &state_order)?;
    let (blob_order, renames, mut stats) =
        blob_lineage_order(store, &states, &state_order, objects)?;
    stats.missing_parents = missing_parents;
    let order = state_order
        .iter()
        .copied()
        .map(ObjectRef::State)
        .chain(tree_order.into_iter().map(ObjectRef::Tree))
        .chain(blob_order.into_iter().map(ObjectRef::Blob))
        .collect::<Vec<_>>();
    if order.len() != objects.counts().total {
        bail!(
            "lineage order has {} objects, store has {}",
            order.len(),
            objects.counts().total
        );
    }
    Ok(LineageOrder {
        order,
        renames,
        stats,
    })
}

fn load_states(store: &FsStore, ids: &[StateId]) -> Result<HashMap<StateId, State>> {
    ids.iter()
        .map(|id| {
            store
                .get_state(id)?
                .with_context(|| format!("missing state {}", id.to_string_full()))
                .map(|state| (*id, state))
        })
        .collect()
}

fn newest_first_topology(states: &HashMap<StateId, State>) -> Result<(Vec<StateId>, usize)> {
    let mut children = HashMap::<StateId, usize>::new();
    let mut missing = 0;
    for state in states.values() {
        children.entry(state.id()).or_default();
        for parent in &state.parents {
            if states.contains_key(parent) {
                *children.entry(*parent).or_default() += 1;
            } else {
                missing += 1;
            }
        }
    }
    let mut ready = BinaryHeap::new();
    for (id, state) in states {
        if children[id] == 0 {
            ready.push((state.created_at.timestamp_millis(), *id));
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
                ready.push((states[parent].created_at.timestamp_millis(), *parent));
            }
        }
    }
    if order.len() != states.len() {
        bail!("state graph is cyclic or incomplete");
    }
    Ok((order, missing))
}

fn trees_in_history_order(
    store: &FsStore,
    states: &HashMap<StateId, State>,
    state_order: &[StateId],
) -> Result<Vec<ContentHash>> {
    let mut seen = HashSet::new();
    let mut order = Vec::new();
    for id in state_order {
        let mut stack = vec![states[id].tree];
        while let Some(hash) = stack.pop() {
            if !seen.insert(hash) {
                continue;
            }
            order.push(hash);
            let tree = get_tree(store, hash)?;
            stack.extend(
                tree.entries()
                    .iter()
                    .rev()
                    .filter_map(|entry| entry.tree_hash()),
            );
        }
    }
    Ok(order)
}
