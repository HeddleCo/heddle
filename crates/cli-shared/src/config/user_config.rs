// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use objects::fs_atomic::write_file_atomic;
use proto::AuthToken;
use repo::{FsMonitorMode, FsMonitorSettings, OutputFormat, WorktreeStatusOptions};
use serde::{Deserialize, Serialize};

use crate::client_config::ClientConfig;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserConfig {
    #[serde(default)]
    pub principal: Option<UserPrincipalConfig>,
    #[serde(default)]
    pub agent: UserAgentConfig,
    #[serde(default)]
    pub output: UserOutputConfig,
    #[serde(default)]
    pub display: UserDisplayConfig,
    #[serde(default)]
    pub worktree: UserWorktreeConfig,
    #[serde(default)]
    pub logging: UserLoggingConfig,
    #[serde(default)]
    pub remote: UserRemoteConfig,
    #[serde(default)]
    pub harness: UserHarnessConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPrincipalConfig {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserAgentConfig {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub default_policy: Option<String>,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserOutputConfig {
    #[serde(default)]
    pub format: OutputFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserDisplayConfig {
    #[serde(default = "default_hash_length")]
    pub hash_length: usize,
    #[serde(default = "default_change_id_format")]
    pub change_id_format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserWorktreeConfig {
    #[serde(default)]
    pub fsmonitor: UserFsMonitorConfig,
    #[serde(default)]
    pub thread_workspace: UserThreadWorkspaceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserFsMonitorConfig {
    #[serde(default)]
    pub mode: Option<FsMonitorMode>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum UserThreadWorkspaceMode {
    #[default]
    Auto,
    Heavy,
    Light,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserThreadWorkspaceConfig {
    #[serde(default)]
    pub top_level_default: UserThreadWorkspaceMode,
    #[serde(default)]
    pub delegated_default: Option<UserThreadWorkspaceMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserLoggingConfig {
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub include_location: bool,
    #[serde(default)]
    pub include_thread_ids: bool,
    #[serde(default)]
    pub log_spans: bool,
    #[serde(default)]
    pub otel_service_name: Option<String>,
    #[serde(default)]
    pub otel_endpoint: Option<String>,
    #[serde(default)]
    pub otel_traces_endpoint: Option<String>,
    #[serde(default)]
    pub otel_metrics_endpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserRemoteConfig {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub tls_enabled: bool,
    #[serde(default)]
    pub tls_domain_name: Option<String>,
    #[serde(default)]
    pub tls_ca_certificate_path: Option<PathBuf>,
    #[serde(default)]
    pub auth_proof_key_pem_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum HarnessMode {
    #[default]
    Auto,
    Off,
    Required,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum HarnessTransport {
    #[default]
    Spool,
    Direct,
    End,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum HarnessTranscriptMode {
    #[default]
    Off,
    Summary,
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserHarnessOverride {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub thinking_level: Option<String>,
    #[serde(default)]
    pub policy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserHarnessConfig {
    #[serde(default)]
    pub mode: HarnessMode,
    #[serde(default)]
    pub transport: HarnessTransport,
    #[serde(default)]
    pub transcript: HarnessTranscriptMode,
    #[serde(default = "default_auto_infer")]
    pub auto_infer: bool,
    #[serde(default)]
    pub threading: UserHarnessThreadingConfig,
    #[serde(default)]
    pub harnesses: BTreeMap<String, UserHarnessOverride>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum UserHarnessRootThreadPolicy {
    CreateNew,
    #[default]
    AttachCurrent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum UserHarnessSubagentThreadPolicy {
    AttachCurrent,
    #[default]
    CreateChild,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserHarnessThreadingConfig {
    #[serde(default)]
    pub root_actor: UserHarnessRootThreadPolicy,
    #[serde(default)]
    pub subagent: UserHarnessSubagentThreadPolicy,
    #[serde(default)]
    pub workspace_default: Option<UserThreadWorkspaceMode>,
}

fn default_confidence() -> f32 {
    0.8
}

fn default_hash_length() -> usize {
    8
}

fn default_change_id_format() -> String {
    "short".to_string()
}

fn default_auto_infer() -> bool {
    true
}

impl Default for UserDisplayConfig {
    fn default() -> Self {
        Self {
            hash_length: default_hash_length(),
            change_id_format: default_change_id_format(),
        }
    }
}

impl Default for UserHarnessConfig {
    fn default() -> Self {
        Self {
            mode: HarnessMode::Auto,
            transport: HarnessTransport::Spool,
            transcript: HarnessTranscriptMode::Off,
            auto_infer: default_auto_infer(),
            threading: UserHarnessThreadingConfig::default(),
            harnesses: BTreeMap::new(),
        }
    }
}

impl UserConfig {
    pub fn default_path() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("HEDDLE_CONFIG")
            && !path.is_empty()
        {
            return Some(PathBuf::from(path));
        }
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
            && !xdg.is_empty()
        {
            return Some(PathBuf::from(xdg).join("heddle").join("config.toml"));
        }
        if let Ok(home) = std::env::var("HOME")
            && !home.is_empty()
        {
            return Some(PathBuf::from(home).join(".config/heddle/config.toml"));
        }
        None
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut file = fs::File::open(path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Ok(toml::from_str(&contents)?)
    }

    pub fn load_default() -> anyhow::Result<Self> {
        match Self::default_path() {
            Some(path) => match Self::load(&path) {
                Ok(config) => Ok(config),
                Err(err) if path_missing(&err) => Ok(Self::default()),
                Err(err) => Err(err),
            },
            None => Ok(Self::default()),
        }
    }

    pub fn save_default(&self) -> anyhow::Result<PathBuf> {
        let path = Self::default_path()
            .ok_or_else(|| anyhow::anyhow!("unable to determine user config path"))?;
        self.save(&path)?;
        Ok(path)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        write_file_atomic(path, contents.as_bytes())?;
        Ok(())
    }

    pub fn set_principal(&mut self, name: impl Into<String>, email: impl Into<String>) {
        self.principal = Some(UserPrincipalConfig {
            name: name.into(),
            email: email.into(),
        });
    }

    pub fn remote_token(&self) -> Option<AuthToken> {
        std::env::var("HEDDLE_REMOTE_TOKEN")
            .ok()
            .filter(|token| !token.is_empty())
            .map(|token| AuthToken::new(token, "env"))
            .or_else(|| {
                self.remote
                    .token
                    .clone()
                    .map(|token| AuthToken::new(token, "user-config"))
            })
    }

    pub fn weft_client_config(&self, token_override: Option<AuthToken>) -> ClientConfig {
        let token = token_override.or_else(|| self.remote_token());
        let mut config = token
            .map(|token| ClientConfig::default().with_token(token))
            .unwrap_or_default();

        if self.remote.tls_enabled {
            config = config.with_tls(false);
        }
        if let Some(domain) = &self.remote.tls_domain_name {
            config = config.with_tls_domain_name(domain.clone());
        }
        if let Some(path) = &self.remote.tls_ca_certificate_path
            && let Ok(pem) = fs::read_to_string(path)
        {
            config = config.with_tls_ca_certificate_pem(pem);
        }
        if let Some(path) = &self.remote.auth_proof_key_pem_path
            && let Ok(pem) = fs::read_to_string(path)
        {
            config = config.with_auth_proof_key_pem(pem);
        }

        if std::env::var("HEDDLE_REMOTE_TLS")
            .ok()
            .is_some_and(|value| {
                matches!(
                    value.to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
        {
            config = config.with_tls(false);
        }
        if let Ok(domain) = std::env::var("HEDDLE_REMOTE_TLS_DOMAIN") {
            config = config.with_tls_domain_name(domain);
        }
        if let Ok(path) = std::env::var("HEDDLE_REMOTE_TLS_CA_CERT")
            && let Ok(pem) = fs::read_to_string(path)
        {
            config = config.with_tls_ca_certificate_pem(pem);
        }
        config
    }

    pub fn worktree_status_options(
        &self,
        repo_config: Option<&repo::RepoConfig>,
    ) -> WorktreeStatusOptions {
        let mut mode = self
            .worktree
            .fsmonitor
            .mode
            .or_else(|| repo_config.map(|config| config.worktree.fsmonitor.mode))
            .unwrap_or(FsMonitorMode::Off);
        if let Ok(value) = std::env::var("HEDDLE_FSMONITOR")
            && let Some(parsed) = FsMonitorMode::parse(&value)
        {
            mode = parsed;
        }

        WorktreeStatusOptions {
            fsmonitor: FsMonitorSettings { mode },
        }
    }
}

fn path_missing(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use repo::{FsMonitorMode, RepoConfig};

    use super::{HarnessMode, HarnessTranscriptMode, HarnessTransport, UserConfig};

    #[test]
    fn user_worktree_status_options_fall_back_to_repo_config() {
        let mut repo = RepoConfig::default();
        repo.worktree.fsmonitor.mode = FsMonitorMode::Watchman;

        let config = UserConfig::default();
        let options = config.worktree_status_options(Some(&repo));

        assert_eq!(options.fsmonitor.mode, FsMonitorMode::Watchman);
    }

    #[test]
    fn harness_config_defaults_are_magical_but_safe() {
        let config = UserConfig::default();
        assert_eq!(config.harness.mode, HarnessMode::Auto);
        assert_eq!(config.harness.transport, HarnessTransport::Spool);
        assert_eq!(config.harness.transcript, HarnessTranscriptMode::Off);
        assert!(config.harness.auto_infer);
        assert!(config.harness.harnesses.is_empty());
    }
}