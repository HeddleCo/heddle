// SPDX-License-Identifier: Apache-2.0
//! Repository integrity check implementations.

mod objects;
mod refs;
mod repair;
mod state;
#[cfg(test)]
mod tests;

use serde::Serialize;

#[derive(Serialize, Clone)]
pub(super) struct FsckError {
    pub(super) kind: String,
    pub(super) message: String,
    pub(super) object: Option<String>,
}

pub(super) fn make_error(kind: &str, message: &str, object: Option<String>) -> FsckError {
    FsckError {
        kind: kind.to_string(),
        message: message.to_string(),
        object,
    }
}

pub(crate) use objects::{check_blobs, check_trees};
pub(crate) use refs::{check_merge_state, check_refs};
pub(crate) use repair::repair_issues;
pub(crate) use state::check_states;