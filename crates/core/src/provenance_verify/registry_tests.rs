// SPDX-License-Identifier: Apache-2.0

use chrono::{Duration, Utc};
use crypto::{Ed25519Signer, Signer, state_signature_from_signer};
use objects::{
    object::{
        Blob, KeyBinding, KeyBindingRegistry, KeyRole, StateAttachment, StateAttachmentBody,
        StateSignature,
    },
    store::ObjectStore,
};
use repo::{KeyBindingRegistryAnchor, Repository, TrustedKey};
use tempfile::TempDir;

use super::verify_repository_provenance;

fn signed_repository(signer: &Ed25519Signer) -> (TempDir, Repository) {
    let temp = TempDir::new().expect("temp dir");
    let repo = Repository::init_default(temp.path()).expect("init repo");
    let state = repo
        .current_state()
        .expect("read current state")
        .expect("seeded state");
    repo.store()
        .put_state_attachment(&StateAttachment {
            state_id: state.state_id,
            body: StateAttachmentBody::Signature(
                state_signature_from_signer(&state.compute_hash(), signer).expect("sign state"),
            ),
            attribution: state.attribution.clone(),
            created_at: state.created_at,
            supersedes: None,
        })
        .expect("attach state signature");
    (temp, repo)
}

fn binding(signer: &Ed25519Signer, identity: &str) -> KeyBinding {
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
        valid_from: Utc::now() - Duration::days(1),
        revoked_at: None,
        delegated_from: None,
    };
    binding.added_by_sig.signature = hex::encode(
        signer
            .sign(&binding.canonical_signing_payload())
            .expect("sign binding"),
    );
    binding
}

pub(super) fn checkpoint(
    authority: &Ed25519Signer,
    epoch: u64,
    previous_registry: Option<objects::object::ContentHash>,
    bindings: Vec<KeyBinding>,
) -> KeyBindingRegistry {
    let mut registry = KeyBindingRegistry::new(
        epoch,
        previous_registry,
        StateSignature {
            algorithm: authority.algorithm().to_string(),
            public_key: hex::encode(authority.public_key()),
            signature: String::new(),
        },
        bindings,
    );
    registry.authority_signature.signature = hex::encode(
        authority
            .sign(
                &registry
                    .canonical_checkpoint_signing_payload()
                    .expect("registry payload"),
            )
            .expect("sign registry"),
    );
    registry
}

pub(super) fn store_registry(repo: &Repository, registry: &KeyBindingRegistry) {
    repo.store()
        .put_blob(&Blob::new(registry.encode().expect("encode registry")))
        .expect("store registry");
}

pub(super) fn anchor(repo: &Repository, registry: &KeyBindingRegistry, authority: &Ed25519Signer) {
    let config_path = repo.heddle_dir().join("config.toml");
    let mut config = repo::RepoConfig::load_for_repository(&config_path).expect("load anchor");
    config.provenance.key_binding_registry = Some(KeyBindingRegistryAnchor {
        registry_hash: registry.content_hash().expect("registry hash").to_hex(),
        epoch: registry.epoch,
        authority: TrustedKey {
            algorithm: authority.algorithm().to_string(),
            public_key: hex::encode(authority.public_key()),
            label: None,
        },
    });
    config.save(&config_path).expect("save anchor");
}

fn state_result(repo: &Repository) -> super::StateProvenanceVerification {
    let state_id = repo
        .current_state()
        .expect("read current state")
        .expect("seeded state")
        .state_id;
    verify_repository_provenance(repo)
        .expect("verify provenance")
        .states
        .into_iter()
        .find(|result| result.state_id == state_id.to_string_full())
        .expect("current state result")
}

#[test]
fn forged_root_identity_claim_is_rejected_by_pinned_authority() {
    let trusted_authority = Ed25519Signer::generate().expect("trusted authority");
    let attacker = Ed25519Signer::generate().expect("attacker");
    let (_temp, repo) = signed_repository(&attacker);
    let forged = checkpoint(
        &attacker,
        0,
        None,
        vec![binding(&attacker, "identity:victim")],
    );
    store_registry(&repo, &forged);
    anchor(&repo, &forged, &trusted_authority);

    let result = state_result(&repo);
    assert_eq!(result.display_status(), "FAILED(identity)");
    assert!(result.detail.contains("trusted authority"), "{result:?}");
}

#[test]
fn substituted_registry_is_rejected_when_pinned_head_is_missing() {
    let authority = Ed25519Signer::generate().expect("authority");
    let attacker = Ed25519Signer::generate().expect("attacker");
    let (_temp, repo) = signed_repository(&attacker);
    let trusted = checkpoint(
        &authority,
        0,
        None,
        vec![binding(&attacker, "identity:alice")],
    );
    let substitute = checkpoint(
        &attacker,
        0,
        None,
        vec![binding(&attacker, "identity:victim")],
    );
    store_registry(&repo, &substitute);
    anchor(&repo, &trusted, &authority);

    let result = state_result(&repo);
    assert_eq!(result.display_status(), "FAILED(identity)");
    assert!(result.detail.contains("is missing"), "{result:?}");
}

#[test]
fn legitimate_registry_chain_verifies() {
    let authority = Ed25519Signer::generate().expect("authority");
    let signer = Ed25519Signer::generate().expect("state signer");
    let (_temp, repo) = signed_repository(&signer);
    let genesis = checkpoint(
        &authority,
        0,
        None,
        vec![binding(&signer, "identity:alice")],
    );
    let head = checkpoint(
        &authority,
        1,
        Some(genesis.content_hash().expect("genesis hash")),
        vec![binding(&signer, "identity:alice")],
    );
    store_registry(&repo, &genesis);
    store_registry(&repo, &head);
    anchor(&repo, &head, &authority);

    let result = state_result(&repo);
    assert_eq!(result.display_status(), "Verified(identity:alice)");
}

#[test]
fn rolled_back_registry_is_rejected_when_current_checkpoint_is_withheld() {
    let authority = Ed25519Signer::generate().expect("authority");
    let signer = Ed25519Signer::generate().expect("state signer");
    let (_temp, repo) = signed_repository(&signer);
    let genesis = checkpoint(
        &authority,
        0,
        None,
        vec![binding(&signer, "identity:alice")],
    );
    let current = checkpoint(
        &authority,
        1,
        Some(genesis.content_hash().expect("genesis hash")),
        vec![binding(&signer, "identity:alice")],
    );
    store_registry(&repo, &genesis);
    anchor(&repo, &current, &authority);

    let result = state_result(&repo);
    assert_eq!(result.display_status(), "FAILED(identity)");
    assert!(result.detail.contains("epoch 1 is missing"), "{result:?}");
}

#[test]
fn anchored_checkpoint_is_rejected_when_predecessor_is_missing() {
    let authority = Ed25519Signer::generate().expect("authority");
    let signer = Ed25519Signer::generate().expect("state signer");
    let (_temp, repo) = signed_repository(&signer);
    let missing_genesis = checkpoint(
        &authority,
        0,
        None,
        vec![binding(&signer, "identity:alice")],
    );
    let head = checkpoint(
        &authority,
        1,
        Some(missing_genesis.content_hash().expect("genesis hash")),
        vec![binding(&signer, "identity:alice")],
    );
    store_registry(&repo, &head);
    anchor(&repo, &head, &authority);

    let result = state_result(&repo);
    assert_eq!(result.display_status(), "FAILED(identity)");
    assert!(result.detail.contains("epoch 0 is missing"), "{result:?}");
}

#[test]
fn stale_registry_epoch_is_rejected() {
    let authority = Ed25519Signer::generate().expect("authority");
    let signer = Ed25519Signer::generate().expect("state signer");
    let (_temp, repo) = signed_repository(&signer);
    let registry = checkpoint(
        &authority,
        0,
        None,
        vec![binding(&signer, "identity:alice")],
    );
    store_registry(&repo, &registry);
    anchor(&repo, &registry, &authority);
    let config_path = repo.heddle_dir().join("config.toml");
    let mut config = repo::RepoConfig::load_for_repository(&config_path).expect("load anchor");
    config
        .provenance
        .key_binding_registry
        .as_mut()
        .expect("registry anchor")
        .epoch = 1;
    config.save(&config_path).expect("save stale anchor");

    let result = state_result(&repo);
    assert_eq!(result.display_status(), "FAILED(identity)");
    assert!(result.detail.contains("epoch 0, expected 1"), "{result:?}");
}
