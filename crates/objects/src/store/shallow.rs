// SPDX-License-Identifier: Apache-2.0
//! Shallow state management for incomplete history.
//!
//! Shallow clones have "grafted" states where parent history is not available.
//! This module threads which states are shallow and their grafted parents.

use std::{collections::HashSet, fs, path::Path};

use crate::{fs_atomic::write_file_atomic, object::StateId, store::Result};

/// Manages shallow state information.
pub struct ShallowInfo {
    path: std::path::PathBuf,
    shallow_states: HashSet<StateId>,
}

impl ShallowInfo {
    /// Load shallow info from a repository.
    pub fn load(heddle_dir: &Path) -> Result<Self> {
        let path = heddle_dir.join("shallow");
        let shallow_states = if path.exists() {
            let contents = fs::read_to_string(&path)?;
            contents
                .lines()
                .filter_map(|line| {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        None
                    } else {
                        StateId::parse(line).ok()
                    }
                })
                .collect()
        } else {
            HashSet::new()
        };

        Ok(Self {
            path,
            shallow_states,
        })
    }

    /// Check if a state is shallow (has grafted parents).
    pub fn is_shallow(&self, id: &StateId) -> bool {
        self.shallow_states.contains(id)
    }

    /// Get all shallow states.
    pub fn shallow_states(&self) -> &HashSet<StateId> {
        &self.shallow_states
    }

    /// Add a shallow state.
    pub fn add_shallow(&mut self, id: StateId) -> Result<()> {
        if self.shallow_states.insert(id) {
            self.save()?;
        }
        Ok(())
    }

    /// Remove a shallow state (when history is unshallowed).
    pub fn remove_shallow(&mut self, id: &StateId) -> Result<()> {
        if self.shallow_states.remove(id) {
            self.save()?;
        }
        Ok(())
    }

    /// Save shallow info to disk.
    fn save(&self) -> Result<()> {
        let contents: String = self
            .shallow_states
            .iter()
            .map(|id| format!("{}\n", id.to_string_full()))
            .collect();

        write_file_atomic(&self.path, contents.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_shallow_save_rewrites_file_without_temp_residue() {
        let temp_dir = TempDir::new().unwrap();
        let heddle_dir = temp_dir.path().join(".heddle");
        fs::create_dir_all(&heddle_dir).unwrap();

        let mut shallow = ShallowInfo::load(&heddle_dir).unwrap();
        let id = StateId::from_bytes([7; 32]);
        shallow.add_shallow(id).unwrap();

        let reloaded = ShallowInfo::load(&heddle_dir).unwrap();
        assert!(reloaded.is_shallow(&id));

        let temp_entries = fs::read_dir(&heddle_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(temp_entries, 0);
    }
}
