use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

use super::*;
use crate::{creation::*, passkey_delegation::*, wire::*};

fn fixture() -> (SignedMintRootAttachment, VerifiedOwnerState) {
    let owner = TestKey::new(81);
    let passkey = TestKey::new(82);
    let mint = TestKey::new(83);
    let guardian_a = TestKey::new(84);
    let guardian_b = TestKey::new(85);
    let root = signed_root(
        OWNER_UUID,
        &owner,
        &[
            (&guardian_a, RecoveryGuardianKind::Paper),
            (&guardian_b, RecoveryGuardianKind::Social),
        ],
    );
    let state = verify_owner_root(&root).expect("owner root");
    let mut spki = hex::decode("302a300506032b6570032100").expect("SPKI prefix");
    spki.extend_from_slice(&passkey.wire().public_key);
    let authority = PasskeyAuthority {
        format_version: 1,
        account_uuid: OWNER_UUID.to_vec(),
        owner_state_hash: state.state_hash().to_vec(),
        owner_sequence: state.sequence(),
        owner_key: Some(owner.wire()),
        credential_id: vec![6; 32],
        cose_algorithm: -8,
        public_key_spki: spki,
        relying_party_id: "heddle.test".into(),
        allowed_origins: vec!["https://app.heddle.test".into()],
        max_session_ttl_seconds: 43_200,
        nonce: vec![7; 32],
    };
    let certificate = SignedPasskeyAuthority {
        owner_signature: Some(owner.sign_digest(
            &passkey_authority_signing_digest(&authority).expect("certificate digest"),
        )),
        authority: Some(authority),
    };
    let attachment = MintRootAttachment {
        format_version: 1,
        account_uuid: OWNER_UUID.to_vec(),
        owner_state_hash: state.state_hash().to_vec(),
        owner_sequence: state.sequence(),
        owner_key: Some(owner.wire()),
        mint_root_key: Some(mint.wire()),
        not_before_unix_seconds: NOW,
        expires_at_unix_seconds: NOW + 3600,
        nonce: vec![9; 32],
    };
    let client_data_json = serde_json::to_vec(&serde_json::json!({
        "type": "webauthn.get",
        "challenge": URL_SAFE_NO_PAD.encode(mint_root_signing_digest(&attachment).expect("challenge")),
        "origin": "https://app.heddle.test",
        "crossOrigin": false,
    })).expect("client data");
    let mut authenticator_data = Sha256::digest(b"heddle.test").to_vec();
    authenticator_data.push(0x05);
    authenticator_data.extend_from_slice(&0_u32.to_be_bytes());
    let mut signed = authenticator_data.clone();
    signed.extend_from_slice(&Sha256::digest(&client_data_json));
    let signature = passkey.signing.sign(&signed).to_bytes().to_vec();
    (
        SignedMintRootAttachment {
            attachment: Some(attachment),
            owner_signature: None,
            passkey_delegation: Some(PasskeyMintDelegation {
                authority: Some(certificate),
                client_data_json,
                authenticator_data,
                signature,
            }),
        },
        state,
    )
}

fn verify(value: &SignedMintRootAttachment, state: &VerifiedOwnerState) -> Result<()> {
    verify_mint_root_attachment(
        value,
        state,
        &OWNER_UUID,
        &TestKey::new(83).wire().public_key,
        NOW + 1,
    )
}

#[test]
fn owner_authorized_passkey_admits_temporary_mint_without_server_or_existing_device() {
    let (value, state) = fixture();
    verify(&value, &state)
        .expect("owner certificate plus fresh passkey assertion authorizes temporary mint");
    export_vector("ed25519", &value);
}

fn resign_assertion(value: &mut SignedMintRootAttachment, refresh_challenge: bool) {
    let proof = value.passkey_delegation.as_mut().expect("proof");
    if refresh_challenge {
        let mut client: serde_json::Value =
            serde_json::from_slice(&proof.client_data_json).expect("client");
        client["challenge"] = URL_SAFE_NO_PAD
            .encode(
                mint_root_signing_digest(value.attachment.as_ref().expect("attachment"))
                    .expect("challenge"),
            )
            .into();
        proof.client_data_json = serde_json::to_vec(&client).expect("client data");
    }
    let mut signed = proof.authenticator_data.clone();
    signed.extend_from_slice(&Sha256::digest(&proof.client_data_json));
    proof.signature = TestKey::new(82).signing.sign(&signed).to_bytes().to_vec();
}

fn export_vector(name: &str, value: &SignedMintRootAttachment) {
    if std::env::var_os("HEDDLE_EXPORT_PASSKEY_VECTOR").is_some() {
        let authority = value
            .passkey_delegation
            .as_ref()
            .expect("proof")
            .authority
            .as_ref()
            .expect("certificate")
            .authority
            .as_ref()
            .expect("authority");
        println!(
            "PASSKEY_VECTOR={}",
            serde_json::json!({
                "name": name,
                "attachment_proto_hex": hex::encode(value.encode_to_vec()),
                "authority_canonical_hex": hex::encode(canonical_passkey_authority(authority).expect("canonical")),
                "authority_digest_hex": hex::encode(passkey_authority_signing_digest(authority).expect("digest")),
                "now": NOW + 1,
            })
        );
    }
}

#[test]
fn owner_authorized_es256_passkey_accepts_der_assertion() {
    let (mut value, state) = fixture();
    let passkey = p256::ecdsa::SigningKey::from_bytes((&[82_u8; 32]).into()).expect("P256 key");
    let proof = value.passkey_delegation.as_mut().expect("proof");
    let certificate = proof.authority.as_mut().expect("certificate");
    let authority = certificate.authority.as_mut().expect("authority");
    authority.cose_algorithm = -7;
    authority.public_key_spki = hex::decode("3059301306072a8648ce3d020106082a8648ce3d030107034200")
        .expect("P256 SPKI prefix");
    authority.public_key_spki.extend_from_slice(passkey.verifying_key().to_sec1_point(false).as_bytes());
    certificate.owner_signature = Some(
        TestKey::new(81)
            .sign_digest(&passkey_authority_signing_digest(authority).expect("certificate")),
    );
    let mut signed = proof.authenticator_data.clone();
    signed.extend_from_slice(&Sha256::digest(&proof.client_data_json));
    let signature: p256::ecdsa::Signature = passkey.sign(&signed);
    proof.signature = signature.to_der().as_bytes().to_vec();
    verify(&value, &state).expect("P256 certificate and DER signature");
    export_vector("es256", &value);
}

#[test]
fn passkey_assertion_binds_exact_mint_origin_rp_and_user_verification() {
    let (valid, state) = fixture();
    verify(&valid, &state).expect("positive control");
    for (index, mutate) in [
        |value: &mut SignedMintRootAttachment| {
            value.attachment.as_mut().expect("attachment").nonce[0] ^= 1
        },
        |value: &mut SignedMintRootAttachment| {
            value
                .passkey_delegation
                .as_mut()
                .expect("proof")
                .authenticator_data[0] ^= 1
        },
        |value: &mut SignedMintRootAttachment| {
            value
                .passkey_delegation
                .as_mut()
                .expect("proof")
                .authenticator_data[32] = 0x01
        },
        |value: &mut SignedMintRootAttachment| {
            value.passkey_delegation.as_mut().expect("proof").signature[0] ^= 1
        },
        |value: &mut SignedMintRootAttachment| {
            let proof = value.passkey_delegation.as_mut().expect("proof");
            let mut client: serde_json::Value =
                serde_json::from_slice(&proof.client_data_json).expect("client");
            client["origin"] = "https://other.heddle.test".into();
            proof.client_data_json = serde_json::to_vec(&client).expect("client JSON");
        },
    ]
    .into_iter()
    .enumerate()
    {
        let mut changed = valid.clone();
        mutate(&mut changed);
        if matches!(index, 1 | 2 | 4) {
            // Keep the cryptographic signature valid: each independent policy
            // check must reject its own changed RP, UV flag or origin.
            resign_assertion(&mut changed, false);
        }
        assert!(
            verify(&changed, &state).is_err(),
            "mutated authority must be rejected"
        );
    }
}

#[test]
fn passkey_authority_cannot_extend_lifetime_or_select_another_owner() {
    let (valid, state) = fixture();
    verify(&valid, &state).expect("positive control");
    let mut changed = valid.clone();
    changed
        .attachment
        .as_mut()
        .expect("attachment")
        .expires_at_unix_seconds = NOW + 43_201;
    resign_assertion(&mut changed, true);
    assert!(verify(&changed, &state).is_err());
    let mut changed = valid.clone();
    let certificate = changed
        .passkey_delegation
        .as_mut()
        .expect("proof")
        .authority
        .as_mut()
        .expect("certificate");
    certificate
        .authority
        .as_mut()
        .expect("authority")
        .account_uuid = [1; 16].to_vec();
    certificate.owner_signature = Some(
        TestKey::new(81).sign_digest(
            &passkey_authority_signing_digest(certificate.authority.as_ref().expect("authority"))
                .expect("certificate"),
        ),
    );
    assert!(verify(&changed, &state).is_err());
    let mut changed = valid;
    changed.owner_signature = Some(
        TestKey::new(81).sign_digest(
            &mint_root_signing_digest(changed.attachment.as_ref().expect("attachment"))
                .expect("digest"),
        ),
    );
    assert!(
        verify(&changed, &state).is_err(),
        "ambiguous authority forms must be rejected"
    );
}
