// SPDX-License-Identifier: Apache-2.0
//! Read-only projection of reachable Git commits into Heddle states.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

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
    commits: HashMap<String, CommitEntry>,
    states: Vec<(String, State)>,
}

/// A bounded projection of one Git commit plus the stable descriptor IDs of
/// its direct parents. Descriptor IDs intentionally use the same parentless
/// state shape as lazy Git-overlay binding; computing `show <revision>` never
/// needs to walk or materialize the revision's complete ancestry.
pub struct OverlayTipProjection {
    pub git_oid: String,
    pub state: State,
    pub parent_ids: Vec<StateId>,
}

/// One bounded, descriptor-shaped commit returned by an unbound overlay log.
pub struct OverlayLogEntry {
    pub git_oid: String,
    pub state: State,
    pub parent_ids: Vec<StateId>,
}

/// One target-file line and the Git commit that last introduced it.
pub struct OverlayBlameLine {
    pub content: String,
    pub git_oid: String,
}

impl OverlayHistory {
    pub fn project_tip(root: &Path, revision: &str) -> crate::Result<OverlayTipProjection> {
        let git = GitSource::open(root)?;
        let git_oid = git.resolve_history_revision(revision)?;
        let store = InMemoryStore::new();
        let mut trees = HashMap::new();
        let mut states = HashMap::new();
        let (state, parent_ids) =
            project_descriptor_commit(&git, &store, &git_oid, &mut trees, &mut states)?;
        Ok(OverlayTipProjection {
            git_oid,
            state,
            parent_ids,
        })
    }

    pub fn project_log(
        root: &Path,
        revision: &str,
        limit: usize,
        since: Option<&str>,
        agent_model_substring: Option<&str>,
        paths: &[String],
    ) -> crate::Result<Vec<OverlayLogEntry>> {
        let git = GitSource::open(root)?;
        let tip = git.resolve_history_revision(revision)?;
        let mut current = Some(tip.clone());
        let stop_at = since
            .map(|revision| git.resolve_history_revision(revision))
            .transpose()?;
        if let (Some(stop_at), Some(since_revision)) = (stop_at.as_ref(), since)
            && !commit_is_reachable(&git, &tip, stop_at)?
        {
            return Err(IngestError::Other(format!(
                "canonical Git history revision '{since_revision}' is outside the projected graph"
            )));
        }
        let store = InMemoryStore::new();
        let mut trees = HashMap::new();
        let mut states = HashMap::new();
        let mut entries = Vec::new();

        while entries.len() < limit {
            let Some(git_oid) = current else {
                break;
            };
            if stop_at.as_ref() == Some(&git_oid) {
                break;
            }
            let commit = git.read_commit(&git_oid)?;
            current = commit.parents.first().cloned();
            if !paths.is_empty() && !commit_touches_paths(&git, &commit, paths)? {
                continue;
            }
            let (state, parent_ids) =
                project_descriptor_commit(&git, &store, &git_oid, &mut trees, &mut states)?;
            if let Some(filter) = agent_model_substring
                && !state
                    .attribution
                    .agent
                    .as_ref()
                    .is_some_and(|agent| agent.model.contains(filter))
            {
                continue;
            }
            entries.push(OverlayLogEntry {
                git_oid,
                state,
                parent_ids,
            });
        }
        Ok(entries)
    }

    pub fn open(root: &Path, revision: &str) -> crate::Result<Self> {
        let git = GitSource::open(root)?;
        let tip = git.resolve_history_revision(revision)?;
        let commits = git.commits_topo([tip])?;
        let store = InMemoryStore::new();
        let mut trees = HashMap::new();
        let mut commits_by_git = HashMap::with_capacity(commits.len());
        let mut states = Vec::with_capacity(commits.len());
        for commit in commits {
            let tree = translate_tree(&git, &store, &commit.tree_sha, &mut trees)?;
            let state = state_from_commit(&commit, tree, Vec::new(), false)?;
            states.push((commit.sha.clone(), state));
            commits_by_git.insert(commit.sha.clone(), commit);
        }
        states.reverse();
        Ok(Self {
            git,
            commits: commits_by_git,
            states,
        })
    }

    pub fn states(&self) -> &[(String, State)] {
        &self.states
    }

    pub fn tip(&self) -> Option<&(String, State)> {
        self.states.first()
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

fn commit_is_reachable(git: &GitSource, tip: &str, target: &str) -> crate::Result<bool> {
    let mut pending = vec![tip.to_string()];
    let mut seen = HashSet::new();
    while let Some(git_oid) = pending.pop() {
        if git_oid == target {
            return Ok(true);
        }
        if !seen.insert(git_oid.clone()) {
            continue;
        }
        pending.extend(git.read_commit(&git_oid)?.parents);
    }
    Ok(false)
}

fn project_descriptor_commit(
    git: &GitSource,
    store: &InMemoryStore,
    git_oid: &str,
    trees: &mut HashMap<String, ContentHash>,
    states: &mut HashMap<String, State>,
) -> crate::Result<(State, Vec<StateId>)> {
    let commit = git.read_commit(git_oid)?;
    let state = project_descriptor_state(git, store, &commit, trees, states)?;
    let mut parent_ids = Vec::with_capacity(commit.parents.len());
    for parent_oid in &commit.parents {
        let parent = git.read_commit(parent_oid)?;
        parent_ids.push(project_descriptor_state(git, store, &parent, trees, states)?.state_id);
    }
    Ok((state, parent_ids))
}

fn project_descriptor_state(
    git: &GitSource,
    store: &InMemoryStore,
    commit: &CommitEntry,
    trees: &mut HashMap<String, ContentHash>,
    states: &mut HashMap<String, State>,
) -> crate::Result<State> {
    if let Some(state) = states.get(&commit.sha) {
        return Ok(state.clone());
    }
    let tree = translate_tree(git, store, &commit.tree_sha, trees)?;
    let state = state_from_commit(commit, tree, Vec::new(), false)?;
    states.insert(commit.sha.clone(), state.clone());
    Ok(state)
}

fn commit_touches_paths(
    git: &GitSource,
    commit: &CommitEntry,
    paths: &[String],
) -> crate::Result<bool> {
    let parent_tree = commit
        .parents
        .first()
        .map(|parent| git.read_commit(parent).map(|parent| parent.tree_sha))
        .transpose()?;
    for path in paths {
        let current = tree_entry_at_path(git, &commit.tree_sha, path)?;
        let parent = parent_tree
            .as_deref()
            .map(|tree| tree_entry_at_path(git, tree, path))
            .transpose()?
            .flatten();
        if current != parent {
            return Ok(true);
        }
    }
    Ok(false)
}

fn tree_entry_at_path(
    git: &GitSource,
    root_tree: &str,
    path: &str,
) -> crate::Result<Option<TreeChild>> {
    let components = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect::<Vec<_>>();
    if components.is_empty() || components.contains(&"..") {
        return Ok(None);
    }
    let mut tree_oid = root_tree.to_string();
    for (index, component) in components.iter().enumerate() {
        let Some(child) = git
            .read_tree(&tree_oid)?
            .into_iter()
            .find(|child| child.raw_name == component.as_bytes())
        else {
            return Ok(None);
        };
        if index + 1 == components.len() {
            return Ok(Some(child));
        }
        if child.kind != TreeChildKind::Tree {
            return Ok(None);
        }
        tree_oid = child.sha;
    }
    Ok(None)
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

#[cfg(test)]
mod tests {
    use std::{io::Write, process::Command};

    use tempfile::TempDir;

    use super::*;

    fn git(path: &Path, args: &[&str], input: Option<&[u8]>) -> String {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(path)
            .stdin(if input.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("GIT_AUTHOR_NAME", "Overlay Test")
            .env("GIT_AUTHOR_EMAIL", "overlay@example.com")
            .env("GIT_COMMITTER_NAME", "Overlay Test")
            .env("GIT_COMMITTER_EMAIL", "overlay@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        let mut child = command.spawn().expect("spawn git");
        if let Some(input) = input {
            child
                .stdin
                .as_mut()
                .expect("git stdin")
                .write_all(input)
                .expect("write git stdin");
        }
        let output = child.wait_with_output().expect("git output");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git output utf8")
            .trim()
            .to_string()
    }

    #[test]
    fn tip_projection_does_not_walk_beyond_direct_parents() {
        let repo = TempDir::new().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"], None);
        let blob = git(
            repo.path(),
            &["hash-object", "-w", "--stdin"],
            Some(b"ok\n"),
        );
        let mut invalid_tree = format!("100644 blob {blob}\t").into_bytes();
        invalid_tree.extend_from_slice(b"bad\xffname\0");
        let invalid_tree = git(repo.path(), &["mktree", "-z"], Some(&invalid_tree));
        let root = git(
            repo.path(),
            &["commit-tree", &invalid_tree, "-m", "invalid root"],
            None,
        );
        let safe_tree = git(
            repo.path(),
            &["mktree"],
            Some(format!("100644 blob {blob}\tsafe.txt\n").as_bytes()),
        );
        let parent = git(
            repo.path(),
            &["commit-tree", &safe_tree, "-p", &root, "-m", "safe parent"],
            None,
        );
        let tip = git(
            repo.path(),
            &["commit-tree", &safe_tree, "-p", &parent, "-m", "safe tip"],
            None,
        );
        git(repo.path(), &["update-ref", "refs/heads/main", &tip], None);

        let projection = OverlayHistory::project_tip(repo.path(), "HEAD")
            .expect("tip projection must stay bounded to tip + direct parent");
        assert_eq!(projection.git_oid, tip);
        assert_eq!(projection.state.intent.as_deref(), Some("safe tip"));
        assert_eq!(projection.parent_ids.len(), 1);
        let log = OverlayHistory::project_log(repo.path(), "HEAD", 1, None, None, &[])
            .expect("bounded log projection must not inspect older history");
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].state.state_id, projection.state.state_id);
        assert!(
            OverlayHistory::open(repo.path(), "HEAD").is_err(),
            "the full-history control must reach the unrepresentable grandparent"
        );
    }
}
