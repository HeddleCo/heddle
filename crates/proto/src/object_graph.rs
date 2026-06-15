// SPDX-License-Identifier: Apache-2.0
use std::collections::{HashSet, VecDeque};

use objects::{
    object::{ChangeId, ContentHash, EntryType},
    store::ObjectStore,
};
use serde::{Deserialize, Serialize};

use crate::{ProtocolError, Result};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectId {
    Hash(ContentHash),
    ChangeId(ChangeId),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub id: ObjectId,
    pub obj_type: ObjectType,
    pub size: u64,
    pub delta_base: Option<ContentHash>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlannedObject {
    pub id: ObjectId,
    pub obj_type: ObjectType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectType {
    Blob,
    Tree,
    State,
    Action,
    /// A `RedactionsBlob` sidecar — the rmp-encoded record(s) declaring
    /// that a specific blob has been redacted (and possibly purged) by
    /// an authorized operator. Keyed on the wire by `ObjectId::Hash` of
    /// the *redacted blob*, since `Repository`'s sidecar store is
    /// indexed that way.
    Redaction,
    /// A `StateVisibilityBlob` sidecar — the rmp-encoded record(s)
    /// declaring a non-public audience tier for a specific state. Keyed
    /// on the wire by `ObjectId::ChangeId` of the *state*, since the
    /// per-state sidecar store is indexed that way. Like `Redaction`, it
    /// is a sidecar record that lives outside the content-addressed pack
    /// and ships via the per-object transfer path, not the pack.
    StateVisibility,
}

#[derive(Debug, Clone, Default)]
pub struct StateClosureOptions {
    pub depth: Option<u32>,
    pub exclude_states: Vec<ChangeId>,
}

pub fn enumerate_state_closure(
    store: &impl ObjectStore,
    state_id: ChangeId,
) -> Result<Vec<ObjectInfo>> {
    enumerate_state_closure_with_options(store, state_id, StateClosureOptions::default())
}

pub fn enumerate_state_closure_with_options(
    store: &impl ObjectStore,
    state_id: ChangeId,
    options: StateClosureOptions,
) -> Result<Vec<ObjectInfo>> {
    let (excluded_states, excluded_hashes) = collect_excluded(store, &options.exclude_states)?;

    let mut out = Vec::new();
    let mut seen_states: HashSet<ChangeId> = HashSet::new();
    let mut seen_hashes: HashSet<ContentHash> = HashSet::new();
    let mut queue: VecDeque<(ChangeId, u32)> = VecDeque::new();
    queue.push_back((state_id, 0));

    while let Some((id, depth)) = queue.pop_front() {
        if excluded_states.contains(&id) {
            continue;
        }
        if !seen_states.insert(id) {
            continue;
        }

        let state = store
            .get_state(&id)?
            .ok_or_else(|| ProtocolError::ObjectNotFound(id.to_string()))?;

        let state_bytes = rmp_serde::to_vec_named(&state)?;
        out.push(ObjectInfo {
            id: ObjectId::ChangeId(id),
            obj_type: ObjectType::State,
            size: state_bytes.len() as u64,
            delta_base: None,
        });
        emit_state_visibility_info(store, &id, &mut out)?;

        if options.depth.map(|max| depth < max).unwrap_or(true) {
            for parent in &state.parents {
                queue.push_back((*parent, depth + 1));
            }
        }

        enumerate_tree_closure_filtered(
            store,
            state.tree,
            &excluded_hashes,
            &mut seen_hashes,
            &mut out,
        )?;
        if let Some(provenance_root) = state.provenance {
            enumerate_tree_closure_filtered(
                store,
                provenance_root,
                &excluded_hashes,
                &mut seen_hashes,
                &mut out,
            )?;
        }
        if let Some(context_root) = state.context {
            enumerate_tree_closure_filtered(
                store,
                context_root,
                &excluded_hashes,
                &mut seen_hashes,
                &mut out,
            )?;
        }
    }

    Ok(out)
}

pub fn enumerate_state_closure_plan(
    store: &impl ObjectStore,
    state_id: ChangeId,
) -> Result<Vec<PlannedObject>> {
    enumerate_state_closure_plan_with_options(store, state_id, StateClosureOptions::default())
}

pub fn enumerate_state_closure_plan_with_options(
    store: &impl ObjectStore,
    state_id: ChangeId,
    options: StateClosureOptions,
) -> Result<Vec<PlannedObject>> {
    let (excluded_states, excluded_hashes) = collect_excluded(store, &options.exclude_states)?;

    let mut out = Vec::new();
    let mut seen_states: HashSet<ChangeId> = HashSet::new();
    let mut seen_hashes: HashSet<ContentHash> = HashSet::new();
    let mut queue: VecDeque<(ChangeId, u32)> = VecDeque::new();
    queue.push_back((state_id, 0));

    while let Some((id, depth)) = queue.pop_front() {
        if excluded_states.contains(&id) {
            continue;
        }
        if !seen_states.insert(id) {
            continue;
        }

        let state = store
            .get_state(&id)?
            .ok_or_else(|| ProtocolError::ObjectNotFound(id.to_string()))?;

        out.push(PlannedObject {
            id: ObjectId::ChangeId(id),
            obj_type: ObjectType::State,
        });
        emit_state_visibility_plan(store, &id, &mut out)?;

        if options.depth.map(|max| depth < max).unwrap_or(true) {
            for parent in &state.parents {
                queue.push_back((*parent, depth + 1));
            }
        }

        enumerate_tree_plan_filtered(
            store,
            state.tree,
            &excluded_hashes,
            &mut seen_hashes,
            &mut out,
        )?;
        if let Some(provenance_root) = state.provenance {
            enumerate_tree_plan_filtered(
                store,
                provenance_root,
                &excluded_hashes,
                &mut seen_hashes,
                &mut out,
            )?;
        }
        if let Some(context_root) = state.context {
            enumerate_tree_plan_filtered(
                store,
                context_root,
                &excluded_hashes,
                &mut seen_hashes,
                &mut out,
            )?;
        }
    }

    Ok(out)
}

fn enumerate_tree_closure_filtered(
    store: &impl ObjectStore,
    tree_hash: ContentHash,
    excluded: &HashSet<ContentHash>,
    seen: &mut HashSet<ContentHash>,
    out: &mut Vec<ObjectInfo>,
) -> Result<()> {
    if excluded.contains(&tree_hash) {
        return Ok(());
    }
    if !seen.insert(tree_hash) {
        return Ok(());
    }

    let tree = store
        .get_tree(&tree_hash)?
        .ok_or_else(|| ProtocolError::ObjectNotFound(tree_hash.to_hex()))?;

    let tree_bytes = rmp_serde::to_vec_named(&tree)?;
    out.push(ObjectInfo {
        id: ObjectId::Hash(tree_hash),
        obj_type: ObjectType::Tree,
        size: tree_bytes.len() as u64,
        delta_base: None,
    });

    for entry in tree.entries() {
        match entry.entry_type {
            EntryType::Blob => {
                if excluded.contains(&entry.hash) {
                    continue;
                }
                if !seen.insert(entry.hash) {
                    continue;
                }
                let blob = store
                    .get_blob(&entry.hash)?
                    .ok_or_else(|| ProtocolError::ObjectNotFound(entry.hash.to_hex()))?;
                out.push(ObjectInfo {
                    id: ObjectId::Hash(entry.hash),
                    obj_type: ObjectType::Blob,
                    size: blob.size() as u64,
                    delta_base: None,
                });
                emit_redaction_info(store, &entry.hash, out)?;
            }
            EntryType::Tree => {
                enumerate_tree_closure_filtered(store, entry.hash, excluded, seen, out)?;
            }
            EntryType::Symlink => {
                if excluded.contains(&entry.hash) {
                    continue;
                }
                if !seen.insert(entry.hash) {
                    continue;
                }
                let blob = store
                    .get_blob(&entry.hash)?
                    .ok_or_else(|| ProtocolError::ObjectNotFound(entry.hash.to_hex()))?;
                out.push(ObjectInfo {
                    id: ObjectId::Hash(entry.hash),
                    obj_type: ObjectType::Blob,
                    size: blob.size() as u64,
                    delta_base: None,
                });
                emit_redaction_info(store, &entry.hash, out)?;
            }
        }
    }

    Ok(())
}

/// If `state` carries a state-visibility sidecar, push a StateVisibility
/// `ObjectInfo` keyed by the state id. No-op when the state is public by
/// absence.
fn emit_state_visibility_info(
    store: &impl ObjectStore,
    state: &ChangeId,
    out: &mut Vec<ObjectInfo>,
) -> Result<()> {
    if let Some(bytes) = store.get_state_visibility_bytes_for_state(state)? {
        out.push(ObjectInfo {
            id: ObjectId::ChangeId(*state),
            obj_type: ObjectType::StateVisibility,
            size: bytes.len() as u64,
            delta_base: None,
        });
    }
    Ok(())
}

fn emit_state_visibility_plan(
    store: &impl ObjectStore,
    state: &ChangeId,
    out: &mut Vec<PlannedObject>,
) -> Result<()> {
    if store.has_state_visibility_for_state(state)? {
        out.push(PlannedObject {
            id: ObjectId::ChangeId(*state),
            obj_type: ObjectType::StateVisibility,
        });
    }
    Ok(())
}

/// If `blob` carries a redaction sidecar, push a Redaction `ObjectInfo`
/// keyed by the blob hash. No-op when the blob has no redactions.
///
/// Redactions are not deduped via the `seen: HashSet<ContentHash>` used
/// for blob/tree dedup because the `ObjectId` for a redaction is the
/// *redacted blob's* hash — and that hash is already inserted into
/// `seen` by the blob's own emission. A blob can only appear once in
/// the closure (dedup'd by hash), so its redaction can only be emitted
/// once too.
fn emit_redaction_info(
    store: &impl ObjectStore,
    blob: &ContentHash,
    out: &mut Vec<ObjectInfo>,
) -> Result<()> {
    if let Some(bytes) = store.get_redactions_bytes_for_blob(blob)? {
        out.push(ObjectInfo {
            id: ObjectId::Hash(*blob),
            obj_type: ObjectType::Redaction,
            size: bytes.len() as u64,
            delta_base: None,
        });
    }
    Ok(())
}

fn enumerate_tree_plan_filtered(
    store: &impl ObjectStore,
    tree_hash: ContentHash,
    excluded: &HashSet<ContentHash>,
    seen: &mut HashSet<ContentHash>,
    out: &mut Vec<PlannedObject>,
) -> Result<()> {
    if excluded.contains(&tree_hash) {
        return Ok(());
    }
    if !seen.insert(tree_hash) {
        return Ok(());
    }

    let tree = store
        .get_tree(&tree_hash)?
        .ok_or_else(|| ProtocolError::ObjectNotFound(tree_hash.to_hex()))?;

    out.push(PlannedObject {
        id: ObjectId::Hash(tree_hash),
        obj_type: ObjectType::Tree,
    });

    for entry in tree.entries() {
        match entry.entry_type {
            EntryType::Blob | EntryType::Symlink => {
                if excluded.contains(&entry.hash) {
                    continue;
                }
                if !seen.insert(entry.hash) {
                    continue;
                }
                out.push(PlannedObject {
                    id: ObjectId::Hash(entry.hash),
                    obj_type: ObjectType::Blob,
                });
                emit_redaction_plan(store, &entry.hash, out)?;
            }
            EntryType::Tree => {
                enumerate_tree_plan_filtered(store, entry.hash, excluded, seen, out)?;
            }
        }
    }

    Ok(())
}

fn emit_redaction_plan(
    store: &impl ObjectStore,
    blob: &ContentHash,
    out: &mut Vec<PlannedObject>,
) -> Result<()> {
    if store.has_redactions_for_blob(blob)? {
        out.push(PlannedObject {
            id: ObjectId::Hash(*blob),
            obj_type: ObjectType::Redaction,
        });
    }
    Ok(())
}

fn collect_excluded(
    store: &impl ObjectStore,
    roots: &[ChangeId],
) -> Result<(HashSet<ChangeId>, HashSet<ContentHash>)> {
    if roots.is_empty() {
        return Ok((HashSet::new(), HashSet::new()));
    }

    let mut excluded_states: HashSet<ChangeId> = HashSet::new();
    let mut excluded_hashes: HashSet<ContentHash> = HashSet::new();
    let mut queue: VecDeque<ChangeId> = VecDeque::new();

    for id in roots {
        queue.push_back(*id);
    }

    while let Some(id) = queue.pop_front() {
        if !excluded_states.insert(id) {
            continue;
        }

        let state = match store.get_state(&id)? {
            Some(state) => state,
            None => continue,
        };

        for parent in &state.parents {
            queue.push_back(*parent);
        }

        collect_tree_hashes(store, state.tree, &mut excluded_hashes)?;
        if let Some(provenance_root) = state.provenance {
            collect_tree_hashes(store, provenance_root, &mut excluded_hashes)?;
        }
        if let Some(context_root) = state.context {
            collect_tree_hashes(store, context_root, &mut excluded_hashes)?;
        }
    }

    Ok((excluded_states, excluded_hashes))
}

fn collect_tree_hashes(
    store: &impl ObjectStore,
    tree_hash: ContentHash,
    excluded: &mut HashSet<ContentHash>,
) -> Result<()> {
    if !excluded.insert(tree_hash) {
        return Ok(());
    }

    let tree = match store.get_tree(&tree_hash)? {
        Some(tree) => tree,
        None => return Ok(()),
    };

    for entry in tree.entries() {
        match entry.entry_type {
            EntryType::Blob | EntryType::Symlink => {
                excluded.insert(entry.hash);
            }
            EntryType::Tree => {
                collect_tree_hashes(store, entry.hash, excluded)?;
            }
        }
    }

    Ok(())
}

pub fn is_ancestor(
    store: &impl ObjectStore,
    ancestor: ChangeId,
    descendant: ChangeId,
) -> Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }

    let mut seen: HashSet<ChangeId> = HashSet::new();
    let mut queue: VecDeque<ChangeId> = VecDeque::new();
    queue.push_back(descendant);

    while let Some(id) = queue.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        let state = match store.get_state(&id)? {
            Some(s) => s,
            None => return Ok(false),
        };
        for parent in state.parents {
            if parent == ancestor {
                return Ok(true);
            }
            queue.push_back(parent);
        }
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use objects::{
        object::{Principal, Redaction, StateVisibility, VisibilityTier},
        store::ObjectStore,
    };
    use repo::Repository;
    use tempfile::TempDir;

    use super::{
        ObjectId, ObjectType, StateClosureOptions, enumerate_state_closure_plan_with_options,
        enumerate_state_closure_with_options,
    };

    #[test]
    fn lean_closure_planner_matches_object_info_ids_and_types() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("README.md"), "hello\n").unwrap();
        std::fs::write(temp.path().join("src/lib.rs"), "pub fn hi() {}\n").unwrap();
        let state = repo.snapshot(Some("seed".to_string()), None).unwrap();

        let full = enumerate_state_closure_with_options(
            repo.store(),
            state.change_id,
            StateClosureOptions::default(),
        )
        .unwrap();
        let lean = enumerate_state_closure_plan_with_options(
            repo.store(),
            state.change_id,
            StateClosureOptions::default(),
        )
        .unwrap();

        let full_pairs = full
            .into_iter()
            .map(|info| (info.id, info.obj_type))
            .collect::<std::collections::HashSet<_>>();
        let lean_pairs = lean
            .into_iter()
            .map(|info| (info.id, info.obj_type))
            .collect::<std::collections::HashSet<_>>();

        assert_eq!(full_pairs, lean_pairs);
        assert!(
            full_pairs
                .iter()
                .any(|(id, _)| matches!(id, ObjectId::ChangeId(_)))
        );
    }

    /// Once a redaction is declared for a blob in a snapshot, the
    /// state closure must include an `ObjectType::Redaction` entry
    /// keyed on that blob's hash — that's the wire-side signal the
    /// receiver replays.
    #[test]
    fn enumerate_state_closure_emits_redaction_for_redacted_blob() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("secret.toml"), "api_token = \"x\"\n").unwrap();
        let state = repo.snapshot(Some("seed".to_string()), None).unwrap();

        // Find the blob hash for secret.toml by walking the snapshot's tree.
        let tree = repo
            .store()
            .get_tree(&state.tree)
            .unwrap()
            .expect("tree present");
        let blob_hash = tree
            .iter()
            .find(|e| e.name == "secret.toml")
            .expect("entry present")
            .hash;

        let redaction = Redaction {
            redacted_blob: blob_hash,
            state: state.change_id,
            path: "secret.toml".to_string(),
            reason: "test leak".to_string(),
            redactor: Principal {
                name: "Tester".into(),
                email: "tester@heddle.sh".into(),
            },
            redacted_at: Utc::now(),
            signature: None,
            purged_at: None,
            supersedes: None,
        };
        repo.put_redaction(redaction).unwrap();

        let full = enumerate_state_closure_with_options(
            repo.store(),
            state.change_id,
            StateClosureOptions::default(),
        )
        .unwrap();
        let plan = enumerate_state_closure_plan_with_options(
            repo.store(),
            state.change_id,
            StateClosureOptions::default(),
        )
        .unwrap();

        assert!(
            full.iter()
                .any(|info| info.obj_type == ObjectType::Redaction
                    && info.id == ObjectId::Hash(blob_hash)),
            "full closure must include a Redaction entry for the redacted blob"
        );
        assert!(
            plan.iter()
                .any(|p| p.obj_type == ObjectType::Redaction && p.id == ObjectId::Hash(blob_hash)),
            "plan closure must include a Redaction entry for the redacted blob"
        );
    }

    #[test]
    fn enumerate_state_closure_emits_state_visibility_for_visible_state() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("README.md"), "hello\n").unwrap();
        let state = repo.snapshot(Some("seed".to_string()), None).unwrap();

        repo.put_state_visibility(StateVisibility {
            state: state.change_id,
            tier: VisibilityTier::Restricted {
                scope_label: "security-embargo".into(),
            },
            embargo_until: None,
            declarer: Principal {
                name: "Tester".into(),
                email: "tester@heddle.sh".into(),
            },
            declared_at: Utc::now(),
            signature: None,
            supersedes: None,
        })
        .unwrap();

        let full = enumerate_state_closure_with_options(
            repo.store(),
            state.change_id,
            StateClosureOptions::default(),
        )
        .unwrap();
        let plan = enumerate_state_closure_plan_with_options(
            repo.store(),
            state.change_id,
            StateClosureOptions::default(),
        )
        .unwrap();

        assert!(
            full.iter()
                .any(|info| info.obj_type == ObjectType::StateVisibility
                    && info.id == ObjectId::ChangeId(state.change_id)),
            "full closure must include a StateVisibility entry for the visible state"
        );
        assert!(
            plan.iter()
                .any(|p| p.obj_type == ObjectType::StateVisibility
                    && p.id == ObjectId::ChangeId(state.change_id)),
            "plan closure must include a StateVisibility entry for the visible state"
        );
    }
}
