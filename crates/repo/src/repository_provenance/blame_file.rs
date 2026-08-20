// SPDX-License-Identifier: Apache-2.0
//! Path-targeted blame, mirroring git blame's algorithm.
//!
//! The walk lives in [`objects::blame`]: scratch-budgeted equal-run LCS plus
//! storage-neutral resumable slices. This wrapper loops those slices with
//! unlimited local caps so `heddle blame` keeps exact provenance.

use std::path::Path;

use objects::{
    blame::{BlameSliceError, BlameSliceLimits, blame_file},
    object::{FileProvenance, State},
};

use super::{Repository, Result};
use crate::repository::history_instrumentation::HistoryObjectSource;

impl Repository {
    /// Path-targeted blame: walk ancestry only along `path`'s blob-hash
    /// boundary and synthesize a [`FileProvenance`].
    ///
    /// Returns `Ok(None)` if `path` does not exist at `state` or the file is
    /// binary. Store misses are [`crate::HeddleError::MissingObject`], not
    /// `Ok(None)`. Otherwise produces the same shape
    /// `get_file_provenance_for_state` used to return from the tree-oriented
    /// path.
    pub(crate) fn blame_file_via_path_walk(
        &self,
        state: &State,
        path: &Path,
    ) -> Result<Option<FileProvenance>> {
        let source = HistoryObjectSource::new(&self.store);
        match blame_file(&source, state, path, BlameSliceLimits::unlimited()) {
            Ok(provenance) => Ok(Some(provenance)),
            Err(BlameSliceError::MissingPath | BlameSliceError::Unblamable) => Ok(None),
            Err(BlameSliceError::MissingObject { kind, id }) => {
                Err(super::HeddleError::MissingObject {
                    object_type: kind.to_string(),
                    id,
                })
            }
            Err(BlameSliceError::Store(error)) => Err(error),
            Err(error) => Err(super::HeddleError::InvalidObject(error.to_string())),
        }
    }
}
