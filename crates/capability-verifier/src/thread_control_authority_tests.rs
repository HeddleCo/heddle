use super::*;
use crate::thread_control_authority::{self as proof, Context, Revocation};

const METHOD: &str = "/heddle.api.v2alpha1.ThreadService/RenameThread";
fn fixture(agent: bool) -> (Vec<u8>, VerifiedOwnerState, [u8; 32]) {
    fixture_mint(agent, false)
}
fn fixture_mint(agent: bool, attached: bool) -> (Vec<u8>, VerifiedOwnerState, [u8; 32]) {
    fixture_mint_method(agent, attached, "RenameThread")
}
fn fixture_mint_method(
    agent: bool,
    attached: bool,
    operation: &str,
) -> (Vec<u8>, VerifiedOwnerState, [u8; 32]) {
    fixture_mint_method_with_facts(agent, attached, operation, "")
}
fn fixture_mint_method_with_facts(
    agent: bool,
    attached: bool,
    operation: &str,
    extra: &str,
) -> (Vec<u8>, VerifiedOwnerState, [u8; 32]) {
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
    let token = Biscuit::builder().code(format!("user(\"11111111-1111-1111-1111-111111111111\"); session(\"original-session\"); credential_id(\"original-credential\"); device_pop_key(\"{}\"); {} {extra} check if operation(\"{operation}\"); check if resource(\"spool\", \"acme/project\"); check if time($now), $now < {};", hex::encode(publisher), agent_fact, expiry.to_rfc3339()).as_str()).expect("facts").build(&pair).expect("token");
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

#[test]
fn retained_device_certificate_and_cached_proof_survive_rotation_without_new_authority() {
    let (bytes, previous, publisher) = fixture_mint(false, true);
    let owner_key = TestKey::new(91);
    let next_key = TestKey::new(96);
    let rotated = apply_accepted_transition(
        &previous,
        &rotation(&previous, &owner_key, &next_key),
        NOW,
        VerificationLimits::new(30 * 24 * 60 * 60).expect("limits"),
    )
    .expect("accepted rotation");
    let envelope = crate::wire::ThreadControlAuthority::decode(bytes.as_slice()).expect("proof");
    let retained = envelope
        .mint_root_attachment
        .clone()
        .expect("admitted certificate");
    assert!(
        proof::verify(&bytes, context(&rotated, &publisher), |_| false).is_err(),
        "a historical signature alone cannot prove earlier enrollment"
    );
    proof::verify_with_retained_mint_roots(
        &bytes,
        context(&rotated, &publisher),
        std::slice::from_ref(&retained),
        |_| false,
    )
    .expect("independently admitted device and cached owner prefix survive rotation");
    let mut backdated = envelope;
    let attachment = backdated
        .mint_root_attachment
        .as_mut()
        .expect("certificate");
    attachment.attachment.as_mut().expect("body").nonce[0] ^= 1;
    attachment.owner_signature = Some(
        owner_key.sign_digest(
            &crate::creation::mint_root_signing_digest(
                attachment.attachment.as_ref().expect("body"),
            )
            .expect("digest"),
        ),
    );
    assert!(
        proof::verify_with_retained_mint_roots(
            &backdated.encode_to_vec(),
            context(&rotated, &publisher),
            std::slice::from_ref(&retained),
            |_| false
        )
        .is_err(),
        "even a valid backdated certificate from the retired owner needs its own earlier admission"
    );
    let (direct, _, direct_publisher) = fixture(false);
    assert!(
        proof::verify_with_retained_mint_roots(
            &direct,
            context(&rotated, &direct_publisher),
            std::slice::from_ref(&retained),
            |_| false
        )
        .is_err(),
        "retired direct owner root cannot mint fresh authority"
    );
    assert!(
        proof::verify_with_retained_mint_roots(
            &bytes,
            context(&rotated, &publisher),
            std::slice::from_ref(&retained),
            |kind| matches!(kind, Revocation::MintRoot(_))
        )
        .is_err(),
        "retained admission cannot override explicit revocation"
    );
    let recovered = apply_accepted_transition(
        &previous,
        &recovery_transition(
            &previous,
            &[&TestKey::new(93), &TestKey::new(94)],
            &next_key,
            previous.recovery_policy().clone(),
            NOW - 1,
        ),
        NOW,
        VerificationLimits::new(30 * 24 * 60 * 60).expect("limits"),
    )
    .expect("accepted recovery");
    assert!(
        proof::verify_with_retained_mint_roots(
            &bytes,
            context(&recovered, &publisher),
            &[retained],
            |_| false
        )
        .is_err(),
        "recovery invalidates previously admitted independent roots"
    );
}

#[test]
fn evidence_original_authority_requires_its_exact_evidence_method() {
    for (operation, method) in [
        (
            "RecordEvidence",
            "/heddle.api.v2alpha1.EvidenceService/RecordEvidence",
        ),
        (
            "AcknowledgeCheck",
            "/heddle.api.v2alpha1.EvidenceService/AcknowledgeCheck",
        ),
    ] {
        let (bytes, owner, publisher) = fixture_mint_method(false, false, operation);
        let mut expected = context(&owner, &publisher);
        expected.method = method;
        proof::verify(&bytes, expected, |_| false).expect("exact evidence author scope");
        assert!(
            proof::verify(&bytes, context(&owner, &publisher), |_| false).is_err(),
            "evidence-only authority cannot mutate Thread metadata"
        );
    }
}

#[test]
fn thread_authority_checks_session_and_stored_credential_revocations() {
    let (bytes, owner, publisher) = fixture(false);
    for revoked_id in ["original-session", "original-credential"] {
        let error = proof::verify(
            &bytes,
            context(&owner, &publisher),
            |revocation| matches!(revocation, Revocation::Credential(id) if id == revoked_id),
        )
        .err()
        .expect("exact credential identity must be revoked");
        assert!(
            error
                .to_string()
                .contains("original Thread capability is revoked"),
            "{revoked_id}: {error}"
        );
    }
}

fn boundary_acceptor(clauses: &str) -> (Vec<u8>, VerifiedOwnerState, [u8; 32]) {
    let (template, owner, _) = fixture(false);
    let envelope =
        crate::wire::ThreadControlAuthority::decode(template.as_slice()).expect("template owner");
    let key = TestKey::new(91);
    let pair = KeyPair::from(
        &PrivateKey::from_bytes(&key.seed, Algorithm::Ed25519).expect("current owner"),
    );
    let publisher = key.signing.verifying_key().to_bytes();
    let token=Biscuit::builder().code(format!("user(\"11111111-1111-1111-1111-111111111111\"); subject_kind(\"user\"); subject_user_uuid(\"11111111-1111-1111-1111-111111111111\"); session(\"accepting-session\"); credential_id(\"accepting-credential\"); device_pop_key(\"{}\"); check if operation(\"PublishContent\"); check if resource(\"spool\", \"acme/project\"); check if time($now), $now < {}; {clauses}",hex::encode(publisher),chrono::DateTime::from_timestamp(NOW+1000,0).expect("expiry").to_rfc3339()).as_str()).expect("current bounded authority").build(&pair).expect("owner credential");
    (
        proof::encode(
            envelope.owner.as_ref().expect("owner history"),
            &publisher,
            None,
            &token,
        )
        .expect("current envelope"),
        owner,
        publisher,
    )
}
fn boundary_scope<'a>(
    thread: &'a [u8; 32],
    subject: &'a [u8; 32],
    publisher: &'a [u8; 32],
) -> crate::boundary_authority::OriginalSubjectScope<'a> {
    crate::boundary_authority::OriginalSubjectScope {
        kind: crate::boundary_authority::BoundarySubjectKind::Source,
        account: &OWNER_UUID,
        thread,
        subject,
        publisher,
        agent_id: Some("original-session"),
    }
}
#[test]
fn boundary_revoked_expired_original_is_provenance_only_and_acceptor_must_be_current() {
    use crate::boundary_authority::{inspect_original_identity, verify_accepting_authority};
    let (old, owner, old_key) = fixture_mint_method(true, true, "PublishContent");
    let mut old_context = context(&owner, &old_key);
    old_context.agent_id = Some("original-session");
    old_context.method = "/heddle.api.v2alpha1.SyncService/PublishContent";
    old_context.now = NOW + 200;
    let inspected = inspect_original_identity(&old, old_context, |_| true)
        .expect("expired revoked signatures remain provenance");
    assert_eq!(inspected.publisher, old_key);
    assert_eq!(inspected.agent_id.as_deref(), Some("original-session"));
    assert!(inspected.explicitly_revoked);
    assert!(
        inspected
            .revocations
            .identifiers()
            .any(|id| id == "original-session")
    );
    assert!(
        inspected
            .revocations
            .identifiers()
            .any(|id| id == "original-credential")
    );
    let mut expired = context(&owner, &old_key);
    expired.agent_id = Some("original-session");
    expired.method = "/heddle.api.v2alpha1.SyncService/PublishContent";
    expired.now = NOW + 200;
    assert!(
        proof::verify(&old, expired, |_| true).is_err(),
        "revoked original cannot authorize its own acceptance"
    );
    let (fresh, current, key) = boundary_acceptor("");
    let thread = [21; 32];
    let subject = [22; 32];
    let mut fresh_context = context(&current, &key);
    fresh_context.now = NOW + 200;
    fresh_context.method = "/heddle.api.v2alpha1.SyncService/PublishContent";
    let accepting = verify_accepting_authority(
        &fresh,
        fresh_context,
        boundary_scope(&thread, &subject, &old_key),
        &[],
        |item| {
            matches!(
                item,
                Revocation::Credential("original-session")
                    | Revocation::Credential("original-credential")
            )
        },
    )
    .expect("different live owner accepts exact old work");
    assert_eq!(accepting.publisher, key);
    assert_ne!(accepting.publisher, inspected.publisher);
    for revoked in ["accepting-session", "accepting-credential"] {
        let mut current_context = context(&current, &key);
        current_context.method = "/heddle.api.v2alpha1.SyncService/PublishContent";
        current_context.now = NOW + 200;
        assert!(
            verify_accepting_authority(
                &fresh,
                current_context,
                boundary_scope(&thread, &subject, &old_key),
                &[],
                |item| matches!(item,Revocation::Credential(id) if id==revoked)
            )
            .is_err(),
            "current accepting revocation must deny: {revoked}"
        );
    }
}
#[test]
fn boundary_acceptance_preserves_subject_attenuation_and_rejects_asserted_selectors() {
    use crate::boundary_authority::{REQUEST_PREDICATE, verify_accepting_authority};
    let thread = [21; 32];
    let subject = [22; 32];
    let original = [92; 32];
    let selector = format!(
        "{REQUEST_PREDICATE}(\"source\",\"{}\",\"{}\",\"{}\",\"{}\",\"original-session\")",
        hex::encode(OWNER_UUID),
        hex::encode(thread),
        hex::encode(subject),
        hex::encode(original)
    );
    let clauses = format!("check if {selector};");
    let (proof, current, key) = boundary_acceptor(&clauses);
    let mut ctx = context(&current, &key);
    ctx.method = "/heddle.api.v2alpha1.SyncService/PublishContent";
    verify_accepting_authority(
        &proof,
        ctx,
        boundary_scope(&thread, &subject, &original),
        &[],
        |_| false,
    )
    .expect("exact original permitted");
    let mut ctx = context(&current, &key);
    ctx.method = "/heddle.api.v2alpha1.SyncService/PublishContent";
    assert!(
        verify_accepting_authority(
            &proof,
            ctx,
            boundary_scope(&[23; 32], &subject, &original),
            &[],
            |_| false
        )
        .is_err(),
        "subject-scoped acceptance must not widen"
    );
    let (forged, current, key) = boundary_acceptor(&format!("{selector}; {clauses}"));
    let mut ctx = context(&current, &key);
    ctx.method = "/heddle.api.v2alpha1.SyncService/PublishContent";
    let error = verify_accepting_authority(
        &forged,
        ctx,
        boundary_scope(&[23; 32], &subject, &original),
        &[],
        |_| false,
    )
    .err()
    .expect("credential cannot claim different request selectors");
    assert!(
        error.to_string().contains("reserved boundary acceptance"),
        "{error}"
    );
}
#[test]
fn boundary_current_delegate_is_not_replaced_by_an_owner_role_shortcut() {
    let (bytes, owner, key) = fixture_mint_method(true, false, "PublishContent");
    let mut ctx = context(&owner, &key);
    ctx.agent_id = Some("original-session");
    ctx.method = "/heddle.api.v2alpha1.SyncService/PublishContent";
    let author = crate::boundary_authority::verify_accepting_authority(
        &bytes,
        ctx,
        boundary_scope(&[21; 32], &[22; 32], &[23; 32]),
        &[],
        |_| false,
    )
    .expect("owner-derived current delegate retains its action permission");
    assert_eq!(author.agent_id.as_deref(), Some("original-session"));
    let mut ctx = context(&owner, &key);
    ctx.agent_id = Some("original-session");
    ctx.method = "/heddle.api.v2alpha1.SyncService/PublishContent";
    let mut scope = boundary_scope(&[21; 32], &[22; 32], &[23; 32]);
    scope.account = &[24; 16];
    assert!(
        crate::boundary_authority::verify_accepting_authority(&bytes, ctx, scope, &[], |_| false)
            .is_err(),
        "same-account original binding is mandatory"
    );
}

#[test]
fn boundary_provenance_cannot_relabel_original_identity_or_enroll_an_incoming_root() {
    use crate::boundary_authority::inspect_original_identity;
    let (bytes, owner, key) = fixture_mint_method(true, true, "PublishContent");
    let mut ctx = context(&owner, &key);
    ctx.agent_id = Some("different-agent");
    assert!(
        inspect_original_identity(&bytes, ctx, |_| true).is_err(),
        "revoked provenance cannot relabel original agent"
    );
    let other_key = [99; 32];
    let mut ctx = context(&owner, &other_key);
    ctx.agent_id = Some("original-session");
    assert!(
        inspect_original_identity(&bytes, ctx, |_| true).is_err(),
        "revoked provenance cannot relabel original publisher"
    );
    let mut ctx = context(&owner, &key);
    ctx.agent_id = Some("original-session");
    ctx.account_uuid = &[9; 16];
    assert!(
        inspect_original_identity(&bytes, ctx, |_| true).is_err(),
        "incoming owner cannot choose a different account"
    );
    let mut changed =
        crate::wire::ThreadControlAuthority::decode(bytes.as_slice()).expect("envelope");
    changed.mint_root_public_key = other_key.to_vec();
    let mut ctx = context(&owner, &key);
    ctx.agent_id = Some("original-session");
    assert!(
        inspect_original_identity(&changed.encode_to_vec(), ctx, |_| true).is_err(),
        "an incoming mint key is not independent trust"
    );
}

#[test]
fn boundary_historical_identity_survives_rotation_and_recovery_without_issuance_rights() {
    let (bytes, previous, publisher) = fixture_mint(false, true);
    let old = TestKey::new(91);
    let next = TestKey::new(96);
    let limits = VerificationLimits::new(30 * 24 * 60 * 60).expect("limits");
    let rotated =
        apply_accepted_transition(&previous, &rotation(&previous, &old, &next), NOW, limits)
            .expect("rotation");
    let recovered = apply_accepted_transition(
        &previous,
        &recovery_transition(
            &previous,
            &[&TestKey::new(93), &TestKey::new(94)],
            &next,
            previous.recovery_policy().clone(),
            NOW - 1,
        ),
        NOW,
        limits,
    )
    .expect("recovery");
    for current in [&rotated, &recovered] {
        let mut ctx = context(current, &publisher);
        ctx.now = NOW + 200;
        let inspected = crate::boundary_authority::inspect_original_identity(&bytes, ctx, |_| true)
            .expect("historical cryptographic identity remains auditable");
        assert_eq!(inspected.publisher, publisher);
        assert!(inspected.explicitly_revoked);
        let mut ctx = context(current, &publisher);
        ctx.now = NOW + 200;
        assert!(
            proof::verify(&bytes, ctx, |_| true).is_err(),
            "inspecting an old issuer never authorizes it to issue now"
        );
    }
}

#[test]
fn boundary_genesis_derives_original_agent_without_weakening_signed_actor_checks() {
    use crate::boundary_authority::{inspect_original_genesis_identity, inspect_original_identity};
    let method = "/heddle.api.v2alpha1.ThreadService/StartThread";
    let (bytes, owner, publisher) = fixture_mint_method(true, false, "StartThread");
    let ctx = || {
        let mut value = context(&owner, &publisher);
        value.method = method;
        value
    };
    let current = proof::verify_genesis_with_retained_mint_roots(&bytes, ctx(), &[], |_| false)
        .expect("ordinary delegated genesis");
    let original = inspect_original_genesis_identity(&bytes, ctx(), |_| false)
        .expect("delegated genesis provenance");
    assert_eq!(original.agent_id.as_deref(), Some("original-session"));
    assert_eq!(original.agent_id, current.agent_id);
    assert!(
        inspect_original_identity(&bytes, ctx(), |_| false).is_err(),
        "signed Source/Claim actor cannot silently become human"
    );
    let mut misleading = ctx();
    misleading.agent_id = Some("unrelated-agent");
    assert!(
        inspect_original_genesis_identity(&bytes, misleading, |_| false).is_err(),
        "genesis callers cannot provide an agent label"
    );
    let mut strict = ctx();
    strict.agent_id = Some("original-session");
    inspect_original_identity(&bytes, strict, |_| false)
        .expect("correct signed source actor still works");
    let mut wrong = ctx();
    wrong.publisher = &[7; 32];
    assert!(
        inspect_original_genesis_identity(&bytes, wrong, |_| false).is_err(),
        "genesis provenance keeps exact original creator binding"
    );
    let (human, human_owner, human_key) = fixture_mint_method(false, false, "StartThread");
    let mut human_ctx = context(&human_owner, &human_key);
    human_ctx.method = method;
    assert!(
        inspect_original_genesis_identity(&human, human_ctx, |_| false)
            .expect("human genesis")
            .agent_id
            .is_none()
    );
}

#[test]
fn boundary_original_device_revocation_is_observed_while_acceptor_still_requires_current_authority()
{
    use heddle_biscuit_verifier::inspection::RevocationSelector;

    use crate::boundary_authority::{
        OriginalRevocation, inspect_original_identity, verify_accepting_authority,
    };
    let (bytes, owner, publisher) = fixture_mint_method_with_facts(
        true,
        true,
        "PublishContent",
        "device(\"original-device\");",
    );
    let ctx = || {
        let mut value = context(&owner, &publisher);
        value.method = "/heddle.api.v2alpha1.SyncService/PublishContent";
        value.agent_id = Some("original-session");
        value.now = NOW + 200;
        value
    };
    let observed = inspect_original_identity(&bytes, ctx(), |selector| {
        matches!(
            selector,
            OriginalRevocation::Credential(RevocationSelector::Device("original-device"))
        )
    })
    .expect("revoked original device remains authenticated provenance");
    assert!(
        observed.explicitly_revoked,
        "original device selector must reach observation callback"
    );
    let selectors = observed.revocations.selectors().collect::<Vec<_>>();
    assert!(selectors.contains(&RevocationSelector::Session("original-session")));
    assert!(selectors.contains(&RevocationSelector::Credential("original-credential")));
    assert!(selectors.contains(&RevocationSelector::Device("original-device")));
    assert!(
        selectors
            .iter()
            .any(|selector| matches!(selector, RevocationSelector::Block(_)))
    );
    let key = hex::encode(TestKey::new(95).signing.verifying_key().to_bytes());
    assert!(
        selectors.contains(&RevocationSelector::EnvelopeDeviceKey(&key)),
        "original verified envelope key is retained independently of delegated publisher"
    );
    assert!(
        !inspect_original_identity(&bytes, ctx(), |_| false)
            .expect("unrevoked observation")
            .explicitly_revoked
    );
    let (fresh, current, key) = boundary_acceptor("");
    let thread = [21; 32];
    let subject = [22; 32];
    let current_ctx = || {
        let mut value = context(&current, &key);
        value.now = NOW + 200;
        value.method = "/heddle.api.v2alpha1.SyncService/PublishContent";
        value
    };
    verify_accepting_authority(
        &fresh,
        current_ctx(),
        boundary_scope(&thread, &subject, &publisher),
        &[],
        |_| false,
    )
    .expect("different current accepting authority");
    assert!(
        verify_accepting_authority(
            &fresh,
            current_ctx(),
            boundary_scope(&thread, &subject, &publisher),
            &[],
            |revocation| matches!(revocation, Revocation::Publisher(_))
        )
        .is_err(),
        "current accepting publisher revocation still denies authorization"
    );
}
