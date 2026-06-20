// SPDX-License-Identifier: Apache-2.0
//! Backend-agnostic compliance test suite for [`LocalObjectStore`] implementations.
//!
//! Call [`run_compliance_tests`] from any `#[test]` or `#[tokio::test]` that
//! has a concrete [`LocalObjectStore`] to verify it satisfies the full contract.

use crate::{
    error::StorageErrorKind,
    object::{Attribution, Blob, ContentHash, Principal, State, Tree},
    store::{LocalObjectStore, PageRequest, PageToken},
};

fn attribution() -> Attribution {
    Attribution::human(Principal::new("Compliance Test", "test@example.com"))
}

/// Run the full LocalObjectStore compliance suite against `store`.
///
/// Panics on the first assertion failure. Designed to be called from unit or
/// integration tests.
pub fn run_compliance_tests<S: LocalObjectStore>(store: &S) {
    blob_round_trip(store);
    blob_missing_returns_none(store);
    blob_has(store);
    blob_list(store);
    blob_list_paginates(store);
    storage_error_kinds_are_machine_readable(store);
    tree_round_trip(store);
    tree_missing_returns_none(store);
    state_round_trip(store);
    state_has(store);
    state_list(store);
}

// ── Blob ─────────────────────────────────────────────────────────────────────

fn blob_round_trip<S: LocalObjectStore>(store: &S) {
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

fn blob_missing_returns_none<S: LocalObjectStore>(store: &S) {
    let hash = ContentHash::compute(b"compliance-nonexistent-blob");
    let result = store
        .get_blob(&hash)
        .expect("get_blob error on missing key");
    assert!(
        result.is_none(),
        "get_blob should return None for unknown hash"
    );
}

fn blob_has<S: LocalObjectStore>(store: &S) {
    let blob = Blob::from("compliance: has_blob");
    let hash = store.put_blob(&blob).expect("put_blob failed");
    assert!(
        store.has_blob(&hash).expect("has_blob failed"),
        "has_blob returned false immediately after put"
    );
}

fn blob_list<S: LocalObjectStore>(store: &S) {
    let blob = Blob::from("compliance: list_blobs");
    let hash = store.put_blob(&blob).expect("put_blob failed");
    let list = store.list_blobs().expect("list_blobs failed");
    assert!(
        list.contains(&hash),
        "list_blobs does not contain hash after put"
    );
}

fn blob_list_paginates<S: LocalObjectStore>(store: &S) {
    let expected: Vec<_> = (0..3)
        .map(|i| {
            let blob = Blob::from(format!("compliance: paginated blob {i}"));
            store.put_blob(&blob).expect("put_blob failed")
        })
        .collect();

    let mut seen = Vec::new();
    let mut token = None;
    for _ in 0..64 {
        let page = store
            .list_blobs_page(PageRequest {
                limit: Some(2),
                token,
            })
            .expect("list_blobs_page failed");
        assert!(
            page.items.len() <= 2,
            "list_blobs_page returned more items than requested"
        );
        seen.extend(page.items);
        token = page.next_token;
        if token.is_none() {
            break;
        }
    }
    assert!(token.is_none(), "paginated listing did not terminate");

    for hash in expected {
        assert!(
            seen.contains(&hash),
            "paginated list_blobs did not include hash after put"
        );
    }

    let err = store
        .list_blobs_page(PageRequest {
            limit: Some(1),
            token: Some(PageToken::new("not-a-local-offset")),
        })
        .expect_err("invalid local page token must fail");
    assert_eq!(err.storage_kind(), StorageErrorKind::Invalid);
}

fn storage_error_kinds_are_machine_readable<S: LocalObjectStore>(store: &S) {
    let blob = Blob::from("compliance: cas mismatch");
    let wrong_hash = Blob::from("compliance: wrong hash").hash();
    let err = store
        .put_blob_with_hash(&blob, wrong_hash)
        .expect_err("hash mismatch must fail");
    assert_eq!(err.storage_kind(), StorageErrorKind::CasMismatch);
    assert!(!err.is_retryable_storage_error());
}

// ── Tree ──────────────────────────────────────────────────────────────────────

fn tree_round_trip<S: LocalObjectStore>(store: &S) {
    let tree = Tree::new();
    let hash = store.put_tree(&tree).expect("put_tree failed");
    let got = store
        .get_tree(&hash)
        .expect("get_tree failed")
        .expect("tree missing after put");
    assert_eq!(got.hash(), hash, "tree hash changed after round-trip");
}

fn tree_missing_returns_none<S: LocalObjectStore>(store: &S) {
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

fn state_round_trip<S: LocalObjectStore>(store: &S) {
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

fn state_has<S: LocalObjectStore>(store: &S) {
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

fn state_list<S: LocalObjectStore>(store: &S) {
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
