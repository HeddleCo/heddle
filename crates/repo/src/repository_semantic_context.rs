// SPDX-License-Identifier: Apache-2.0
//! Capture-time [`SemanticContext`]: tree-diff paths, fmt-prune, bounded parse.

#![cfg(feature = "tree-sitter-symbols")]

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
};

use objects::{
    error::HeddleError,
    object::{
        AnnotationKind, AnnotationScope, AnnotationStatus, Blob, ContentHash, ContextTarget,
        DiffKind, LeafPolicy, ObjectSource, SemanticEntryKind, SemanticIndexRoot, SemanticTreeNode,
        SignalAnchor, State, StateId, Tree, diff_trees, resolve_tree_path,
    },
    store::ObjectStore,
};
use semantic::{
    SemanticParseCache,
    parser::{FunctionDef, Language},
};
use tracing::warn;

use crate::{Repository, Result};

pub(crate) const PARSE_BUDGET_BYTES: usize = 1 << 20;

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

/// Capture-side semantic context handed to a registered
/// [`SignalComputer`](crate::signals::SignalComputer). Field-for-field
/// the same shape `state-review` assembles into its registry input.
#[derive(Debug)]
pub struct CaptureSemanticContext {
    pub prior_functions: BTreeMap<PathBuf, Vec<FunctionDef>>,
    pub new_functions: BTreeMap<PathBuf, Vec<FunctionDef>>,
    pub changed_paths: BTreeSet<PathBuf>,
    pub changed_symbols: BTreeSet<(PathBuf, String)>,
    pub corpus_complete: bool,
    pub invariant_annotations: Vec<CaptureInvariantAnnotation>,
}

/// Storage-neutral invariant annotation carried across the `repo` signal
/// seam. `repo` owns extraction while `state-review` owns scoring, avoiding a
/// dependency cycle between the two crates.
#[derive(Debug, Clone)]
pub struct CaptureInvariantAnnotation {
    pub anchor: SignalAnchor,
    pub kind: AnnotationKind,
    pub content: String,
    pub tags: Vec<String>,
}

/// Build the capture-time context for signal computation.
pub fn build_semantic_context(
    repo: &Repository,
    prior: Option<&State>,
    new: &State,
    new_index: Option<&ContentHash>,
    source_blobs: Option<&HashMap<ContentHash, &[u8]>>,
    source_trees: Option<&HashMap<ContentHash, &Tree>>,
) -> Result<CaptureSemanticContext> {
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
    let mut added = Vec::new();
    let mut deleted = Vec::new();
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
        let path = PathBuf::from(&change.path);
        match change.kind {
            DiffKind::Added => added.push(path.clone()),
            DiffKind::Deleted => deleted.push(path.clone()),
            DiffKind::Modified | DiffKind::Unchanged => {}
        }
        changed_paths.insert(path);
    }

    let cache = SemanticParseCache::shared();
    let prior_tree = prior.map(|state| state.tree);
    let renames = crate::repository_semantic_corpus::pair_exact_blob_renames(
        &overlay,
        prior_tree.as_ref(),
        &new.tree,
        &added,
        &deleted,
    )?;
    let mut prior_functions = BTreeMap::new();
    let mut new_functions = BTreeMap::new();
    let mut budget = crate::repository_semantic_corpus::CorpusBudget::default();
    let mut corpus_complete = true;
    for path in &changed_paths {
        let path_str = path.to_string_lossy();
        let Some((fns, bytes)) =
            parse_tree_functions_sized(&overlay, Some(&new.tree), &path_str, cache)
        else {
            continue;
        };
        if !budget.try_add(bytes) {
            corpus_complete = false;
            break;
        }
        new_functions.insert(path.clone(), fns);
        let prior_path = renames.get(path).unwrap_or(path);
        if let Some(fns) = parse_tree_functions(
            &overlay,
            prior_tree.as_ref(),
            &prior_path.to_string_lossy(),
            cache,
        ) {
            prior_functions.insert(path.clone(), fns);
        }
    }

    if corpus_complete {
        corpus_complete = crate::repository_semantic_corpus::populate_new_function_corpus(
            &overlay,
            new_root.as_ref(),
            &new.tree,
            cache,
            &mut budget,
            &mut new_functions,
        )?;
    }
    let changed_symbols = crate::repository_semantic_corpus::collect_changed_symbols(
        &changed_paths,
        &prior_functions,
        &new_functions,
    );
    let invariant_annotations = prior
        .map(|state| {
            collect_invariant_annotations(repo, state).unwrap_or_else(|err| {
                warn!(
                    error = %err,
                    "semantic context: failed to load invariant annotations; invariant_adjacency module will stay quiet"
                );
                Vec::new()
            })
        })
        .unwrap_or_default();

    Ok(CaptureSemanticContext {
        prior_functions,
        new_functions,
        changed_paths,
        changed_symbols,
        corpus_complete,
        invariant_annotations,
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
            warn!(error = %err, "semantic context: new index root unavailable");
            Ok(None)
        }
    }
}

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

/// Collect active invariant annotations from the prior state's context.
///
/// File and line scopes use a file anchor; symbol scopes retain the symbol.
/// State-target annotations are advisory and do not participate in adjacency
/// scoring.
fn collect_invariant_annotations(
    repo: &Repository,
    prior: &State,
) -> Result<Vec<CaptureInvariantAnnotation>> {
    let Some(context_root) = repo.inherit_parent_context(prior)? else {
        return Ok(Vec::new());
    };
    let mut annotations = Vec::new();

    for entry in repo.list_context_entries(&context_root, None)? {
        let path = match &entry.target {
            ContextTarget::File { path } => path.clone(),
            ContextTarget::State { .. } => continue,
        };
        for annotation in &entry.blob.annotations {
            if annotation.status != AnnotationStatus::Active {
                continue;
            }
            let Some(revision) = annotation.current_revision() else {
                continue;
            };
            if !matches!(revision.kind, AnnotationKind::Invariant)
                && !revision.tags.iter().any(|tag| tag == "enforces")
            {
                continue;
            }
            let anchor = match &annotation.scope {
                AnnotationScope::Symbol { name, .. } => {
                    SignalAnchor::symbol(path.clone(), name.clone())
                }
                AnnotationScope::File | AnnotationScope::Lines(..) => {
                    SignalAnchor::file(path.clone())
                }
            };
            annotations.push(CaptureInvariantAnnotation {
                anchor,
                kind: revision.kind,
                content: revision.content.clone(),
                tags: revision.tags.clone(),
            });
        }
    }

    Ok(annotations)
}
