// SPDX-License-Identifier: Apache-2.0
//! In-memory commit graph index with persistence and Bloom filter support.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
#[cfg(feature = "async-source")]
use objects::store::AsyncObjectSource;
use objects::{
    object::{ContentHash, StateId, Tree, diff_trees},
    store::{AnyStore, ObjectSource, ObjectStore},
};
use oplog::OpLogBackend;
use refs::RefBackend;
use tracing::warn;

use super::{
    Repository,
    bloom_filter::bloom_insert,
    commit_graph_persistence::{
        CommitGraphCache, FsCommitGraphCache, LoadedCommitGraph, PersistedCommitGraphNode,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct CommitGraphNode {
    parents: Vec<StateId>,
    generation: usize,
    tree_hash: ContentHash,
    created_at_secs: i64,
    agent_model: Option<String>,
    bloom: Option<[u8; 256]>,
}

type CommitGraphStateData = (Vec<StateId>, ContentHash, i64, Option<String>);

impl CommitGraphNode {
    fn is_legacy_unresolved_placeholder(&self) -> bool {
        self.parents.is_empty()
            && self.tree_hash == ContentHash::from_bytes([0; 32])
            && self.created_at_secs == 0
            && self.agent_model.is_none()
    }
}

#[cfg(feature = "async-source")]
#[derive(Clone, Debug)]
struct AsyncCommitGraphNode {
    parents: Vec<StateId>,
    generation: usize,
}

/// Cached metadata for a node — cheap to return by value.
pub struct CachedNodeMetadata {
    pub tree_hash: ContentHash,
    pub first_parent: Option<StateId>,
    pub agent_model: Option<String>,
    pub created_at_secs: i64,
}

pub struct CommitGraphIndex<'source, S: ObjectSource + ?Sized = AnyStore> {
    source: &'source S,
    nodes: HashMap<StateId, CommitGraphNode>,
    cache: Box<dyn CommitGraphCache + 'source>,
    persistence_dirty: bool,
}

impl<'source, S> CommitGraphIndex<'source, S>
where
    S: ObjectStore,
{
    pub fn new<R, O>(repo: &'source Repository<R, O, S>) -> Self
    where
        R: RefBackend,
        O: OpLogBackend,
    {
        let cache = FsCommitGraphCache::new(&repo.root);
        Self::with_cache(repo.store(), cache)
    }
}

impl<'source, S> CommitGraphIndex<'source, S>
where
    S: ObjectSource + ?Sized,
{
    pub(crate) fn with_cache<C>(source: &'source S, cache: C) -> Self
    where
        C: CommitGraphCache + 'source,
    {
        let (nodes, persistence_dirty) = match cache.load() {
            Ok(Some(nodes)) => {
                let mut nodes = nodes.into_memory_nodes();
                let loaded_len = nodes.len();
                // Older v2 caches encoded an unavailable parent as a real
                // zero-tree root. A zero content hash is not a valid Heddle
                // tree identity, so discard those placeholders and let the
                // object source resolve the edge when it becomes available.
                nodes.retain(|_, node| !node.is_legacy_unresolved_placeholder());
                let removed_placeholder = nodes.len() != loaded_len;
                (nodes, removed_placeholder)
            }
            Ok(None) => (HashMap::new(), false),
            Err(error) => {
                let cache_label = cache_label(&cache);
                warn!(
                    "Failed to load commit graph from {}: {}. Rebuilding in memory.",
                    cache_label, error
                );
                (HashMap::new(), true)
            }
        };

        Self {
            source,
            nodes,
            cache: Box::new(cache),
            persistence_dirty,
        }
    }

    /// Return cached metadata for a node if it has been loaded.
    pub fn node_metadata(&self, id: &StateId) -> Option<CachedNodeMetadata> {
        self.nodes.get(id).map(|node| CachedNodeMetadata {
            tree_hash: node.tree_hash,
            first_parent: node.parents.first().copied(),
            agent_model: node.agent_model.clone(),
            created_at_secs: node.created_at_secs,
        })
    }

    pub fn is_ancestor(&mut self, ancestor_id: &StateId, descendant_id: &StateId) -> Result<bool> {
        if ancestor_id == descendant_id {
            return Ok(true);
        }

        self.ensure_loaded(*descendant_id)?;
        let Some(ancestor_generation) = self.generation(*ancestor_id) else {
            return Ok(false);
        };

        let mut stack = vec![*descendant_id];
        let mut visited = HashSet::new();
        while let Some(state_id) = stack.pop() {
            if !visited.insert(state_id) {
                continue;
            }
            if state_id == *ancestor_id {
                return Ok(true);
            }

            let Some(node) = self.nodes.get(&state_id) else {
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
        state_a: &StateId,
        state_b: &StateId,
    ) -> Result<Option<StateId>> {
        self.ensure_loaded(*state_a)?;
        self.ensure_loaded(*state_b)?;

        let ancestors_a = self.collect_ancestors(*state_a)?;
        let ancestors_b = self.collect_ancestors(*state_b)?;
        let best = ancestors_a
            .intersection(&ancestors_b)
            .copied()
            .max_by(|left, right| {
                self.generation(*left)
                    .cmp(&self.generation(*right))
                    .then_with(|| right.as_bytes().cmp(left.as_bytes()))
            });

        Ok(best)
    }

    fn collect_ancestors(&mut self, start: StateId) -> Result<HashSet<StateId>> {
        self.ensure_loaded(start)?;

        let mut ancestors = HashSet::new();
        let mut stack = vec![start];
        while let Some(state_id) = stack.pop() {
            if !ancestors.insert(state_id) {
                continue;
            }
            if let Some(node) = self.nodes.get(&state_id) {
                stack.extend(node.parents.iter().copied());
            }
        }

        Ok(ancestors)
    }

    pub fn ensure_loaded(&mut self, state_id: StateId) -> Result<()> {
        // Walk the complete cached parent closure even when the requested
        // node itself is already cached. A lazy Git descriptor can name
        // unavailable parents; later adoption must discover those parents
        // and recompute descendant generations without a manual rebuild.
        let mut scheduled = HashSet::new();
        let mut pending: HashMap<StateId, CommitGraphStateData> = HashMap::new();
        let mut stack = vec![(state_id, false)];
        while let Some((current, expanded)) = stack.pop() {
            if expanded {
                let Some((parents, tree_hash, created_at_secs, agent_model)) =
                    pending.remove(&current)
                else {
                    continue;
                };
                let previous = self.nodes.get(&current);
                let bloom = previous
                    .filter(|node| node.parents == parents && node.tree_hash == tree_hash)
                    .and_then(|node| node.bloom);
                let generation = parents
                    .iter()
                    .filter_map(|parent| self.generation(*parent))
                    .max()
                    .map_or(0, |generation| generation + 1);
                let node = CommitGraphNode {
                    parents,
                    generation,
                    tree_hash,
                    created_at_secs,
                    agent_model,
                    bloom,
                };
                if previous != Some(&node) {
                    self.nodes.insert(current, node);
                    self.persistence_dirty = true;
                }
                continue;
            }

            if !scheduled.insert(current) {
                continue;
            }
            let data = if let Some(node) = self.nodes.get(&current) {
                Some((
                    node.parents.clone(),
                    node.tree_hash,
                    node.created_at_secs,
                    node.agent_model.clone(),
                ))
            } else {
                self.load_state_data(current)?
            };
            let Some(data) = data else {
                // An unavailable parent is an unresolved edge, not a real
                // parentless zero-tree node. Never persist a dummy identity
                // that could mask later materialization.
                continue;
            };
            let parents = data.0.clone();
            pending.insert(current, data);
            stack.push((current, true));
            for parent in parents.into_iter().rev() {
                stack.push((parent, false));
            }
        }
        self.persist_if_dirty();

        Ok(())
    }

    /// Compute and cache the Bloom filter for the given node.
    pub fn ensure_bloom_populated(&mut self, id: StateId) -> Result<()> {
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

        let changes = diff_trees(self.source, &parent_tree, &state_tree)?;
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
    pub fn node_bloom(&self, id: &StateId) -> Option<&[u8; 256]> {
        self.nodes.get(id).and_then(|n| n.bloom.as_ref())
    }

    fn load_state_data(&self, state_id: StateId) -> Result<Option<CommitGraphStateData>> {
        Ok(self.source.get_state(&state_id)?.map(|state| {
            (
                state.parents,
                state.tree,
                state.created_at.timestamp(),
                state.attribution.agent.map(|a| a.model),
            )
        }))
    }

    fn generation(&self, state_id: StateId) -> Option<usize> {
        self.nodes.get(&state_id).map(|node| node.generation)
    }

    fn persist_if_dirty(&mut self) {
        if !self.persistence_dirty {
            return;
        }

        let persisted_nodes: HashMap<_, _> = self
            .nodes
            .iter()
            .map(|(state_id, node)| {
                (
                    *state_id,
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

        match self.cache.save(&persisted_nodes) {
            Ok(()) => self.persistence_dirty = false,
            Err(error) => {
                let cache_label = cache_label(self.cache.as_ref());
                warn!(
                    "Failed to persist commit graph to {}: {}",
                    cache_label, error
                );
            }
        }
    }
}

fn cache_label(cache: &(impl CommitGraphCache + ?Sized)) -> String {
    cache
        .path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "commit graph cache".to_string())
}

trait LoadedCommitGraphExt {
    fn into_memory_nodes(self) -> HashMap<StateId, CommitGraphNode>;
}

impl LoadedCommitGraphExt for LoadedCommitGraph {
    fn into_memory_nodes(self) -> HashMap<StateId, CommitGraphNode> {
        self.into_iter()
            .map(|(state_id, node)| {
                (
                    state_id,
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
            .collect()
    }
}

/// Return whether `ancestor_id` is reachable from `descendant_id` using an async object source.
#[cfg(feature = "async-source")]
pub async fn is_ancestor_async<S>(
    source: &S,
    ancestor_id: &StateId,
    descendant_id: &StateId,
) -> Result<bool>
where
    S: AsyncObjectSource + ?Sized,
{
    if ancestor_id == descendant_id {
        return Ok(true);
    }

    let mut nodes = HashMap::new();
    ensure_loaded_async(source, &mut nodes, *descendant_id).await?;
    let Some(ancestor_generation) = generation_async(&nodes, *ancestor_id) else {
        return Ok(false);
    };

    let mut stack = vec![*descendant_id];
    let mut visited = HashSet::new();
    while let Some(state_id) = stack.pop() {
        if !visited.insert(state_id) {
            continue;
        }
        if state_id == *ancestor_id {
            return Ok(true);
        }

        let Some(node) = nodes.get(&state_id) else {
            continue;
        };
        for parent in &node.parents {
            if generation_async(&nodes, *parent)
                .is_some_and(|generation| generation >= ancestor_generation)
            {
                stack.push(*parent);
            }
        }
    }

    Ok(false)
}

/// Find the best common ancestor of two states using an async object source.
#[cfg(feature = "async-source")]
pub async fn find_merge_base_async<S>(
    source: &S,
    state_a: &StateId,
    state_b: &StateId,
) -> Result<Option<StateId>>
where
    S: AsyncObjectSource + ?Sized,
{
    let mut nodes = HashMap::new();
    ensure_loaded_async(source, &mut nodes, *state_a).await?;
    ensure_loaded_async(source, &mut nodes, *state_b).await?;

    let ancestors_a = collect_ancestors_async(source, &mut nodes, *state_a).await?;
    let ancestors_b = collect_ancestors_async(source, &mut nodes, *state_b).await?;
    let best = ancestors_a
        .intersection(&ancestors_b)
        .copied()
        .max_by(|left, right| {
            generation_async(&nodes, *left)
                .cmp(&generation_async(&nodes, *right))
                .then_with(|| right.as_bytes().cmp(left.as_bytes()))
        });

    Ok(best)
}

#[cfg(feature = "async-source")]
async fn collect_ancestors_async<S>(
    source: &S,
    nodes: &mut HashMap<StateId, AsyncCommitGraphNode>,
    start: StateId,
) -> Result<HashSet<StateId>>
where
    S: AsyncObjectSource + ?Sized,
{
    ensure_loaded_async(source, nodes, start).await?;

    let mut ancestors = HashSet::new();
    let mut stack = vec![start];
    while let Some(state_id) = stack.pop() {
        if !ancestors.insert(state_id) {
            continue;
        }
        if let Some(node) = nodes.get(&state_id) {
            stack.extend(node.parents.iter().copied());
        }
    }

    Ok(ancestors)
}

#[cfg(feature = "async-source")]
async fn ensure_loaded_async<S>(
    source: &S,
    nodes: &mut HashMap<StateId, AsyncCommitGraphNode>,
    state_id: StateId,
) -> Result<()>
where
    S: AsyncObjectSource + ?Sized,
{
    let mut expanded = HashSet::new();
    let mut stack = vec![state_id];
    while let Some(current) = stack.pop() {
        if nodes.contains_key(&current) {
            continue;
        }

        if expanded.insert(current) {
            stack.push(current);
            let Some(parents) = load_state_parents_async(source, current).await? else {
                continue;
            };
            for parent in &parents {
                if !nodes.contains_key(parent) {
                    stack.push(*parent);
                }
            }
            continue;
        }

        let Some(parents) = load_state_parents_async(source, current).await? else {
            continue;
        };
        let generation = parents
            .iter()
            .filter_map(|parent| generation_async(nodes, *parent))
            .max()
            .map_or(0, |generation| generation + 1);
        nodes.insert(
            current,
            AsyncCommitGraphNode {
                parents,
                generation,
            },
        );
    }

    Ok(())
}

#[cfg(feature = "async-source")]
async fn load_state_parents_async<S>(source: &S, state_id: StateId) -> Result<Option<Vec<StateId>>>
where
    S: AsyncObjectSource + ?Sized,
{
    Ok(source
        .get_state(&state_id)
        .await?
        .map(|state| state.parents))
}

#[cfg(feature = "async-source")]
fn generation_async(
    nodes: &HashMap<StateId, AsyncCommitGraphNode>,
    state_id: StateId,
) -> Option<usize> {
    nodes.get(&state_id).map(|node| node.generation)
}

pub fn find_merge_base<R, O, S>(
    repo: &Repository<R, O, S>,
    state_a: &StateId,
    state_b: &StateId,
) -> Result<Option<StateId>>
where
    R: RefBackend,
    O: OpLogBackend,
    S: ObjectStore,
{
    let mut graph = CommitGraphIndex::new(repo);
    graph.find_merge_base(state_a, state_b)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::Result;
    use objects::{
        object::{Attribution, ContentHash, Principal, State, Tree},
        store::ObjectStore,
    };
    #[cfg(feature = "async-source")]
    use objects::{
        object::{Blob, StateId},
        store::{AsyncObjectSource, InMemoryStore},
    };
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
        assert!(graph.is_ancestor(&base.id(), &next.id())?);
        assert!(!graph.is_ancestor(&next.id(), &base.id())?);

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

        repo.goto(&state_b.id())?;
        std::fs::write(temp_dir.path().join("side.txt"), "d")?;
        let state_d = repo.snapshot(Some("d".to_string()), None)?;

        let mut graph = CommitGraphIndex::new(&repo);
        assert_eq!(
            graph.find_merge_base(&state_c.id(), &state_d.id())?,
            Some(state_b.id())
        );
        assert_eq!(
            graph.find_merge_base(&state_a.id(), &state_d.id())?,
            Some(state_a.id())
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
        assert!(first_graph.is_ancestor(&base.id(), &next.id())?);
        assert!(path.exists());

        let mut reloaded_graph = CommitGraphIndex::new(&repo);
        assert!(reloaded_graph.is_ancestor(&base.id(), &next.id())?);

        Ok(())
    }

    #[test]
    fn commit_graph_reloads_parent_edges_materialized_after_lazy_tip() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let repo = Repository::init_default(temp_dir.path())?;
        let attribution = Attribution::human(Principal::new("Test User", "test@example.com"));
        let parent = State::new(Tree::new().hash(), vec![], attribution.clone());
        let tip = State::new(Tree::new().hash(), vec![parent.id()], attribution);
        repo.store().put_state(&tip)?;

        let mut lazy_graph = CommitGraphIndex::new(&repo);
        lazy_graph.ensure_loaded(tip.id())?;
        assert!(lazy_graph.node_metadata(&parent.id()).is_none());
        drop(lazy_graph);

        // Emulate the exact poisoned v2 cache shape written by older builds,
        // so reopening also proves the on-disk upgrade path rather than only
        // the new no-placeholder write path.
        let graph_path = commit_graph_path(&repo);
        let mut persisted = super::super::commit_graph_persistence::load_commit_graph(&graph_path)?
            .expect("lazy graph cache");
        persisted.insert(
            parent.id(),
            super::super::commit_graph_persistence::PersistedCommitGraphNode {
                parents: vec![],
                generation: 0,
                tree_hash: ContentHash::from_bytes([0; 32]),
                created_at_secs: 0,
                agent_model: None,
                bloom: None,
            },
        );
        super::super::commit_graph_persistence::save_commit_graph(&graph_path, &persisted)?;

        repo.store().put_state(&parent)?;
        let mut materialized_graph = CommitGraphIndex::new(&repo);
        assert!(materialized_graph.is_ancestor(&parent.id(), &tip.id())?);
        assert_eq!(
            materialized_graph.find_merge_base(&parent.id(), &tip.id())?,
            Some(parent.id())
        );
        assert_eq!(
            materialized_graph
                .node_metadata(&parent.id())
                .expect("materialized parent metadata")
                .tree_hash,
            parent.tree
        );

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
        assert!(graph.is_ancestor(&base.id(), &next.id())?);

        fs::remove_file(&path)?;
        let mut missing_graph = CommitGraphIndex::new(&repo);
        assert!(missing_graph.is_ancestor(&base.id(), &next.id())?);
        assert!(path.exists());

        fs::write(&path, b"invalid")?;
        let mut invalid_graph = CommitGraphIndex::new(&repo);
        assert!(invalid_graph.is_ancestor(&base.id(), &next.id())?);

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

        repo.goto(&state_b.id())?;
        fs::write(temp_dir.path().join("side.txt"), "d")?;
        let state_d = repo.snapshot(Some("d".to_string()), None)?;

        let mut initial_graph = CommitGraphIndex::new(&repo);
        assert_eq!(
            initial_graph.find_merge_base(&state_c.id(), &state_d.id())?,
            Some(state_b.id())
        );

        let mut reloaded_graph = CommitGraphIndex::new(&repo);
        assert_eq!(
            reloaded_graph.find_merge_base(&state_c.id(), &state_d.id())?,
            Some(state_b.id())
        );
        assert_eq!(
            reloaded_graph.find_merge_base(&state_a.id(), &state_d.id())?,
            Some(state_a.id())
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
        graph.ensure_loaded(state.id())?;

        let meta = graph
            .node_metadata(&state.id())
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
        graph.ensure_bloom_populated(state.id())?;

        let bloom = graph
            .node_bloom(&state.id())
            .expect("bloom should be present");
        // alpha.txt should be in the bloom filter
        use super::super::bloom_filter::bloom_maybe_contains;
        assert!(bloom_maybe_contains(bloom, "alpha.txt"));

        Ok(())
    }

    #[cfg(feature = "async-source")]
    #[test]
    fn async_ancestry_matches_sync_commit_graph_on_fixture() {
        let store = InMemoryStore::new();

        let root = put_state(&store, vec![]);
        let left_base = put_state(&store, vec![root.id()]);
        let right_base = put_state(&store, vec![root.id()]);
        let left_tip = put_state(&store, vec![left_base.id()]);
        let right_tip = put_state(&store, vec![right_base.id()]);
        let merge_a = put_state(&store, vec![left_base.id(), right_base.id()]);
        let merge_b = put_state(&store, vec![right_base.id(), left_base.id()]);

        let async_source = AsyncInMemorySource(&store);
        for (ancestor, descendant) in [
            (root.id(), merge_a.id()),
            (left_base.id(), merge_a.id()),
            (merge_a.id(), merge_a.id()),
            (merge_a.id(), merge_b.id()),
            (left_tip.id(), right_tip.id()),
        ] {
            let mut graph = CommitGraphIndex::with_cache(
                &store,
                super::super::commit_graph_persistence::NullCommitGraphCache,
            );
            let sync = graph.is_ancestor(&ancestor, &descendant).unwrap();
            let async_result = block_on(super::is_ancestor_async(
                &async_source,
                &ancestor,
                &descendant,
            ))
            .unwrap();
            assert_eq!(
                async_result, sync,
                "is_ancestor mismatch for {ancestor} -> {descendant}"
            );
        }

        for (left, right) in [
            (merge_a.id(), merge_b.id()),
            (left_base.id(), merge_a.id()),
            (left_tip.id(), right_tip.id()),
            (merge_a.id(), merge_a.id()),
        ] {
            let mut graph = CommitGraphIndex::with_cache(
                &store,
                super::super::commit_graph_persistence::NullCommitGraphCache,
            );
            let sync = graph.find_merge_base(&left, &right).unwrap();
            let async_result =
                block_on(super::find_merge_base_async(&async_source, &left, &right)).unwrap();
            assert_eq!(
                async_result, sync,
                "find_merge_base mismatch for {left} and {right}"
            );
        }

        let mut graph = CommitGraphIndex::with_cache(
            &store,
            super::super::commit_graph_persistence::NullCommitGraphCache,
        );
        let tie_base = graph.find_merge_base(&merge_a.id(), &merge_b.id()).unwrap();
        assert!(matches!(tie_base, Some(id) if id == left_base.id() || id == right_base.id()));
    }

    #[cfg(feature = "async-source")]
    struct AsyncInMemorySource<'a>(&'a InMemoryStore);

    #[cfg(feature = "async-source")]
    impl AsyncObjectSource for AsyncInMemorySource<'_> {
        async fn get_tree(&self, hash: &ContentHash) -> objects::error::Result<Option<Tree>> {
            ObjectStore::get_tree(self.0, hash)
        }

        async fn get_state(&self, id: &StateId) -> objects::error::Result<Option<State>> {
            ObjectStore::get_state(self.0, id)
        }

        async fn get_blob(&self, hash: &ContentHash) -> objects::error::Result<Option<Blob>> {
            ObjectStore::get_blob(self.0, hash)
        }
    }

    #[cfg(feature = "async-source")]
    fn put_state(store: &InMemoryStore, parents: Vec<StateId>) -> State {
        let state = State::new(
            Tree::new().hash(),
            parents,
            Attribution::human(Principal::new("Test User", "test@example.com")),
        );
        ObjectStore::put_state(store, &state).unwrap();
        state
    }

    #[cfg(feature = "async-source")]
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, Waker};

        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = std::pin::pin!(future);

        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }
}
