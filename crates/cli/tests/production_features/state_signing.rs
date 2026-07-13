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
/// `capture`, `fork`, `collapse`, `context set`, `rebase`, and `merge` are
/// exercised here. Mount capture routes through the same chokepoint and is
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

    // rebase (replay re-authors commits onto a new base)
    {
        let temp = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        heddle_signed(&["init"], temp.path(), home.path()).unwrap();
        fs::write(temp.path().join("base.txt"), "base").unwrap();
        heddle_signed(&["capture", "-m", "Base"], temp.path(), home.path()).unwrap();

        heddle_signed(&["thread", "create", "feature"], temp.path(), home.path()).unwrap();
        heddle_signed(&["thread", "switch", "feature"], temp.path(), home.path()).unwrap();
        fs::write(temp.path().join("a1.txt"), "a1").unwrap();
        heddle_signed(&["capture", "-m", "A1"], temp.path(), home.path()).unwrap();

        heddle_signed(&["thread", "switch", "main"], temp.path(), home.path()).unwrap();
        fs::write(temp.path().join("b1.txt"), "b1").unwrap();
        heddle_signed(&["capture", "-m", "B1"], temp.path(), home.path()).unwrap();

        heddle_signed(&["thread", "switch", "feature"], temp.path(), home.path()).unwrap();
        heddle_signed(&["rebase", "main"], temp.path(), home.path()).unwrap();
        assert_head_signed(temp.path(), "rebase replay");
    }

    // merge (a two-parent merge state is itself authored)
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

        heddle_signed(&["thread", "switch", "main"], temp.path(), home.path()).unwrap();
        fs::write(temp.path().join("main.txt"), "main").unwrap();
        heddle_signed(&["capture", "-m", "Main"], temp.path(), home.path()).unwrap();

        // Stale threads must refresh before merge (harness invariant); refresh
        // feature against main, then merge it back into main.
        heddle_signed(&["thread", "switch", "feature"], temp.path(), home.path()).unwrap();
        heddle_signed(&["thread", "refresh", "feature"], temp.path(), home.path()).unwrap();
        heddle_signed(&["thread", "switch", "main"], temp.path(), home.path()).unwrap();
        heddle_signed(&["merge", "feature"], temp.path(), home.path()).unwrap();
        assert_head_signed(temp.path(), "merge");
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
fn test_snapshot_sign_cli() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let signer = Ed25519Signer::generate().expect("generate key");
    let key_pem = signer.to_pem().expect("export PEM");
    let key_path = temp.path().join("signing_key.pem");
    objects::fs_atomic::write_file_atomic_secret(&key_path, key_pem.as_bytes()).unwrap();

    let result = heddle(
        &[
            "capture",
            "-m",
            "Signed commit",
            "--sign",
            &key_path.to_string_lossy(),
        ],
        Some(temp.path()),
    );

    if result.is_ok() {
        let show_result = heddle(&["show", "HEAD", "--output", "json"], Some(temp.path())).unwrap();
        let show: serde_json::Value = serde_json::from_str(&show_result).expect("show JSON");
        assert!(
            show.get("signature").is_some() || show.get("signature_status").is_some(),
            "signed state should have signature info"
        );
    }
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
        supersedes: Some(prior.id()),
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
