// SPDX-License-Identifier: Apache-2.0
use crypto::Ed25519Signer;
use objects::{
    object::{
        Attribution, Blob, FileProvenance, LineSpan, Origin, OriginSet, Principal, State,
        StateAttachment, StateAttachmentBody, Tree, TreeEntry,
    },
    store::ObjectStore,
};
use repo::Repository;
use sley::ObjectId as GitObjectId;
use tempfile::TempDir;

use super::{objects::check_tree_objects, state::check_states};

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

fn sample_origin(state_id: objects::object::StateId) -> Origin {
    Origin {
        state_id,
        attribution: sample_attribution(),
        created_at: chrono::Utc::now(),
        authored_at: None,
    }
}

#[test]
fn test_check_states_thorough_rejects_invalid_signature() {
    let (_temp, repo) = setup_repo();
    let tree_hash = put_empty_tree(&repo).expect("put tree");
    let state = State::new(tree_hash, vec![], sample_attribution());
    let signer = Ed25519Signer::from_seed(&[7u8; 32]).expect("create signer");

    repo.store().put_state(&state).expect("put state");
    repo.sign_state(&state.state_id, &signer)
        .expect("sign state");
    let prior = repo
        .latest_state_attachment(&state.state_id, repo::StateAttachmentKind::Signature)
        .expect("read signature attachment")
        .expect("signature attachment");
    let prior_id = prior.id();
    let StateAttachmentBody::Signature(mut signature) = prior.body else {
        panic!("expected signature attachment")
    };
    signature.signature = "00".repeat(64);
    repo.put_state_attachment(&StateAttachment {
        state_id: state.state_id,
        body: StateAttachmentBody::Signature(signature),
        attribution: sample_attribution(),
        created_at: chrono::Utc::now() + chrono::Duration::seconds(1),
        supersedes: Some(prior_id),
    })
    .expect("put invalid signature attachment");

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
        vec![sample_origin(objects::object::StateId::from_bytes([7; 32]))],
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

    check_tree_objects(&repo, &mut errors, &mut warnings, &mut objects_checked)
        .expect("check tree objects");

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
fn test_fsck_does_not_require_gitlink_target_object() {
    let (_temp, repo) = setup_repo();
    let target: GitObjectId = "0303030303030303030303030303030303030303"
        .parse()
        .expect("git oid");
    let tree = Tree::from_entries(vec![
        TreeEntry::gitlink("vendor", target).expect("gitlink entry"),
    ]);
    let tree_hash = repo.store().put_tree(&tree).expect("put tree");
    let state = State::new(tree_hash, vec![], sample_attribution());
    repo.store().put_state(&state).expect("put state");

    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut objects_checked = 0;

    check_tree_objects(&repo, &mut errors, &mut warnings, &mut objects_checked)
        .expect("check tree objects");

    assert!(
        errors.is_empty(),
        "gitlink target lives outside the Heddle object store: {errors:?}"
    );
    assert!(warnings.is_empty(), "warnings={warnings:?}");
}

/// Fixture exercising every tree/blob fsck finding class. Expected values were
/// captured from the pre-refactor `check_trees` + `check_blobs` pair.
#[test]
fn test_tree_blob_checks_characterization() {
    use objects::object::ContentHash;

    let (_temp, repo) = setup_repo();

    // Missing blob (not partial-fetch): referenced but never stored.
    let missing_hash = ContentHash::compute(b"ghost-blob");

    // Partial-fetch missing blob: referenced and explicitly marked absent.
    let partial_blob = Blob::from("partial-fetch\n");
    let partial_hash = partial_blob.hash();

    // Shared subtree reused by two states.
    let shared_blob = Blob::from("shared-leaf\n");
    let shared_blob_hash = repo
        .store()
        .put_blob(&shared_blob)
        .expect("put shared blob");
    let shared_tree = Tree::from_entries(vec![
        objects::object::TreeEntry::file("shared.txt", shared_blob_hash, false).unwrap(),
    ]);
    let shared_tree_hash = repo
        .store()
        .put_tree(&shared_tree)
        .expect("put shared tree");

    // Dangling subtree ref (missing child tree — fsck stays silent).
    let dangling_tree_hash = ContentHash::compute(b"missing-subtree");
    let dangling_parent = Tree::from_entries(vec![
        objects::object::TreeEntry::directory("missing", dangling_tree_hash).unwrap(),
        objects::object::TreeEntry::file("absent.txt", missing_hash, false).unwrap(),
        objects::object::TreeEntry::file("partial.txt", partial_hash, false).unwrap(),
    ]);
    let dangling_parent_hash = repo
        .store()
        .put_tree(&dangling_parent)
        .expect("put dangling parent");

    let state_shared_a = State::new(shared_tree_hash, vec![], sample_attribution());
    let state_shared_b = State::new(shared_tree_hash, vec![], sample_attribution());
    let state_dangling = State::new(dangling_parent_hash, vec![], sample_attribution());
    repo.store()
        .put_state(&state_shared_a)
        .expect("put state a");
    repo.store()
        .put_state(&state_shared_b)
        .expect("put state b");
    repo.store()
        .put_state(&state_dangling)
        .expect("put state dangling");
    repo.record_missing_blob(partial_hash)
        .expect("record partial-fetch blob");

    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut objects_checked = 0;

    check_tree_objects(&repo, &mut errors, &mut warnings, &mut objects_checked)
        .expect("check tree objects");

    assert_eq!(
        objects_checked, 6,
        "three unique trees plus three unique blob checks"
    );

    let error_kinds: Vec<_> = errors.iter().map(|error| error.kind.as_str()).collect();
    assert_eq!(
        error_kinds,
        vec!["missing_blob", "missing_blob"],
        "tree-phase then blob-phase ordering must be preserved"
    );

    assert_eq!(
        errors[0].message,
        "Tree entry 'absent.txt' references missing blob"
    );
    assert_eq!(errors[1].message, "Tree references missing blob");
    assert_eq!(errors[0].object, errors[1].object, "same missing blob hash");

    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].contains("partial.txt")
            && warnings[0].contains("explicitly absent under partial fetch"),
        "partial-fetch warning: {}",
        warnings[0]
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
