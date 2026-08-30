// SPDX-License-Identifier: Apache-2.0
//! Worktree-relative cache directories persisted under `cache_root`.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use thiserror::Error;

/// Prefix for every cache-directory environment variable.
pub const CACHE_ENV_PREFIX: &str = "HCI_CACHE_";

/// A cache path the host-exec slice will not persist.
#[derive(Debug, Error)]
pub enum CachePathError {
    /// Absolute path or `..` would escape the evaluated worktree.
    #[error(
        "check {check:?} cache path {path:?} is not a worktree-relative directory (absolute paths and .. are refused)"
    )]
    EscapesWorktree {
        /// Offending check.
        check: String,
        /// Declared path.
        path: String,
    },
    /// The only durable copy could not be written to the slot.
    #[error("check {check:?} cache path {path:?} could not be saved: {reason}")]
    SaveFailed {
        /// Offending check.
        check: String,
        /// Declared path.
        path: String,
        /// Filesystem error.
        reason: String,
    },
}

/// Prepared cache slots for one check.
#[derive(Debug, Clone, Default)]
pub struct PreparedCaches {
    /// Environment exports. Values are the worktree directories the check uses.
    pub env: BTreeMap<String, String>,
    /// Worktree directories bound for this check.
    pub dirs: Vec<PathBuf>,
    slots: Vec<BoundSlot>,
}

#[derive(Debug, Clone)]
struct BoundSlot {
    check: String,
    path: String,
    worktree: PathBuf,
    slot: PathBuf,
}

/// Bind declared cache paths onto `workdir/<path>` and hydrate from `cache_root`.
///
/// Slots are keyed by check name so two checks declaring the same path do not
/// clobber each other. A failed hydrate degrades to a cold worktree directory.
/// Invalid paths fail closed.
pub fn prepare_caches(
    check_name: &str,
    paths: &[String],
    workdir: &Path,
    cache_root: &Path,
) -> Result<PreparedCaches, CachePathError> {
    let mut prepared = PreparedCaches::default();
    let mut used = BTreeMap::<String, u32>::new();
    for path in paths {
        if !ci_config::cache_path_is_worktree_relative(path) {
            return Err(CachePathError::EscapesWorktree {
                check: check_name.to_string(),
                path: path.clone(),
            });
        }
        let base = slot_name(path);
        let leaf = match used.get_mut(&base) {
            Some(count) => {
                *count += 1;
                format!("{base}_{count}")
            }
            None => {
                used.insert(base.clone(), 0);
                base
            }
        };
        let worktree = workdir.join(path);
        let slot = cache_root.join(slot_name(check_name)).join(leaf);
        hydrate_or_cold(&slot, &worktree);
        prepared.env.insert(
            format!("{CACHE_ENV_PREFIX}{}", slot_name(path)),
            worktree.display().to_string(),
        );
        prepared.dirs.push(worktree.clone());
        prepared.slots.push(BoundSlot {
            check: check_name.to_string(),
            path: path.clone(),
            worktree,
            slot,
        });
    }
    Ok(prepared)
}

/// Copy each worktree cache directory back to its slot.
///
/// Called after the check, success or failure. A missing worktree path is a
/// no-op. A failed save is an error so the only copy is not dropped silently.
/// On success the worktree directory is removed so the evaluated tree stays
/// clean; the slot is the durable copy.
pub fn save_caches(prepared: &PreparedCaches) -> Result<(), CachePathError> {
    for bound in &prepared.slots {
        if bound.worktree.exists() {
            replace_dir(&bound.worktree, &bound.slot).map_err(|error| {
                CachePathError::SaveFailed {
                    check: bound.check.clone(),
                    path: bound.path.clone(),
                    reason: error.to_string(),
                }
            })?;
        }
        if bound.worktree.exists() {
            std::fs::remove_dir_all(&bound.worktree).map_err(|error| {
                CachePathError::SaveFailed {
                    check: bound.check.clone(),
                    path: bound.path.clone(),
                    reason: format!("restore evaluated tree: {error}"),
                }
            })?;
        }
    }
    Ok(())
}

fn hydrate_or_cold(slot: &Path, worktree: &Path) {
    if slot.exists() {
        if copy_tree(slot, worktree).is_err() {
            let _ = std::fs::remove_dir_all(worktree);
            let _ = std::fs::create_dir_all(worktree);
        }
        return;
    }
    let _ = std::fs::create_dir_all(worktree);
}

fn replace_dir(src: &Path, slot: &Path) -> std::io::Result<()> {
    let Some(parent) = slot.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cache slot is missing a parent directory",
        ));
    };
    let Some(name) = slot.file_name() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cache slot is missing a file name",
        ));
    };
    std::fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".{}.staging", name.to_string_lossy()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    copy_tree(src, &staging)?;
    if slot.exists() {
        std::fs::remove_dir_all(slot)?;
    }
    std::fs::rename(&staging, slot)
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_tree(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn slot_name(path: &str) -> String {
    let mut output = String::with_capacity(path.len());
    let mut separated = true;
    for character in path.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_uppercase());
            separated = false;
        } else if !separated {
            output.push('_');
            separated = true;
        }
    }
    while output.ends_with('_') {
        output.pop();
    }
    if output.is_empty() {
        "CACHE".to_string()
    } else {
        output
    }
}
