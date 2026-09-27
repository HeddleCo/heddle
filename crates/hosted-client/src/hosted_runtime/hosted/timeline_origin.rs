//! Freeze the verified run principal's signing identity before a local run is published.
use anyhow::{Context, Result, ensure};
use api::heddle::api::v1alpha2::{
    AuthorizationKeyAlgorithm, OwnerAuthorizationBundle, RecordRef, SpoolCapabilityAction,
    ThreadRef, TimelineAdmissionAcceptance, TimelineOfflineDerivedCredential,
    TimelineOriginCredentialClass, TimelineOriginCredentialIdentity, TimelineOriginEndorsement,
    TimelineServerIssuedCredential, UploadScrubbedTimelineRequest,
    timeline_admission_acceptance::Authority, timeline_origin_credential_identity::Identity,
};
use crypto::{Ed25519Signer, Signer};
use prost::Message;
use sha2::{Digest, Sha256};

use super::{
    canonical_server_authority, credential::server_keys_match, descriptor_trust,
    resolve_hosted_credential,
};

/// The raw Biscuit is provenance evidence, not a field in the logical digest.
/// It stays private to the device outbox and is never printed.
pub struct PreparedTimelineOrigin {
    pub origin: TimelineOriginEndorsement,
    pub origin_credential_biscuit: Vec<u8>,
}

/// Resolve all inputs from pinned local authority. This performs no network I/O.
/// An unverified or unpinned run remains local instead of gaining hosted identity.
pub fn prepare_timeline_origin(
    thread: &ThreadRef,
    run: &RecordRef,
    principal_id: &str,
) -> Result<Option<PreparedTimelineOrigin>> {
    let home = repo::identity::heddle_home_dir();
    if !home.join("state/device-rpc/authority.bin").try_exists()? {
        return Ok(None);
    }
    let Some(device) = repo::identity::load_device(&repo::identity::device_identity_path())? else {
        return Ok(None);
    };
    let server =
        config::credentials::default_server()?.context("hosted run has no selected deployment")?;
    ensure!(
        server_keys_match(&device.server, &server),
        "uploader device belongs to another deployment"
    );
    let credential = resolve_hosted_credential(Some(&server))?;
    let Some(token) = credential.token else {
        return Ok(None);
    };
    let signer = Ed25519Signer::from_pem(
        credential
            .proof_key_pem
            .as_deref()
            .context("run principal proof key missing")?,
    )?;
    let uploader = Ed25519Signer::from_pem(&device.private_key_pem)?;
    ensure!(
        hex::encode(uploader.public_key()) == device.public_key,
        "uploader device key differs from its enrolled identity"
    );
    let root = biscuit_verifier::unverified_authority_device_pop_key(&token.id)?
        .context("run principal has no issued ancestor root")?;
    let parsed = biscuit_verifier::parse_token(&token.id, &[root])?;
    let now = chrono::Utc::now().timestamp();
    let authority = repo::device_authority::load(&home, now)?;
    authority.verify_mint_root(&root.to_bytes(), now)?;
    let inspected = biscuit_verifier::inspect_verified_credential(&parsed, &root)?;
    ensure!(
        inspected.expires_at_unix_seconds == 0 || inspected.expires_at_unix_seconds > now as u64,
        "run principal credential expired"
    );
    ensure!(
        !inspected
            .revocation_ids
            .iter()
            .any(|id| authority.revoked_ids.contains(id)),
        "run principal credential revoked"
    );
    authority.verify_publisher(&inspected.proof_public_key)?;
    ensure!(
        signer.public_key() == inspected.proof_public_key,
        "run principal proof key differs from the verified chain"
    );
    let account = authority
        .owner
        .owner
        .as_ref()
        .context("admitted run account missing")?
        .id
        .as_str();
    ensure!(
        account == principal_id
            && inspected
                .asserted_account
                .is_none_or(|asserted| asserted.to_string() == principal_id),
        "run principal differs from the admitted account"
    );
    let issued_id = inspected
        .credential_id
        .as_deref()
        .context("run principal has no issued credential identity")?;
    let canonical_server = canonical_server_authority(&server)?;
    let deployment_key = descriptor_trust::load_automatic_pin(&canonical_server)?
        .context("hosted deployment has no verified local identity pin")?
        .public_key_bytes()?;
    let path_ids: Vec<Vec<u8>> = parsed
        .revocation_identifiers()
        .iter()
        .map(|id| id.to_vec())
        .collect();
    let identity = credential_identity(issued_id.as_bytes(), &path_ids)?;
    let offline_derived = matches!(identity, Identity::OfflineDerived(_));
    let agent = offline_derived || inspected.agent_id.is_some();
    let spool = thread.spool.as_ref().context("run Thread has no Spool")?;
    ensure!(
        run.spool.as_ref() == Some(spool),
        "run and Thread Spools differ"
    );
    let mut origin = TimelineOriginEndorsement {
        deployment_public_key: deployment_key.to_vec(),
        spool_id: spool.id.clone(),
        thread_id: thread
            .id
            .as_ref()
            .context("run Thread has no ID")?
            .value
            .clone(),
        run_id: run.id.clone(),
        principal_id: principal_id.to_owned(),
        credential_class: if agent {
            TimelineOriginCredentialClass::Agent as i32
        } else {
            TimelineOriginCredentialClass::DirectHuman as i32
        },
        effective_pop_key_sha256: Sha256::digest(signer.public_key()).to_vec(),
        credential_identity: Some(TimelineOriginCredentialIdentity {
            identity: Some(identity),
        }),
        uploader_device_public_key: uploader.public_key().to_vec(),
        signature: Vec::new(),
    };
    origin.signature = signer.sign(&api::timeline_upload::origin_signing_bytes(&origin)?)?;
    api::timeline_upload::validate_origin(&origin)?;
    Ok(Some(PreparedTimelineOrigin {
        origin,
        origin_credential_biscuit: if offline_derived {
            parsed.to_vec()?
        } else {
            Vec::new()
        },
    }))
}

fn credential_identity(issued_id: &[u8], path_ids: &[Vec<u8>]) -> Result<Identity> {
    ensure!(
        !issued_id.is_empty() && issued_id.len() <= 128,
        "issued credential ID invalid"
    );
    match path_ids {
        [_] => Ok(Identity::ServerIssued(TimelineServerIssuedCredential {
            credential_id: issued_id.to_vec(),
        })),
        [_, _, ..] => Ok(Identity::OfflineDerived(TimelineOfflineDerivedCredential {
            issued_ancestor_credential_id: issued_id.to_vec(),
            terminal_revocation_id: path_ids
                .last()
                .context("offline origin has no terminal block")?
                .clone(),
            derivation_path_sha256: api::timeline_upload::derivation_path_sha256(path_ids)?
                .to_vec(),
        })),
        [] => anyhow::bail!("origin credential has no signed block"),
    }
}

/// The owner-derived v3 authority is an exact, current subject grant. Weft
/// verifies its signatures and pinned owner state at admission; this device
/// checks that its visible scope names precisely the stored original.
pub fn sign_owner_timeline_acceptance(
    request: &UploadScrubbedTimelineRequest,
    bundle: &OwnerAuthorizationBundle,
    subject: &Ed25519Signer,
) -> Result<TimelineAdmissionAcceptance> {
    let origin = request
        .origin
        .as_ref()
        .context("upload has no original endorsement")?;
    let signed = bundle.capability_chain.as_slice();
    ensure!(
        signed.len() == 1,
        "timeline acceptance requires one direct capability"
    );
    let capability = signed[0]
        .capability
        .as_ref()
        .context("owner capability missing")?;
    ensure!(
        capability.format_version == 3 && capability.parent_capability_id.is_empty(),
        "timeline acceptance requires direct owner capability format 3"
    );
    let root = bundle
        .owner_root
        .as_ref()
        .and_then(|signed| signed.root.as_ref())
        .context("owner root missing")?;
    ensure!(
        root.account_uuid == uuid::Uuid::parse_str(&origin.principal_id)?.as_bytes(),
        "owner root belongs to another principal"
    );
    ensure!(
        capability.owner_id == root.owner_id,
        "owner capability names another root"
    );
    let key = capability
        .subject
        .as_ref()
        .and_then(|subject| subject.key.as_ref())
        .context("owner capability subject key missing")?;
    ensure!(
        key.algorithm == AuthorizationKeyAlgorithm::Ed25519 as i32
            && key.public_key == subject.public_key(),
        "acceptance subject key differs from capability"
    );
    ensure!(
        !bundle.subject_biscuit.is_empty(),
        "owner capability subject proof missing"
    );
    let grants = capability.grants.as_slice();
    ensure!(
        grants.len() == 1,
        "timeline acceptance requires one exact grant"
    );
    let grant = &grants[0];
    let selector = grant
        .spool
        .as_ref()
        .context("timeline grant Spool selector missing")?;
    ensure!(
        !selector.include_descendants,
        "timeline acceptance grant is not exact"
    );
    ensure!(
        grant.action == SpoolCapabilityAction::AcceptTimelineOrigin as i32,
        "owner capability does not permit timeline acceptance"
    );
    let scope = grant
        .timeline_acceptance
        .as_ref()
        .context("timeline grant scope missing")?;
    let digest = api::timeline_upload::origin_digest(origin)?;
    ensure!(
        scope.principal_account_uuid == root.account_uuid
            && scope.credential_identity == origin.credential_identity
            && scope.effective_pop_key_sha256 == origin.effective_pop_key_sha256
            && scope.credential_class == origin.credential_class as u32
            && scope.thread_id == origin.thread_id
            && scope.origin_sha256 == digest,
        "owner timeline grant does not name this original"
    );
    let bytes = bundle.encode_to_vec();
    ensure!(
        (1..=4096).contains(&bytes.len()),
        "owner acceptance bundle exceeds contract bound"
    );
    let mut acceptance = TimelineAdmissionAcceptance {
        origin_sha256: digest.to_vec(),
        uploader_device_public_key: origin.uploader_device_public_key.clone(),
        deployment_public_key: origin.deployment_public_key.clone(),
        request_sha256: api::timeline_upload::logical_request_digest(
            request,
            i128::from(chrono::Utc::now().timestamp_micros()),
        )?
        .to_vec(),
        first_position: request.first_position,
        event_count: request.events.len() as u32,
        authority: Some(Authority::OwnerDerivedCapability(bytes)),
        signature: Vec::new(),
    };
    acceptance.signature = subject.sign(&api::timeline_upload::acceptance_signing_bytes(
        &acceptance,
    )?)?;
    api::timeline_upload::validate_acceptance(&acceptance)?;
    Ok(acceptance)
}

#[cfg(test)]
mod tests {
    use api::heddle::api::v1alpha2::{
        AuthorizationVerificationKey, CapabilityPrincipal, OwnerCapability, OwnerRoot,
        SignedOwnerCapability, SignedOwnerRoot, SpoolCapabilityGrant, SpoolRef, SpoolSelector,
        ThreadId, TimelineAcceptanceScope, UploadRunSummary, operation_record::State,
    };

    use super::*;

    fn acceptance_fixture() -> (
        UploadScrubbedTimelineRequest,
        OwnerAuthorizationBundle,
        Ed25519Signer,
    ) {
        let subject = Ed25519Signer::from_seed(&[3; 32]).expect("subject signer");
        let principal = uuid::Uuid::from_u128(3);
        let origin = TimelineOriginEndorsement {
            deployment_public_key: vec![4; 32],
            spool_id: uuid::Uuid::from_u128(1).to_string(),
            thread_id: vec![2; 32],
            run_id: "run_1".into(),
            principal_id: principal.to_string(),
            credential_class: TimelineOriginCredentialClass::Agent as i32,
            effective_pop_key_sha256: vec![5; 32],
            credential_identity: Some(TimelineOriginCredentialIdentity {
                identity: Some(Identity::OfflineDerived(TimelineOfflineDerivedCredential {
                    issued_ancestor_credential_id: b"issued".to_vec(),
                    terminal_revocation_id: vec![7; 64],
                    derivation_path_sha256: vec![8; 32],
                })),
            }),
            uploader_device_public_key: vec![6; 32],
            signature: vec![9; 64],
        };
        let spool = SpoolRef {
            id: origin.spool_id.clone(),
        };
        let request = UploadScrubbedTimelineRequest {
            client_operation_id: uuid::Uuid::from_u128(10).to_string(),
            thread: Some(ThreadRef {
                spool: Some(spool.clone()),
                id: Some(ThreadId {
                    value: origin.thread_id.clone(),
                }),
            }),
            run: Some(RecordRef {
                spool: Some(spool),
                id: origin.run_id.clone(),
            }),
            canonicalization_version: 1,
            run_revision: 1,
            snapshot: Some(UploadRunSummary {
                state: State::Running as i32,
                harness: "codex".into(),
            }),
            events: Vec::new(),
            origin: Some(origin.clone()),
            acceptance: None,
            first_position: 0,
            origin_credential_biscuit: vec![1],
        };
        let scope = TimelineAcceptanceScope {
            principal_account_uuid: principal.as_bytes().to_vec(),
            credential_identity: origin.credential_identity.clone(),
            effective_pop_key_sha256: origin.effective_pop_key_sha256.clone(),
            credential_class: origin.credential_class as u32,
            thread_id: origin.thread_id.clone(),
            origin_sha256: api::timeline_upload::origin_digest(&origin)
                .expect("origin digest")
                .to_vec(),
        };
        let bundle = OwnerAuthorizationBundle {
            owner_root: Some(SignedOwnerRoot {
                root: Some(OwnerRoot {
                    account_uuid: principal.as_bytes().to_vec(),
                    owner_id: vec![11; 32],
                    ..Default::default()
                }),
                ..Default::default()
            }),
            capability_chain: vec![SignedOwnerCapability {
                capability: Some(OwnerCapability {
                    format_version: 3,
                    owner_id: vec![11; 32],
                    subject: Some(CapabilityPrincipal {
                        key: Some(AuthorizationVerificationKey {
                            algorithm: AuthorizationKeyAlgorithm::Ed25519 as i32,
                            public_key: subject.public_key().to_vec(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    grants: vec![SpoolCapabilityGrant {
                        spool: Some(SpoolSelector {
                            root_spool_uuid: uuid::Uuid::from_u128(1).as_bytes().to_vec(),
                            ..Default::default()
                        }),
                        action: SpoolCapabilityAction::AcceptTimelineOrigin as i32,
                        timeline_acceptance: Some(scope),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            }],
            subject_biscuit: vec![1],
            ..Default::default()
        };
        (request, bundle, subject)
    }

    #[test]
    fn a_redelegates_as_b_produces_a_different_endorsement_identity() {
        let ancestor = vec![1; 64];
        let a = credential_identity(b"issued-ancestor", &[ancestor.clone(), vec![2; 64]])
            .expect("A's identity");
        let b = credential_identity(b"issued-ancestor", &[ancestor, vec![3; 64]])
            .expect("B's identity");
        assert_ne!(a, b);
        assert!(matches!(a, Identity::OfflineDerived(_)));
        assert!(matches!(b, Identity::OfflineDerived(_)));
    }

    #[test]
    fn unlabelled_proof_key_transfer_cannot_pose_as_direct_human() {
        let direct =
            credential_identity(b"issued-ancestor", &[vec![1; 64]]).expect("direct identity");
        let delegated = credential_identity(b"issued-ancestor", &[vec![1; 64], vec![2; 64]])
            .expect("unlabelled delegated identity");
        assert!(matches!(direct, Identity::ServerIssued(_)));
        assert!(matches!(delegated, Identity::OfflineDerived(_)));
        assert_ne!(direct, delegated);
    }

    #[test]
    fn owner_acceptance_signs_exact_original_request_and_v3_authority() {
        let (request, mut bundle, subject) = acceptance_fixture();
        let accepted =
            sign_owner_timeline_acceptance(&request, &bundle, &subject).expect("acceptance");
        let mut uploaded = request.clone();
        uploaded.acceptance = Some(accepted.clone());
        api::timeline_upload::validate_upload(
            &uploaded,
            i128::from(chrono::Utc::now().timestamp_micros()),
        )
        .expect("bound acceptance");
        assert_eq!(accepted.first_position, request.first_position);
        assert_eq!(accepted.event_count, 0);
        let mut expected = b"heddle-timeline-run-acceptance-v1\0".to_vec();
        for field in [
            &accepted.origin_sha256,
            &accepted.uploader_device_public_key,
            &accepted.deployment_public_key,
            &accepted.request_sha256,
        ] {
            expected.extend_from_slice(&(field.len() as u32).to_be_bytes());
            expected.extend_from_slice(field);
        }
        expected.extend_from_slice(&accepted.first_position.to_be_bytes());
        expected.extend_from_slice(&accepted.event_count.to_be_bytes());
        expected.push(2);
        let authority = bundle.encode_to_vec();
        expected.extend_from_slice(&(authority.len() as u32).to_be_bytes());
        expected.extend_from_slice(&authority);
        assert_eq!(
            api::timeline_upload::acceptance_signing_bytes(&accepted).expect("transcript"),
            expected
        );
        bundle.capability_chain[0]
            .capability
            .as_mut()
            .expect("capability")
            .grants[0]
            .timeline_acceptance
            .as_mut()
            .expect("scope")
            .thread_id[0] ^= 1;
        assert!(sign_owner_timeline_acceptance(&request, &bundle, &subject).is_err());
    }

    #[test]
    fn origin_endorsement_matches_contract_v3_golden_bytes() {
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
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("timeline-origin-v3.json"))
                .expect("contract golden transcript");
        assert_eq!(
            hex::encode(
                api::timeline_upload::origin_signing_bytes(&origin).expect("issued transcript")
            ),
            golden["server_issued"].as_str().expect("issued golden")
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
                api::timeline_upload::origin_signing_bytes(&origin).expect("derived transcript")
            ),
            golden["offline_derived"].as_str().expect("derived golden")
        );
    }
}
