// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

use chrono::Duration;
use crypto::{Ed25519Signer, Signer, state_signature_from_signer};
use objects::{
    object::{
        Blob, KeyBinding, KeyBindingRegistry, StateAttachment, StateAttachmentBody, StateSignature,
    },
    store::ObjectStore,
};
use repo::{KeyBindingRegistryAnchor, Repository, TrustedKey};
use serde_json::Value;
use tempfile::TempDir;

fn signed_repository(anchored: bool) -> (TempDir, Repository) {
    let temp = TempDir::new().expect("temp dir");
    let repo = Repository::init_default(temp.path()).expect("init repo");
    let state = repo
        .current_state()
        .expect("read current state")
        .expect("seeded state");
    let signer = Ed25519Signer::generate().expect("signer");
    repo.store()
        .put_state_attachment(&StateAttachment {
            state_id: state.state_id,
            body: StateAttachmentBody::Signature(
                state_signature_from_signer(&state.compute_hash(), &signer).expect("sign state"),
            ),
            attribution: state.attribution.clone(),
            created_at: state.created_at,
            supersedes: None,
        })
        .expect("attach state signature");

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
        valid_from: state.created_at - Duration::seconds(1),
        revoked_at: None,
        delegated_from: None,
    };
    binding.added_by_sig.signature = hex::encode(
        signer
            .sign(&binding.canonical_signing_payload())
            .expect("sign binding"),
    );
    let mut registry = KeyBindingRegistry::new(
        0,
        None,
        StateSignature {
            algorithm: signer.algorithm().to_string(),
            public_key: hex::encode(signer.public_key()),
            signature: String::new(),
        },
        vec![binding],
    );
    registry.authority_signature.signature = hex::encode(
        signer
            .sign(
                &registry
                    .canonical_checkpoint_signing_payload()
                    .expect("registry payload"),
            )
            .expect("sign registry"),
    );
    let registry_hash = registry.content_hash().expect("registry hash");
    repo.store()
        .put_blob(&Blob::new(registry.encode().expect("encode registry")))
        .expect("store registry");
    if anchored {
        let mut config = repo.config().clone();
        config.provenance.key_binding_registry = Some(KeyBindingRegistryAnchor {
            registry_hash: registry_hash.to_hex(),
            epoch: 0,
            authority: TrustedKey {
                algorithm: signer.algorithm().to_string(),
                public_key: hex::encode(signer.public_key()),
                label: None,
            },
        });
        config
            .save(&repo.heddle_dir().join("config.toml"))
            .expect("save registry anchor");
    }
    (temp, repo)
}

fn run_json(repo: &Repository, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(args)
        .current_dir(repo.root())
        .env_remove("CODEX_THREAD_ID")
        .output()
        .expect("run heddle");
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON output")
}

#[test]
fn verify_and_fsck_render_verified_registry_identity_end_to_end() {
    let (_temp, repo) = signed_repository(true);

    let verify = run_json(&repo, &["--output", "json", "verify", "--provenance"]);
    assert_eq!(verify["clean"], true, "{verify}");
    assert_eq!(
        verify["provenance"]["states"][0]["status"], "Verified(identity:alice)",
        "{verify}"
    );
    assert_eq!(
        verify["provenance"]["states"][0]["identity"], "identity:alice",
        "{verify}"
    );

    let fsck = run_json(
        &repo,
        &["--output", "json", "fsck", "--thorough", "--provenance"],
    );
    assert_eq!(fsck["valid"], true, "{fsck}");
    assert_eq!(
        fsck["provenance"]["states"][0]["status"], "Verified(identity:alice)",
        "{fsck}"
    );
}

#[test]
fn verify_rejects_self_signed_registry_without_trusted_anchor() {
    let (_temp, repo) = signed_repository(false);

    let output = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(["--output", "json", "verify", "--provenance"])
        .current_dir(repo.root())
        .env_remove("CODEX_THREAD_ID")
        .output()
        .expect("run heddle");

    assert!(
        !output.status.success(),
        "an unanchored self-signed registry must fail closed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
