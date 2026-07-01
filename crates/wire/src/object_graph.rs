// SPDX-License-Identifier: Apache-2.0
use std::collections::{HashSet, VecDeque};

use objects::{
    object::{ChangeId, ContentHash, State, TreeEntryTarget},
    store::{ObjectStore, pack::ObjectType as PackObjectType},
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

#[derive(Debug, Clone)]
pub struct StateClosureTransferObjects {
    pub planned_objects: Vec<PlannedObject>,
    pub full_objects: Option<Vec<ObjectInfo>>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectTypeBucket {
    Blob,
    Tree,
    State,
    Action,
    Redaction,
    StateVisibility,
}

impl ObjectType {
    pub fn wire_name(self) -> &'static str {
        match self {
            ObjectType::Blob => "blob",
            ObjectType::Tree => "tree",
            ObjectType::State => "state",
            ObjectType::Action => "action",
            ObjectType::Redaction => "redaction",
            ObjectType::StateVisibility => "state_visibility",
        }
    }

    pub fn from_wire(value: &str) -> Result<Self> {
        match value {
            "blob" => Ok(ObjectType::Blob),
            "tree" => Ok(ObjectType::Tree),
            "state" => Ok(ObjectType::State),
            "action" => Ok(ObjectType::Action),
            "redaction" => Ok(ObjectType::Redaction),
            "state_visibility" => Ok(ObjectType::StateVisibility),
            _ => Err(ProtocolError::InvalidState(format!(
                "unknown object type: {value}"
            ))),
        }
    }

    pub fn packable(self) -> bool {
        !matches!(self, ObjectType::Redaction | ObjectType::StateVisibility)
    }

    pub fn pack_object_type(self) -> Result<PackObjectType> {
        match self {
            ObjectType::Blob => Ok(PackObjectType::Blob),
            ObjectType::Tree => Ok(PackObjectType::Tree),
            ObjectType::State => Ok(PackObjectType::State),
            ObjectType::Action => Ok(PackObjectType::Action),
            ObjectType::Redaction => Err(ProtocolError::InvalidState(
                "Redaction sidecar records cannot be packed into the content-addressed object pack"
                    .to_string(),
            )),
            ObjectType::StateVisibility => Err(ProtocolError::InvalidState(
                "StateVisibility sidecar records cannot be packed into the content-addressed object pack"
                    .to_string(),
            )),
        }
    }

    pub fn bucket(self) -> ObjectTypeBucket {
        match self {
            ObjectType::Blob => ObjectTypeBucket::Blob,
            ObjectType::Tree => ObjectTypeBucket::Tree,
            ObjectType::State => ObjectTypeBucket::State,
            ObjectType::Action => ObjectTypeBucket::Action,
            ObjectType::Redaction => ObjectTypeBucket::Redaction,
            ObjectType::StateVisibility => ObjectTypeBucket::StateVisibility,
        }
    }
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
    let mut out = Vec::new();
    walk_state_closure(store, state_id, options, |event| {
        if let Some(info) = object_info_from_event(store, event)? {
            out.push(info);
        }
        Ok(())
    })?;

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
    let mut out = Vec::new();
    walk_state_closure(store, state_id, options, |event| {
        if let Some(object) = planned_object_from_event(store, event)? {
            out.push(object);
        }
        Ok(())
    })?;

    Ok(out)
}

pub fn enumerate_state_closure_transfer_with_options(
    store: &impl ObjectStore,
    state_id: ChangeId,
    options: StateClosureOptions,
    full_descriptor_object_threshold: usize,
) -> Result<StateClosureTransferObjects> {
    let mut planned_objects = Vec::new();
    let mut full_objects = Some(Vec::new());

    walk_state_closure(store, state_id, options, |event| {
        if let Some(object) = planned_object_from_event(store, event)? {
            planned_objects.push(object);
        }

        if full_objects.is_some() && planned_objects.len() > full_descriptor_object_threshold {
            full_objects = None;
        }
        if let Some(objects) = full_objects.as_mut()
            && let Some(info) = object_info_from_event(store, event)?
        {
            objects.push(info);
        }

        Ok(())
    })?;

    Ok(StateClosureTransferObjects {
        planned_objects,
        full_objects,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlobSource {
    Tree,
    StateMetadata,
}

#[derive(Debug, Clone, Copy)]
enum StateClosureEvent<'a> {
    State { id: ChangeId, state: &'a State },
    Tree { hash: ContentHash, tree: &'a objects::object::Tree },
    Blob { hash: ContentHash, source: BlobSource },
    Redaction { blob: ContentHash },
    StateVisibility { state: ChangeId },
    ExcludedState { id: ChangeId },
    ExcludedHash { hash: ContentHash },
}

fn walk_state_closure(
    store: &impl ObjectStore,
    state_id: ChangeId,
    options: StateClosureOptions,
    mut visit: impl for<'event> FnMut(StateClosureEvent<'event>) -> Result<()>,
) -> Result<()> {
    let (excluded_states, excluded_hashes) = collect_excluded(store, &options.exclude_states)?;

    let mut seen_states: HashSet<ChangeId> = HashSet::new();
    let mut seen_hashes: HashSet<ContentHash> = HashSet::new();
    let mut queue: VecDeque<(ChangeId, u32)> = VecDeque::new();
    queue.push_back((state_id, 0));

    while let Some((id, depth)) = queue.pop_front() {
        if excluded_states.contains(&id) {
            visit(StateClosureEvent::ExcludedState { id })?;
            continue;
        }
        if !seen_states.insert(id) {
            continue;
        }

        let state = store
            .get_state(&id)?
            .ok_or_else(|| ProtocolError::ObjectNotFound(id.to_string()))?;

        visit(StateClosureEvent::State { id, state: &state })?;
        if store.has_state_visibility_for_state(&id)? {
            visit(StateClosureEvent::StateVisibility { state: id })?;
        }

        if options.depth.map(|max| depth < max).unwrap_or(true) {
            for parent in &state.parents {
                queue.push_back((*parent, depth + 1));
            }
        }

        walk_tree_closure_filtered(
            store,
            state.tree,
            &excluded_hashes,
            &mut seen_hashes,
            &mut visit,
        )?;
        if let Some(provenance_root) = state.provenance {
            walk_tree_closure_filtered(
                store,
                provenance_root,
                &excluded_hashes,
                &mut seen_hashes,
                &mut visit,
            )?;
        }
        if let Some(context_root) = state.context {
            walk_tree_closure_filtered(
                store,
                context_root,
                &excluded_hashes,
                &mut seen_hashes,
                &mut visit,
            )?;
        }
        for metadata_blob in state_blob_dependencies(&state) {
            walk_blob_filtered(
                store,
                metadata_blob,
                BlobSource::StateMetadata,
                &excluded_hashes,
                &mut seen_hashes,
                &mut visit,
            )?;
        }
    }

    Ok(())
}

fn walk_tree_closure_filtered(
    store: &impl ObjectStore,
    tree_hash: ContentHash,
    excluded: &HashSet<ContentHash>,
    seen: &mut HashSet<ContentHash>,
    visit: &mut impl for<'event> FnMut(StateClosureEvent<'event>) -> Result<()>,
) -> Result<()> {
    if excluded.contains(&tree_hash) {
        visit(StateClosureEvent::ExcludedHash { hash: tree_hash })?;
        return Ok(());
    }
    if !seen.insert(tree_hash) {
        return Ok(());
    }

    let tree = store
        .get_tree(&tree_hash)?
        .ok_or_else(|| ProtocolError::ObjectNotFound(tree_hash.to_hex()))?;

    visit(StateClosureEvent::Tree {
        hash: tree_hash,
        tree: &tree,
    })?;

    for entry in tree.entries() {
        match entry.target() {
            TreeEntryTarget::Blob { hash, .. } | TreeEntryTarget::Symlink { hash } => {
                walk_blob_filtered(
                    store,
                    *hash,
                    BlobSource::Tree,
                    excluded,
                    seen,
                    visit,
                )?;
            }
            TreeEntryTarget::Tree { hash } => {
                walk_tree_closure_filtered(store, *hash, excluded, seen, visit)?;
            }
            TreeEntryTarget::Gitlink { .. } => {}
        }
    }

    Ok(())
}

fn walk_blob_filtered(
    store: &impl ObjectStore,
    blob_hash: ContentHash,
    source: BlobSource,
    excluded: &HashSet<ContentHash>,
    seen: &mut HashSet<ContentHash>,
    visit: &mut impl for<'event> FnMut(StateClosureEvent<'event>) -> Result<()>,
) -> Result<()> {
    if excluded.contains(&blob_hash) {
        visit(StateClosureEvent::ExcludedHash { hash: blob_hash })?;
        return Ok(());
    }
    if !seen.insert(blob_hash) {
        return Ok(());
    }
    visit(StateClosureEvent::Blob {
        hash: blob_hash,
        source,
    })?;
    if store.has_redactions_for_blob(&blob_hash)? {
        visit(StateClosureEvent::Redaction { blob: blob_hash })?;
    }
    Ok(())
}

fn object_info_from_event(
    store: &impl ObjectStore,
    event: StateClosureEvent<'_>,
) -> Result<Option<ObjectInfo>> {
    match event {
        StateClosureEvent::State { id, state } => {
            let state_bytes = rmp_serde::to_vec_named(state)?;
            Ok(Some(ObjectInfo {
                id: ObjectId::ChangeId(id),
                obj_type: ObjectType::State,
                size: state_bytes.len() as u64,
                delta_base: None,
            }))
        }
        StateClosureEvent::Tree { hash, tree } => {
            let tree_bytes = rmp_serde::to_vec_named(tree)?;
            Ok(Some(ObjectInfo {
                id: ObjectId::Hash(hash),
                obj_type: ObjectType::Tree,
                size: tree_bytes.len() as u64,
                delta_base: None,
            }))
        }
        StateClosureEvent::Blob { hash, .. } => {
            let blob = store
                .get_blob(&hash)?
                .ok_or_else(|| ProtocolError::ObjectNotFound(hash.to_hex()))?;
            Ok(Some(ObjectInfo {
                id: ObjectId::Hash(hash),
                obj_type: ObjectType::Blob,
                size: blob.size() as u64,
                delta_base: None,
            }))
        }
        StateClosureEvent::Redaction { blob } => {
            Ok(store.get_redactions_bytes_for_blob(&blob)?.map(|bytes| ObjectInfo {
                id: ObjectId::Hash(blob),
                obj_type: ObjectType::Redaction,
                size: bytes.len() as u64,
                delta_base: None,
            }))
        }
        StateClosureEvent::StateVisibility { state } => Ok(store
            .get_state_visibility_bytes_for_state(&state)?
            .map(|bytes| ObjectInfo {
                id: ObjectId::ChangeId(state),
                obj_type: ObjectType::StateVisibility,
                size: bytes.len() as u64,
                delta_base: None,
            })),
        StateClosureEvent::ExcludedState { id } => {
            let _ = id;
            Ok(None)
        }
        StateClosureEvent::ExcludedHash { hash } => {
            let _ = hash;
            Ok(None)
        }
    }
}

fn planned_object_from_event(
    store: &impl ObjectStore,
    event: StateClosureEvent<'_>,
) -> Result<Option<PlannedObject>> {
    match event {
        StateClosureEvent::State { id, .. } => Ok(Some(PlannedObject {
            id: ObjectId::ChangeId(id),
            obj_type: ObjectType::State,
        })),
        StateClosureEvent::Tree { hash, .. } => Ok(Some(PlannedObject {
            id: ObjectId::Hash(hash),
            obj_type: ObjectType::Tree,
        })),
        StateClosureEvent::Blob { hash, source } => {
            if source == BlobSource::StateMetadata && store.get_blob(&hash)?.is_none() {
                return Err(ProtocolError::ObjectNotFound(hash.to_hex()));
            }
            Ok(Some(PlannedObject {
                id: ObjectId::Hash(hash),
                obj_type: ObjectType::Blob,
            }))
        }
        StateClosureEvent::Redaction { blob } => Ok(Some(PlannedObject {
            id: ObjectId::Hash(blob),
            obj_type: ObjectType::Redaction,
        })),
        StateClosureEvent::StateVisibility { state } => Ok(Some(PlannedObject {
            id: ObjectId::ChangeId(state),
            obj_type: ObjectType::StateVisibility,
        })),
        StateClosureEvent::ExcludedState { id } => {
            let _ = id;
            Ok(None)
        }
        StateClosureEvent::ExcludedHash { hash } => {
            let _ = hash;
            Ok(None)
        }
    }
}

pub fn missing_blobs_in_tree(
    store: &impl ObjectStore,
    tree_hash: ContentHash,
) -> Result<Vec<ContentHash>> {
    let mut missing = Vec::new();
    collect_missing_blobs_recursive(store, &tree_hash, &mut missing)?;
    Ok(missing)
}

fn collect_missing_blobs_recursive(
    store: &impl ObjectStore,
    tree_hash: &ContentHash,
    missing: &mut Vec<ContentHash>,
) -> Result<()> {
    let Some(tree) = store.get_tree(tree_hash).map_err(|err| {
        ProtocolError::InvalidState(format!(
            "load tree {} while collecting lazy hydration missing blobs: {err}",
            tree_hash.to_hex()
        ))
    })?
    else {
        return Ok(());
    };

    for entry in tree.entries() {
        match entry.target() {
            TreeEntryTarget::Blob { hash, .. } | TreeEntryTarget::Symlink { hash } => {
                if !store.has_blob(hash).map_err(|err| {
                    ProtocolError::InvalidState(format!(
                        "check blob {} while collecting lazy hydration missing blobs: {err}",
                        hash.to_hex()
                    ))
                })? {
                    missing.push(*hash);
                }
            }
            TreeEntryTarget::Tree { hash } => {
                collect_missing_blobs_recursive(store, hash, missing)?;
            }
            TreeEntryTarget::Gitlink { .. } => {}
        }
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
        for metadata_blob in state_blob_dependencies(&state) {
            excluded_hashes.insert(metadata_blob);
        }
    }

    Ok((excluded_states, excluded_hashes))
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
        match entry.target() {
            TreeEntryTarget::Blob { hash, .. } | TreeEntryTarget::Symlink { hash } => {
                excluded.insert(*hash);
            }
            TreeEntryTarget::Tree { hash } => {
                collect_tree_hashes(store, *hash, excluded)?;
            }
            TreeEntryTarget::Gitlink { .. } => {}
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
            Action, ActionId, Attribution, Blob, ChangeId, ContentHash, Discussion,
            DiscussionResolution, DiscussionTurn, DiscussionsBlob, Principal, Redaction, State,
            StateVisibility, SymbolAnchor, Tree, TreeEntry, VisibilityTier,
        },
        store::{ObjectStore, Result as StoreResult},
    };
    use repo::Repository;
    use sley::ObjectId as GitObjectId;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    use super::{
        ObjectId, ObjectInfo, ObjectType, PlannedObject, StateClosureOptions,
        enumerate_state_closure_plan_with_options, enumerate_state_closure_transfer_with_options,
        enumerate_state_closure_with_options, missing_blobs_in_tree,
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

    fn object_info_fingerprint(
        objects: &[ObjectInfo],
    ) -> Vec<(ObjectId, ObjectType, u64, Option<ContentHash>)> {
        objects
            .iter()
            .map(|info| {
                (
                    info.id.clone(),
                    info.obj_type,
                    info.size,
                    info.delta_base,
                )
            })
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

    fn assert_contains_object(
        objects: &HashSet<(ObjectId, ObjectType)>,
        id: ObjectId,
        obj_type: ObjectType,
    ) {
        assert!(
            objects.contains(&(id.clone(), obj_type)),
            "expected closure to contain {id:?} as {obj_type:?}: {objects:?}"
        );
    }

    struct CountingStore<'a, S> {
        inner: &'a S,
        state_reads: AtomicUsize,
    }

    impl<'a, S> CountingStore<'a, S> {
        fn new(inner: &'a S) -> Self {
            Self {
                inner,
                state_reads: AtomicUsize::new(0),
            }
        }

        fn state_reads(&self) -> usize {
            self.state_reads.load(Ordering::SeqCst)
        }
    }

    impl<S: ObjectStore> ObjectStore for CountingStore<'_, S> {
        fn get_blob(&self, hash: &ContentHash) -> StoreResult<Option<Blob>> {
            self.inner.get_blob(hash)
        }

        fn put_blob(&self, blob: &Blob) -> StoreResult<ContentHash> {
            self.inner.put_blob(blob)
        }

        fn has_blob(&self, hash: &ContentHash) -> StoreResult<bool> {
            self.inner.has_blob(hash)
        }

        fn get_tree(&self, hash: &ContentHash) -> StoreResult<Option<Tree>> {
            self.inner.get_tree(hash)
        }

        fn put_tree(&self, tree: &Tree) -> StoreResult<ContentHash> {
            self.inner.put_tree(tree)
        }

        fn has_tree(&self, hash: &ContentHash) -> StoreResult<bool> {
            self.inner.has_tree(hash)
        }

        fn get_state(&self, id: &ChangeId) -> StoreResult<Option<State>> {
            self.state_reads.fetch_add(1, Ordering::SeqCst);
            self.inner.get_state(id)
        }

        fn put_state(&self, state: &State) -> StoreResult<()> {
            self.inner.put_state(state)
        }

        fn has_state(&self, id: &ChangeId) -> StoreResult<bool> {
            self.inner.has_state(id)
        }

        fn list_states(&self) -> StoreResult<Vec<ChangeId>> {
            self.inner.list_states()
        }

        fn get_action(&self, id: &ActionId) -> StoreResult<Option<Action>> {
            self.inner.get_action(id)
        }

        fn put_action(&self, action: &mut Action) -> StoreResult<ActionId> {
            self.inner.put_action(action)
        }

        fn list_actions(&self) -> StoreResult<Vec<ActionId>> {
            self.inner.list_actions()
        }

        fn list_blobs(&self) -> StoreResult<Vec<ContentHash>> {
            self.inner.list_blobs()
        }

        fn list_trees(&self) -> StoreResult<Vec<ContentHash>> {
            self.inner.list_trees()
        }
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
    fn transfer_projection_matches_full_and_plan_on_mixed_state_closure_fixture() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();

        let excluded_blob = repo
            .store()
            .put_blob(&Blob::from("excluded"))
            .expect("put excluded blob");
        let excluded_tree_hash = repo
            .store()
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("excluded.txt", excluded_blob, false).unwrap(),
            ]))
            .expect("put excluded tree");
        let excluded_parent = State::new(excluded_tree_hash, Vec::new(), test_attribution());
        repo.store()
            .put_state(&excluded_parent)
            .expect("put excluded parent");

        let redacted_blob = repo
            .store()
            .put_blob(&Blob::from("secret"))
            .expect("put redacted blob");
        let nested_blob = repo
            .store()
            .put_blob(&Blob::from("nested"))
            .expect("put nested blob");
        let symlink_blob = repo
            .store()
            .put_blob(&Blob::from("target"))
            .expect("put symlink blob");
        let context_blob = repo
            .store()
            .put_blob(&Blob::from("context"))
            .expect("put context blob");
        let provenance_blob = repo
            .store()
            .put_blob(&Blob::from("provenance"))
            .expect("put provenance blob");
        let risk_blob = repo
            .store()
            .put_blob(&Blob::from("risk"))
            .expect("put risk blob");
        let review_blob = repo
            .store()
            .put_blob(&Blob::from("review"))
            .expect("put review blob");
        let discussions_blob = repo
            .store()
            .put_blob(&Blob::from("discussion"))
            .expect("put discussion blob");
        let conflicts_blob = repo
            .store()
            .put_blob(&Blob::from("conflicts"))
            .expect("put conflicts blob");

        let nested_tree_hash = repo
            .store()
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("nested.txt", nested_blob, false).unwrap(),
                TreeEntry::symlink("latest", symlink_blob).unwrap(),
            ]))
            .expect("put nested tree");
        let context_tree_hash = repo
            .store()
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("context.txt", context_blob, false).unwrap(),
            ]))
            .expect("put context tree");
        let provenance_tree_hash = repo
            .store()
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("lineage.txt", provenance_blob, false).unwrap(),
            ]))
            .expect("put provenance tree");
        let gitlink_target: GitObjectId = "0303030303030303030303030303030303030303"
            .parse()
            .expect("git oid");
        let root_tree_hash = repo
            .store()
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("secret.txt", redacted_blob, false).unwrap(),
                TreeEntry::directory("nested", nested_tree_hash).unwrap(),
                TreeEntry::gitlink("vendor", gitlink_target).unwrap(),
            ]))
            .expect("put root tree");
        let state = State::new(
            root_tree_hash,
            vec![excluded_parent.change_id],
            test_attribution(),
        )
        .with_context(context_tree_hash)
        .with_provenance(provenance_tree_hash)
        .with_risk_signals(risk_blob)
        .with_review_signatures(review_blob)
        .with_discussions(discussions_blob)
        .with_structured_conflicts(conflicts_blob);
        repo.store().put_state(&state).expect("put state");

        repo.put_redaction(Redaction {
            redacted_blob,
            state: state.change_id,
            path: "secret.txt".to_string(),
            reason: "test leak".to_string(),
            redactor: Principal::new("Tester", "tester@example.test"),
            redacted_at: Utc::now(),
            signature: None,
            purged_at: None,
            supersedes: None,
        })
        .expect("put redaction");
        repo.put_state_visibility(StateVisibility {
            state: state.change_id,
            tier: VisibilityTier::Restricted {
                scope_label: "security".to_string(),
            },
            embargo_until: None,
            declarer: Principal::new("Tester", "tester@example.test"),
            declared_at: Utc::now(),
            signature: None,
            supersedes: None,
        })
        .expect("put visibility");

        let options = StateClosureOptions {
            depth: None,
            exclude_states: vec![excluded_parent.change_id],
        };
        let transfer = enumerate_state_closure_transfer_with_options(
            repo.store(),
            state.change_id,
            options.clone(),
            512,
        )
        .expect("transfer projection");

        let full = enumerate_state_closure_with_options(
            repo.store(),
            state.change_id,
            options.clone(),
        )
        .expect("full closure");
        let plan = enumerate_state_closure_plan_with_options(
            repo.store(),
            state.change_id,
            options,
        )
        .expect("plan closure");
        assert_eq!(
            transfer.full_objects.as_deref().map(object_info_fingerprint),
            Some(object_info_fingerprint(&full))
        );
        assert_eq!(transfer.planned_objects, plan);

        let full_pairs = pairs_from_full(&full);
        assert_eq!(full_pairs, pairs_from_plan(&plan));
        assert_contains_object(
            &full_pairs,
            ObjectId::ChangeId(state.change_id),
            ObjectType::State,
        );
        assert_contains_object(
            &full_pairs,
            ObjectId::ChangeId(state.change_id),
            ObjectType::StateVisibility,
        );
        assert_contains_object(&full_pairs, ObjectId::Hash(redacted_blob), ObjectType::Blob);
        assert_contains_object(&full_pairs, ObjectId::Hash(redacted_blob), ObjectType::Redaction);
        for hash in [
            root_tree_hash,
            nested_tree_hash,
            context_tree_hash,
            provenance_tree_hash,
        ] {
            assert_contains_object(&full_pairs, ObjectId::Hash(hash), ObjectType::Tree);
        }
        for hash in [
            nested_blob,
            symlink_blob,
            context_blob,
            provenance_blob,
            risk_blob,
            review_blob,
            discussions_blob,
            conflicts_blob,
        ] {
            assert_contains_object(&full_pairs, ObjectId::Hash(hash), ObjectType::Blob);
        }
        assert!(!full_pairs.contains(&(
            ObjectId::ChangeId(excluded_parent.change_id),
            ObjectType::State
        )));
        assert!(!full_pairs.contains(&(ObjectId::Hash(excluded_tree_hash), ObjectType::Tree)));
        assert!(!full_pairs.contains(&(ObjectId::Hash(excluded_blob), ObjectType::Blob)));
    }

    #[test]
    fn transfer_projection_reads_root_state_once_on_small_transfer() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let blob = repo
            .store()
            .put_blob(&Blob::from("hello\n"))
            .expect("put blob");
        let tree_hash = repo
            .store()
            .put_tree(&Tree::from_entries(vec![
                TreeEntry::file("README.md", blob, false).unwrap(),
            ]))
            .expect("put tree");
        let state = State::new(tree_hash, Vec::new(), test_attribution());
        repo.store().put_state(&state).expect("put state");
        let store = CountingStore::new(repo.store());

        let transfer = enumerate_state_closure_transfer_with_options(
            &store,
            state.change_id,
            StateClosureOptions::default(),
            512,
        )
        .expect("transfer projection");

        assert!(
            !transfer.planned_objects.is_empty(),
            "lean projection should be available"
        );
        assert!(transfer.full_objects.is_some());
        assert_eq!(
            store.state_reads(),
            1,
            "small transfer projection must not read the root state through a second closure walk"
        );
    }

    #[test]
    fn transfer_projection_drops_full_descriptors_after_threshold() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("README.md"), "hello\n").unwrap();
        let state = repo.snapshot(Some("seed".to_string()), None).unwrap();

        let transfer = enumerate_state_closure_transfer_with_options(
            repo.store(),
            state.change_id,
            StateClosureOptions::default(),
            0,
        )
        .expect("transfer projection");

        assert!(
            !transfer.planned_objects.is_empty(),
            "lean projection should still be available over the threshold"
        );
        assert!(transfer.full_objects.is_none());
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

    #[test]
    fn state_closure_skips_gitlink_targets() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let target: GitObjectId = "0303030303030303030303030303030303030303"
            .parse()
            .expect("git oid");
        let root = Tree::from_entries(vec![
            TreeEntry::gitlink("vendor", target).expect("gitlink entry"),
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

        assert_eq!(pairs_from_full(&full), pairs_from_plan(&plan));
        assert!(
            !full.iter().any(|info| info.obj_type == ObjectType::Blob),
            "gitlinks carry foreign Git commit ids, not Heddle blob dependencies: {full:?}"
        );
        assert!(full.iter().any(|info| {
            info.id == ObjectId::Hash(root_hash) && info.obj_type == ObjectType::Tree
        }));
    }

    #[test]
    fn missing_blobs_in_tree_skips_gitlinks_and_walks_nested_side_paths() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let present_blob = repo
            .store()
            .put_blob(&Blob::from("already local"))
            .expect("put present blob");
        let missing_nested = ContentHash::from_bytes([7; 32]);
        let missing_symlink = ContentHash::from_bytes([8; 32]);
        let nested_tree = Tree::from_entries(vec![
            TreeEntry::file("remote.txt", missing_nested, false).unwrap(),
            TreeEntry::symlink("remote-link", missing_symlink).unwrap(),
        ]);
        let nested_tree_hash = repo.store().put_tree(&nested_tree).expect("put nested tree");
        let gitlink_target: GitObjectId = "0404040404040404040404040404040404040404"
            .parse()
            .expect("git oid");
        let root = Tree::from_entries(vec![
            TreeEntry::file("local.txt", present_blob, false).unwrap(),
            TreeEntry::directory("nested", nested_tree_hash).unwrap(),
            TreeEntry::gitlink("vendor", gitlink_target).unwrap(),
        ]);
        let root_hash = repo.store().put_tree(&root).expect("put root tree");

        let missing = missing_blobs_in_tree(repo.store(), root_hash).expect("missing blobs");

        assert_eq!(
            missing.into_iter().collect::<HashSet<_>>(),
            HashSet::from([missing_nested, missing_symlink])
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
            .find(|e| e.name() == "secret.toml")
            .expect("entry present")
            .blob_hash()
            .expect("secret.toml is a blob");

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
    fn enumerate_state_closure_emits_state_metadata_blobs() {
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
        let risk_hash = repo
            .store()
            .put_blob(&Blob::from_slice(b"risk signals"))
            .expect("put risk blob");
        let review_hash = repo
            .store()
            .put_blob(&Blob::from_slice(b"review signatures"))
            .expect("put review blob");
        let conflicts_hash = repo
            .store()
            .put_blob(&Blob::from_slice(b"structured conflicts"))
            .expect("put conflicts blob");
        let state_with_metadata = state
            .with_risk_signals(risk_hash)
            .with_review_signatures(review_hash)
            .with_discussions(discussion_hash)
            .with_structured_conflicts(conflicts_hash);
        repo.store()
            .put_state(&state_with_metadata)
            .expect("put state with metadata");

        let full = enumerate_state_closure_with_options(
            repo.store(),
            state_with_metadata.change_id,
            StateClosureOptions::default(),
        )
        .unwrap();
        let plan = enumerate_state_closure_plan_with_options(
            repo.store(),
            state_with_metadata.change_id,
            StateClosureOptions::default(),
        )
        .unwrap();

        for metadata_hash in [risk_hash, review_hash, discussion_hash, conflicts_hash] {
            assert!(
                full.iter().any(|info| info.obj_type == ObjectType::Blob
                    && info.id == ObjectId::Hash(metadata_hash)),
                "full closure must include state metadata blob {metadata_hash}"
            );
            assert!(
                plan.iter().any(
                    |p| p.obj_type == ObjectType::Blob && p.id == ObjectId::Hash(metadata_hash)
                ),
                "plan closure must include state metadata blob {metadata_hash}"
            );
        }
    }
}
