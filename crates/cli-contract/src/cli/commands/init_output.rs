// SPDX-License-Identifier: Apache-2.0
//! Wire shape for `heddle init --output json`.
//!
//! This is the real output struct, not a schema mirror. `Serialize` and
//! `JsonSchema` are derived on the same type so clap's sibling args
//! struct (`InitArgs`) and this output cannot drift at the schema layer.

use std::path::PathBuf;

use heddle_cli_macro::HeddleVerbOutput;
use schemars::JsonSchema;
use serde::Serialize;

use super::verification_health::RepositoryVerificationState;

/// JSON payload for a successful `heddle init`.
#[derive(Serialize, JsonSchema, HeddleVerbOutput)]
#[heddle_verb("init")]
#[schemars(rename = "InitSchema")]
pub struct InitOutput {
    /// Stable machine discriminator. Always `init`.
    pub output_kind: &'static str,
    /// Always `initialized` on success.
    pub status: String,
    /// Always `init` on success.
    pub action: String,
    /// Path to the initialized `.heddle` metadata directory.
    #[schemars(with = "String")]
    pub path: PathBuf,
    /// Repository capability after init, e.g. `git-overlay` or native Heddle storage.
    pub repository_mode: String,
    /// Whether init detected an existing Git repository.
    pub git_detected: bool,
    /// Whether Heddle metadata is now present.
    pub heddle_initialized: bool,
    /// Whether init installed a Heddle ignore-policy file. Currently always false.
    pub installed_heddleignore: bool,
    /// Whether init wrote a default principal into user config.
    pub principal_configured: bool,
    /// Principal configuration status (`configured` or `not_configured`).
    pub principal_status: String,
    /// Where the principal was resolved from, when configured.
    pub principal_source: Option<String>,
    /// Configured principal identity, when present.
    pub principal: Option<InitPrincipalOutput>,
    /// Suggested command to set a principal when none is configured.
    pub principal_recommended_action: Option<String>,
    /// Text-only warning; never serialized.
    #[serde(skip)]
    #[schemars(skip)]
    pub placeholder_principal_warning: Option<String>,
    /// Human-readable list of what init changed or intentionally left untouched.
    pub side_effects: Vec<String>,
    /// Human summary.
    pub message: String,
    /// Primary verification-guided next command.
    pub next_action: Option<String>,
    /// Same contract as `next_action` for callers that read the recommended field.
    pub recommended_action: Option<String>,
    /// Omitted from mutation replies. Use `heddle verify` / `heddle status`.
    #[allow(dead_code)]
    #[serde(skip_serializing)]
    #[serde(rename = "verification")]
    #[schemars(skip)]
    pub trust: RepositoryVerificationState,
}

/// Principal identity included in init JSON when one is configured.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "InitPrincipalSchema")]
pub struct InitPrincipalOutput {
    pub name: String,
    pub email: String,
}
