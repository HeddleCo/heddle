// SPDX-License-Identifier: Apache-2.0
//! Heddle: An AI-native version control system
//!
//! Heddle provides content-addressed storage, immutable history with stable change
//! identifiers, and explicit agent attribution for AI-augmented development.

#[cfg(not(any(feature = "git-overlay", feature = "native")))]
compile_error!(
    "At least one of the `git-overlay` or `native` features must be enabled. \
     The OSS CLI ships as git-overlay-only, native-only, or both."
);

pub(crate) mod attribution;
pub mod bench;
// The bridge module stays always-compiled so light consumers (fsck,
// clone, fetch, remote, checkpoint, operator_loop, gc) keep working in
// native-only builds without fanning #[cfg] through their use blocks.
// User-visible separation is enforced at the command surface:
// `Commands::Bridge` and `Commands::GitOverlay` are gated behind
// `git-overlay`, so a native-only `heddle` binary exposes no
// overlay-specific subcommands. Deeper code-elimination can come later.
pub mod bridge;
pub mod cli;
pub mod client;
pub mod exit;
pub mod extensions;
pub mod harness;
pub mod operation_id;
pub mod perf;
#[cfg(feature = "semantic")]
pub mod semantic;
pub mod ts_codegen;
pub mod util;

// Shared types now live in cli-shared (so heddle-client can depend on
// them without a cli ↔ heddle-client cycle). Re-export under the
// historical paths so internal code keeps working.
pub use cli_shared::{
    LogFormat, LoggingConfig, LoggingGuard, OutputMode, config, init_logging,
    init_logging_default, is_enabled, logging, log_operation, log_repo_event, remote,
};
pub use objects::{
    error::{HeddleError, HeddleError as StoreError},
    store::ObjectStore,
};
pub use repo::Repository;
pub type StoreResult<T> = objects::error::Result<T>;

#[cfg(test)]
mod object_graph_tests;
