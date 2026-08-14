// SPDX-License-Identifier: Apache-2.0

use crypto::{Ed25519Signer, Signer, state_signature_from_signer};
use objects::{
    object::{
        Blob, KeyBinding, KeyRole, Principal, ReviewKind, ReviewScope, ReviewSignature,
        ReviewSignaturesBlob, State, StateAttachment, StateAttachmentBody, signing_payload,
    },
    store::ObjectStore,
};
use repo::Repository;

use super::tests::{attach_signature, binding, put_registry, result_for, setup, state_with_blob};

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
        &author,
        vec![
            binding(&author, "identity:alice", None),
            reviewer_binding(&reviewer, "identity:bob"),
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
        &author,
        vec![
            binding(&author, "identity:alice", None),
            reviewer_binding(&reviewer, "identity:bob"),
        ],
    );

    let result = result_for(&repo, &state);
    assert_eq!(result.display_status(), "FAILED(review)");
    assert!(result.detail.contains("did not verify"));
}

#[test]
fn author_role_cannot_sign_review_evidence() {
    let (_temp, repo) = setup();
    let blob_hash = repo.store().put_blob(&Blob::from("hello")).unwrap();
    let state = state_with_blob(&repo, blob_hash);
    let author = Ed25519Signer::generate().unwrap();
    repo.store().put_state(&state).unwrap();
    attach_signature(
        &repo,
        &state,
        state_signature_from_signer(&state.compute_hash(), &author).unwrap(),
        state.attribution.clone(),
    );
    attach_review(&repo, &state, &author, false);
    put_registry(
        &repo,
        &author,
        vec![binding(&author, "identity:alice", None)],
    );

    let result = result_for(&repo, &state);
    assert_eq!(result.display_status(), "FAILED(review)");
    assert!(result.detail.contains("UnauthorizedRole"), "{result:?}");
}

fn reviewer_binding(signer: &Ed25519Signer, identity: &str) -> KeyBinding {
    let mut reviewer = binding(signer, identity, None);
    reviewer.role = KeyRole::Reviewer;
    reviewer.added_by_sig.signature = hex::encode(
        signer
            .sign(&reviewer.canonical_signing_payload())
            .expect("sign reviewer binding"),
    );
    reviewer
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
