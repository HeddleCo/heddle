// SPDX-License-Identifier: Apache-2.0
//! Shared history traversal and filtering primitives.

use std::{
    ops::ControlFlow,
    path::{Component, Path},
};

#[cfg(feature = "async-source")]
use objects::{object::diff_trees_visit_async, store::AsyncObjectSource};
use objects::{
    object::{ContentHash, State, StateId, Tree, diff_trees_visit},
    store::ObjectSource,
};
use tracing::{instrument, trace};

#[cfg(feature = "async-source")]
use crate::repository::commit_graph_persistence::NullCommitGraphCache;
use crate::{
    HeddleError, Repository, Result,
    repository::commit_graph_persistence::{CommitGraphCache, FsCommitGraphCache},
};

/// A normalized changed-path filter for history traversal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedPathFilter {
    path: String,
}

impl ChangedPathFilter {
    /// Parse and normalize a repository-relative path filter.
    pub fn new(path: impl AsRef<str>) -> Result<Self> {
        Ok(Self {
            path: normalize_repo_relative_path(path.as_ref())?,
        })
    }

    fn matches(&self, candidate: &str) -> bool {
        candidate == self.path
            || candidate
                .strip_prefix(&self.path)
                .is_some_and(|rest| rest.starts_with('/'))
    }
}

/// A set of changed-path filters applied with OR semantics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangedPathFilters {
    filters: Vec<ChangedPathFilter>,
}

impl ChangedPathFilters {
    /// Build changed-path filters from raw path strings.
    pub fn try_from_paths<I, S>(paths: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let filters = paths
            .into_iter()
            .map(ChangedPathFilter::new)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { filters })
    }

    /// Returns true when no changed-path filtering is active.
    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }

    fn matches(&self, candidate: &str) -> bool {
        self.filters.iter().any(|filter| filter.matches(candidate))
    }

    fn len(&self) -> usize {
        self.filters.len()
    }

    pub(crate) fn bloom_maybe_matches(&self, bloom: &[u8; 256]) -> bool {
        use super::bloom_filter::bloom_maybe_contains;
        self.filters
            .iter()
            .any(|f| bloom_maybe_contains(bloom, &f.path))
    }
}

/// A reusable first-parent history query.
#[derive(Clone, Debug, Default)]
pub struct HistoryQuery {
    start: Option<StateId>,
    limit: usize,
    agent_model_substring: Option<String>,
    changed_paths: ChangedPathFilters,
    /// Exclusive lower bound: walk terminates BEFORE visiting this
    /// state. Applied during traversal — *before* `agent` / `paths`
    /// filters — so a `--since` bound that itself doesn't match the
    /// active filter still bounds the walk correctly. (Without this, a
    /// `--since` whose state is filtered out by `--path` would silently
    /// degrade to "no bound", and matches older than the bound would
    /// leak into the result.)
    stop_at: Option<StateId>,
}

impl HistoryQuery {
    /// Create a new history query.
    pub fn new(start: Option<StateId>) -> Self {
        Self {
            start,
            limit: 20,
            agent_model_substring: None,
            changed_paths: ChangedPathFilters::default(),
            stop_at: None,
        }
    }

    /// Override the maximum number of matching states returned.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Filter states by agent model substring.
    pub fn with_agent_filter(mut self, agent_model_substring: Option<String>) -> Self {
        self.agent_model_substring = agent_model_substring;
        self
    }

    /// Filter states by the paths they changed relative to their first parent.
    pub fn with_changed_paths(mut self, changed_paths: ChangedPathFilters) -> Self {
        self.changed_paths = changed_paths;
        self
    }

    /// Stop walking when we reach this state (exclusive). Applied
    /// during the first-parent walk *before* filters, so the bound is
    /// honored even when `--since` resolves to a state that wouldn't
    /// otherwise survive the active filter set.
    pub fn with_stop_at(mut self, stop_at: Option<StateId>) -> Self {
        self.stop_at = stop_at;
        self
    }
}

impl Repository {
    /// Walk first-parent history and return states matching the query.
    #[instrument(skip(self, query), fields(limit = query.limit, changed_path_filters = query.changed_paths.len()))]
    pub fn query_history(&self, query: &HistoryQuery) -> Result<Vec<State>> {
        query_history_with_cache(self.store(), query, FsCommitGraphCache::new(self.root()))
    }
}

/// Walk first-parent history against any read-only object source.
pub(crate) fn query_history_with_cache<S, C>(
    source: &S,
    query: &HistoryQuery,
    cache: C,
) -> Result<Vec<State>>
where
    S: ObjectSource + ?Sized,
    C: CommitGraphCache,
{
    use super::commit_graph::CommitGraphIndex;

    let mut graph = CommitGraphIndex::with_cache(source, cache);
    let mut candidate_ids = Vec::new();
    let mut current = query.start;

    while let Some(state_id) = current {
        if candidate_ids.len() >= query.limit {
            break;
        }

        // `stop_at` (the exclusive `--since` bound) is checked
        // BEFORE filters so the walk terminates at the bound even
        // when the bound state itself is filtered out by `--agent`
        // or `--path`. Without this, a `--since` whose resolved
        // state doesn't match the active filter would silently
        // degrade and matches older than the bound would leak.
        if let Some(stop) = query.stop_at
            && state_id == stop
        {
            break;
        }

        graph
            .ensure_loaded(state_id)
            .map_err(|e| HeddleError::InvalidObject(e.to_string()))?;
        let Some(meta) = graph.node_metadata(&state_id) else {
            break;
        };
        current = meta.first_parent;

        // Fast agent filter from cached metadata — no state load needed
        if let Some(ref filter) = query.agent_model_substring {
            match &meta.agent_model {
                Some(model) if model.contains(filter.as_str()) => {}
                _ => continue,
            }
        }

        if query.changed_paths.is_empty() {
            // No path filter — this is a match
            candidate_ids.push(state_id);
            trace!(state = %state_id, "history query matched state (no path filter)");
            continue;
        }

        // Use bloom filter to skip expensive tree diffs
        graph
            .ensure_bloom_populated(state_id)
            .map_err(|e| HeddleError::InvalidObject(e.to_string()))?;
        if graph
            .node_bloom(&state_id)
            .is_some_and(|bloom| !query.changed_paths.bloom_maybe_matches(bloom))
        {
            // Bloom says "definitely not changed" — skip
            continue;
        }

        // Bloom says "maybe" (or no bloom) — confirm with full tree diff
        let Some(state) = source.get_state(&state_id)? else {
            break;
        };
        if !state_matches_changed_paths(source, &state, &query.changed_paths)? {
            continue;
        }
        trace!(state = %state_id, "history query matched state");
        candidate_ids.push(state_id);
    }

    // Load full State objects only for final matches
    let mut result = Vec::with_capacity(candidate_ids.len());
    for id in candidate_ids {
        if let Some(state) = source.get_state(&id)? {
            result.push(state);
        }
    }
    Ok(result)
}

pub(crate) fn state_matches_changed_paths<S>(
    source: &S,
    state: &State,
    changed_paths: &ChangedPathFilters,
) -> Result<bool>
where
    S: ObjectSource + ?Sized,
{
    let base_tree = parent_tree_hash(source, state)?;
    // Early-exit: stop diffing the moment the first change matches the
    // filter, rather than materializing the whole change list to scan it.
    let flow = diff_trees_visit(source, &base_tree, &state.tree, |change| {
        if changed_paths.matches(&change.path) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .map_err(|error| HeddleError::InvalidObject(format!("tree diff failed: {error}")))?;
    let matched = flow.is_break();
    trace!(
        state = %state.id(),
        matched,
        "evaluated changed-path filters"
    );
    Ok(matched)
}

fn parent_tree_hash<S>(source: &S, state: &State) -> Result<ContentHash>
where
    S: ObjectSource + ?Sized,
{
    match state.first_parent() {
        Some(parent_id) => {
            let parent = source
                .get_state(parent_id)?
                .ok_or(HeddleError::StateNotFound(*parent_id))?;
            Ok(parent.tree)
        }
        None => Ok(Tree::new().hash()),
    }
}

/// Walk first-parent history against an async read-only object source.
#[cfg(feature = "async-source")]
pub async fn query_history_async<S>(source: &S, query: &HistoryQuery) -> Result<Vec<State>>
where
    S: AsyncObjectSource + Sync + ?Sized,
{
    query_history_async_with_cache(source, query, NullCommitGraphCache).await
}

#[cfg(feature = "async-source")]
async fn query_history_async_with_cache<S>(
    source: &S,
    query: &HistoryQuery,
    _cache: NullCommitGraphCache,
) -> Result<Vec<State>>
where
    S: AsyncObjectSource + Sync + ?Sized,
{
    let mut candidate_ids = Vec::new();
    let mut current = query.start;

    while let Some(state_id) = current {
        if candidate_ids.len() >= query.limit {
            break;
        }

        if let Some(stop) = query.stop_at
            && state_id == stop
        {
            break;
        }

        let Some(state) = source.get_state(&state_id).await? else {
            break;
        };
        current = state.parents.first().copied();

        if let Some(ref filter) = query.agent_model_substring {
            match state.attribution.agent.as_ref().map(|agent| &agent.model) {
                Some(model) if model.contains(filter.as_str()) => {}
                _ => continue,
            }
        }

        if query.changed_paths.is_empty()
            || state_matches_changed_paths_async(source, &state, &query.changed_paths).await?
        {
            trace!(state = %state_id, "async history query matched state");
            candidate_ids.push(state_id);
        }
    }

    let mut result = Vec::with_capacity(candidate_ids.len());
    for id in candidate_ids {
        if let Some(state) = source.get_state(&id).await? {
            result.push(state);
        }
    }
    Ok(result)
}

#[cfg(feature = "async-source")]
async fn state_matches_changed_paths_async<S>(
    source: &S,
    state: &State,
    changed_paths: &ChangedPathFilters,
) -> Result<bool>
where
    S: AsyncObjectSource + Sync + ?Sized,
{
    let base_tree = parent_tree_hash_async(source, state).await?;
    let flow = diff_trees_visit_async(source, &base_tree, &state.tree, |change| {
        if changed_paths.matches(&change.path) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .await
    .map_err(|error| HeddleError::InvalidObject(format!("tree diff failed: {error}")))?;
    Ok(flow.is_break())
}

#[cfg(feature = "async-source")]
async fn parent_tree_hash_async<S>(source: &S, state: &State) -> Result<ContentHash>
where
    S: AsyncObjectSource + Sync + ?Sized,
{
    match state.first_parent() {
        Some(parent_id) => {
            let parent = source
                .get_state(parent_id)
                .await?
                .ok_or(HeddleError::StateNotFound(*parent_id))?;
            Ok(parent.tree)
        }
        None => Ok(Tree::new().hash()),
    }
}

fn normalize_repo_relative_path(path: &str) -> Result<String> {
    let input = Path::new(path);
    if input.is_absolute() {
        return Err(HeddleError::Config(format!(
            "changed-path filter must be repository-relative: '{path}'"
        )));
    }

    let mut segments = Vec::new();
    for component in input.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(segment) => {
                let segment = segment.to_str().ok_or_else(|| {
                    HeddleError::Config(format!(
                        "changed-path filter must be valid UTF-8: '{path}'"
                    ))
                })?;
                segments.push(segment);
            }
            Component::ParentDir => {
                return Err(HeddleError::Config(format!(
                    "changed-path filter cannot escape repository root: '{path}'"
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(HeddleError::Config(format!(
                    "changed-path filter must be repository-relative: '{path}'"
                )));
            }
        }
    }

    if segments.is_empty() {
        return Err(HeddleError::Config(
            "changed-path filter cannot be empty".to_string(),
        ));
    }

    Ok(segments.join("/"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    #[cfg(feature = "async-source")]
    use objects::{
        object::{
            Attribution, Blob, ContentHash, EntryType, Principal, State, StateId, Tree, TreeEntry,
        },
        store::{AsyncObjectSource, InMemoryStore, ObjectStore},
    };
    use tempfile::TempDir;

    use super::{
        ChangedPathFilter, ChangedPathFilters, HistoryQuery, normalize_repo_relative_path,
        query_history_with_cache,
    };
    use crate::{Repository, repository::commit_graph_persistence::NullCommitGraphCache};

    #[test]
    fn changed_path_filter_matches_exact_paths_and_children() {
        let filter = ChangedPathFilter::new("src").unwrap();

        assert!(filter.matches("src"));
        assert!(filter.matches("src/lib.rs"));
        assert!(!filter.matches("src-lib.rs"));
    }

    #[test]
    fn changed_path_filters_normalize_curdir_prefixes() {
        let filters = ChangedPathFilters::try_from_paths(["./src/lib.rs"]).unwrap();

        assert!(filters.matches("src/lib.rs"));
    }

    #[test]
    fn query_history_path_filter_matches_with_and_without_fs_commit_graph_cache() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init_default(temp_dir.path()).unwrap();

        let src_dir = temp_dir.path().join("src");
        fs::create_dir(&src_dir).unwrap();
        fs::write(src_dir.join("lib.rs"), "one\n").unwrap();
        let base = repo.snapshot(Some("base".to_string()), None).unwrap();

        fs::write(temp_dir.path().join("README.md"), "docs\n").unwrap();
        let _docs = repo.snapshot(Some("docs".to_string()), None).unwrap();

        fs::write(src_dir.join("lib.rs"), "two\n").unwrap();
        let src = repo.snapshot(Some("src".to_string()), None).unwrap();

        fs::write(temp_dir.path().join("README.md"), "more docs\n").unwrap();
        let head = repo.snapshot(Some("head".to_string()), None).unwrap();

        let query = HistoryQuery::new(Some(head.id()))
            .with_limit(10)
            .with_changed_paths(ChangedPathFilters::try_from_paths(["src"]).unwrap());
        let expected = vec![src.id(), base.id()];

        let warmed = repo.query_history(&query).unwrap();
        assert_eq!(
            warmed.iter().map(|state| state.id()).collect::<Vec<_>>(),
            expected
        );

        let graph_path = super::super::commit_graph_persistence::commit_graph_path(repo.root());
        assert!(graph_path.exists());

        let with_cache = repo.query_history(&query).unwrap();
        let null_cache =
            query_history_with_cache(repo.store(), &query, NullCommitGraphCache).unwrap();
        fs::remove_file(&graph_path).unwrap();
        let without_cache = repo.query_history(&query).unwrap();

        assert_eq!(with_cache, without_cache);
        assert_eq!(with_cache, null_cache);
        assert_eq!(
            without_cache
                .iter()
                .map(|state| state.id())
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[cfg(feature = "async-source")]
    #[test]
    fn query_history_async_matches_sync_null_cache_for_path_filtered_first_parent_walk() {
        let store = InMemoryStore::new();

        let base_tree = root_tree(&store, "one\n", None);
        let base = put_state(&store, base_tree, vec![]);

        let docs_tree = root_tree(&store, "one\n", Some("docs\n"));
        let docs = put_state(&store, docs_tree, vec![base.id()]);

        let src_tree = root_tree(&store, "two\n", Some("docs\n"));
        let src = put_state(&store, src_tree, vec![docs.id()]);

        let side_tree = root_tree(&store, "side\n", None);
        let side = put_state(&store, side_tree, vec![base.id()]);

        let head_tree = root_tree(&store, "two\n", Some("more docs\n"));
        let head = put_state(&store, head_tree, vec![src.id(), side.id()]);

        let query = HistoryQuery::new(Some(head.id()))
            .with_limit(10)
            .with_changed_paths(ChangedPathFilters::try_from_paths(["src"]).unwrap());

        let sync = query_history_with_cache(&store, &query, NullCommitGraphCache).unwrap();
        let async_source = AsyncInMemorySource(&store);
        let async_result = block_on(super::query_history_async(&async_source, &query)).unwrap();

        assert_eq!(sync, async_result);
        assert_eq!(
            async_result
                .iter()
                .map(|state| state.id())
                .collect::<Vec<_>>(),
            vec![src.id(), base.id()]
        );
    }

    #[cfg(feature = "async-source")]
    struct AsyncInMemorySource<'a>(&'a InMemoryStore);

    #[cfg(feature = "async-source")]
    impl AsyncObjectSource for AsyncInMemorySource<'_> {
        async fn get_tree(&self, hash: &ContentHash) -> objects::error::Result<Option<Tree>> {
            ObjectStore::get_tree(self.0, hash)
        }

        async fn get_state(
            &self,
            id: &objects::object::StateId,
        ) -> objects::error::Result<Option<State>> {
            ObjectStore::get_state(self.0, id)
        }

        async fn get_blob(&self, hash: &ContentHash) -> objects::error::Result<Option<Blob>> {
            ObjectStore::get_blob(self.0, hash)
        }
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

    #[cfg(feature = "async-source")]
    fn root_tree(store: &InMemoryStore, src_content: &str, readme: Option<&str>) -> ContentHash {
        let lib = ObjectStore::put_blob(store, &Blob::from_slice(src_content.as_bytes())).unwrap();
        let src = tree(store, vec![("lib.rs", lib, EntryType::Blob)]);
        let mut entries = vec![("src", src, EntryType::Tree)];
        if let Some(readme) = readme {
            let readme =
                ObjectStore::put_blob(store, &Blob::from_slice(readme.as_bytes())).unwrap();
            entries.push(("README.md", readme, EntryType::Blob));
        }
        tree(store, entries)
    }

    #[cfg(feature = "async-source")]
    fn tree(store: &InMemoryStore, entries: Vec<(&str, ContentHash, EntryType)>) -> ContentHash {
        let entries = entries
            .into_iter()
            .map(|(name, hash, entry_type)| match entry_type {
                EntryType::Blob => TreeEntry::file(name.to_string(), hash, false),
                EntryType::Tree => TreeEntry::directory(name.to_string(), hash),
                EntryType::Symlink => TreeEntry::symlink(name.to_string(), hash),
                EntryType::Gitlink => {
                    unreachable!("repository history tests do not build gitlinks")
                }
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        ObjectStore::put_tree(store, &Tree::from_entries(entries)).unwrap()
    }

    #[cfg(feature = "async-source")]
    fn put_state(store: &InMemoryStore, tree: ContentHash, parents: Vec<StateId>) -> State {
        let state = State::new(
            tree,
            parents,
            Attribution::human(Principal::new("Test User", "test@example.com")),
        );
        ObjectStore::put_state(store, &state).unwrap();
        state
    }

    #[test]
    fn normalize_repo_relative_path_rejects_parent_segments() {
        let error = normalize_repo_relative_path("../secret").unwrap_err();
        assert!(error.to_string().contains("cannot escape repository root"));
    }
}
