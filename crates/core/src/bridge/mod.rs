// SPDX-License-Identifier: Apache-2.0
//! Bridge modules for interoperability with other version control systems.
//!
//! This module provides bidirectional conversion between Heddle and other VCS,
//! starting with Git support.

pub mod git_core;
pub mod git_export;
pub mod git_ingest;
pub mod git_mapping;
pub mod git_notes;
pub mod git_reconstruct;
pub mod git_sync;
pub mod git_util;
#[cfg(debug_assertions)]
#[doc(hidden)]
pub mod test_support;

pub use git_core::{
    GitBridge, GitBridgeError, GitResult, SyncMapping, WriteThroughOutcome, WriteThroughSkipReason,
};
