// SPDX-License-Identifier: Apache-2.0
//! Bounded new-state function corpus for capture-time risk signals.

#![cfg(feature = "tree-sitter-symbols")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use objects::object::{
    ContentHash, LeafPolicy, ObjectSource, SemanticEntryKind, SemanticFileNode, SemanticIndexRoot,
    SymbolKindTag, resolve_tree_path,
};
use semantic::{SemanticParseCache, parser::FunctionDef};
use tracing::warn;

use crate::{Result, repository_semantic_query::MAX_SEMANTIC_TREE_DEPTH};

use super::repository_semantic_context::{
    PARSE_BUDGET_BYTES, load_semantic_tree, parse_tree_functions_sized,
};

/// File page for the new-state corpus. Alias of the shared parse-cache
/// page so a filled page is a bound, not a smaller product universe.
pub(crate) const CORPUS_FILE_BUDGET: usize = SemanticParseCache::DEFAULT_MAX_ENTRIES;
/// Aggregate bytes parsed for the new-state function corpus.
/// Two times the per-file semantic-index opaque threshold.
pub(crate) const CORPUS_BYTE_BUDGET: usize = PARSE_BUDGET_BYTES.saturating_mul(2);

/// Running file/byte totals for the new-state corpus. Shared by the
/// changed-path parse and [`populate_new_function_corpus`].
#[derive(Clone, Debug, Default)]
pub(crate) struct CorpusBudget {
    files: usize,
    bytes: usize,
}

impl CorpusBudget {
    pub(crate) fn try_add(&mut self, bytes: usize) -> bool {
        if self.files >= CORPUS_FILE_BUDGET {
            return false;
        }
        let next = self.bytes.saturating_add(bytes);
        if next > CORPUS_BYTE_BUDGET {
            return false;
        }
        self.files += 1;
        self.bytes = next;
        true
    }

    pub(crate) fn has_room(&self) -> bool {
        self.files < CORPUS_FILE_BUDGET && self.bytes <= CORPUS_BYTE_BUDGET
    }

    pub(crate) fn remaining_files(&self) -> usize {
        CORPUS_FILE_BUDGET.saturating_sub(self.files)
    }
}

pub(crate) fn collect_changed_symbols(
    changed_paths: &BTreeSet<PathBuf>,
    prior_functions: &BTreeMap<PathBuf, Vec<FunctionDef>>,
    new_functions: &BTreeMap<PathBuf, Vec<FunctionDef>>,
) -> BTreeSet<(PathBuf, String)> {
    let mut changed = BTreeSet::new();
    for path in changed_paths {
        let Some(new_fns) = new_functions.get(path) else {
            continue;
        };
        let prior_fns = prior_functions.get(path);
        for fn_def in new_fns {
            if function_changed(prior_fns.map(Vec::as_slice), fn_def) {
                changed.insert((path.clone(), fn_def.symbol_identity()));
            }
        }
    }
    changed
}

fn function_changed(prior_fns: Option<&[FunctionDef]>, new_fn: &FunctionDef) -> bool {
    let Some(prior_fns) = prior_fns else {
        return true;
    };
    match prior_fns
        .iter()
        .find(|prior| prior.symbol_identity() == new_fn.symbol_identity())
    {
        None => true,
        Some(prior) => prior.content != new_fn.content,
    }
}

/// Walk the new semantic index and parse remaining function-bearing files
/// into `new_functions`. Returns `true` only when the walk finished inside
/// the page/byte budgets. Missing or unreadable indexes fail closed.
pub(crate) fn populate_new_function_corpus(
    source: &impl ObjectSource,
    new_root: Option<&SemanticIndexRoot>,
    new_tree: &ContentHash,
    cache: &SemanticParseCache,
    budget: &mut CorpusBudget,
    new_functions: &mut BTreeMap<PathBuf, Vec<FunctionDef>>,
) -> Result<bool> {
    let Some(root) = new_root else {
        return Ok(false);
    };
    let skip: BTreeSet<PathBuf> = new_functions.keys().cloned().collect();
    let mut files = Vec::new();
    if !collect_function_file_paths(source, root, budget.remaining_files(), &skip, &mut files)? {
        return Ok(false);
    }
    for path in files {
        let rel = PathBuf::from(&path);
        if new_functions.contains_key(&rel) {
            continue;
        }
        let Some((functions, bytes)) =
            parse_tree_functions_sized(source, Some(new_tree), &path, cache)
        else {
            warn!(
                path,
                "semantic corpus: index-listed function file failed to parse; fail-closed"
            );
            return Ok(false);
        };
        if functions.is_empty() {
            warn!(
                path,
                "semantic corpus: index-listed function file yielded no functions; fail-closed"
            );
            return Ok(false);
        }
        if !budget.try_add(bytes) {
            return Ok(false);
        }
        new_functions.insert(rel, functions);
    }
    Ok(true)
}

pub(crate) fn collect_function_file_paths(
    source: &impl ObjectSource,
    root: &SemanticIndexRoot,
    remaining: usize,
    skip: &BTreeSet<PathBuf>,
    out: &mut Vec<String>,
) -> Result<bool> {
    let mut stack = vec![(String::new(), root.tree, 0usize)];
    let mut inspected = 0usize;
    while let Some((prefix, hash, depth)) = stack.pop() {
        if depth > MAX_SEMANTIC_TREE_DEPTH {
            return Ok(false);
        }
        let node = match load_semantic_tree(source, &hash) {
            Ok(node) => node,
            Err(err) => {
                warn!(
                    error = %err,
                    "semantic corpus: index node unreadable; fail-closed"
                );
                return Ok(false);
            }
        };
        for entry in node.entries.iter().rev() {
            let path = join_semantic_path(&prefix, &entry.name);
            match entry.kind {
                SemanticEntryKind::Dir => stack.push((path, entry.node, depth + 1)),
                SemanticEntryKind::Opaque => {}
                SemanticEntryKind::File => {
                    if skip.contains(&PathBuf::from(&path)) {
                        continue;
                    }
                    if remaining > 0 && inspected >= remaining {
                        return Ok(false);
                    }
                    inspected += 1;
                    match load_semantic_file(source, &entry.node) {
                        Ok(file) if file_has_function(&file) => {
                            if remaining == 0 || out.len() >= remaining {
                                return Ok(false);
                            }
                            out.push(path);
                        }
                        Ok(_) => {}
                        Err(err) => {
                            warn!(
                                error = %err,
                                path,
                                "semantic corpus: file node unreadable; fail-closed"
                            );
                            return Ok(false);
                        }
                    }
                }
            }
        }
    }
    Ok(true)
}

fn load_semantic_file(source: &impl ObjectSource, hash: &ContentHash) -> Result<SemanticFileNode> {
    let blob = source.get_blob(hash)?.ok_or_else(|| {
        objects::error::HeddleError::NotFound(format!("semantic file node {hash}"))
    })?;
    SemanticFileNode::decode(blob.content())
        .map_err(|err| objects::error::HeddleError::InvalidObject(err.to_string()))
}

fn file_has_function(file: &SemanticFileNode) -> bool {
    file.symbols
        .iter()
        .any(|symbol| symbol.kind == SymbolKindTag::Function)
}

/// Pair added paths with deleted paths that share an exact blob hash.
/// Destination → source. Each hash is used at most once.
pub(crate) fn pair_exact_blob_renames(
    source: &impl ObjectSource,
    prior_tree: Option<&ContentHash>,
    new_tree: &ContentHash,
    added: &[PathBuf],
    deleted: &[PathBuf],
) -> Result<BTreeMap<PathBuf, PathBuf>> {
    let Some(prior_tree) = prior_tree else {
        return Ok(BTreeMap::new());
    };
    let mut unused_deleted: BTreeMap<ContentHash, PathBuf> = BTreeMap::new();
    for path in deleted {
        if let Some(hash) = blob_hash_at_path(source, prior_tree, &path.to_string_lossy())? {
            unused_deleted.entry(hash).or_insert_with(|| path.clone());
        }
    }
    let mut renames = BTreeMap::new();
    for path in added {
        let Some(hash) = blob_hash_at_path(source, new_tree, &path.to_string_lossy())? else {
            continue;
        };
        if let Some(old_path) = unused_deleted.remove(&hash) {
            renames.insert(path.clone(), old_path);
        }
    }
    Ok(renames)
}

fn blob_hash_at_path(
    source: &impl ObjectSource,
    tree: &ContentHash,
    path: &str,
) -> Result<Option<ContentHash>> {
    let resolved = resolve_tree_path(
        source,
        tree,
        std::path::Path::new(path),
        LeafPolicy::BlobOnly,
    )?;
    Ok(resolved.and_then(|target| target.content_hash))
}

fn join_semantic_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

#[cfg(test)]
#[path = "repository_semantic_corpus_tests.rs"]
mod tests;
