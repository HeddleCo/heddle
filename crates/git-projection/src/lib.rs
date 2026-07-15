// SPDX-License-Identifier: Apache-2.0
//! Git projection engine modules for interoperability with Git.
//!
//! This module provides bidirectional conversion between Heddle state and Git
//! projection state.

pub mod credential;
pub mod git_core;
pub mod git_export;
pub mod git_ingest;
pub mod git_mapping;
pub mod git_notes;
pub mod git_reconstruct;
pub mod git_residual;
pub mod git_sync;
pub mod git_util;
#[cfg(debug_assertions)]
#[doc(hidden)]
pub mod test_support;

pub use git_core::{
    GitProjection, GitProjectionError, GitProjectionResult, SyncMapping, WriteThroughOutcome,
    WriteThroughSkipReason,
};
pub use git_residual::{
    BridgeMirrorRetirementStatus, RESIDUALS_DIR_NAME, ResidualObject, ResidualStore,
    bridge_mirror_retirement_status, resolve_lossy_object,
};
