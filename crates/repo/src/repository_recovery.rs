// SPDX-License-Identifier: Apache-2.0
//! Recovery worktree materialization that preserves repository refs.

use std::{
    fs,
    path::{Path, PathBuf},
};

use objects::{
    RecoveryDetails, lock::RepositoryLockExt, object::Blob, store::ObjectStore,
    util::symlink_target_bytes,
};

use super::{
    HeddleError, Repository, Result,
    repository_materialization::WorktreeWriteOp,
    repository_worktree_apply::{WorktreeApplyDirtyBehavior, WorktreeApplyPlan},
};
use crate::WorktreeStatusDetailed;

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
        let detailed = match current_tree.as_ref() {
            Some(tree) => Some(self.compare_worktree_cached_detailed(tree)?),
            None => None,
        };
        let plan = self.plan_worktree_apply(
            current_tree.as_ref(),
            &target_tree,
            self.root(),
            true,
            WorktreeApplyDirtyBehavior::RefuseOnDirty,
        )?;
        if let Some(detailed) = detailed {
            let conflicts = recovery_conflicting_paths(self.root(), &detailed, &plan)?;
            if !conflicts.is_empty() {
                return Err(recovery_dirty_conflict(conflicts));
            }
        }
        self.execute_worktree_apply(&plan, &target_tree, self.root())?;
        Ok(())
    }
}

fn recovery_conflicting_paths(
    root: &Path,
    detailed: &WorktreeStatusDetailed,
    plan: &WorktreeApplyPlan,
) -> Result<Vec<String>> {
    let untracked = detailed.untracked.flatten_paths();
    let dirty = detailed
        .modified
        .iter()
        .map(|path| ("modified", path))
        .chain(detailed.deleted.iter().map(|path| ("deleted", path)))
        .chain(untracked.iter().map(|path| ("untracked", path)));

    let mut conflicts = Vec::new();
    for (kind, path) in dirty {
        let absolute = absolute_worktree_path(root, path);
        if recovery_path_conflicts(&absolute, plan)? {
            conflicts.push(format!("{kind}: {}", path.display()));
        }
    }
    Ok(conflicts)
}

fn recovery_path_conflicts(path: &Path, plan: &WorktreeApplyPlan) -> Result<bool> {
    for write in &plan.writes {
        let write_path = write.path();
        if path == write_path {
            return Ok(!worktree_matches_write(write)?);
        }
        if path.starts_with(write_path) || write_path.starts_with(path) {
            return Ok(true);
        }
    }

    for removal in &plan.removals {
        if path == removal {
            return Ok(fs::symlink_metadata(path).is_ok());
        }
        if removal.starts_with(path) {
            return Ok(true);
        }
        // A dirty descendant does not conflict with removing a tracked
        // directory. Incremental apply removes only tracked leaves and leaves
        // a non-empty directory in place, so unrelated local children survive.
    }

    Ok(false)
}

fn absolute_worktree_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn worktree_matches_write(write: &WorktreeWriteOp) -> Result<bool> {
    let metadata = match fs::symlink_metadata(write.path()) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(HeddleError::Io(error)),
    };

    match write {
        WorktreeWriteOp::Blob {
            path,
            hash,
            executable,
        } => {
            if !metadata.file_type().is_file() {
                return Ok(false);
            }
            let bytes = fs::read(path)?;
            Ok(Blob::new(bytes).hash() == *hash && executable_mode_matches(&metadata, *executable))
        }
        WorktreeWriteOp::Symlink { path, hash, .. } => {
            if !metadata.file_type().is_symlink() {
                return Ok(false);
            }
            let target = fs::read_link(path)?;
            Ok(Blob::new(symlink_target_bytes(&target)).hash() == *hash)
        }
        WorktreeWriteOp::GitlinkPlaceholder { path, .. } => {
            if !metadata.file_type().is_file() {
                return Ok(false);
            }
            Ok(Blob::new(fs::read(path)?).hash() == write.hash())
        }
    }
}

#[cfg(unix)]
fn executable_mode_matches(metadata: &fs::Metadata, executable: bool) -> bool {
    use std::os::unix::fs::PermissionsExt;

    (metadata.permissions().mode() & 0o111 != 0) == executable
}

#[cfg(not(unix))]
fn executable_mode_matches(_metadata: &fs::Metadata, _executable: bool) -> bool {
    true
}

fn recovery_dirty_conflict(paths: Vec<String>) -> HeddleError {
    let rendered = paths
        .iter()
        .take(12)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let overflow = paths.len().saturating_sub(12);
    let rendered = if overflow == 0 {
        rendered
    } else {
        format!("{rendered}, and {overflow} more")
    };
    HeddleError::recovery(RecoveryDetails::safety_refusal(
        "dirty_worktree",
        format!("recovery would overwrite local worktree changes: {rendered}"),
        "Move or capture the listed paths, then retry `heddle undo --recover`.",
        format!("unsaved recovery-target path(s): {rendered}"),
        "recovery would replace differing local content at paths stored in the preserved state",
        "HEAD, repository state, and worktree files were left unchanged",
    ))
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
