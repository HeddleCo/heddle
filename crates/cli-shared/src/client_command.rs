// SPDX-License-Identifier: Apache-2.0
//! Shared client-command context and recovery advice.

use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use repo::{Config, OutputFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientOutputOverride {
    Json,
    Text,
}

impl ClientOutputOverride {
    fn as_format(self) -> OutputFormat {
        match self {
            Self::Json => OutputFormat::Json,
            Self::Text => OutputFormat::Text,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClientCommandContext {
    repo_path: Option<PathBuf>,
    operation_id_wire: String,
    user_output_format: OutputFormat,
    output_override: Option<ClientOutputOverride>,
}

impl ClientCommandContext {
    pub fn new(
        repo_path: Option<PathBuf>,
        operation_id_wire: impl Into<String>,
        user_output_format: OutputFormat,
        output_override: Option<ClientOutputOverride>,
    ) -> Self {
        Self {
            repo_path,
            operation_id_wire: operation_id_wire.into(),
            user_output_format,
            output_override,
        }
    }

    pub fn repo_path(&self) -> Option<&Path> {
        self.repo_path.as_deref()
    }

    pub fn operation_id_wire(&self) -> &str {
        &self.operation_id_wire
    }

    pub fn should_output_json(&self, repo_config: Option<&Config>) -> bool {
        let mut format = repo_config
            .and_then(|cfg| cfg.output.format)
            .unwrap_or(self.user_output_format);
        if let Some(output_override) = self.output_override {
            format = output_override.as_format();
        }
        matches!(format, OutputFormat::Json)
    }
}

/// Typed remote-command recovery advice that the CLI can render as the same
/// JSON/text envelope used by native commands without depending on client
/// implementation details.
#[derive(Debug, Clone)]
pub struct RemoteRecoveryAdvice {
    pub kind: &'static str,
    pub error: String,
    pub hint: String,
    pub unsafe_condition: String,
    pub would_change: String,
    pub preserved: String,
    pub primary_command: String,
    pub recovery_commands: Vec<String>,
}

impl RemoteRecoveryAdvice {
    pub fn invalid_usage(
        kind: &'static str,
        error: impl Into<String>,
        hint: impl Into<String>,
        primary_command: impl Into<String>,
    ) -> Self {
        let primary_command = primary_command.into();
        Self {
            kind,
            error: error.into(),
            hint: hint.into(),
            unsafe_condition: "the command arguments do not describe a valid remote operation"
                .to_string(),
            would_change:
                "running with ambiguous or invalid arguments could target the wrong remote resource"
                    .to_string(),
            preserved: "no remote request was sent and local repository state was left unchanged"
                .to_string(),
            primary_command: primary_command.clone(),
            recovery_commands: vec![primary_command],
        }
    }

    pub fn auth_required(server: &str) -> Self {
        let primary_command = format!("heddle auth login --server {server}");
        Self {
            kind: "auth_required",
            error: format!("Not authenticated with {server}"),
            hint: format!(
                "Run `{primary_command}` to authenticate, then retry the remote command."
            ),
            unsafe_condition: "no usable remote credential is available for the selected server"
                .to_string(),
            would_change:
                "continuing without credentials would send an unauthenticated remote mutation"
                    .to_string(),
            preserved: "no remote request was sent and local repository state was left unchanged"
                .to_string(),
            primary_command: primary_command.clone(),
            recovery_commands: vec![primary_command],
        }
    }

    pub fn remote_required(remote: &str, feature: &str) -> Self {
        Self {
            kind: "remote_required",
            error: format!("{feature} requires a network remote; remote '{remote}' is local"),
            hint: "Configure a network remote or retry against one that resolves to a network target."
                .to_string(),
            unsafe_condition: format!(
                "remote '{remote}' is local, but {feature} runs on the remote service"
            ),
            would_change:
                "running locally would imply a remote policy or support change that no service recorded"
                    .to_string(),
            preserved: "no network request was sent and local repository state was left unchanged"
                .to_string(),
            primary_command: "heddle remote list".to_string(),
            recovery_commands: vec!["heddle remote list".to_string()],
        }
    }
}

impl fmt::Display for RemoteRecoveryAdvice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}. Unsafe: {}. Would change: {}. Preserved: {}. Primary recovery: `{}`.",
            self.error,
            self.unsafe_condition,
            self.would_change,
            self.preserved,
            self.primary_command
        )
    }
}

impl Error for RemoteRecoveryAdvice {}
