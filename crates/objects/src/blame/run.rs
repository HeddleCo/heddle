// SPDX-License-Identifier: Apache-2.0
//! One-shot blame that loops storage-neutral slices until complete.

use std::{cell::RefCell, collections::HashMap, path::Path};

use crate::object::{Blob, ContentHash, FileProvenance, ObjectSource, State, StateId, Tree};

use super::{
    advance::advance_file_blame_slice,
    finalize::finalize_file_provenance,
    prepare::prepare_file_blame,
    types::{
        BlamePreparation, BlameSliceAdvance, BlameSliceError, BlameSliceLimits, BlameTarget,
        OriginRange,
    },
};

/// Resolved objects shared by every slice of one local blame walk.
///
/// Preparing a blame and advancing its frontier revisit the target objects;
/// parents are likewise loaded once while being claimed and again if they
/// become the next frontier. Keeping this cache at the one-shot boundary
/// avoids re-decoding those objects without making resumable slices stateful.
struct BlameObjectCache<'source, S> {
    source: &'source S,
    trees: RefCell<HashMap<ContentHash, Option<Tree>>>,
    states: RefCell<HashMap<StateId, Option<State>>>,
    blobs: RefCell<HashMap<ContentHash, Option<Blob>>>,
}

impl<'source, S> BlameObjectCache<'source, S> {
    fn new(source: &'source S) -> Self {
        Self {
            source,
            trees: RefCell::new(HashMap::new()),
            states: RefCell::new(HashMap::new()),
            blobs: RefCell::new(HashMap::new()),
        }
    }
}

impl<S: ObjectSource> ObjectSource for BlameObjectCache<'_, S> {
    fn get_tree(&self, hash: &ContentHash) -> crate::error::Result<Option<Tree>> {
        if let Some(tree) = self.trees.borrow().get(hash).cloned() {
            return Ok(tree);
        }
        let tree = self.source.get_tree(hash)?;
        self.trees.borrow_mut().insert(*hash, tree.clone());
        Ok(tree)
    }

    fn get_state(&self, id: &StateId) -> crate::error::Result<Option<State>> {
        if let Some(state) = self.states.borrow().get(id).cloned() {
            return Ok(state);
        }
        let state = self.source.get_state(id)?;
        self.states.borrow_mut().insert(*id, state.clone());
        Ok(state)
    }

    fn get_blob(&self, hash: &ContentHash) -> crate::error::Result<Option<Blob>> {
        if let Some(blob) = self.blobs.borrow().get(hash).cloned() {
            return Ok(blob);
        }
        let blob = self.source.get_blob(hash)?;
        self.blobs.borrow_mut().insert(*hash, blob.clone());
        Ok(blob)
    }

    fn decoded_blob_len(&self, hash: &ContentHash) -> crate::error::Result<Option<u64>> {
        if let Some(blob) = self.blobs.borrow().get(hash) {
            return Ok(blob.as_ref().map(|blob| blob.content().len() as u64));
        }
        self.source.decoded_blob_len(hash)
    }
}

/// Walk `path` at `state` by repeating [`advance_file_blame_slice`] until the
/// frontier is exhausted, then finalize. This is not an eager full-file shim;
/// each slice stays inside `limits`.
pub fn blame_file<S: ObjectSource>(
    source: &S,
    state: &State,
    path: &Path,
    limits: BlameSliceLimits,
) -> Result<FileProvenance, BlameSliceError> {
    let source = BlameObjectCache::new(source);
    match prepare_file_blame(&source, state, path, limits)? {
        BlamePreparation::MissingPath => Err(BlameSliceError::MissingPath),
        BlamePreparation::Unblamable => Err(BlameSliceError::Unblamable),
        BlamePreparation::Empty { file_blob, origin } => finalize_file_provenance(
            file_blob,
            0,
            [OriginRange {
                target_start: 0,
                len: 0,
                origin,
            }],
        ),
        BlamePreparation::Active {
            file_blob,
            line_count,
            mut frontier,
        } => {
            let expected = BlameTarget::bind(state.id(), path, file_blob, line_count)?;
            frontier.require_target(&expected)?;
            let mut finalized = Vec::new();
            loop {
                frontier.require_target(&expected)?;
                match advance_file_blame_slice(&source, path, frontier, limits)? {
                    BlameSliceAdvance::Progress {
                        next,
                        finalized: more,
                        ..
                    } => {
                        finalized.extend(more);
                        frontier = next;
                    }
                    BlameSliceAdvance::Complete {
                        finalized: more, ..
                    } => {
                        finalized.extend(more);
                        break;
                    }
                }
            }
            finalize_file_provenance(file_blob, line_count, finalized)
        }
    }
}
