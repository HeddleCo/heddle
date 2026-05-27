// SPDX-License-Identifier: Apache-2.0
//! Per-worktree runtime state for Heddle.

use std::path::Path;

use objects::fs_atomic::write_file_atomic;
use serde::{Deserialize, Serialize};

use super::Result;

/// Runtime state that belongs to a specific checkout/worktree, not repo config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorktreeState {
    #[serde(default)]
    pub current_session_id: Option<String>,
    #[serde(default)]
    pub current_segment_id: Option<String>,
}

impl WorktreeState {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&contents)?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let contents = toml::to_string_pretty(self)?;
        write_file_atomic(path, contents.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_default_state_when_missing() {
        let temp = TempDir::new().unwrap();
        let state = WorktreeState::load(&temp.path().join("state.toml")).unwrap();

        assert!(state.current_session_id.is_none());
        assert!(state.current_segment_id.is_none());
    }

    #[test]
    fn test_save_overwrites_atomically() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("runtime/worktree.toml");

        let state = WorktreeState {
            current_session_id: Some("sess-123".to_string()),
            current_segment_id: Some("seg-456".to_string()),
        };
        state.save(&path).unwrap();

        let loaded = WorktreeState::load(&path).unwrap();
        assert_eq!(loaded.current_session_id.as_deref(), Some("sess-123"));
        assert_eq!(loaded.current_segment_id.as_deref(), Some("seg-456"));

        let sibling_entries = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(sibling_entries, 0);
    }
}
