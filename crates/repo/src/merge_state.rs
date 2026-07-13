// SPDX-License-Identifier: Apache-2.0
//! Merge state tracking for conflict resolution.

use std::{fs, path::PathBuf};

use objects::{fs_atomic::write_file_atomic, lock::RepoLock, object::StateId};
use serde::{Deserialize, Serialize};

use crate::{Repository, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeState {
    pub ours: StateId,
    pub theirs: StateId,
    pub base: Option<StateId>,
    pub conflicts: Vec<String>,
    pub resolved: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_root: Option<PathBuf>,
}

pub struct MergeStateManager {
    merge_state_path: PathBuf,
    lock: RepoLock,
    worktree_root: Option<PathBuf>,
}

impl MergeStateManager {
    pub fn new(heddle_dir: impl AsRef<std::path::Path>) -> Self {
        let heddle_dir = heddle_dir.as_ref();
        Self {
            merge_state_path: heddle_dir.join("MERGE_STATE"),
            lock: RepoLock::at(heddle_dir.join("locks/merge_state.lock")),
            worktree_root: None,
        }
    }

    pub fn new_for_worktree(
        heddle_dir: impl AsRef<std::path::Path>,
        worktree_root: impl Into<PathBuf>,
    ) -> Self {
        let heddle_dir = heddle_dir.as_ref();
        Self {
            merge_state_path: heddle_dir.join("MERGE_STATE"),
            lock: RepoLock::at(heddle_dir.join("locks/merge_state.lock")),
            worktree_root: Some(worktree_root.into()),
        }
    }

    pub fn start(
        &self,
        ours: StateId,
        theirs: StateId,
        base: Option<StateId>,
        conflicts: Vec<String>,
    ) -> Result<()> {
        let _lock = self.write_lock()?;
        let state = MergeState {
            ours,
            theirs,
            base,
            conflicts,
            resolved: Vec::new(),
            worktree_root: self.worktree_root.clone(),
        };
        self.write_state(&state)?;
        Ok(())
    }

    pub fn load(&self) -> Result<Option<MergeState>> {
        let _lock = self.read_lock()?;
        self.load_unlocked_for_worktree()
    }

    pub fn resolve(&self, path: &str) -> Result<()> {
        let _lock = self.write_lock()?;
        let mut state = self
            .load_unlocked_for_worktree()?
            .ok_or(crate::HeddleError::NoMergeInProgress)?;

        if state.conflicts.iter().any(|conflict| conflict == path)
            && state.resolved.iter().all(|resolved| resolved != path)
        {
            state.resolved.push(path.to_string());
        }

        self.write_state(&state)?;
        Ok(())
    }

    pub fn resolve_all(&self) -> Result<Vec<String>> {
        let _lock = self.write_lock()?;
        let mut state = self
            .load_unlocked_for_worktree()?
            .ok_or(crate::HeddleError::NoMergeInProgress)?;

        let newly_resolved: Vec<String> = state
            .conflicts
            .iter()
            .filter(|c| !state.resolved.contains(c))
            .cloned()
            .collect();

        state.resolved = state.conflicts.clone();

        self.write_state(&state)?;
        Ok(newly_resolved)
    }

    pub fn unresolved(&self) -> Result<Vec<String>> {
        let _lock = self.read_lock()?;
        let state = self
            .load_unlocked_for_worktree()?
            .ok_or(crate::HeddleError::NoMergeInProgress)?;

        Ok(state
            .conflicts
            .iter()
            .filter(|c| !state.resolved.contains(c))
            .cloned()
            .collect())
    }

    pub fn abort(&self) -> Result<MergeState> {
        let _lock = self.write_lock()?;
        let state = self
            .load_unlocked_for_worktree()?
            .ok_or(crate::HeddleError::NoMergeInProgress)?;

        if !self.merge_state_path.exists() {
            return Ok(state);
        }

        fs::remove_file(&self.merge_state_path)?;
        Ok(state)
    }

    pub fn finish(&self) -> Result<MergeState> {
        let _lock = self.write_lock()?;
        let state = self
            .load_unlocked_for_worktree()?
            .ok_or(crate::HeddleError::NoMergeInProgress)?;

        let unresolved: Vec<_> = state
            .conflicts
            .iter()
            .filter(|c| !state.resolved.contains(c))
            .collect();

        if !unresolved.is_empty() {
            let unresolved_strs: Vec<&str> = unresolved.iter().map(|s| s.as_str()).collect();
            return Err(crate::HeddleError::Conflict(format!(
                "Unresolved conflicts: {}",
                unresolved_strs.join(", ")
            )));
        }

        if self.merge_state_path.exists() {
            fs::remove_file(&self.merge_state_path)?;
        }

        Ok(state)
    }

    pub fn is_merge_in_progress(&self) -> bool {
        self.load().is_ok_and(|state| state.is_some())
    }

    /// Re-point `ours` at a new state without ending the merge. Used by
    /// stack-aware workflows that checkpoint mid-merge: the merge stays
    /// alive, but its "our" side now reflects the just-committed
    /// checkpoint so subsequent `--ours` resolves pull from the latest
    /// WIP rather than the pre-merge baseline. `theirs`, `base`,
    /// `conflicts`, and `resolved` are preserved untouched.
    pub fn carry_forward(&self, new_ours: StateId) -> Result<MergeState> {
        let _lock = self.write_lock()?;
        let mut state = self
            .load_unlocked()?
            .ok_or(crate::HeddleError::NoMergeInProgress)?;
        state.ours = new_ours;
        self.write_state(&state)?;
        Ok(state)
    }

    fn load_unlocked(&self) -> Result<Option<MergeState>> {
        if !self.merge_state_path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&self.merge_state_path)?;
        let state: MergeState = serde_json::from_str(&content)?;
        Ok(Some(state))
    }

    fn load_unlocked_for_worktree(&self) -> Result<Option<MergeState>> {
        let Some(state) = self.load_unlocked()? else {
            return Ok(None);
        };
        if self.state_belongs_to_worktree(&state) {
            Ok(Some(state))
        } else {
            Ok(None)
        }
    }

    fn state_belongs_to_worktree(&self, state: &MergeState) -> bool {
        match (&self.worktree_root, &state.worktree_root) {
            (_, None) | (None, _) => true,
            (Some(current), Some(recorded)) => current == recorded,
        }
    }

    fn write_state(&self, state: &MergeState) -> Result<()> {
        let content = serde_json::to_vec(state)?;
        write_file_atomic(&self.merge_state_path, &content)?;
        Ok(())
    }

    fn read_lock(&self) -> Result<objects::lock::ReadLockGuard> {
        self.lock
            .read()
            .map_err(|e| crate::HeddleError::Io(std::io::Error::other(e.to_string())))
    }

    fn write_lock(&self) -> Result<objects::lock::WriteLockGuard> {
        self.lock
            .write()
            .map_err(|e| crate::HeddleError::Io(std::io::Error::other(e.to_string())))
    }
}

impl Repository {
    pub fn merge_state_manager(&self) -> MergeStateManager {
        MergeStateManager::new_for_worktree(self.heddle_dir(), self.root().to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn create_manager() -> (TempDir, MergeStateManager) {
        let temp = TempDir::new().unwrap();
        let heddle_dir = temp.path().join(".heddle");
        std::fs::create_dir_all(&heddle_dir).unwrap();
        (temp, MergeStateManager::new(&heddle_dir))
    }

    fn sample_state_ids() -> (StateId, StateId, StateId) {
        (
            crate::test_state_id(),
            crate::test_state_id(),
            crate::test_state_id(),
        )
    }

    #[test]
    fn start_and_resolve_persist_state_atomically() {
        let (_temp, manager) = create_manager();
        let (ours, theirs, base) = sample_state_ids();

        manager
            .start(
                ours,
                theirs,
                Some(base),
                vec!["a.txt".to_string(), "b.txt".to_string()],
            )
            .unwrap();
        manager.resolve("a.txt").unwrap();

        let state = manager.load().unwrap().unwrap();
        assert_eq!(state.ours, ours);
        assert_eq!(state.theirs, theirs);
        assert_eq!(state.base, Some(base));
        assert_eq!(state.resolved, vec!["a.txt".to_string()]);
    }

    #[test]
    fn resolve_all_marks_everything_resolved() {
        let (_temp, manager) = create_manager();
        let (ours, theirs, _base) = sample_state_ids();

        manager
            .start(
                ours,
                theirs,
                None,
                vec!["a.txt".to_string(), "b.txt".to_string()],
            )
            .unwrap();
        let newly_resolved = manager.resolve_all().unwrap();

        assert_eq!(
            newly_resolved,
            vec!["a.txt".to_string(), "b.txt".to_string()]
        );
        assert!(manager.unresolved().unwrap().is_empty());
    }
}
