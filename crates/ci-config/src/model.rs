// SPDX-License-Identifier: Apache-2.0
//! Engine-facing check model mapped from a canonical `TreadleDefinition`.

use std::collections::BTreeMap;

/// Default per-check timeout used by engine tests that construct [`Check`] directly.
pub const DEFAULT_TIMEOUT_SECS: u64 = 3600;

/// A decoded and mapped local CI definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiConfig {
    /// Pipeline name from the definition.
    pub name: String,
    /// Definition-format version. Version 1 readers accept exactly 1.
    pub format_version: u32,
    /// Checks in canonical job-then-check name order.
    pub checks: Vec<Check>,
}

impl CiConfig {
    /// Wrap checks in a test pipeline.
    #[must_use]
    pub fn from_checks(checks: Vec<Check>) -> Self {
        Self {
            name: "test".to_string(),
            format_version: 1,
            checks,
        }
    }
}

/// One signable argv check, flattened from a concrete job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Unique name within the definition.
    pub name: String,
    /// Gating class hint.
    pub class: CheckClass,
    /// Exact argv to execute (`command` followed by `args`).
    pub command: Vec<String>,
    /// Per-check deadline in seconds.
    pub timeout_secs: u64,
    /// Literal environment overrides. Secret refs are never inlined.
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
    /// Optional isolation profile name.
    pub isolation: Option<String>,
    /// Repository-relative working directory. Empty means the evaluated root.
    pub working_directory: String,
}

impl Check {
    /// A required argv check with engine defaults.
    #[must_use]
    pub fn new(name: impl Into<String>, command: Vec<String>) -> Self {
        Self {
            name: name.into(),
            class: CheckClass::Required,
            command,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            env: BTreeMap::new(),
            services: Vec::new(),
            cache_paths: Vec::new(),
            retry: Retry::default(),
            triggers: Vec::new(),
            supersede: true,
            isolation: None,
            working_directory: String::new(),
        }
    }
}

/// Whether a check gates, advises, or only informs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckClass {
    /// A non-green result is a required-check failure.
    Required,
    /// Result is advisory.
    Advisory,
    /// Result is context only.
    Informational,
}

/// A service container requested by a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// Local service name.
    pub name: String,
    /// Container image locator. Identity is the digest on the proto, not this tag.
    pub image: String,
    /// Ports published one-to-one.
    pub ports: Vec<u16>,
    /// Literal service environment.
    pub env: BTreeMap<String, String>,
    /// Optional readiness-probe argv.
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
