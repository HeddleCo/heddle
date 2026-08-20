// SPDX-License-Identifier: Apache-2.0
//! Bounded new-state function corpus for capture-time risk signals.

#![cfg(feature = "tree-sitter-symbols")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use objects::object::{
    ContentHash, ObjectSource, SemanticEntryKind, SemanticFileNode, SemanticIndexRoot,
    SymbolKindTag,
};
use semantic::{SemanticParseCache, parser::FunctionDef};
use tracing::warn;

use crate::{Result, repository_semantic_query::MAX_SEMANTIC_TREE_DEPTH};

use super::repository_semantic_context::{
    PARSE_BUDGET_BYTES, load_semantic_tree, parse_tree_functions_sized,
};

/// Additional files parsed beyond the emit-scope `changed_paths`. Hitting
/// this bound fail-closes (`corpus_complete = false`) so novelty /
/// test_reachability stay quiet on a partial repo. Tens of files — well
/// under the shared [`SemanticParseCache`] page cap (256).
const CORPUS_FILE_BUDGET: usize = 32;
/// Extra bytes parsed for the corpus walk (on top of changed-file parses).
/// Two times the per-file semantic-index opaque threshold.
const CORPUS_BYTE_BUDGET: usize = PARSE_BUDGET_BYTES.saturating_mul(2);

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
                changed.insert((path.clone(), fn_def.name.clone()));
            }
        }
    }
    changed
}

fn function_changed(prior_fns: Option<&[FunctionDef]>, new_fn: &FunctionDef) -> bool {
    let Some(prior_fns) = prior_fns else {
        return true;
    };
    match prior_fns.iter().find(|prior| prior.name == new_fn.name) {
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
    new_functions: &mut BTreeMap<PathBuf, Vec<FunctionDef>>,
) -> Result<bool> {
    let Some(root) = new_root else {
        return Ok(false);
    };
    let mut files = Vec::new();
    if !collect_function_file_paths(source, root, &mut files)? {
        return Ok(false);
    }
    let mut extra_files = 0usize;
    let mut extra_bytes = 0usize;
    for path in files {
        let rel = PathBuf::from(&path);
        if new_functions.contains_key(&rel) {
            continue;
        }
        if extra_files >= CORPUS_FILE_BUDGET {
            return Ok(false);
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
        extra_bytes = extra_bytes.saturating_add(bytes);
        if extra_bytes > CORPUS_BYTE_BUDGET {
            return Ok(false);
        }
        extra_files += 1;
        new_functions.insert(rel, functions);
    }
    Ok(true)
}

fn collect_function_file_paths(
    source: &impl ObjectSource,
    root: &SemanticIndexRoot,
    out: &mut Vec<String>,
) -> Result<bool> {
    let mut stack = vec![(String::new(), root.tree, 0usize)];
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
                SemanticEntryKind::File => match load_semantic_file(source, &entry.node) {
                    Ok(file) if file_has_function(&file) => out.push(path),
                    Ok(_) => {}
                    Err(err) => {
                        warn!(
                            error = %err,
                            path,
                            "semantic corpus: file node unreadable; fail-closed"
                        );
                        return Ok(false);
                    }
                },
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
