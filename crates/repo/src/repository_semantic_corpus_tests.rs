// SPDX-License-Identifier: Apache-2.0
//! Tests for the capture-time function corpus walk.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use objects::object::{
    ContentHash, ObjectSource, SemanticEntryKind, SemanticFileFacts, SemanticFileNode,
    SemanticIndexRoot, SemanticTreeEntry, SemanticTreeNode, SymbolEntry, SymbolKindTag,
};
use semantic::{SemanticParseCache, parser::FunctionDef};

use super::{collect_changed_symbols, populate_new_function_corpus};

fn fdef(name: &str, content: &str) -> FunctionDef {
    FunctionDef {
        name: name.to_string(),
        signature: format!("fn {name}()"),
        start_line: 1,
        end_line: 3,
        content: content.to_string(),
    }
}

#[test]
fn changed_symbols_ignore_untouched_siblings() {
    let path = PathBuf::from("lib.rs");
    let mut changed_paths = BTreeSet::new();
    changed_paths.insert(path.clone());
    let mut prior = BTreeMap::new();
    prior.insert(
        path.clone(),
        vec![
            fdef("keep", "fn keep() { 1 }"),
            fdef("edit", "fn edit() { 1 }"),
        ],
    );
    let mut new = BTreeMap::new();
    new.insert(
        path.clone(),
        vec![
            fdef("keep", "fn keep() { 1 }"),
            fdef("edit", "fn edit() { 2 }"),
        ],
    );
    let symbols = collect_changed_symbols(&changed_paths, &prior, &new);
    assert_eq!(symbols, BTreeSet::from([(path, "edit".to_string())]));
}

#[test]
fn missing_index_is_incomplete() {
    assert!(
        !populate_new_function_corpus(
            &FailingSource,
            None,
            &ContentHash::compute(b"tree"),
            SemanticParseCache::shared(),
            &mut BTreeMap::new(),
        )
        .unwrap()
    );
}

#[test]
fn index_listed_parse_miss_is_incomplete() {
    let file = SemanticFileNode::new(
        "rust",
        "0",
        1,
        ContentHash::compute(b"src"),
        ContentHash::compute(b"scaffold"),
        SemanticFileFacts {
            symbols: vec![SymbolEntry {
                name: "foo".to_string(),
                kind: SymbolKindTag::Function,
                container_path: Vec::new(),
                semantic_hash: ContentHash::compute(b"foo"),
                span: (1, 2),
            }],
            ..SemanticFileFacts::default()
        },
    );
    let file_bytes = file.encode().unwrap();
    let file_hash = ContentHash::compute(&file_bytes);
    let (tree_node, _) = SemanticTreeNode::new(vec![SemanticTreeEntry {
        name: "lib.rs".to_string(),
        kind: SemanticEntryKind::File,
        node: file_hash,
        semantic_digest: file.semantic_digest,
    }]);
    let tree_bytes = tree_node.encode().unwrap();
    let tree_hash = ContentHash::compute(&tree_bytes);
    let root = SemanticIndexRoot::new(1, BTreeMap::new(), tree_hash, tree_node.semantic_digest());

    let mut blobs = HashMap::new();
    blobs.insert(file_hash, file_bytes);
    blobs.insert(tree_hash, tree_bytes);
    let source = IndexOnlySource { blobs };

    assert!(
        !populate_new_function_corpus(
            &source,
            Some(&root),
            &ContentHash::compute(b"missing-source-tree"),
            SemanticParseCache::shared(),
            &mut BTreeMap::new(),
        )
        .unwrap(),
        "a listed function file that fails to parse must fail-close the corpus"
    );
}

struct IndexOnlySource {
    blobs: HashMap<ContentHash, Vec<u8>>,
}

impl ObjectSource for IndexOnlySource {
    fn get_tree(
        &self,
        _hash: &ContentHash,
    ) -> objects::error::Result<Option<objects::object::Tree>> {
        Ok(None)
    }

    fn get_blob(
        &self,
        hash: &ContentHash,
    ) -> objects::error::Result<Option<objects::object::Blob>> {
        Ok(self
            .blobs
            .get(hash)
            .map(|bytes| objects::object::Blob::new(bytes.clone())))
    }

    fn get_state(
        &self,
        _id: &objects::object::StateId,
    ) -> objects::error::Result<Option<objects::object::State>> {
        Ok(None)
    }
}

struct FailingSource;

impl ObjectSource for FailingSource {
    fn get_tree(
        &self,
        _hash: &ContentHash,
    ) -> objects::error::Result<Option<objects::object::Tree>> {
        Ok(None)
    }

    fn get_blob(
        &self,
        _hash: &ContentHash,
    ) -> objects::error::Result<Option<objects::object::Blob>> {
        Ok(None)
    }

    fn get_state(
        &self,
        _id: &objects::object::StateId,
    ) -> objects::error::Result<Option<objects::object::State>> {
        Ok(None)
    }
}
