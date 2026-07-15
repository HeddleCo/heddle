// SPDX-License-Identifier: Apache-2.0
//! Read-only projection of reachable Git commits into Heddle states.

use std::{collections::HashMap, path::Path};

use objects::{
    object::{Blob, ContentHash, State, StateId, Tree, TreeEntry},
    store::{InMemoryStore, ObjectStore},
    util::{GitTreeNameClassification, classify_git_tree_name, lcs_line_matches, split_text_lines},
};

use crate::{
    GitSource, IngestError,
    git_walk::{CommitEntry, TreeChild, TreeChildKind},
    state_writer::state_from_commit,
};

/// A deterministic, in-memory Heddle view of a Git revision and its ancestry.
/// Constructing it performs no Heddle writes and moves no Git references.
pub struct OverlayHistory {
    git: GitSource,
    store: InMemoryStore,
    commits: HashMap<String, CommitEntry>,
    states: Vec<(String, State)>,
    git_by_state: HashMap<StateId, String>,
}

/// One target-file line and the Git commit that last introduced it.
pub struct OverlayBlameLine {
    pub content: String,
    pub git_oid: String,
}

impl OverlayHistory {
    pub fn open(root: &Path, revision: &str) -> crate::Result<Self> {
        let git = GitSource::open(root)?;
        let tip = git.resolve_history_revision(revision)?;
        let commits = git.commits_topo([tip])?;
        let store = InMemoryStore::new();
        let mut trees = HashMap::new();
        let mut commits_by_git = HashMap::with_capacity(commits.len());
        let mut states_by_git = HashMap::<String, StateId>::new();
        let mut git_by_state = HashMap::with_capacity(commits.len());
        let mut states = Vec::with_capacity(commits.len());
        for commit in commits {
            let tree = translate_tree(&git, &store, &commit.tree_sha, &mut trees)?;
            let parents = commit
                .parents
                .iter()
                .filter_map(|parent| states_by_git.get(parent).copied())
                .collect();
            let state = state_from_commit(&commit, tree, parents, false)?;
            store.put_state(&state)?;
            states_by_git.insert(commit.sha.clone(), state.state_id);
            git_by_state.insert(state.state_id, commit.sha.clone());
            states.push((commit.sha.clone(), state));
            commits_by_git.insert(commit.sha.clone(), commit);
        }
        states.reverse();
        Ok(Self {
            git,
            store,
            commits: commits_by_git,
            states,
            git_by_state,
        })
    }

    pub fn states(&self) -> &[(String, State)] {
        &self.states
    }

    pub fn tip(&self) -> Option<&(String, State)> {
        self.states.first()
    }

    pub fn source(&self) -> &InMemoryStore {
        &self.store
    }

    pub fn state_id_for_revision(&self, revision: &str) -> crate::Result<StateId> {
        let git_oid = self.git.resolve_history_revision(revision)?;
        self.states
            .iter()
            .find_map(|(candidate, state)| (candidate == &git_oid).then_some(state.state_id))
            .ok_or_else(|| {
                IngestError::Other(format!(
                    "canonical Git history revision '{revision}' is outside the projected graph"
                ))
            })
    }

    pub fn git_oid_for_state(&self, state: &StateId) -> Option<&str> {
        self.git_by_state.get(state).map(String::as_str)
    }

    /// Attribute a UTF-8 file using the same path-targeted, merge-aware LCS
    /// walk as native Heddle blame, while reading Git objects through Sley.
    pub fn blame_file(&self, path: &str) -> crate::Result<Vec<OverlayBlameLine>> {
        let (tip_oid, _) = self
            .tip()
            .ok_or_else(|| IngestError::Other("cannot blame an empty Git history".to_string()))?;
        let tip = self
            .commits
            .get(tip_oid)
            .ok_or_else(|| IngestError::Other(format!("projected Git tip {tip_oid} is missing")))?;
        let (target_blob, target_bytes) = self
            .blob_at_path(&tip.tree_sha, path)?
            .ok_or_else(|| IngestError::Other(format!("path '{path}' is absent from Git tip")))?;
        let target_lines = split_text_lines(&target_bytes)
            .ok_or_else(|| IngestError::Other(format!("path '{path}' is not a UTF-8 text file")))?;
        let mut origins = vec![tip_oid.clone(); target_lines.len()];
        let mut worklist = vec![GitBlameFrontier {
            commit_oid: tip_oid.clone(),
            blob_oid: target_blob,
            commit_lines: target_lines.clone(),
            commit_to_target: (0..target_lines.len()).map(Some).collect(),
        }];

        while let Some(frontier) = worklist.pop() {
            let Some(commit) = self.commits.get(&frontier.commit_oid) else {
                continue;
            };
            let mut moved = vec![false; frontier.commit_lines.len()];
            for parent_oid in &commit.parents {
                let Some(parent) = self.commits.get(parent_oid) else {
                    continue;
                };
                let Some((parent_blob, parent_bytes)) =
                    self.blob_at_path(&parent.tree_sha, path)?
                else {
                    continue;
                };
                if parent_blob == frontier.blob_oid {
                    let mut parent_to_target = vec![None; frontier.commit_lines.len()];
                    let mut any_moved = false;
                    for (index, target_index) in frontier.commit_to_target.iter().enumerate() {
                        if moved[index] {
                            continue;
                        }
                        if let Some(target_index) = target_index {
                            origins[*target_index] = parent_oid.clone();
                            parent_to_target[index] = Some(*target_index);
                            moved[index] = true;
                            any_moved = true;
                        }
                    }
                    if any_moved {
                        worklist.push(GitBlameFrontier {
                            commit_oid: parent_oid.clone(),
                            blob_oid: parent_blob,
                            commit_lines: frontier.commit_lines.clone(),
                            commit_to_target: parent_to_target,
                        });
                    }
                    break;
                }

                let Some(parent_lines) = split_text_lines(&parent_bytes) else {
                    continue;
                };
                let mut parent_to_target = vec![None; parent_lines.len()];
                let mut any_moved = false;
                for (parent_index, frontier_index) in
                    lcs_line_matches(&parent_lines, &frontier.commit_lines)
                {
                    if moved[frontier_index] {
                        continue;
                    }
                    if let Some(target_index) = frontier.commit_to_target[frontier_index] {
                        origins[target_index] = parent_oid.clone();
                        parent_to_target[parent_index] = Some(target_index);
                        moved[frontier_index] = true;
                        any_moved = true;
                    }
                }
                if any_moved {
                    worklist.push(GitBlameFrontier {
                        commit_oid: parent_oid.clone(),
                        blob_oid: parent_blob,
                        commit_lines: parent_lines,
                        commit_to_target: parent_to_target,
                    });
                }
            }
        }

        Ok(target_lines
            .into_iter()
            .zip(origins)
            .map(|(content, git_oid)| OverlayBlameLine { content, git_oid })
            .collect())
    }

    fn blob_at_path(
        &self,
        root_tree: &str,
        path: &str,
    ) -> crate::Result<Option<(String, Vec<u8>)>> {
        let components = path
            .split('/')
            .filter(|component| !component.is_empty())
            .collect::<Vec<_>>();
        if components.is_empty()
            || components
                .iter()
                .any(|component| matches!(*component, "." | ".."))
        {
            return Ok(None);
        }
        let mut tree_oid = root_tree.to_string();
        for (index, component) in components.iter().enumerate() {
            let Some(child) = self
                .git
                .read_tree(&tree_oid)?
                .into_iter()
                .find(|child| child.raw_name == component.as_bytes())
            else {
                return Ok(None);
            };
            let is_leaf = index + 1 == components.len();
            match (is_leaf, child.kind) {
                (false, TreeChildKind::Tree) => tree_oid = child.sha,
                (true, TreeChildKind::Blob { .. }) => {
                    let bytes = self.git.read_blob(&child.sha)?;
                    return Ok(Some((child.sha, bytes)));
                }
                _ => return Ok(None),
            }
        }
        Ok(None)
    }
}

struct GitBlameFrontier {
    commit_oid: String,
    blob_oid: String,
    commit_lines: Vec<String>,
    commit_to_target: Vec<Option<usize>>,
}

fn translate_tree(
    git: &GitSource,
    store: &InMemoryStore,
    git_sha: &str,
    cache: &mut HashMap<String, ContentHash>,
) -> crate::Result<ContentHash> {
    if let Some(hash) = cache.get(git_sha) {
        return Ok(*hash);
    }
    let mut entries = Vec::new();
    for child in git.read_tree(git_sha)? {
        entries.push(translate_child(git, store, &child, cache)?);
    }
    let hash = store.put_tree(&Tree::from_entries(entries))?;
    cache.insert(git_sha.to_string(), hash);
    Ok(hash)
}

fn translate_child(
    git: &GitSource,
    store: &InMemoryStore,
    child: &TreeChild,
    cache: &mut HashMap<String, ContentHash>,
) -> crate::Result<TreeEntry> {
    let name = match classify_git_tree_name(&child.raw_name) {
        GitTreeNameClassification::Representable(name) => name,
        GitTreeNameClassification::NeedsLossy(lossy) => {
            return Err(IngestError::Other(format!(
                "Git path cannot be represented without a lossy import: {} ({})",
                lossy.name, lossy.reason
            )));
        }
    };
    match child.kind {
        TreeChildKind::Blob { executable } => {
            let hash = store.put_blob(&Blob::from_slice(&git.read_blob(&child.sha)?))?;
            TreeEntry::file(name, hash, executable)
        }
        TreeChildKind::Tree => {
            TreeEntry::directory(name, translate_tree(git, store, &child.sha, cache)?)
        }
        TreeChildKind::Symlink => {
            let hash = store.put_blob(&Blob::from_slice(&git.read_blob(&child.sha)?))?;
            TreeEntry::symlink(name, hash)
        }
        TreeChildKind::Gitlink => {
            let target =
                sley::ObjectId::from_hex(git.object_format(), &child.sha).map_err(|error| {
                    IngestError::Git(format!("parse gitlink {}: {error}", child.sha))
                })?;
            TreeEntry::gitlink(name, target)
        }
    }
    .map_err(|error| IngestError::Heddle(error.into()))
}
