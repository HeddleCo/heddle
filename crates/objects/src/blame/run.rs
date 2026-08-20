// SPDX-License-Identifier: Apache-2.0
//! One-shot blame that loops storage-neutral slices until complete.

use std::path::Path;

use crate::object::{FileProvenance, ObjectSource, State};

use super::{
    advance::advance_file_blame_slice,
    finalize::finalize_file_provenance,
    prepare::prepare_file_blame,
    types::{BlamePreparation, BlameSliceAdvance, BlameSliceError, BlameSliceLimits, OriginRange},
};

/// Walk `path` at `state` by repeating [`advance_file_blame_slice`] until the
/// frontier is exhausted, then finalize. This is not an eager full-file shim;
/// each slice stays inside `limits`.
pub fn blame_file<S: ObjectSource>(
    source: &S,
    state: &State,
    path: &Path,
    limits: BlameSliceLimits,
) -> Result<FileProvenance, BlameSliceError> {
    match prepare_file_blame(source, state, path, limits)? {
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
            let mut finalized = Vec::new();
            loop {
                match advance_file_blame_slice(source, path, frontier, limits)? {
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
