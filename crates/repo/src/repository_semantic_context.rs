// SPDX-License-Identifier: Apache-2.0
//! Capture-time [`SemanticContext`] for the risk-signal registry.
//!
//! `changed_paths` come from the snapshot tree diff, then formatting-only
//! churn is pruned by the merkle index (`digest_at_path`, the same
//! comparison [`Repository::semantic_changed`] uses). Emit-scope functions
//! are parsed for that pruned set via the shared [`SemanticParseCache`].
//! The new-state comparison corpus is then filled from the semantic index
//! under fail-closed page/byte budgets.

#![cfg(feature = "tree-sitter-symbols")]

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
};

use objects::{
    error::HeddleError,
    object::{
        Blob, ContentHash, LeafPolicy, ObjectSource, SemanticEntryKind, SemanticIndexRoot,
        SemanticTreeNode, State, StateId, Tree, diff_trees, resolve_tree_path,
    },
    store::ObjectStore,
};
use semantic::{
    SemanticParseCache,
    parser::{FunctionDef, Language},
};
use state_review::SemanticContext;
use tracing::warn;

use crate::{Repository, Result};

/// Skip generated/vendored blobs the semantic index also treats as opaque.
pub(crate) const PARSE_BUDGET_BYTES: usize = 1 << 20;

/// In-memory objects from a packed worktree snapshot, then the durable store.
struct OverlaySource<'a, S: ObjectStore> {
    store: &'a S,
    blobs: Option<&'a HashMap<ContentHash, &'a [u8]>>,
    trees: Option<&'a HashMap<ContentHash, &'a Tree>>,
}

impl<S: ObjectStore> ObjectSource for OverlaySource<'_, S> {
    fn get_tree(&self, hash: &ContentHash) -> objects::error::Result<Option<Tree>> {
        if let Some(trees) = self.trees
            && let Some(tree) = trees.get(hash)
        {
            return Ok(Some((*tree).clone()));
        }
        if let Some(tree) = self.store.get_tree(hash)? {
            return Ok(Some(tree));
        }
        if *hash == Tree::new().hash() {
            return Ok(Some(Tree::new()));
        }
        Ok(None)
    }

    fn get_blob(&self, hash: &ContentHash) -> objects::error::Result<Option<Blob>> {
        if let Some(blobs) = self.blobs
            && let Some(bytes) = blobs.get(hash)
        {
            return Ok(Some(Blob::new(bytes.to_vec())));
        }
        self.store.get_blob(hash)
    }

    fn get_state(&self, id: &StateId) -> objects::error::Result<Option<State>> {
        self.store.get_state(id)
    }
}

/// Build the capture-time context for `run_all`.
///
/// `new_index` is the just-computed merkle root hash (not yet attached).
/// Packed snapshots pass in-memory blobs/trees so parse does not wait for
/// the commit pack. When both that root and the prior attached index are
/// readable, paths whose digests match are dropped so a fmt-sweep parses
/// nothing.
pub(crate) fn build_semantic_context(
    repo: &Repository,
    prior: Option<&State>,
    new: &State,
    new_index: Option<&ContentHash>,
    source_blobs: Option<&HashMap<ContentHash, &[u8]>>,
    source_trees: Option<&HashMap<ContentHash, &Tree>>,
) -> Result<SemanticContext> {
    let overlay = OverlaySource {
        store: repo.store(),
        blobs: source_blobs,
        trees: source_trees,
    };
    let from_tree = prior
        .map(|state| state.tree)
        .unwrap_or_else(|| Tree::new().hash());
    let changes = diff_trees(&overlay, &from_tree, &new.tree)
        .map_err(|err| HeddleError::InvalidObject(format!("tree diff failed: {err}")))?;
    let prior_root = prior
        .map(|state| repo.attached_semantic_index(&state.id()))
        .transpose()?
        .flatten();
    let new_root = new_index
        .map(|hash| load_new_index_root(repo, hash, source_blobs))
        .transpose()?
        .flatten();

    let mut changed_paths = BTreeSet::new();
    for change in changes.iter() {
        if !change.kind.is_change() {
            continue;
        }
        if !path_semantically_changed(
            &overlay,
            prior_root.as_ref(),
            new_root.as_ref(),
            &change.path,
        )? {
            continue;
        }
        changed_paths.insert(PathBuf::from(&change.path));
    }

    let cache = SemanticParseCache::shared();
    let prior_tree = prior.map(|state| state.tree);
    let mut prior_functions = BTreeMap::new();
    let mut new_functions = BTreeMap::new();
    for path in &changed_paths {
        let path_str = path.to_string_lossy();
        if let Some(fns) = parse_tree_functions(&overlay, prior_tree.as_ref(), &path_str, cache) {
            prior_functions.insert(path.clone(), fns);
        }
        if let Some(fns) = parse_tree_functions(&overlay, Some(&new.tree), &path_str, cache) {
            new_functions.insert(path.clone(), fns);
        }
    }

    let corpus_complete = crate::repository_semantic_corpus::populate_new_function_corpus(
        &overlay,
        new_root.as_ref(),
        &new.tree,
        cache,
        &mut new_functions,
    )?;
    let changed_symbols = crate::repository_semantic_corpus::collect_changed_symbols(
        &changed_paths,
        &prior_functions,
        &new_functions,
    );

    Ok(SemanticContext {
        prior_functions,
        new_functions,
        changed_paths,
        changed_symbols,
        corpus_complete,
    })
}

fn load_new_index_root(
    repo: &Repository,
    hash: &ContentHash,
    source_blobs: Option<&HashMap<ContentHash, &[u8]>>,
) -> Result<Option<SemanticIndexRoot>> {
    if let Some(blobs) = source_blobs
        && let Some(bytes) = blobs.get(hash)
    {
        return SemanticIndexRoot::decode(bytes)
            .map(Some)
            .map_err(|err| HeddleError::InvalidObject(err.to_string()));
    }
    match repo.load_index_root(hash) {
        Ok(root) => Ok(Some(root)),
        Err(err) => {
            warn!(
                error = %err,
                "semantic context: new index root unavailable; parse tree-diff paths"
            );
            Ok(None)
        }
    }
}

/// Same digest comparison as `semantic_changed`, but only prunes when both
/// indexes exist. Missing indexes keep the tree-diff path (fail toward parse).
fn path_semantically_changed(
    source: &impl ObjectSource,
    prior_root: Option<&SemanticIndexRoot>,
    new_root: Option<&SemanticIndexRoot>,
    path: &str,
) -> Result<bool> {
    let (Some(prior_root), Some(new_root)) = (prior_root, new_root) else {
        return Ok(true);
    };
    let prior_digest = digest_at_path(source, prior_root, path)?;
    let new_digest = digest_at_path(source, new_root, path)?;
    Ok(prior_digest != new_digest)
}

fn digest_at_path(
    source: &impl ObjectSource,
    root: &SemanticIndexRoot,
    path_prefix: &str,
) -> Result<Option<ContentHash>> {
    let components: Vec<&str> = path_prefix.split('/').filter(|c| !c.is_empty()).collect();
    if components.is_empty() {
        return Ok(Some(root.semantic_digest));
    }
    let mut node = load_semantic_tree(source, &root.tree)?;
    for (i, comp) in components.iter().enumerate() {
        let Some(entry) = node.get(comp) else {
            return Ok(None);
        };
        if i + 1 == components.len() {
            return Ok(Some(entry.semantic_digest));
        }
        if entry.kind != SemanticEntryKind::Dir {
            return Ok(None);
        }
        node = load_semantic_tree(source, &entry.node)?;
    }
    Ok(None)
}

pub(crate) fn load_semantic_tree(
    source: &impl ObjectSource,
    hash: &ContentHash,
) -> Result<SemanticTreeNode> {
    let blob = source
        .get_blob(hash)?
        .ok_or_else(|| HeddleError::NotFound(format!("semantic tree node {hash}")))?;
    SemanticTreeNode::decode(blob.content())
        .map_err(|err| HeddleError::InvalidObject(err.to_string()))
}

fn parse_tree_functions(
    source: &impl ObjectSource,
    tree: Option<&ContentHash>,
    path: &str,
    cache: &SemanticParseCache,
) -> Option<Vec<FunctionDef>> {
    parse_tree_functions_sized(source, tree, path, cache).map(|(fns, _)| fns)
}

pub(crate) fn parse_tree_functions_sized(
    source: &impl ObjectSource,
    tree: Option<&ContentHash>,
    path: &str,
    cache: &SemanticParseCache,
) -> Option<(Vec<FunctionDef>, usize)> {
    let tree = tree?;
    let language = Language::from_path(Path::new(path));
    if matches!(language, Language::Unknown) {
        return None;
    }
    let bytes = match blob_bytes_at_path(source, tree, path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return None,
        Err(err) => {
            warn!(error = %err, path, "semantic context: blob load failed; skip parse");
            return None;
        }
    };
    if bytes.len() > PARSE_BUDGET_BYTES {
        return None;
    }
    let byte_len = bytes.len();
    let source = std::str::from_utf8(&bytes).ok()?;
    let parsed = cache.parse(source, language)?;
    Some((parsed.extract_functions(), byte_len))
}

fn blob_bytes_at_path(
    source: &impl ObjectSource,
    tree: &ContentHash,
    path: &str,
) -> Result<Option<Vec<u8>>> {
    let resolved = resolve_tree_path(source, tree, Path::new(path), LeafPolicy::BlobOnly)?;
    let Some(hash) = resolved.and_then(|target| target.content_hash) else {
        return Ok(None);
    };
    Ok(source.get_blob(&hash)?.map(|blob| blob.content().to_vec()))
}

#[cfg(test)]
#[path = "repository_semantic_context_tests.rs"]
mod tests;
