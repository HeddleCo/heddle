// SPDX-License-Identifier: Apache-2.0

use objects::object::{ContentHash, SemanticFileFacts, SemanticFileNode};

use super::*;
use crate::{
    parser::Language,
    semantic_index::{EXTRACTOR_VERSION, extract_semantic_file, grammar_version, language_name},
};

fn file(source: &str, language: Language) -> RepositorySemanticFile {
    let extracted = extract_semantic_file(source.as_bytes(), language).expect("source parses");
    let source_hash = ContentHash::compute(source.as_bytes());
    let node = SemanticFileNode::new(
        language_name(language),
        grammar_version(language),
        EXTRACTOR_VERSION,
        source_hash,
        extracted.scaffold_hash,
        SemanticFileFacts {
            symbols: extracted.symbols,
            scopes: extracted.scopes,
            imports: extracted.imports,
            occurrences: extracted.occurrences,
        },
    );
    let node_hash = ContentHash::compute(&node.encode().unwrap());
    RepositorySemanticFile { node_hash, node }
}

#[test]
fn rust_imported_call_resolves_and_missing_name_stays_unresolved() {
    let mut files = BTreeMap::new();
    files.insert(
        "src/api.rs".to_string(),
        file("pub fn greet() {}\n", Language::Rust),
    );
    files.insert(
        "src/client.rs".to_string(),
        file(
            "use crate::api::greet;\nfn run() { greet(); missing(); }\n",
            Language::Rust,
        ),
    );

    let resolution = resolve_repository(&files);
    let only_client = resolve_paths(&files, ["src/client.rs"]);
    assert_eq!(
        only_client.len(),
        1,
        "resolve_paths must not visit other files"
    );
    assert_eq!(only_client["src/client.rs"], resolution["src/client.rs"]);
    let client = &resolution["src/client.rs"];
    assert_eq!(
        client.dependencies,
        BTreeSet::from(["src/api.rs".to_string()])
    );
    assert_eq!(
        client.edges.len(),
        1,
        "missing must not bind to another name"
    );
    assert_eq!(client.edges[0].target_path, "src/api.rs");
    let target = &files["src/api.rs"].node.symbols[client.edges[0].target_definition as usize];
    assert_eq!(target.name, "greet");
}

#[test]
fn rust_qualified_path_resolves() {
    let mut files = BTreeMap::new();
    files.insert(
        "src/api.rs".to_string(),
        file("pub fn greet() {}\n", Language::Rust),
    );
    files.insert(
        "src/qualified.rs".to_string(),
        file("fn run() { crate::api::greet(); }\n", Language::Rust),
    );
    let resolution = resolve_repository(&files);
    assert_eq!(resolution["src/qualified.rs"].edges.len(), 1);
    assert_eq!(
        resolution["src/qualified.rs"].edges[0].target_path,
        "src/api.rs"
    );
}

#[cfg(feature = "lang-typescript")]
#[test]
fn typescript_aliased_import_resolves_across_files() {
    let mut files = BTreeMap::new();
    files.insert(
        "src/api.ts".to_string(),
        file("export function greet() {}\n", Language::TypeScript),
    );
    files.insert(
        "src/client.ts".to_string(),
        file(
            "import { greet as hello } from './api';\nexport function run() { hello(); }\n",
            Language::TypeScript,
        ),
    );

    let resolution = resolve_repository(&files);
    let client = &resolution["src/client.ts"];
    assert_eq!(client.edges.len(), 1);
    assert_eq!(client.edges[0].target_path, "src/api.ts");
    let target = &files["src/api.ts"].node.symbols[client.edges[0].target_definition as usize];
    assert_eq!(target.name, "greet");
}
