// SPDX-License-Identifier: Apache-2.0
//! CLI-side utilities shared between the OSS `cli` crate and the
//! closed `heddle-client` crate.
//!
//! These items would create a circular dependency if they stayed in
//! `cli` (which depends on `heddle-client` when the `heddle-client`
//! feature is on, and `heddle-client` needs `UserConfig` /
//! `RemoteTarget` / `ClientConfig`). Pulling them out lets both sides
//! resolve cleanly.

pub mod client_config;
pub mod config;
pub mod logging;
pub mod output;
pub mod remote;

pub use client_config::{
    ClientConfig, cleartext_connect_allowed, cleartext_refused_message, is_loopback_ip,
};
pub use config::UserConfig;
pub use logging::{
    LogFormat, LoggingConfig, LoggingGuard, init_logging, init_logging_default, is_enabled,
};
pub use output::OutputMode;
pub use remote::{
    Remote, RemoteConfig, RemoteTarget, remote_allows_insecure, resolve_remote_with_key,
    resolve_remote_with_key_and_insecure,
};
