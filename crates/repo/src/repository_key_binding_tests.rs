// SPDX-License-Identifier: Apache-2.0

use chrono::{TimeZone, Utc};
use crypto::{Ed25519Signer, Signer, state_signature_from_signer};
use objects::{
    object::{
        Attribution, ContentHash, KeyBinding, KeyBindingRegistry, Principal, State,
        StateAttachment, StateAttachmentBody, StateSignature, Tree,
    },
    store::ObjectStore,
};
use tempfile::TempDir;

use super::{AuthorshipVerification, Repository};

fn setup() -> (TempDir, Repository, State) {
    let temp = TempDir::new().expect("temp dir");
    let repo = Repository::init_default(temp.path()).expect("init repo");
    let tree = Tree::new();
    let tree_hash = repo.store().put_tree(&tree).expect("put tree");
    let mut state = State::new(
        tree_hash,
        Vec::new(),
        Attribution::human(Principal::new("Alice", "alice@example.com")),
    );
    state.created_at = Utc.timestamp_opt(2_000, 0).unwrap();
    repo.store().put_state(&state).expect("put state");
    (temp, repo, state)
}

fn attach_signature(repo: &Repository, state: &State, signature: StateSignature) {
    repo.put_state_attachment(&StateAttachment {
        state_id: state.id(),
        body: StateAttachmentBody::Signature(signature),
        attribution: state.attribution.clone(),
        created_at: state.created_at,
        supersedes: None,
    })
    .expect("attach signature");
}

fn binding(signer: &Ed25519Signer, valid_from: i64, revoked_at: Option<i64>) -> KeyBinding {
    let public_key = hex::encode(signer.public_key());
    let mut binding = KeyBinding {
        algorithm: signer.algorithm().to_string(),
        public_key: public_key.clone(),
        identity_ref: "identity:alice".to_string(),
        role: "author".to_string(),
        added_by_sig: StateSignature {
            algorithm: signer.algorithm().to_string(),
            public_key,
            signature: String::new(),
        },
        valid_from: Utc.timestamp_opt(valid_from, 0).unwrap(),
        revoked_at: revoked_at.map(|timestamp| Utc.timestamp_opt(timestamp, 0).unwrap()),
        delegated_from: None,
    };
    binding.added_by_sig.signature = hex::encode(
        signer
            .sign(&binding.canonical_signing_payload())
            .expect("sign binding"),
    );
    binding
}

fn delegated_binding(
    signer: &Ed25519Signer,
    root_signer: &Ed25519Signer,
    root: &KeyBinding,
) -> KeyBinding {
    let mut binding = KeyBinding {
        algorithm: signer.algorithm().to_string(),
        public_key: hex::encode(signer.public_key()),
        identity_ref: root.identity_ref.clone(),
        role: "author".to_string(),
        added_by_sig: StateSignature {
            algorithm: root_signer.algorithm().to_string(),
            public_key: hex::encode(root_signer.public_key()),
            signature: String::new(),
        },
        valid_from: Utc.timestamp_opt(1_500, 0).unwrap(),
        revoked_at: None,
        delegated_from: Some(root.content_hash().expect("root hash")),
    };
    binding.added_by_sig.signature = hex::encode(
        root_signer
            .sign(&binding.canonical_signing_payload())
            .expect("sign delegated binding"),
    );
    binding
}

fn signed_state(state: &State, signer: &Ed25519Signer) -> StateSignature {
    state_signature_from_signer(&state.compute_hash(), signer).expect("sign state")
}

#[test]
fn resolves_registered_active_key_as_verified_identity() {
    let (_temp, repo, state) = setup();
    let signer = Ed25519Signer::generate().expect("signer");
    attach_signature(&repo, &state, signed_state(&state, &signer));
    let registry = KeyBindingRegistry::new(vec![binding(&signer, 1_000, None)]);

    assert_eq!(
        repo.verify_authored_by_known_actor(&state, &registry)
            .unwrap(),
        AuthorshipVerification::Verified("identity:alice".to_string())
    );
}

#[test]
fn resolves_one_hop_identity_delegation() {
    let (_temp, repo, state) = setup();
    let root_signer = Ed25519Signer::generate().expect("root signer");
    let state_signer = Ed25519Signer::generate().expect("state signer");
    let root = binding(&root_signer, 1_000, None);
    let delegated = delegated_binding(&state_signer, &root_signer, &root);
    attach_signature(&repo, &state, signed_state(&state, &state_signer));
    let registry = KeyBindingRegistry::new(vec![root, delegated]);

    assert_eq!(
        repo.verify_authored_by_known_actor(&state, &registry)
            .unwrap(),
        AuthorshipVerification::Verified("identity:alice".to_string())
    );
}

#[test]
fn valid_signature_from_unregistered_key_is_unknown_not_verified() {
    let (_temp, repo, state) = setup();
    let signer = Ed25519Signer::generate().expect("signer");
    let other_signer = Ed25519Signer::generate().expect("registered signer");
    attach_signature(&repo, &state, signed_state(&state, &signer));
    let non_matching_registry = KeyBindingRegistry::new(vec![binding(&other_signer, 1_000, None)]);

    assert_eq!(
        repo.verify_authored_by_known_actor(&state, &non_matching_registry)
            .unwrap(),
        AuthorshipVerification::UnknownKey
    );
    assert_eq!(
        repo.verify_authored_by_known_actor(&state, &KeyBindingRegistry::empty())
            .unwrap(),
        AuthorshipVerification::UnknownKey,
        "an empty registry must also fail closed"
    );
}

#[test]
fn registered_key_past_revoked_at_is_revoked() {
    let (_temp, repo, state) = setup();
    let signer = Ed25519Signer::generate().expect("signer");
    attach_signature(&repo, &state, signed_state(&state, &signer));
    let registry = KeyBindingRegistry::new(vec![binding(&signer, 1_000, Some(1_999))]);

    assert_eq!(
        repo.verify_authored_by_known_actor(&state, &registry)
            .unwrap(),
        AuthorshipVerification::Revoked
    );
}

#[test]
fn registered_key_with_bad_state_signature_is_invalid() {
    let (_temp, repo, state) = setup();
    let signer = Ed25519Signer::generate().expect("signer");
    let mut signature = signed_state(&state, &signer);
    let mut bytes = hex::decode(&signature.signature).expect("decode signature");
    bytes[0] ^= 0xff;
    signature.signature = hex::encode(bytes);
    attach_signature(&repo, &state, signature);
    let registry = KeyBindingRegistry::new(vec![binding(&signer, 1_000, None)]);

    assert_eq!(
        repo.verify_authored_by_known_actor(&state, &registry)
            .unwrap(),
        AuthorshipVerification::Invalid
    );
}

#[test]
fn registered_key_before_valid_from_is_invalid() {
    let (_temp, repo, state) = setup();
    let signer = Ed25519Signer::generate().expect("signer");
    attach_signature(&repo, &state, signed_state(&state, &signer));
    let registry = KeyBindingRegistry::new(vec![binding(&signer, 2_001, None)]);

    assert_eq!(
        repo.verify_authored_by_known_actor(&state, &registry)
            .unwrap(),
        AuthorshipVerification::Invalid
    );
}

#[test]
fn invalid_binding_signature_makes_registry_invalid() {
    let (_temp, repo, state) = setup();
    let signer = Ed25519Signer::generate().expect("signer");
    attach_signature(&repo, &state, signed_state(&state, &signer));
    let mut invalid_binding = binding(&signer, 1_000, None);
    invalid_binding.added_by_sig.signature = hex::encode([0; 64]);
    let registry = KeyBindingRegistry::new(vec![invalid_binding]);

    assert_eq!(
        repo.verify_authored_by_known_actor(&state, &registry)
            .unwrap(),
        AuthorshipVerification::Invalid
    );
}

#[test]
fn registry_encoding_is_content_addressed() {
    let signer = Ed25519Signer::generate().expect("signer");
    let registry = KeyBindingRegistry::new(vec![binding(&signer, 1_000, None)]);
    let encoded = registry.encode().expect("encode registry");
    let decoded = KeyBindingRegistry::decode(&encoded).expect("decode registry");

    assert_eq!(decoded, registry);
    assert_eq!(
        decoded.content_hash().unwrap(),
        registry.content_hash().unwrap()
    );
    assert_ne!(
        registry.content_hash().unwrap(),
        ContentHash::compute_typed("some-other-object", &encoded)
    );
}
