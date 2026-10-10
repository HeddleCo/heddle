// SPDX-License-Identifier: Apache-2.0
//! Git projection engine modules for interoperability with Git.
//!
//! This module provides bidirectional conversion between Heddle state and Git
//! projection state.

pub mod credential;
pub mod facet_gate;
pub mod gateway_received;
pub mod gateway_view;
pub mod gateway_write;
#[cfg(feature = "gateway-publication")]
pub mod gateway_publication;
pub mod git_core;
pub mod git_export;
pub mod git_frontier;
pub mod git_ingest;
pub mod git_mapping;
pub mod git_notes;
pub mod git_reconstruct;
pub mod git_residual;
pub mod git_sync;
pub mod git_util;
pub mod source_ref_budget;
#[cfg(debug_assertions)]
#[doc(hidden)]
pub mod test_support;

pub use git_core::{
    GitProjection, GitProjectionError, GitProjectionResult, SyncMapping, WriteThroughOutcome,
    WriteThroughSkipReason, configure_https_ca_certificate_pem, configured_https_client,
    discover_git_source_ref_targets, discover_git_source_refs, git_transport_error_message,
};
pub use git_residual::{RESIDUALS_DIR_NAME, ResidualObject, ResidualStore};
