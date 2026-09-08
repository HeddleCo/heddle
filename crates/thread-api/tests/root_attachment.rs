#![cfg(feature = "root-attachment")]
use biscuit_auth::{Biscuit, KeyPair};
use chrono::{DateTime, Utc};
use crypto::{Ed25519Signer, Signer};
use heddle_thread_api::{contract::*, root_attachment};

const NOW: i64 = 2_000_000_000;

#[test]
fn attached_endpoint_requires_trusted_root_exact_credential_and_subject_possession() {
    let root = KeyPair::new();
    let subject = Ed25519Signer::from_seed(&[17; 32]).expect("subject");
    let deadline = DateTime::<Utc>::from_timestamp(NOW + 300, 0).expect("expiry");
    let token = Biscuit::builder()
        .fact("user(\"account\")")
        .expect("account")
        .fact("session(\"attached-device\")")
        .expect("session")
        .fact(format!("device_pop_key(\"{}\")", hex::encode(subject.public_key())).as_str())
        .expect("proof key")
        .fact(format!("expires_at({})", deadline.to_rfc3339()).as_str())
        .expect("expiry")
        .check(format!("check if time($now), $now < {}", deadline.to_rfc3339()).as_str())
        .expect("expiry check")
        .build(&root)
        .expect("root credential")
        .to_vec()
        .expect("raw credential");
    let endpoint = EndpointRef {
        public_key: vec![91; 32],
        kind: EndpointKind::Device as i32,
    };
    let attachment = root_attachment::sign(
        &subject,
        root.public().to_bytes().as_slice(),
        &token,
        endpoint.clone(),
        NOW,
        NOW + 300,
    )
    .expect("attachment");
    let now = DateTime::<Utc>::from_timestamp(NOW + 1, 0).expect("now");
    let verified = root_attachment::verify(&attachment, &token, &[root.public()], &endpoint, now)
        .expect("portable attachment");
    assert_eq!(verified.subject_public_key(), subject.public_key());
    assert_eq!(
        verified.root_public_key(),
        root.public().to_bytes().as_slice()
    );
    let wrong = KeyPair::new();
    assert!(
        root_attachment::verify(&attachment, &token, &[wrong.public()], &endpoint, now).is_err()
    );
    let mut changed = attachment.clone();
    changed.subject_public_key = [4; 32].to_vec();
    assert!(root_attachment::verify(&changed, &token, &[root.public()], &endpoint, now).is_err());
    let mut changed = attachment.clone();
    changed.attachment.as_mut().expect("record").signatures[0].signature[0] ^= 1;
    assert!(root_attachment::verify(&changed, &token, &[root.public()], &endpoint, now).is_err());
    let other_endpoint = EndpointRef {
        public_key: vec![92; 32],
        ..endpoint.clone()
    };
    assert!(
        root_attachment::verify(&attachment, &token, &[root.public()], &other_endpoint, now)
            .is_err()
    );
    assert!(
        root_attachment::verify(&attachment, &token, &[root.public()], &endpoint, deadline)
            .is_err()
    );
    let mut different = token.clone();
    different.push(0);
    assert!(
        root_attachment::verify(&attachment, &different, &[root.public()], &endpoint, now).is_err()
    );
}

#[test]
fn delegated_attachment_preserves_ancestor_expiry_and_proof_key_chain() {
    let root = KeyPair::new();
    let seed: [u8; 32] = root.private().to_bytes().as_slice().try_into().expect("root seed");
    let root_signer = Ed25519Signer::from_seed(&seed).expect("root signer");
    let child = Ed25519Signer::from_seed(&[31; 32]).expect("child");
    let now = DateTime::<Utc>::from_timestamp(NOW, 0).expect("now");
    let expiry = DateTime::<Utc>::from_timestamp(NOW + 60, 0).expect("expiry");
    let token = Biscuit::builder()
        .fact("user(\"owner\")")
        .expect("owner")
        .fact("session(\"owner-session\")")
        .expect("session")
        .fact(
            format!(
                "device_pop_key(\"{}\")",
                hex::encode(root_signer.public_key())
            )
            .as_str(),
        )
        .expect("root proof key")
        .build(&root)
        .expect("root token");
    let parent_id = token
        .revocation_identifiers()
        .last()
        .expect("parent revocation")
        .to_vec();
    let statement = [
        b"heddle-pop-delegation-v1\0".as_slice(),
        &parent_id,
        child.public_key(),
    ]
    .concat();
    let signature = root_signer.sign(&statement).expect("child delegation");
    let child_token = token
        .append(
            biscuit_verifier::delegation::AgentAttenuation::time_bounded("paired-device", expiry)
                .block()
                .expect("shared restrictions")
                .fact(
                    format!(
                        "pop_delegation(\"{}\", \"{}\", \"{}\")",
                        hex::encode(parent_id),
                        hex::encode(child.public_key()),
                        hex::encode(signature)
                    )
                    .as_str(),
                )
                .expect("proof transition"),
        )
        .expect("child token")
        .to_vec()
        .expect("raw child");
    let endpoint = EndpointRef {
        public_key: vec![13; 32],
        kind: EndpointKind::Device as i32,
    };
    let attachment = root_attachment::sign(
        &child,
        &root.public().to_bytes(),
        &child_token,
        endpoint.clone(),
        NOW,
        NOW + 60,
    )
    .expect("child attachment");
    root_attachment::verify(&attachment, &child_token, &[root.public()], &endpoint, now)
        .expect("root derived proof chain");
    let overly_long = root_attachment::sign(
        &child,
        &root.public().to_bytes(),
        &child_token,
        endpoint.clone(),
        NOW,
        NOW + 61,
    )
    .expect("untrusted claimed lifetime");
    assert!(
        root_attachment::verify(&overly_long, &child_token, &[root.public()], &endpoint, now)
            .is_err(),
        "attachment cannot outlive an ancestor attenuation"
    );
    assert!(
        root_attachment::verify(
            &attachment,
            &child_token,
            &[root.public()],
            &endpoint,
            expiry
        )
        .is_err()
    );
}
