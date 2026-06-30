// SPDX-License-Identifier: Apache-2.0
use std::collections::{HashSet, VecDeque};

use objects::{
    object::{ChangeId, ContentHash, EntryType, State},
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

impl PlannedObject {
    pub fn to_object_info(&self) -> ObjectInfo {
        ObjectInfo {
            id: self.id.clone(),
            obj_type: self.obj_type,
            size: 0,
            delta_base: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectTransferPlan {
    objects: Vec<PlannedObject>,
}

impl ObjectTransferPlan {
    pub fn from_objects(objects: Vec<PlannedObject>) -> Self {
        Self { objects }
    }

    pub fn objects(&self) -> &[PlannedObject] {
        &self.objects
    }

    pub fn into_objects(self) -> Vec<PlannedObject> {
        self.objects
    }

    pub fn iter(&self) -> std::slice::Iter<'_, PlannedObject> {
        self.objects.iter()
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    pub fn object_infos(&self, store: &impl ObjectStore) -> Result<Vec<ObjectInfo>> {
        self.objects
            .iter()
            .map(|object| planned_object_info(store, object))
            .collect()
    }
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
        for blob in state_blob_dependencies(&state) {
            enumerate_blob_filtered(store, blob, &excluded_hashes, &mut seen_hashes, &mut out)?;
        }
    }

    Ok(out)
}

pub fn enumerate_state_closure_plan(
    store: &impl ObjectStore,
    state_id: ChangeId,
) -> Result<Vec<PlannedObject>> {
    Ok(plan_state_transfer(store, state_id)?.into_objects())
}

pub fn enumerate_state_closure_plan_with_options(
    store: &impl ObjectStore,
    state_id: ChangeId,
    options: StateClosureOptions,
) -> Result<Vec<PlannedObject>> {
    Ok(plan_state_transfer_with_options(store, state_id, options)?.into_objects())
}

pub fn plan_state_transfer(
    store: &impl ObjectStore,
    state_id: ChangeId,
) -> Result<ObjectTransferPlan> {
    plan_state_transfer_with_options(store, state_id, StateClosureOptions::default())
}

pub fn plan_state_transfer_with_options(
    store: &impl ObjectStore,
    state_id: ChangeId,
    options: StateClosureOptions,
) -> Result<ObjectTransferPlan> {
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
        for blob in state_blob_dependencies(&state) {
            enumerate_blob_plan_filtered(
                store,
                blob,
                &excluded_hashes,
                &mut seen_hashes,
                &mut out,
            )?;
        }
    }

    Ok(ObjectTransferPlan::from_objects(out))
}

fn state_blob_dependencies(state: &State) -> impl Iterator<Item = ContentHash> + '_ {
    [
        state.risk_signals,
        state.review_signatures,
        state.discussions,
        state.structured_conflicts,
    ]
    .into_iter()
    .flatten()
}

fn planned_object_info(store: &impl ObjectStore, object: &PlannedObject) -> Result<ObjectInfo> {
    let size = match (&object.id, object.obj_type) {
        (ObjectId::Hash(hash), ObjectType::Blob) => store
            .get_blob(hash)?
            .ok_or_else(|| ProtocolError::ObjectNotFound(hash.to_hex()))?
            .size() as u64,
        (ObjectId::Hash(hash), ObjectType::Tree) => {
            let tree = store
                .get_tree(hash)?
                .ok_or_else(|| ProtocolError::ObjectNotFound(hash.to_hex()))?;
            rmp_serde::to_vec_named(&tree)?.len() as u64
        }
        (ObjectId::ChangeId(change_id), ObjectType::State) => {
            let state = store
                .get_state(change_id)?
                .ok_or_else(|| ProtocolError::ObjectNotFound(change_id.to_string()))?;
            rmp_serde::to_vec_named(&state)?.len() as u64
        }
        (ObjectId::Hash(hash), ObjectType::Action) => {
            let action_id = objects::object::ActionId::from_hash(*hash);
            let action = store
                .get_action(&action_id)?
                .ok_or_else(|| ProtocolError::ObjectNotFound(hash.to_hex()))?;
            rmp_serde::to_vec_named(&action)?.len() as u64
        }
        (ObjectId::Hash(hash), ObjectType::Redaction) => store
            .get_redactions_bytes_for_blob(hash)?
            .ok_or_else(|| ProtocolError::ObjectNotFound(hash.to_hex()))?
            .len() as u64,
        (ObjectId::ChangeId(change_id), ObjectType::StateVisibility) => store
            .get_state_visibility_bytes_for_state(change_id)?
            .ok_or_else(|| ProtocolError::ObjectNotFound(change_id.to_string_full()))?
            .len() as u64,
        _ => {
            return Err(ProtocolError::InvalidState(
                "object id/type mismatch".to_string(),
            ));
        }
    };

    Ok(ObjectInfo {
        id: object.id.clone(),
        obj_type: object.obj_type,
        size,
        delta_base: None,
    })
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

fn enumerate_blob_filtered(
    store: &impl ObjectStore,
    blob_hash: ContentHash,
    excluded: &HashSet<ContentHash>,
    seen: &mut HashSet<ContentHash>,
    out: &mut Vec<ObjectInfo>,
) -> Result<()> {
    if excluded.contains(&blob_hash) || !seen.insert(blob_hash) {
        return Ok(());
    }
    let blob = store
        .get_blob(&blob_hash)?
        .ok_or_else(|| ProtocolError::ObjectNotFound(blob_hash.to_hex()))?;
    out.push(ObjectInfo {
        id: ObjectId::Hash(blob_hash),
        obj_type: ObjectType::Blob,
        size: blob.size() as u64,
        delta_base: None,
    });
    emit_redaction_info(store, &blob_hash, out)
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

fn enumerate_blob_plan_filtered(
    store: &impl ObjectStore,
    blob_hash: ContentHash,
    excluded: &HashSet<ContentHash>,
    seen: &mut HashSet<ContentHash>,
    out: &mut Vec<PlannedObject>,
) -> Result<()> {
    if excluded.contains(&blob_hash) || !seen.insert(blob_hash) {
        return Ok(());
    }
    if store.get_blob(&blob_hash)?.is_none() {
        return Err(ProtocolError::ObjectNotFound(blob_hash.to_hex()));
    }
    out.push(PlannedObject {
        id: ObjectId::Hash(blob_hash),
        obj_type: ObjectType::Blob,
    });
    emit_redaction_plan(store, &blob_hash, out)
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
        for blob in state_blob_dependencies(&state) {
            excluded_hashes.insert(blob);
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
    use std::collections::HashSet;

    use chrono::Utc;
    use objects::{
        object::{
            Attribution, Blob, ChangeId, Discussion, DiscussionResolution, DiscussionTurn,
            DiscussionsBlob, Principal, Redaction, State, StateVisibility, SymbolAnchor, Tree,
            TreeEntry, VisibilityTier,
        },
        store::ObjectStore,
    };
    use repo::Repository;
    use tempfile::TempDir;

    use super::{
        ObjectId, ObjectInfo, ObjectType, PlannedObject, StateClosureOptions,
        enumerate_state_closure_plan_with_options, enumerate_state_closure_with_options,
        plan_state_transfer_with_options,
    };

    fn pairs_from_full(objects: &[ObjectInfo]) -> HashSet<(ObjectId, ObjectType)> {
        objects
            .iter()
            .map(|info| (info.id.clone(), info.obj_type))
            .collect()
    }

    fn pairs_from_plan(objects: &[PlannedObject]) -> HashSet<(ObjectId, ObjectType)> {
        objects
            .iter()
            .map(|info| (info.id.clone(), info.obj_type))
            .collect()
    }

    fn assert_plan_parity(
        repo: &Repository,
        state_id: ChangeId,
        options: StateClosureOptions,
    ) -> HashSet<(ObjectId, ObjectType)> {
        let full =
            enumerate_state_closure_with_options(repo.store(), state_id, options.clone()).unwrap();
        let plan =
            enumerate_state_closure_plan_with_options(repo.store(), state_id, options).unwrap();

        let full_pairs = pairs_from_full(&full);
        let plan_pairs = pairs_from_plan(&plan);
        assert_eq!(full_pairs, plan_pairs);
        full_pairs
    }

    fn test_attribution() -> Attribution {
        Attribution::human(Principal::new("Graph Tester", "graph@example.com"))
    }

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

    #[test]
    fn transfer_plan_enriches_object_infos_from_planned_closure() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("README.md"), "hello\n").unwrap();
        let state = repo.snapshot(Some("seed".to_string()), None).unwrap();

        let plan = plan_state_transfer_with_options(
            repo.store(),
            state.change_id,
            StateClosureOptions::default(),
        )
        .unwrap();
        let planned_infos = plan.object_infos(repo.store()).unwrap();
        let full = enumerate_state_closure_with_options(
            repo.store(),
            state.change_id,
            StateClosureOptions::default(),
        )
        .unwrap();

        assert_eq!(pairs_from_full(&planned_infos), pairs_from_full(&full));
        assert!(
            planned_infos
                .iter()
                .filter(|info| !matches!(info.obj_type, ObjectType::Redaction))
                .all(|info| info.size > 0),
            "planned info enrichment must carry sizes for primary objects"
        );
    }

    #[test]
    fn depth_and_exclude_options_match_between_full_and_plan() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let path = temp.path().join("story.txt");

        std::fs::write(&path, "base\n").unwrap();
        let base = repo.snapshot(Some("base".to_string()), None).unwrap();
        std::fs::write(&path, "middle\n").unwrap();
        let middle = repo.snapshot(Some("middle".to_string()), None).unwrap();
        std::fs::write(&path, "tip\n").unwrap();
        let tip = repo.snapshot(Some("tip".to_string()), None).unwrap();

        let depth_zero = assert_plan_parity(
            &repo,
            tip.change_id,
            StateClosureOptions {
                depth: Some(0),
                exclude_states: Vec::new(),
            },
        );
        assert!(depth_zero.contains(&(ObjectId::ChangeId(tip.change_id), ObjectType::State)));
        assert!(!depth_zero.contains(&(ObjectId::ChangeId(middle.change_id), ObjectType::State)));
        assert!(!depth_zero.contains(&(ObjectId::ChangeId(base.change_id), ObjectType::State)));

        let depth_one = assert_plan_parity(
            &repo,
            tip.change_id,
            StateClosureOptions {
                depth: Some(1),
                exclude_states: Vec::new(),
            },
        );
        assert!(depth_one.contains(&(ObjectId::ChangeId(tip.change_id), ObjectType::State)));
        assert!(depth_one.contains(&(ObjectId::ChangeId(middle.change_id), ObjectType::State)));
        assert!(!depth_one.contains(&(ObjectId::ChangeId(base.change_id), ObjectType::State)));

        let exclude_middle = assert_plan_parity(
            &repo,
            tip.change_id,
            StateClosureOptions {
                depth: None,
                exclude_states: vec![middle.change_id],
            },
        );
        assert!(exclude_middle.contains(&(ObjectId::ChangeId(tip.change_id), ObjectType::State)));
        assert!(
            !exclude_middle.contains(&(ObjectId::ChangeId(middle.change_id), ObjectType::State))
        );
        assert!(!exclude_middle.contains(&(ObjectId::ChangeId(base.change_id), ObjectType::State)));
    }

    #[test]
    fn shared_tree_and_blob_references_are_emitted_once() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();

        let shared_blob = Blob::from("shared contents\n");
        let shared_blob_hash = repo.store().put_blob(&shared_blob).unwrap();
        let shared_tree = Tree::from_entries(vec![
            TreeEntry::file("shared.txt", shared_blob_hash, false).unwrap(),
        ]);
        let shared_tree_hash = repo.store().put_tree(&shared_tree).unwrap();
        let root = Tree::from_entries(vec![
            TreeEntry::directory("left", shared_tree_hash).unwrap(),
            TreeEntry::directory("right", shared_tree_hash).unwrap(),
        ]);
        let root_hash = repo.store().put_tree(&root).unwrap();
        let state = State::new(root_hash, Vec::new(), test_attribution());
        repo.store().put_state(&state).unwrap();

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

        assert_eq!(
            pairs_from_full(&full),
            pairs_from_plan(&plan),
            "full and lean closure enumerators must dedup the same objects"
        );

        assert_eq!(
            full.iter()
                .filter(|info| info.id == ObjectId::Hash(root_hash)
                    && info.obj_type == ObjectType::Tree)
                .count(),
            1
        );
        assert_eq!(
            full.iter()
                .filter(|info| info.id == ObjectId::Hash(shared_tree_hash)
                    && info.obj_type == ObjectType::Tree)
                .count(),
            1
        );
        assert_eq!(
            full.iter()
                .filter(|info| info.id == ObjectId::Hash(shared_blob_hash)
                    && info.obj_type == ObjectType::Blob)
                .count(),
            1
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

    #[test]
    fn enumerate_state_closure_emits_discussions_blob() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("README.md"), "hello\n").unwrap();
        let state = repo.snapshot(Some("seed".to_string()), None).unwrap();

        let principal = Principal::new("Tester", "tester@example.test");
        let discussion_bytes = DiscussionsBlob::new(vec![Discussion {
            id: "disc-1".to_string(),
            anchor: SymbolAnchor::new("src/lib.rs", "answer"),
            opened_against_state: state.change_id,
            opened_at: 1_782_400_000,
            thread_ref: None,
            turns: vec![DiscussionTurn {
                author: principal,
                body: "Should this sync?".to_string(),
                posted_at: 1_782_400_000,
            }],
            resolution: DiscussionResolution::Open,
            body_changed_since_open: false,
            orphaned: false,
            visibility: VisibilityTier::default(),
            resolved_annotation_id: None,
        }])
        .encode()
        .expect("encode discussions");
        let discussion_hash = repo
            .store()
            .put_blob(&Blob::new(discussion_bytes))
            .expect("put discussions blob");
        let state_with_discussions = state.with_discussions(discussion_hash);
        repo.store()
            .put_state(&state_with_discussions)
            .expect("put state with discussions");

        let full = enumerate_state_closure_with_options(
            repo.store(),
            state_with_discussions.change_id,
            StateClosureOptions::default(),
        )
        .unwrap();
        let plan = enumerate_state_closure_plan_with_options(
            repo.store(),
            state_with_discussions.change_id,
            StateClosureOptions::default(),
        )
        .unwrap();

        assert!(
            full.iter().any(|info| info.obj_type == ObjectType::Blob
                && info.id == ObjectId::Hash(discussion_hash)),
            "full closure must include the discussions blob referenced by the state"
        );
        assert!(
            plan.iter()
                .any(|p| p.obj_type == ObjectType::Blob && p.id == ObjectId::Hash(discussion_hash)),
            "plan closure must include the discussions blob referenced by the state"
        );
    }

    #[test]
    fn enumerate_state_closure_emits_state_tail_metadata_blobs() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("README.md"), "hello\n").unwrap();
        let state = repo.snapshot(Some("seed".to_string()), None).unwrap();

        let risk = repo
            .store()
            .put_blob(&Blob::from("risk signals\n"))
            .expect("put risk signals");
        let review = repo
            .store()
            .put_blob(&Blob::from("review signatures\n"))
            .expect("put review signatures");
        let conflicts = repo
            .store()
            .put_blob(&Blob::from("structured conflicts\n"))
            .expect("put structured conflicts");
        let state_with_tail = state
            .with_risk_signals(risk)
            .with_review_signatures(review)
            .with_structured_conflicts(conflicts);
        repo.store()
            .put_state(&state_with_tail)
            .expect("put state with tail metadata");

        let full = enumerate_state_closure_with_options(
            repo.store(),
            state_with_tail.change_id,
            StateClosureOptions::default(),
        )
        .unwrap();
        let plan = enumerate_state_closure_plan_with_options(
            repo.store(),
            state_with_tail.change_id,
            StateClosureOptions::default(),
        )
        .unwrap();

        for hash in [risk, review, conflicts] {
            assert!(
                full.iter().any(
                    |info| info.obj_type == ObjectType::Blob && info.id == ObjectId::Hash(hash)
                ),
                "full closure must include state tail metadata blob {}",
                hash.to_hex()
            );
            assert!(
                plan.iter()
                    .any(|p| p.obj_type == ObjectType::Blob && p.id == ObjectId::Hash(hash)),
                "plan closure must include state tail metadata blob {}",
                hash.to_hex()
            );
        }
    }
}
