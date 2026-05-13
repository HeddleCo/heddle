// SPDX-License-Identifier: Apache-2.0
//! CLI-side utilities shared between the OSS `cli` crate and the
//! closed `hosted-client` crate.
//!
//! These items would create a circular dependency if they stayed in
//! `cli` (which depends on `hosted-client` when the `hosted-client`
//! feature is on, and `hosted-client` needs `UserConfig` /
//! `RemoteTarget` / `ClientConfig`). Pulling them out lets both sides
//! resolve cleanly.

pub mod client_config;
pub mod config;
pub mod remote;

pub use client_config::ClientConfig;
pub use config::UserConfig;
pub use remote::{Remote, RemoteConfig, RemoteTarget, resolve_remote_with_key};