// SPDX-License-Identifier: Apache-2.0
//! In-memory commit graph index with persistence and Bloom filter support.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use anyhow::Result;
use objects::object::{ChangeId, ContentHash, Tree, diff_trees};
use tracing::warn;

use super::{
    Repository,
    bloom_filter::bloom_insert,
    commit_graph_persistence::{
        PersistedCommitGraphNode, commit_graph_path, load_commit_graph, save_commit_graph,
    },
};

#[derive(Clone, Debug)]
struct CommitGraphNode {
    parents: Vec<ChangeId>,
    generation: usize,
    tree_hash: ContentHash,
    created_at_secs: i64,
    agent_model: Option<String>,
    bloom: Option<[u8; 256]>,
}

/// Cached metadata for a node — cheap to return by value.
pub struct CachedNodeMetadata {
    pub tree_hash: ContentHash,
    pub first_parent: Option<ChangeId>,
    pub agent_model: Option<String>,
    pub created_at_secs: i64,
}

pub struct CommitGraphIndex<'repo> {
    repo: &'repo Repository,
    nodes: HashMap<ChangeId, CommitGraphNode>,
    graph_path: PathBuf,
    persistence_dirty: bool,
}

impl<'repo> CommitGraphIndex<'repo> {
    pub fn new(repo: &'repo Repository) -> Self {
        let graph_path = commit_graph_path(repo.root());
        let (nodes, persistence_dirty) = match load_commit_graph(&graph_path) {
            Ok(Some(nodes)) => (
                nodes
                    .into_iter()
                    .map(|(change_id, node)| {
                        (
                            change_id,
                            CommitGraphNode {
                                parents: node.parents,
                                generation: node.generation,
                                tree_hash: node.tree_hash,
                                created_at_secs: node.created_at_secs,
                                agent_model: node.agent_model,
                                bloom: node.bloom,
                            },
                        )
                    })
                    .collect(),
                false,
            ),
            Ok(None) => (HashMap::new(), false),
            Err(error) => {
                warn!(
                    "Failed to load commit graph from {}: {}. Rebuilding in memory.",
                    graph_path.display(),
                    error
                );
                (HashMap::new(), true)
            }
        };

        Self {
            repo,
            nodes,
            graph_path,
            persistence_dirty,
        }
    }

    /// Return cached metadata for a node if it has been loaded.
    pub fn node_metadata(&self, id: &ChangeId) -> Option<CachedNodeMetadata> {
        self.nodes.get(id).map(|node| CachedNodeMetadata {
            tree_hash: node.tree_hash,
            first_parent: node.parents.first().copied(),
            agent_model: node.agent_model.clone(),
            created_at_secs: node.created_at_secs,
        })
    }

    pub fn is_ancestor(
        &mut self,
        ancestor_id: &ChangeId,
        descendant_id: &ChangeId,
    ) -> Result<bool> {
        if ancestor_id == descendant_id {
            return Ok(true);
        }

        self.ensure_loaded(*descendant_id)?;
        let Some(ancestor_generation) = self.generation(*ancestor_id) else {
            return Ok(false);
        };

        let mut stack = vec![*descendant_id];
        let mut visited = HashSet::new();
        while let Some(change_id) = stack.pop() {
            if !visited.insert(change_id) {
                continue;
            }
            if change_id == *ancestor_id {
                return Ok(true);
            }

            let Some(node) = self.nodes.get(&change_id) else {
                continue;
            };
            for parent in &node.parents {
                if self
                    .generation(*parent)
                    .is_some_and(|generation| generation >= ancestor_generation)
                {
                    stack.push(*parent);
                }
            }
        }

        Ok(false)
    }

    pub fn find_merge_base(
        &mut self,
        state_a: &ChangeId,
        state_b: &ChangeId,
    ) -> Result<Option<ChangeId>> {
        self.ensure_loaded(*state_a)?;
        self.ensure_loaded(*state_b)?;

        let ancestors_a = self.collect_ancestors(*state_a)?;
        let ancestors_b = self.collect_ancestors(*state_b)?;
        let mut common: Vec<ChangeId> = ancestors_a.intersection(&ancestors_b).copied().collect();

        common.sort_by(|left, right| {
            self.generation(*right)
                .cmp(&self.generation(*left))
                .then_with(|| left.as_bytes().cmp(right.as_bytes()))
        });

        Ok(common.into_iter().next())
    }

    fn collect_ancestors(&mut self, start: ChangeId) -> Result<HashSet<ChangeId>> {
        self.ensure_loaded(start)?;

        let mut ancestors = HashSet::new();
        let mut stack = vec![start];
        while let Some(change_id) = stack.pop() {
            if !ancestors.insert(change_id) {
                continue;
            }
            if let Some(node) = self.nodes.get(&change_id) {
                stack.extend(node.parents.iter().copied());
            }
        }

        Ok(ancestors)
    }

    pub fn ensure_loaded(&mut self, change_id: ChangeId) -> Result<()> {
        if self.nodes.contains_key(&change_id) {
            return Ok(());
        }

        let initial_len = self.nodes.len();
        let mut expanded = HashSet::new();
        let mut stack = vec![change_id];
        while let Some(current) = stack.pop() {
            if self.nodes.contains_key(&current) {
                continue;
            }

            if expanded.insert(current) {
                stack.push(current);
                let (parents, _, _, _) = self.load_state_data(current)?;
                for parent in &parents {
                    if !self.nodes.contains_key(parent) {
                        stack.push(*parent);
                    }
                }
                continue;
            }

            let (parents, tree_hash, created_at_secs, agent_model) =
                self.load_state_data(current)?;
            let generation = parents
                .iter()
                .filter_map(|parent| self.generation(*parent))
                .max()
                .map_or(0, |generation| generation + 1);
            self.nodes.insert(
                current,
                CommitGraphNode {
                    parents,
                    generation,
                    tree_hash,
                    created_at_secs,
                    agent_model,
                    bloom: None,
                },
            );
        }

        if self.nodes.len() != initial_len {
            self.persistence_dirty = true;
        }
        self.persist_if_dirty();

        Ok(())
    }

    /// Compute and cache the Bloom filter for the given node.
    pub fn ensure_bloom_populated(&mut self, id: ChangeId) -> Result<()> {
        if self
            .nodes
            .get(&id)
            .map(|n| n.bloom.is_some())
            .unwrap_or(false)
        {
            return Ok(());
        }

        self.ensure_loaded(id)?;
        let Some(node) = self.nodes.get(&id) else {
            return Ok(());
        };
        let state_tree = node.tree_hash;
        let parent_id = node.parents.first().copied();

        let parent_tree = if let Some(pid) = parent_id {
            self.ensure_loaded(pid)?;
            self.nodes
                .get(&pid)
                .map(|n| n.tree_hash)
                .unwrap_or_else(|| Tree::new().hash())
        } else {
            Tree::new().hash()
        };

        let changes = diff_trees(self.repo.store(), &parent_tree, &state_tree)?;
        let mut bloom = [0u8; 256];
        for change in changes.iter() {
            bloom_insert(&mut bloom, &change.path);
        }
        self.nodes.get_mut(&id).unwrap().bloom = Some(bloom);
        self.persistence_dirty = true;
        self.persist_if_dirty();
        Ok(())
    }

    /// Return the Bloom filter for a node if it has been computed.
    pub fn node_bloom(&self, id: &ChangeId) -> Option<&[u8; 256]> {
        self.nodes.get(id).and_then(|n| n.bloom.as_ref())
    }

    fn load_state_data(
        &self,
        change_id: ChangeId,
    ) -> Result<(Vec<ChangeId>, ContentHash, i64, Option<String>)> {
        match self.repo.store().get_state(&change_id)? {
            Some(state) => Ok((
                state.parents,
                state.tree,
                state.created_at.timestamp(),
                state.attribution.agent.map(|a| a.model),
            )),
            None => Ok((vec![], ContentHash::from_bytes([0; 32]), 0, None)),
        }
    }

    fn generation(&self, change_id: ChangeId) -> Option<usize> {
        self.nodes.get(&change_id).map(|node| node.generation)
    }

    fn persist_if_dirty(&mut self) {
        if !self.persistence_dirty {
            return;
        }

        let persisted_nodes: HashMap<_, _> = self
            .nodes
            .iter()
            .map(|(change_id, node)| {
                (
                    *change_id,
                    PersistedCommitGraphNode {
                        parents: node.parents.clone(),
                        generation: node.generation,
                        tree_hash: node.tree_hash,
                        created_at_secs: node.created_at_secs,
                        agent_model: node.agent_model.clone(),
                        bloom: node.bloom,
                    },
                )
            })
            .collect();

        match save_commit_graph(&self.graph_path, &persisted_nodes) {
            Ok(()) => self.persistence_dirty = false,
            Err(error) => warn!(
                "Failed to persist commit graph to {}: {}",
                self.graph_path.display(),
                error
            ),
        }
    }
}

pub fn find_merge_base(
    repo: &Repository,
    state_a: &ChangeId,
    state_b: &ChangeId,
) -> Result<Option<ChangeId>> {
    let mut graph = CommitGraphIndex::new(repo);
    graph.find_merge_base(state_a, state_b)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::Result;
    use tempfile::TempDir;

    use super::{super::Repository, CommitGraphIndex};

    fn commit_graph_path(repo: &Repository) -> std::path::PathBuf {
        repo.root().join(".heddle/state").join("commit-graph.bin")
    }

    #[test]
    fn commit_graph_detects_ancestor_relationships() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let repo = Repository::init_default(temp_dir.path())?;

        std::fs::write(temp_dir.path().join("file.txt"), "base")?;
        let base = repo.snapshot(Some("base".to_string()), None)?;
        std::fs::write(temp_dir.path().join("file.txt"), "next")?;
        let next = repo.snapshot(Some("next".to_string()), None)?;

        let mut graph = CommitGraphIndex::new(&repo);
        assert!(graph.is_ancestor(&base.change_id, &next.change_id)?);
        assert!(!graph.is_ancestor(&next.change_id, &base.change_id)?);

        Ok(())
    }

    #[test]
    fn commit_graph_prefers_nearest_merge_base() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let repo = Repository::init_default(temp_dir.path())?;

        std::fs::write(temp_dir.path().join("file.txt"), "a")?;
        let state_a = repo.snapshot(Some("a".to_string()), None)?;
        std::fs::write(temp_dir.path().join("file.txt"), "b")?;
        let state_b = repo.snapshot(Some("b".to_string()), None)?;
        std::fs::write(temp_dir.path().join("file.txt"), "c")?;
        let state_c = repo.snapshot(Some("c".to_string()), None)?;

        repo.goto(&state_b.change_id)?;
        std::fs::write(temp_dir.path().join("side.txt"), "d")?;
        let state_d = repo.snapshot(Some("d".to_string()), None)?;

        let mut graph = CommitGraphIndex::new(&repo);
        assert_eq!(
            graph.find_merge_base(&state_c.change_id, &state_d.change_id)?,
            Some(state_b.change_id)
        );
        assert_eq!(
            graph.find_merge_base(&state_a.change_id, &state_d.change_id)?,
            Some(state_a.change_id)
        );

        Ok(())
    }

    #[test]
    fn commit_graph_persists_and_reloads() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let repo = Repository::init_default(temp_dir.path())?;

        fs::write(temp_dir.path().join("file.txt"), "base")?;
        let base = repo.snapshot(Some("base".to_string()), None)?;
        fs::write(temp_dir.path().join("file.txt"), "next")?;
        let next = repo.snapshot(Some("next".to_string()), None)?;

        let path = commit_graph_path(&repo);
        let mut first_graph = CommitGraphIndex::new(&repo);
        assert!(first_graph.is_ancestor(&base.change_id, &next.change_id)?);
        assert!(path.exists());

        let mut reloaded_graph = CommitGraphIndex::new(&repo);
        assert!(reloaded_graph.is_ancestor(&base.change_id, &next.change_id)?);

        Ok(())
    }

    #[test]
    fn commit_graph_recovers_from_missing_and_invalid_files() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let repo = Repository::init_default(temp_dir.path())?;

        fs::write(temp_dir.path().join("file.txt"), "base")?;
        let base = repo.snapshot(Some("base".to_string()), None)?;
        fs::write(temp_dir.path().join("file.txt"), "next")?;
        let next = repo.snapshot(Some("next".to_string()), None)?;

        let path = commit_graph_path(&repo);
        let mut graph = CommitGraphIndex::new(&repo);
        assert!(graph.is_ancestor(&base.change_id, &next.change_id)?);

        fs::remove_file(&path)?;
        let mut missing_graph = CommitGraphIndex::new(&repo);
        assert!(missing_graph.is_ancestor(&base.change_id, &next.change_id)?);
        assert!(path.exists());

        fs::write(&path, b"invalid")?;
        let mut invalid_graph = CommitGraphIndex::new(&repo);
        assert!(invalid_graph.is_ancestor(&base.change_id, &next.change_id)?);

        let bytes = fs::read(&path)?;
        assert!(bytes.starts_with(b"LMGRAPH\0"));

        Ok(())
    }

    #[test]
    fn merge_base_remains_correct_after_graph_reload() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let repo = Repository::init_default(temp_dir.path())?;

        fs::write(temp_dir.path().join("file.txt"), "a")?;
        let state_a = repo.snapshot(Some("a".to_string()), None)?;
        fs::write(temp_dir.path().join("file.txt"), "b")?;
        let state_b = repo.snapshot(Some("b".to_string()), None)?;
        fs::write(temp_dir.path().join("file.txt"), "c")?;
        let state_c = repo.snapshot(Some("c".to_string()), None)?;

        repo.goto(&state_b.change_id)?;
        fs::write(temp_dir.path().join("side.txt"), "d")?;
        let state_d = repo.snapshot(Some("d".to_string()), None)?;

        let mut initial_graph = CommitGraphIndex::new(&repo);
        assert_eq!(
            initial_graph.find_merge_base(&state_c.change_id, &state_d.change_id)?,
            Some(state_b.change_id)
        );

        let mut reloaded_graph = CommitGraphIndex::new(&repo);
        assert_eq!(
            reloaded_graph.find_merge_base(&state_c.change_id, &state_d.change_id)?,
            Some(state_b.change_id)
        );
        assert_eq!(
            reloaded_graph.find_merge_base(&state_a.change_id, &state_d.change_id)?,
            Some(state_a.change_id)
        );

        Ok(())
    }

    #[test]
    fn node_metadata_returns_correct_values_after_ensure_loaded() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let repo = Repository::init_default(temp_dir.path())?;

        fs::write(temp_dir.path().join("file.txt"), "content")?;
        let state = repo.snapshot(Some("snapshot".to_string()), None)?;

        let mut graph = CommitGraphIndex::new(&repo);
        graph.ensure_loaded(state.change_id)?;

        let meta = graph
            .node_metadata(&state.change_id)
            .expect("metadata should be present");
        // The state's tree_hash should match what the graph stores
        assert_eq!(meta.tree_hash, state.tree);
        // first parent is None for the first commit
        assert_eq!(meta.first_parent, state.parents.first().copied());

        Ok(())
    }

    #[test]
    fn bloom_filter_populated_after_ensure_bloom() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let repo = Repository::init_default(temp_dir.path())?;

        fs::write(temp_dir.path().join("alpha.txt"), "alpha")?;
        let state = repo.snapshot(Some("alpha".to_string()), None)?;

        let mut graph = CommitGraphIndex::new(&repo);
        graph.ensure_bloom_populated(state.change_id)?;

        let bloom = graph
            .node_bloom(&state.change_id)
            .expect("bloom should be present");
        // alpha.txt should be in the bloom filter
        use super::super::bloom_filter::bloom_maybe_contains;
        assert!(bloom_maybe_contains(bloom, "alpha.txt"));

        Ok(())
    }
}
