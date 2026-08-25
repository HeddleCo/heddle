// SPDX-License-Identifier: Apache-2.0

use std::fs;

use chrono::{TimeZone, Utc};
use crypto::{Ed25519Signer, Signer, state_signature_from_signer};
use objects::{
    object::{
        Attribution, Blob, KeyBinding, KeyRole, Principal, State, StateAttachment,
        StateAttachmentBody, StateSignature, Tree, TreeEntry,
    },
    store::ObjectStore,
};
use repo::Repository;
use tempfile::TempDir;

use super::{
    StateProvenanceVerification,
    registry_tests::{anchor, checkpoint, store_registry},
    verify_repository_provenance,
};

pub(super) fn setup() -> (TempDir, Repository) {
    let temp = TempDir::new().expect("temp dir");
    let repo = Repository::init_default(temp.path()).expect("init repo");
    (temp, repo)
}

fn alice() -> Attribution {
    Attribution::human(Principal::new("Alice", "alice@example.com"))
}

pub(super) fn state_with_blob(repo: &Repository, blob_hash: objects::object::ContentHash) -> State {
    let tree = Tree::from_entries(vec![
        TreeEntry::file("proof.txt", blob_hash, false).expect("tree entry"),
    ]);
    let tree_hash = repo.store().put_tree(&tree).expect("put tree");
    State::new(tree_hash, Vec::new(), alice()).with_timestamp(Utc.timestamp_opt(2_000, 0).unwrap())
}

pub(super) fn attach_signature(
    repo: &Repository,
    state: &State,
    signature: StateSignature,
    attribution: Attribution,
) -> objects::object::StateAttachmentId {
    repo.store()
        .put_state_attachment(&StateAttachment {
            state_id: state.state_id,
            body: StateAttachmentBody::Signature(signature),
            attribution,
            created_at: state.created_at,
            supersedes: None,
        })
        .expect("put signature attachment")
}

pub(super) fn binding(
    signer: &Ed25519Signer,
    identity: &str,
    revoked_at: Option<i64>,
) -> KeyBinding {
    let public_key = hex::encode(signer.public_key());
    let mut binding = KeyBinding {
        algorithm: signer.algorithm().to_string(),
        public_key: public_key.clone(),
        identity_ref: identity.to_string(),
        role: KeyRole::Author,
        added_by_sig: StateSignature {
            algorithm: signer.algorithm().to_string(),
            public_key,
            signature: String::new(),
        },
        valid_from: Utc.timestamp_opt(1_000, 0).unwrap(),
        revoked_at: revoked_at.map(|value| Utc.timestamp_opt(value, 0).unwrap()),
        delegated_from: None,
    };
    binding.added_by_sig.signature = hex::encode(
        signer
            .sign(&binding.canonical_signing_payload())
            .expect("sign binding"),
    );
    binding
}

pub(super) fn put_registry(
    repo: &Repository,
    authority: &Ed25519Signer,
    bindings: Vec<KeyBinding>,
) {
    let registry = checkpoint(authority, 0, None, bindings);
    store_registry(repo, &registry);
    anchor(repo, &registry, authority);
}

pub(super) fn result_for(repo: &Repository, state: &State) -> StateProvenanceVerification {
    verify_repository_provenance(repo)
        .expect("verify provenance")
        .states
        .into_iter()
        .find(|result| result.state_id == state.state_id.to_string_full())
        .expect("state result")
}

#[test]
fn flipped_tree_byte_fails_content_link() {
    let (_temp, repo) = setup();
    let expected_blob = Blob::from("hello");
    let expected_hash = expected_blob.hash();
    let state = state_with_blob(&repo, expected_hash);
    let signer = Ed25519Signer::generate().unwrap();
    repo.store().put_state(&state).unwrap();
    attach_signature(
        &repo,
        &state,
        state_signature_from_signer(&state.compute_hash(), &signer).unwrap(),
        state.attribution.clone(),
    );
    put_registry(
        &repo,
        &signer,
        vec![binding(&signer, "identity:alice", None)],
    );
    repo.store().put_blob(&expected_blob).unwrap();
    let tree_hex = state.tree.to_hex();
    let path = repo
        .heddle_dir()
        .join("objects/trees")
        .join(&tree_hex[..2])
        .join(&tree_hex[2..]);
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.last_mut().expect("non-empty tree file");
    *last ^= 1;
    fs::write(path, bytes).unwrap();
    repo.store().clear_recent_caches();

    let result = result_for(&repo, &state);
    assert_eq!(result.display_status(), "FAILED(content)");
    assert!(result.detail.contains("content binding"));
}

#[test]
fn swapped_attribution_fails_identity_link() {
    let (_temp, repo) = setup();
    let blob_hash = repo.store().put_blob(&Blob::from("hello")).unwrap();
    let original = state_with_blob(&repo, blob_hash);
    let signer = Ed25519Signer::generate().unwrap();
    let signature = state_signature_from_signer(&original.compute_hash(), &signer).unwrap();
    repo.store().put_state(&original).unwrap();
    attach_signature(
        &repo,
        &original,
        signature,
        Attribution::human(Principal::new("Mallory", "mallory@example.com")),
    );
    put_registry(
        &repo,
        &signer,
        vec![binding(&signer, "identity:alice", None)],
    );

    let result = result_for(&repo, &original);
    assert_eq!(result.display_status(), "FAILED(identity)");
    assert!(result.detail.contains("attribution"));
}

#[test]
fn unregistered_key_fails_identity_as_unknown_key() {
    let (_temp, repo) = setup();
    let blob_hash = repo.store().put_blob(&Blob::from("hello")).unwrap();
    let state = state_with_blob(&repo, blob_hash);
    let signer = Ed25519Signer::generate().unwrap();
    let other = Ed25519Signer::generate().unwrap();
    repo.store().put_state(&state).unwrap();
    attach_signature(
        &repo,
        &state,
        state_signature_from_signer(&state.compute_hash(), &signer).unwrap(),
        state.attribution.clone(),
    );
    put_registry(&repo, &other, vec![binding(&other, "identity:other", None)]);

    let result = result_for(&repo, &state);
    assert_eq!(result.display_status(), "FAILED(identity)");
    assert!(result.detail.contains("UnknownKey"));
}

#[test]
fn revoked_key_fails_identity_as_revoked() {
    let (_temp, repo) = setup();
    let blob_hash = repo.store().put_blob(&Blob::from("hello")).unwrap();
    let state = state_with_blob(&repo, blob_hash);
    let signer = Ed25519Signer::generate().unwrap();
    repo.store().put_state(&state).unwrap();
    attach_signature(
        &repo,
        &state,
        state_signature_from_signer(&state.compute_hash(), &signer).unwrap(),
        state.attribution.clone(),
    );
    put_registry(
        &repo,
        &signer,
        vec![binding(&signer, "identity:alice", Some(1_999))],
    );

    let result = result_for(&repo, &state);
    assert_eq!(result.display_status(), "FAILED(identity)");
    assert!(result.detail.contains("Revoked"));
}

#[test]
fn backdated_state_from_currently_revoked_key_fails_identity() {
    let (_temp, repo) = setup();
    let blob_hash = repo.store().put_blob(&Blob::from("hello")).unwrap();
    let state = state_with_blob(&repo, blob_hash);
    let signer = Ed25519Signer::generate().unwrap();
    repo.store().put_state(&state).unwrap();
    attach_signature(
        &repo,
        &state,
        state_signature_from_signer(&state.compute_hash(), &signer).unwrap(),
        state.attribution.clone(),
    );
    put_registry(
        &repo,
        &signer,
        vec![binding(&signer, "identity:alice", Some(2_001))],
    );

    let result = result_for(&repo, &state);
    assert_eq!(result.display_status(), "FAILED(identity)");
    assert!(result.detail.contains("Revoked"), "{result:?}");
}

#[test]
fn reviewer_role_cannot_author_a_state() {
    let (_temp, repo) = setup();
    let blob_hash = repo.store().put_blob(&Blob::from("hello")).unwrap();
    let state = state_with_blob(&repo, blob_hash);
    let signer = Ed25519Signer::generate().unwrap();
    repo.store().put_state(&state).unwrap();
    attach_signature(
        &repo,
        &state,
        state_signature_from_signer(&state.compute_hash(), &signer).unwrap(),
        state.attribution.clone(),
    );
    let mut reviewer = binding(&signer, "identity:alice", None);
    reviewer.role = KeyRole::Reviewer;
    reviewer.added_by_sig.signature = hex::encode(
        signer
            .sign(&reviewer.canonical_signing_payload())
            .expect("sign reviewer binding"),
    );
    put_registry(&repo, &signer, vec![reviewer]);

    let result = result_for(&repo, &state);
    assert_eq!(result.display_status(), "FAILED(identity)");
    assert!(result.detail.contains("UnauthorizedRole"), "{result:?}");
}

#[test]
fn stripped_authorship_evidence_does_not_downgrade_to_integrity_only() {
    let (_temp, repo) = setup();
    let blob_hash = repo.store().put_blob(&Blob::from("hello")).unwrap();
    let state = state_with_blob(&repo, blob_hash);
    let signer = Ed25519Signer::generate().unwrap();
    repo.store().put_state(&state).unwrap();
    let attachment_id = attach_signature(
        &repo,
        &state,
        state_signature_from_signer(&state.compute_hash(), &signer).unwrap(),
        state.attribution.clone(),
    );
    put_registry(
        &repo,
        &signer,
        vec![binding(&signer, "identity:alice", None)],
    );
    assert_eq!(
        result_for(&repo, &state).display_status(),
        "Verified(identity:alice)"
    );

    let attachment_path = repo
        .heddle_dir()
        .join("objects/state-attachments")
        .join(state.state_id.to_string_full())
        .join(format!("{}.attachment", attachment_id.as_hash().to_hex()));
    fs::remove_file(attachment_path).expect("attacker strips signature attachment");
    let index_path = repo
        .heddle_dir()
        .join("objects/state-attachment-index")
        .join(format!("{}.msgpack", state.state_id.to_string_full()));
    fs::remove_file(index_path).expect("attacker strips signature index");
    repo.store().clear_recent_caches();

    let report = verify_repository_provenance(&repo).unwrap();
    let result = report
        .states
        .iter()
        .find(|result| result.state_id == state.state_id.to_string_full())
        .expect("attacked state result");
    assert_eq!(result.display_status(), "FAILED(identity)");
    assert!(!report.clean);
}

#[test]
fn missing_parent_fails_provenance_chain() {
    let (_temp, repo) = setup();
    let blob_hash = repo.store().put_blob(&Blob::from("hello")).unwrap();
    let parent = state_with_blob(&repo, blob_hash);
    let child = State::new(parent.tree, vec![parent.state_id], alice())
        .with_timestamp(Utc.timestamp_opt(2_001, 0).unwrap());
    let grandchild = State::new(child.tree, vec![child.state_id], alice())
        .with_timestamp(Utc.timestamp_opt(2_002, 0).unwrap());
    let signer = Ed25519Signer::generate().unwrap();
    for state in [&parent, &child, &grandchild] {
        repo.store().put_state(state).unwrap();
        attach_signature(
            &repo,
            state,
            state_signature_from_signer(&state.compute_hash(), &signer).unwrap(),
            state.attribution.clone(),
        );
    }
    put_registry(
        &repo,
        &signer,
        vec![binding(&signer, "identity:alice", None)],
    );

    let parent_path = repo
        .heddle_dir()
        .join("objects/states")
        .join(format!("{}.state", parent.state_id.to_string_full()));
    fs::remove_file(parent_path).expect("attacker removes parent state");
    repo.store().clear_recent_caches();

    let result = result_for(&repo, &grandchild);
    assert_eq!(result.display_status(), "FAILED(chain)");
    assert!(result.detail.contains("ancestor"), "{result:?}");
}

#[test]
fn untagged_signature_is_legacy_and_not_clean() {
    let (_temp, repo) = setup();
    let blob_hash = repo.store().put_blob(&Blob::from("hello")).unwrap();
    let state = state_with_blob(&repo, blob_hash);
    let signer = Ed25519Signer::generate().unwrap();
    repo.store().put_state(&state).unwrap();
    let legacy = StateSignature {
        algorithm: signer.algorithm().to_string(),
        public_key: hex::encode(signer.public_key()),
        signature: hex::encode(signer.sign(state.compute_hash().as_bytes()).unwrap()),
    };
    attach_signature(&repo, &state, legacy, state.attribution.clone());

    let report = verify_repository_provenance(&repo).unwrap();
    let result = report
        .states
        .iter()
        .find(|result| result.state_id == state.state_id.to_string_full())
        .unwrap();
    assert_eq!(result.display_status(), "Legacy");
    assert!(!report.clean);
}
