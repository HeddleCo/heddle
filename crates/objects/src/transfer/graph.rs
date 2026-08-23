// SPDX-License-Identifier: Apache-2.0
use std::collections::{HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::{
    error::{HeddleError, Result},
    object::{
        AnnotatedTag, BindingDelta, ContentHash, RedactionsBlob, ReverseDependencyIndex,
        SemanticEntryKind, SemanticIndexRoot, SemanticTreeNode, State, StateAttachment,
        StateAttachmentBody, StateAttachmentId, StateAttachmentKind, StateId, TreeEntryTarget,
    },
    store::{ObjectStore, SidecarStore, pack::ObjectType as PackObjectType},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectId {
    Hash(ContentHash),
    StateId(StateId),
    StateAttachment {
        state: StateId,
        id: StateAttachmentId,
        /// The attachment's kind, a pure projection of its body
        /// ([`StateAttachmentBody::kind`]). Carried through the wire so
        /// descriptors self-describe their kind; the dedup/identity key is
        /// still `(state, id)`, and kind is coherent under `Eq`/`Hash` because
        /// it is a deterministic function of the same record.
        kind: StateAttachmentKind,
    },
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
    AnnotatedTag,
    /// A `RedactionsBlob` sidecar — the rmp-encoded record(s) declaring
    /// that a specific blob has been redacted by a writer. Keyed on the
    /// wire by `ObjectId::Hash` of
    /// the *redacted blob*, since `Repository`'s sidecar store is
    /// indexed that way.
    Redaction,
    /// An owner-authorized purge sidecar. It carries the same encoded
    /// `RedactionsBlob` as `Redaction`, but is a distinct operation because
    /// receiving it can irreversibly erase blob bytes.
    Purge,
    /// A `StateVisibilityBlob` sidecar — the rmp-encoded record(s)
    /// declaring a non-public audience tier for a specific state. Keyed
    /// on the wire by `ObjectId::StateId` of the state, since the
    /// per-state sidecar store is indexed that way. Like `Redaction`, it
    /// is a sidecar record that lives outside the content-addressed pack
    /// and ships via the per-object transfer path, not the pack.
    StateVisibility,
    StateAttachment,
    /// A content-addressed `KeyBindingRegistry` together with each binding's
    /// revocation/liveness overlay. Hosted materializers append this object to
    /// a closure and carry it out of pack because the native pack format has no
    /// key-binding record kind.
    KeyBinding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectTypeBucket {
    Blob,
    Tree,
    State,
    Action,
    AnnotatedTag,
    Redaction,
    Purge,
    StateVisibility,
    StateAttachment,
    KeyBinding,
}

impl ObjectType {
    pub fn wire_name(self) -> &'static str {
        match self {
            ObjectType::Blob => "blob",
            ObjectType::Tree => "tree",
            ObjectType::State => "state",
            ObjectType::Action => "action",
            ObjectType::AnnotatedTag => "annotated_tag",
            ObjectType::Redaction => "redaction",
            ObjectType::Purge => "purge",
            ObjectType::StateVisibility => "state_visibility",
            ObjectType::StateAttachment => "state_attachment",
            ObjectType::KeyBinding => "key_binding",
        }
    }

    pub fn from_wire(value: &str) -> Result<Self> {
        match value {
            "blob" => Ok(ObjectType::Blob),
            "tree" => Ok(ObjectType::Tree),
            "state" => Ok(ObjectType::State),
            "action" => Ok(ObjectType::Action),
            "annotated_tag" => Ok(ObjectType::AnnotatedTag),
            "redaction" => Ok(ObjectType::Redaction),
            "purge" => Ok(ObjectType::Purge),
            "state_visibility" => Ok(ObjectType::StateVisibility),
            "state_attachment" => Ok(ObjectType::StateAttachment),
            "key_binding" => Ok(ObjectType::KeyBinding),
            _ => Err(HeddleError::InvalidObject(format!(
                "unknown object type: {value}"
            ))),
        }
    }

    /// Whether this object type can ride the content-addressed native pack in
    /// general (the historical, direction-agnostic predicate). Sidecar records
    /// (`Redaction`, `StateVisibility`) live outside `.heddle/objects/` and can
    /// never be packed; everything else can.
    ///
    /// Prefer [`ObjectType::packable_for_push`] /
    /// [`ObjectType::packable_for_pull`] at transfer sites: packability is
    /// direction-dependent for `StateAttachment` (see those methods). This
    /// method retains the pull/general semantics so pack construction,
    /// have-set, and local-copy planners are unchanged.
    pub fn packable(self) -> bool {
        !matches!(
            self,
            ObjectType::Redaction
                | ObjectType::Purge
                | ObjectType::StateVisibility
                | ObjectType::KeyBinding
        )
    }

    /// Whether this object type may ride the client→server **push** pack.
    ///
    /// `StateAttachment` is deliberately excluded on push even though it is a
    /// content-addressed object: a deployed server rejects any pack-carried
    /// attachment as a forgery-prevention measure (weft#549). Pushed
    /// attachments must instead ride the out-of-pack **sidecar** lane (the same
    /// lane as `Redaction`/`StateVisibility`), where the server can verify them
    /// per-kind at finalize while the pack itself stays forgery-sealed. Every
    /// other type keeps its [`ObjectType::packable`] answer.
    pub fn packable_for_push(self) -> bool {
        self.packable() && !matches!(self, ObjectType::StateAttachment)
    }

    /// Whether this object type may ride the server→client **pull/clone** pack.
    ///
    /// Identical to [`ObjectType::packable`]: attachments are carried in the
    /// pull pack exactly as today. The push/pull split exists solely so the
    /// push direction can exclude `StateAttachment` without disturbing the
    /// working pull carriage.
    pub fn packable_for_pull(self) -> bool {
        self.packable()
    }

    pub fn pack_object_type(self) -> Result<PackObjectType> {
        match self {
            ObjectType::Blob => Ok(PackObjectType::Blob),
            ObjectType::Tree => Ok(PackObjectType::Tree),
            ObjectType::State => Ok(PackObjectType::State),
            ObjectType::Action => Ok(PackObjectType::Action),
            ObjectType::AnnotatedTag => Ok(PackObjectType::AnnotatedTag),
            ObjectType::StateAttachment => Ok(PackObjectType::StateAttachment),
            ObjectType::Redaction => Err(HeddleError::InvalidObject(
                "Redaction sidecar records cannot be packed into the content-addressed object pack"
                    .to_string(),
            )),
            ObjectType::Purge => Err(HeddleError::InvalidObject(
                "Purge sidecar records cannot be packed into the content-addressed object pack"
                    .to_string(),
            )),
            ObjectType::StateVisibility => Err(HeddleError::InvalidObject(
                "StateVisibility sidecar records cannot be packed into the content-addressed object pack"
                    .to_string(),
            )),
            ObjectType::KeyBinding => Err(HeddleError::InvalidObject(
                "KeyBinding registry objects cannot be packed into the content-addressed object pack"
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
            ObjectType::AnnotatedTag => ObjectTypeBucket::AnnotatedTag,
            ObjectType::Redaction => ObjectTypeBucket::Redaction,
            ObjectType::Purge => ObjectTypeBucket::Purge,
            ObjectType::StateVisibility => ObjectTypeBucket::StateVisibility,
            ObjectType::StateAttachment => ObjectTypeBucket::StateAttachment,
            ObjectType::KeyBinding => ObjectTypeBucket::KeyBinding,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StateClosureOptions {
    pub depth: Option<u32>,
    pub exclude_states: Vec<StateId>,
}

pub fn enumerate_state_closure(
    store: &(impl ObjectStore + SidecarStore),
    state_id: StateId,
) -> Result<Vec<ObjectInfo>> {
    enumerate_state_closure_with_options(store, state_id, StateClosureOptions::default())
}

pub fn enumerate_state_closure_with_options(
    store: &(impl ObjectStore + SidecarStore),
    state_id: StateId,
    options: StateClosureOptions,
) -> Result<Vec<ObjectInfo>> {
    let mut out = Vec::new();
    walk_state_closure(store, state_id, options, |event| {
        if let Some(info) = object_info_from_event(store, event)? {
            out.push(info);
        }
        Ok(())
    })?;
    for (hash, tag) in annotated_tags_for_state(store, state_id)? {
        out.push(annotated_tag_info(hash, &tag));
    }

    Ok(out)
}

pub fn enumerate_state_closure_plan(
    store: &(impl ObjectStore + SidecarStore),
    state_id: StateId,
) -> Result<Vec<PlannedObject>> {
    enumerate_state_closure_plan_with_options(store, state_id, StateClosureOptions::default())
}

pub fn enumerate_state_closure_plan_with_options(
    store: &(impl ObjectStore + SidecarStore),
    state_id: StateId,
    options: StateClosureOptions,
) -> Result<Vec<PlannedObject>> {
    let mut out = Vec::new();
    walk_state_closure(store, state_id, options, |event| {
        if let Some(object) = planned_object_from_event(store, event)? {
            out.push(object);
        }
        Ok(())
    })?;
    out.extend(
        annotated_tags_for_state(store, state_id)?
            .into_iter()
            .map(|(hash, _)| PlannedObject {
                id: ObjectId::Hash(hash),
                obj_type: ObjectType::AnnotatedTag,
            }),
    );

    Ok(out)
}

pub fn enumerate_state_closure_transfer_with_options(
    store: &(impl ObjectStore + SidecarStore),
    state_id: StateId,
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
    let tags = annotated_tags_for_state(store, state_id)?;
    planned_objects.extend(tags.iter().map(|(hash, _)| PlannedObject {
        id: ObjectId::Hash(*hash),
        obj_type: ObjectType::AnnotatedTag,
    }));
    if full_objects.is_some() && planned_objects.len() > full_descriptor_object_threshold {
        full_objects = None;
    }
    if let Some(objects) = full_objects.as_mut() {
        objects.extend(
            tags.iter()
                .map(|(hash, tag)| annotated_tag_info(*hash, tag)),
        );
    }

    Ok(StateClosureTransferObjects {
        planned_objects,
        full_objects,
    })
}

fn annotated_tags_for_state(
    store: &(impl ObjectStore + SidecarStore),
    state_id: StateId,
) -> Result<Vec<(ContentHash, AnnotatedTag)>> {
    let mut roots = Vec::new();
    for hash in store.list_annotated_tags()? {
        let Some(tag) = store.get_annotated_tag(&hash)? else {
            continue;
        };
        if tag
            .marker()
            .is_some_and(|marker| marker.peeled_state == state_id)
        {
            roots.push((hash, tag));
        }
    }

    let mut tags = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = roots;
    while let Some((hash, tag)) = stack.pop() {
        if !seen.insert(hash) {
            continue;
        }
        if let Some(inner_hash) = tag.target_tag() {
            let inner = store.get_annotated_tag(&inner_hash)?.ok_or_else(|| {
                HeddleError::NotFound(format!(
                    "annotated tag {hash} references missing inner tag {inner_hash}"
                ))
            })?;
            stack.push((inner_hash, inner));
        }
        tags.push((hash, tag));
    }
    Ok(tags)
}

fn annotated_tag_info(hash: ContentHash, tag: &AnnotatedTag) -> ObjectInfo {
    ObjectInfo {
        id: ObjectId::Hash(hash),
        obj_type: ObjectType::AnnotatedTag,
        size: tag.encode_current_msgpack().len() as u64,
        delta_base: None,
    }
}

/// Enumerate a transfer delta while treating `boundary_states` as complete
/// server-held roots. Unlike [`StateClosureOptions::exclude_states`], this does
/// not expand each boundary's tree/history to build a hash exclusion set: the
/// walk stops as soon as it reaches the boundary state. Objects reused by the
/// new tip may still be advertised, and the receiver's have-set filters them.
/// This keeps incremental push planning proportional to the new history.
pub fn enumerate_state_closure_transfer_from_boundaries(
    store: &(impl ObjectStore + SidecarStore),
    state_id: StateId,
    boundary_states: &[StateId],
    full_descriptor_object_threshold: usize,
) -> Result<StateClosureTransferObjects> {
    let mut planned_objects = Vec::new();
    let mut full_objects = Some(Vec::new());
    let excluded_states = boundary_states.iter().copied().collect();

    walk_state_closure_with_exclusions(
        store,
        state_id,
        None,
        excluded_states,
        HashSet::new(),
        |event| {
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
        },
    )?;

    Ok(StateClosureTransferObjects {
        planned_objects,
        full_objects,
    })
}

#[derive(Debug, Clone, Copy)]
enum StateClosureEvent<'a> {
    State {
        id: StateId,
        state: &'a State,
    },
    Tree {
        hash: ContentHash,
        tree: &'a crate::object::Tree,
    },
    Blob {
        hash: ContentHash,
    },
    Redaction {
        blob: ContentHash,
    },
    StateVisibility {
        state: StateId,
    },
    StateAttachment {
        state: StateId,
        attachment: &'a StateAttachment,
    },
    ExcludedState {
        id: StateId,
    },
    ExcludedHash {
        hash: ContentHash,
    },
}

fn walk_state_closure(
    store: &(impl ObjectStore + SidecarStore),
    state_id: StateId,
    options: StateClosureOptions,
    visit: impl for<'event> FnMut(StateClosureEvent<'event>) -> Result<()>,
) -> Result<()> {
    let (excluded_states, excluded_hashes) = collect_excluded(store, &options.exclude_states)?;

    walk_state_closure_with_exclusions(
        store,
        state_id,
        options.depth,
        excluded_states,
        excluded_hashes,
        visit,
    )
}

fn walk_state_closure_with_exclusions(
    store: &(impl ObjectStore + SidecarStore),
    state_id: StateId,
    max_depth: Option<u32>,
    excluded_states: HashSet<StateId>,
    excluded_hashes: HashSet<ContentHash>,
    mut visit: impl for<'event> FnMut(StateClosureEvent<'event>) -> Result<()>,
) -> Result<()> {
    let mut seen_states: HashSet<StateId> = HashSet::new();
    let mut seen_hashes: HashSet<ContentHash> = HashSet::new();
    let mut queue: VecDeque<(StateId, u32)> = VecDeque::new();
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
            .ok_or_else(|| HeddleError::MissingObject {
                object_type: "state".to_string(),
                id: id.to_string(),
            })?;

        visit(StateClosureEvent::State { id, state: &state })?;
        if store.has_state_visibility_for_state(&id)? {
            visit(StateClosureEvent::StateVisibility { state: id })?;
        }
        for attachment in store.list_state_attachments(&id)? {
            visit(StateClosureEvent::StateAttachment {
                state: id,
                attachment: &attachment,
            })?;
            match attachment.body {
                StateAttachmentBody::Context(root) => walk_tree_closure_filtered(
                    store,
                    root,
                    &excluded_hashes,
                    &mut seen_hashes,
                    &mut visit,
                )?,
                StateAttachmentBody::RiskSignals(hash)
                | StateAttachmentBody::ReviewSignatures(hash)
                | StateAttachmentBody::Discussions(hash)
                | StateAttachmentBody::StructuredConflicts(hash) => {
                    walk_blob_filtered(store, hash, &excluded_hashes, &mut seen_hashes, &mut visit)?
                }
                StateAttachmentBody::SemanticIndex(root) => walk_semantic_index_closure(
                    store,
                    root,
                    &excluded_hashes,
                    &mut seen_hashes,
                    &mut visit,
                )?,
                StateAttachmentBody::Signature(_) => {}
            }
        }

        if max_depth.map(|max| depth < max).unwrap_or(true) {
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
    }

    Ok(())
}

fn walk_tree_closure_filtered(
    store: &(impl ObjectStore + SidecarStore),
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
        .ok_or_else(|| HeddleError::MissingObject {
            object_type: "tree".to_string(),
            id: tree_hash.to_hex(),
        })?;

    visit(StateClosureEvent::Tree {
        hash: tree_hash,
        tree: &tree,
    })?;

    for entry in tree.entries() {
        match entry.target() {
            TreeEntryTarget::Blob { hash, .. } | TreeEntryTarget::Symlink { hash } => {
                walk_blob_filtered(store, *hash, excluded, seen, visit)?;
            }
            TreeEntryTarget::Tree { hash } => {
                walk_tree_closure_filtered(store, *hash, excluded, seen, visit)?;
            }
            TreeEntryTarget::Gitlink { .. } => {}
            // Native child-spool edge: its target lives in a separate spool
            // object graph, not this store, so it is not walked here.
            TreeEntryTarget::Spoollink { .. } => {}
        }
    }

    Ok(())
}

fn walk_blob_filtered(
    store: &(impl ObjectStore + SidecarStore),
    blob_hash: ContentHash,
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
    visit(StateClosureEvent::Blob { hash: blob_hash })?;
    if store.has_redactions_for_blob(&blob_hash)? {
        visit(StateClosureEvent::Redaction { blob: blob_hash })?;
    }
    Ok(())
}

/// Walk the merkle semantic-index closure rooted at `root_hash` (a
/// `SemanticIndexRoot` blob), emitting every reachable semantic node blob.
///
/// All semantic-index nodes (root, tree nodes, file nodes) are stored as
/// ordinary content-addressed blobs, so replication just enumerates them.
/// Opaque entries point back at raw source blobs already covered by the state's
/// tree closure, so they are not re-walked here.
///
/// Iterative (explicit stack) so a crafted deep `SemanticTreeNode` chain in a
/// pushed state can't overflow the stack. A missing or undecodable node in the
/// closure is a HARD failure (`ObjectNotFound`/`Serialization`) — a partial or
/// corrupt semantic closure must never be shipped silently.
fn walk_semantic_index_closure(
    store: &(impl ObjectStore + SidecarStore),
    root_hash: ContentHash,
    excluded: &HashSet<ContentHash>,
    seen: &mut HashSet<ContentHash>,
    visit: &mut impl for<'event> FnMut(StateClosureEvent<'event>) -> Result<()>,
) -> Result<()> {
    // Stack of (node_hash, is_tree_node): the root and dir nodes must be decoded
    // to enumerate their children; file/opaque leaves are emitted only.
    let mut stack: Vec<ContentHash> = vec![root_hash];
    while let Some(node_hash) = stack.pop() {
        if !emit_semantic_blob(store, node_hash, excluded, seen, visit)? {
            continue; // excluded (present on the far side) or already seen.
        }
        let blob = store
            .get_blob(&node_hash)?
            .ok_or_else(|| missing_blob(node_hash))?;
        // The root and tree nodes decode to the same `SemanticTreeNode.entries`
        // shape after the root's one indirection; walk uniformly by decoding
        // every interior node as a tree node, tolerating the root shape.
        let node = decode_semantic_container(&blob, node_hash)?;
        for child in node {
            match child {
                SemanticChild::Interior(hash) => stack.push(hash),
                SemanticChild::Leaf(hash) => {
                    // Emit the leaf (file node) blob; it has no children.
                    emit_semantic_blob(store, hash, excluded, seen, visit)?;
                }
                SemanticChild::BindingDelta(hash) => {
                    // Binding deltas are state-scoped attachments. Emit this
                    // state's direct delta; ancestor states contribute their
                    // own deltas when (and only when) the state walk reaches
                    // them. Following `delta.parent` here would cross a
                    // shallow-clone boundary.
                    emit_binding_delta(store, hash, excluded, seen, visit)?;
                }
                SemanticChild::ImporterIndex(hash) => {
                    emit_importer_index(store, hash, excluded, seen, visit)?;
                }
            }
        }
    }
    Ok(())
}

/// Child kinds encountered while walking a state's semantic closure.
enum SemanticChild {
    Interior(ContentHash),
    Leaf(ContentHash),
    BindingDelta(ContentHash),
    ImporterIndex(ContentHash),
}

/// Decode a semantic-closure node — the root (which points at a tree node) or a
/// tree node — into the child hashes to walk. Missing/corrupt is a hard error.
fn decode_semantic_container(
    blob: &crate::object::Blob,
    node_hash: ContentHash,
) -> Result<Vec<SemanticChild>> {
    // Try the root shape first (it has an extra `tree` indirection), then the
    // tree-node shape. Content-addressed hashes make this unambiguous in
    // practice; a blob that decodes as neither is corrupt.
    if let Ok(root) = SemanticIndexRoot::decode(blob.content()) {
        let mut children = vec![SemanticChild::Interior(root.tree)];
        if let Some(binding_delta) = root.binding_delta {
            children.push(SemanticChild::BindingDelta(binding_delta));
        }
        if let Some(importer_index) = root.importer_index {
            children.push(SemanticChild::ImporterIndex(importer_index));
        }
        return Ok(children);
    }
    let node = SemanticTreeNode::decode(blob.content())
        .map_err(|err| HeddleError::Serialization(format!("semantic node {node_hash}: {err}")))?;
    Ok(node
        .entries
        .iter()
        .filter_map(|entry| match entry.kind {
            SemanticEntryKind::Dir => Some(SemanticChild::Interior(entry.node)),
            SemanticEntryKind::File => Some(SemanticChild::Leaf(entry.node)),
            // Opaque `node` is the raw source blob, already in the tree closure.
            SemanticEntryKind::Opaque => None,
        })
        .collect())
}

fn emit_binding_delta(
    store: &(impl ObjectStore + SidecarStore),
    hash: ContentHash,
    excluded: &HashSet<ContentHash>,
    seen: &mut HashSet<ContentHash>,
    visit: &mut impl for<'event> FnMut(StateClosureEvent<'event>) -> Result<()>,
) -> Result<()> {
    if !emit_semantic_blob(store, hash, excluded, seen, visit)? {
        return Ok(());
    }
    let blob = store.get_blob(&hash)?.ok_or_else(|| missing_blob(hash))?;
    BindingDelta::decode(blob.content()).map_err(|err| {
        HeddleError::Serialization(format!("semantic binding delta {hash}: {err}"))
    })?;
    Ok(())
}

fn emit_importer_index(
    store: &(impl ObjectStore + SidecarStore),
    hash: ContentHash,
    excluded: &HashSet<ContentHash>,
    seen: &mut HashSet<ContentHash>,
    visit: &mut impl for<'event> FnMut(StateClosureEvent<'event>) -> Result<()>,
) -> Result<()> {
    if !emit_semantic_blob(store, hash, excluded, seen, visit)? {
        return Ok(());
    }
    let blob = store.get_blob(&hash)?.ok_or_else(|| missing_blob(hash))?;
    ReverseDependencyIndex::decode(blob.content()).map_err(|err| {
        HeddleError::Serialization(format!("semantic reverse-dependency index {hash}: {err}"))
    })?;
    Ok(())
}

/// Emit a semantic node blob as state metadata. Returns `true` when the blob
/// was newly visited (so the caller may descend into it), `false` when it was
/// excluded (present on the far side) or already seen. A blob that is neither
/// excluded nor present is a HARD failure — the closure must be complete.
fn emit_semantic_blob(
    store: &(impl ObjectStore + SidecarStore),
    hash: ContentHash,
    excluded: &HashSet<ContentHash>,
    seen: &mut HashSet<ContentHash>,
    visit: &mut impl for<'event> FnMut(StateClosureEvent<'event>) -> Result<()>,
) -> Result<bool> {
    if excluded.contains(&hash) {
        visit(StateClosureEvent::ExcludedHash { hash })?;
        return Ok(false);
    }
    if !seen.insert(hash) {
        return Ok(false);
    }
    if store.get_blob(&hash)?.is_none() {
        return Err(missing_blob(hash));
    }
    visit(StateClosureEvent::Blob { hash })?;
    Ok(true)
}

/// Collect every semantic-index node blob hash reachable from `root_hash` into
/// `excluded`, for the have-set computation. Iterative + tolerant (a broken
/// have-set index is not fatal — it just means fewer objects are marked
/// already-present, which is safe over-fetching, not corruption).
fn collect_semantic_hashes(
    store: &(impl ObjectStore + SidecarStore),
    root_hash: ContentHash,
    excluded: &mut HashSet<ContentHash>,
) -> Result<()> {
    let mut stack: Vec<ContentHash> = vec![root_hash];
    while let Some(node_hash) = stack.pop() {
        if !excluded.insert(node_hash) {
            continue;
        }
        let Some(blob) = store.get_blob(&node_hash)? else {
            continue;
        };
        let children = match decode_semantic_container(&blob, node_hash) {
            Ok(children) => children,
            Err(_) => continue,
        };
        for child in children {
            match child {
                SemanticChild::Interior(hash) => stack.push(hash),
                SemanticChild::Leaf(hash) => {
                    excluded.insert(hash);
                }
                SemanticChild::BindingDelta(hash) | SemanticChild::ImporterIndex(hash) => {
                    excluded.insert(hash);
                }
            }
        }
    }
    Ok(())
}

fn object_info_from_event(
    store: &(impl ObjectStore + SidecarStore),
    event: StateClosureEvent<'_>,
) -> Result<Option<ObjectInfo>> {
    match event {
        StateClosureEvent::State { id, state } => {
            let state_bytes = rmp_serde::to_vec_named(state)?;
            Ok(Some(ObjectInfo {
                id: ObjectId::StateId(id),
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
            let Some(blob) = store.get_blob(&hash)? else {
                if blob_has_purge_evidence(store, &hash)? {
                    return Ok(None);
                }
                return Err(missing_blob(hash));
            };
            Ok(Some(ObjectInfo {
                id: ObjectId::Hash(hash),
                obj_type: ObjectType::Blob,
                size: blob.size() as u64,
                delta_base: None,
            }))
        }
        StateClosureEvent::Redaction { blob } => Ok(store
            .get_redactions_bytes_for_blob(&blob)?
            .map(|bytes| ObjectInfo {
                id: ObjectId::Hash(blob),
                obj_type: ObjectType::Redaction,
                size: bytes.len() as u64,
                delta_base: None,
            })),
        StateClosureEvent::StateVisibility { state } => Ok(store
            .get_state_visibility_bytes_for_state(&state)?
            .map(|bytes| ObjectInfo {
                id: ObjectId::StateId(state),
                obj_type: ObjectType::StateVisibility,
                size: bytes.len() as u64,
                delta_base: None,
            })),
        StateClosureEvent::StateAttachment { state, attachment } => {
            let bytes = rmp_serde::to_vec_named(attachment)?;
            Ok(Some(ObjectInfo {
                id: ObjectId::StateAttachment {
                    state,
                    id: attachment.id(),
                    kind: attachment.body.kind(),
                },
                obj_type: ObjectType::StateAttachment,
                size: bytes.len() as u64,
                delta_base: None,
            }))
        }
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
    store: &(impl ObjectStore + SidecarStore),
    event: StateClosureEvent<'_>,
) -> Result<Option<PlannedObject>> {
    match event {
        StateClosureEvent::State { id, .. } => Ok(Some(PlannedObject {
            id: ObjectId::StateId(id),
            obj_type: ObjectType::State,
        })),
        StateClosureEvent::Tree { hash, .. } => Ok(Some(PlannedObject {
            id: ObjectId::Hash(hash),
            obj_type: ObjectType::Tree,
        })),
        StateClosureEvent::Blob { hash, .. } => {
            if store.get_blob(&hash)?.is_none() {
                if blob_has_purge_evidence(store, &hash)? {
                    return Ok(None);
                }
                return Err(missing_blob(hash));
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
            id: ObjectId::StateId(state),
            obj_type: ObjectType::StateVisibility,
        })),
        StateClosureEvent::StateAttachment { state, attachment } => Ok(Some(PlannedObject {
            id: ObjectId::StateAttachment {
                state,
                id: attachment.id(),
                kind: attachment.body.kind(),
            },
            obj_type: ObjectType::StateAttachment,
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

/// A purged blob is intentionally absent from the object closure; its sidecar
/// remains and is verified by the receiver before the absence is accepted.
/// Mere redaction never excuses a missing blob.
fn missing_blob(hash: ContentHash) -> HeddleError {
    HeddleError::MissingObject {
        object_type: "blob".to_string(),
        id: hash.to_hex(),
    }
}

fn blob_has_purge_evidence(
    store: &(impl ObjectStore + SidecarStore),
    hash: &ContentHash,
) -> Result<bool> {
    let Some(bytes) = store.get_redactions_bytes_for_blob(hash)? else {
        return Ok(false);
    };
    let redactions = RedactionsBlob::decode(&bytes).map_err(|error| {
        HeddleError::InvalidObject(format!(
            "invalid redaction sidecar for missing blob {}: {error}",
            hash.to_hex()
        ))
    })?;
    Ok(redactions
        .redactions
        .iter()
        .any(|redaction| redaction.redacted_blob == *hash && redaction.is_purged()))
}

pub fn missing_blobs_in_tree(
    store: &(impl ObjectStore + SidecarStore),
    tree_hash: ContentHash,
) -> Result<Vec<ContentHash>> {
    let mut missing = Vec::new();
    collect_missing_blobs_recursive(store, &tree_hash, &mut missing)?;
    Ok(missing)
}

fn collect_missing_blobs_recursive(
    store: &(impl ObjectStore + SidecarStore),
    tree_hash: &ContentHash,
    missing: &mut Vec<ContentHash>,
) -> Result<()> {
    let Some(tree) = store.get_tree(tree_hash).map_err(|err| {
        HeddleError::InvalidObject(format!(
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
                    HeddleError::InvalidObject(format!(
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
            // Native child-spool edge: its target lives in a separate spool
            // object graph, not this store, so it is not walked here.
            TreeEntryTarget::Spoollink { .. } => {}
        }
    }
    Ok(())
}

fn collect_excluded(
    store: &(impl ObjectStore + SidecarStore),
    roots: &[StateId],
) -> Result<(HashSet<StateId>, HashSet<ContentHash>)> {
    if roots.is_empty() {
        return Ok((HashSet::new(), HashSet::new()));
    }

    let mut excluded_states: HashSet<StateId> = HashSet::new();
    let mut excluded_hashes: HashSet<ContentHash> = HashSet::new();
    let mut queue: VecDeque<StateId> = VecDeque::new();

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
        for attachment in store.list_state_attachments(&id)? {
            match attachment.body {
                StateAttachmentBody::Context(root) => {
                    collect_tree_hashes(store, root, &mut excluded_hashes)?
                }
                StateAttachmentBody::RiskSignals(hash)
                | StateAttachmentBody::ReviewSignatures(hash)
                | StateAttachmentBody::Discussions(hash)
                | StateAttachmentBody::StructuredConflicts(hash) => {
                    excluded_hashes.insert(hash);
                }
                StateAttachmentBody::SemanticIndex(root) => {
                    collect_semantic_hashes(store, root, &mut excluded_hashes)?;
                }
                StateAttachmentBody::Signature(_) => {}
            }
        }
    }

    Ok((excluded_states, excluded_hashes))
}

fn collect_tree_hashes(
    store: &(impl ObjectStore + SidecarStore),
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
            // Native child-spool edge: its target lives in a separate spool
            // object graph, not this store, so it is not walked here.
            TreeEntryTarget::Spoollink { .. } => {}
        }
    }

    Ok(())
}

pub fn is_ancestor(
    store: &(impl ObjectStore + SidecarStore),
    ancestor: StateId,
    descendant: StateId,
) -> Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }

    let mut seen: HashSet<StateId> = HashSet::new();
    let mut queue: VecDeque<StateId> = VecDeque::new();
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
