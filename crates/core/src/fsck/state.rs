// SPDX-License-Identifier: Apache-2.0
use std::collections::{HashMap, HashSet};

use objects::{
    error::Result,
    object::{SignatureStatus, State},
    store::ObjectStore,
};
use repo::Repository;

use super::{FsckError, make_error, provenance::check_provenance_tree};

pub(crate) fn check_states(
    repo: &Repository,
    errors: &mut Vec<FsckError>,
    objects_checked: &mut usize,
    thorough: bool,
) -> Result<()> {
    let states = repo.store().list_states()?;
    let mut parent_map = HashMap::with_capacity(states.len());

    for state_id in states {
        match repo.store().get_state(&state_id)? {
            Some(state) => {
                *objects_checked += 1;
                if thorough {
                    parent_map.insert(state.state_id, state.parents.clone());
                }
                check_state_integrity(repo, &state, errors, thorough)?;
            }
            None => errors.push(make_error(
                "missing_state",
                &format!("State {} is listed but cannot be read", state_id),
                Some(state_id.short()),
            )),
        }
    }

    if thorough {
        check_state_cycles(&parent_map, errors);
    }
    Ok(())
}

fn check_state_integrity(
    repo: &Repository,
    state: &State,
    errors: &mut Vec<FsckError>,
    thorough: bool,
) -> Result<()> {
    if !repo.store().has_tree(&state.tree)? {
        errors.push(make_error(
            "missing_tree",
            &format!("State references missing tree {}", state.tree.short()),
            Some(state.tree.short()),
        ));
    }
    for parent in &state.parents {
        if !repo.store().has_state(parent)? {
            errors.push(make_error(
                "missing_parent",
                &format!("State references missing parent {}", parent.short()),
                Some(parent.short()),
            ));
        }
    }
    if thorough && repo.verify_state_signature(&state.state_id)? == SignatureStatus::Invalid {
        errors.push(make_error(
            "invalid_signature",
            &format!(
                "State {} signature could not be verified",
                state.state_id.short()
            ),
            Some(state.state_id.short()),
        ));
    }
    if thorough && let Some(provenance_root) = state.provenance {
        if !repo.store().has_tree(&provenance_root)? {
            errors.push(make_error(
                "missing_provenance",
                &format!(
                    "State {} references missing provenance tree {}",
                    state.state_id.short(),
                    provenance_root.short()
                ),
                Some(provenance_root.short()),
            ));
        } else if let Some(tree) = repo.store().get_tree(&state.tree)? {
            check_provenance_tree(repo, &tree, provenance_root, errors)?;
        }
    }
    Ok(())
}

fn check_state_cycles(
    parent_map: &HashMap<objects::object::StateId, Vec<objects::object::StateId>>,
    errors: &mut Vec<FsckError>,
) {
    #[derive(Clone, Copy, Eq, PartialEq)]
    enum VisitState {
        Visiting,
        Visited,
    }

    let mut states = HashMap::with_capacity(parent_map.len());
    let mut reported = HashSet::new();
    for start in parent_map.keys().copied() {
        if states.contains_key(&start) {
            continue;
        }
        states.insert(start, VisitState::Visiting);
        let mut stack = vec![(start, 0usize)];

        while let Some((state_id, next_parent)) = stack.last_mut() {
            let parents = parent_map.get(state_id).map(Vec::as_slice).unwrap_or(&[]);
            let next = parents.get(*next_parent).copied();
            *next_parent += usize::from(next.is_some());

            let Some(parent) = next else {
                let completed = *state_id;
                stack.pop();
                states.insert(completed, VisitState::Visited);
                continue;
            };
            if !parent_map.contains_key(&parent) {
                continue;
            }
            match states.get(&parent).copied() {
                Some(VisitState::Visited) => {}
                Some(VisitState::Visiting) => {
                    if reported.insert(parent) {
                        errors.push(make_error(
                            "state_cycle",
                            &format!(
                                "State parent graph contains a cycle involving {}",
                                parent.short()
                            ),
                            Some(parent.short()),
                        ));
                    }
                }
                None => {
                    states.insert(parent, VisitState::Visiting);
                    stack.push((parent, 0));
                }
            }
        }
    }
}

#[cfg(test)]
mod cycle_tests {
    use super::*;

    fn state_id(index: usize) -> objects::object::StateId {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&(index as u64).to_le_bytes());
        objects::object::StateId::from_bytes(bytes)
    }

    #[test]
    fn deep_parent_chain_is_checked_without_recursion() {
        let mut parents = HashMap::new();
        for index in 0..50_000 {
            parents.insert(
                state_id(index),
                (index > 0)
                    .then(|| state_id(index - 1))
                    .into_iter()
                    .collect(),
            );
        }
        let mut errors = Vec::new();
        check_state_cycles(&parents, &mut errors);
        assert!(errors.is_empty());
    }

    #[test]
    fn cycle_is_reported_once() {
        let first = state_id(1);
        let second = state_id(2);
        let parents = HashMap::from([(first, vec![second]), (second, vec![first])]);
        let mut errors = Vec::new();
        check_state_cycles(&parents, &mut errors);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, "state_cycle");
    }
}
