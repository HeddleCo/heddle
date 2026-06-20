// SPDX-License-Identifier: Apache-2.0
//! Backend-agnostic compliance test suite for [`ObjectStore`] implementations.
//!
//! Call [`run_compliance_tests`] from any `#[test]` or `#[tokio::test]` that
//! has a concrete [`ObjectStore`] to verify it satisfies the full contract.
//! `InMemoryStore` is validated this way.

use crate::{
    object::{Attribution, Blob, ContentHash, Principal, State, Tree},
    store::ObjectStore,
};

fn attribution() -> Attribution {
    Attribution::human(Principal::new("Compliance Test", "test@example.com"))
}

/// Run the full ObjectStore compliance suite against `store`.
///
/// Panics on the first assertion failure. Designed to be called from unit or
/// integration tests.
pub fn run_compliance_tests<S: ObjectStore>(store: &S) {
    blob_round_trip(store);
    blob_missing_returns_none(store);
    blob_has(store);
    blob_list(store);
    tree_round_trip(store);
    tree_missing_returns_none(store);
    state_round_trip(store);
    state_has(store);
    state_list(store);
}

// ── Blob ─────────────────────────────────────────────────────────────────────

fn blob_round_trip<S: ObjectStore>(store: &S) {
    let blob = Blob::from("compliance: blob round-trip");
    let hash = store.put_blob(&blob).expect("put_blob failed");
    let got = store
        .get_blob(&hash)
        .expect("get_blob failed")
        .expect("blob missing after put");
    assert_eq!(
        got.content(),
        blob.content(),
        "blob content changed after round-trip"
    );
}

fn blob_missing_returns_none<S: ObjectStore>(store: &S) {
    let hash = ContentHash::compute(b"compliance-nonexistent-blob");
    let result = store
        .get_blob(&hash)
        .expect("get_blob error on missing key");
    assert!(
        result.is_none(),
        "get_blob should return None for unknown hash"
    );
}

fn blob_has<S: ObjectStore>(store: &S) {
    let blob = Blob::from("compliance: has_blob");
    let hash = store.put_blob(&blob).expect("put_blob failed");
    assert!(
        store.has_blob(&hash).expect("has_blob failed"),
        "has_blob returned false immediately after put"
    );
}

fn blob_list<S: ObjectStore>(store: &S) {
    let blob = Blob::from("compliance: list_blobs");
    let hash = store.put_blob(&blob).expect("put_blob failed");
    let list = store.list_blobs().expect("list_blobs failed");
    assert!(
        list.contains(&hash),
        "list_blobs does not contain hash after put"
    );
}

// ── Tree ──────────────────────────────────────────────────────────────────────

fn tree_round_trip<S: ObjectStore>(store: &S) {
    let tree = Tree::new();
    let hash = store.put_tree(&tree).expect("put_tree failed");
    let got = store
        .get_tree(&hash)
        .expect("get_tree failed")
        .expect("tree missing after put");
    assert_eq!(got.hash(), hash, "tree hash changed after round-trip");
}

fn tree_missing_returns_none<S: ObjectStore>(store: &S) {
    let hash = ContentHash::compute(b"compliance-nonexistent-tree");
    let result = store
        .get_tree(&hash)
        .expect("get_tree error on missing key");
    assert!(
        result.is_none(),
        "get_tree should return None for unknown hash"
    );
}

// ── State ─────────────────────────────────────────────────────────────────────

fn state_round_trip<S: ObjectStore>(store: &S) {
    let tree = Tree::new();
    let tree_hash = store
        .put_tree(&tree)
        .expect("put_tree in state test failed");
    let state = State::new(tree_hash, vec![], attribution());
    let id = state.change_id;

    store.put_state(&state).expect("put_state failed");

    let got = store
        .get_state(&id)
        .expect("get_state failed")
        .expect("state missing after put");

    assert_eq!(got.change_id, id, "change_id changed after round-trip");
    assert_eq!(got.tree, tree_hash, "tree hash changed after round-trip");
}

fn state_has<S: ObjectStore>(store: &S) {
    let tree = Tree::new();
    let tree_hash = store
        .put_tree(&tree)
        .expect("put_tree in state test failed");
    let state = State::new(tree_hash, vec![], attribution());
    let id = state.change_id;
    store.put_state(&state).expect("put_state failed");
    assert!(
        store.has_state(&id).expect("has_state failed"),
        "has_state returned false immediately after put"
    );
}

fn state_list<S: ObjectStore>(store: &S) {
    let tree = Tree::new();
    let tree_hash = store
        .put_tree(&tree)
        .expect("put_tree in state test failed");
    let state = State::new(tree_hash, vec![], attribution());
    let id = state.change_id;
    store.put_state(&state).expect("put_state failed");
    let ids = store.list_states().expect("list_states failed");
    assert!(
        ids.contains(&id),
        "list_states does not contain id after put"
    );
}
