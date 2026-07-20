// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::BTreeMap,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

use objects::fs_atomic::{StagedAtomicWrite, stage_file_atomic_secret};
use repo::{FsMonitorMode, FsMonitorSettings, OutputFormat, WorktreeStatusOptions};
use serde::{Deserialize, Serialize};
use wire::AuthToken;

use crate::client_config::ClientConfig;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserConfig {
    #[serde(default)]
    pub principal: Option<UserPrincipalConfig>,
    #[serde(default)]
    pub agent: UserAgentConfig,
    #[serde(default)]
    pub capture: UserCaptureConfig,
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
    #[serde(default)]
    pub land: UserLandConfig,
}

pub struct StagedUserConfig {
    path: PathBuf,
    write: StagedAtomicWrite,
}

impl StagedUserConfig {
    pub fn publish(self) -> anyhow::Result<PathBuf> {
        self.write.publish()?;
        Ok(self.path)
    }
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
pub struct UserCaptureConfig {
    #[serde(default)]
    pub auto: UserAutoCaptureMode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum UserAutoCaptureMode {
    #[default]
    Off,
    Command,
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

/// User-config default for thread workspace mode. Same vocabulary
/// as [`crate::config::repo::ThreadMode`] and the `--workspace` flag,
/// so a user setting `top_level_default = "materialized"` reads
/// uniformly across the CLI surface and the thread record on disk.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum UserThreadWorkspaceMode {
    #[default]
    Auto,
    Materialized,
    Virtualized,
    Solid,
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
    #[serde(default)]
    pub iroh_descriptor_key_id: Option<String>,
    #[serde(default)]
    pub iroh_descriptor_public_key_path: Option<PathBuf>,
    /// Allow cleartext connections to non-loopback hosts without TLS.
    /// Prefer enabling TLS; this is an explicit opt-in for lab/VPN testing.
    #[serde(default)]
    pub insecure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserLandConfig {
    #[serde(default = "default_land_squash")]
    pub squash: bool,
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

fn default_land_squash() -> bool {
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

impl Default for UserLandConfig {
    fn default() -> Self {
        Self {
            squash: default_land_squash(),
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
        let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(value) = invalid_output_format_value(&contents) {
            return Err(objects::error::HeddleError::ConfigInvalidValue {
                path: resolved,
                key: "output.format".to_string(),
                value,
                valid_values: vec!["'text'".to_string(), "'json'".to_string()],
            }
            .into());
        }
        // Route TOML parse failures through `HeddleError::ConfigParse` so
        // the CLI error envelope (see `print_error_with_hint`) can
        // classify them and render the *actual* source file in the
        // recovery advice — not a hard-coded `.heddle/config.toml`
        // (Codex R3 cid 3313132711 on #271). The path is canonicalized
        // so the rendered hint is copy/paste-safe even when the caller
        // passed a relative or env-derived path.
        toml::from_str::<Self>(&contents).map_err(|err| {
            objects::error::HeddleError::ConfigParse {
                path: resolved,
                source: err,
            }
            .into()
        })
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
        self.stage_default()?.publish()
    }

    pub fn stage_default(&self) -> anyhow::Result<StagedUserConfig> {
        let path = Self::default_path()
            .ok_or_else(|| anyhow::anyhow!("unable to determine user config path"))?;
        self.stage(&path)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        self.stage(path)?.publish()?;
        Ok(())
    }

    pub fn stage(&self, path: &Path) -> anyhow::Result<StagedUserConfig> {
        let contents = toml::to_string_pretty(self)?;
        let write = stage_file_atomic_secret(path, contents.as_bytes())?;
        Ok(StagedUserConfig {
            path: path.to_path_buf(),
            write,
        })
    }

    pub fn set_principal(&mut self, name: impl Into<String>, email: impl Into<String>) {
        self.principal = Some(UserPrincipalConfig {
            name: name.into(),
            email: email.into(),
        });
    }

    pub fn remote_token(&self) -> anyhow::Result<Option<AuthToken>> {
        match env::var("HEDDLE_REMOTE_TOKEN") {
            Ok(token) if !token.is_empty() => Ok(Some(AuthToken::new(token, "env"))),
            Ok(_) | Err(env::VarError::NotPresent) => Ok(self
                .remote
                .token
                .clone()
                .map(|token| AuthToken::new(token, "user-config"))),
            Err(err @ env::VarError::NotUnicode(_)) => Err(security_config_error(
                "HEDDLE_REMOTE_TOKEN",
                format!("read environment value: {err}"),
            )),
        }
    }

    pub fn command_auto_capture_enabled(&self) -> anyhow::Result<bool> {
        let mut mode = self.capture.auto;
        match env::var("HEDDLE_AUTO_CAPTURE") {
            Ok(value) if !value.trim().is_empty() => {
                mode = parse_auto_capture_env("HEDDLE_AUTO_CAPTURE", &value)?;
            }
            Ok(_) | Err(env::VarError::NotPresent) => {}
            Err(err @ env::VarError::NotUnicode(_)) => {
                return Err(config_value_error(
                    "HEDDLE_AUTO_CAPTURE",
                    format!("read environment value: {err}"),
                ));
            }
        }
        Ok(matches!(mode, UserAutoCaptureMode::Command))
    }

    pub fn heddle_client_config(
        &self,
        token_override: Option<AuthToken>,
    ) -> anyhow::Result<ClientConfig> {
        let token = match token_override {
            Some(token) => Some(token),
            None => self.remote_token()?,
        };
        let mut config = token
            .map(|token| ClientConfig::default().with_token(token))
            .unwrap_or_default();

        if self.remote.tls_enabled {
            config = config.with_tls(false);
        }
        if self.remote.insecure {
            config = config.with_allow_insecure(true);
        }
        if let Some(domain) = &self.remote.tls_domain_name {
            config = config.with_tls_domain_name(domain.clone());
        }
        if let Some(path) = &self.remote.tls_ca_certificate_path {
            let pem = read_security_config_file("remote.tls_ca_certificate_path", path)?;
            config = config.with_tls_ca_certificate_pem(pem);
        }
        if let Some(path) = &self.remote.auth_proof_key_pem_path {
            let pem = read_security_config_file("remote.auth_proof_key_pem_path", path)?;
            config = config.with_auth_proof_key_pem(pem);
        }
        if let (Some(key_id), Some(path)) = (
            self.remote.iroh_descriptor_key_id.as_deref(),
            self.remote.iroh_descriptor_public_key_path.as_deref(),
        ) {
            config = config.with_descriptor_trust(
                key_id,
                read_descriptor_public_key("remote.iroh_descriptor_public_key_path", path)?,
            );
        } else if self.remote.iroh_descriptor_key_id.is_some()
            || self.remote.iroh_descriptor_public_key_path.is_some()
        {
            return Err(security_config_error(
                "remote.iroh_descriptor_key_id/remote.iroh_descriptor_public_key_path",
                "both descriptor trust fields are required".to_string(),
            ));
        }

        if env_bool("HEDDLE_REMOTE_TLS")? {
            config = config.with_tls(false);
        }
        if env_bool("HEDDLE_REMOTE_INSECURE")? {
            config = config.with_allow_insecure(true);
        }
        match env::var("HEDDLE_REMOTE_TLS_DOMAIN") {
            Ok(domain) => config = config.with_tls_domain_name(domain),
            Err(env::VarError::NotPresent) => {}
            Err(err @ env::VarError::NotUnicode(_)) => {
                return Err(security_config_error(
                    "HEDDLE_REMOTE_TLS_DOMAIN",
                    format!("read environment value: {err}"),
                ));
            }
        }
        match env::var("HEDDLE_REMOTE_TLS_CA_CERT") {
            Ok(path) => {
                let pem =
                    read_security_config_file("HEDDLE_REMOTE_TLS_CA_CERT", &PathBuf::from(path))?;
                config = config.with_tls_ca_certificate_pem(pem);
            }
            Err(env::VarError::NotPresent) => {}
            Err(err @ env::VarError::NotUnicode(_)) => {
                return Err(security_config_error(
                    "HEDDLE_REMOTE_TLS_CA_CERT",
                    format!("read environment value: {err}"),
                ));
            }
        }
        match (
            env::var("HEDDLE_REMOTE_IROH_DESCRIPTOR_KEY_ID"),
            env::var("HEDDLE_REMOTE_IROH_DESCRIPTOR_PUBLIC_KEY"),
        ) {
            (Ok(key_id), Ok(public_key)) if !key_id.is_empty() && !public_key.is_empty() => {
                config = config.with_descriptor_trust(
                    key_id,
                    parse_descriptor_public_key(
                        "HEDDLE_REMOTE_IROH_DESCRIPTOR_PUBLIC_KEY",
                        &public_key,
                    )?,
                );
            }
            (Err(env::VarError::NotPresent), Err(env::VarError::NotPresent)) => {}
            (Ok(_), Err(env::VarError::NotPresent))
            | (Err(env::VarError::NotPresent), Ok(_))
            | (Ok(_), Ok(_)) => {
                return Err(security_config_error(
                    "HEDDLE_REMOTE_IROH_DESCRIPTOR_KEY_ID/HEDDLE_REMOTE_IROH_DESCRIPTOR_PUBLIC_KEY",
                    "both non-empty descriptor trust values are required".to_string(),
                ));
            }
            (Err(error), _) | (_, Err(error)) => {
                return Err(security_config_error(
                    "HEDDLE_REMOTE_IROH_DESCRIPTOR_KEY_ID/HEDDLE_REMOTE_IROH_DESCRIPTOR_PUBLIC_KEY",
                    format!("read environment value: {error}"),
                ));
            }
        }
        Ok(config)
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

fn parse_auto_capture_env(setting: &str, value: &str) -> anyhow::Result<UserAutoCaptureMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "command" | "commands" => Ok(UserAutoCaptureMode::Command),
        "0" | "false" | "no" | "off" => Ok(UserAutoCaptureMode::Off),
        _ => Err(config_value_error(
            setting,
            format!(
                "parse auto-capture value {value:?}; expected one of off, command, true, or false"
            ),
        )),
    }
}

fn invalid_output_format_value(contents: &str) -> Option<String> {
    let value = toml::from_str::<toml::Value>(contents).ok()?;
    let format = value
        .get("output")
        .and_then(|output| output.get("format"))
        .and_then(toml::Value::as_str)?;
    (!matches!(format, "text" | "json")).then(|| format.to_string())
}

fn read_security_config_file(setting: &str, path: &Path) -> anyhow::Result<String> {
    fs::read_to_string(path).map_err(|err| {
        security_config_error(
            setting,
            format!("read configured file {}: {err}", path.display()),
        )
    })
}

fn read_descriptor_public_key(setting: &str, path: &Path) -> anyhow::Result<[u8; 32]> {
    let value = read_security_config_file(setting, path)?;
    parse_descriptor_public_key(setting, value.trim())
}

fn parse_descriptor_public_key(setting: &str, value: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = hex::decode(value).map_err(|error| {
        security_config_error(setting, format!("decode hex descriptor key: {error}"))
    })?;
    bytes.try_into().map_err(|_| {
        security_config_error(
            setting,
            "descriptor key must be a 32-byte Ed25519 public key".to_string(),
        )
    })
}

fn env_bool(name: &str) -> anyhow::Result<bool> {
    let value = match env::var(name) {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(false),
        Err(err @ env::VarError::NotUnicode(_)) => {
            return Err(security_config_error(
                name,
                format!("read environment value: {err}"),
            ));
        }
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(security_config_error(
            name,
            format!(
                "parse boolean value {value:?}; expected one of 1/0, true/false, yes/no, or on/off"
            ),
        )),
    }
}

fn config_value_error(setting: &str, reason: String) -> anyhow::Error {
    anyhow::anyhow!("fatal configuration error for `{setting}`: {reason}")
}

fn security_config_error(setting: &str, reason: String) -> anyhow::Error {
    anyhow::anyhow!(
        "fatal TLS/auth configuration error for `{setting}`: {reason}; refusing to proceed with an ambiguous security posture"
    )
}

fn path_missing(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs,
        path::PathBuf,
        sync::MutexGuard,
        time::{SystemTime, UNIX_EPOCH},
    };

    use repo::{FsMonitorMode, RepoConfig};

    use super::{
        HarnessMode, HarnessTranscriptMode, HarnessTransport, UserAutoCaptureMode,
        UserCaptureConfig, UserConfig, UserRemoteConfig,
    };

    static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    const REMOTE_ENV_KEYS: &[&str] = &[
        "HEDDLE_REMOTE_TOKEN",
        "HEDDLE_REMOTE_TLS",
        "HEDDLE_REMOTE_TLS_DOMAIN",
        "HEDDLE_REMOTE_TLS_CA_CERT",
        "HEDDLE_REMOTE_INSECURE",
        "HEDDLE_REMOTE_IROH_DESCRIPTOR_KEY_ID",
        "HEDDLE_REMOTE_IROH_DESCRIPTOR_PUBLIC_KEY",
        "HEDDLE_AUTO_CAPTURE",
    ];

    struct RemoteEnvGuard {
        _guard: MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl RemoteEnvGuard {
        fn clean() -> Self {
            let guard = TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let saved = REMOTE_ENV_KEYS
                .iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect();
            for key in REMOTE_ENV_KEYS {
                unsafe { std::env::remove_var(key) };
            }
            Self {
                _guard: guard,
                saved,
            }
        }

        fn set(&self, key: &str, value: impl AsRef<std::ffi::OsStr>) {
            unsafe { std::env::set_var(key, value) };
        }
    }

    impl Drop for RemoteEnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                unsafe {
                    if let Some(value) = value {
                        std::env::set_var(key, value);
                    } else {
                        std::env::remove_var(key);
                    }
                }
            }
        }
    }

    fn unique_temp_path(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{unique}", std::process::id()))
    }

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

    #[test]
    fn command_auto_capture_defaults_off() {
        let _env = RemoteEnvGuard::clean();

        let config = UserConfig::default();

        assert!(!config.command_auto_capture_enabled().unwrap());
    }

    #[test]
    fn command_auto_capture_reads_user_config() {
        let _env = RemoteEnvGuard::clean();
        let config = UserConfig {
            capture: UserCaptureConfig {
                auto: UserAutoCaptureMode::Command,
            },
            ..UserConfig::default()
        };

        assert!(config.command_auto_capture_enabled().unwrap());
    }

    #[test]
    fn command_auto_capture_env_overrides_user_config() {
        let env = RemoteEnvGuard::clean();
        env.set("HEDDLE_AUTO_CAPTURE", "off");
        let config = UserConfig {
            capture: UserCaptureConfig {
                auto: UserAutoCaptureMode::Command,
            },
            ..UserConfig::default()
        };

        assert!(!config.command_auto_capture_enabled().unwrap());

        env.set("HEDDLE_AUTO_CAPTURE", "command");
        assert!(
            UserConfig::default()
                .command_auto_capture_enabled()
                .unwrap()
        );
    }

    #[test]
    fn user_config_toml_parses_capture_auto_command() {
        let parsed: UserConfig = toml::from_str(
            r#"
                [capture]
                auto = "command"
            "#,
        )
        .expect("capture auto config should parse");

        assert_eq!(parsed.capture.auto, UserAutoCaptureMode::Command);
    }

    #[test]
    fn heddle_client_config_absent_security_settings_uses_defaults() {
        let _env = RemoteEnvGuard::clean();
        let config = UserConfig::default()
            .heddle_client_config(None)
            .expect("absent optional settings should not error");

        assert!(!config.tls_enabled);
        assert!(!config.tls_skip_verify);
        assert!(config.tls_ca_certificate_pem.is_none());
        assert!(config.auth_proof_key_pem.is_none());
        assert!(config.token.is_none());
        assert!(config.descriptor_key_id.is_none());
        assert!(config.descriptor_public_key.is_none());
    }

    #[test]
    fn heddle_client_config_loads_descriptor_trust_from_environment() {
        let env = RemoteEnvGuard::clean();
        env.set("HEDDLE_REMOTE_IROH_DESCRIPTOR_KEY_ID", "weft-current");
        env.set(
            "HEDDLE_REMOTE_IROH_DESCRIPTOR_PUBLIC_KEY",
            hex::encode([17; 32]),
        );

        let config = UserConfig::default().heddle_client_config(None).unwrap();

        assert_eq!(config.descriptor_key_id.as_deref(), Some("weft-current"));
        assert_eq!(config.descriptor_public_key, Some([17; 32]));
    }

    #[test]
    fn heddle_client_config_rejects_partial_descriptor_trust() {
        let env = RemoteEnvGuard::clean();
        env.set("HEDDLE_REMOTE_IROH_DESCRIPTOR_KEY_ID", "weft-current");

        let error = UserConfig::default()
            .heddle_client_config(None)
            .expect_err("partial descriptor trust must fail closed");

        assert!(
            error
                .to_string()
                .contains("both non-empty descriptor trust")
        );
    }

    #[test]
    fn heddle_client_config_valid_security_files_are_applied() {
        let _env = RemoteEnvGuard::clean();
        let dir = unique_temp_path("heddle-user-config-valid-security");
        fs::create_dir_all(&dir).expect("create temp dir");
        let ca_path = dir.join("ca.pem");
        let key_path = dir.join("proof-key.pem");
        fs::write(&ca_path, "test ca pem").expect("write ca pem");
        fs::write(&key_path, "test key pem").expect("write key pem");
        let user = UserConfig {
            remote: UserRemoteConfig {
                tls_ca_certificate_path: Some(ca_path),
                auth_proof_key_pem_path: Some(key_path),
                ..UserRemoteConfig::default()
            },
            ..UserConfig::default()
        };

        let config = user
            .heddle_client_config(None)
            .expect("valid TLS/auth files should load");

        assert!(config.tls_enabled);
        assert_eq!(
            config.tls_ca_certificate_pem.as_deref(),
            Some("test ca pem")
        );
        assert_eq!(config.auth_proof_key_pem.as_deref(), Some("test key pem"));

        fs::remove_dir_all(dir).expect("remove temp dir");
    }

    #[test]
    fn heddle_client_config_missing_tls_ca_path_fails_closed() {
        let _env = RemoteEnvGuard::clean();
        let missing = unique_temp_path("heddle-user-config-missing-ca").join("ca.pem");
        let user = UserConfig {
            remote: UserRemoteConfig {
                tls_ca_certificate_path: Some(missing),
                ..UserRemoteConfig::default()
            },
            ..UserConfig::default()
        };

        let err = user
            .heddle_client_config(None)
            .expect_err("missing configured CA path must fail closed");
        let message = err.to_string();

        assert!(message.contains("fatal TLS/auth configuration error"));
        assert!(message.contains("remote.tls_ca_certificate_path"));
    }

    #[test]
    fn heddle_client_config_missing_auth_proof_key_path_fails_closed() {
        let _env = RemoteEnvGuard::clean();
        let missing = unique_temp_path("heddle-user-config-missing-key").join("proof-key.pem");
        let user = UserConfig {
            remote: UserRemoteConfig {
                auth_proof_key_pem_path: Some(missing),
                ..UserRemoteConfig::default()
            },
            ..UserConfig::default()
        };

        let err = user
            .heddle_client_config(None)
            .expect_err("missing configured proof key path must fail closed");
        let message = err.to_string();

        assert!(message.contains("fatal TLS/auth configuration error"));
        assert!(message.contains("remote.auth_proof_key_pem_path"));
    }

    #[test]
    fn heddle_client_config_missing_env_tls_ca_path_fails_closed() {
        let env = RemoteEnvGuard::clean();
        let missing = unique_temp_path("heddle-user-config-missing-env-ca").join("ca.pem");
        env.set("HEDDLE_REMOTE_TLS_CA_CERT", missing);

        let err = UserConfig::default()
            .heddle_client_config(None)
            .expect_err("missing env CA path must fail closed");
        let message = err.to_string();

        assert!(message.contains("fatal TLS/auth configuration error"));
        assert!(message.contains("HEDDLE_REMOTE_TLS_CA_CERT"));
    }

    #[test]
    fn heddle_client_config_invalid_env_tls_value_fails_closed() {
        let env = RemoteEnvGuard::clean();
        env.set("HEDDLE_REMOTE_TLS", "enabled");

        let err = UserConfig::default()
            .heddle_client_config(None)
            .expect_err("invalid TLS env value must fail closed");
        let message = err.to_string();

        assert!(message.contains("fatal TLS/auth configuration error"));
        assert!(message.contains("HEDDLE_REMOTE_TLS"));
    }
}
