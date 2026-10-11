// SPDX-License-Identifier: Apache-2.0
use std::{collections::HashMap, future::Future};

use heddle_object_model::object::{
    Blob, ContentHash, State, StateId, Tree, TreeEntryTarget,
    thread_replication::initial_base::synthetic_initial_base,
};

use crate::{
    GitObjectKind, ObjectFormat, ObjectId, SerializeError, object_id, serialize_commit,
    serialize_tree,
};

/// A native object to reconstruct; Gitlinks are external references, not roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeObjectId {
    Commit(StateId),
    Tree(ContentHash),
    Blob(ContentHash),
}

/// Store-independent reads. Implementations may borrow an async database client.
///
/// Reads must be stable for the duration of reconstruction, return the requested
/// content-addressed object, and enforce `max_bytes` on the decoded object's
/// memory before allocating it.
/// Each distinct object is read once and retained until its children resolve.
/// The caller owns authorization, redaction policy and missing-object errors.
pub trait ObjectReader {
    type Error;

    fn read_state(
        &self,
        id: StateId,
        max_bytes: usize,
    ) -> impl Future<Output = std::result::Result<State, Self::Error>> + Send;

    fn read_tree(
        &self,
        id: ContentHash,
        max_bytes: usize,
    ) -> impl Future<Output = std::result::Result<Tree, Self::Error>> + Send;

    fn read_blob(
        &self,
        id: ContentHash,
        max_bytes: usize,
    ) -> impl Future<Output = std::result::Result<Blob, Self::Error>> + Send;
}

/// Stream reconstructed content to a pack writer, verifier or object database.
///
/// Children arrive before their containing tree/commit, once per native ID.
/// The sink must consume or persist each object before returning to keep memory bounded. An error or
/// cancellation can leave a partial closure; the caller owns transactionality.
pub trait ObjectSink {
    type Error;

    /// Return an already reconstructed ID from the caller's object table.
    /// The binding must be exact, authorized, and in the requested Git format.
    /// A hit skips this object and its entire dependency closure.
    fn resolved(&self, _id: NativeObjectId) -> Option<ObjectId> {
        None
    }

    fn write_object(
        &mut self,
        native: NativeObjectId,
        kind: GitObjectKind,
        id: ObjectId,
        content: &[u8],
    ) -> impl Future<Output = std::result::Result<(), Self::Error>> + Send;
}

/// Hard traversal limits, checked before reads and output allocation.
///
/// The native-to-Git memo (including active objects) has at most `max_objects`
/// entries. At most `max_depth` native objects are retained, each reader-bounded
/// by `max_object_bytes`; dependency/resolved IDs have `max_pending_links` slots.
/// `max_total_bytes` bounds total serialized output, reserving tree/commit
/// output before descending. Commit reservations use a conservative upper bound.
/// Reader and sink allocations remain their contracts.
#[derive(Clone, Copy, Debug)]
pub struct ReconstructionLimits {
    pub max_depth: usize,
    pub max_pending_links: usize,
    /// Reader memory limit and serialized output limit. Commit admission uses
    /// a conservative upper bound including identity/header formatting.
    pub max_object_bytes: usize,
    /// Distinct native objects, including memo entries and sink cache hits.
    pub max_objects: usize,
    /// Total serialized content bytes, excluding Git framing.
    pub max_total_bytes: usize,
}

impl Default for ReconstructionLimits {
    fn default() -> Self {
        Self {
            max_depth: 4096,
            max_pending_links: 1_000_000,
            max_object_bytes: 64 * 1024 * 1024,
            max_objects: 10_000_000,
            max_total_bytes: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReconstructionError<R, W> {
    #[error("native object read failed: {0}")]
    Reader(R),
    #[error("Git object sink failed: {0}")]
    Sink(W),
    #[error(transparent)]
    Serialize(#[from] SerializeError),
    #[error("Git reconstruction limit exceeded: {0}")]
    LimitExceeded(&'static str),
}

enum NativeObject {
    Commit(Box<State>),
    Tree(Tree),
    Blob(Blob),
}

struct Frame {
    id: NativeObjectId,
    object: NativeObject,
    byte_bound: usize,
    dependencies: Vec<NativeObjectId>,
    resolved: Vec<ObjectId>,
}

/// Rebuild a tree/blob closure or a commit's full history closure without an engine.
///
/// The walk is iterative, including commit parents. It never follows gitlinks,
/// memoizes native IDs without retaining emitted bytes, and fails on
/// lossy commits and spoollinks rather than claiming exact reconstruction.
/// Native states without a raw message serialize an empty Git message; use the
/// pure serializer when a caller supplies different parent or message policy.
/// The deterministic hosted seed is omitted from parent lists, as in local
/// projection: it represents initialization and has no Git commit.
pub async fn reconstruct_closure<R: ObjectReader + Sync, W: ObjectSink + Send>(
    reader: &R,
    sink: &mut W,
    root: NativeObjectId,
    format: ObjectFormat,
    limits: ReconstructionLimits,
) -> std::result::Result<ObjectId, ReconstructionError<R::Error, W::Error>> {
    let seed = synthetic_initial_base()
        .map_err(|error| SerializeError::InvalidObject(error.to_string()))?
        .id();
    if root == NativeObjectId::Commit(seed) {
        return Err(SerializeError::InvalidObject("hosted seed has no Git commit".into()).into());
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut pending_links = 0usize;
    let mut memo: HashMap<NativeObjectId, Option<ObjectId>> = HashMap::new();
    let mut total_bytes = 0usize;
    let mut next = Some(root);

    loop {
        if let Some(id) = next.take() {
            if let Some(cached) = memo.get(&id) {
                let oid = cached
                    .ok_or_else(|| SerializeError::InvalidObject("native object cycle".into()))?;
                if let Some(parent) = stack.last_mut() {
                    parent.resolved.push(oid);
                    continue;
                }
                return Ok(oid);
            }
            if memo.len() >= limits.max_objects {
                return Err(ReconstructionError::LimitExceeded("objects"));
            }
            if let Some(oid) = sink.resolved(id) {
                if oid.format() != format {
                    return Err(SerializeError::InvalidObject(
                        "sink resolved ID uses a different Git object format".into(),
                    )
                    .into());
                }
                memo.insert(id, Some(oid));
                if let Some(parent) = stack.last_mut() {
                    parent.resolved.push(oid);
                    continue;
                }
                return Ok(oid);
            }
            if stack.len() >= limits.max_depth {
                return Err(ReconstructionError::LimitExceeded("depth"));
            }
            memo.insert(id, None);
            let remaining_bytes = limits.max_total_bytes.saturating_sub(total_bytes);
            let read_limit = limits.max_object_bytes.min(remaining_bytes);
            let available = limits.max_pending_links.saturating_sub(pending_links);
            let (object, dependencies, byte_bound) = match id {
                NativeObjectId::Commit(id) => {
                    let state = reader
                        .read_state(id, read_limit)
                        .await
                        .map_err(ReconstructionError::Reader)?;
                    if state.git_lossy {
                        return Err(
                            SerializeError::InvalidObject("lossy imported commit".into()).into(),
                        );
                    }
                    let parents = state.parents.iter().copied().filter(|id| *id != seed);
                    if parents.clone().count().saturating_add(1) > available {
                        return Err(ReconstructionError::LimitExceeded("pending links"));
                    }
                    let dependencies: Vec<_> = std::iter::once(NativeObjectId::Tree(state.tree))
                        .chain(parents.map(NativeObjectId::Commit))
                        .collect();
                    let bytes = commit_byte_bound(&state, format, dependencies.len() - 1);
                    (NativeObject::Commit(Box::new(state)), dependencies, bytes)
                }
                NativeObjectId::Tree(id) => {
                    let tree = reader
                        .read_tree(id, read_limit)
                        .await
                        .map_err(ReconstructionError::Reader)?;
                    if tree.entries().len() > available {
                        return Err(ReconstructionError::LimitExceeded("pending links"));
                    }
                    let mut dependencies = Vec::new();
                    for entry in tree.entries() {
                        match entry.target() {
                            TreeEntryTarget::Tree { hash } => {
                                dependencies.push(NativeObjectId::Tree(*hash))
                            }
                            TreeEntryTarget::Blob { hash, .. }
                            | TreeEntryTarget::Symlink { hash } => {
                                dependencies.push(NativeObjectId::Blob(*hash))
                            }
                            TreeEntryTarget::Gitlink { .. } => {}
                            TreeEntryTarget::Spoollink { .. } => {
                                return Err(SerializeError::InvalidObject(
                                    "spoollink has no Git representation".into(),
                                )
                                .into());
                            }
                        }
                    }
                    let bytes = tree_byte_bound(&tree, format);
                    (NativeObject::Tree(tree), dependencies, bytes)
                }
                NativeObjectId::Blob(id) => {
                    let blob = reader
                        .read_blob(id, read_limit)
                        .await
                        .map_err(ReconstructionError::Reader)?;
                    let bytes = blob.size();
                    (NativeObject::Blob(blob), Vec::new(), bytes)
                }
            };
            if byte_bound > limits.max_object_bytes {
                return Err(ReconstructionError::LimitExceeded("object bytes"));
            }
            if byte_bound > remaining_bytes {
                return Err(ReconstructionError::LimitExceeded("total bytes"));
            }
            total_bytes += byte_bound;
            pending_links += dependencies.len();
            stack.push(Frame {
                id,
                object,
                byte_bound,
                resolved: Vec::with_capacity(dependencies.len()),
                dependencies,
            });
        }

        let Some(frame) = stack.last() else {
            return Err(SerializeError::InvalidObject("empty reconstruction stack".into()).into());
        };
        if let Some(child) = frame.dependencies.get(frame.resolved.len()).copied() {
            next = Some(child);
            continue;
        }
        let Some(frame) = stack.pop() else {
            return Err(SerializeError::InvalidObject("empty reconstruction stack".into()).into());
        };
        pending_links -= frame.dependencies.len();
        let (kind, body) = match frame.object {
            NativeObject::Blob(blob) => (GitObjectKind::Blob, blob.into_content()),
            NativeObject::Commit(state) => {
                let Some(tree) = frame.resolved.first() else {
                    return Err(SerializeError::InvalidObject(
                        "commit has no resolved tree".into(),
                    )
                    .into());
                };
                (
                    GitObjectKind::Commit,
                    serialize_commit(state.as_ref(), tree, &frame.resolved[1..])?,
                )
            }
            NativeObject::Tree(tree) => {
                // Dependency order is native name order. Resolve by that order,
                // while the serializer owns the independently preserved Git order.
                let mut resolved = frame.resolved.iter();
                let ids: Vec<_> = tree
                    .entries()
                    .iter()
                    .map(|entry| match entry.target() {
                        TreeEntryTarget::Gitlink { target } => Some(*target),
                        _ => resolved.next().copied(),
                    })
                    .collect();
                let body = serialize_tree(&tree, format, |entry| {
                    let index = tree
                        .entries()
                        .binary_search_by(|candidate| candidate.name().cmp(entry.name()))
                        .map_err(|_| {
                            SerializeError::InvalidObject("tree entry disappeared".into())
                        })?;
                    ids[index].ok_or_else(|| {
                        SerializeError::InvalidObject("tree dependency disappeared".into())
                    })
                })?;
                (GitObjectKind::Tree, body)
            }
        };
        if body.len() > limits.max_object_bytes {
            return Err(ReconstructionError::LimitExceeded("object bytes"));
        }
        let oid = object_id(format, kind, &body)?;
        let unused_bytes = frame
            .byte_bound
            .checked_sub(body.len())
            .ok_or(ReconstructionError::LimitExceeded("total bytes"))?;
        total_bytes -= unused_bytes;
        sink.write_object(frame.id, kind, oid, &body)
            .await
            .map_err(ReconstructionError::Sink)?;
        memo.insert(frame.id, Some(oid));
        if let Some(parent) = stack.last_mut() {
            parent.resolved.push(oid);
        } else {
            return Ok(oid);
        }
    }
}

fn commit_byte_bound(state: &State, format: ObjectFormat, parents: usize) -> usize {
    // 128 covers both actor labels, separators, decimal i64 timestamps and
    // even the longest i32 timezone spelling, plus tree/body separators.
    let author = &state.attribution.principal;
    let committer = state.committer.as_ref().unwrap_or(author);
    let mut bytes = 128usize
        .saturating_add(format.hex_len())
        .saturating_add(parents.saturating_mul(8 + format.hex_len()))
        .saturating_add(author.name.len())
        .saturating_add(author.email.len())
        .saturating_add(committer.name.len())
        .saturating_add(committer.email.len())
        .saturating_add(state.raw_message.as_ref().map_or(0, Vec::len));
    for (name, value) in &state.extra_headers {
        bytes = bytes
            .saturating_add(name.len())
            .saturating_add(value.len())
            .saturating_add(value.iter().filter(|byte| **byte == b'\n').count())
            .saturating_add(2);
    }
    bytes
}

fn tree_byte_bound(tree: &Tree, format: ObjectFormat) -> usize {
    let mut digits = Vec::new();
    tree.entries().iter().fold(0usize, |bytes, entry| {
        let Some(mode) = entry.git_mode() else {
            return bytes;
        };
        digits.clear();
        mode.write_digits(&mut digits);
        bytes
            .saturating_add(digits.len())
            .saturating_add(entry.name().len())
            .saturating_add(2 + format.raw_len())
    })
}
