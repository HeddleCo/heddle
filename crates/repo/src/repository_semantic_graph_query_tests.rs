// SPDX-License-Identifier: Apache-2.0
//! Never-compute goldens for `refs_of` / `importers_of` (heddle#1276).

use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use objects::{
    object::{
        Attribution, BindingDelta, Blob, ByteSpan, ContentHash, FileBindingDelta, OccurrenceEntry,
        OccurrenceRole, Principal, ResolvedSemanticEdge, ReverseDependencyIndex, SemanticEdgeKind,
        SemanticEntryKind, SemanticFileFacts, SemanticFileNode, SemanticIndexRoot,
        SemanticTreeEntry, SemanticTreeNode, State, StateAttachment, StateAttachmentBody, StateId,
        SymbolAnchor, SymbolEntry, SymbolKindTag, SymbolNamespace,
    },
    store::ObjectStore,
};
use tempfile::TempDir;

use crate::Repository;

fn repo() -> (TempDir, Repository) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    (temp, repo)
}

fn author() -> Attribution {
    Attribution::human(Principal::new("Test", "test@example.com"))
}

fn put_bytes(repo: &Repository, bytes: Vec<u8>) -> ContentHash {
    repo.store().put_blob(&Blob::new(bytes)).unwrap()
}

fn file_node(name: &str, occurrence: Option<OccurrenceEntry>) -> SemanticFileNode {
    let symbol = SymbolEntry {
        name: name.to_string(),
        kind: SymbolKindTag::Function,
        container_path: vec![],
        semantic_hash: ContentHash::compute(name.as_bytes()),
        span: (1, 1),
    };
    SemanticFileNode::new(
        "rust",
        "0.24",
        1,
        ContentHash::compute(name.as_bytes()),
        ContentHash::compute(b"scaffold"),
        SemanticFileFacts {
            symbols: vec![symbol],
            occurrences: occurrence.into_iter().collect(),
            ..SemanticFileFacts::default()
        },
    )
}

fn occurrence(local_id: u32, name: &str, role: OccurrenceRole) -> OccurrenceEntry {
    OccurrenceEntry {
        local_id,
        role,
        name: name.to_string(),
        qualifier: vec![],
        namespace: SymbolNamespace::Value,
        scope: 0,
        span: ByteSpan::new(10, 16),
    }
}

fn edge(source_occurrence: u32, kind: SemanticEdgeKind) -> ResolvedSemanticEdge {
    ResolvedSemanticEdge {
        source_occurrence,
        target_path: "a.rs".to_string(),
        target_file_node: ContentHash::compute(b"a-file"),
        target_definition: 0,
        kind,
    }
}

struct GraphState {
    id: StateId,
    root_hash: ContentHash,
    importer_hash: ContentHash,
}

fn attach_graph(
    repo: &Repository,
    files: &[(&str, SemanticFileNode)],
    importers: BTreeMap<String, BTreeSet<String>>,
    parent_delta: Option<ContentHash>,
    delta_files: Vec<FileBindingDelta>,
) -> GraphState {
    let entries = files
        .iter()
        .map(|(name, node)| SemanticTreeEntry {
            name: (*name).to_string(),
            kind: SemanticEntryKind::File,
            node: put_bytes(repo, node.encode().unwrap()),
            semantic_digest: node.semantic_digest,
        })
        .collect();
    let (tree, digest) = SemanticTreeNode::new(entries);
    let tree_hash = put_bytes(repo, tree.encode().unwrap());
    let importer = ReverseDependencyIndex::new(importers);
    let importer_hash = put_bytes(repo, importer.encode().unwrap());
    let delta = BindingDelta::new(parent_delta, delta_files);
    let delta_hash = put_bytes(repo, delta.encode().unwrap());
    let root = SemanticIndexRoot::new(1, BTreeMap::new(), tree_hash, digest)
        .with_binding_delta(delta_hash, 1)
        .with_importer_index(importer_hash);
    let root_hash = put_bytes(repo, root.encode().unwrap());
    let state = State::new(ContentHash::compute(b"dangling-tree"), vec![], author());
    repo.store().put_state(&state).unwrap();
    repo.put_state_attachment(&StateAttachment {
        state_id: state.id(),
        body: StateAttachmentBody::SemanticIndex(root_hash),
        attribution: author(),
        created_at: Utc::now(),
        supersedes: None,
    })
    .unwrap();
    GraphState {
        id: state.id(),
        root_hash,
        importer_hash,
    }
}

fn first_graph(repo: &Repository) -> GraphState {
    let a = file_node("greet", None);
    let b = file_node("run", Some(occurrence(1, "greet", OccurrenceRole::Call)));
    attach_graph(
        repo,
        &[("a.rs", a), ("b.rs", b)],
        BTreeMap::from([("a.rs".into(), BTreeSet::from(["b.rs".into()]))]),
        None,
        vec![FileBindingDelta::new(
            "b.rs",
            Some(ContentHash::compute(b"b")),
            vec![edge(1, SemanticEdgeKind::Calls)],
        )],
    )
}

#[test]
fn graph_queries_are_absent_without_an_attached_index() {
    let (_temp, repo) = repo();
    let state = State::new(ContentHash::compute(b"no-index"), vec![], author());
    repo.store().put_state(&state).unwrap();
    let id = state.id();
    let anchor = SymbolAnchor::new("a.rs", "greet");
    assert!(repo.refs_of(&id, &anchor).unwrap().is_none());
    assert!(repo.callers_of(&id, &anchor).unwrap().is_none());
    assert!(repo.importers_of(&id, "a.rs").unwrap().is_none());
    assert!(repo.attached_semantic_index(&id).unwrap().is_none());
}

#[test]
fn time_travel_reads_the_old_attached_graph_without_recompute() {
    let (_temp, repo) = repo();
    let first = first_graph(&repo);
    let first_root = repo.attached_semantic_index(&first.id).unwrap().unwrap();
    let first_delta = first_root.binding_delta;
    let c = file_node(
        "other",
        Some(occurrence(2, "greet", OccurrenceRole::Reference)),
    );
    let second = attach_graph(
        &repo,
        &[
            ("a.rs", file_node("greet", None)),
            (
                "b.rs",
                file_node("run", Some(occurrence(1, "greet", OccurrenceRole::Call))),
            ),
            ("c.rs", c),
        ],
        BTreeMap::from([(
            "a.rs".into(),
            BTreeSet::from(["b.rs".into(), "c.rs".into()]),
        )]),
        first_delta,
        vec![FileBindingDelta::new(
            "c.rs",
            Some(ContentHash::compute(b"c")),
            vec![edge(2, SemanticEdgeKind::RefersTo)],
        )],
    );

    let anchor = SymbolAnchor::new("a.rs", "greet");
    let old_refs = repo.refs_of(&first.id, &anchor).unwrap().unwrap();
    let old_callers = repo.callers_of(&first.id, &anchor).unwrap().unwrap();
    let old_importers = repo.importers_of(&first.id, "a.rs").unwrap().unwrap();
    assert_eq!(old_refs.len(), 1);
    assert_eq!(old_refs[0].source_path, "b.rs");
    assert_eq!(old_refs[0].kind, SemanticEdgeKind::Calls);
    assert_eq!(old_callers, old_refs);
    assert_eq!(old_importers, vec!["b.rs".to_string()]);

    let new_refs = repo.refs_of(&second.id, &anchor).unwrap().unwrap();
    let new_callers = repo.callers_of(&second.id, &anchor).unwrap().unwrap();
    assert_eq!(
        new_refs
            .iter()
            .map(|r| r.source_path.as_str())
            .collect::<Vec<_>>(),
        ["b.rs", "c.rs"]
    );
    assert_eq!(new_callers.len(), 1);
    assert_eq!(
        repo.importers_of(&second.id, "a.rs").unwrap().unwrap(),
        ["b.rs", "c.rs"]
    );

    let after = repo.attached_semantic_index(&first.id).unwrap().unwrap();
    assert_eq!(after.tree, first_root.tree);
    assert!(
        repo.store().get_blob(&first.root_hash).unwrap().is_some(),
        "querying the old state must not replace its attached root"
    );
}

#[test]
fn shared_importer_index_hash_short_circuits_importers_of() {
    let (_temp, repo) = repo();
    let first = first_graph(&repo);
    let reused = attach_graph(
        &repo,
        &[("a.rs", file_node("greet", None))],
        BTreeMap::from([("a.rs".into(), BTreeSet::from(["b.rs".into()]))]),
        None,
        vec![],
    );
    assert_eq!(first.importer_hash, reused.importer_hash);
    assert_eq!(
        repo.importers_of(&first.id, "a.rs").unwrap(),
        repo.importers_of(&reused.id, "a.rs").unwrap()
    );
}
