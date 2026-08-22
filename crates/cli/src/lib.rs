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
pub mod cli;
pub mod client;
pub mod exit;
pub mod extensions;
pub mod harness;
mod hosted_failure;
#[cfg(feature = "client")]
mod hosted_runtime;
pub mod operation_id;
pub mod perf;
#[cfg(feature = "semantic")]
pub mod semantic;
pub mod ts_codegen;
pub mod util;

// User-config schema, credentials, transport knobs, and tracing init live in
// the config crate. `::config` disambiguates the extern crate from the
// module named `config` re-exported below.
pub use ::config::{
    LogFormat, LoggingConfig, LoggingGuard, OutputMode, config, init_logging, init_logging_default,
    is_enabled, log_operation, log_repo_event, logging,
};
// Remote aliases and `.heddle/remotes.toml` parsing live with the repo layer.
pub use repo::remote;
pub use objects::{
    error::{HeddleError, HeddleError as StoreError},
    store::ObjectStore,
};
pub use repo::Repository;
pub type StoreResult<T> = objects::error::Result<T>;

/// Register factories needed to reopen CLI-owned lazy hosted repositories.
#[cfg(feature = "client")]
pub fn register_hosted_factory() {
    hosted_runtime::hosted::register_hosted_factory();
}

#[cfg(test)]
mod object_graph_tests;
