// SPDX-License-Identifier: Apache-2.0
//! In-tree rust goldens for parse-free graph queries over a captured index.

#![cfg(feature = "tree-sitter-symbols")]

use objects::object::{Attribution, Principal, SemanticEdgeKind, StateId, SymbolAnchor};
use tempfile::TempDir;

use crate::Repository;

fn repo() -> (TempDir, Repository) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    (temp, repo)
}

fn snapshot(repo: &Repository, temp: &TempDir, files: &[(&str, &str)]) -> StateId {
    for (path, content) in files {
        let path = temp.path().join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }
    repo.snapshot_with_attribution(
        Some("semantic graph query".to_string()),
        None,
        Attribution::human(Principal::new("Test", "test@example.com")),
    )
    .unwrap()
    .id()
}

#[test]
fn rust_refs_and_importers_are_state_anchored_without_reparse() {
    let (temp, repo) = repo();
    let first = snapshot(
        &repo,
        &temp,
        &[
            ("src/api.rs", "pub fn greet() -> u8 { 1 }\n"),
            (
                "src/client.rs",
                "use crate::api::greet;\npub fn run() { greet(); }\n",
            ),
            (
                "src/qualified.rs",
                "pub fn run_qualified() { crate::api::greet(); }\n",
            ),
        ],
    );
    let first_root = repo.attached_semantic_index(&first).unwrap().unwrap();
    let greet = SymbolAnchor::new("src/api.rs", "greet");

    std::fs::write(
        temp.path().join("src/api.rs"),
        "pub fn greet() -> u8 { 2 }\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("src/extra.rs"),
        "pub fn extra() { crate::api::greet(); }\n",
    )
    .unwrap();

    let dirty_refs = repo.refs_of(&first, &greet).unwrap().unwrap();
    let dirty_importers = repo.importers_of(&first, "src/api.rs").unwrap().unwrap();
    assert_eq!(
        dirty_refs
            .iter()
            .map(|r| r.source_path.as_str())
            .collect::<Vec<_>>(),
        ["src/client.rs", "src/qualified.rs"]
    );
    assert_eq!(dirty_importers, ["src/client.rs", "src/qualified.rs"]);
    assert_eq!(
        repo.attached_semantic_index(&first).unwrap().unwrap().tree,
        first_root.tree,
        "querying an old state must not rebuild its attached index from the dirty worktree"
    );

    let second = snapshot(&repo, &temp, &[]);
    let later_refs = repo.refs_of(&second, &greet).unwrap().unwrap();
    assert!(
        later_refs.iter().any(|r| r.source_path == "src/extra.rs"),
        "the later state must see the new importer: {later_refs:?}"
    );
    assert_eq!(
        repo.refs_of(&first, &greet)
            .unwrap()
            .unwrap()
            .iter()
            .map(|r| r.source_path.as_str())
            .collect::<Vec<_>>(),
        ["src/client.rs", "src/qualified.rs"],
        "time-travel must keep the old state's refs"
    );
    assert_eq!(
        repo.importers_of(&first, "src/api.rs").unwrap().unwrap(),
        ["src/client.rs", "src/qualified.rs"]
    );
    assert!(
        repo.callers_of(&first, &greet)
            .unwrap()
            .unwrap()
            .iter()
            .all(|r| r.kind == SemanticEdgeKind::Calls)
    );
}
