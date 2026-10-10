// SPDX-License-Identifier: Apache-2.0
//! Exact receiver-side Git projection from complete, independently validated
//! native source content. No repository initialization, identity creation, native
//! admission, shared Git mirror, or current authority is inferred here.
use crate::{
    GitProjection, GitProjectionError, GitProjectionResult, SyncMapping, gateway_view::ViewLimits,
};
use crypto::thread_operation::SignedOperation;
use objects::object::{AudienceTier, ContentHash, ObjectSource, StateId, TreeEntryTarget, visible};
use sley::{CommitObject, EntryKind, GitObjectType, ObjectId, Repository as GitRepository};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

pub const RECEIVED_GIT_RECIPE: &str = "heddle-git-history-v1-no-hosted-url";
fn fail(error: impl std::fmt::Display) -> GitProjectionError {
    GitProjectionError::Git(error.to_string())
}

/// Byte/graph proof only. The receiver must separately verify current author,
/// disclosure, retention and native head CAS under its acceptance transaction.
pub struct ReceiverGitHistory {
    mapping: SyncMapping,
    order: Vec<StateId>,
    original_ids: Vec<ContentHash>,
    blob_ids: Vec<ContentHash>,
}
impl ReceiverGitHistory {
    pub fn mapping(&self) -> &SyncMapping {
        &self.mapping
    }
    pub fn states(&self) -> &[StateId] {
        &self.order
    }
    pub fn original_ids(&self) -> &[ContentHash] {
        &self.original_ids
    }
    pub fn blob_ids(&self) -> &[ContentHash] {
        &self.blob_ids
    }
    pub fn git_oid(&self, state: &StateId) -> Option<ObjectId> {
        self.mapping.get_git(state)
    }
}

/// A disposable private Git object database containing only the selected closure.
/// Hold/recheck the real disclosure fence before giving these bytes to Git clients.
pub struct ReceivedGitProjection {
    directory: tempfile::TempDir,
    history: ReceiverGitHistory,
}
impl ReceivedGitProjection {
    pub fn git_dir(&self) -> &std::path::Path {
        self.directory.path()
    }
    pub fn history(&self) -> &ReceiverGitHistory {
        &self.history
    }
}

/// Verify the complete final Git graph and both authoritative CAS identities.
/// `source` must hydrate ALL historical trees/blobs: a selected-tip SourcePack
/// alone does not do that. No claimed OID or State fidelity flag is trusted.
#[allow(clippy::too_many_arguments)]
pub fn verify_received_git_history(
    source: &impl ObjectSource,
    originals: &[SignedOperation],
    expected: StateId,
    accepted: StateId,
    expected_git: &[u8; 20],
    accepted_git: &[u8; 20],
    limits: ViewLimits,
) -> GitProjectionResult<ReceiverGitHistory> {
    let projected = project_received_git_history(source, originals, accepted, limits)?;
    if expected == accepted
        || projected
            .history
            .git_oid(&expected)
            .is_none_or(|oid| oid.as_bytes() != expected_git)
        || projected
            .history
            .git_oid(&accepted)
            .is_none_or(|oid| oid.as_bytes() != accepted_git)
    {
        return Err(fail(
            "receiver Git old/new identity or fast-forward mismatch",
        ));
    }
    Ok(projected.history)
}

/// Reconstruct the complete bounded graph with the fixed hosted Git recipe.
/// Imported commits retain exact captured bytes; native commits use the existing
/// canonical footer without a machine-configured hosted URL or identity fallback.
/// The caller must validate all current source constraints; structural success is
/// never an access grant. Failure drops the entire private object database.
pub fn project_received_git_history(
    source: &impl ObjectSource,
    originals: &[SignedOperation],
    tip: StateId,
    limits: ViewLimits,
) -> GitProjectionResult<ReceivedGitProjection> {
    if originals.is_empty()
        || originals.len() > 4096
        || limits.states == 0
        || limits.states > 128
        || limits.entries == 0
        || limits.entries > 10_000
        || limits.bytes == 0
        || limits.bytes > 64 * 1024 * 1024
        || limits.blob_bytes == 0
        || limits.blob_bytes > 16 * 1024 * 1024
    {
        return Err(fail("receiver bounded complete history required"));
    }
    let mut signed_states: HashMap<StateId, Vec<u8>> = HashMap::new();
    let mut original_ids = BTreeSet::new();
    let mut metadata = 0usize;
    for signed in originals {
        metadata = metadata
            .checked_add(signed.canonical.len() + signed.signature.len())
            .ok_or_else(|| fail("original bytes overflow"))?;
        if metadata > 16 * 1024 * 1024 {
            return Err(fail("receiver original metadata limit"));
        }
        let operation = signed.verify().map_err(fail)?;
        let state = operation
            .source_state()?
            .ok_or_else(|| fail("receiver non-source original"))?;
        let result = operation
            .source_result()?
            .ok_or_else(|| fail("receiver source result absent"))?;
        if let Some(privacy) = &result.visibility
            && (privacy
                .state
                .as_ref()
                .is_some_and(|tier| !visible(tier, &AudienceTier::Public))
                || privacy
                    .entry_sidecar(&state)
                    .map_err(fail)?
                    .is_some_and(|entries| {
                        entries
                            .entries
                            .iter()
                            .any(|entry| !visible(&entry.tier, &AudienceTier::Public))
                    }))
        {
            return Err(fail("original visibility withholds receiver Git history"));
        }
        let bytes = state.encode_current_msgpack()?;
        if signed_states
            .insert(state.id(), bytes.clone())
            .is_some_and(|prior| prior != bytes)
        {
            return Err(fail("conflicting signed canonical State bytes"));
        }
        original_ids.insert(operation.id()?);
    }
    let seed = objects::object::thread_replication::initial_base::synthetic_initial_base()?;
    let mut states = BTreeMap::new();
    let mut order = Vec::new();
    let mut active = HashSet::new();
    let mut done = HashSet::new();
    let mut pending = vec![(tip, false)];
    while let Some((id, finish)) = pending.pop() {
        if pending.len() > 4096 {
            return Err(fail("receiver history edge limit"));
        }
        if finish {
            active.remove(&id);
            done.insert(id);
            order.push(id);
            continue;
        }
        if done.contains(&id) {
            continue;
        }
        if !active.insert(id) {
            return Err(fail("receiver cyclic source graph"));
        }
        if states.len() >= limits.states {
            return Err(fail("receiver state limit"));
        }
        // SourcePack carries real selected States, not the schema-defined
        // initialization sentinel. Derive only that exact canonical sentinel;
        // any supplied noncanonical replacement is still rejected below.
        let state = match source.get_state(&id)? {
            Some(state) => state,
            None if id == seed.id() => seed.clone(),
            None => return Err(GitProjectionError::StateNotFound(id)),
        };
        if state.id() != id {
            return Err(fail("receiver State content identity mismatch"));
        }
        let canonical = state.encode_current_msgpack()?;
        if id == seed.id() {
            if canonical != seed.encode_current_msgpack()? {
                return Err(fail("receiver initialization differs"));
            }
            active.remove(&id);
            done.insert(id);
            states.insert(id, state);
            continue;
        }
        if signed_states.get(&id) != Some(&canonical) {
            return Err(fail("receiver State lacks exact signed original"));
        }
        if state.git_lossy {
            return Err(fail("receiver lossy history refused"));
        }
        pending.push((id, true));
        for parent in state.parents.iter().rev() {
            pending.push((*parent, false));
        }
        states.insert(id, state);
    }
    if signed_states.keys().any(|id| !states.contains_key(id)) {
        return Err(fail("receiver unrelated source originals"));
    }
    let directory = tempfile::tempdir()?;
    let sink = GitRepository::init_bare(directory.path()).map_err(fail)?;
    let mut mapping = SyncMapping::new();
    let mut budget = limits;
    let mut blob_ids = BTreeSet::new();
    for id in &order {
        let state = states
            .get(id)
            .ok_or_else(|| fail("receiver ordered State missing"))?;
        let tree = write_tree(source, &sink, state.tree, 0, &mut budget, &mut blob_ids)?;
        let parents = crate::git_reconstruct::mapped_git_parents(state, &mapping)?;
        let body = if state.raw_message.is_some() {
            crate::git_reconstruct::build_commit_content(state, &tree, &parents)?
        } else {
            if crate::git_core::principal_lacks_identity(&state.attribution.principal) {
                return Err(fail("receiver accountable native identity required"));
            }
            let signature = crate::git_export::state_to_signature(state);
            CommitObject {
                tree,
                parents,
                author: signature.to_ident_bytes(),
                committer: signature.to_ident_bytes(),
                encoding: None,
                message: GitProjection::build_commit_message_with_footer(state, None, 0)
                    .into_bytes(),
            }
            .write()
        };
        let oid = sink
            .write_raw_object(GitObjectType::Commit, body)
            .map_err(fail)?;
        mapping.insert_checked(*id, oid)?;
    }
    Ok(ReceivedGitProjection {
        directory,
        history: ReceiverGitHistory {
            mapping,
            order,
            original_ids: original_ids.into_iter().collect(),
            blob_ids: blob_ids.into_iter().collect(),
        },
    })
}

fn write_tree(
    source: &impl ObjectSource,
    sink: &GitRepository,
    hash: ContentHash,
    depth: usize,
    limits: &mut ViewLimits,
    blobs: &mut BTreeSet<ContentHash>,
) -> GitProjectionResult<ObjectId> {
    if depth > 64 {
        return Err(fail("receiver tree depth limit"));
    }
    let tree = source
        .get_tree(&hash)?
        .ok_or_else(|| fail("receiver historical tree missing"))?;
    if tree.hash() != hash {
        return Err(fail("receiver tree content identity mismatch"));
    }
    let mut entries = Vec::new();
    for entry in tree.git_ordered_entries() {
        limits.entries = limits
            .entries
            .checked_sub(1)
            .ok_or_else(|| fail("receiver entry limit"))?;
        let (kind, oid) = match entry.target() {
            TreeEntryTarget::Tree { hash } => (
                EntryKind::Tree,
                write_tree(source, sink, *hash, depth + 1, limits, blobs)?,
            ),
            TreeEntryTarget::Blob { hash, .. } | TreeEntryTarget::Symlink { hash } => {
                let blob = source
                    .get_blob(hash)?
                    .ok_or_else(|| fail("receiver historical blob missing"))?;
                if blob.hash() != *hash {
                    return Err(fail("receiver blob content identity mismatch"));
                }
                let len = blob.content().len();
                if len > limits.blob_bytes {
                    return Err(fail("receiver blob limit"));
                }
                limits.bytes = limits
                    .bytes
                    .checked_sub(len)
                    .ok_or_else(|| fail("receiver complete history byte limit"))?;
                blobs.insert(*hash);
                let kind = match entry.target() {
                    TreeEntryTarget::Symlink { .. } => EntryKind::Symlink,
                    TreeEntryTarget::Blob {
                        executable: true, ..
                    } => EntryKind::BlobExecutable,
                    _ => EntryKind::Blob,
                };
                (kind, sink.write_blob(blob.content()).map_err(fail)?)
            }
            _ => return Err(fail("receiver nested repository edge unsupported")),
        };
        entries.push((entry, kind, oid));
    }
    if tree.has_git_layout() {
        let mut body = Vec::new();
        for (entry, _, oid) in entries {
            entry
                .git_mode()
                .ok_or_else(|| fail("receiver preserved Git mode missing"))?
                .write_digits(&mut body);
            body.push(b' ');
            body.extend_from_slice(entry.name().as_bytes());
            body.push(0);
            body.extend_from_slice(oid.as_bytes());
        }
        sink.write_raw_object(GitObjectType::Tree, body)
            .map_err(fail)
    } else {
        let mut editor = sink
            .edit_tree(&ObjectId::empty_tree(sink.object_format()))
            .map_err(fail)?;
        for (entry, kind, oid) in entries {
            editor.upsert(entry.name(), kind, oid);
        }
        sink.write_tree(editor).map_err(fail)
    }
}
