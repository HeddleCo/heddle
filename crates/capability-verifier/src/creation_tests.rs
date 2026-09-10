use super::*;
use crate::{creation::*, wire::*};

fn creation_fixture(
    separate_mint: bool,
    sealed: bool,
) -> (SignedSpoolOwnerGenesis, VerifiedOwnerState) {
    let owner = TestKey::new(71);
    let creator = TestKey::new(72);
    let independent = TestKey::new(73);
    let guardian_a = TestKey::new(74);
    let guardian_b = TestKey::new(75);
    let root = signed_root(
        OWNER_UUID,
        &owner,
        &[
            (&guardian_a, RecoveryGuardianKind::Paper),
            (&guardian_b, RecoveryGuardianKind::Social),
        ],
    );
    let current = verify_owner_root(&root).expect("owner root");
    let genesis = SpoolOwnerGenesis {
        spool_uuid: SPOOL.to_vec(),
        owner_public_key: Some(owner.wire()),
    };
    let statement = SpoolCreationStatement {
        format_version: 1,
        genesis_digest: spool_genesis_digest(&genesis)
            .expect("genesis digest")
            .to_vec(),
        account_uuid: OWNER_UUID.to_vec(),
        owner_state_hash: current.state_hash().to_vec(),
        owner_sequence: 0,
        creator_key: Some(creator.wire()),
        parent_spool_uuid: OTHER_SPOOL.to_vec(),
        parent_path_segments: vec!["acme".into()],
        name: "project".into(),
        created_at_unix_seconds: NOW,
    };
    let mint = if separate_mint { &independent } else { &owner };
    let pair =
        KeyPair::from(&PrivateKey::from_bytes(&mint.seed, Algorithm::Ed25519).expect("mint key"));
    let expiry = chrono::DateTime::from_timestamp(NOW + 100, 0).expect("expiry");
    let parent = Biscuit::builder().code(format!("user(\"11111111-1111-1111-1111-111111111111\"); session(\"agent-session\"); device_pop_key(\"{}\"); right(\"spool\", \"acme\", \"admin\"); check if time($now), $now < {};", hex::encode(creator.wire().public_key), expiry.to_rfc3339()).as_str()).expect("facts").build(&pair).expect("parent token").to_base64().expect("base64");
    let child_key = creator.signing.verifying_key().to_bytes();
    let signature = creator
        .signing
        .sign(
            &heddle_biscuit_verifier::key_delegation::statement(&parent, &child_key)
                .expect("delegation statement"),
        )
        .to_bytes();
    let child = heddle_biscuit_verifier::key_delegation::append(
        &parent,
        &child_key,
        &signature,
        creation_restrictions(&statement).expect("creation restriction"),
    )
    .expect("narrow child");
    let child = biscuit_auth::UnverifiedBiscuit::from_base64(&child).expect("child");
    let child = if sealed {
        child.seal().expect("sealed")
    } else {
        child
    };
    let certificate =
        separate_mint.then(|| {
            let attachment = MintRootAttachment {
                format_version: 1,
                account_uuid: OWNER_UUID.to_vec(),
                owner_state_hash: current.state_hash().to_vec(),
                owner_sequence: 0,
                owner_key: Some(owner.wire()),
                mint_root_key: Some(mint.wire()),
                not_before_unix_seconds: NOW - 1,
                expires_at_unix_seconds: NOW + 50,
                nonce: vec![5; 32],
            };
            SignedMintRootAttachment {
                owner_signature: Some(owner.sign_digest(
                    &mint_root_signing_digest(&attachment).expect("certificate digest"),
                )),
                attachment: Some(attachment),
            }
        });
    let proof = SpoolCreationProof {
        creator_signature: Some(
            creator
                .sign_digest(&spool_creation_signing_digest(&statement).expect("creation digest")),
        ),
        statement: Some(statement),
        sealed_biscuit: child.to_vec().expect("serialized proof"),
        mint_root_attachment: certificate,
        owner_history: Some(OwnerHistory {
            root: Some(root),
            accepted_transitions: vec![],
            state_hash: current.state_hash().to_vec(),
        }),
    };
    (
        SignedSpoolOwnerGenesis {
            genesis: Some(genesis),
            owner_signature: None,
            delegated_creation: Some(proof),
        },
        current,
    )
}

#[test]
fn delegated_creation_uses_exact_sealed_permission_and_actual_current_time() {
    let (signed, current) = creation_fixture(false, true);
    validate_spool_creation_structure(&signed, NOW).expect("structure and lineage");
    let facts =
        admit_fresh_spool_creation(&signed, &current, NOW).expect("authorized delegated creation");
    assert_eq!(facts.sid, "agent-session");
    assert_eq!(
        facts.cnf.as_deref(),
        Some(hex::encode(TestKey::new(72).wire().public_key).as_str())
    );
    validate_spool_creation_structure(&signed, NOW + 101)
        .expect("expiry does not erase structural evidence");
    assert!(
        admit_fresh_spool_creation(&signed, &current, NOW + 101)
            .expect_err("claimed old timestamp is not admission")
            .to_string()
            .contains("creation capability")
    );
    let mut other = signed.clone();
    other.genesis.as_mut().expect("genesis").spool_uuid = OTHER_SPOOL.to_vec();
    assert!(
        validate_spool_creation_structure(&other, NOW)
            .err()
            .expect("different spool rejected")
            .to_string()
            .contains("another genesis")
    );
    let (unsealed, _) = creation_fixture(false, false);
    assert!(
        validate_spool_creation_structure(&unsealed, NOW)
            .err()
            .expect("private attenuation key must not be public")
            .to_string()
            .contains("must be sealed")
    );
}

#[test]
fn independent_mint_root_requires_current_owner_certificate() {
    let (signed, current) = creation_fixture(true, true);
    admit_fresh_spool_creation(&signed, &current, NOW)
        .expect("owner-associated independent mint root");
    assert!(
        admit_fresh_spool_creation(&signed, &current, NOW + 51)
            .expect_err("owner certificate expiry")
            .to_string()
            .contains("not currently valid")
    );
    let mut missing = signed.clone();
    missing
        .delegated_creation
        .as_mut()
        .expect("proof")
        .mint_root_attachment = None;
    assert!(
        validate_spool_creation_structure(&missing, NOW).is_err(),
        "host mapping cannot replace owner proof"
    );
    let mut forged = signed;
    forged
        .delegated_creation
        .as_mut()
        .expect("proof")
        .mint_root_attachment
        .as_mut()
        .expect("certificate")
        .owner_signature
        .as_mut()
        .expect("signature")
        .signature[0] ^= 1;
    assert!(
        validate_spool_creation_structure(&forged, NOW).is_err(),
        "owner must sign association"
    );
}

#[test]
fn retired_owner_state_cannot_admit_a_new_delegated_creation() {
    let (signed, current) = creation_fixture(false, true);
    let next = TestKey::new(79);
    let transition = rotation(&current, &TestKey::new(71), &next);
    let rotated =
        apply_transition(&current, &transition, NOW, limits()).expect("accepted rotation");
    validate_spool_creation_structure(&signed, NOW)
        .expect("original proof remains structural evidence");
    assert!(
        admit_fresh_spool_creation(&signed, &rotated, NOW)
            .expect_err("retired state cannot newly admit")
            .to_string()
            .contains("actual current owner state")
    );
    let verified = verify_spool_owner_genesis(&signed).expect("portable delegated signatures");
    assert_eq!(verified.spool_uuid(), SPOOL);
}

#[test]
fn mint_root_attachment_canonical_browser_fixture() {
    let (signed, current) = creation_fixture(true, true);
    let certificate = signed
        .delegated_creation
        .expect("proof")
        .mint_root_attachment
        .expect("certificate");
    let attachment = certificate.attachment.as_ref().expect("body");
    verify_mint_root_attachment(
        &certificate,
        &current,
        &OWNER_UUID,
        &TestKey::new(73).wire().public_key,
        NOW,
    )
    .expect("enrollment association");
    let fixture = serde_json::json!({
        "format": "heddle-mint-root-attachment-v1", "account_uuid_hex": hex::encode(&attachment.account_uuid),
        "owner_state_hash_hex": hex::encode(&attachment.owner_state_hash), "owner_sequence": attachment.owner_sequence,
        "owner_public_key_hex": hex::encode(&attachment.owner_key.as_ref().expect("owner").public_key),
        "mint_root_public_key_hex": hex::encode(&attachment.mint_root_key.as_ref().expect("mint").public_key),
        "not_before": attachment.not_before_unix_seconds, "expires_at": attachment.expires_at_unix_seconds,
        "nonce_hex": hex::encode(&attachment.nonce), "canonical_hex": hex::encode(canonical_mint_root_attachment(attachment).expect("canonical")),
        "signing_digest_hex": hex::encode(mint_root_signing_digest(attachment).expect("digest")),
        "signature_hex": hex::encode(&certificate.owner_signature.as_ref().expect("signature").signature),
        "record_hex": hex::encode(certificate.encode_to_vec())
    });
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "../tests/fixtures/mint_root_attachment_v1.json"
    ))
    .expect("fixture JSON");
    assert_eq!(fixture, expected);
}
