// SPDX-License-Identifier: Apache-2.0
//! Tree diffing utilities.

use objects::{
    error::HeddleError,
    object::{ContentHash, FileChangeSet, diff_trees},
};

use super::{Repository, Result};

impl Repository {
    /// Get the difference between two trees.
    pub fn diff_trees(&self, from: &ContentHash, to: &ContentHash) -> Result<FileChangeSet> {
        diff_trees(&self.store, from, to)
            .map_err(|error| HeddleError::InvalidObject(format!("tree diff failed: {error}")))
    }
}
