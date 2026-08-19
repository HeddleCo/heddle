// SPDX-License-Identifier: Apache-2.0
//! Shared Heddle CLI configuration and credential-file contracts.
//!
//! This crate contains transport configuration inputs and the durable global
//! credential-store schema used by both the Heddle CLI and operator tooling.
//! Transport implementations and application policy remain in their callers.

pub mod client_config;
pub mod config;
pub mod credentials;
pub mod logging;
pub mod output;
pub mod remote;

pub use client_config::{
    ClientConfig, cleartext_connect_allowed, cleartext_refused_message, is_loopback_ip,
};
pub use config::{
    ResolvedPrincipal, UserConfig, principal_source_display, resolve_principal,
    resolve_principal_without_repo,
};
pub use logging::{
    LogFormat, LoggingConfig, LoggingGuard, init_logging, init_logging_default, is_enabled,
};
pub use output::OutputMode;
pub use remote::{
    Remote, RemoteConfig, RemoteTarget, remote_allows_insecure, resolve_remote_with_key,
    resolve_remote_with_key_and_insecure,
};
