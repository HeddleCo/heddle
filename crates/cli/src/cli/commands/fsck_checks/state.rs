// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use anyhow::Result;
use crypto::StateSigningExt;
use objects::{
    object::{ContentHash, State, Tree},
    store::BlockingObjectStore,
};
use repo::Repository;

use super::{FsckError, make_error};

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
                    parent_map.insert(state.change_id, state.parents.clone());
                }
                check_state_integrity(repo, &state, errors, thorough)?;
            }
            None => {
                errors.push(make_error(
                    "missing_state",
                    &format!("State {} is listed but cannot be read", state_id),
                    Some(state_id.short()),
                ));
            }
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

    if thorough && state.signature.is_some() {
        match state.verify_signature() {
            Ok(()) => {}
            Err(error) => errors.push(make_error(
                "invalid_signature",
                &format!(
                    "State {} signature could not be verified: {}",
                    state.change_id.short(),
                    error
                ),
                Some(state.change_id.short()),
            )),
        }
    }

    if thorough && let Some(provenance_root) = state.provenance {
        if !repo.store().has_tree(&provenance_root)? {
            errors.push(make_error(
                "missing_provenance",
                &format!(
                    "State {} references missing provenance tree {}",
                    state.change_id.short(),
                    provenance_root.short()
                ),
                Some(provenance_root.short()),
            ));
        } else if let Some(tree) = repo.store().get_tree(&state.tree)? {
            check_provenance_tree(repo, &tree, &provenance_root, Path::new(""), errors)?;
        }
    }

    Ok(())
}

fn check_provenance_tree(
    repo: &Repository,
    data_tree: &Tree,
    provenance_root: &ContentHash,
    path: &Path,
    errors: &mut Vec<FsckError>,
) -> Result<()> {
    let Some(provenance_tree) = repo.store().get_tree(provenance_root)? else {
        return Ok(());
    };

    for entry in provenance_tree.entries() {
        let entry_path = path.join(&entry.name);
        let Some(data_entry) = data_tree.get(&entry.name) else {
            errors.push(make_error(
                "invalid_provenance",
                &format!(
                    "Provenance path '{}' does not exist in the data tree",
                    entry_path.display()
                ),
                None,
            ));
            continue;
        };

        match entry.entry_type {
            objects::object::EntryType::Tree => {
                if !data_entry.is_tree() {
                    errors.push(make_error(
                        "invalid_provenance",
                        &format!(
                            "Provenance path '{}' points to a directory but data tree has a file",
                            entry_path.display()
                        ),
                        None,
                    ));
                    continue;
                }
                if let Some(subtree) = repo.store().get_tree(&data_entry.hash)? {
                    check_provenance_tree(repo, &subtree, &entry.hash, &entry_path, errors)?;
                }
            }
            objects::object::EntryType::Blob => {
                if !data_entry.is_blob() {
                    errors.push(make_error(
                        "invalid_provenance",
                        &format!(
                            "Provenance path '{}' points to a file but data tree has a directory",
                            entry_path.display()
                        ),
                        None,
                    ));
                    continue;
                }
                let Some(provenance_blob) = repo.store().get_blob(&entry.hash)? else {
                    errors.push(make_error(
                        "invalid_provenance",
                        &format!("Missing provenance blob for '{}'", entry_path.display()),
                        Some(entry.hash.short()),
                    ));
                    continue;
                };
                let provenance: objects::object::FileProvenance =
                    match rmp_serde::from_slice(provenance_blob.content()) {
                        Ok(provenance) => provenance,
                        Err(error) => {
                            errors.push(make_error(
                                "invalid_provenance",
                                &format!(
                                    "Invalid provenance blob for '{}': {}",
                                    entry_path.display(),
                                    error
                                ),
                                Some(entry.hash.short()),
                            ));
                            continue;
                        }
                    };
                if let Err(error) = provenance.validate() {
                    errors.push(make_error(
                        "invalid_provenance",
                        &format!(
                            "Invalid provenance spans for '{}': {}",
                            entry_path.display(),
                            error
                        ),
                        Some(entry.hash.short()),
                    ));
                    continue;
                }
                if provenance.file_blob != data_entry.hash {
                    errors.push(make_error(
                        "invalid_provenance",
                        &format!(
                            "Provenance for '{}' points to blob {} but file uses {}",
                            entry_path.display(),
                            provenance.file_blob.short(),
                            data_entry.hash.short()
                        ),
                        Some(entry.hash.short()),
                    ));
                    continue;
                }
                if let Some(blob) = repo.store().get_blob(&data_entry.hash)?
                    && let Ok(text) = std::str::from_utf8(blob.content())
                {
                    let line_count = text.lines().count() as u32;
                    if provenance.line_count != line_count {
                        errors.push(make_error(
                            "invalid_provenance",
                            &format!(
                                "Provenance for '{}' records {} lines but file has {}",
                                entry_path.display(),
                                provenance.line_count,
                                line_count
                            ),
                            Some(entry.hash.short()),
                        ));
                    }
                }
            }
            objects::object::EntryType::Symlink => {}
        }
    }

    Ok(())
}

fn check_state_cycles(
    parent_map: &HashMap<objects::object::ChangeId, Vec<objects::object::ChangeId>>,
    errors: &mut Vec<FsckError>,
) {
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    let mut reported = HashSet::new();

    for state_id in parent_map.keys().copied() {
        detect_cycle(
            state_id,
            parent_map,
            &mut visiting,
            &mut visited,
            &mut reported,
            errors,
        );
    }
}

fn detect_cycle(
    state_id: objects::object::ChangeId,
    parent_map: &HashMap<objects::object::ChangeId, Vec<objects::object::ChangeId>>,
    visiting: &mut HashSet<objects::object::ChangeId>,
    visited: &mut HashSet<objects::object::ChangeId>,
    reported: &mut HashSet<objects::object::ChangeId>,
    errors: &mut Vec<FsckError>,
) {
    if visited.contains(&state_id) {
        return;
    }

    if !visiting.insert(state_id) {
        if reported.insert(state_id) {
            errors.push(make_error(
                "state_cycle",
                &format!(
                    "State parent graph contains a cycle involving {}",
                    state_id.short()
                ),
                Some(state_id.short()),
            ));
        }
        return;
    }

    if let Some(parents) = parent_map.get(&state_id) {
        for parent in parents {
            if parent_map.contains_key(parent) {
                detect_cycle(*parent, parent_map, visiting, visited, reported, errors);
            }
        }
    }

    visiting.remove(&state_id);
    visited.insert(state_id);
}
