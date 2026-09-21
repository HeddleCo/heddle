// SPDX-License-Identifier: Apache-2.0
//! Worktree status configuration and execution options.

use serde::{Deserialize, Serialize};

use crate::repository::RepoConfig;

pub use objects::config_types::FsMonitorMode;

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

#[cfg(test)]
mod tests {
    use super::{FsMonitorConfig, FsMonitorMode, FsMonitorSettings, WorktreeStatusOptions};

    #[test]
    fn default_fsmonitor_mode_is_off() {
        assert_eq!(FsMonitorMode::default(), FsMonitorMode::Off);
        assert_eq!(FsMonitorConfig::default().mode, FsMonitorMode::Off);
        assert_eq!(FsMonitorSettings::default().mode, FsMonitorMode::Off);
        assert_eq!(
            WorktreeStatusOptions::default().fsmonitor.mode,
            FsMonitorMode::Off
        );
    }
}

/// Resolved options for worktree status operations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorktreeStatusOptions {
    /// Fsmonitor integration settings.
    pub fsmonitor: FsMonitorSettings,
}

/// Resolve worktree-status options from the user-config fsmonitor knob
/// (when the caller has a user config), repository config, and the
/// `HEDDLE_FSMONITOR` environment override. Precedence is environment,
/// then user knob, then repo config, then the fail-closed default.
pub fn resolve_worktree_status_options(
    user_mode: Option<FsMonitorMode>,
    repo_config: Option<&RepoConfig>,
) -> WorktreeStatusOptions {
    let mut mode = user_mode
        .or_else(|| repo_config.map(|config| config.worktree.fsmonitor.mode))
        .unwrap_or_default();
    if let Ok(value) = std::env::var("HEDDLE_FSMONITOR")
        && let Some(parsed) = FsMonitorMode::parse(&value)
    {
        mode = parsed;
    }

    WorktreeStatusOptions {
        fsmonitor: FsMonitorSettings { mode },
    }
}
