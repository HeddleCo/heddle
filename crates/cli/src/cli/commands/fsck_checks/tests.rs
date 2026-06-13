// SPDX-License-Identifier: Apache-2.0
use crypto::{Ed25519Signer, StateSigningExt};
use objects::object::{
    Attribution, Blob, FileProvenance, LineSpan, Origin, OriginSet, Principal, State, Tree,
};
use objects::store::ObjectStore;
use repo::Repository;
use tempfile::TempDir;

use super::{
    objects::{check_blobs, check_trees},
    state::check_states,
};

fn setup_repo() -> (TempDir, Repository) {
    let temp = TempDir::new().expect("create temp dir");
    let repo = Repository::init_default(temp.path()).expect("init repo");
    (temp, repo)
}

fn sample_attribution() -> Attribution {
    Attribution::human(Principal::new("Test User", "test@example.com"))
}

fn put_empty_tree(repo: &Repository) -> objects::error::Result<objects::object::ContentHash> {
    repo.store().put_tree(&Tree::new())
}

fn sample_origin(state_id: objects::object::ChangeId) -> Origin {
    Origin {
        state_id,
        attribution: sample_attribution(),
        created_at: chrono::Utc::now(),
        authored_at: None,
    }
}

#[test]
fn test_check_states_thorough_rejects_parent_cycles() {
    let (_temp, repo) = setup_repo();
    let tree_hash = put_empty_tree(&repo).expect("put tree");
    let state_a_id = objects::object::ChangeId::generate();
    let state_b_id = objects::object::ChangeId::generate();

    let state_a =
        State::new(tree_hash, vec![state_b_id], sample_attribution()).with_change_id(state_a_id);
    let state_b =
        State::new(tree_hash, vec![state_a_id], sample_attribution()).with_change_id(state_b_id);

    repo.store().put_state(&state_a).expect("put state a");
    repo.store().put_state(&state_b).expect("put state b");

    let mut errors = Vec::new();
    let mut objects_checked = 0;
    check_states(&repo, &mut errors, &mut objects_checked, true).expect("check states");

    assert!(
        errors.iter().any(|error| error.kind == "state_cycle"),
        "expected cycle error, got {:?}",
        errors.iter().map(|error| &error.kind).collect::<Vec<_>>()
    );
}

#[test]
fn test_check_states_thorough_rejects_invalid_signature() {
    let (_temp, repo) = setup_repo();
    let tree_hash = put_empty_tree(&repo).expect("put tree");
    let mut state = State::new(tree_hash, vec![], sample_attribution());
    let signer = Ed25519Signer::from_seed(&[7u8; 32]).expect("create signer");

    state.sign(&signer).expect("sign state");
    state.signature.as_mut().expect("signature").signature = "00".repeat(64);
    repo.store().put_state(&state).expect("put state");

    let mut errors = Vec::new();
    let mut objects_checked = 0;
    check_states(&repo, &mut errors, &mut objects_checked, true).expect("check states");

    assert!(
        errors.iter().any(|error| error.kind == "invalid_signature"),
        "expected invalid signature error, got {:?}",
        errors.iter().map(|error| &error.kind).collect::<Vec<_>>()
    );
}

#[test]
fn test_check_states_thorough_rejects_invalid_provenance() {
    let (_temp, repo) = setup_repo();
    let blob = Blob::from("hello\nworld\n");
    let blob_hash = repo.store().put_blob(&blob).expect("put blob");
    let tree = Tree::from_entries(vec![
        objects::object::TreeEntry::file("file.txt", blob_hash, false).unwrap(),
    ]);
    let tree_hash = repo.store().put_tree(&tree).expect("put tree");

    let bad_provenance = FileProvenance::new(
        objects::object::ContentHash::compute(b"wrong"),
        1,
        vec![LineSpan {
            start_line: 0,
            line_len: 1,
            origin_set_index: 0,
        }],
        vec![sample_origin(objects::object::ChangeId::generate())],
        vec![OriginSet {
            origin_indexes: vec![0],
        }],
    );
    let provenance_blob = Blob::new(rmp_serde::to_vec(&bad_provenance).unwrap());
    let provenance_blob_hash = repo
        .store()
        .put_blob(&provenance_blob)
        .expect("put provenance blob");
    let provenance_tree = Tree::from_entries(vec![
        objects::object::TreeEntry::file("file.txt", provenance_blob_hash, false).unwrap(),
    ]);
    let provenance_root = repo
        .store()
        .put_tree(&provenance_tree)
        .expect("put provenance tree");

    let state =
        State::new(tree_hash, vec![], sample_attribution()).with_provenance(provenance_root);
    repo.store().put_state(&state).expect("put state");

    let mut errors = Vec::new();
    let mut objects_checked = 0;
    check_states(&repo, &mut errors, &mut objects_checked, true).expect("check states");

    assert!(
        errors
            .iter()
            .any(|error| error.kind == "invalid_provenance"),
        "expected invalid provenance error, got {:?}",
        errors.iter().map(|error| &error.kind).collect::<Vec<_>>()
    );
}

#[test]
fn test_fsck_treats_explicitly_missing_partial_fetch_blob_as_warning() {
    let (_temp, repo) = setup_repo();
    let blob = Blob::from("partial fetch blob\n");
    let blob_hash = blob.hash();
    let tree = Tree::from_entries(vec![
        objects::object::TreeEntry::file("README.md", blob_hash, false).unwrap(),
    ]);
    let tree_hash = repo.store().put_tree(&tree).expect("put tree");
    let state = State::new(tree_hash, vec![], sample_attribution());
    repo.store().put_state(&state).expect("put state");
    repo.record_missing_blob(blob_hash)
        .expect("record missing blob");

    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut objects_checked = 0;

    check_trees(&repo, &mut errors, &mut warnings, &mut objects_checked).expect("check trees");
    check_blobs(&repo, &mut errors, &mut warnings, &mut objects_checked).expect("check blobs");

    assert!(
        errors.iter().all(|error| error.kind != "missing_blob"),
        "explicitly missing partial-fetch blob should not be treated as corruption: {:?}",
        errors.iter().map(|error| &error.kind).collect::<Vec<_>>()
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("explicitly absent under partial fetch")),
        "expected partial-fetch warning, got {warnings:?}"
    );
}

#[test]
fn test_require_blob_clears_stale_partial_fetch_marker_when_blob_exists() {
    let (_temp, repo) = setup_repo();
    let blob = Blob::from("present after refetch\n");
    let blob_hash = repo.store().put_blob(&blob).expect("put blob");
    repo.record_missing_blob(blob_hash)
        .expect("record missing blob");

    let loaded = repo.require_blob(&blob_hash).expect("require blob");

    assert_eq!(loaded.content(), blob.content());
    assert!(!repo.is_missing_blob(&blob_hash).expect("check missing"));
    assert!(repo.missing_blobs().expect("list missing").is_empty());
}
