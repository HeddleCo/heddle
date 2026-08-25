// SPDX-License-Identifier: Apache-2.0
//! Git-overlay surface of `Repository`: branch/tag tips, state<->commit
//! mappings, worktree status, durable Git checkpoints, and remote-tracking
//! reporting.

use schemars::JsonSchema;
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
};

use chrono::Utc;
#[cfg(feature = "git-overlay")]
use objects::object::MarkerName;
use objects::{
    error::{HeddleError, Result},
    fs_atomic::{enrich_fs_error, write_file_atomic},
    object::{StateId, ThreadName},
    store::ObjectStore,
    worktree::WorktreeStatus,
};
use oplog::OpRecord;
use refs::Head;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sley::{
    ObjectId as SleyObjectId, ReferenceTarget as SleyRefTarget,
    Repository as SleyRepository,
};

#[cfg(feature = "git-overlay")]
use sley::{
    ShortStatusOptions as SleyShortStatusOptions, StatusUntrackedMode as SleyStatusUntrackedMode,
    StreamControl as SleyStreamControl,
};

use crate::{GitRefContentNamespace, GitRefName};

#[cfg(feature = "git-overlay")]
use super::CommitGraphIndex;
use super::{Repository, RepositoryCapability, open_git_repository_at_root};

const GIT_CHECKPOINTS_FILE: &str = "git-checkpoints.json";
const GIT_CHECKPOINT_INTENT_FILE: &str = "git-checkpoint-intent.json";
const GIT_OVERLAY_LOCAL_EXCLUDE_PATTERNS: &[&str] = &[".heddle/"];

#[derive(Debug)]
pub struct GitOverlayShortStatus {
    pub worktree: WorktreeStatus,
    pub index_staged_paths: Vec<String>,
    pub index_extra_paths: Vec<String>,
    pub index_plan_applicable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GitHeadState {
    Attached(String),
    Detached(SleyObjectId),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitCheckpointRecord {
    pub state_id: String,
    pub git_commit: String,
    pub summary: String,
    pub committed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitCheckpointIntentPhase {
    Prepared,
    Published,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitCheckpointIntent {
    pub version: u32,
    pub state_id: String,
    pub branch: String,
    pub previous_git_oid: Option<String>,
    pub new_git_oid: String,
    pub summary: String,
    pub phase: GitCheckpointIntentPhase,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitImportGuidance {
    pub current_branch: String,
    pub missing_branch_count: usize,
    pub missing_branches: Vec<String>,
    pub recommended_command: String,
}

#[cfg(feature = "git-overlay")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitOverlayBranchTip {
    pub branch: String,
    pub git_commit: String,
    pub history_imported: bool,
    #[serde(skip)]
    pub mapped_state: Option<StateId>,
}

#[cfg(feature = "git-overlay")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitOverlayTagTip {
    pub tag: String,
    pub git_commit: String,
    pub history_imported: bool,
    #[serde(skip)]
    pub mapped_state: Option<StateId>,
}

/// How many Git commits reachable from a branch tip have no Heddle mapping
/// (neither imported/projection-mapped nor checkpointed). Used to report
/// how far a Git branch moved out-of-band before `heddle bridge git import --ref`
/// reconciles it.
#[cfg(feature = "git-overlay")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitOverlayOutOfBandCommits {
    pub count: usize,
    /// True when the walk stopped at the scan limit before exhausting the
    /// unmapped history; `count` is then a lower bound.
    pub truncated: bool,
}

/// Cap for the out-of-band commit walk so a read path (status/verify/health)
/// never pays an O(full-history) traversal when external history was rewritten
/// and no mapped ancestor exists.
#[cfg(feature = "git-overlay")]
const GIT_OVERLAY_OUT_OF_BAND_SCAN_LIMIT: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GitRemoteTrackingStatus {
    pub branch: String,
    pub upstream: String,
    pub ahead: usize,
    pub behind: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_oid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_oid: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub upstream_is_undone_checkpoint: bool,
    pub message: String,
    #[schemars(with = "Option<String>")]
    pub next_action: String,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Deserialize)]
struct GitProjectionMappingEntry {
    state_id: String,
    git_oid: String,
}

#[derive(Debug, Deserialize, Default)]
struct GitProjectionMappingFile {
    entries: Vec<GitProjectionMappingEntry>,
}

impl Repository {
    pub fn git_overlay_sley_repository(&self) -> Result<Option<SleyRepository>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }

        if let Some(repo) = self
            .git_overlay_repo
            .read()
            .map_err(|_| HeddleError::Config("git overlay repo cache lock poisoned".into()))?
            .clone()
        {
            return Ok(Some(repo));
        }

        let mut cached = self
            .git_overlay_repo
            .write()
            .map_err(|_| HeddleError::Config("git overlay repo cache lock poisoned".into()))?;
        if let Some(repo) = cached.clone() {
            return Ok(Some(repo));
        }

        let repo = open_git_repository_at_root(&self.root)?.ok_or_else(|| {
            HeddleError::Config(format!(
                "failed to inspect Git-overlay repository rooted at '{}': no valid .git metadata at that root",
                self.root.display()
            ))
        })?;
        *cached = Some(repo.clone());
        Ok(Some(repo))
    }
    pub fn git_remote_tracking_status(&self) -> Result<Option<GitRemoteTrackingStatus>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }

        let branch = match self.git_overlay_current_branch()? {
            Some(branch) => branch,
            None => return Ok(None),
        };

        let Some(git) = self.git_overlay_sley_repository()? else {
            return Ok(None);
        };
        let Some(head) = git_resolve_oid(&git, "HEAD")? else {
            return Ok(None);
        };

        let local_ref_name = GitRefName::branch_full_name(&branch);
        if git
            .reference_exists(&local_ref_name)
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to inspect Git reference '{local_ref_name}': {error}"
                ))
            })?
            && let Some(tracking_name) = git_configured_tracking_ref(&git, &branch)?
            && let Some(upstream_head) = git_resolve_oid(&git, &tracking_name)?
        {
            let (ahead, behind) = git_ahead_behind_counts(&git, head, upstream_head)?;
            if ahead == 0 && behind == 0 {
                return Ok(None);
            }
            let upstream = git_remote_tracking_display_name(&tracking_name);
            let local_oid = head.to_string();
            let upstream_oid = upstream_head.to_string();
            let upstream_is_undone_checkpoint =
                self.remote_tracks_undone_git_checkpoint(&branch, &local_oid, &upstream_oid)?;
            return Ok(Some(GitRemoteTrackingStatus {
                branch: branch.clone(),
                upstream: upstream.clone(),
                ahead,
                behind,
                local_oid: Some(local_oid),
                upstream_oid: Some(upstream_oid),
                upstream_is_undone_checkpoint,
                message: git_remote_tracking_message(
                    &branch,
                    &upstream,
                    ahead,
                    behind,
                    upstream_is_undone_checkpoint,
                ),
                next_action: git_remote_tracking_next_action(
                    ahead,
                    behind,
                    upstream_is_undone_checkpoint,
                ),
            }));
        }

        let remotes = git_remote_names(&self.root)?;
        if remotes.is_empty() {
            return Ok(None);
        }
        for remote in &remotes {
            let remote_ref = GitRefName::remote_branch_full_name(remote, &branch);
            if let Some(remote_head) = git_resolve_oid(&git, &remote_ref)? {
                if remote_head == head {
                    return Ok(None);
                }
                let (ahead, behind) = git_ahead_behind_counts(&git, head, remote_head)?;
                if behind > 0 {
                    let upstream = format!("{remote}/{branch}");
                    let local_oid = head.to_string();
                    let upstream_oid = remote_head.to_string();
                    let upstream_is_undone_checkpoint = self.remote_tracks_undone_git_checkpoint(
                        &branch,
                        &local_oid,
                        &upstream_oid,
                    )?;
                    return Ok(Some(GitRemoteTrackingStatus {
                        branch: branch.clone(),
                        upstream: upstream.clone(),
                        ahead,
                        behind,
                        local_oid: Some(local_oid),
                        upstream_oid: Some(upstream_oid),
                        upstream_is_undone_checkpoint,
                        message: git_remote_tracking_message(
                            &branch,
                            &upstream,
                            ahead,
                            behind,
                            upstream_is_undone_checkpoint,
                        ),
                        next_action: git_remote_tracking_next_action(
                            ahead,
                            behind,
                            upstream_is_undone_checkpoint,
                        ),
                    }));
                }
            }
        }

        Ok(Some(GitRemoteTrackingStatus {
            branch: branch.clone(),
            upstream: String::new(),
            ahead: 0,
            behind: 0,
            local_oid: Some(head.to_string()),
            upstream_oid: None,
            upstream_is_undone_checkpoint: false,
            message: format!("Git branch '{branch}' has no upstream tracking branch"),
            next_action: "heddle push".to_string(),
        }))
    }

    fn remote_tracks_undone_git_checkpoint(
        &self,
        branch: &str,
        local_oid: &str,
        upstream_oid: &str,
    ) -> Result<bool> {
        let scope = self.op_scope();
        let batches = match self.oplog().redo_batches_scoped(64, Some(&scope)) {
            Ok(batches) => batches,
            Err(error) => {
                tracing::warn!(
                    branch,
                    local_oid,
                    upstream_oid,
                    error = %error,
                    "could not inspect redo oplog for undone Git checkpoint status"
                );
                return Ok(false);
            }
        };
        Ok(batches.iter().any(|batch| {
            batch.entries.iter().any(|entry| {
                if !entry.undone {
                    return false;
                }
                matches!(
                    &entry.operation,
                    OpRecord::GitCheckpoint {
                        branch: checkpoint_branch,
                        previous_git_oid: Some(previous_git_oid),
                        new_git_oid,
                        ..
                    } if checkpoint_branch == branch
                        && previous_git_oid == local_oid
                        && new_git_oid == upstream_oid
                )
            })
        }))
    }

    pub fn git_import_guidance(&self) -> Result<Option<GitImportGuidance>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }
        // Git-overlay treats Git refs and commits as Git-owned storage that
        // Heddle reads directly. Missing Git->Heddle state mappings are not an
        // everyday "needs adopt" condition; `adopt` is reserved for explicit
        // transition to native source authority.
        Ok(None)
    }
    /// Enumerate Git branch tips with Heddle mapping status.
    ///
    /// Gated behind `git-overlay`: native-only builds do not expose overlay
    /// branch enumeration on `Repository`.
    #[cfg(feature = "git-overlay")]
    pub fn git_overlay_branch_tips(&self) -> Result<Vec<GitOverlayBranchTip>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(Vec::new());
        }

        let Some(git_repo) = self.git_overlay_sley_repository()? else {
            return Ok(Vec::new());
        };

        let imported_threads: std::collections::HashSet<ThreadName> =
            self.refs().list_threads()?.into_iter().collect();
        let projection_mapping = self.git_projection_mapping()?;
        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        let mut branch_tips = Vec::new();

        for branch in git_repo.references().list_refs().map_err(|error| {
            HeddleError::Config(format!(
                "failed to enumerate git branches at '{}': {}",
                self.root.display(),
                error
            ))
        })? {
            let ref_name = GitRefName::new(&branch.name);
            if ref_name.content_namespace() != Some(GitRefContentNamespace::Branch) {
                continue;
            };
            let Some(name) = ref_name.short_name().map(str::to_string) else {
                continue;
            };
            let Some(target) =
                self.git_overlay_commit_tip_oid(&git_repo, &branch, "branch", &name)?
            else {
                continue;
            };
            let git_commit = target.to_string();
            let mapped_state = self.git_overlay_mapped_state_for_commit(
                &git_commit,
                &projection_mapping,
                &ingest_mapping,
                &checkpoint_mapping,
            )?;
            let thread_name = ThreadName::from(name.as_str());
            let history_imported = if imported_threads.contains(&thread_name) {
                // Read the thread ref once; the mapped + checkpointed
                // checks each used to re-read it, which doubled the
                // ref-store hits per branch on a 60+ branch repo.
                let existing_thread = self.refs().get_thread(&thread_name)?;
                let mapped = matches!(
                    (existing_thread.as_ref(), mapped_state.as_ref()),
                    (Some(existing), Some(mapped_state))
                        if existing == mapped_state
                );
                let checkpointed = if mapped {
                    false
                } else if let Some(existing) = existing_thread {
                    self.latest_git_checkpoint_for_state(&existing)?
                        .is_some_and(|record| record.git_commit == git_commit)
                        || mapped_state.as_ref().is_some_and(|mapped_state| {
                            self.state_is_ancestor(mapped_state, &existing)
                        })
                } else {
                    false
                };
                mapped || checkpointed
            } else {
                mapped_state.is_some()
            };
            branch_tips.push(GitOverlayBranchTip {
                branch: name,
                git_commit,
                history_imported,
                mapped_state,
            });
        }
        branch_tips.sort_by(|a, b| a.branch.cmp(&b.branch));
        Ok(branch_tips)
    }

    #[cfg(feature = "git-overlay")]
    pub fn git_overlay_tag_tips(&self) -> Result<Vec<GitOverlayTagTip>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(Vec::new());
        }

        let Some(git_repo) = self.git_overlay_sley_repository()? else {
            return Ok(Vec::new());
        };

        let imported_markers: std::collections::HashSet<MarkerName> =
            self.refs().list_markers()?.into_iter().collect();
        let projection_mapping = self.git_projection_mapping()?;
        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        let mut tag_tips = Vec::new();

        for tag in git_repo.references().list_refs().map_err(|error| {
            HeddleError::Config(format!(
                "failed to enumerate git tags at '{}': {}",
                self.root.display(),
                error
            ))
        })? {
            let ref_name = GitRefName::new(&tag.name);
            if ref_name.content_namespace() != Some(GitRefContentNamespace::Tag) {
                continue;
            };
            let Some(name) = ref_name.short_name().map(str::to_string) else {
                continue;
            };
            let Some(target) = self.git_overlay_commit_tip_oid(&git_repo, &tag, "tag", &name)?
            else {
                continue;
            };
            let git_commit = target.to_string();
            let mapped_state = self.git_overlay_mapped_state_for_commit(
                &git_commit,
                &projection_mapping,
                &ingest_mapping,
                &checkpoint_mapping,
            )?;
            let marker_name = MarkerName::from(name.as_str());
            let history_imported = if imported_markers.contains(&marker_name) {
                matches!(
                    (self.refs().get_marker(&marker_name)?, mapped_state.as_ref()),
                    (Some(existing), Some(mapped_state)) if existing == *mapped_state
                )
            } else {
                false
            };
            tag_tips.push(GitOverlayTagTip {
                tag: name,
                git_commit,
                history_imported,
                mapped_state,
            });
        }

        tag_tips.sort_by(|a, b| a.tag.cmp(&b.tag));
        Ok(tag_tips)
    }

    #[cfg(feature = "git-overlay")]
    pub fn git_overlay_branch_tip(&self, name: &str) -> Result<Option<GitOverlayBranchTip>> {
        Ok(self
            .git_overlay_branch_tips()?
            .into_iter()
            .find(|tip| tip.branch == name))
    }

    #[cfg(feature = "git-overlay")]
    pub fn git_overlay_tag_tip(&self, name: &str) -> Result<Option<GitOverlayTagTip>> {
        Ok(self
            .git_overlay_tag_tips()?
            .into_iter()
            .find(|tip| tip.tag == name))
    }
    /// Map a Git branch name to a Heddle state id when known.
    ///
    /// Kept available without `git-overlay` feature so open/HEAD reconciliation
    /// can compile under native-only builds (it no-ops when capability is not
    /// Git Overlay). Tip enumeration (`git_overlay_branch_tips`) remains gated.
    pub fn git_overlay_mapped_state_for_branch(&self, name: &str) -> Result<Option<StateId>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }
        let Some(git_repo) = self.git_overlay_sley_repository()? else {
            return Ok(None);
        };
        let full_name = format!("refs/heads/{name}");
        let projection_mapping = self.git_projection_mapping()?;
        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        for reference in git_repo.references().list_refs().map_err(|error| {
            HeddleError::Config(format!(
                "failed to enumerate git branches at '{}': {}",
                self.root.display(),
                error
            ))
        })? {
            if reference.name != full_name {
                continue;
            }
            let Some(target) =
                self.git_overlay_commit_tip_oid(&git_repo, &reference, "branch", name)?
            else {
                return Ok(None);
            };
            return self.git_overlay_mapped_state_for_commit(
                &target.to_string(),
                &projection_mapping,
                &ingest_mapping,
                &checkpoint_mapping,
            );
        }
        Ok(None)
    }

    #[cfg(feature = "git-overlay")]
    pub fn git_overlay_mapped_state_for_remote_tracking_ref(
        &self,
        name: &str,
    ) -> Result<Option<StateId>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }
        let Some(git_repo) = self.git_overlay_sley_repository()? else {
            return Ok(None);
        };
        let full_name = GitRefName::remote_tracking_full_name(name);
        let projection_mapping = self.git_projection_mapping()?;
        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        for reference in git_repo.references().list_refs().map_err(|error| {
            HeddleError::Config(format!(
                "failed to enumerate git remote-tracking refs at '{}': {}",
                self.root.display(),
                error
            ))
        })? {
            if reference.name != full_name {
                continue;
            }
            let Some(target) =
                self.git_overlay_commit_tip_oid(&git_repo, &reference, "remote branch", name)?
            else {
                return Ok(None);
            };
            return self.git_overlay_mapped_state_for_commit(
                &target.to_string(),
                &projection_mapping,
                &ingest_mapping,
                &checkpoint_mapping,
            );
        }
        Ok(None)
    }

    pub fn git_overlay_mapped_state_for_tag(&self, name: &str) -> Result<Option<StateId>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }
        let Some(git_repo) = self.git_overlay_sley_repository()? else {
            return Ok(None);
        };
        let full_name = format!("refs/tags/{name}");
        let projection_mapping = self.git_projection_mapping()?;
        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        for reference in git_repo.references().list_refs().map_err(|error| {
            HeddleError::Config(format!(
                "failed to enumerate git tags at '{}': {}",
                self.root.display(),
                error
            ))
        })? {
            if reference.name != full_name {
                continue;
            }
            let Some(target) =
                self.git_overlay_commit_tip_oid(&git_repo, &reference, "tag", name)?
            else {
                return Ok(None);
            };
            return self.git_overlay_mapped_state_for_commit(
                &target.to_string(),
                &projection_mapping,
                &ingest_mapping,
                &checkpoint_mapping,
            );
        }
        Ok(None)
    }

    #[cfg(feature = "git-overlay")]
    fn state_is_ancestor(&self, ancestor: &StateId, descendant: &StateId) -> bool {
        let mut graph = CommitGraphIndex::new(self);
        graph.is_ancestor(ancestor, descendant).unwrap_or(false)
    }
    /// Git-overlay worktree status, compared against the **Git index** (distinct
    /// from `compare_worktree_cached*`, which compares against heddle's own index).
    ///
    /// The expensive part — deciding whether each tracked file changed since it
    /// was staged — is handled by sley's `stream_short_status_with_options`, which
    /// honors git's racy-clean stat cache: when a file's mode + size + mtime match
    /// its Git index entry (and the entry is not racily clean), sley reuses the
    /// staged OID and SKIPS re-reading + SHA-1ing the file (`reuse_tracked_entry`),
    /// falling back to a full content hash whenever the stat is ambiguous. On a
    /// warm worktree this turns the walk from "hash every file" into "stat every
    /// file" (~0.35s vs minutes on the ~6k-file ghostty tree). This stat-cache
    /// MUST be preserved across sley bumps — a sley that re-hashes unconditionally
    /// would silently reintroduce the pathological checkpoint cost.
    #[cfg(feature = "git-overlay")]
    pub fn git_overlay_worktree_status(&self) -> Result<Option<WorktreeStatus>> {
        Ok(self
            .git_overlay_short_status()?
            .map(|status| status.worktree))
    }

    /// Build worktree status and Git-index intent from one Sley status stream.
    #[cfg(feature = "git-overlay")]
    pub fn git_overlay_short_status(&self) -> Result<Option<GitOverlayShortStatus>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }
        let git_repo = match self.git_overlay_sley_repository() {
            Ok(Some(repo)) => repo,
            Ok(None) | Err(_) => return Ok(None),
        };
        if git_repo.workdir().is_none() {
            return Ok(None);
        }

        let mut added = BTreeSet::new();
        let mut modified = BTreeSet::new();
        let mut deleted = BTreeSet::new();
        let ignore_patterns = self.ignore_patterns()?;
        let worktree_ignore =
            crate::worktree_ignore::WorktreeIgnoreMatcher::cached(&ignore_patterns);
        let index_ignore = objects::worktree::build_worktree_ignore(&ignore_patterns);
        let index_plan_applicable = git_worktree_matches_repo_root(&git_repo, self.root());
        let mut index_staged_paths = Vec::new();
        let mut index_extra_paths = Vec::new();

        git_repo
            .stream_short_status_with_options(
                SleyShortStatusOptions {
                    untracked_mode: SleyStatusUntrackedMode::All,
                    ..SleyShortStatusOptions::default()
                },
                |entry| {
                    // Borrow when the path is clean UTF-8; only kept rows
                    // materialize an owned PathBuf.
                    let path = String::from_utf8_lossy(entry.path);
                    if path.is_empty() {
                        return Ok(SleyStreamControl::Continue);
                    }
                    if index_plan_applicable {
                        append_short_status_to_index_intent(
                            &mut index_staged_paths,
                            &mut index_extra_paths,
                            &index_ignore,
                            entry,
                            &path,
                        );
                    }
                    if ignored_git_overlay_status_path(&path) {
                        return Ok(SleyStreamControl::Continue);
                    }
                    let path = PathBuf::from(&*path);

                    if entry.index == b'?' && entry.worktree == b'?' {
                        if git_overlay_untracked_path_ignored(&worktree_ignore, &path) {
                            return Ok(SleyStreamControl::Continue);
                        }
                        added.insert(path);
                    } else if entry.index == b'D' || entry.worktree == b'D' {
                        deleted.insert(path);
                    } else if entry.index == b'A'
                        || entry.index == b'R'
                        || entry.index == b'C'
                        || entry.head_oid.is_none()
                    {
                        added.insert(path);
                    } else {
                        modified.insert(path);
                    }

                    Ok(SleyStreamControl::Continue)
                },
            )
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to inspect Git worktree status at '{}': {}",
                    self.root.display(),
                    error
                ))
            })?;

        Ok(Some(GitOverlayShortStatus {
            worktree: WorktreeStatus {
                modified: modified.into_iter().collect(),
                added: added.into_iter().collect(),
                deleted: deleted.into_iter().collect(),
            },
            index_staged_paths,
            index_extra_paths,
            index_plan_applicable,
        }))
    }

    /// Native-only builds have no Git status stream.
    #[cfg(not(feature = "git-overlay"))]
    pub fn git_overlay_short_status(&self) -> Result<Option<GitOverlayShortStatus>> {
        Ok(None)
    }
    fn git_projection_mapping(&self) -> Result<HashMap<String, String>> {
        use objects::sync::LockExt;
        use std::sync::{Mutex, OnceLock};

        // The mapping file only changes when a Git Projection import or
        // bridge lands; status paths re-read it many times per process.
        // Include file identity in the cache key: projection writers replace
        // this file atomically, and equal-size replacements can share an mtime
        // tick on coarse filesystems.
        type MappingIdentity = (u64, u64, i64, i64, u32);
        type MappingCache = Mutex<Option<(PathBuf, MappingIdentity, HashMap<String, String>)>>;
        static CACHE: OnceLock<MappingCache> = OnceLock::new();

        let path = self
            .heddle_dir
            .join("git-projection")
            .join("git-projection-mapping.json");
        let identity = match fs::metadata(&path) {
            Ok(metadata) => crate::stat_signature::stat_signature(&path, &metadata),
            Err(_) => return Ok(HashMap::new()),
        };

        let cache = CACHE.get_or_init(|| Mutex::new(None));
        {
            let cached = cache.lock_or_poisoned();
            if let Some((cached_path, cached_identity, mapping)) = cached.as_ref()
                && *cached_path == path
                && *cached_identity == identity
            {
                return Ok(mapping.clone());
            }
        }

        let contents = fs::read_to_string(&path)?;
        if contents.trim().is_empty() {
            return Ok(HashMap::new());
        }
        let file: GitProjectionMappingFile = serde_json::from_str(&contents)?;
        let mapping: HashMap<String, String> = file
            .entries
            .into_iter()
            .map(|entry| (entry.git_oid, entry.state_id))
            .collect();
        *cache.lock_or_poisoned() = Some((path, identity, mapping.clone()));
        Ok(mapping)
    }

    pub fn git_overlay_ingest_commit_mapping(&self) -> Result<HashMap<String, String>> {
        let path = self.heddle_dir.join("ingest").join("sha_map.sqlite");
        if !path.exists() {
            return Ok(HashMap::new());
        }

        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| {
            HeddleError::Config(format!(
                "failed to open ingest SHA map at '{}': {}",
                path.display(),
                error
            ))
        })?;
        let mut stmt = conn
            .prepare_cached("SELECT git_sha, heddle_repr FROM sha_map WHERE kind = 0")
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to read ingest SHA map at '{}': {}",
                    path.display(),
                    error
                ))
            })?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| {
                HeddleError::Config(format!(
                    "failed to enumerate ingest SHA map at '{}': {}",
                    path.display(),
                    error
                ))
            })?;

        let mut mapping = HashMap::new();
        for row in rows {
            let (git_sha, state_id) = row.map_err(|error| {
                HeddleError::Config(format!(
                    "failed to read ingest SHA map row at '{}': {}",
                    path.display(),
                    error
                ))
            })?;
            mapping.insert(git_sha, state_id);
        }
        Ok(mapping)
    }

    fn git_overlay_checkpoint_mapping(&self) -> Result<HashMap<String, String>> {
        Ok(self
            .list_git_checkpoints()?
            .into_iter()
            .map(|record| (record.git_commit, record.state_id))
            .collect())
    }

    fn git_overlay_mapped_state_for_commit(
        &self,
        git_commit: &str,
        projection_mapping: &HashMap<String, String>,
        ingest_mapping: &HashMap<String, String>,
        checkpoint_mapping: &HashMap<String, String>,
    ) -> Result<Option<StateId>> {
        let Some(change) = projection_mapping
            .get(git_commit)
            .or_else(|| ingest_mapping.get(git_commit))
            .or_else(|| checkpoint_mapping.get(git_commit))
        else {
            return Ok(None);
        };
        let state_id = StateId::parse(change).map_err(|error| {
            HeddleError::Config(format!(
                "git commit {git_commit} maps to invalid Heddle state id '{change}': {error}"
            ))
        })?;
        if self.store.get_state(&state_id)?.is_some() {
            Ok(Some(state_id))
        } else {
            Ok(None)
        }
    }

    fn git_overlay_mapped_git_commit_for_state_in(
        &self,
        state_id: &StateId,
        mapping: &HashMap<String, String>,
    ) -> Result<Option<String>> {
        for (git_commit, mapped_state) in mapping {
            let mapped_state_id = StateId::parse(mapped_state).map_err(|error| {
                HeddleError::Config(format!(
                    "git commit {git_commit} maps to invalid Heddle state id '{mapped_state}': {error}"
                ))
            })?;
            if mapped_state_id == *state_id {
                return Ok(Some(git_commit.clone()));
            }
        }
        Ok(None)
    }

    pub fn git_overlay_mapped_git_commit_for_state(
        &self,
        state_id: &StateId,
    ) -> Result<Option<String>> {
        let projection_mapping = self.git_projection_mapping()?;
        if let Some(git_commit) =
            self.git_overlay_mapped_git_commit_for_state_in(state_id, &projection_mapping)?
        {
            return Ok(Some(git_commit));
        }

        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
        if let Some(git_commit) =
            self.git_overlay_mapped_git_commit_for_state_in(state_id, &ingest_mapping)?
        {
            return Ok(Some(git_commit));
        }

        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        self.git_overlay_mapped_git_commit_for_state_in(state_id, &checkpoint_mapping)
    }

    pub fn git_overlay_mapped_state_for_git_commit(
        &self,
        git_commit: &str,
    ) -> Result<Option<StateId>> {
        let projection_mapping = self.git_projection_mapping()?;
        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;
        self.git_overlay_mapped_state_for_commit(
            git_commit,
            &projection_mapping,
            &ingest_mapping,
            &checkpoint_mapping,
        )
    }

    pub(super) fn git_overlay_mapped_state_for_git_oid(
        &self,
        git_oid: SleyObjectId,
    ) -> Result<Option<StateId>> {
        self.git_overlay_mapped_state_for_git_commit(&git_oid.to_string())
    }
    /// Count the Git commits reachable from `tip_git_commit` that are not
    /// represented in Heddle state (no Git Projection Mapping, ingest identity
    /// mapping, or checkpoint mapping). The walk prunes at the first mapped
    /// commit on each lineage, so the cost is proportional to the out-of-band
    /// suffix, capped at `GIT_OVERLAY_OUT_OF_BAND_SCAN_LIMIT`.
    ///
    /// Returns `Ok(None)` when the repository is not a Git overlay or the tip
    /// cannot be resolved; callers should degrade to a countless report.
    #[cfg(feature = "git-overlay")]
    pub fn git_overlay_out_of_band_commits(
        &self,
        tip_git_commit: &str,
    ) -> Result<Option<GitOverlayOutOfBandCommits>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }
        let git_repo = match self.git_overlay_sley_repository() {
            Ok(Some(repo)) => repo,
            Ok(None) | Err(_) => return Ok(None),
        };
        let Ok(tip) = SleyObjectId::from_hex(git_repo.object_format(), tip_git_commit) else {
            return Ok(None);
        };

        let projection_mapping = self.git_projection_mapping()?;
        let ingest_mapping = self.git_overlay_ingest_commit_mapping()?;
        let checkpoint_mapping = self.git_overlay_checkpoint_mapping()?;

        let mut pending = vec![tip];
        let mut visited = std::collections::HashSet::new();
        let mut count = 0usize;
        while let Some(oid) = pending.pop() {
            if !visited.insert(oid) {
                continue;
            }
            let git_commit = oid.to_string();
            if self
                .git_overlay_mapped_state_for_commit(
                    &git_commit,
                    &projection_mapping,
                    &ingest_mapping,
                    &checkpoint_mapping,
                )?
                .is_some()
            {
                // Mapped into Heddle: this lineage is reconciled; stop here.
                continue;
            }
            count += 1;
            if count >= GIT_OVERLAY_OUT_OF_BAND_SCAN_LIMIT {
                return Ok(Some(GitOverlayOutOfBandCommits {
                    count,
                    truncated: true,
                }));
            }
            let Ok(commit) = git_repo.read_commit(&oid) else {
                continue;
            };
            for parent in commit.parents {
                pending.push(parent);
            }
        }
        Ok(Some(GitOverlayOutOfBandCommits {
            count,
            truncated: false,
        }))
    }
    pub fn git_overlay_current_branch(&self) -> Result<Option<String>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }

        match detect_git_head_state(&self.root)? {
            Some(GitHeadState::Attached(branch)) => return Ok(Some(branch)),
            Some(GitHeadState::Detached(_)) | None => {}
        }

        detect_git_in_progress_branch(&self.root)
    }

    pub fn git_overlay_head_is_detached(&self) -> Result<bool> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(false);
        }

        Ok(matches!(
            detect_git_head_state(&self.root)?,
            Some(GitHeadState::Detached(_))
        ))
    }

    pub fn git_overlay_detached_head_commit(&self) -> Result<Option<String>> {
        if self.capability() != RepositoryCapability::GitOverlay {
            return Ok(None);
        }

        Ok(match detect_git_head_state(&self.root)? {
            Some(GitHeadState::Detached(git_oid)) => Some(git_oid.to_string()),
            Some(GitHeadState::Attached(_)) | None => None,
        })
    }

    fn git_overlay_commit_tip_oid(
        &self,
        git_repo: &SleyRepository,
        reference: &sley::plumbing::sley_refs::Ref,
        ref_kind: &str,
        ref_name: &str,
    ) -> Result<Option<SleyObjectId>> {
        let target = match &reference.target {
            SleyRefTarget::Direct(oid) => *oid,
            SleyRefTarget::Symbolic(_) => return Ok(None),
        };
        let target = match git_repo.peel_to_commit_oid(target) {
            Ok(target) => target,
            Err(_) => return Ok(None),
        };

        let _ = (ref_kind, ref_name);
        Ok(Some(target))
    }
    pub fn list_git_checkpoints(&self) -> Result<Vec<GitCheckpointRecord>> {
        let path = self.root.join(".heddle/state").join(GIT_CHECKPOINTS_FILE);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let contents = fs::read_to_string(path)?;
        if contents.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_str(&contents)?)
    }

    pub fn latest_git_checkpoint_for_state(
        &self,
        state_id: &StateId,
    ) -> Result<Option<GitCheckpointRecord>> {
        let full_id = state_id.to_string_full();
        Ok(self
            .list_git_checkpoints()?
            .into_iter()
            .rev()
            .find(|record| record.state_id == full_id))
    }

    pub fn record_git_checkpoint(
        &self,
        state_id: &StateId,
        git_commit: impl Into<String>,
        summary: impl Into<String>,
    ) -> Result<GitCheckpointRecord> {
        let mut records = self.list_git_checkpoints()?;
        let git_commit = git_commit.into();
        if let Some(existing) = records.iter().rev().find(|record| {
            record.state_id == state_id.to_string_full() && record.git_commit == git_commit
        }) {
            return Ok(existing.clone());
        }
        let record = GitCheckpointRecord {
            state_id: state_id.to_string_full(),
            git_commit,
            summary: summary.into(),
            committed_at: Utc::now().to_rfc3339(),
        };
        let path = self.root.join(".heddle/state").join(GIT_CHECKPOINTS_FILE);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        records.push(record.clone());
        write_file_atomic(&path, serde_json::to_string_pretty(&records)?.as_bytes())?;
        Ok(record)
    }

    fn git_checkpoint_intent_path(&self) -> PathBuf {
        self.root
            .join(".heddle/state")
            .join(GIT_CHECKPOINT_INTENT_FILE)
    }

    pub fn pending_git_checkpoint_intent(&self) -> Result<Option<GitCheckpointIntent>> {
        let path = self.git_checkpoint_intent_path();
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(serde_json::from_str(&contents)?))
    }

    pub fn begin_git_checkpoint_intent(
        &self,
        intent: &GitCheckpointIntent,
    ) -> Result<GitCheckpointIntent> {
        if intent.version != 1 || intent.phase != GitCheckpointIntentPhase::Prepared {
            return Err(HeddleError::InvalidObject(
                "new Git checkpoint intent must be prepared v1".to_string(),
            ));
        }
        if let Some(existing) = self.pending_git_checkpoint_intent()? {
            let same_operation = existing.version == intent.version
                && existing.state_id == intent.state_id
                && existing.branch == intent.branch
                && existing.previous_git_oid == intent.previous_git_oid
                && existing.new_git_oid == intent.new_git_oid
                && existing.summary == intent.summary;
            if same_operation {
                return Ok(existing);
            }
            return Err(HeddleError::Config(format!(
                "Git checkpoint {} -> {} is still pending on branch '{}'; retry that checkpoint before starting another",
                existing.previous_git_oid.as_deref().unwrap_or("<unborn>"),
                existing.new_git_oid,
                existing.branch
            )));
        }
        let path = self.git_checkpoint_intent_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_file_atomic(&path, serde_json::to_string_pretty(intent)?.as_bytes())?;
        Ok(intent.clone())
    }

    pub fn mark_git_checkpoint_published(
        &self,
        state_id: &StateId,
        git_oid: &str,
    ) -> Result<GitCheckpointIntent> {
        let mut intent = self.pending_git_checkpoint_intent()?.ok_or_else(|| {
            HeddleError::Config("Git checkpoint intent disappeared before publish".to_string())
        })?;
        if intent.state_id != state_id.to_string_full() || intent.new_git_oid != git_oid {
            return Err(HeddleError::Config(
                "Git checkpoint publish does not match the durable intent".to_string(),
            ));
        }
        intent.phase = GitCheckpointIntentPhase::Published;
        write_file_atomic(
            &self.git_checkpoint_intent_path(),
            serde_json::to_string_pretty(&intent)?.as_bytes(),
        )?;
        Ok(intent)
    }

    pub fn finish_git_checkpoint_intent(&self, state_id: &StateId, git_oid: &str) -> Result<()> {
        let Some(intent) = self.pending_git_checkpoint_intent()? else {
            return Ok(());
        };
        if intent.state_id != state_id.to_string_full() || intent.new_git_oid != git_oid {
            return Err(HeddleError::Config(
                "cannot finalize a Git checkpoint that does not match the durable intent"
                    .to_string(),
            ));
        }
        let path = self.git_checkpoint_intent_path();
        fs::remove_file(&path)?;
        if let Some(parent) = path.parent() {
            objects::fs_atomic::sync_directory(parent)?;
        }
        Ok(())
    }
}

pub(super) fn ensure_git_overlay_exclude(root: &Path) -> Result<()> {
    let Some(git) = open_git_repository_at_root(root)? else {
        return Ok(());
    };
    let git_dir = git.git_dir();

    let info_dir = git_dir.join("info");
    fs::create_dir_all(&info_dir).map_err(|error| {
        HeddleError::Io(enrich_fs_error(
            &info_dir,
            "creating Git metadata directory",
            error,
        ))
    })?;
    let exclude_path = info_dir.join("exclude");
    let mut contents = match fs::read_to_string(&exclude_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(HeddleError::Io(enrich_fs_error(
                &exclude_path,
                "reading Git exclude file",
                error,
            )));
        }
    };
    let existing_lines = contents.lines().map(str::trim).collect::<BTreeSet<_>>();
    let mut missing = Vec::new();
    for pattern in GIT_OVERLAY_LOCAL_EXCLUDE_PATTERNS {
        if !existing_lines
            .iter()
            .any(|line| git_overlay_exclude_line_matches(line, pattern))
        {
            missing.push(*pattern);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str("# Heddle local metadata\n");
    for pattern in missing {
        contents.push_str(pattern);
        contents.push('\n');
    }
    fs::write(&exclude_path, contents).map_err(|error| {
        HeddleError::Io(enrich_fs_error(
            &exclude_path,
            "writing Git exclude file",
            error,
        ))
    })?;
    Ok(())
}

fn git_overlay_exclude_line_matches(line: &str, pattern: &str) -> bool {
    line == pattern
        || matches!(
            (line, pattern),
            (".heddle", ".heddle/") | ("/.heddle/", ".heddle/") | ("/.heddle", ".heddle/")
        )
}
#[cfg(feature = "git-overlay")]
fn ignored_git_overlay_status_path(path: &str) -> bool {
    path == ".heddle" || path.starts_with(".heddle/")
}

#[cfg(feature = "git-overlay")]
const GIT_MODE_COMMIT: u32 = 0o160000;

#[cfg(feature = "git-overlay")]
fn git_worktree_matches_repo_root(git: &SleyRepository, root: &Path) -> bool {
    git.workdir().is_some_and(
        |workdir| match (workdir.canonicalize(), root.canonicalize()) {
            (Ok(workdir), Ok(root)) => workdir == root,
            _ => false,
        },
    )
}

#[cfg(feature = "git-overlay")]
fn append_short_status_to_index_intent(
    staged_paths: &mut Vec<String>,
    extra_paths: &mut Vec<String>,
    ignore_matcher: &objects::worktree::WorktreeIgnoreMatcher,
    entry: sley::ShortStatusRow<'_>,
    path: &str,
) {
    if entry.index == b'?' && entry.worktree == b'?' {
        if !ignore_matcher.is_ignored(Path::new(path)) {
            extra_paths.push(format!("untracked: {path}"));
        }
        return;
    }
    if entry.index != b' ' && entry.index != b'!' {
        staged_paths.push(path.to_string());
    }
    if entry.worktree != b' '
        && entry.worktree != b'!'
        && !status_row_is_gitlink_worktree_only(entry)
    {
        extra_paths.push(format!("unstaged: {path}"));
    }
}

#[cfg(feature = "git-overlay")]
fn status_row_is_gitlink_worktree_only(entry: sley::ShortStatusRow<'_>) -> bool {
    entry.index == b' '
        && (entry.index_mode == Some(GIT_MODE_COMMIT)
            || entry.head_mode == Some(GIT_MODE_COMMIT)
            || entry.worktree_mode == Some(GIT_MODE_COMMIT))
}

#[cfg(feature = "git-overlay")]
fn git_overlay_untracked_path_ignored(
    ignore_matcher: &crate::worktree_ignore::WorktreeIgnoreMatcher,
    path: &Path,
) -> bool {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    ignore_matcher.should_prune_directory_child(parent, name)
}
fn git_remote_names(root: &Path) -> Result<Vec<String>> {
    let Some(repo) = open_git_repository_at_root(root)? else {
        return Ok(Vec::new());
    };
    repo.remote_names()
        .map(|names| {
            names
                .into_iter()
                .filter(|name| !name.trim().is_empty())
                .collect()
        })
        .map_err(|error| HeddleError::Config(error.to_string()))
}

fn git_resolve_oid(repo: &SleyRepository, rev: &str) -> Result<Option<SleyObjectId>> {
    match repo.rev_parse(rev) {
        Ok(id) => Ok(Some(id)),
        Err(_) => Ok(None),
    }
}

fn git_configured_tracking_ref(repo: &SleyRepository, branch: &str) -> Result<Option<String>> {
    let config = repo
        .config_snapshot()
        .map_err(|error| HeddleError::Config(error.to_string()))?;
    let Some(remote) = config.get("branch", Some(branch), "remote") else {
        return Ok(None);
    };
    let Some(merge) = config.get("branch", Some(branch), "merge") else {
        return Ok(None);
    };
    if remote == "." {
        return Ok(Some(merge.to_string()));
    }
    let merge_ref = GitRefName::new(merge);
    if merge_ref.content_namespace() != Some(GitRefContentNamespace::Branch) {
        return Ok(None);
    };
    let Some(short) = merge_ref.short_name() else {
        return Ok(None);
    };
    Ok(Some(GitRefName::remote_branch_full_name(remote, short)))
}

fn git_ahead_behind_counts(
    git: &SleyRepository,
    head: SleyObjectId,
    upstream: SleyObjectId,
) -> Result<(usize, usize)> {
    if upstream == head {
        return Ok((0, 0));
    }
    let (ahead, behind) = git
        .rev_graph()
        .ahead_behind(head, upstream)
        .map_err(|error| HeddleError::Config(error.to_string()))?;
    Ok((ahead, behind))
}

fn git_remote_tracking_display_name(name: &str) -> String {
    name.strip_prefix("refs/remotes/")
        .unwrap_or(name)
        .to_string()
}

fn git_remote_tracking_message(
    branch: &str,
    upstream: &str,
    ahead: usize,
    behind: usize,
    upstream_is_undone_checkpoint: bool,
) -> String {
    if upstream_is_undone_checkpoint && ahead == 0 && behind > 0 {
        return format!(
            "Upstream '{upstream}' still points at a Git commit that was undone locally on branch '{branch}'"
        );
    }
    match (ahead, behind) {
        (0, behind) => format!(
            "Git branch '{}' is behind upstream '{}' by {} commit(s)",
            branch, upstream, behind
        ),
        (ahead, 0) => format!(
            "Git branch '{}' is ahead of upstream '{}' by {} commit(s)",
            branch, upstream, ahead
        ),
        (ahead, behind) => format!(
            "Git branch '{}' has diverged from upstream '{}' (ahead {}, behind {})",
            branch, upstream, ahead, behind
        ),
    }
}

fn git_remote_tracking_next_action(
    ahead: usize,
    behind: usize,
    upstream_is_undone_checkpoint: bool,
) -> String {
    if upstream_is_undone_checkpoint && ahead == 0 && behind > 0 {
        return "heddle push --force".to_string();
    }
    match (ahead, behind) {
        (0, _) => "heddle pull".to_string(),
        (_, 0) => "heddle push".to_string(),
        _ => "heddle pull".to_string(),
    }
}
/// Read git's HEAD via sley's [`SleyRepository::head_state`], including
/// worktree `gitdir:` indirections and detached HEAD.
pub(super) fn detect_git_head_state(path: &Path) -> Result<Option<GitHeadState>> {
    let repo = open_git_repository_at_root(path)?.ok_or_else(|| {
        HeddleError::Config(format!(
            "failed to inspect Git repository rooted at '{}': no valid .git metadata at that root",
            path.display()
        ))
    })?;
    let head = match repo.head_state() {
        Ok(head) => head,
        Err(_) => return Ok(None),
    };

    if head.is_missing() {
        return Ok(None);
    }
    if let Some(name) = head.branch_name() {
        if name.is_empty() {
            return Ok(None);
        }
        return Ok(Some(GitHeadState::Attached(name.to_string())));
    }
    if head.is_detached()
        && let Some(id) = head.oid()
    {
        return Ok(Some(GitHeadState::Detached(id)));
    }
    Ok(None)
}

/// Detect git's current HEAD branch.
pub(super) fn detect_git_head(path: &Path) -> Result<Option<Head>> {
    if let Some(GitHeadState::Attached(thread)) = detect_git_head_state(path)? {
        return Ok(Some(Head::Attached {
            thread: ThreadName::from(thread),
        }));
    }
    Ok(None)
}

pub(super) fn resolve_git_dir(path: &Path) -> Result<PathBuf> {
    let repo = open_git_repository_at_root(path)?.ok_or_else(|| {
        HeddleError::Config(format!(
            "failed to resolve Git directory for repository root '{}': no valid .git metadata at that root",
            path.display()
        ))
    })?;
    Ok(repo.git_dir().to_path_buf())
}

pub(super) fn detect_git_in_progress_branch(path: &Path) -> Result<Option<String>> {
    let git_dir = resolve_git_dir(path)?;
    for marker in ["rebase-merge/head-name", "rebase-apply/head-name"] {
        let branch_path = git_dir.join(marker);
        if !branch_path.exists() {
            continue;
        }
        let raw = fs::read_to_string(&branch_path)?;
        let value = raw.trim();
        let ref_name = GitRefName::new(value);
        if ref_name.content_namespace() == Some(GitRefContentNamespace::Branch)
            && let Some(short) = ref_name.short_name()
        {
            return Ok(Some(short.to_string()));
        }
        if !value.is_empty() {
            return Ok(Some(value.to_string()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    /// The projection-mapping cache must serve fresh parses when an atomic
    /// replacement preserves both the path and mtime.
    #[test]
    fn git_projection_mapping_picks_up_same_mtime_replacement() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();

        let dir = repo.heddle_dir().join("git-projection");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("git-projection-mapping.json");

        let mapping = |state_suffix: &str| {
            format!(
                r#"{{"entries":[{{"git_oid":"abc123","state_id":"{}{state_suffix}"}}]}}"#,
                "0".repeat(64 - state_suffix.len()),
            )
        };

        fs::write(&path, mapping("a")).unwrap();
        let original_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let first = repo.git_projection_mapping().unwrap();
        assert_eq!(
            first.get("abc123").unwrap(),
            &format!("{}a", "0".repeat(63))
        );

        write_file_atomic(&path, mapping("b").as_bytes()).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(original_mtime))
            .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            original_mtime,
            "test requires the replacement to preserve mtime"
        );

        let second = repo.git_projection_mapping().unwrap();
        assert_eq!(
            second.get("abc123").unwrap(),
            &format!("{}b", "0".repeat(63)),
            "changed mapping file must be re-parsed"
        );
    }
}
