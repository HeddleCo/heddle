// SPDX-License-Identifier: Apache-2.0
//! Capture-time [`SemanticContext`] for the risk-signal registry.
//!
//! `changed_paths` come from the snapshot tree diff, then formatting-only
//! churn is pruned by the merkle index (`digest_at_path`, the same
//! comparison [`Repository::semantic_changed`] uses). Functions are parsed
//! only for that pruned set via the shared [`SemanticParseCache`].

#![cfg(feature = "tree-sitter-symbols")]

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use objects::{
    object::{ContentHash, LeafPolicy, SemanticIndexRoot, State, Tree, resolve_tree_path},
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
const PARSE_BUDGET_BYTES: usize = 1 << 20;

/// Build the capture-time context for `run_all`.
///
/// `new_index` is the just-computed merkle root hash (not yet attached).
/// When both that root and the prior state's attached index are readable,
/// paths whose digests match are dropped so a fmt-sweep parses nothing.
pub(crate) fn build_semantic_context(
    repo: &Repository,
    prior: Option<&State>,
    new: &State,
    new_index: Option<&ContentHash>,
) -> Result<SemanticContext> {
    let from_tree = prior_tree_hash(repo, prior)?;
    let changes = repo.diff_trees(&from_tree, &new.tree)?;
    let prior_root = prior
        .map(|state| repo.attached_semantic_index(&state.id()))
        .transpose()?
        .flatten();
    let new_root = new_index
        .map(|hash| load_new_index_root(repo, hash))
        .transpose()?
        .flatten();

    let mut changed_paths = BTreeSet::new();
    for change in changes.iter() {
        if !change.kind.is_change() {
            continue;
        }
        if !path_semantically_changed(repo, prior_root.as_ref(), new_root.as_ref(), &change.path)? {
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
        if let Some(fns) = parse_tree_functions(repo, prior_tree.as_ref(), &path_str, cache) {
            prior_functions.insert(path.clone(), fns);
        }
        if let Some(fns) = parse_tree_functions(repo, Some(&new.tree), &path_str, cache) {
            new_functions.insert(path.clone(), fns);
        }
    }

    Ok(SemanticContext {
        prior_functions,
        new_functions,
        changed_paths,
    })
}

fn prior_tree_hash(repo: &Repository, prior: Option<&State>) -> Result<ContentHash> {
    if let Some(state) = prior {
        return Ok(state.tree);
    }
    let empty = Tree::new();
    repo.store().put_tree(&empty)
}

fn load_new_index_root(
    repo: &Repository,
    hash: &ContentHash,
) -> Result<Option<SemanticIndexRoot>> {
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
    repo: &Repository,
    prior_root: Option<&SemanticIndexRoot>,
    new_root: Option<&SemanticIndexRoot>,
    path: &str,
) -> Result<bool> {
    let (Some(prior_root), Some(new_root)) = (prior_root, new_root) else {
        return Ok(true);
    };
    let prior_digest = repo.digest_at_path(prior_root, path)?;
    let new_digest = repo.digest_at_path(new_root, path)?;
    Ok(prior_digest != new_digest)
}

fn parse_tree_functions(
    repo: &Repository,
    tree: Option<&ContentHash>,
    path: &str,
    cache: &SemanticParseCache,
) -> Option<Vec<FunctionDef>> {
    let tree = tree?;
    let language = Language::from_path(Path::new(path));
    if matches!(language, Language::Unknown) {
        return None;
    }
    let bytes = match blob_bytes_at_path(repo, tree, path) {
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
    let source = std::str::from_utf8(&bytes).ok()?;
    let parsed = cache.parse(source, language)?;
    Some(parsed.extract_functions())
}

fn blob_bytes_at_path(repo: &Repository, tree: &ContentHash, path: &str) -> Result<Option<Vec<u8>>> {
    let resolved = resolve_tree_path(repo.store(), tree, Path::new(path), LeafPolicy::BlobOnly)?;
    let Some(hash) = resolved.and_then(|target| target.content_hash) else {
        return Ok(None);
    };
    Ok(repo
        .store()
        .get_blob(&hash)?
        .map(|blob| blob.content().to_vec()))
}

#[cfg(test)]
mod tests {
    use objects::object::{Attribution, Principal, RiskSignalKind};
    use tempfile::TempDir;

    use super::*;
    use crate::StateAttachmentKind;

    fn author() -> Attribution {
        Attribution::human(Principal::new("Test", "test@example.com"))
    }

    fn snapshot(repo: &Repository, root: &std::path::Path, path: &str, content: &str) -> State {
        std::fs::write(root.join(path), content).unwrap();
        repo.snapshot_with_attribution(Some("capture".to_string()), None, author())
            .unwrap()
    }

    fn attachment_hash(
        repo: &Repository,
        state: &State,
        kind: StateAttachmentKind,
    ) -> Option<ContentHash> {
        repo.latest_state_attachment(&state.id(), kind)
            .unwrap()
            .and_then(|attachment| match attachment.body {
                objects::object::StateAttachmentBody::RiskSignals(hash)
                | objects::object::StateAttachmentBody::SemanticIndex(hash) => Some(hash),
                _ => None,
            })
    }

    fn load_risk_kinds(repo: &Repository, state: &State) -> Vec<RiskSignalKind> {
        let hash = match attachment_hash(repo, state, StateAttachmentKind::RiskSignals) {
            Some(hash) => hash,
            None => return Vec::new(),
        };
        let blob = repo.store().get_blob(&hash).unwrap().unwrap();
        objects::object::RiskSignalBlob::decode(blob.content())
            .unwrap()
            .signals
            .into_iter()
            .map(|signal| signal.kind)
            .collect()
    }

    const CORPUS: &str = "\
fn alpha() { let total = first + second + third + fourth; }
fn beta() { for widget in inventory { ship(widget); } }
fn gamma() { match colour { Red => stop(), Green => go() } }
fn delta() { while pending { dequeue().handle(); } flush(); }
";

    #[test]
    fn fmt_sweep_prunes_changed_paths_and_persists_no_tree_sitter_signals() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let first = snapshot(&repo, temp.path(), "hello.rs", "fn foo() -> i32 { 1 }\n");
        let reformatted = snapshot(
            &repo,
            temp.path(),
            "hello.rs",
            "fn foo() -> i32 {\n    1\n}\n",
        );

        let new_index = attachment_hash(&repo, &reformatted, StateAttachmentKind::SemanticIndex);
        let ctx =
            build_semantic_context(&repo, Some(&first), &reformatted, new_index.as_ref()).unwrap();
        assert!(
            ctx.changed_paths.is_empty(),
            "fmt-sweep must prune semantic changed_paths: {ctx:?}"
        );
        assert!(ctx.new_functions.is_empty());
        assert!(
            load_risk_kinds(&repo, &reformatted)
                .iter()
                .all(|kind| !matches!(
                    kind,
                    RiskSignalKind::Novelty
                        | RiskSignalKind::PatternDeviation
                        | RiskSignalKind::TestReachability
                )),
            "fmt-sweep must persist zero tree-sitter signals"
        );
    }

    #[test]
    fn novel_shape_populates_context_and_fires_novelty() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let state = snapshot(&repo, temp.path(), "changed.rs", CORPUS);

        let kinds = load_risk_kinds(&repo, &state);
        assert!(
            kinds.contains(&RiskSignalKind::Novelty),
            "novel-shape capture must persist novelty, got {kinds:?}"
        );

        let index_hash = attachment_hash(&repo, &state, StateAttachmentKind::SemanticIndex);
        let ctx = build_semantic_context(&repo, None, &state, index_hash.as_ref()).unwrap();
        assert!(ctx.changed_paths.contains(&PathBuf::from("changed.rs")));
        let fns = ctx
            .new_functions
            .get(&PathBuf::from("changed.rs"))
            .expect("changed.rs must be parsed");
        assert_eq!(fns.len(), 4, "parse only the changed file: {fns:?}");
    }
}
