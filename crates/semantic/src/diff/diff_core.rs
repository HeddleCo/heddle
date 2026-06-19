// SPDX-License-Identifier: Apache-2.0
//! Public semantic diff APIs backed by the shared semantic engine.

use std::path::Path;

use objects::{
    object::{ContentHash, FileChangeSet, diff_trees},
    store::LocalObjectStore,
    worktree::WorktreeStatus,
};

use super::{diff_helpers::TreeBlobContentLoader, diff_options::SemanticDiffOptions};
use crate::{
    cache::SemanticParseCache,
    diff::{
        diff_engine::SemanticEngine,
        diff_types::{SemanticCheckOnlyResult, SemanticDiffResult, SemanticSummaryResult},
    },
};

/// Perform a cheap semantic no-op check between two trees.
pub fn semantic_check_only<S: LocalObjectStore + ?Sized>(
    store: &S,
    from_tree_hash: &ContentHash,
    to_tree_hash: &ContentHash,
    options: &SemanticDiffOptions,
) -> Result<SemanticCheckOnlyResult, anyhow::Error> {
    semantic_check_only_with_cache(
        store,
        from_tree_hash,
        to_tree_hash,
        options,
        SemanticParseCache::shared(),
    )
}

/// Perform a cheap semantic no-op check between two trees using an injected cache.
pub fn semantic_check_only_with_cache<S: LocalObjectStore + ?Sized>(
    store: &S,
    from_tree_hash: &ContentHash,
    to_tree_hash: &ContentHash,
    options: &SemanticDiffOptions,
    cache: &SemanticParseCache,
) -> Result<SemanticCheckOnlyResult, anyhow::Error> {
    let file_changes = diff_trees(store, from_tree_hash, to_tree_hash)?;
    let old_loader = TreeBlobContentLoader::new(store, *from_tree_hash);
    let new_loader = TreeBlobContentLoader::new(store, *to_tree_hash);
    SemanticEngine::new(
        file_changes,
        |path| old_loader.load_content(path),
        |path| new_loader.load_content(path),
        options,
        cache,
    )
    .check_only()
}

/// Perform a cheap semantic no-op check between a tree and worktree content.
pub fn semantic_check_only_worktree<S: LocalObjectStore + ?Sized>(
    store: &S,
    from_tree_hash: &ContentHash,
    worktree_root: &Path,
    status: &WorktreeStatus,
    options: &SemanticDiffOptions,
) -> Result<SemanticCheckOnlyResult, anyhow::Error> {
    semantic_check_only_worktree_with_cache(
        store,
        from_tree_hash,
        worktree_root,
        status,
        options,
        SemanticParseCache::shared(),
    )
}

/// Perform a cheap semantic no-op check between a tree and worktree content using an injected cache.
pub fn semantic_check_only_worktree_with_cache<S: LocalObjectStore + ?Sized>(
    store: &S,
    from_tree_hash: &ContentHash,
    worktree_root: &Path,
    status: &WorktreeStatus,
    options: &SemanticDiffOptions,
    cache: &SemanticParseCache,
) -> Result<SemanticCheckOnlyResult, anyhow::Error> {
    let old_loader = TreeBlobContentLoader::new(store, *from_tree_hash);
    SemanticEngine::new(
        file_changes_from_status(status),
        |path| old_loader.load_content(path),
        |path| load_worktree_blob_content(worktree_root, path),
        options,
        cache,
    )
    .check_only()
}

/// Perform semantic summary analysis between two trees.
pub fn semantic_diff_summary<S: LocalObjectStore + ?Sized>(
    store: &S,
    from_tree_hash: &ContentHash,
    to_tree_hash: &ContentHash,
    options: &SemanticDiffOptions,
) -> Result<SemanticSummaryResult, anyhow::Error> {
    semantic_diff_summary_with_cache(
        store,
        from_tree_hash,
        to_tree_hash,
        options,
        SemanticParseCache::shared(),
    )
}

/// Perform semantic summary analysis between two trees using an injected cache.
pub fn semantic_diff_summary_with_cache<S: LocalObjectStore + ?Sized>(
    store: &S,
    from_tree_hash: &ContentHash,
    to_tree_hash: &ContentHash,
    options: &SemanticDiffOptions,
    cache: &SemanticParseCache,
) -> Result<SemanticSummaryResult, anyhow::Error> {
    let file_changes = diff_trees(store, from_tree_hash, to_tree_hash)?;
    let old_loader = TreeBlobContentLoader::new(store, *from_tree_hash);
    let new_loader = TreeBlobContentLoader::new(store, *to_tree_hash);
    SemanticEngine::new(
        file_changes,
        |path| old_loader.load_content(path),
        |path| new_loader.load_content(path),
        options,
        cache,
    )
    .summary()
}

/// Perform semantic summary between a tree and worktree content.
pub fn semantic_diff_summary_worktree<S: LocalObjectStore + ?Sized>(
    store: &S,
    from_tree_hash: &ContentHash,
    worktree_root: &Path,
    status: &WorktreeStatus,
    options: &SemanticDiffOptions,
) -> Result<SemanticSummaryResult, anyhow::Error> {
    semantic_diff_summary_worktree_with_cache(
        store,
        from_tree_hash,
        worktree_root,
        status,
        options,
        SemanticParseCache::shared(),
    )
}

/// Perform semantic summary between a tree and worktree content using an injected cache.
pub fn semantic_diff_summary_worktree_with_cache<S: LocalObjectStore + ?Sized>(
    store: &S,
    from_tree_hash: &ContentHash,
    worktree_root: &Path,
    status: &WorktreeStatus,
    options: &SemanticDiffOptions,
    cache: &SemanticParseCache,
) -> Result<SemanticSummaryResult, anyhow::Error> {
    let old_loader = TreeBlobContentLoader::new(store, *from_tree_hash);
    SemanticEngine::new(
        file_changes_from_status(status),
        |path| old_loader.load_content(path),
        |path| load_worktree_blob_content(worktree_root, path),
        options,
        cache,
    )
    .summary()
}

/// Perform semantic diff analysis between two trees.
pub fn semantic_diff<S: LocalObjectStore + ?Sized>(
    store: &S,
    from_tree_hash: &ContentHash,
    to_tree_hash: &ContentHash,
    options: &SemanticDiffOptions,
) -> Result<SemanticDiffResult, anyhow::Error> {
    semantic_diff_with_cache(
        store,
        from_tree_hash,
        to_tree_hash,
        options,
        SemanticParseCache::shared(),
    )
}

/// Perform semantic diff analysis between two trees using an injected cache.
pub fn semantic_diff_with_cache<S: LocalObjectStore + ?Sized>(
    store: &S,
    from_tree_hash: &ContentHash,
    to_tree_hash: &ContentHash,
    options: &SemanticDiffOptions,
    cache: &SemanticParseCache,
) -> Result<SemanticDiffResult, anyhow::Error> {
    let file_changes = diff_trees(store, from_tree_hash, to_tree_hash)?;
    let old_loader = TreeBlobContentLoader::new(store, *from_tree_hash);
    let new_loader = TreeBlobContentLoader::new(store, *to_tree_hash);
    SemanticEngine::new(
        file_changes,
        |path| old_loader.load_content(path),
        |path| new_loader.load_content(path),
        options,
        cache,
    )
    .full()
}

/// Perform semantic diff between a tree and worktree content.
pub fn semantic_diff_worktree<S: LocalObjectStore + ?Sized>(
    store: &S,
    from_tree_hash: &ContentHash,
    worktree_root: &Path,
    status: &WorktreeStatus,
    options: &SemanticDiffOptions,
) -> Result<SemanticDiffResult, anyhow::Error> {
    semantic_diff_worktree_with_cache(
        store,
        from_tree_hash,
        worktree_root,
        status,
        options,
        SemanticParseCache::shared(),
    )
}

/// Perform semantic diff between a tree and worktree content using an injected cache.
pub fn semantic_diff_worktree_with_cache<S: LocalObjectStore + ?Sized>(
    store: &S,
    from_tree_hash: &ContentHash,
    worktree_root: &Path,
    status: &WorktreeStatus,
    options: &SemanticDiffOptions,
    cache: &SemanticParseCache,
) -> Result<SemanticDiffResult, anyhow::Error> {
    let old_loader = TreeBlobContentLoader::new(store, *from_tree_hash);
    SemanticEngine::new(
        file_changes_from_status(status),
        |path| old_loader.load_content(path),
        |path| load_worktree_blob_content(worktree_root, path),
        options,
        cache,
    )
    .full()
}

fn file_changes_from_status(status: &WorktreeStatus) -> FileChangeSet {
    let mut file_changes = FileChangeSet::with_capacity(status.change_count());
    for path in &status.deleted {
        file_changes.push_deleted(path.display().to_string());
    }
    for path in &status.added {
        file_changes.push_added(path.display().to_string());
    }
    for path in &status.modified {
        file_changes.push_modified(path.display().to_string());
    }
    file_changes
}

fn load_worktree_blob_content(
    worktree_root: &Path,
    path: &Path,
) -> Result<Option<String>, anyhow::Error> {
    let worktree_path = worktree_root.join(path);
    match std::fs::read_to_string(&worktree_path) {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}
