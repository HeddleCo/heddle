// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "tree-sitter-symbols")]

use std::collections::{BTreeSet, HashMap};

use objects::{
    object::{Attribution, OccurrenceRole, Principal, SemanticIndexRoot, StateId},
    store::ObjectStore,
};

use crate::SemanticGraphBind;
use tempfile::TempDir;

use crate::{Repository, ResolvedSemanticEdgeSet};

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
        Some("semantic graph".to_string()),
        None,
        Attribution::human(Principal::new("Test", "test@example.com")),
    )
    .unwrap()
    .id()
}

fn call_id(repo: &Repository, state: &StateId, path: &str, name: &str) -> u32 {
    repo.semantic_file_node(state, path)
        .unwrap()
        .unwrap()
        .occurrences
        .into_iter()
        .find(|occurrence| occurrence.role == OccurrenceRole::Call && occurrence.name == name)
        .unwrap()
        .local_id
}

fn overlay(
    mut parent: ResolvedSemanticEdgeSet,
    delta: objects::object::BindingDelta,
) -> ResolvedSemanticEdgeSet {
    for file in delta.files {
        if file.file_node.is_some() {
            parent.insert(file.path, file.replace_edges);
        } else {
            parent.remove(&file.path);
        }
    }
    parent
}

#[test]
fn rust_cross_file_roundtrip_and_frontier_delta_are_exact() {
    let (temp, repo) = repo();
    let first = snapshot(
        &repo,
        &temp,
        &[
            ("src/api.rs", "pub fn greet() -> u8 { 1 }\n"),
            (
                "src/client.rs",
                "use crate::api::greet;\npub fn run() { greet(); missing(); }\n",
            ),
            (
                "src/qualified.rs",
                "pub fn run_qualified() { crate::api::greet(); }\n",
            ),
            ("src/stable_api.rs", "pub fn stable() {}\n"),
            (
                "src/stable_client.rs",
                "use crate::stable_api::stable;\npub fn run_stable() { stable(); }\n",
            ),
        ],
    );
    let first_edges = repo.resolved_semantic_edges(&first).unwrap().unwrap();
    let greet = call_id(&repo, &first, "src/client.rs", "greet");
    let missing = call_id(&repo, &first, "src/client.rs", "missing");
    assert_eq!(
        repo.resolved_semantic_occurrence(&first, "src/client.rs", greet)
            .unwrap()
            .unwrap()
            .target_path,
        "src/api.rs"
    );
    assert!(
        repo.resolved_semantic_occurrence(&first, "src/client.rs", missing)
            .unwrap()
            .is_none(),
        "an unknown name must remain unresolved"
    );

    let second = snapshot(
        &repo,
        &temp,
        &[("src/api.rs", "pub fn greet() -> u8 { 2 }\n")],
    );
    let second_edges = repo.resolved_semantic_edges(&second).unwrap().unwrap();
    let delta = repo.semantic_edge_delta(&second).unwrap().unwrap();
    let delta_paths = delta
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        delta_paths,
        BTreeSet::from(["src/api.rs", "src/client.rs", "src/qualified.rs"]),
        "only the changed file and its importer belong to the frontier"
    );
    assert_eq!(overlay(first_edges.clone(), delta), second_edges);
    assert_eq!(
        first_edges["src/stable_client.rs"], second_edges["src/stable_client.rs"],
        "unrelated unchanged edges are inherited, not re-stored"
    );

    let first_root = repo.attached_semantic_index(&first).unwrap().unwrap();
    let second_root = repo.attached_semantic_index(&second).unwrap().unwrap();
    let second_delta = repo
        .load_binding_delta(&second_root.binding_delta.unwrap())
        .unwrap();
    assert_eq!(second_delta.parent, first_root.binding_delta);
    assert!(
        repo.store()
            .has_blob(&second_root.binding_delta.unwrap())
            .unwrap(),
        "the attached delta is a content-addressed blob"
    );
    let first_index = repo
        .load_reverse_dependency_index(&first_root.importer_index.unwrap())
        .unwrap();
    assert_eq!(
        first_index.importers_of("src/api.rs"),
        ["src/client.rs", "src/qualified.rs"]
    );
}

#[test]
fn opaque_only_change_inherits_bindings_without_a_repository_wide_resolve() {
    let (temp, repo) = repo();
    for index in 0..128 {
        let directory = temp.path().join(format!("data/{index:03}"));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("value.txt"), format!("value {index}\n")).unwrap();
    }
    let first = snapshot(&repo, &temp, &[]);
    std::fs::write(temp.path().join("data/000/value.txt"), "changed\n").unwrap();
    let second = snapshot(&repo, &temp, &[]);

    let first_root = repo.attached_semantic_index(&first).unwrap().unwrap();
    let second_root = repo.attached_semantic_index(&second).unwrap().unwrap();
    assert_ne!(
        first_root.tree, second_root.tree,
        "opaque content still changes the semantic index tree"
    );
    let second_delta = repo
        .load_binding_delta(&second_root.binding_delta.unwrap())
        .unwrap();
    assert_eq!(second_delta.parent, first_root.binding_delta);
    assert!(
        second_delta.files.is_empty(),
        "opaque files cannot change cross-file symbol bindings"
    );
    assert_eq!(
        first_root.importer_index, second_root.importer_index,
        "opaque-only capture content-address reuses the parent importer index"
    );
}

#[test]
fn typescript_named_import_resolves_across_files() {
    let (temp, repo) = repo();
    let state = snapshot(
        &repo,
        &temp,
        &[
            ("src/api.ts", "export function greet(): void {}\n"),
            (
                "src/client.ts",
                "import { greet as hello } from './api';\nexport function run() { hello(); unknown(); }\n",
            ),
        ],
    );
    let hello = call_id(&repo, &state, "src/client.ts", "hello");
    let unknown = call_id(&repo, &state, "src/client.ts", "unknown");
    assert_eq!(
        repo.resolved_semantic_occurrence(&state, "src/client.ts", hello)
            .unwrap()
            .unwrap()
            .target_path,
        "src/api.ts"
    );
    assert!(
        repo.resolved_semantic_occurrence(&state, "src/client.ts", unknown)
            .unwrap()
            .is_none()
    );
}

fn unbound_root(repo: &Repository, state: &StateId) -> SemanticIndexRoot {
    let root = repo.attached_semantic_index(state).unwrap().unwrap();
    SemanticIndexRoot::new(
        root.extractor_version,
        root.grammars,
        root.tree,
        root.semantic_digest,
    )
}

fn rebind(repo: &Repository, parent: &StateId, current: &StateId) -> SemanticGraphBind {
    let parent_state = repo.store().get_state(parent).unwrap().unwrap();
    repo.bind_semantic_graph(
        Some(&parent_state),
        unbound_root(repo, current),
        &HashMap::new(),
    )
    .unwrap()
}

/// GOLDEN (heddle#1275): editing A's exports re-resolves exactly A and A's
/// transitive importers — not the unrelated import lane.
#[test]
fn export_edit_reresolves_exactly_the_transitive_importer_frontier() {
    let (temp, repo) = repo();
    let first = snapshot(
        &repo,
        &temp,
        &[
            ("src/a.rs", "pub fn export_a() -> u8 { 1 }\n"),
            (
                "src/b.rs",
                "use crate::a::export_a;\npub fn export_b() -> u8 { export_a() }\n",
            ),
            (
                "src/c.rs",
                "use crate::b::export_b;\npub fn run() { export_b(); }\n",
            ),
            ("src/d.rs", "pub fn unrelated() {}\n"),
            (
                "src/e.rs",
                "use crate::d::unrelated;\npub fn use_d() { unrelated(); }\n",
            ),
        ],
    );
    let first_root = repo.attached_semantic_index(&first).unwrap().unwrap();
    let first_index = repo
        .load_reverse_dependency_index(&first_root.importer_index.unwrap())
        .unwrap();
    assert_eq!(first_index.importers_of("src/a.rs"), ["src/b.rs"]);
    assert_eq!(first_index.importers_of("src/b.rs"), ["src/c.rs"]);
    assert_eq!(first_index.importers_of("src/d.rs"), ["src/e.rs"]);

    let second = snapshot(
        &repo,
        &temp,
        &[("src/a.rs", "pub fn export_a() -> u8 { 2 }\n")],
    );
    let bound = rebind(&repo, &first, &second);
    assert_eq!(
        bound.frontier,
        BTreeSet::from([
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "src/c.rs".to_string()
        ]),
        "edit A's exports must invalidate exactly A's transitive importers"
    );
    assert_eq!(
        bound.resolve_count, 3,
        "only the frontier may re-resolve: {:?}",
        bound.frontier
    );

    let delta = repo.semantic_edge_delta(&second).unwrap().unwrap();
    let delta_paths = delta
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        delta_paths,
        BTreeSet::from(["src/a.rs", "src/b.rs", "src/c.rs"])
    );
}

#[test]
fn unrelated_export_lane_does_not_reresolve_a_importers() {
    let (temp, repo) = repo();
    let first = snapshot(
        &repo,
        &temp,
        &[
            ("src/a.rs", "pub fn export_a() -> u8 { 1 }\n"),
            (
                "src/b.rs",
                "use crate::a::export_a;\npub fn export_b() -> u8 { export_a() }\n",
            ),
            ("src/d.rs", "pub fn unrelated() {}\n"),
            (
                "src/e.rs",
                "use crate::d::unrelated;\npub fn use_d() { unrelated(); }\n",
            ),
        ],
    );
    let second = snapshot(&repo, &temp, &[("src/d.rs", "pub fn unrelated() { 1; }\n")]);
    let bound = rebind(&repo, &first, &second);
    assert_eq!(
        bound.frontier,
        BTreeSet::from(["src/d.rs".to_string(), "src/e.rs".to_string()])
    );
    assert_eq!(bound.resolve_count, 2);
}
