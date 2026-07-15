// SPDX-License-Identifier: Apache-2.0
//! Recovery worktree materialization that preserves repository refs.

use objects::{lock::RepositoryLockExt, store::ObjectStore};

use super::{
    HeddleError, Repository, Result, repository_worktree_apply::WorktreeApplyDirtyBehavior,
};

impl Repository {
    /// Materialize a saved state as recoverable worktree changes.
    ///
    /// The current HEAD and attached thread remain unchanged. The resulting
    /// tree is intentionally dirty relative to that tip so a later capture can
    /// preserve it as new history.
    pub fn restore_state_tree_to_worktree(&self, target: &objects::object::StateId) -> Result<()> {
        let _lock = self
            .locker()
            .write()
            .map_err(|error| HeddleError::Io(std::io::Error::other(error.to_string())))?;
        let target_state = self
            .store()
            .get_state(target)?
            .ok_or(HeddleError::StateNotFound(*target))?;
        let target_tree = self
            .store()
            .get_tree(&target_state.tree)?
            .ok_or_else(|| HeddleError::NotFound(format!("tree {}", target_state.tree)))?;
        let current_tree = match self.head()? {
            Some(state_id) => {
                let state = self
                    .store()
                    .get_state(&state_id)?
                    .ok_or(HeddleError::StateNotFound(state_id))?;
                Some(
                    self.store()
                        .get_tree(&state.tree)?
                        .ok_or_else(|| HeddleError::NotFound(format!("tree {}", state.tree)))?,
                )
            }
            None => None,
        };
        let plan = self.plan_worktree_apply(
            current_tree.as_ref(),
            &target_tree,
            self.root(),
            false,
            WorktreeApplyDirtyBehavior::RefuseOnDirty,
        )?;
        self.execute_worktree_apply(&plan, &target_tree, self.root())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use objects::store::ObjectStore;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn missing_current_head_state_refuses_recovery_materialization() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();

        fs::write(temp.path().join("notes.md"), "recoverable\n").unwrap();
        let recovery = repo
            .snapshot(Some("recoverable".to_string()), None)
            .unwrap();
        fs::write(temp.path().join("notes.md"), "current\n").unwrap();
        let current = repo.snapshot(Some("current".to_string()), None).unwrap();

        let current_path = repo
            .heddle_dir()
            .join("objects/states")
            .join(format!("{}.state", current.id().to_string_full()));
        fs::remove_file(current_path).unwrap();
        let packs = repo.heddle_dir().join("packs");
        for entry in fs::read_dir(packs).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                fs::remove_dir_all(path).unwrap();
            } else {
                fs::remove_file(path).unwrap();
            }
        }
        repo.store().clear_recent_caches();

        let error = repo
            .restore_state_tree_to_worktree(&recovery.id())
            .expect_err("missing current HEAD state must be an integrity error");
        assert!(
            matches!(error, HeddleError::StateNotFound(id) if id == current.id()),
            "unexpected error: {error}"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("notes.md")).unwrap(),
            "current\n"
        );
    }
}
