use super::*;
use crate::thread_control_authority::{self as proof, Context, Revocation};

const METHOD: &str = "/heddle.api.v2alpha1.ThreadService/RenameThread";
fn fixture(agent: bool) -> (Vec<u8>, VerifiedOwnerState, [u8; 32]) {
    fixture_mint(agent, false)
}
fn fixture_mint(agent: bool, attached: bool) -> (Vec<u8>, VerifiedOwnerState, [u8; 32]) {
    let owner = TestKey::new(91);
    let publisher = TestKey::new(92).signing.verifying_key().to_bytes();
    let a = TestKey::new(93);
    let b = TestKey::new(94);
    let root = signed_root(
        OWNER_UUID,
        &owner,
        &[
            (&a, RecoveryGuardianKind::Paper),
            (&b, RecoveryGuardianKind::Social),
        ],
    );
    let current = verify_owner_root(&root).expect("admitted owner");
    let separate = TestKey::new(95);
    let mint = if attached { &separate } else { &owner };
    let pair =
        KeyPair::from(&PrivateKey::from_bytes(&mint.seed, Algorithm::Ed25519).expect("mint"));
    let expiry = chrono::DateTime::from_timestamp(NOW + 100, 0).expect("expiry");
    let agent_fact = if agent {
        "agent_provider(\"local\");"
    } else {
        ""
    };
    let token = Biscuit::builder().code(format!("user(\"11111111-1111-1111-1111-111111111111\"); session(\"original-session\"); device_pop_key(\"{}\"); {} check if operation(\"RenameThread\"); check if resource(\"spool\", \"acme/project\"); check if time($now), $now < {};", hex::encode(publisher), agent_fact, expiry.to_rfc3339()).as_str()).expect("facts").build(&pair).expect("token");
    let attachment = attached.then(|| {
        let body = crate::wire::MintRootAttachment {
            format_version: 1,
            account_uuid: OWNER_UUID.to_vec(),
            owner_state_hash: current.state_hash().to_vec(),
            owner_sequence: 0,
            owner_key: Some(owner.wire()),
            mint_root_key: Some(mint.wire()),
            not_before_unix_seconds: NOW - 1,
            expires_at_unix_seconds: NOW + 50,
            nonce: vec![7; 32],
        };
        crate::wire::SignedMintRootAttachment {
            owner_signature: Some(owner.sign_digest(
                &crate::creation::mint_root_signing_digest(&body).expect("attachment digest"),
            )),
            attachment: Some(body),
        }
    });
    let bytes = proof::encode(
        &OwnerHistory {
            root: Some(root),
            accepted_transitions: vec![],
            state_hash: current.state_hash().to_vec(),
        },
        &mint.wire().public_key,
        attachment.as_ref(),
        &token,
    )
    .expect("proof");
    (bytes, current, publisher)
}
fn context<'a>(owner: &'a VerifiedOwnerState, publisher: &'a [u8; 32]) -> Context<'a> {
    Context {
        owner,
        account_uuid: &OWNER_UUID,
        publisher,
        agent_id: None,
        method: METHOD,
        spool_path: "acme/project",
        now: NOW,
    }
}
#[test]
fn thread_authority_preserves_original_publisher_account_and_agent() {
    let (bytes, owner, publisher) = fixture(false);
    let valid = proof::verify(&bytes, context(&owner, &publisher), |_| false).expect("human");
    assert_eq!(valid.publisher, publisher);
    assert_eq!(valid.account_uuid, OWNER_UUID);
    assert!(valid.agent_id.is_none());
    assert!(
        proof::verify(&bytes, context(&owner, &[8; 32]), |_| false)
            .err()
            .expect("publisher bound")
            .to_string()
            .contains("publisher")
    );
    let mut wrong = context(&owner, &publisher);
    wrong.account_uuid = &[8; 16];
    assert!(
        proof::verify(&bytes, wrong, |_| false)
            .err()
            .expect("account bound")
            .to_string()
            .contains("account authority")
    );
    let (bytes, owner, publisher) = fixture(true);
    assert!(
        proof::verify(&bytes, context(&owner, &publisher), |_| false)
            .err()
            .expect("agent cannot become human")
            .to_string()
            .contains("agent attribution")
    );
    let mut agent = context(&owner, &publisher);
    agent.agent_id = Some("original-session");
    assert_eq!(
        proof::verify(&bytes, agent, |_| false)
            .expect("agent")
            .agent_id
            .as_deref(),
        Some("original-session")
    );
}
#[test]
fn thread_authority_checks_actual_time_method_and_resource() {
    let (bytes, owner, publisher) = fixture(false);
    let mut expired = context(&owner, &publisher);
    expired.now = NOW + 101;
    assert!(
        proof::verify(&bytes, expired, |_| false)
            .err()
            .expect("actual admission expiry")
            .to_string()
            .contains("original Thread authorization")
    );
    let mut method = context(&owner, &publisher);
    method.method = "/heddle.api.v2alpha1.ThreadService/ReviseIntent";
    assert!(
        proof::verify(&bytes, method, |_| false)
            .err()
            .expect("exact method")
            .to_string()
            .contains("original Thread authorization")
    );
    let mut resource = context(&owner, &publisher);
    resource.spool_path = "acme/other";
    assert!(
        proof::verify(&bytes, resource, |_| false)
            .err()
            .expect("exact resource")
            .to_string()
            .contains("original Thread authorization")
    );
}
#[test]
fn thread_authority_typed_revocations_and_bounds_are_enforced() {
    let (bytes, owner, publisher) = fixture(false);
    for kind in 0..3 {
        assert!(
            proof::verify(&bytes, context(&owner, &publisher), |revoked| matches!(
                (kind, revoked),
                (0, Revocation::MintRoot(_))
                    | (1, Revocation::Publisher(_))
                    | (2, Revocation::Credential(_))
            ))
            .is_err(),
            "revocation kind {kind}"
        );
    }
    let mut trailing = bytes.clone();
    trailing.extend([0x30, 1]);
    assert!(
        proof::verify(&trailing, context(&owner, &publisher), |_| false)
            .err()
            .expect("unknown noncanonical field")
            .to_string()
            .contains("noncanonical")
    );
    assert!(matches!(
        proof::verify(
            &vec![0; proof::MAX_BYTES + 1],
            context(&owner, &publisher),
            |_| false
        ),
        Err(Error::TooLarge { .. })
    ));
}

#[test]
fn thread_authority_requires_current_independent_mint_attachment() {
    let (bytes, owner, publisher) = fixture_mint(false, true);
    proof::verify(&bytes, context(&owner, &publisher), |_| false)
        .expect("attached independent mint");
    let mut later = context(&owner, &publisher);
    later.now = NOW + 51;
    assert!(
        proof::verify(&bytes, later, |_| false)
            .err()
            .expect("mint attachment expires before token")
            .to_string()
            .contains("not currently valid")
    );
    let other = TestKey::new(98);
    let a = TestKey::new(96);
    let b = TestKey::new(97);
    let different = verify_owner_root(&signed_root(
        OWNER_UUID,
        &other,
        &[
            (&a, RecoveryGuardianKind::Paper),
            (&b, RecoveryGuardianKind::Social),
        ],
    ))
    .expect("different independent pin");
    assert!(
        proof::verify(&bytes, context(&different, &publisher), |_| false)
            .err()
            .expect("proof cannot introduce own trust")
            .to_string()
            .contains("independently admitted")
    );
}
