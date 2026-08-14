// SPDX-License-Identifier: Apache-2.0
//! Hermetic check environment construction.

use std::collections::BTreeMap;

/// Host variables needed to locate ordinary POSIX/Rust tooling.
pub const BASE_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "TERM",
    "LANG",
    "LC_ALL",
    "CARGO_HOME",
    "RUSTUP_HOME",
];
/// Deterministic Git author/committer name.
pub const GIT_IDENTITY_NAME: &str = "heddle ci";
/// Deterministic Git author/committer email.
pub const GIT_IDENTITY_EMAIL: &str = "ci@heddle.invalid";

/// Builder for the exact environment both executed and recorded in `repro`.
#[derive(Debug, Clone)]
pub struct HermeticEnv {
    git_hermetic: bool,
    host: BTreeMap<String, String>,
}

impl HermeticEnv {
    /// Capture the allowed variables from the current process.
    #[must_use]
    pub fn new() -> Self {
        let host = BASE_ALLOWLIST
            .iter()
            .filter_map(|name| {
                std::env::var(name)
                    .ok()
                    .map(|value| ((*name).to_string(), value))
            })
            .collect();
        Self {
            git_hermetic: true,
            host,
        }
    }

    /// Construct from an explicit host map, primarily for tests.
    #[must_use]
    pub fn with_host(host: BTreeMap<String, String>) -> Self {
        Self {
            git_hermetic: true,
            host,
        }
    }

    /// Enable or disable deterministic Git configuration.
    #[must_use]
    pub fn git_hermetic(mut self, enabled: bool) -> Self {
        self.git_hermetic = enabled;
        self
    }

    /// Produce the sorted effective environment.
    #[must_use]
    pub fn build(
        &self,
        check: &BTreeMap<String, String>,
        services: &BTreeMap<String, String>,
        caches: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        let mut output = self.host.clone();
        if self.git_hermetic {
            output.insert("GIT_CONFIG_GLOBAL".into(), "/dev/null".into());
            output.insert("GIT_CONFIG_SYSTEM".into(), "/dev/null".into());
            output.insert("GIT_AUTHOR_NAME".into(), GIT_IDENTITY_NAME.into());
            output.insert("GIT_AUTHOR_EMAIL".into(), GIT_IDENTITY_EMAIL.into());
            output.insert("GIT_COMMITTER_NAME".into(), GIT_IDENTITY_NAME.into());
            output.insert("GIT_COMMITTER_EMAIL".into(), GIT_IDENTITY_EMAIL.into());
        }
        for source in [services, caches, check] {
            output.extend(
                source
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        }
        output
    }
}

impl Default for HermeticEnv {
    fn default() -> Self {
        Self::new()
    }
}
