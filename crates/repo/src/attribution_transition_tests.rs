// SPDX-License-Identifier: Apache-2.0
use super::*;
use objects::{
    object::{
        AttributionBasis, AttributionClaim, AttributionOperation, AttributionOperationIdentity,
        AttributionScope, AttributionSource, ModelAttribution, TreeEntry,
    },
    store::{InMemoryStore, ObjectStore},
};

fn hash(value: &str) -> ContentHash {
    Blob::from_slice(value.as_bytes()).hash()
}
fn tree(value: Option<&str>) -> Tree {
    Tree::from_entries(
        value
            .into_iter()
            .map(|value| TreeEntry::file("file", hash(value), false).expect("file"))
            .collect(),
    )
}
fn operation(
    path: &str,
    before: Option<&str>,
    after: Option<&str>,
    model: &str,
) -> AttributionOperation {
    AttributionOperation {
        identity: AttributionOperationIdentity {
            selected: ModelAttribution {
                model: Some(AttributionClaim::new(
                    model,
                    AttributionBasis::RequestReported,
                    AttributionSource::HarnessHook,
                )),
                ..Default::default()
            },
            scope: AttributionScope {
                tool_call_id: Some(format!("tool-{model}")),
                ..Default::default()
            },
            ..Default::default()
        },
        changes: vec![AttributionFileChange {
            path: path.into(),
            before: before.map(hash),
            after: after.map(hash),
        }],
        resolution: AttributionOperationResolution::ContentBound,
    }
}
fn evidence(operations: Vec<AttributionOperation>) -> AttributionEvidenceV1 {
    AttributionEvidenceV1 {
        operations,
        ..Default::default()
    }
}

#[test]
fn bound_multi_model_chain_requires_actual_first_parent_and_final_content() {
    let store = InMemoryStore::new();
    let evidence = evidence(vec![
        operation("file", Some("before"), Some("middle"), "model-a"),
        operation("file", Some("middle"), Some("after"), "model-b"),
    ]);
    evidence.to_blob().expect("valid scoped contributors");
    let validate = |evidence: &AttributionEvidenceV1, parent: &Tree, actual: &Tree| {
        validate_attribution_transitions(&store, evidence, Some(parent), actual, &HashMap::new())
    };
    validate(&evidence, &tree(Some("before")), &tree(Some("after")))
        .expect("unbroken content chain");
    assert!(validate(&evidence, &tree(Some("raced-before")), &tree(Some("after"))).is_err());
    assert!(validate(&evidence, &tree(Some("before")), &tree(Some("raced-after"))).is_err());
    let mut reversed = evidence.clone();
    reversed.operations.reverse();
    assert!(validate(&reversed, &tree(Some("before")), &tree(Some("after"))).is_err());
    let mut gap = evidence;
    gap.operations[1].changes[0].before = Some(hash("unobserved"));
    assert!(validate(&gap, &tree(Some("before")), &tree(Some("after"))).is_err());
}

#[test]
fn bound_paths_cannot_overlap_unresolved_claims_in_either_order() {
    let store = InMemoryStore::new();
    let bound = operation("file", None, Some("after"), "known");
    let mut unresolved = operation("file", None, Some("unknown"), "unknown");
    unresolved.resolution = AttributionOperationResolution::Unresolved;
    for operations in [
        vec![bound.clone(), unresolved.clone()],
        vec![unresolved.clone(), bound.clone()],
    ] {
        assert!(
            validate_attribution_transitions(
                &store,
                &evidence(operations),
                None,
                &tree(Some("after")),
                &HashMap::new()
            )
            .is_err()
        );
    }
    unresolved.changes[0].path = "another".into();
    validate_attribution_transitions(
        &store,
        &evidence(vec![bound, unresolved]),
        None,
        &tree(Some("after")),
        &HashMap::new(),
    )
    .expect("unresolved different path never borrows bound status");
}

#[test]
fn exact_creation_deletion_and_recreation_preserve_absent_boundaries() {
    let store = InMemoryStore::new();
    validate_attribution_transitions(
        &store,
        &evidence(vec![operation("file", None, Some("new"), "creator")]),
        None,
        &tree(Some("new")),
        &HashMap::new(),
    )
    .expect("creation");
    validate_attribution_transitions(
        &store,
        &evidence(vec![operation("file", Some("old"), None, "deleter")]),
        Some(&tree(Some("old"))),
        &tree(None),
        &HashMap::new(),
    )
    .expect("deletion");
    validate_attribution_transitions(
        &store,
        &evidence(vec![
            operation("file", Some("old"), None, "deleter"),
            operation("file", None, Some("new"), "creator"),
        ]),
        Some(&tree(Some("old"))),
        &tree(Some("new")),
        &HashMap::new(),
    )
    .expect("recreation chain");
}

#[test]
fn pending_nested_trees_are_resolved_and_missing_or_mismatched_trees_fail() {
    let store = InMemoryStore::new();
    let leaf = tree(Some("after"));
    let middle = Tree::from_entries(vec![
        TreeEntry::directory("nested", leaf.hash()).expect("nested"),
    ]);
    let root = Tree::from_entries(vec![
        TreeEntry::directory("src", middle.hash()).expect("src"),
    ]);
    assert!(attribution_path_blob(&store, &root, &HashMap::new(), "src/nested/file").is_err());
    let pending = HashMap::from([(middle.hash(), &middle), (leaf.hash(), &leaf)]);
    assert_eq!(
        attribution_path_blob(&store, &root, &pending, "src/nested/file").expect("pending"),
        Some(hash("after"))
    );
    assert_eq!(
        attribution_path_blob(&store, &root, &pending, "src/nested/missing").expect("missing"),
        None
    );
    validate_attribution_transitions(
        &store,
        &evidence(vec![operation(
            "src/nested/file",
            None,
            Some("after"),
            "known",
        )]),
        None,
        &root,
        &pending,
    )
    .expect("prepared nested source not yet stored");
    let wrong = tree(Some("wrong"));
    let broken = HashMap::from([(middle.hash(), &middle), (leaf.hash(), &wrong)]);
    assert!(attribution_path_blob(&store, &root, &broken, "src/nested/file").is_err());
    store.put_tree(&leaf).expect("leaf");
    store.put_tree(&middle).expect("middle");
    assert_eq!(
        attribution_path_blob(&store, &root, &HashMap::new(), "src/nested/file").expect("stored"),
        Some(hash("after"))
    );
}

#[test]
fn links_directories_and_non_directory_intermediates_are_never_absent_files() {
    let store = InMemoryStore::new();
    let root = Tree::from_entries(vec![
        TreeEntry::symlink("link", hash("target")).expect("link"),
        TreeEntry::gitlink(
            "gitlink",
            sley::ObjectId::from_hex(
                sley::ObjectFormat::Sha1,
                "1234567890abcdef1234567890abcdef12345678",
            )
            .expect("Git ID"),
        )
        .expect("gitlink"),
        TreeEntry::file("file", hash("bytes"), false).expect("file"),
        TreeEntry::directory("dir", Tree::new().hash()).expect("dir"),
    ]);
    for path in [
        "link",
        "link/child",
        "gitlink",
        "gitlink/child",
        "file/child",
        "dir",
    ] {
        assert!(
            attribution_path_blob(&store, &root, &HashMap::new(), path).is_err(),
            "{path} must not mean absent"
        );
    }
    assert!(
        validate_attribution_transitions(
            &store,
            &evidence(vec![operation("link", Some("target"), None, "known")]),
            Some(&root),
            &Tree::new(),
            &HashMap::new()
        )
        .is_err()
    );
    assert!(
        validate_attribution_transitions(
            &store,
            &evidence(vec![operation("link", None, Some("target"), "known")]),
            None,
            &root,
            &HashMap::new()
        )
        .is_err()
    );
    for path in [
        "",
        "/file",
        "../file",
        "dir//file",
        "dir/./file",
        "dir\\file",
        "C:/file",
    ] {
        assert!(attribution_path_blob(&store, &root, &HashMap::new(), path).is_err());
    }
}
