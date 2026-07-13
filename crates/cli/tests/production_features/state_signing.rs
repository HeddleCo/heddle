// SPDX-License-Identifier: Apache-2.0
use crypto::{Ed25519Signer, P256Signer, Signer, SignerError};
use objects::store::ObjectStore;
use serial_test::serial;

use super::*;

/// Run `heddle` with a pinned `HEDDLE_HOME` (so no device identity leaks in
/// from the dev's real home) and a fixed principal. Auto-signing then resolves
/// the per-repo local identity, minted on first capture.
fn heddle_signed(
    args: &[&str],
    cwd: &std::path::Path,
    home: &std::path::Path,
) -> Result<String, String> {
    heddle_with_env(
        args,
        Some(cwd),
        &[
            ("HEDDLE_HOME", home.to_str().expect("home utf8")),
            ("HEDDLE_PRINCIPAL_NAME", "Sign Test"),
            ("HEDDLE_PRINCIPAL_EMAIL", "sign@heddle.dev"),
        ],
    )
}

/// Assert the repo's current HEAD state carries a valid auto-signature. This
/// is the conformance gate: it fails for ANY authored writer that reaches the
/// store without routing through the signing chokepoint (heddle#482).
fn assert_head_signed(path: &std::path::Path, what: &str) {
    let repo = repo::Repository::open(path).expect("open repo");
    let head = repo
        .current_state()
        .expect("current state")
        .expect("repo has a HEAD state");
    assert!(
        repo.get_state_signature(&head.state_id).unwrap().is_some(),
        "{what}: authored HEAD state {} must be auto-signed, not stored unsigned",
        head.state_id.short(),
    );
    assert_eq!(
        repo.verify_state_signature(&head.state_id).expect("verify"),
        crypto::SignatureStatus::Valid,
        "{what}: the auto-signature on the HEAD state must verify",
    );
}

/// Coverage conformance (heddle#482): drive each production command that
/// produces a new *authored* state and assert the resulting HEAD state is
/// signed + verifies. Because signing is lifted to the repo-layer chokepoint
/// (`Repository::put_authored_state`), a future writer that bypasses it —
/// reaching `store().put_state` directly for an authored state — makes the
/// matching arm below fail, so the regression can't merge.
///
/// `capture`, `collapse`, `context set`, and `land` are exercised here.
/// Mount capture routes through the same chokepoint and is
/// covered by `crates/mount` tests + the repo-level signing unit tests (it
/// needs a FUSE mount this subprocess harness can't stand up).
#[test]
#[serial]
fn authored_commands_auto_sign_their_states() {
    // capture / commit
    {
        let temp = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        heddle_signed(&["init"], temp.path(), home.path()).unwrap();
        fs::write(temp.path().join("f.txt"), "x").unwrap();
        heddle_signed(&["capture", "-m", "c"], temp.path(), home.path()).unwrap();
        assert_head_signed(temp.path(), "capture");
    }

    // collapse (squash two states into one authored state)
    {
        let temp = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        heddle_signed(&["init"], temp.path(), home.path()).unwrap();
        fs::write(temp.path().join("f.txt"), "one").unwrap();
        heddle_signed(&["capture", "-m", "first"], temp.path(), home.path()).unwrap();
        fs::write(temp.path().join("f.txt"), "two").unwrap();
        heddle_signed(&["capture", "-m", "second"], temp.path(), home.path()).unwrap();

        let repo = repo::Repository::open(temp.path()).unwrap();
        let head = repo.current_state().unwrap().unwrap();
        let first = head.parents[0];
        heddle_signed(
            &[
                "collapse",
                &first.to_string_full(),
                &head.state_id.to_string_full(),
                "--into",
                "combined",
            ],
            temp.path(),
            home.path(),
        )
        .unwrap();
        assert_head_signed(temp.path(), "collapse");
    }

    // context set (annotation advances HEAD to a new authored state)
    {
        let temp = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        heddle_signed(&["init"], temp.path(), home.path()).unwrap();
        fs::write(temp.path().join("f.txt"), "code\n").unwrap();
        heddle_signed(&["capture", "-m", "base"], temp.path(), home.path()).unwrap();
        heddle_signed(
            &["context", "set", "--path", "f.txt", "-m", "why this exists"],
            temp.path(),
            home.path(),
        )
        .unwrap();
        assert_head_signed(temp.path(), "context set");
    }

    // land (the integrated state is itself authored)
    {
        let temp = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        heddle_signed(&["init"], temp.path(), home.path()).unwrap();
        fs::write(temp.path().join("base.txt"), "base").unwrap();
        heddle_signed(&["capture", "-m", "Base"], temp.path(), home.path()).unwrap();

        heddle_signed(&["thread", "create", "feature"], temp.path(), home.path()).unwrap();
        heddle_signed(&["thread", "switch", "feature"], temp.path(), home.path()).unwrap();
        fs::write(temp.path().join("feat.txt"), "feature").unwrap();
        heddle_signed(&["capture", "-m", "Feature"], temp.path(), home.path()).unwrap();
        let feature_tip = repo::Repository::open(temp.path())
            .unwrap()
            .current_state()
            .unwrap()
            .unwrap()
            .state_id;

        heddle_signed(&["thread", "switch", "main"], temp.path(), home.path()).unwrap();
        fs::write(temp.path().join("main.txt"), "main").unwrap();
        heddle_signed(&["capture", "-m", "Main"], temp.path(), home.path()).unwrap();
        let main_tip = repo::Repository::open(temp.path())
            .unwrap()
            .current_state()
            .unwrap()
            .unwrap()
            .state_id;

        heddle_signed(&["thread", "switch", "feature"], temp.path(), home.path()).unwrap();
        let output = heddle_signed(
            &["--output", "json", "land", "--thread", "feature"],
            temp.path(),
            home.path(),
        )
        .unwrap();
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["synced"], true, "land must sync a stale thread");
        assert_eq!(output["integrated"], true, "land must integrate the thread");

        let repo = repo::Repository::open(temp.path()).unwrap();
        let landed = repo.current_state().unwrap().unwrap();
        assert_ne!(
            landed.state_id, feature_tip,
            "land must not merely fast-forward to the pre-land feature tip"
        );
        assert_ne!(
            landed.state_id, main_tip,
            "land must advance the target thread"
        );
        assert!(
            landed.parents.contains(&main_tip),
            "the state integrated by land must descend from the pre-land target tip"
        );
        assert_head_signed(temp.path(), "land");
    }
}

#[test]
fn test_sign_verify_ed25519() {
    let signer = Ed25519Signer::generate().expect("generate Ed25519 key");
    let data = b"test data for signing";

    let signature = signer.sign(data).expect("sign data");
    signer.verify(data, &signature).expect("verify signature");

    let err = signer
        .verify(b"wrong data", &signature)
        .expect_err("verify should fail");
    assert!(matches!(err, SignerError::VerificationFailed));
}

#[test]
fn test_sign_verify_p256() {
    let signer = P256Signer::generate().expect("generate P256 key");
    let data = b"test data for signing";

    let signature = signer.sign(data).expect("sign data");
    signer.verify(data, &signature).expect("verify signature");

    let err = signer
        .verify(b"wrong data", &signature)
        .expect_err("verify should fail");
    assert!(matches!(err, SignerError::VerificationFailed));
}

#[test]
fn test_capture_signs_cli() {
    let temp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    heddle_signed(&["init"], temp.path(), home.path()).unwrap();
    std::fs::write(temp.path().join("signed.txt"), "signed content").unwrap();
    heddle_signed(
        &["capture", "-m", "Signed commit"],
        temp.path(),
        home.path(),
    )
    .expect("capture must succeed");

    let repo = repo::Repository::open(temp.path()).expect("open signed repository");
    let state_id = repo.head().unwrap().expect("signed capture created HEAD");
    assert!(
        repo.get_state_signature(&state_id)
            .expect("read state signature")
            .is_some(),
        "capture must attach a signature to the authored state"
    );
    assert_eq!(
        repo.verify_state_signature(&state_id)
            .expect("verify state signature"),
        crypto::SignatureStatus::Valid,
        "capture must create a cryptographically valid signature"
    );
}

#[test]
fn test_state_signing_via_repository() {
    use objects::object::{Attribution, Principal, Tree};
    use repo::Repository;

    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).expect("init repo");

    let tree = Tree::new();
    let tree_hash = repo.store().put_tree(&tree).expect("put tree");
    let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
    let state = objects::object::State::new(tree_hash, vec![], attribution);
    repo.store().put_state(&state).expect("put state");
    let state_id = state.state_id;

    let status = repo.verify_state_signature(&state_id).expect("verify");
    assert_eq!(
        status,
        crypto::SignatureStatus::Unsigned,
        "state should initially be unsigned"
    );

    let signer = Ed25519Signer::generate().expect("generate key");
    repo.sign_state(&state_id, &signer).expect("sign state");

    let status = repo.verify_state_signature(&state_id).expect("verify");
    assert_eq!(
        status,
        crypto::SignatureStatus::Valid,
        "state should have valid signature after signing"
    );
}

#[test]
fn test_signature_tampering_detected() {
    use objects::object::{Attribution, Principal, Tree};
    use repo::Repository;

    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).expect("init repo");

    let tree = Tree::new();
    let tree_hash = repo.store().put_tree(&tree).expect("put tree");
    let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
    let state = objects::object::State::new(tree_hash, vec![], attribution);
    repo.store().put_state(&state).expect("put state");
    let state_id = state.state_id;

    let signer = Ed25519Signer::generate().expect("generate key");
    repo.sign_state(&state_id, &signer).expect("sign state");

    let prior = repo
        .latest_state_attachment(&state_id, repo::StateAttachmentKind::Signature)
        .unwrap()
        .unwrap();
    let prior_id = prior.id();
    let objects::object::StateAttachmentBody::Signature(mut signature) = prior.body else {
        panic!("signature attachment")
    };
    let mut sig_bytes = hex::decode(&signature.signature).expect("decode");
    sig_bytes[0] ^= 0xff;
    signature.signature = hex::encode(&sig_bytes);
    repo.put_state_attachment(&objects::object::StateAttachment {
        state_id,
        body: objects::object::StateAttachmentBody::Signature(signature),
        attribution: state.attribution,
        created_at: chrono::Utc::now(),
        supersedes: Some(prior_id),
    })
    .unwrap();

    let status = repo.verify_state_signature(&state_id).expect("verify");
    assert_eq!(
        status,
        crypto::SignatureStatus::Invalid,
        "tampered signature should be invalid"
    );
}

#[test]
fn test_cross_algorithm_verification() {
    let ed_signer = Ed25519Signer::generate().expect("generate Ed25519");
    let p256_signer = P256Signer::generate().expect("generate P256");

    let data = b"same data";

    let ed_sig = ed_signer.sign(data).expect("Ed25519 sign");
    let p256_sig = p256_signer.sign(data).expect("P256 sign");

    ed_signer.verify(data, &ed_sig).expect("verify Ed25519");
    p256_signer.verify(data, &p256_sig).expect("verify P256");

    assert_eq!(ed_sig.len(), 64, "Ed25519 signature is 64 bytes");
    assert!(!p256_sig.is_empty(), "P256 signature should not be empty");
}
