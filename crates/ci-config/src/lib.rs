// SPDX-License-Identifier: Apache-2.0
//! Typed, schema-versioned `.heddle/ci.toml` definitions.

mod model;
mod parse;

pub use model::{
    Check, CheckClass, CiConfig, DEFAULT_TIMEOUT_SECS, Meta, Retry, SUPPORTED_SCHEMA, Service,
    Trigger,
};
pub use parse::{ConfigError, definition_digest, parse};
