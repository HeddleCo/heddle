// SPDX-License-Identifier: Apache-2.0
use std::future::Future;

use heddle_object_model::object::{
    Blob, ContentHash, State, StateId, Tree, TreeEntryTarget,
    thread_replication::initial_base::synthetic_initial_base,
};

use crate::{ObjectFormat, ObjectId, SerializeError, object_id, serialize_commit, serialize_tree};

/// A native object to reconstruct; Gitlinks are external references, not roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeObjectId {
    Commit(StateId),
    Tree(ContentHash),
    Blob(ContentHash),
}

/// Store-independent reads. Implementations may borrow an async database client.
///
/// Reads must be stable for the duration of reconstruction, return the requested
/// content-addressed object, and enforce `max_bytes` on the decoded object's
/// memory before allocating it. The traversal retains no returned native object
/// across child reads. Non-leaf objects are read twice, once for dependencies
/// and once for serialization, to avoid keeping native objects on the walk stack.
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
/// Children arrive before their containing tree/commit. Shared objects may be
/// emitted more than once; use the Git ID for idempotence. The sink must consume
/// or persist each object before returning to keep memory bounded. An error or
/// cancellation can leave a partial closure; the caller owns transactionality.
pub trait ObjectSink {
    type Error;

    fn write_object(
        &mut self,
        kind: &'static str,
        id: ObjectId,
        content: &[u8],
    ) -> impl Future<Output = std::result::Result<(), Self::Error>> + Send;
}

/// Hard traversal limits. No cache grows with the total closure size.
///
/// Memory is bounded by `max_depth` fixed-size frames, `max_pending_links`
/// dependency IDs plus resolved IDs, and one reader object plus its serialized
/// content (each limited by `max_object_bytes`). Tree sorting uses scratch
/// proportional to that one tree. Reader and sink allocations are their contracts.
#[derive(Clone, Copy, Debug)]
pub struct ReconstructionLimits {
    pub max_depth: usize,
    pub max_pending_links: usize,
    /// Reader memory limit and serialized output limit. Commit admission uses
    /// a conservative upper bound including identity/header formatting.
    pub max_object_bytes: usize,
    /// Includes repeated shared subgraphs; also bounds work on invalid cycles.
    pub max_objects: usize,
}

impl Default for ReconstructionLimits {
    fn default() -> Self {
        Self {
            max_depth: 4096,
            max_pending_links: 1_000_000,
            max_object_bytes: 64 * 1024 * 1024,
            max_objects: 10_000_000,
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

struct Frame {
    id: NativeObjectId,
    dependencies: Vec<NativeObjectId>,
    resolved: Vec<ObjectId>,
}

/// Rebuild a tree/blob closure or a commit's full history closure without an engine.
///
/// The walk is iterative, including commit parents. It never follows gitlinks,
/// never retains all emitted bytes or a closure-wide visited set, and fails on
/// lossy commits and spoollinks rather than claiming exact reconstruction.
/// Native states without a raw message serialize an empty Git message; use the
/// pure serializer when a caller supplies different parent or message policy.
/// The deterministic hosted seed is omitted from parent lists, as in local
/// projection: it represents initialization and has no Git commit.
pub async fn reconstruct_closure<R: ObjectReader, W: ObjectSink>(
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
    let mut visited = 0usize;
    let mut next = Some(root);

    loop {
        if let Some(id) = next.take() {
            if stack.len() >= limits.max_depth {
                return Err(ReconstructionError::LimitExceeded("depth"));
            }
            if visited >= limits.max_objects {
                return Err(ReconstructionError::LimitExceeded("objects"));
            }
            visited += 1;
            let available = limits.max_pending_links.saturating_sub(pending_links);
            let dependencies = match id {
                NativeObjectId::Commit(id) => {
                    let state = reader
                        .read_state(id, limits.max_object_bytes)
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
                    std::iter::once(NativeObjectId::Tree(state.tree))
                        .chain(parents.map(NativeObjectId::Commit))
                        .collect()
                }
                NativeObjectId::Tree(id) => {
                    let tree = reader
                        .read_tree(id, limits.max_object_bytes)
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
                    dependencies
                }
                NativeObjectId::Blob(_) => Vec::new(),
            };
            pending_links += dependencies.len();
            stack.push(Frame {
                id,
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
        let (kind, body) = match frame.id {
            NativeObjectId::Blob(id) => {
                let blob = reader
                    .read_blob(id, limits.max_object_bytes)
                    .await
                    .map_err(ReconstructionError::Reader)?;
                ("blob", blob.into_content())
            }
            NativeObjectId::Commit(id) => {
                let state = reader
                    .read_state(id, limits.max_object_bytes)
                    .await
                    .map_err(ReconstructionError::Reader)?;
                let Some(tree) = frame.resolved.first() else {
                    return Err(SerializeError::InvalidObject(
                        "commit has no resolved tree".into(),
                    )
                    .into());
                };
                if commit_byte_bound(&state, format, frame.resolved.len() - 1)
                    > limits.max_object_bytes
                {
                    return Err(ReconstructionError::LimitExceeded("object bytes"));
                }
                (
                    "commit",
                    serialize_commit(&state, tree, &frame.resolved[1..])?,
                )
            }
            NativeObjectId::Tree(id) => {
                let tree = reader
                    .read_tree(id, limits.max_object_bytes)
                    .await
                    .map_err(ReconstructionError::Reader)?;
                // Check the complete output size before allocating its body.
                let mut digits = Vec::new();
                let mut bytes = 0usize;
                for entry in tree.entries() {
                    if let Some(mode) = entry.git_mode() {
                        digits.clear();
                        mode.write_digits(&mut digits);
                        bytes = bytes
                            .saturating_add(digits.len())
                            .saturating_add(entry.name().len())
                            .saturating_add(2 + format.raw_len());
                    }
                }
                if bytes > limits.max_object_bytes {
                    return Err(ReconstructionError::LimitExceeded("object bytes"));
                }
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
                ("tree", body)
            }
        };
        if body.len() > limits.max_object_bytes {
            return Err(ReconstructionError::LimitExceeded("object bytes"));
        }
        let oid = object_id(format, kind, &body)?;
        sink.write_object(kind, oid, &body)
            .await
            .map_err(ReconstructionError::Sink)?;
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
