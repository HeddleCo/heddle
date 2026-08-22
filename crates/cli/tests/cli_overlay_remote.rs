// SPDX-License-Identifier: Apache-2.0
//! Git Overlay, projection, and remote integration tests.
//!
//! One of four themed CLI integration binaries split out of the former
//! single `cli_integration.rs` target (61 modules in one binary made
//! link times and `--test` filters unwieldy). Shared harness lives in
//! `tests/support/mod.rs`.

#[path = "support/mod.rs"]
mod support;
use support::*;

#[path = "cli_integration/clone_fsmonitor_decoupling.rs"]
mod clone_fsmonitor_decoupling;
#[path = "cli_integration/clone_output_contract.rs"]
mod clone_output_contract;
#[path = "cli_integration/git_overlay_fixtures.rs"]
mod git_overlay_fixtures;
#[path = "cli_integration/git_overlay_interop_matrix.rs"]
mod git_overlay_interop_matrix;
#[path = "cli_integration/git_overlay_matrix.rs"]
mod git_overlay_matrix;
#[path = "cli_integration/git_overlay_remote_ref_import.rs"]
mod git_overlay_remote_ref_import;
#[path = "cli_integration/git_overlay_sync_adoption.rs"]
mod git_overlay_sync_adoption;
#[path = "cli_integration/git_projection_commands.rs"]
mod git_projection_commands;
#[path = "cli_integration/git_replacement_matrix.rs"]
mod git_replacement_matrix;
#[path = "cli_integration/hydrate.rs"]
mod hydrate;
#[path = "cli_integration/native_scope_boundary.rs"]
mod native_scope_boundary;
#[path = "cli_integration/realworld_git.rs"]
mod realworld_git;
#[path = "cli_integration/remotes.rs"]
mod remotes;
#[path = "cli_integration/shared_target.rs"]
mod shared_target;
#[path = "cli_integration/unrelated_histories_recovery.rs"]
mod unrelated_histories_recovery;
