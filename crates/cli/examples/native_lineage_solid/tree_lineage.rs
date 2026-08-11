// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use objects::{
    object::{ContentHash, State, StateId},
    store::{FsStore, ObjectStore},
};
use serde::Serialize;

use crate::{model::ObjectSet, tree_access::get_tree};

#[derive(Serialize)]
pub struct TreeLineageStats {
    pub directory_path_groups: usize,
    pub ordered_trees: usize,
}

pub fn tree_path_order(
    store: &FsStore,
    objects: &ObjectSet,
    state_order: &[StateId],
) -> Result<(Vec<ContentHash>, TreeLineageStats)> {
    let states = load_states(store, &objects.states)?;
    let mut seen = HashSet::new();
    let mut group_indices = HashMap::<String, usize>::new();
    let mut groups = Vec::<Vec<ContentHash>>::new();
    for state_id in state_order {
        let state = states
            .get(state_id)
            .with_context(|| format!("missing ordered state {}", state_id.to_string_full()))?;
        visit_tree(
            store,
            state.tree,
            "",
            &mut seen,
            &mut group_indices,
            &mut groups,
        )?;
    }
    let order = groups.into_iter().flatten().collect::<Vec<_>>();
    if order.len() != objects.trees.len() {
        bail!(
            "directory-path order has {} trees, store has {}",
            order.len(),
            objects.trees.len()
        );
    }
    Ok((
        order,
        TreeLineageStats {
            directory_path_groups: group_indices.len(),
            ordered_trees: seen.len(),
        },
    ))
}

fn visit_tree(
    store: &FsStore,
    hash: ContentHash,
    path: &str,
    seen: &mut HashSet<ContentHash>,
    group_indices: &mut HashMap<String, usize>,
    groups: &mut Vec<Vec<ContentHash>>,
) -> Result<()> {
    if !seen.insert(hash) {
        return Ok(());
    }
    let index = *group_indices.entry(path.to_string()).or_insert_with(|| {
        groups.push(Vec::new());
        groups.len() - 1
    });
    groups[index].push(hash);
    let tree = get_tree(store, hash)?;
    for entry in tree.entries() {
        if let Some(child) = entry.tree_hash() {
            let child_path = if path.is_empty() {
                entry.name().to_string()
            } else {
                format!("{path}/{}", entry.name())
            };
            visit_tree(store, child, &child_path, seen, group_indices, groups)?;
        }
    }
    Ok(())
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
