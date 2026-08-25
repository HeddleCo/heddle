// SPDX-License-Identifier: Apache-2.0
//! Heddle user-config TOML schema, credential-file store, transport knobs,
//! and tracing initialization.
//!
//! This crate contains configuration inputs and the durable global
//! credential-store schema used by both the Heddle CLI and operator tooling.
//! Transport implementations and application policy remain in their callers.

pub mod client_config;
pub mod config;
pub mod credentials;
pub mod logging;
pub mod output;
pub mod tls_trust;

pub use client_config::{
    ClientConfig, cleartext_connect_allowed, cleartext_refused_message, is_loopback_ip,
};
pub use config::UserConfig;
pub use logging::{
    LogFormat, LoggingConfig, LoggingGuard, init_logging, init_logging_default, is_enabled,
};
pub use output::OutputMode;
pub use tls_trust::{
    REMOTE_TLS_CA_CERT_SETTING, annotate_error_chain_tls_trust_failure, annotate_tls_trust_failure,
    is_tls_trust_failure,
};
