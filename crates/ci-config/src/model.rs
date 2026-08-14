// SPDX-License-Identifier: Apache-2.0
//! Parsed CI-definition model.

use std::collections::BTreeMap;

use serde::Deserialize;

/// The only definition schema accepted by this build.
pub const SUPPORTED_SCHEMA: u32 = 1;
/// Default per-check timeout: one hour.
pub const DEFAULT_TIMEOUT_SECS: u64 = 3600;

/// A parsed and validated `.heddle/ci.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiConfig {
    /// Definition metadata.
    pub meta: Meta,
    /// Checks in authored order.
    pub checks: Vec<Check>,
    /// Non-fatal unknown-key warnings.
    pub warnings: Vec<String>,
}

/// Definition metadata.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Meta {
    /// Definition schema version.
    pub schema: u32,
}

/// One authored check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Unique name within the definition.
    pub name: String,
    /// Gating class.
    pub class: CheckClass,
    /// Exact argv to execute.
    pub command: Vec<String>,
    /// Per-check deadline in seconds.
    pub timeout_secs: u64,
    /// Explicit environment overrides.
    pub env: BTreeMap<String, String>,
    /// Requested service containers.
    pub services: Vec<Service>,
    /// Persistent cache slots.
    pub cache_paths: Vec<String>,
    /// Flake retry policy.
    pub retry: Retry,
    /// Check triggers.
    pub triggers: Vec<Trigger>,
    /// Whether a newer state supersedes an in-flight run.
    pub supersede: bool,
    /// Optional isolation class.
    pub isolation: Option<String>,
}

/// Whether a check gates, advises, or only informs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckClass {
    /// A non-green result is a required-check failure.
    Required,
    /// Result is advisory.
    Advisory,
    /// Result is context only.
    Informational,
}

/// A service container requested by a check.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Service {
    /// Local service name.
    pub name: String,
    /// Container image reference.
    pub image: String,
    /// Ports published one-to-one.
    #[serde(default)]
    pub ports: Vec<u16>,
    /// Service environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Optional readiness-probe argv.
    #[serde(default)]
    pub ready_cmd: Option<Vec<String>>,
}

/// Flake retry policy.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Retry {
    /// Maximum retries after the initial attempt.
    pub max: u32,
    /// Regexes which identify retryable output.
    pub flake_signatures: Vec<String>,
}

/// A check trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// Every push.
    Push,
    /// Explicit dispatch.
    Manual,
    /// Five-field cron expression.
    Cron(String),
}

impl Trigger {
    /// Canonical serialized token.
    #[must_use]
    pub fn as_str(&self) -> String {
        match self {
            Self::Push => "push".to_string(),
            Self::Manual => "manual".to_string(),
            Self::Cron(expression) => format!("cron:{expression}"),
        }
    }
}
