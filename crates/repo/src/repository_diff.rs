// SPDX-License-Identifier: Apache-2.0
//! Tree diffing utilities.

use std::ops::ControlFlow;

use objects::{
    error::HeddleError,
    object::{ContentHash, FileChange, FileChangeSet, diff_trees, diff_trees_visit},
};

use super::{Repository, Result};

impl Repository {
    /// Get the difference between two trees.
    pub fn diff_trees(&self, from: &ContentHash, to: &ContentHash) -> Result<FileChangeSet> {
        diff_trees(&self.store, from, to)
            .map_err(|error| HeddleError::InvalidObject(format!("tree diff failed: {error}")))
    }

    /// Diff two trees with internal iteration, invoking `visitor` for each
    /// change in traversal order.
    ///
    /// Unlike [`Repository::diff_trees`], this never materializes the full
    /// change list: the visitor returns a [`ControlFlow`] so early-exit
    /// consumers can stop on the first relevant change without allocating a
    /// [`FileChangeSet`]. Returns the carried `B` on early exit, or
    /// `ControlFlow::Continue(())` when the full diff is walked.
    pub fn diff_trees_visit<V, B>(
        &self,
        from: &ContentHash,
        to: &ContentHash,
        visitor: V,
    ) -> Result<ControlFlow<B>>
    where
        V: FnMut(FileChange) -> ControlFlow<B>,
    {
        diff_trees_visit(&self.store, from, to, visitor)
            .map_err(|error| HeddleError::InvalidObject(format!("tree diff failed: {error}")))
    }
}
