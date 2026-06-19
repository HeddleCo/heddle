// SPDX-License-Identifier: Apache-2.0
//! CLI-side utilities shared between the OSS `cli` crate and the
//! closed `heddle-client` crate.
//!
//! These items would create a circular dependency if they stayed in
//! `cli` (which depends on `heddle-client` when the `heddle-client`
//! feature is on, and `heddle-client` needs `UserConfig` /
//! `RemoteTarget` / `ClientConfig`). Pulling them out lets both sides
//! resolve cleanly.

pub mod client_command;
pub mod client_config;
pub mod config;
pub mod remote;

pub use client_command::{ClientCommandContext, ClientOutputOverride, RemoteRecoveryAdvice};
pub use client_config::ClientConfig;
pub use config::UserConfig;
pub use remote::{Remote, RemoteConfig, RemoteTarget, resolve_remote_with_key};
