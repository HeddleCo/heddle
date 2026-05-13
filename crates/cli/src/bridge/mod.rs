// SPDX-License-Identifier: Apache-2.0
//! Bridge modules for interoperability with other version control systems.
//!
//! This module provides bidirectional conversion between Heddle and other VCS,
//! starting with Git support.

pub mod git_core;
pub mod git_export;
pub mod git_import;
pub(crate) mod git_import_tree;
pub mod git_mapping;
pub mod git_notes;
pub mod git_sync;
pub mod git_util;

pub use git_core::{
    GitBridge, GitBridgeError, GitResult, SyncMapping, WriteThroughOutcome, WriteThroughSkipReason,
};

#[cfg(test)]
mod git_bridge_tests;