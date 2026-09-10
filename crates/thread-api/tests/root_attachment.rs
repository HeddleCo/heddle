#![cfg(feature = "root-attachment")]
use biscuit_auth::{Biscuit, KeyPair};
use chrono::{DateTime, Utc};
use crypto::{Ed25519Signer, Signer};
use heddle_thread_api::{contract::*, root_attachment};

const ACCOUNT: &str = "00000000-0000-0000-0000-000000000011";
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
    let endpoint_signer = Ed25519Signer::from_seed(&[91; 32]).expect("endpoint");
    let endpoint = EndpointRef {
        public_key: endpoint_signer.public_key().to_vec(),
        kind: EndpointKind::Device as i32,
    };
    let attachment = signed(
        &subject,
        &endpoint_signer,
        root.public().to_bytes().as_slice(),
        &token,
        endpoint.clone(),
        NOW,
        NOW + 300,
    )
    .expect("attachment");
    let now = DateTime::<Utc>::from_timestamp(NOW + 1, 0).expect("now");
    let verified = root_attachment::verify(
        &attachment,
        &token,
        &[root.public()],
        ACCOUNT,
        &endpoint,
        now,
    )
    .expect("portable attachment");
    assert_eq!(verified.subject_public_key(), subject.public_key());
    assert_eq!(
        verified.root_public_key(),
        root.public().to_bytes().as_slice()
    );
    let wrong = KeyPair::new();
    assert!(
        root_attachment::verify(
            &attachment,
            &token,
            &[wrong.public()],
            ACCOUNT,
            &endpoint,
            now
        )
        .is_err()
    );
    let mut changed = attachment.clone();
    changed.subject_public_key = [4; 32].to_vec();
    assert!(
        root_attachment::verify(&changed, &token, &[root.public()], ACCOUNT, &endpoint, now)
            .is_err()
    );
    let mut changed = attachment.clone();
    changed.attachment.as_mut().expect("record").signatures[0].signature[0] ^= 1;
    assert!(
        root_attachment::verify(&changed, &token, &[root.public()], ACCOUNT, &endpoint, now)
            .is_err()
    );
    let other_endpoint = EndpointRef {
        public_key: vec![92; 32],
        ..endpoint.clone()
    };
    assert!(
        root_attachment::verify(
            &attachment,
            &token,
            &[root.public()],
            ACCOUNT,
            &other_endpoint,
            now
        )
        .is_err()
    );
    assert!(
        root_attachment::verify(
            &attachment,
            &token,
            &[root.public()],
            ACCOUNT,
            &endpoint,
            deadline
        )
        .is_err()
    );
    let mut different = token.clone();
    different.push(0);
    assert!(
        root_attachment::verify(
            &attachment,
            &different,
            &[root.public()],
            ACCOUNT,
            &endpoint,
            now
        )
        .is_err()
    );
}

#[test]
fn delegated_attachment_preserves_ancestor_expiry_and_proof_key_chain() {
    let root = KeyPair::new();
    let seed: [u8; 32] = root
        .private()
        .to_bytes()
        .as_slice()
        .try_into()
        .expect("root seed");
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
    let endpoint_signer = Ed25519Signer::from_seed(&[13; 32]).expect("endpoint");
    let endpoint = EndpointRef {
        public_key: endpoint_signer.public_key().to_vec(),
        kind: EndpointKind::Device as i32,
    };
    let attachment = signed(
        &child,
        &endpoint_signer,
        &root.public().to_bytes(),
        &child_token,
        endpoint.clone(),
        NOW,
        NOW + 60,
    )
    .expect("child attachment");
    root_attachment::verify(
        &attachment,
        &child_token,
        &[root.public()],
        ACCOUNT,
        &endpoint,
        now,
    )
    .expect("root derived proof chain");
    let overly_long = signed(
        &child,
        &endpoint_signer,
        &root.public().to_bytes(),
        &child_token,
        endpoint.clone(),
        NOW,
        NOW + 61,
    )
    .expect("untrusted claimed lifetime");
    assert!(
        root_attachment::verify(
            &overly_long,
            &child_token,
            &[root.public()],
            ACCOUNT,
            &endpoint,
            now
        )
        .is_err(),
        "attachment cannot outlive an ancestor attenuation"
    );
    assert!(
        root_attachment::verify(
            &attachment,
            &child_token,
            &[root.public()],
            ACCOUNT,
            &endpoint,
            expiry
        )
        .is_err()
    );
}

fn signed(
    subject: &impl Signer,
    endpoint: &impl Signer,
    root: &[u8],
    token: &[u8],
    device: EndpointRef,
    from: i64,
    to: i64,
) -> Result<RootAttachment, heddle_thread_api::transport::Error> {
    root_attachment::sign_binding(
        subject,
        endpoint,
        RootAttachmentBinding {
            format_version: 2,
            root_public_key: root.to_vec(),
            subject_public_key: subject.public_key().to_vec(),
            device: Some(device),
            credential_digest: blake3::hash(token).as_bytes().to_vec(),
            not_before_unix_seconds: from,
            expires_at_unix_seconds: to,
            account_id: "00000000-0000-0000-0000-000000000011".into(),
            pairing_challenge: vec![42; 32],
        },
    )
}

#[test]
fn binding_requires_distinct_endpoint_key_and_exact_approval_challenge() {
    use prost::Message;
    let subject = Ed25519Signer::from_seed(&[21; 32]).expect("subject");
    let endpoint = Ed25519Signer::from_seed(&[22; 32]).expect("endpoint");
    let binding = RootAttachmentBinding {
        format_version: 2,
        root_public_key: vec![23; 32],
        subject_public_key: subject.public_key().to_vec(),
        device: Some(EndpointRef {
            public_key: endpoint.public_key().to_vec(),
            kind: EndpointKind::Device as i32,
        }),
        credential_digest: vec![24; 32],
        not_before_unix_seconds: NOW,
        expires_at_unix_seconds: NOW + 300,
        account_id: ACCOUNT.into(),
        pairing_challenge: vec![25; 32],
    };
    let attachment =
        root_attachment::sign_binding(&subject, &endpoint, binding.clone()).expect("dual proof");
    root_attachment::verify_possession(&attachment, &binding).expect("both keys proved");
    let mut missing = attachment.clone();
    missing
        .attachment
        .as_mut()
        .expect("record")
        .signatures
        .pop();
    assert!(
        root_attachment::verify_possession(&missing, &binding).is_err(),
        "subject alone cannot register another endpoint"
    );
    let mut forged = attachment.clone();
    forged.attachment.as_mut().expect("record").signatures[1].signature[0] ^= 1;
    assert!(
        root_attachment::verify_possession(&forged, &binding).is_err(),
        "endpoint signature must verify"
    );
    let mut other = binding.clone();
    other.pairing_challenge[0] ^= 1;
    assert!(
        root_attachment::verify_possession(&attachment, &other).is_err(),
        "approval cannot move across pairings"
    );
    let mut shared = binding.clone();
    shared.device.as_mut().expect("device").public_key = subject.public_key().to_vec();
    let same = root_attachment::sign_binding(&subject, &subject, shared.clone())
        .expect("one key serves both roles");
    assert_eq!(
        same.attachment.as_ref().expect("record").signatures.len(),
        1
    );
    root_attachment::verify_possession(&same, &shared).expect("same-key proof");
    let proof = attachment.attachment.as_ref().expect("proof");
    let vector = format!(
        "binding={}\nsubject_public_key={}\nendpoint_public_key={}\nsubject_signature={}\nendpoint_signature={}\n",
        hex::encode(binding.encode_to_vec()),
        hex::encode(subject.public_key()),
        hex::encode(endpoint.public_key()),
        hex::encode(&proof.signatures[0].signature),
        hex::encode(&proof.signatures[1].signature)
    );
    assert_eq!(vector, include_str!("fixtures/root_attachment_v2.txt"));
}

#[test]
fn attached_endpoint_rejects_different_credential_account() {
    let root = KeyPair::new();
    let subject = Ed25519Signer::from_seed(&[17; 32]).expect("subject");
    let endpoint_signer = Ed25519Signer::from_seed(&[91; 32]).expect("endpoint");
    let endpoint = EndpointRef { public_key: endpoint_signer.public_key().to_vec(), kind: EndpointKind::Device as i32 };
    let token = Biscuit::builder()
        .fact("user(\"other-account\")").expect("user")
        .fact("subject_user_uuid(\"00000000-0000-0000-0000-000000000022\")").expect("different account")
        .fact("session(\"other-account-session\")").expect("session")
        .fact(format!("device_pop_key(\"{}\")", hex::encode(subject.public_key())).as_str()).expect("subject")
        .build(&root).expect("valid root credential").to_vec().expect("credential");
    let attachment = signed(&subject, &endpoint_signer, &root.public().to_bytes(), &token, endpoint.clone(), NOW, NOW + 300).expect("correct signatures");
    let error = root_attachment::verify(&attachment, &token, &[root.public()], ACCOUNT, &endpoint,
        DateTime::<Utc>::from_timestamp(NOW + 1, 0).expect("clock")).err().expect("root trust cannot relabel credential account");
    assert!(error.to_string().contains("credential subject or lifetime"), "{error}");
}
