// SPDX-License-Identifier: Apache-2.0
//! Worktree status configuration and execution options.

use serde::{Deserialize, Serialize};

/// Optional fsmonitor backend selection for worktree status hot paths.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsMonitorMode {
    /// Disable fsmonitor integration.
    #[default]
    Off,
    /// Auto-detect a supported backend at runtime.
    Auto,
    /// Use Heddle's local native backend.
    Native,
    /// Use the Watchman CLI backend when available.
    Watchman,
}

impl FsMonitorMode {
    /// Parse an environment override value.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "0" | "off" | "false" | "disabled" => Some(Self::Off),
            "1" | "auto" | "true" | "enabled" => Some(Self::Auto),
            "native" | "local" => Some(Self::Native),
            "watchman" => Some(Self::Watchman),
            _ => None,
        }
    }
}

/// Serializable fsmonitor configuration stored in user or repo config.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsMonitorConfig {
    /// Backend selection mode.
    #[serde(default)]
    pub mode: FsMonitorMode,
}

/// Resolved runtime fsmonitor settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsMonitorSettings {
    /// Backend selection mode.
    pub mode: FsMonitorMode,
}

impl From<FsMonitorConfig> for FsMonitorSettings {
    fn from(config: FsMonitorConfig) -> Self {
        Self { mode: config.mode }
    }
}

/// Resolved options for worktree status operations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorktreeStatusOptions {
    /// Fsmonitor integration settings.
    pub fsmonitor: FsMonitorSettings,
}