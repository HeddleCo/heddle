use super::*;
use crate::{
    canonical::OWNER_CAPABILITY_V3_DOMAIN,
    wire::{
        TimelineAcceptanceScope, TimelineAdmissionAcceptance, TimelineOriginCredentialClass,
        TimelineOriginCredentialIdentity, TimelineOriginEndorsement,
        TimelineServerIssuedCredential, timeline_admission_acceptance::Authority,
        timeline_origin_credential_identity::Identity,
    },
};

const PRINCIPAL: &str = "11111111-1111-1111-1111-111111111111";
const SPOOL_ID: &str = "22222222-2222-2222-2222-222222222222";

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn api_origin_v3_byte_vectors_match_both_identity_variants() {
    use crate::wire::TimelineOfflineDerivedCredential;
    let golden: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/timeline-origin-v3.json"))
            .expect("API vector");
    let mut origin = TimelineOriginEndorsement {
        deployment_public_key: vec![1; 32],
        spool_id: "123e4567-e89b-12d3-a456-426614174000".into(),
        thread_id: vec![2; 32],
        run_id: "run_7".into(),
        principal_id: "123e4567-e89b-12d3-a456-426614174000".into(),
        credential_class: TimelineOriginCredentialClass::Agent as i32,
        effective_pop_key_sha256: vec![3; 32],
        credential_identity: Some(TimelineOriginCredentialIdentity {
            identity: Some(Identity::ServerIssued(TimelineServerIssuedCredential {
                credential_id: vec![4; 16],
            })),
        }),
        uploader_device_public_key: vec![5; 32],
        signature: vec![6; 64],
    };
    assert_eq!(
        hex::encode(
            heddle_api::timeline_upload::origin_signing_bytes(&origin).expect("issued bytes")
        ),
        golden["server_issued"].as_str().expect("issued vector")
    );
    origin.credential_identity = Some(TimelineOriginCredentialIdentity {
        identity: Some(Identity::OfflineDerived(TimelineOfflineDerivedCredential {
            issued_ancestor_credential_id: vec![4; 16],
            terminal_revocation_id: vec![7; 64],
            derivation_path_sha256: vec![8; 32],
        })),
    });
    assert_eq!(
        hex::encode(
            heddle_api::timeline_upload::origin_signing_bytes(&origin).expect("offline bytes")
        ),
        golden["offline_derived"].as_str().expect("offline vector")
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn v3_capability_body_matches_contract_field_order_and_identity_tags() {
    fn counted(into: &mut Vec<u8>, bytes: &[u8]) {
        into.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        into.extend_from_slice(bytes);
    }
    for offline in [false, true] {
        let mut fixture = TimelineFixture::new();
        if offline {
            fixture.use_offline_identity();
        }
        let capability = fixture.bundle.capability_chain[0]
            .capability
            .as_ref()
            .expect("capability");
        let subject = capability.subject.as_ref().expect("subject");
        let key = subject.key.as_ref().expect("key");
        let grant = &capability.grants[0];
        let selector = grant.spool.as_ref().expect("selector");
        let scope = grant.timeline_acceptance.as_ref().expect("scope");
        let mut expected = Vec::new();
        expected.extend_from_slice(&3u32.to_be_bytes());
        for field in [
            &capability.owner_id,
            &capability.issuer_state_hash,
            &capability.parent_capability_id,
        ] {
            counted(&mut expected, field);
        }
        expected.extend_from_slice(&subject.kind.to_be_bytes());
        counted(&mut expected, &subject.principal_id);
        expected.push(1);
        expected.extend_from_slice(&key.algorithm.to_be_bytes());
        counted(&mut expected, &key.public_key);
        expected.extend_from_slice(&1u32.to_be_bytes());
        counted(&mut expected, &selector.root_spool_uuid);
        expected.extend_from_slice(&(selector.path_segments.len() as u32).to_be_bytes());
        for segment in &selector.path_segments {
            counted(&mut expected, segment.as_bytes());
        }
        expected.push(0);
        expected
            .extend_from_slice(&(SpoolCapabilityAction::AcceptTimelineOrigin as i32).to_be_bytes());
        expected.push(1);
        counted(&mut expected, &scope.principal_account_uuid);
        match scope
            .credential_identity
            .as_ref()
            .and_then(|identity| identity.identity.as_ref())
            .expect("identity")
        {
            Identity::ServerIssued(value) => {
                expected.push(1);
                counted(&mut expected, &value.credential_id);
            }
            Identity::OfflineDerived(value) => {
                expected.push(2);
                counted(&mut expected, &value.issued_ancestor_credential_id);
                counted(&mut expected, &value.terminal_revocation_id);
                counted(&mut expected, &value.derivation_path_sha256);
            }
        }
        counted(&mut expected, &scope.effective_pop_key_sha256);
        expected.extend_from_slice(&scope.credential_class.to_be_bytes());
        counted(&mut expected, &scope.thread_id);
        counted(&mut expected, &scope.origin_sha256);
        expected.extend_from_slice(&capability.not_before_unix_seconds.to_be_bytes());
        expected.extend_from_slice(&capability.expires_at_unix_seconds.to_be_bytes());
        counted(&mut expected, &capability.nonce);
        assert_eq!(
            capability_without_id(capability).expect("canonical v3"),
            expected
        );
        assert_eq!(
            capability.capability_id,
            digest(OWNER_CAPABILITY_V3_DOMAIN, &expected)
        );
    }
}

struct TimelineFixture {
    bundle: OwnerAuthorizationBundle,
    origin: TimelineOriginEndorsement,
    acceptance: TimelineAdmissionAcceptance,
    state_hash: [u8; 32],
    owner: TestKey,
    subject: TestKey,
}

fn timeline_subject_biscuit(capability: &OwnerCapability, signer: &TestKey) -> Vec<u8> {
    let subject = capability.subject.as_ref().expect("subject");
    let key = subject.key.as_ref().expect("subject key");
    let grant = &capability.grants[0];
    let selector = grant.spool.as_ref().expect("selector");
    let scope = grant.timeline_acceptance.as_ref().expect("scope");
    let mut builder = Biscuit::builder()
        .fact(
            format!(
                "owner_subject({}, \"{}\", \"{}\")",
                subject.kind,
                hex::encode(&subject.principal_id),
                hex::encode(key_id(key))
            )
            .as_str(),
        )
        .expect("subject fact")
        .fact(
            format!(
                "owner_capability(\"{}\")",
                hex::encode(&capability.capability_id)
            )
            .as_str(),
        )
        .expect("capability fact")
        .fact(
            format!(
                "owner_validity({}, {})",
                capability.not_before_unix_seconds, capability.expires_at_unix_seconds
            )
            .as_str(),
        )
        .expect("validity fact");
    let prefix = format!(
        "\"{}\", \"{}\", \"{}\", ",
        hex::encode(&selector.root_spool_uuid),
        path_hex(&selector.path_segments),
        hex::encode(&scope.principal_account_uuid)
    );
    let suffix = format!(
        "\"{}\", {}, \"{}\", \"{}\")",
        hex::encode(&scope.effective_pop_key_sha256),
        scope.credential_class,
        hex::encode(&scope.thread_id),
        hex::encode(&scope.origin_sha256)
    );
    let fact = match scope
        .credential_identity
        .as_ref()
        .and_then(|identity| identity.identity.as_ref())
        .expect("identity")
    {
        Identity::ServerIssued(value) => format!(
            "owner_timeline_accept_server({prefix}\"{}\", {suffix}",
            hex::encode(&value.credential_id)
        ),
        Identity::OfflineDerived(value) => format!(
            "owner_timeline_accept_offline({prefix}\"{}\", \"{}\", \"{}\", {suffix}",
            hex::encode(&value.issued_ancestor_credential_id),
            hex::encode(&value.terminal_revocation_id),
            hex::encode(&value.derivation_path_sha256)
        ),
    };
    builder = builder.fact(fact.as_str()).expect("timeline fact");
    let private = PrivateKey::from_bytes(&signer.seed, Algorithm::Ed25519).expect("Biscuit key");
    let next_private =
        PrivateKey::from_bytes(&[0x55; 32], Algorithm::Ed25519).expect("next Biscuit key");
    builder
        .build_v1_with_key_pair(&KeyPair::from(&private), &KeyPair::from(&next_private))
        .expect("subject Biscuit root")
        .to_vec()
        .expect("subject Biscuit")
}

impl TimelineFixture {
    fn new() -> Self {
        let owner = TestKey::new(1);
        let paper = TestKey::new(2);
        let social = TestKey::new(3);
        let subject = TestKey::new(4);
        let root = signed_root(
            OWNER_UUID,
            &owner,
            &[
                (&paper, RecoveryGuardianKind::Paper),
                (&social, RecoveryGuardianKind::Social),
            ],
        );
        let state = verify_owner_root(&root).expect("owner state");
        let origin = TimelineOriginEndorsement {
            deployment_public_key: vec![5; 32],
            spool_id: SPOOL_ID.into(),
            thread_id: vec![6; 32],
            run_id: "run_7".into(),
            principal_id: PRINCIPAL.into(),
            credential_class: TimelineOriginCredentialClass::Agent as i32,
            effective_pop_key_sha256: vec![7; 32],
            credential_identity: Some(TimelineOriginCredentialIdentity {
                identity: Some(Identity::ServerIssued(TimelineServerIssuedCredential {
                    credential_id: b"credential-1".to_vec(),
                })),
            }),
            uploader_device_public_key: vec![8; 32],
            signature: vec![9; 64],
        };
        let scope = TimelineAcceptanceScope {
            principal_account_uuid: OWNER_UUID.to_vec(),
            credential_identity: origin.credential_identity.clone(),
            effective_pop_key_sha256: origin.effective_pop_key_sha256.clone(),
            credential_class: origin.credential_class as u32,
            thread_id: origin.thread_id.clone(),
            origin_sha256: heddle_api::timeline_upload::origin_digest(&origin)
                .expect("origin digest")
                .to_vec(),
        };
        let capability = OwnerCapability {
            format_version: 3,
            owner_id: state.owner_id().to_vec(),
            issuer_state_hash: state.state_hash().to_vec(),
            parent_capability_id: Vec::new(),
            subject: Some(CapabilityPrincipal {
                kind: CapabilityPrincipalKind::Agent as i32,
                principal_id: b"accepting-agent".to_vec(),
                key: Some(subject.wire()),
            }),
            grants: vec![SpoolCapabilityGrant {
                spool: Some(SpoolSelector {
                    root_spool_uuid: SPOOL.to_vec(),
                    path_segments: path(),
                    include_descendants: false,
                }),
                action: SpoolCapabilityAction::AcceptTimelineOrigin as i32,
                timeline_acceptance: Some(scope),
            }],
            not_before_unix_seconds: NOW - 1,
            expires_at_unix_seconds: NOW + 1000,
            nonce: vec![10; 32],
            capability_id: Vec::new(),
        };
        let mut fixture = Self {
            bundle: OwnerAuthorizationBundle {
                owner_root: Some(root),
                owner_state_chain: Vec::new(),
                capability_chain: vec![SignedOwnerCapability {
                    capability: Some(capability),
                    signature: None,
                }],
                subject_biscuit: Vec::new(),
            },
            origin,
            acceptance: TimelineAdmissionAcceptance {
                origin_sha256: Vec::new(),
                uploader_device_public_key: Vec::new(),
                deployment_public_key: Vec::new(),
                request_sha256: vec![11; 32],
                first_position: 4,
                event_count: 1,
                authority: None,
                signature: Vec::new(),
            },
            state_hash: state.state_hash(),
            owner,
            subject,
        };
        fixture.resign();
        fixture
    }

    fn capability_mut(&mut self) -> &mut OwnerCapability {
        self.bundle.capability_chain[0]
            .capability
            .as_mut()
            .expect("capability")
    }

    fn use_offline_identity(&mut self) {
        use crate::wire::TimelineOfflineDerivedCredential;
        self.origin.credential_identity = Some(TimelineOriginCredentialIdentity {
            identity: Some(Identity::OfflineDerived(TimelineOfflineDerivedCredential {
                issued_ancestor_credential_id: b"issued-ancestor".to_vec(),
                terminal_revocation_id: vec![13; 64],
                derivation_path_sha256: vec![14; 32],
            })),
        });
        let origin_digest = heddle_api::timeline_upload::origin_digest(&self.origin)
            .expect("offline origin digest");
        let identity = self.origin.credential_identity.clone();
        let scope = self.capability_mut().grants[0]
            .timeline_acceptance
            .as_mut()
            .expect("scope");
        scope.credential_identity = identity;
        scope.origin_sha256 = origin_digest.to_vec();
        self.resign();
    }

    fn use_signed_purge_action_in_v3(&mut self) {
        fn change_action(bytes: &mut [u8]) {
            let marker = [0, 0, 0, 2, 1, 0, 0, 0, 16];
            let offset = bytes
                .windows(marker.len())
                .position(|window| window == marker)
                .expect("v3 action and scope marker");
            bytes[offset + 3] = 1;
        }
        let mut capability = self.capability_mut().clone();
        let mut without_id = capability_without_id(&capability).expect("v3 body without ID");
        change_action(&mut without_id);
        capability.capability_id = digest(OWNER_CAPABILITY_V3_DOMAIN, &without_id).to_vec();
        let mut with_id = capability_body(&capability).expect("v3 body with ID");
        change_action(&mut with_id);
        capability.grants[0].action = SpoolCapabilityAction::Purge as i32;
        self.bundle.capability_chain[0].signature =
            Some(self.owner.sign(OWNER_CAPABILITY_V3_DOMAIN, &with_id));
        self.bundle.subject_biscuit = timeline_subject_biscuit(&capability, &self.subject);
        self.bundle.capability_chain[0].capability = Some(capability);
        self.acceptance.authority = Some(Authority::OwnerDerivedCapability(
            self.bundle.encode_to_vec(),
        ));
        self.acceptance.signature = self
            .subject
            .signing
            .sign(
                &heddle_api::timeline_upload::acceptance_signing_bytes(&self.acceptance)
                    .expect("acceptance bytes"),
            )
            .to_bytes()
            .to_vec();
    }

    fn resign(&mut self) {
        let body = {
            let capability = self.capability_mut();
            capability.capability_id = digest(
                OWNER_CAPABILITY_V3_DOMAIN,
                &capability_without_id(capability).expect("canonical v3 body"),
            )
            .to_vec();
            capability_body(capability).expect("signed v3 body")
        };
        let signature = self.owner.sign(OWNER_CAPABILITY_V3_DOMAIN, &body);
        self.bundle.capability_chain[0].signature = Some(signature);
        self.bundle.subject_biscuit = timeline_subject_biscuit(
            self.bundle.capability_chain[0]
                .capability
                .as_ref()
                .expect("capability"),
            &self.subject,
        );
        self.acceptance.origin_sha256 = heddle_api::timeline_upload::origin_digest(&self.origin)
            .expect("origin digest")
            .to_vec();
        self.acceptance.uploader_device_public_key = self.origin.uploader_device_public_key.clone();
        self.acceptance.deployment_public_key = self.origin.deployment_public_key.clone();
        self.refresh_acceptance_authority();
    }

    fn refresh_acceptance_authority(&mut self) {
        self.acceptance.authority = Some(Authority::OwnerDerivedCapability(
            self.bundle.encode_to_vec(),
        ));
        self.acceptance.signature = self
            .subject
            .signing
            .sign(
                &heddle_api::timeline_upload::acceptance_signing_bytes(&self.acceptance)
                    .expect("acceptance bytes"),
            )
            .to_bytes()
            .to_vec();
    }

    fn attenuate_subject(&mut self) {
        use biscuit_auth::{PublicKey, builder::BlockBuilder};
        let subject = self.bundle.capability_chain[0]
            .capability
            .as_ref()
            .expect("capability")
            .subject
            .as_ref()
            .expect("subject");
        let key = subject.key.as_ref().expect("key");
        let public =
            PublicKey::from_bytes(&key.public_key, Algorithm::Ed25519).expect("public key");
        let biscuit = heddle_biscuit_verifier::signature_v1::verify(
            self.bundle.subject_biscuit.as_slice(),
            move |_| Ok(public),
        )
        .expect("subject Biscuit");
        let next_private =
            PrivateKey::from_bytes(&[0x56; 32], Algorithm::Ed25519).expect("next Biscuit key");
        self.bundle.subject_biscuit = biscuit
            .append_with_keypair(&KeyPair::from(&next_private), BlockBuilder::new())
            .expect("attenuation")
            .to_vec()
            .expect("attenuated bytes");
        self.refresh_acceptance_authority();
    }

    fn subject_revocation_id(&self) -> Vec<u8> {
        use biscuit_auth::PublicKey;
        let subject = self.bundle.capability_chain[0]
            .capability
            .as_ref()
            .expect("capability")
            .subject
            .as_ref()
            .expect("subject");
        let key = subject.key.as_ref().expect("key");
        let public =
            PublicKey::from_bytes(&key.public_key, Algorithm::Ed25519).expect("public key");
        heddle_biscuit_verifier::signature_v1::verify(
            self.bundle.subject_biscuit.as_slice(),
            move |_| Ok(public),
        )
        .expect("subject Biscuit")
        .revocation_identifiers()[0]
            .to_vec()
    }

    fn verify(
        &self,
        now: i64,
        revoked_capability_ids: &[Vec<u8>],
        revoked_subject_ids: &[Vec<u8>],
    ) -> Result<VerifiedAuthorizationBundle> {
        verify_timeline_acceptance(
            &self.origin,
            &self.acceptance,
            &TimelineAcceptanceContext {
                accepted_state_hash: &self.state_hash,
                spool_path_segments: &path(),
                request_sha256: &[11; 32],
                first_position: 4,
                event_count: 1,
                revoked_capability_ids,
                revoked_subject_ids,
                now_unix_seconds: now,
                limits: limits(),
            },
        )
    }
}

fn recovered_away_fixture(rotations: &[u8]) -> TimelineFixture {
    let mut fixture = TimelineFixture::new();
    let paper = TestKey::new(2);
    let social = TestKey::new(3);
    let guardians = [
        (&paper, RecoveryGuardianKind::Paper),
        (&social, RecoveryGuardianKind::Social),
    ];
    fixture.bundle.owner_root = Some(signed_root_with_policy(
        OWNER_UUID,
        &fixture.owner,
        &guardians,
        recovery_policy(&guardians, Some(1)),
    ));
    let mut state =
        verify_owner_root(fixture.bundle.owner_root.as_ref().expect("root")).expect("root state");
    fixture.capability_mut().owner_id = state.owner_id().to_vec();
    fixture.capability_mut().issuer_state_hash = state.state_hash().to_vec();
    let mut current_key = TestKey::new(1);
    for byte in rotations {
        let next_key = TestKey::new(*byte);
        let rotate = rotation(&state, &current_key, &next_key);
        state =
            apply_accepted_transition(&state, &rotate, NOW, limits()).expect("accepted rotation");
        fixture.bundle.owner_state_chain.push(rotate);
        current_key = next_key;
    }
    fixture.capability_mut().not_before_unix_seconds = NOW;
    fixture.resign();
    if !rotations.is_empty() {
        fixture.state_hash = state.state_hash();
        assert!(
            fixture.verify(NOW, &[], &[]).is_ok(),
            "rotation overlap control"
        );
    }
    let recovered_key = TestKey::new(20);
    let next_paper = TestKey::new(21);
    let next_social = TestKey::new(22);
    let recover = recovery_transition(
        &state,
        &[&paper, &social],
        &recovered_key,
        recovery_policy(
            &[
                (&next_paper, RecoveryGuardianKind::Paper),
                (&next_social, RecoveryGuardianKind::Social),
            ],
            Some(1),
        ),
        NOW,
        &[&next_paper, &next_social],
    );
    verify_transition_timelock(&state, &recover, NOW - 1).expect("recovery veto window elapsed");
    state = apply_accepted_transition(&state, &recover, NOW, limits()).expect("accepted recovery");
    fixture.bundle.owner_state_chain.push(recover);
    fixture.state_hash = state.state_hash();
    // The historical key signs a new grant after recovery has been accepted.
    fixture.resign();
    fixture
}

fn long_owner_history_fixture() -> TimelineFixture {
    let mut fixture = TimelineFixture::new();
    let paper = TestKey::new(2);
    let social = TestKey::new(3);
    let mut state =
        verify_owner_root(fixture.bundle.owner_root.as_ref().expect("root")).expect("root state");
    for seed in 30..50 {
        let next = TestKey::new(seed);
        let rotate = rotation(&state, &fixture.owner, &next);
        state = apply_accepted_transition(&state, &rotate, NOW, limits()).expect("rotation");
        fixture.bundle.owner_state_chain.push(rotate);
        fixture.owner = next;
    }
    let recovered = TestKey::new(50);
    let next_paper = TestKey::new(51);
    let next_social = TestKey::new(52);
    let recover = recovery_transition(
        &state,
        &[&paper, &social],
        &recovered,
        recovery_policy(
            &[
                (&next_paper, RecoveryGuardianKind::Paper),
                (&next_social, RecoveryGuardianKind::Social),
            ],
            None,
        ),
        NOW,
        &[&next_paper, &next_social],
    );
    verify_transition_timelock(&state, &recover, NOW - 604_800).expect("recovery window elapsed");
    state = apply_accepted_transition(&state, &recover, NOW, limits()).expect("recovery");
    fixture.bundle.owner_state_chain.push(recover);
    fixture.owner = recovered;
    fixture.state_hash = state.state_hash();
    fixture.capability_mut().issuer_state_hash = state.state_hash().to_vec();
    fixture.capability_mut().not_before_unix_seconds = NOW;
    fixture.resign();
    fixture
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn timeline_acceptance_with_twenty_rotations_and_recovery() {
    let fixture = long_owner_history_fixture();
    assert_eq!(fixture.bundle.owner_state_chain.len(), 21);
    assert!(fixture.bundle.encoded_len() > 8192);
    assert!(fixture.bundle.encoded_len() < 65_536);
    let verified = fixture
        .verify(NOW, &[], &[])
        .expect("fresh recovered-owner acceptance");
    assert_eq!(
        verified.capability().capability().issuer_state_hash,
        fixture.state_hash
    );
    #[cfg(target_arch = "wasm32")]
    assert!(
        crate::wasm::verify_timeline_acceptance_binding(
            &fixture.origin.encode_to_vec(),
            &fixture.acceptance.encode_to_vec(),
            &fixture.state_hash,
            path(),
            &[11; 32],
            4_u64.into(),
            1,
            vec![],
            vec![],
            NOW.into(),
            3600_i64.into(),
        )
        .expect("WASM acceptance binding")
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn timeline_acceptance_rejects_bundle_over_64_kib() {
    let fixture = oversized_bundle_fixture();
    assert!(
        matches!(fixture.verify(NOW, &[], &[]), Err(Error::Invalid(reason)) if reason.contains("acceptance capability"))
    );
    #[cfg(target_arch = "wasm32")]
    assert!(
        !crate::wasm::verify_timeline_acceptance_binding(
            &fixture.origin.encode_to_vec(),
            &fixture.acceptance.encode_to_vec(),
            &fixture.state_hash,
            path(),
            &[11; 32],
            4_u64.into(),
            1,
            vec![],
            vec![],
            NOW.into(),
            3600_i64.into(),
        )
        .expect("WASM acceptance binding")
    );
}

fn oversized_bundle_fixture() -> TimelineFixture {
    let mut fixture = TimelineFixture::new();
    fixture.bundle.subject_biscuit.resize(65_537, 0);
    let overhead = fixture.bundle.encoded_len() - 65_537;
    fixture.bundle.subject_biscuit.truncate(65_537 - overhead);
    let bytes = fixture.bundle.encode_to_vec();
    assert_eq!(bytes.len(), 65_537);
    fixture.acceptance.authority = Some(Authority::OwnerDerivedCapability(bytes));
    fixture
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn producer_layout_round_trips_through_format_three_verifier() {
    let fixture = TimelineFixture::new();
    assert_eq!(
        fixture
            .verify(NOW, &[], &[])
            .expect("v3 acceptance")
            .capability()
            .capability()
            .format_version,
        3
    );
    assert!(
        verify_authorization_bundle_for_state(&fixture.bundle, &fixture.state_hash, NOW, limits())
            .is_err()
    );
    let mut offline = TimelineFixture::new();
    offline.use_offline_identity();
    assert!(
        offline.verify(NOW, &[], &[]).is_ok(),
        "offline-derived grant uses its distinct transcript and fact"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn timeline_scope_action_format_lifetime_and_revocations_fail_closed() {
    let mut fixture = TimelineFixture::new();
    fixture.capability_mut().grants[0]
        .timeline_acceptance
        .as_mut()
        .expect("scope")
        .thread_id[0] ^= 1;
    fixture.resign();
    assert!(fixture.verify(NOW, &[], &[]).is_err(), "wrong Thread");
    let mut fixture = TimelineFixture::new();
    fixture.capability_mut().grants[0]
        .timeline_acceptance
        .as_mut()
        .expect("scope")
        .origin_sha256[0] ^= 1;
    fixture.resign();
    assert!(fixture.verify(NOW, &[], &[]).is_err(), "wrong origin");
    let mut fixture = TimelineFixture::new();
    fixture.capability_mut().grants[0]
        .timeline_acceptance
        .as_mut()
        .expect("scope")
        .principal_account_uuid[0] ^= 1;
    fixture.resign();
    assert!(
        fixture.verify(NOW, &[], &[]).is_err(),
        "wrong principal scope"
    );
    let mut fixture = TimelineFixture::new();
    if let Some(Identity::ServerIssued(issued)) = fixture.capability_mut().grants[0]
        .timeline_acceptance
        .as_mut()
        .expect("scope")
        .credential_identity
        .as_mut()
        .and_then(|identity| identity.identity.as_mut())
    {
        issued.credential_id[0] ^= 1;
    }
    fixture.resign();
    assert!(
        fixture.verify(NOW, &[], &[]).is_err(),
        "wrong credential identity"
    );
    let mut fixture = TimelineFixture::new();
    fixture.capability_mut().grants[0]
        .timeline_acceptance
        .as_mut()
        .expect("scope")
        .effective_pop_key_sha256[0] ^= 1;
    fixture.resign();
    assert!(
        fixture.verify(NOW, &[], &[]).is_err(),
        "wrong effective key digest"
    );
    let mut fixture = TimelineFixture::new();
    fixture.capability_mut().grants[0]
        .timeline_acceptance
        .as_mut()
        .expect("scope")
        .credential_class = 1;
    fixture.resign();
    assert!(
        fixture.verify(NOW, &[], &[]).is_err(),
        "wrong credential class"
    );
    let fixture = TimelineFixture::new();
    assert!(
        fixture.verify(NOW + 1001, &[], &[]).is_err(),
        "expired grant"
    );
    let id = fixture.bundle.capability_chain[0]
        .capability
        .as_ref()
        .expect("capability")
        .capability_id
        .clone();
    assert!(
        fixture.verify(NOW, &[id], &[]).is_err(),
        "revoked capability"
    );
    let mut fixture = TimelineFixture::new();
    fixture.capability_mut().format_version = 2;
    fixture.resign();
    assert!(fixture.verify(NOW, &[], &[]).is_err(), "format 2 action");
    let mut fixture = TimelineFixture::new();
    fixture.use_signed_purge_action_in_v3();
    assert!(fixture.verify(NOW, &[], &[]).is_err(), "format 3 purge");
    let mut fixture = TimelineFixture::new();
    fixture.capability_mut().grants[0]
        .spool
        .as_mut()
        .expect("selector")
        .root_spool_uuid[0] ^= 1;
    fixture.resign();
    assert!(fixture.verify(NOW, &[], &[]).is_err(), "wrong Spool");
    let mut fixture = TimelineFixture::new();
    fixture.capability_mut().grants[0]
        .spool
        .as_mut()
        .expect("selector")
        .path_segments[0] = "another".into();
    fixture.resign();
    assert!(fixture.verify(NOW, &[], &[]).is_err(), "wrong path");
    let mut fixture = TimelineFixture::new();
    fixture.acceptance.signature[0] ^= 1;
    assert!(
        fixture.verify(NOW, &[], &[]).is_err(),
        "wrong subject signature"
    );
    let mut fixture = TimelineFixture::new();
    fixture.acceptance.request_sha256[0] ^= 1;
    fixture.resign();
    assert!(
        fixture.verify(NOW, &[], &[]).is_err(),
        "wrong request digest"
    );
    let mut fixture = TimelineFixture::new();
    fixture.acceptance.first_position += 1;
    fixture.resign();
    assert!(
        fixture.verify(NOW, &[], &[]).is_err(),
        "wrong position range"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn current_owner_state_and_rotation_window_are_checked() {
    let mut fixture = TimelineFixture::new();
    let root_state =
        verify_owner_root(fixture.bundle.owner_root.as_ref().expect("root")).expect("state");
    let next = TestKey::new(12);
    let signed = rotation(&root_state, &fixture.owner, &next);
    let state =
        apply_accepted_transition(&root_state, &signed, NOW, limits()).expect("accepted rotation");
    fixture.bundle.owner_state_chain.push(signed);
    fixture.state_hash = state.state_hash();
    fixture.resign();
    assert!(
        fixture.verify(NOW + 100, &[], &[]).is_ok(),
        "old issuer remains valid inside window"
    );
    assert!(
        fixture.verify(NOW + 101, &[], &[]).is_err(),
        "retired issuer cannot authorize acceptance"
    );
    fixture.state_hash[0] ^= 1;
    assert!(fixture.verify(NOW, &[], &[]).is_err(), "unpinned state");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn recovery_and_subject_biscuit_revocation_or_attenuation_reject_acceptance() {
    let fixture = TimelineFixture::new();
    let revoked = fixture.subject_revocation_id();
    assert!(
        fixture.verify(NOW, &[], &[revoked]).is_err(),
        "revoked subject Biscuit"
    );
    let mut attenuated = TimelineFixture::new();
    attenuated.attenuate_subject();
    assert!(
        attenuated.verify(NOW, &[], &[]).is_err(),
        "attenuated subject Biscuit"
    );

    let recovered = recovered_away_fixture(&[]);
    assert!(
        recovered.verify(NOW, &[], &[]).is_err(),
        "recovery retires old issuer immediately"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn direct_grant_shape_validity_start_and_issuer_signature_are_checked() {
    let mut fixture = TimelineFixture::new();
    fixture.capability_mut().parent_capability_id = vec![17; 32];
    fixture.resign();
    assert!(
        fixture.verify(NOW, &[], &[]).is_err(),
        "parent capability forbidden"
    );
    let mut fixture = TimelineFixture::new();
    let grant = fixture.bundle.capability_chain[0]
        .capability
        .as_ref()
        .expect("capability")
        .grants[0]
        .clone();
    fixture.capability_mut().grants.push(grant);
    fixture.resign();
    assert!(
        fixture.verify(NOW, &[], &[]).is_err(),
        "duplicate timeline grant forbidden"
    );
    let mut fixture = TimelineFixture::new();
    fixture
        .bundle
        .capability_chain
        .push(fixture.bundle.capability_chain[0].clone());
    fixture.refresh_acceptance_authority();
    assert!(
        fixture.verify(NOW, &[], &[]).is_err(),
        "attenuated capability chain forbidden"
    );
    let mut fixture = TimelineFixture::new();
    fixture.capability_mut().not_before_unix_seconds = NOW + 1;
    fixture.resign();
    assert!(
        fixture.verify(NOW, &[], &[]).is_err(),
        "grant not yet valid"
    );
    let mut fixture = TimelineFixture::new();
    fixture.bundle.capability_chain[0]
        .signature
        .as_mut()
        .expect("issuer signature")
        .signature[0] ^= 1;
    fixture.refresh_acceptance_authority();
    assert!(
        fixture.verify(NOW, &[], &[]).is_err(),
        "wrong owner issuer signature"
    );
}

fn timeline_v3_fixture_json() -> String {
    fn case(
        name: &str,
        fixture: &TimelineFixture,
        expected_accept: bool,
        now: i64,
        revoked: Vec<String>,
        revoked_subjects: Vec<String>,
    ) -> serde_json::Value {
        serde_json::json!({
            "name": name, "expected_accept": expected_accept,
            "origin_hex": hex::encode(fixture.origin.encode_to_vec()),
            "acceptance_hex": hex::encode(fixture.acceptance.encode_to_vec()),
            "current_owner_state_hash_hex": hex::encode(fixture.state_hash),
            "spool_path_segments": path(), "request_sha256_hex": hex::encode([11; 32]),
            "first_position": 4, "event_count": 1,
            "revoked_capability_ids_hex": revoked, "revoked_subject_ids_hex": revoked_subjects,
            "now_unix_seconds": now,
        })
    }
    let valid = TimelineFixture::new();
    let mut offline = TimelineFixture::new();
    offline.use_offline_identity();
    let mut wrong_thread = TimelineFixture::new();
    wrong_thread.capability_mut().grants[0]
        .timeline_acceptance
        .as_mut()
        .expect("scope")
        .thread_id[0] ^= 1;
    wrong_thread.resign();
    let mut wrong_origin = TimelineFixture::new();
    wrong_origin.capability_mut().grants[0]
        .timeline_acceptance
        .as_mut()
        .expect("scope")
        .origin_sha256[0] ^= 1;
    wrong_origin.resign();
    let mut wrong_action = TimelineFixture::new();
    wrong_action.use_signed_purge_action_in_v3();
    let mut wrong_format = TimelineFixture::new();
    wrong_format.capability_mut().format_version = 2;
    wrong_format.resign();
    let recovered = recovered_away_fixture(&[]);
    let recovered_away = recovered_away_fixture(&[12]);
    let recovered_after_rotations = recovered_away_fixture(&[12, 13, 14]);
    let long_history = long_owner_history_fixture();
    let oversized = oversized_bundle_fixture();
    let mut retired = TimelineFixture::new();
    let root_state =
        verify_owner_root(retired.bundle.owner_root.as_ref().expect("root")).expect("state");
    let rotate = rotation(&root_state, &retired.owner, &TestKey::new(12));
    retired.state_hash = apply_accepted_transition(&root_state, &rotate, NOW, limits())
        .expect("rotation")
        .state_hash();
    retired.bundle.owner_state_chain.push(rotate);
    retired.refresh_acceptance_authority();
    let mut attenuated = TimelineFixture::new();
    attenuated.attenuate_subject();
    let revoked_subject = hex::encode(valid.subject_revocation_id());
    let revoked_id = hex::encode(
        &valid.bundle.capability_chain[0]
            .capability
            .as_ref()
            .expect("capability")
            .capability_id,
    );
    let fixture = serde_json::json!({
        "format_version": 3, "max_capability_ttl_seconds": 3600,
        "cases": [
            case("producer-layout-accepted", &valid, true, NOW, vec![], vec![]),
            case("offline-derived-accepted", &offline, true, NOW, vec![], vec![]),
            case("wrong-thread", &wrong_thread, false, NOW, vec![], vec![]),
            case("wrong-origin", &wrong_origin, false, NOW, vec![], vec![]),
            case("wrong-action", &wrong_action, false, NOW, vec![], vec![]),
            case("format-two-rejected", &wrong_format, false, NOW, vec![], vec![]),
            case("expired", &valid, false, NOW + 1001, vec![], vec![]),
            case("revoked", &valid, false, NOW, vec![revoked_id], vec![]),
            case("subject-revoked", &valid, false, NOW, vec![], vec![revoked_subject]),
            case("subject-attenuated", &attenuated, false, NOW, vec![], vec![]),
            case("recovered-root-issuer", &recovered, false, NOW, vec![], vec![]),
            case("recovered-away-issuer", &recovered_away, false, NOW, vec![], vec![]),
            case("recovered-after-three-rotations", &recovered_after_rotations, false, NOW, vec![], vec![]),
            case("twenty-rotations-and-recovery", &long_history, true, NOW, vec![], vec![]),
            case("bundle-over-64-kib", &oversized, false, NOW, vec![], vec![]),
            case("retired-issuer", &retired, false, NOW + 101, vec![], vec![]),
        ]
    });
    serde_json::to_string_pretty(&fixture).expect("fixture JSON") + "\n"
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
#[ignore = "maintainer-only fixture regeneration"]
fn print_timeline_v3_fixture_json() {
    println!("{}", timeline_v3_fixture_json());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn timeline_v3_parity_fixture_is_current() {
    let json = timeline_v3_fixture_json();
    assert_eq!(
        json,
        include_str!("../conformance/fixtures/timeline-v3.json")
    );
    let outcomes = crate::conformance::run_timeline_fixture(&json).expect("timeline corpus");
    assert!(outcomes.iter().all(|outcome| outcome.matches));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn review_recovery_retires_every_pre_recovery_issuer() {
    for rotations in [&[12][..], &[12, 13, 14][..]] {
        let fixture = recovered_away_fixture(rotations);
        assert!(
            fixture.verify(NOW, &[], &[]).is_err(),
            "recovery must retire every older issuer while rotation overlaps remain"
        );
    }
}
