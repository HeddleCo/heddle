// SPDX-License-Identifier: Apache-2.0
//! Wire shape for `heddle init --output json`.
//!
//! This is the registered schema type, not a mirror. `Serialize` and
//! `JsonSchema` are derived on the same struct so skip-serialized
//! fields cannot reappear on the published schema.

use std::path::PathBuf;

use heddle_cli_args::INIT_VERB;
use schemars::JsonSchema;
use serde::Serialize;

use super::verification_health::RepositoryVerificationState;

/// JSON payload for a successful `heddle init`.
#[derive(Serialize, JsonSchema)]
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

impl InitOutput {
    /// Same identifier as [`INIT_VERB`].
    pub const VERB: &'static str = INIT_VERB;
}

/// Principal identity included in init JSON when one is configured.
#[derive(Serialize, JsonSchema)]
#[schemars(rename = "InitPrincipalSchema")]
pub struct InitPrincipalOutput {
    pub name: String,
    pub email: String,
}

#[cfg(test)]
mod tests {
    use schemars::schema_for;
    use serde_json::Value;

    use super::*;

    fn property_keys(schema: &Value) -> Vec<String> {
        let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
            return Vec::new();
        };
        properties.keys().cloned().collect()
    }

    #[test]
    fn init_schema_is_the_real_output_type() {
        let from_type = serde_json::to_value(schema_for!(InitOutput))
            .expect("InitOutput schema should serialize");
        let type_keys = property_keys(&from_type);
        assert!(
            !type_keys.iter().any(|key| key == "verification"),
            "skip-serialized verification must not appear on the derived schema"
        );
        assert!(
            !type_keys
                .iter()
                .any(|key| key == "placeholder_principal_warning"),
            "text-only placeholder warning must not appear on the derived schema"
        );
        assert_eq!(
            from_type.get("title").and_then(Value::as_str),
            Some("InitSchema"),
            "published schema title stays InitSchema"
        );
    }
}
