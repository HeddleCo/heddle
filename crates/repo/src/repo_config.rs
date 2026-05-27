// SPDX-License-Identifier: Apache-2.0
//! Repository configuration handling.

use std::{io::Read, path::Path};

use objects::fs_atomic::write_file_atomic;
use serde::{Deserialize, Serialize};

use super::Result;
use crate::FsMonitorConfig;

/// Repository configuration stored in `.heddle/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    pub repository: RepositoryConfig,
    #[serde(default)]
    pub principal: Option<PrincipalConfig>,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub worktree: WorktreeConfig,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    #[serde(default)]
    pub output: OutputConfig,
    #[serde(default)]
    pub policies: PoliciesConfig,
    #[serde(default)]
    pub display: DisplayConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub hosted: HostedConfig,
    #[serde(default)]
    pub review: ReviewConfig,
    #[serde(default)]
    pub redact: RedactConfig,
}

/// `[redact]` config section. Houses the list of operator public keys
/// that the receiver trusts when accepting redactions over the wire
/// (`Repository::accept_wire_redactions`). The list is fail-closed:
/// an empty list rejects every incoming signed redaction. Operators
/// populate it via `heddle redact trust <pem>` (planned) or by
/// hand-editing `.heddle/config.toml`.
///
/// The reason the trust list lives in repo config rather than in the
/// signed payload itself: a signature only proves *who* declared the
/// redaction, not whether the receiver should *honor* that operator's
/// authority for this workspace. Without a separate trust anchor an
/// attacker can mint a redaction with their own key and pass
/// signature verification trivially.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RedactConfig {
    /// Trusted operator public keys. Every signed redaction received
    /// over the wire must match one of these (algorithm + hex-encoded
    /// public key) before the receiver persists the sidecar.
    #[serde(default)]
    pub trusted_keys: Vec<TrustedKey>,
}

/// One trusted operator key entry. Algorithm and key strings use the
/// same wire-format as `StateSignature`: `algorithm` is `ed25519`,
/// `p256`, or `rsa`; `public_key` is hex-encoded raw key bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustedKey {
    pub algorithm: String,
    pub public_key: String,
    /// Free-text label so operators can name keys in the config
    /// (`"luke-laptop"`, `"ci-signing"`) without changing trust
    /// semantics. Optional; doesn't affect matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Review-epic configuration. Houses the `[review.signals]` sub-table read by
/// the risk-signal registry. The struct is intentionally shaped as a thin
/// wrapper so later epics can hang neighbours (`[review.discussion]`,
/// `[review.tick_budget]`) off it without churning every consumer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewConfig {
    #[serde(default)]
    pub signals: ReviewSignalsToml,
}

/// TOML representation of `[review.signals]`. Re-serialised into the
/// `state_review` crate's `ReviewSignalsConfig` at the call site so
/// `repo_config` doesn't have to depend on `state_review` (which would
/// either need to be unconditional or duplicate every consumer's feature
/// gate). The shape mirrors `ReviewSignalsConfig` field-for-field; tail-
/// append only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewSignalsToml {
    #[serde(default)]
    pub novelty: SignalModuleToml,
    #[serde(default)]
    pub test_reachability: TestReachabilityToml,
    #[serde(default)]
    pub pattern_deviation: PatternDeviationToml,
    #[serde(default)]
    pub invariant_adjacency: SignalEnableToml,
    #[serde(default)]
    pub self_flagged_uncertainty: SelfFlaggedToml,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalModuleToml {
    #[serde(default = "review_default_true")]
    pub enabled: bool,
    #[serde(default = "review_default_novelty_tolerance")]
    pub tolerance: f32,
}

impl Default for SignalModuleToml {
    fn default() -> Self {
        Self {
            enabled: true,
            tolerance: review_default_novelty_tolerance(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestReachabilityToml {
    #[serde(default = "review_default_true")]
    pub enabled: bool,
    #[serde(default = "review_default_min_tests")]
    pub min_test_functions_in_repo: u32,
}

impl Default for TestReachabilityToml {
    fn default() -> Self {
        Self {
            enabled: true,
            min_test_functions_in_repo: review_default_min_tests(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternDeviationToml {
    #[serde(default = "review_default_true")]
    pub enabled: bool,
    #[serde(default = "review_default_pattern_threshold")]
    pub threshold: f32,
}

impl Default for PatternDeviationToml {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: review_default_pattern_threshold(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalEnableToml {
    #[serde(default = "review_default_true")]
    pub enabled: bool,
}

impl Default for SignalEnableToml {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfFlaggedToml {
    #[serde(default = "review_default_true")]
    pub enabled: bool,
    #[serde(default = "review_default_self_flag_cap")]
    pub max_per_state: u32,
}

impl Default for SelfFlaggedToml {
    fn default() -> Self {
        Self {
            enabled: true,
            max_per_state: review_default_self_flag_cap(),
        }
    }
}

fn review_default_true() -> bool {
    true
}
fn review_default_novelty_tolerance() -> f32 {
    0.15
}
fn review_default_min_tests() -> u32 {
    3
}
fn review_default_pattern_threshold() -> f32 {
    0.6
}
fn review_default_self_flag_cap() -> u32 {
    5
}

/// Per-repository hosted-service linkage. Populated when the repo is attached
/// to a Heddle hosted server; consulted by presence publishers, sync workflows,
/// and any future feature that needs to know which upstream namespace owns
/// the local checkout.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostedConfig {
    /// Base URL of the hosted server (e.g. `https://heddle.example.com`).
    /// Presence WebSocket clients append `/presence/ws` — they tolerate
    /// `http(s)://` or `ws(s)://` on input.
    #[serde(default)]
    pub upstream_url: Option<String>,
    /// Hosted namespace path (e.g. `heddle/core`) that this repository
    /// publishes into. When absent, presence stays local-only.
    #[serde(default)]
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryConfig {
    pub version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrincipalConfig {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentConfig {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeConfig {
    #[serde(default = "default_ignore")]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub fsmonitor: FsMonitorConfig,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            ignore: default_ignore(),
            fsmonitor: FsMonitorConfig::default(),
        }
    }
}

fn default_ignore() -> Vec<String> {
    vec![".heddle".to_string()]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultsConfig {
    #[serde(default = "default_confidence")]
    pub confidence: f32,
}

fn default_confidence() -> f32 {
    0.8
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            confidence: default_confidence(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Json,
    #[default]
    Text,
}

// Hand-written Deserialize so a legacy `output.format = "auto"` fails
// with a field-named, value-named message instead of the default serde
// "unknown variant" wording. The bug class #271 closes is the silent
// JSON-when-piped surprise the old `auto` mode produced; rather than
// keeping an alias that would re-route it to `text`, pre-1.0 we error
// loudly so the operator updates the config. The error string is the
// load-bearing contract — the CLI's error envelope (see
// `print_error_with_hint`) classifies it by substring match.
impl<'de> Deserialize<'de> for OutputFormat {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct OutputFormatVisitor;
        impl<'de> serde::de::Visitor<'de> for OutputFormatVisitor {
            type Value = OutputFormat;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("'text' or 'json'")
            }
            fn visit_str<E>(self, value: &str) -> std::result::Result<OutputFormat, E>
            where
                E: serde::de::Error,
            {
                match value {
                    "text" => Ok(OutputFormat::Text),
                    "json" => Ok(OutputFormat::Json),
                    other => Err(E::custom(format!(
                        "invalid output.format: '{other}' — valid values are 'text' or 'json'"
                    ))),
                }
            }
        }
        deserializer.deserialize_str(OutputFormatVisitor)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OutputConfig {
    // Optional so callers can distinguish "repo explicitly set
    // `output.format`" from "repo left it at the default" — needed
    // to layer repo config on top of user config without the
    // repo's default overwriting a user-configured value.
    #[serde(default)]
    pub format: Option<OutputFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PoliciesConfig {
    #[serde(default)]
    pub default_policy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    #[serde(default = "default_hash_length")]
    pub hash_length: usize,
    #[serde(default = "default_change_id_format")]
    pub change_id_format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StorageConfig {
    #[serde(default)]
    pub filesystem: Option<FilesystemStorageConfig>,
    #[cfg(feature = "s3")]
    #[serde(default)]
    pub s3: Option<S3StorageConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemStorageConfig {
    /// Path to the storage directory (relative to .heddle or absolute)
    pub path: Option<String>,
}

/// S3-compatible object storage configuration.
///
/// When present in `.heddle/config.toml`, the repository stores blobs, trees,
/// states, and actions in the specified S3 bucket instead of the local
/// filesystem.
#[cfg(feature = "s3")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3StorageConfig {
    pub bucket: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub endpoint_url: Option<String>,
    #[serde(default)]
    pub access_key_id: Option<String>,
    #[serde(default)]
    pub secret_access_key: Option<String>,
    #[serde(default)]
    pub session_token: Option<String>,
    /// Use path-style addressing (`endpoint/bucket/key`).
    /// Required for MinIO and other non-AWS S3-compatible services.
    #[serde(default)]
    pub force_path_style: bool,
}

fn default_hash_length() -> usize {
    8
}

fn default_change_id_format() -> String {
    "short".to_string()
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            hash_length: default_hash_length(),
            change_id_format: default_change_id_format(),
        }
    }
}

impl Default for RepoConfig {
    fn default() -> Self {
        Self {
            repository: RepositoryConfig { version: 1 },
            principal: None,
            agent: AgentConfig::default(),
            worktree: WorktreeConfig::default(),
            defaults: DefaultsConfig::default(),
            output: OutputConfig::default(),
            policies: PoliciesConfig::default(),
            display: DisplayConfig::default(),
            storage: StorageConfig::default(),
            hosted: HostedConfig::default(),
            review: ReviewConfig::default(),
            redact: RedactConfig::default(),
        }
    }
}

impl RepoConfig {
    /// Load configuration from a file.
    pub fn load(path: &Path) -> Result<Self> {
        let mut file = std::fs::File::open(path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        // Wrap parse failures in `HeddleError::ConfigParse` so the CLI
        // error envelope can render the actual config file in the
        // recovery hint (Codex R3 cid 3313132711 on #271). The implicit
        // `From<toml::de::Error>` would lose the path; we attach it
        // here where we still know which file produced the failure.
        toml::from_str::<Self>(&contents).map_err(|err| {
            let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            objects::error::HeddleError::ConfigParse {
                path: resolved,
                message: err.to_string(),
            }
        })
    }

    /// Save configuration to a file.
    pub fn save(&self, path: &Path) -> Result<()> {
        let contents = toml::to_string_pretty(self)?;
        write_file_atomic(path, contents.as_bytes())?;
        Ok(())
    }

    /// Set the principal identity in configuration.
    pub fn set_principal(&mut self, name: impl Into<String>, email: impl Into<String>) {
        self.principal = Some(PrincipalConfig {
            name: name.into(),
            email: email.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_default_config_values() {
        let config = RepoConfig::default();

        assert_eq!(config.worktree.ignore, vec![".heddle".to_string()]);
        assert_eq!(config.worktree.fsmonitor.mode, crate::FsMonitorMode::Off);
        assert_eq!(config.output.format, None);
        assert!(config.policies.default_policy.is_none());
    }

    #[test]
    fn test_defaults_deserialize_when_missing() {
        let toml = r#"
[repository]
version = 1
"#;
        let config: RepoConfig = toml::from_str(toml).expect("config should deserialize");

        assert_eq!(config.repository.version, 1);
        assert_eq!(config.output.format, None);
        assert!(config.policies.default_policy.is_none());
        assert!(config.agent.provider.is_none());
        assert!(config.agent.model.is_none());
    }

    #[test]
    fn output_format_text_and_json_parse_normally() {
        let toml_text = r#"
[repository]
version = 1
[output]
format = "text"
"#;
        let config: RepoConfig = toml::from_str(toml_text).expect("text should parse");
        assert_eq!(config.output.format, Some(OutputFormat::Text));

        let toml_json = r#"
[repository]
version = 1
[output]
format = "json"
"#;
        let config: RepoConfig = toml::from_str(toml_json).expect("json should parse");
        assert_eq!(config.output.format, Some(OutputFormat::Json));
    }

    #[test]
    fn output_format_auto_is_rejected_with_field_specific_message() {
        // Pre-1.0: no silent alias. A repo config with `output.format =
        // "auto"` must fail loudly so the operator updates it to `text`
        // or `json` rather than silently inheriting the
        // JSON-when-piped surprise that motivated #271.
        let toml_auto = r#"
[repository]
version = 1
[output]
format = "auto"
"#;
        let err = toml::from_str::<RepoConfig>(toml_auto)
            .expect_err("output.format='auto' must reject");
        let message = err.to_string();
        assert!(
            message.contains("output.format"),
            "error should name the field: {message}"
        );
        assert!(
            message.contains("'auto'"),
            "error should name the rejected value: {message}"
        );
        assert!(
            message.contains("'text'") && message.contains("'json'"),
            "error should list the valid values: {message}"
        );
    }

    #[test]
    fn test_save_overwrites_atomically() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("config.toml");

        let mut config = RepoConfig::default();
        config.save(&path).unwrap();

        config.set_principal("Test User", "test@example.com");
        config.save(&path).unwrap();

        let loaded = RepoConfig::load(&path).unwrap();
        assert_eq!(
            loaded
                .principal
                .as_ref()
                .map(|principal| principal.name.as_str()),
            Some("Test User")
        );

        let sibling_entries = std::fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(sibling_entries, 0);
    }
}
