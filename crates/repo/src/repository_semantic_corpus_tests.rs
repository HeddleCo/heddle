// SPDX-License-Identifier: Apache-2.0
//! Tests for the capture-time function corpus walk.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use objects::object::{
    ContentHash, ObjectSource, SemanticEntryKind, SemanticFileFacts, SemanticFileNode,
    SemanticIndexRoot, SemanticTreeEntry, SemanticTreeNode, SymbolEntry, SymbolKindTag,
};
use semantic::{SemanticParseCache, parser::FunctionDef};

use super::{
    CORPUS_BYTE_BUDGET, CORPUS_FILE_BUDGET, CorpusBudget, collect_changed_symbols,
    collect_function_file_paths, populate_new_function_corpus,
};

fn fdef(name: &str, content: &str) -> FunctionDef {
    FunctionDef {
        name: name.to_string(),
        container: String::new(),
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
    assert_eq!(
        symbols,
        BTreeSet::from([(path, fdef("edit", "fn edit() { 2 }").symbol_identity())])
    );
}

#[test]
fn changed_symbols_distinguish_duplicate_names_by_container() {
    let path = PathBuf::from("lib.rs");
    let mut changed_paths = BTreeSet::new();
    changed_paths.insert(path.clone());
    let foo_run = FunctionDef {
        name: "run".to_string(),
        container: "Foo".to_string(),
        signature: "run()".to_string(),
        start_line: 1,
        end_line: 3,
        content: "fn run() { 1 }".to_string(),
    };
    let bar_run = FunctionDef {
        name: "run".to_string(),
        container: "Bar".to_string(),
        signature: "run()".to_string(),
        start_line: 5,
        end_line: 7,
        content: "fn run() { 1 }".to_string(),
    };
    let mut foo_edited = foo_run.clone();
    foo_edited.content = "fn run() { 2 }".to_string();
    let mut prior = BTreeMap::new();
    prior.insert(path.clone(), vec![foo_run, bar_run.clone()]);
    let mut new = BTreeMap::new();
    new.insert(path.clone(), vec![foo_edited.clone(), bar_run.clone()]);
    let symbols = collect_changed_symbols(&changed_paths, &prior, &new);
    assert_eq!(
        symbols,
        BTreeSet::from([(path, foo_edited.symbol_identity())])
    );
    assert!(
        !symbols
            .iter()
            .any(|(_, id)| id == &bar_run.symbol_identity()),
        "editing Foo::run must not mark Bar::run: {symbols:?}"
    );
}

#[test]
fn corpus_budget_rejects_file_and_byte_overflow() {
    let mut budget = CorpusBudget::default();
    for _ in 0..CORPUS_FILE_BUDGET {
        assert!(budget.try_add(1));
    }
    assert!(!budget.try_add(1), "file ceiling must fail-close");
    let mut bytes = CorpusBudget::default();
    assert!(bytes.try_add(CORPUS_BYTE_BUDGET));
    assert!(!bytes.try_add(1), "byte ceiling must fail-close");
}

#[test]
fn index_walk_stops_when_file_budget_exhausted() {
    let extra = 20;
    let total = CORPUS_FILE_BUDGET + extra;
    let (root, source) = index_with_function_files(total);
    let mut files = Vec::new();
    assert!(
        !collect_function_file_paths(
            &source,
            &root,
            CORPUS_FILE_BUDGET,
            &BTreeSet::new(),
            &mut files,
        )
        .unwrap(),
        "more function files than the remaining budget must fail-close"
    );
    assert!(
        files.len() <= CORPUS_FILE_BUDGET,
        "walk must not collect the whole index: {}",
        files.len()
    );
    assert!(
        source.file_loads.get() <= CORPUS_FILE_BUDGET,
        "default capture must not decode every index file: loaded {}",
        source.file_loads.get()
    );
}

fn index_with_function_files(count: usize) -> (SemanticIndexRoot, CountingSource) {
    let mut blobs = HashMap::new();
    let mut file_hashes = BTreeSet::new();
    let mut entries = Vec::new();
    for index in 0..count {
        let file = function_index_file(&format!("f{index}"));
        let file_bytes = file.encode().unwrap();
        let file_hash = ContentHash::compute(&file_bytes);
        file_hashes.insert(file_hash);
        blobs.insert(file_hash, file_bytes);
        entries.push(SemanticTreeEntry {
            name: format!("f{index}.rs"),
            kind: SemanticEntryKind::File,
            node: file_hash,
            semantic_digest: file.semantic_digest,
        });
    }
    let (tree_node, _) = SemanticTreeNode::new(entries);
    let tree_bytes = tree_node.encode().unwrap();
    let tree_hash = ContentHash::compute(&tree_bytes);
    blobs.insert(tree_hash, tree_bytes);
    let root = SemanticIndexRoot::new(1, BTreeMap::new(), tree_hash, tree_node.semantic_digest());
    (
        root,
        CountingSource {
            blobs,
            file_hashes,
            file_loads: Cell::new(0),
        },
    )
}

fn function_index_file(name: &str) -> SemanticFileNode {
    SemanticFileNode::new(
        "rust",
        "0",
        1,
        ContentHash::compute(name.as_bytes()),
        ContentHash::compute(name.as_bytes()),
        SemanticFileFacts {
            symbols: vec![SymbolEntry {
                name: name.to_string(),
                kind: SymbolKindTag::Function,
                container_path: Vec::new(),
                semantic_hash: ContentHash::compute(name.as_bytes()),
                span: (1, 2),
            }],
            ..SemanticFileFacts::default()
        },
    )
}

#[test]
fn missing_index_is_incomplete() {
    assert!(
        !populate_new_function_corpus(
            &FailingSource,
            None,
            &ContentHash::compute(b"tree"),
            SemanticParseCache::shared(),
            &mut CorpusBudget::default(),
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
            &mut CorpusBudget::default(),
            &mut BTreeMap::new(),
        )
        .unwrap(),
        "a listed function file that fails to parse must fail-close the corpus"
    );
}

struct CountingSource {
    blobs: HashMap<ContentHash, Vec<u8>>,
    file_hashes: BTreeSet<ContentHash>,
    file_loads: Cell<usize>,
}

impl ObjectSource for CountingSource {
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
        if self.file_hashes.contains(hash) {
            self.file_loads.set(self.file_loads.get() + 1);
        }
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
