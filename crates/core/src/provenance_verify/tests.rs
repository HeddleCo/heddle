// SPDX-License-Identifier: Apache-2.0

use std::fs;

use chrono::{TimeZone, Utc};
use crypto::{Ed25519Signer, Signer, state_signature_from_signer};
use objects::{
    object::{
        Attribution, Blob, KeyBinding, KeyBindingRegistry, Principal, ReviewKind, ReviewScope,
        ReviewSignature, ReviewSignaturesBlob, State, StateAttachment, StateAttachmentBody,
        StateSignature, Tree, TreeEntry, signing_payload,
    },
    store::ObjectStore,
};
use repo::Repository;
use tempfile::TempDir;

use super::{StateProvenanceVerification, verify_repository_provenance};

fn setup() -> (TempDir, Repository) {
    let temp = TempDir::new().expect("temp dir");
    let repo = Repository::init_default(temp.path()).expect("init repo");
    (temp, repo)
}

fn alice() -> Attribution {
    Attribution::human(Principal::new("Alice", "alice@example.com"))
}

fn state_with_blob(repo: &Repository, blob_hash: objects::object::ContentHash) -> State {
    let tree = Tree::from_entries(vec![
        TreeEntry::file("proof.txt", blob_hash, false).expect("tree entry"),
    ]);
    let tree_hash = repo.store().put_tree(&tree).expect("put tree");
    State::new(tree_hash, Vec::new(), alice()).with_timestamp(Utc.timestamp_opt(2_000, 0).unwrap())
}

fn attach_signature(
    repo: &Repository,
    state: &State,
    signature: StateSignature,
    attribution: Attribution,
) {
    repo.store()
        .put_state_attachment(&StateAttachment {
            state_id: state.state_id,
            body: StateAttachmentBody::Signature(signature),
            attribution,
            created_at: state.created_at,
            supersedes: None,
        })
        .expect("put signature attachment");
}

fn binding(signer: &Ed25519Signer, identity: &str, revoked_at: Option<i64>) -> KeyBinding {
    let public_key = hex::encode(signer.public_key());
    let mut binding = KeyBinding {
        algorithm: signer.algorithm().to_string(),
        public_key: public_key.clone(),
        identity_ref: identity.to_string(),
        role: "author".to_string(),
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

fn put_registry(repo: &Repository, bindings: Vec<KeyBinding>) {
    let bytes = KeyBindingRegistry::new(bindings)
        .encode()
        .expect("encode registry");
    repo.store()
        .put_blob(&Blob::new(bytes))
        .expect("put registry blob");
}

fn result_for(repo: &Repository, state: &State) -> StateProvenanceVerification {
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
    put_registry(&repo, vec![binding(&signer, "identity:alice", None)]);
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
    put_registry(&repo, vec![binding(&signer, "identity:alice", None)]);

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
    put_registry(&repo, vec![binding(&other, "identity:other", None)]);

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
    put_registry(&repo, vec![binding(&signer, "identity:alice", Some(1_999))]);

    let result = result_for(&repo, &state);
    assert_eq!(result.display_status(), "FAILED(identity)");
    assert!(result.detail.contains("Revoked"));
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

#[test]
fn verifies_authorship_and_review_chain_by_registry_identity() {
    let (_temp, repo) = setup();
    let blob_hash = repo.store().put_blob(&Blob::from("hello")).unwrap();
    let state = state_with_blob(&repo, blob_hash);
    let author = Ed25519Signer::generate().unwrap();
    let reviewer = Ed25519Signer::generate().unwrap();
    repo.store().put_state(&state).unwrap();
    attach_signature(
        &repo,
        &state,
        state_signature_from_signer(&state.compute_hash(), &author).unwrap(),
        state.attribution.clone(),
    );
    attach_review(&repo, &state, &reviewer, false);
    put_registry(
        &repo,
        vec![
            binding(&author, "identity:alice", None),
            binding(&reviewer, "identity:bob", None),
        ],
    );

    let result = result_for(&repo, &state);
    assert_eq!(result.display_status(), "Verified(identity:alice)");
    assert_eq!(result.reviewer_identities, vec!["identity:bob"]);
}

#[test]
fn tampered_review_signature_fails_review_link() {
    let (_temp, repo) = setup();
    let blob_hash = repo.store().put_blob(&Blob::from("hello")).unwrap();
    let state = state_with_blob(&repo, blob_hash);
    let author = Ed25519Signer::generate().unwrap();
    let reviewer = Ed25519Signer::generate().unwrap();
    repo.store().put_state(&state).unwrap();
    attach_signature(
        &repo,
        &state,
        state_signature_from_signer(&state.compute_hash(), &author).unwrap(),
        state.attribution.clone(),
    );
    attach_review(&repo, &state, &reviewer, true);
    put_registry(
        &repo,
        vec![
            binding(&author, "identity:alice", None),
            binding(&reviewer, "identity:bob", None),
        ],
    );

    let result = result_for(&repo, &state);
    assert_eq!(result.display_status(), "FAILED(review)");
    assert!(result.detail.contains("did not verify"));
}

fn attach_review(repo: &Repository, state: &State, signer: &Ed25519Signer, tamper: bool) {
    let signed_at = 2_001;
    let payload = signing_payload(
        state.state_id,
        ReviewKind::Read,
        &ReviewScope::WholeChange,
        signed_at,
        None,
    );
    let mut review = ReviewSignature {
        actor: Principal::new("Unverified label", "claim@example.com"),
        kind: ReviewKind::Read,
        scope: ReviewScope::WholeChange,
        justification: None,
        signed_at,
        algorithm: signer.algorithm().to_string(),
        public_key: hex::encode(signer.public_key()),
        signature: hex::encode(signer.sign(&payload).unwrap()),
    };
    if tamper {
        let mut signature = hex::decode(&review.signature).unwrap();
        signature[0] ^= 1;
        review.signature = hex::encode(signature);
    }
    let hash = repo
        .store()
        .put_blob(&Blob::new(
            ReviewSignaturesBlob::new(vec![review]).encode().unwrap(),
        ))
        .unwrap();
    repo.store()
        .put_state_attachment(&StateAttachment {
            state_id: state.state_id,
            body: StateAttachmentBody::ReviewSignatures(hash),
            attribution: state.attribution.clone(),
            created_at: state.created_at,
            supersedes: None,
        })
        .unwrap();
}
