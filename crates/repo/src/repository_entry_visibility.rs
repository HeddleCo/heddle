// SPDX-License-Identifier: Apache-2.0
//! Per-entry visibility sidecar authoring (v4 redactable trees, design §8).
//!
//! The author marks entries by **path** (what a human knows); at capture the
//! path is resolved to its enclosing tree + salted leaf hash — the name-free
//! handle a redacted serve projection is keyed by — *after* salts are minted,
//! and the [`EntryVisibility`] sidecar is staged in the SAME oplog batch as the
//! snapshot so one `heddle undo` reverts both. The weft-side accept/persist/
//! serve seam is a later leg; here we only produce + stage the object.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use objects::{
    error::HeddleError,
    fs_atomic::write_file_atomic,
    object::{ChangeId, ContentHash, EntryVisibility, EntryVisibilityEntry, Tree, VisibilityTier},
    store::ObjectStore,
    sync::RwLockExt,
};
use oplog::OpRecord;

use crate::{Repository, Result};

/// A queued per-entry visibility mark, resolved to a leaf hash at capture.
#[derive(Debug, Clone)]
pub struct EntryVisibilityMark {
    /// Repo-relative path to the entry (a file for `entry`, a directory for
    /// `subtree`).
    pub path: PathBuf,
    /// The tier the entry is served at.
    pub tier: VisibilityTier,
    /// Whether the path must name a directory (subtree mark) vs a leaf entry.
    pub subtree: bool,
}

/// The result of writing an entry-visibility sidecar as part of a snapshot
/// batch: the oplog record to fold in, and the before-image for rewind/undo.
pub struct EntryVisibilityBinding {
    pub record: OpRecord,
    pub prior_sidecar: Option<Vec<u8>>,
}

impl Repository {
    /// Queue a per-entry visibility override for `path`, applied by the next
    /// capture on this repo handle. `path` must name a non-directory entry.
    pub fn mark_entry_visibility(
        &self,
        path: impl AsRef<Path>,
        tier: VisibilityTier,
    ) -> Result<()> {
        self.pending_entry_visibility
            .write_or_poisoned()
            .push(EntryVisibilityMark {
                path: path.as_ref().to_path_buf(),
                tier,
                subtree: false,
            });
        Ok(())
    }

    /// Queue a whole-subtree visibility override for the directory at `path`
    /// (the enclosing tree's directory-entry leaf), applied by the next capture.
    /// Subtree granularity is the recommended default (design §8.4).
    pub fn mark_subtree_visibility(
        &self,
        path: impl AsRef<Path>,
        tier: VisibilityTier,
    ) -> Result<()> {
        self.pending_entry_visibility
            .write_or_poisoned()
            .push(EntryVisibilityMark {
                path: path.as_ref().to_path_buf(),
                tier,
                subtree: true,
            });
        Ok(())
    }

    /// Drain the queued marks (consumed by a capture attempt).
    pub(crate) fn take_pending_entry_visibility_marks(&self) -> Vec<EntryVisibilityMark> {
        std::mem::take(&mut *self.pending_entry_visibility.write_or_poisoned())
    }

    /// Resolve queued marks against a just-captured v4 tree into an
    /// [`EntryVisibility`] sidecar, or `None` when there are no marks. FAILS
    /// LOUD on a path that does not resolve to an entry (or resolves to the
    /// wrong kind for the mark), rather than silently dropping it.
    ///
    /// `root` is the v4 root tree; `subtrees` are the other v4 trees produced
    /// by the same capture (nested-tree resolution falls back to the store).
    pub(crate) fn resolve_entry_visibility(
        &self,
        change_id: ChangeId,
        root: &Tree,
        subtrees: &[Tree],
        marks: &[EntryVisibilityMark],
    ) -> Result<Option<EntryVisibility>> {
        if marks.is_empty() {
            return Ok(None);
        }
        let mut by_hash: HashMap<ContentHash, Tree> = HashMap::with_capacity(subtrees.len());
        for tree in subtrees {
            by_hash.insert(tree.hash(), tree.clone());
        }
        let mut entries = Vec::with_capacity(marks.len());
        for mark in marks {
            let (tree_id, leaf_hash, is_dir) =
                self.resolve_mark_leaf(root, &by_hash, &mark.path)?;
            if mark.subtree && !is_dir {
                return Err(HeddleError::NotFound(format!(
                    "mark_subtree_visibility path '{}' is not a directory entry",
                    mark.path.display()
                )));
            }
            if !mark.subtree && is_dir {
                return Err(HeddleError::NotFound(format!(
                    "mark_entry_visibility path '{}' names a directory; use mark_subtree_visibility",
                    mark.path.display()
                )));
            }
            entries.push(EntryVisibilityEntry {
                tree_id,
                leaf_hash,
                tier: mark.tier.clone(),
            });
        }
        let sidecar = EntryVisibility::new(change_id, root.hash(), entries)
            .map_err(|e| HeddleError::Config(e.to_string()))?;
        Ok(Some(sidecar))
    }

    /// Walk `path` from `root` to its enclosing tree and return
    /// `(enclosing_tree_id, entry_leaf_hash, entry_is_directory)`. Fails loud on
    /// any missing component or a component that traverses a non-directory.
    fn resolve_mark_leaf(
        &self,
        root: &Tree,
        by_hash: &HashMap<ContentHash, Tree>,
        path: &Path,
    ) -> Result<(ContentHash, ContentHash, bool)> {
        let components: Vec<&str> = path
            .components()
            .map(|component| match component {
                std::path::Component::Normal(name) => name.to_str(),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .filter(|parts| !parts.is_empty())
            .ok_or_else(|| {
                HeddleError::NotFound(format!(
                    "entry-visibility path '{}' is not a normal repo-relative path",
                    path.display()
                ))
            })?;

        let mut current = root.clone();
        for (index, component) in components.iter().enumerate() {
            let entry = current.get(component).ok_or_else(|| {
                HeddleError::NotFound(format!(
                    "entry-visibility path '{}' does not resolve: '{}' is absent",
                    path.display(),
                    component
                ))
            })?;
            if index + 1 == components.len() {
                let leaf = current.v4_leaf_hash_for(component).ok_or_else(|| {
                    // A precondition/config error (marks require a v4 spool),
                    // NOT `RedactedTree` — Leg 3 maps `RedactedTree` to a wire
                    // status and this must not be misclassified as one.
                    HeddleError::Config(format!(
                        "entry-visibility mark for '{}' requires a v4 (tree_scheme = v4) spool; \
                         the captured tree is not salted",
                        path.display()
                    ))
                })?;
                return Ok((current.hash(), leaf, entry.is_tree()));
            }
            let child_hash = entry.tree_hash().ok_or_else(|| {
                HeddleError::NotFound(format!(
                    "entry-visibility path '{}' traverses non-directory '{}'",
                    path.display(),
                    component
                ))
            })?;
            current = self.resolve_captured_tree(&child_hash, by_hash)?;
        }
        // Unreachable: the loop returns on its final component.
        Err(HeddleError::NotFound(format!(
            "entry-visibility path '{}' did not resolve",
            path.display()
        )))
    }

    fn resolve_captured_tree(
        &self,
        hash: &ContentHash,
        by_hash: &HashMap<ContentHash, Tree>,
    ) -> Result<Tree> {
        if let Some(tree) = by_hash.get(hash) {
            return Ok(tree.clone());
        }
        self.store
            .get_tree(hash)?
            .ok_or_else(|| HeddleError::MissingObject {
                object_type: "tree".to_string(),
                id: hash.to_string(),
            })
    }

    // ── sidecar storage (mirrors the per-state visibility sidecar) ──────

    /// `<heddle_dir>/entry_visibility/` — root of the per-state entry-visibility
    /// sidecar store.
    pub(crate) fn entry_visibility_dir(&self) -> PathBuf {
        self.heddle_dir().join("entry_visibility")
    }

    /// Sidecar file path for a state's entry-visibility record.
    pub(crate) fn entry_visibility_path_for_change(&self, change_id: &ChangeId) -> PathBuf {
        self.entry_visibility_dir()
            .join(format!("{}.bin", change_id.to_string_full()))
    }

    /// The raw sidecar bytes for `change_id`, or `None` if absent.
    pub(crate) fn get_entry_visibility_bytes(
        &self,
        change_id: &ChangeId,
    ) -> Result<Option<Vec<u8>>> {
        let path = self.entry_visibility_path_for_change(change_id);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Restore the entry-visibility sidecar to an absolute snapshot: write
    /// `snapshot`'s bytes, or remove the file when `None`. Idempotent
    /// (write-or-delete), so re-running it on a rollback path is safe. This is
    /// the undo/redo restore point.
    pub fn restore_entry_visibility_sidecar(
        &self,
        change_id: &ChangeId,
        snapshot: Option<Vec<u8>>,
    ) -> Result<()> {
        let path = self.entry_visibility_path_for_change(change_id);
        match snapshot {
            Some(bytes) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                write_file_atomic(&path, &bytes)?;
            }
            None => {
                if path.exists() {
                    std::fs::remove_file(&path)?;
                }
            }
        }
        Ok(())
    }

    /// Write the entry-visibility sidecar for `sidecar.change_id` and return the
    /// binding to fold into the snapshot's oplog batch (record + before-image).
    /// Called from the snapshot commit path under the repo write lock.
    pub(crate) fn stage_entry_visibility_binding(
        &self,
        sidecar: &EntryVisibility,
    ) -> Result<EntryVisibilityBinding> {
        let change_id = sidecar.change_id;
        let prior_sidecar = self.get_entry_visibility_bytes(&change_id)?;
        let new_bytes = sidecar
            .encode()
            .map_err(|e| HeddleError::Serialization(e.to_string()))?;
        let record_id = sidecar
            .content_hash()
            .map_err(|e| HeddleError::Serialization(e.to_string()))?;
        self.restore_entry_visibility_sidecar(&change_id, Some(new_bytes.clone()))?;
        Ok(EntryVisibilityBinding {
            record: OpRecord::EntryVisibilitySet {
                change_id,
                record_id,
                prior_sidecar: prior_sidecar.clone(),
                new_sidecar: Some(new_bytes),
            },
            prior_sidecar,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use objects::{
        error::HeddleError,
        object::{EntryVisibility, VisibilityTier},
        store::ObjectStore,
    };
    use oplog::{OpLogBackend, OpRecord};
    use tempfile::TempDir;

    use crate::{RepoConfig, Repository, TreeSchemePolicy};

    fn v4_repo() -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let config_path = repo.heddle_dir().join("config.toml");
        let mut config = RepoConfig::load_for_repository(&config_path).unwrap();
        config.policies.tree_scheme = TreeSchemePolicy::V4;
        config.save(&config_path).unwrap();
        let repo = Repository::open(temp.path()).unwrap();
        (temp, repo)
    }

    #[test]
    fn mark_entry_visibility_stages_sidecar_keyed_by_change_with_leaf_hash() {
        let (temp, repo) = v4_repo();
        fs::write(temp.path().join("readme.md"), b"public\n").unwrap();
        fs::write(temp.path().join("secret.md"), b"embargoed\n").unwrap();
        repo.mark_entry_visibility(
            "secret.md",
            VisibilityTier::Private {
                scope_label: "embargo".into(),
            },
        )
        .unwrap();
        let state = repo.snapshot(Some("cap".into()), None).unwrap();

        let bytes = repo
            .get_entry_visibility_bytes(&state.change_id)
            .unwrap()
            .expect("sidecar must be staged");
        let sidecar = EntryVisibility::decode(&bytes).unwrap();
        assert_eq!(sidecar.change_id, state.change_id);
        assert_eq!(sidecar.tree_root, state.tree);
        assert_eq!(sidecar.entries.len(), 1);
        let record = &sidecar.entries[0];
        assert_eq!(
            record.tier,
            VisibilityTier::Private {
                scope_label: "embargo".into()
            }
        );

        let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
        assert_eq!(record.tree_id, tree.hash());
        assert_eq!(
            record.leaf_hash,
            tree.v4_leaf_hash_for("secret.md").unwrap()
        );
        // The public sibling was not marked and carries no override.
        assert_ne!(
            record.leaf_hash,
            tree.v4_leaf_hash_for("readme.md").unwrap()
        );
    }

    #[test]
    fn entry_visibility_staged_in_same_oplog_batch_as_snapshot() {
        let (temp, repo) = v4_repo();
        fs::write(temp.path().join("secret.md"), b"x\n").unwrap();
        repo.mark_entry_visibility("secret.md", VisibilityTier::Internal)
            .unwrap();
        let state = repo.snapshot(Some("cap".into()), None).unwrap();

        let recent = repo.oplog().recent(16).unwrap();
        let snapshot_batch = recent
            .iter()
            .find_map(|entry| match entry.operation {
                OpRecord::Snapshot { new_state, .. } if new_state == state.id() => {
                    Some(entry.batch_id)
                }
                _ => None,
            })
            .expect("snapshot record present");
        let ev = recent
            .iter()
            .find(|entry| matches!(entry.operation, OpRecord::EntryVisibilitySet { .. }))
            .expect("entry-visibility record present");
        assert_eq!(
            ev.batch_id, snapshot_batch,
            "entry-visibility must ride the SAME oplog batch as the snapshot"
        );
        match &ev.operation {
            OpRecord::EntryVisibilitySet {
                change_id,
                prior_sidecar,
                new_sidecar,
                ..
            } => {
                assert_eq!(*change_id, state.change_id);
                assert!(
                    prior_sidecar.is_none(),
                    "genesis binding has no before-image"
                );
                let bytes = repo
                    .get_entry_visibility_bytes(&state.change_id)
                    .unwrap()
                    .unwrap();
                assert_eq!(new_sidecar.as_deref(), Some(bytes.as_slice()));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn restore_reverts_the_entry_visibility_sidecar() {
        let (temp, repo) = v4_repo();
        fs::write(temp.path().join("secret.md"), b"x\n").unwrap();
        repo.mark_entry_visibility("secret.md", VisibilityTier::Internal)
            .unwrap();
        let state = repo.snapshot(Some("cap".into()), None).unwrap();
        assert!(
            repo.get_entry_visibility_bytes(&state.change_id)
                .unwrap()
                .is_some()
        );
        // Undo's restore point: rolling back to the before-image (None) removes it.
        repo.restore_entry_visibility_sidecar(&state.change_id, None)
            .unwrap();
        assert!(
            repo.get_entry_visibility_bytes(&state.change_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn mark_on_unresolvable_path_fails_loud() {
        let (temp, repo) = v4_repo();
        fs::write(temp.path().join("readme.md"), b"x\n").unwrap();
        repo.mark_entry_visibility("nope/missing.md", VisibilityTier::Internal)
            .unwrap();
        let err = repo.snapshot(Some("cap".into()), None).unwrap_err();
        assert!(
            matches!(err, HeddleError::NotFound(_)),
            "an unresolvable marked path must fail the capture loudly: {err:?}"
        );
    }

    #[test]
    fn mark_subtree_marks_directory_leaf_and_entry_mark_on_dir_fails() {
        let (temp, repo) = v4_repo();
        fs::create_dir(temp.path().join("dir")).unwrap();
        fs::write(temp.path().join("dir/inner.txt"), b"x\n").unwrap();
        repo.mark_subtree_visibility("dir", VisibilityTier::Internal)
            .unwrap();
        let state = repo.snapshot(Some("cap".into()), None).unwrap();
        let sidecar = EntryVisibility::decode(
            &repo
                .get_entry_visibility_bytes(&state.change_id)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
        assert_eq!(sidecar.entries.len(), 1);
        assert_eq!(
            sidecar.entries[0].leaf_hash,
            tree.v4_leaf_hash_for("dir").unwrap()
        );

        // A plain entry mark on a directory path is a loud error.
        repo.mark_entry_visibility("dir", VisibilityTier::Internal)
            .unwrap();
        assert!(matches!(
            repo.snapshot(Some("again".into()), None).unwrap_err(),
            HeddleError::NotFound(_)
        ));
    }

    #[test]
    fn marks_survive_a_retried_capture() {
        use super::super::repository_snapshot::with_forced_revalidation_retries;
        let (temp, repo) = v4_repo();
        fs::write(temp.path().join("secret.md"), b"x\n").unwrap();
        repo.mark_entry_visibility("secret.md", VisibilityTier::Internal)
            .unwrap();
        // Force two internal revalidation retries (a prepare/commit race). The
        // capture still commits and MUST still stage the sidecar — marks are
        // drained ONCE outside the loop, not re-drained per attempt. FALSIFY:
        // revert to draining in `stage_snapshot_objects` and the sidecar is
        // dropped on the retried commit → this assertion fails.
        let state =
            with_forced_revalidation_retries(2, || repo.snapshot(Some("cap".into()), None))
                .unwrap();
        assert!(
            repo.get_entry_visibility_bytes(&state.change_id)
                .unwrap()
                .is_some(),
            "a retried-then-committed capture must not drop the entry-visibility sidecar"
        );
    }

    #[test]
    fn merge_capture_with_pending_marks_fails_loud() {
        let (temp, repo) = v4_repo();
        fs::write(temp.path().join("a.txt"), b"1\n").unwrap();
        let first = repo.snapshot(Some("a".into()), None).unwrap();
        fs::write(temp.path().join("a.txt"), b"2\n").unwrap();
        let _second = repo.snapshot(Some("b".into()), None).unwrap();
        repo.mark_entry_visibility("a.txt", VisibilityTier::Internal)
            .unwrap();
        // A merge capture builds its own tree and never runs the sidecar path;
        // pending marks must fail loud (re-mark in a later worktree capture).
        let attribution = repo.get_attribution().unwrap();
        let err = repo
            .snapshot_merge_with_attribution(
                &first.id(),
                Some("merge".into()),
                None,
                attribution,
                None,
                false,
            )
            .unwrap_err();
        assert!(
            matches!(err, HeddleError::Config(_)),
            "merge with pending entry-marks must fail loud: {err:?}"
        );
    }
}
