#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use biscuit_auth::{Biscuit, KeyPair, PrivateKey, builder::Algorithm};
use ed25519_dalek::{Signer, SigningKey};
use prost::Message;
use sha2::{Digest, Sha256};

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test;

use super::*;
use crate::{
    canonical::{
        OWNER_CAPABILITY_DOMAIN, OWNER_ROOT_DOMAIN, OWNER_TRANSITION_DOMAIN,
        PURGE_OPERATION_DOMAIN, TRANSFER_ACCEPTANCE_DOMAIN, TRANSFER_HANDOFF_DOMAIN,
        capability_body, capability_without_id, digest, key_id, owner_root_body,
        owner_root_without_id, transfer_acceptance_body, transfer_handoff_body, transition_body,
    },
    crypto::verify_signature,
    wire::{
        AuthorizationKeyAlgorithm, AuthorizationSignature, AuthorizationVerificationKey,
        CapabilityPrincipal, CapabilityPrincipalKind, CloneAuthorizationKeyring, CloneOwnerPin,
        CloneOwnerPinKind, OwnerAuthorizationBundle, OwnerCapability, OwnerKeyTransition,
        OwnerKeyTransitionKind, OwnerRoot, PurgeOperationSigningBody, PurgeSidecarIdentity,
        RecoveryGuardian, RecoveryGuardianKind, RecoveryPolicy, ResourceOwnershipTransfer,
        ResourceTransferAcceptance, ResourceTransferHandoff, SidecarAuthorization,
        SignedOwnerCapability, SignedOwnerKeyTransition, SignedOwnerRoot,
        SignedResourceTransferHandoff, SignedSpoolOwnerGenesis, SpoolCapabilityAction,
        SpoolCapabilityGrant, SpoolOwnerGenesis, SpoolSelector,
    },
};

const NOW: i64 = 1_000_000;
const OWNER_UUID: [u8; 16] = [0x11; 16];
const SPOOL: [u8; 16] = [0x22; 16];
const OTHER_SPOOL: [u8; 16] = [0x23; 16];
const PATH: [&str; 2] = ["acme", "verifier"];
const PAYLOAD: &[u8] = b"canonical purge payload v2";

struct TestKey {
    seed: [u8; 32],
    signing: SigningKey,
}

impl TestKey {
    fn new(byte: u8) -> Self {
        let seed = [byte; 32];
        Self {
            seed,
            signing: SigningKey::from_bytes(&seed),
        }
    }

    fn wire(&self) -> AuthorizationVerificationKey {
        AuthorizationVerificationKey {
            algorithm: AuthorizationKeyAlgorithm::Ed25519 as i32,
            public_key: self.signing.verifying_key().to_bytes().to_vec(),
        }
    }

    fn sign(&self, domain: &[u8], body: &[u8]) -> AuthorizationSignature {
        self.sign_digest(&digest(domain, body))
    }

    fn sign_digest(&self, signed_digest: &[u8; 32]) -> AuthorizationSignature {
        AuthorizationSignature {
            signer_key_id: key_id(&self.wire()).to_vec(),
            signature: self.signing.sign(signed_digest).to_bytes().to_vec(),
        }
    }
}

fn limits() -> VerificationLimits {
    VerificationLimits::new(3_600).expect("test limits")
}

fn path() -> Vec<String> {
    PATH.iter().map(ToString::to_string).collect()
}

fn sorted_guardians(keys: &[(&TestKey, RecoveryGuardianKind)]) -> Vec<RecoveryGuardian> {
    let mut guardians = keys
        .iter()
        .map(|(key, kind)| RecoveryGuardian {
            kind: *kind as i32,
            key: Some(key.wire()),
        })
        .collect::<Vec<_>>();
    guardians.sort_by_key(|guardian| key_id(guardian.key.as_ref().expect("guardian key")));
    guardians
}

fn recovery_policy(
    keys: &[(&TestKey, RecoveryGuardianKind)],
    window_secs: Option<u64>,
) -> RecoveryPolicy {
    RecoveryPolicy {
        threshold: 2,
        guardians: sorted_guardians(keys),
        window_secs,
    }
}

fn signed_root_with_policy(
    owner_uuid: [u8; 16],
    authority: &TestKey,
    guardian_keys: &[(&TestKey, RecoveryGuardianKind)],
    policy: RecoveryPolicy,
) -> SignedOwnerRoot {
    let mut root = OwnerRoot {
        format_version: 1,
        owner_id: Vec::new(),
        account_uuid: owner_uuid.to_vec(),
        authority_key: Some(authority.wire()),
        recovery_policy: Some(policy),
        claimable_deferred_human: false,
        nonce: vec![0x31; 32],
        claimable_until_unix_seconds: 0,
    };
    root.owner_id = digest(
        OWNER_ROOT_DOMAIN,
        &owner_root_without_id(&root).expect("canonical root without id"),
    )
    .to_vec();
    let body = owner_root_body(&root).expect("canonical root");
    let recovery_key_proofs = root
        .recovery_policy
        .as_ref()
        .expect("policy")
        .guardians
        .iter()
        .map(|guardian| {
            let id = key_id(guardian.key.as_ref().expect("guardian key"));
            guardian_keys
                .iter()
                .find(|(key, _)| key_id(&key.wire()) == id)
                .expect("guardian signer")
                .0
                .sign(OWNER_ROOT_DOMAIN, &body)
        })
        .collect();
    SignedOwnerRoot {
        root: Some(root),
        authority_proof: Some(authority.sign(OWNER_ROOT_DOMAIN, &body)),
        recovery_key_proofs,
    }
}

fn signed_root(
    owner_uuid: [u8; 16],
    authority: &TestKey,
    guardian_keys: &[(&TestKey, RecoveryGuardianKind)],
) -> SignedOwnerRoot {
    signed_root_with_policy(
        owner_uuid,
        authority,
        guardian_keys,
        recovery_policy(guardian_keys, None),
    )
}

fn signed_genesis(spool_uuid: [u8; 16], owner: &TestKey) -> SignedSpoolOwnerGenesis {
    let signed_digest: [u8; 32] = Sha256::new()
        .chain_update(owner.wire().public_key)
        .chain_update(spool_uuid)
        .finalize()
        .into();
    SignedSpoolOwnerGenesis {
        genesis: Some(SpoolOwnerGenesis {
            spool_uuid: spool_uuid.to_vec(),
            owner_public_key: Some(owner.wire()),
        }),
        owner_signature: Some(owner.sign_digest(&signed_digest)),
    }
}

fn subject(key: &TestKey) -> CapabilityPrincipal {
    CapabilityPrincipal {
        kind: CapabilityPrincipalKind::Agent as i32,
        principal_id: b"purge-agent".to_vec(),
        key: Some(key.wire()),
    }
}

struct CapabilityArgs<'a> {
    state: &'a VerifiedOwnerState,
    issuer: &'a TestKey,
    subject: &'a TestKey,
    spool_uuid: [u8; 16],
    action: i32,
    parent_capability_id: Vec<u8>,
    not_before: i64,
    expires_at: i64,
    nonce: u8,
}

fn signed_capability(args: CapabilityArgs<'_>) -> SignedOwnerCapability {
    let mut capability = OwnerCapability {
        format_version: 1,
        owner_id: args.state.owner_id().to_vec(),
        issuer_state_hash: args.state.state_hash().to_vec(),
        parent_capability_id: args.parent_capability_id,
        subject: Some(subject(args.subject)),
        grants: vec![SpoolCapabilityGrant {
            spool: Some(SpoolSelector {
                root_spool_uuid: args.spool_uuid.to_vec(),
                path_segments: path(),
                include_descendants: false,
            }),
            action: args.action,
        }],
        not_before_unix_seconds: args.not_before,
        expires_at_unix_seconds: args.expires_at,
        nonce: vec![args.nonce; 32],
        capability_id: Vec::new(),
    };
    capability.capability_id = digest(
        OWNER_CAPABILITY_DOMAIN,
        &capability_without_id(&capability).expect("capability without id"),
    )
    .to_vec();
    let signature = args.issuer.sign(
        OWNER_CAPABILITY_DOMAIN,
        &capability_body(&capability).expect("capability body"),
    );
    SignedOwnerCapability {
        capability: Some(capability),
        signature: Some(signature),
    }
}

fn path_hex(segments: &[String]) -> String {
    let mut bytes = Vec::new();
    for segment in segments {
        bytes.extend_from_slice(&(segment.len() as u32).to_be_bytes());
        bytes.extend_from_slice(segment.as_bytes());
    }
    hex::encode(bytes)
}

fn subject_biscuit(capability: &OwnerCapability, signer: &TestKey) -> Vec<u8> {
    let principal = capability.subject.as_ref().expect("subject");
    let key = principal.key.as_ref().expect("subject key");
    let mut builder = Biscuit::builder()
        .fact(
            format!(
                "owner_subject({}, \"{}\", \"{}\")",
                principal.kind,
                hex::encode(&principal.principal_id),
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
    for grant in &capability.grants {
        let selector = grant.spool.as_ref().expect("selector");
        builder = builder
            .fact(
                format!(
                    "owner_grant(\"{}\", \"{}\", {}, {})",
                    hex::encode(&selector.root_spool_uuid),
                    path_hex(&selector.path_segments),
                    selector.include_descendants,
                    grant.action
                )
                .as_str(),
            )
            .expect("grant fact");
    }
    let private = PrivateKey::from_bytes(&signer.seed, Algorithm::Ed25519).expect("Biscuit key");
    builder
        .build(&KeyPair::from(&private))
        .and_then(|value| value.to_vec())
        .expect("subject Biscuit")
}

fn bundle(
    root: &SignedOwnerRoot,
    transitions: Vec<SignedOwnerKeyTransition>,
    capabilities: Vec<SignedOwnerCapability>,
    subject_key: &TestKey,
) -> OwnerAuthorizationBundle {
    let subject_biscuit = subject_biscuit(
        capabilities
            .last()
            .and_then(|signed| signed.capability.as_ref())
            .expect("leaf capability"),
        subject_key,
    );
    OwnerAuthorizationBundle {
        owner_root: Some(root.clone()),
        owner_state_chain: transitions,
        capability_chain: capabilities,
        subject_biscuit,
    }
}

fn purge_body(
    leaf_capability_id: &[u8],
    payload: &[u8],
    spool_uuid: [u8; 16],
) -> PurgeOperationSigningBody {
    PurgeOperationSigningBody {
        format_version: 2,
        spool_uuid: spool_uuid.to_vec(),
        purge_identity: Some(PurgeSidecarIdentity {
            blob_hash: "ab".repeat(32),
        }),
        payload_sha256: Sha256::digest(payload).to_vec(),
        leaf_capability_id: leaf_capability_id.to_vec(),
    }
}

#[derive(Clone)]
struct Artifact {
    authorization: SidecarAuthorization,
    body: PurgeOperationSigningBody,
    owner_genesis: SignedSpoolOwnerGenesis,
    current_state_hash: [u8; 32],
    spool_uuid: [u8; 16],
    path: Vec<String>,
    payload: Vec<u8>,
}

fn artifact_with(grant_spool: [u8; 16], action: i32, not_before: i64, expires_at: i64) -> Artifact {
    let authority = TestKey::new(1);
    let paper = TestKey::new(2);
    let social = TestKey::new(3);
    let subject_key = TestKey::new(5);
    let root = signed_root(
        OWNER_UUID,
        &authority,
        &[
            (&paper, RecoveryGuardianKind::Paper),
            (&social, RecoveryGuardianKind::Social),
        ],
    );
    let state = verify_owner_root(&root).expect("verified root");
    artifact_for_state(
        &root,
        signed_genesis(SPOOL, &authority),
        Vec::new(),
        &state,
        &authority,
        &subject_key,
        grant_spool,
        action,
        not_before,
        expires_at,
    )
}

#[allow(clippy::too_many_arguments)]
fn artifact_for_state(
    root: &SignedOwnerRoot,
    owner_genesis: SignedSpoolOwnerGenesis,
    transitions: Vec<SignedOwnerKeyTransition>,
    state: &VerifiedOwnerState,
    issuer: &TestKey,
    subject_key: &TestKey,
    grant_spool: [u8; 16],
    action: i32,
    not_before: i64,
    expires_at: i64,
) -> Artifact {
    let capability = signed_capability(CapabilityArgs {
        state,
        issuer,
        subject: subject_key,
        spool_uuid: grant_spool,
        action,
        parent_capability_id: Vec::new(),
        not_before,
        expires_at,
        nonce: 0x61,
    });
    let leaf_id = capability
        .capability
        .as_ref()
        .expect("capability")
        .capability_id
        .clone();
    let body = purge_body(&leaf_id, PAYLOAD, SPOOL);
    let authorization = SidecarAuthorization {
        capability: Some(bundle(root, transitions, vec![capability], subject_key)),
        operation_signature: Some(subject_key.sign(
            PURGE_OPERATION_DOMAIN,
            &canonical_purge_operation(&body).expect("purge operation body"),
        )),
    };
    Artifact {
        authorization,
        body,
        owner_genesis,
        current_state_hash: state.state_hash(),
        spool_uuid: SPOOL,
        path: path(),
        payload: PAYLOAD.to_vec(),
    }
}

fn artifact() -> Artifact {
    artifact_with(
        SPOOL,
        SpoolCapabilityAction::Purge as i32,
        NOW - 10,
        NOW + 200,
    )
}

fn decide(value: &Artifact) -> Decision {
    verify_purge_authorization(
        &value.authorization,
        &value.body,
        &value.payload,
        &PurgeContext {
            owner_genesis: &value.owner_genesis,
            current_owner_state_hash: &value.current_state_hash,
            spool_uuid: &value.spool_uuid,
            spool_path_segments: &value.path,
            now_unix_seconds: NOW,
            limits: limits(),
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn transition(
    state: &VerifiedOwnerState,
    kind: OwnerKeyTransitionKind,
    next_authority: &TestKey,
    next_policy: RecoveryPolicy,
    sequence: u64,
    previous_state_hash: Vec<u8>,
    valid_from: i64,
    previous_key_valid_until: i64,
) -> OwnerKeyTransition {
    OwnerKeyTransition {
        format_version: 1,
        owner_id: state.owner_id().to_vec(),
        previous_state_hash,
        sequence,
        kind: kind as i32,
        next_authority_key: Some(next_authority.wire()),
        next_recovery_policy: Some(next_policy),
        valid_from_unix_seconds: valid_from,
        previous_key_valid_until_unix_seconds: previous_key_valid_until,
        nonce: vec![0x71; 32],
    }
}

fn rotation(
    state: &VerifiedOwnerState,
    current: &TestKey,
    next: &TestKey,
) -> SignedOwnerKeyTransition {
    let transition = transition(
        state,
        OwnerKeyTransitionKind::Rotate,
        next,
        state.recovery_policy().clone(),
        state.sequence() + 1,
        state.state_hash().to_vec(),
        NOW - 1,
        NOW + 100,
    );
    let body = transition_body(&transition).expect("transition body");
    SignedOwnerKeyTransition {
        transition: Some(transition),
        authorizations: vec![current.sign(OWNER_TRANSITION_DOMAIN, &body)],
        next_authority_key_proof: Some(next.sign(OWNER_TRANSITION_DOMAIN, &body)),
        next_recovery_key_proofs: Vec::new(),
    }
}

#[derive(Clone, Copy)]
enum BrokenTransition {
    Gap,
    Fork,
    WrongSigner,
}

fn transition_artifact(broken: Option<BrokenTransition>) -> Artifact {
    let authority = TestKey::new(1);
    let paper = TestKey::new(2);
    let social = TestKey::new(3);
    let next = TestKey::new(7);
    let rogue = TestKey::new(8);
    let subject_key = TestKey::new(5);
    let root = signed_root(
        OWNER_UUID,
        &authority,
        &[
            (&paper, RecoveryGuardianKind::Paper),
            (&social, RecoveryGuardianKind::Social),
        ],
    );
    let root_state = verify_owner_root(&root).expect("root state");
    let mut signed = rotation(&root_state, &authority, &next);
    match broken {
        Some(BrokenTransition::Gap) => {
            let body = transition(
                &root_state,
                OwnerKeyTransitionKind::Rotate,
                &next,
                root_state.recovery_policy().clone(),
                2,
                root_state.state_hash().to_vec(),
                NOW - 1,
                NOW + 100,
            );
            let canonical = transition_body(&body).expect("gap transition body");
            signed = SignedOwnerKeyTransition {
                transition: Some(body),
                authorizations: vec![authority.sign(OWNER_TRANSITION_DOMAIN, &canonical)],
                next_authority_key_proof: Some(next.sign(OWNER_TRANSITION_DOMAIN, &canonical)),
                next_recovery_key_proofs: Vec::new(),
            };
        }
        Some(BrokenTransition::WrongSigner) => {
            let body = transition_body(signed.transition.as_ref().expect("transition"))
                .expect("transition body");
            signed.authorizations = vec![rogue.sign(OWNER_TRANSITION_DOMAIN, &body)];
        }
        Some(BrokenTransition::Fork) | None => {}
    }
    let next_state = apply_transition(
        &root_state,
        &rotation(&root_state, &authority, &next),
        NOW,
        limits(),
    )
    .expect("reference next state");
    let transitions = if matches!(broken, Some(BrokenTransition::Fork)) {
        vec![signed.clone(), signed]
    } else {
        vec![signed]
    };
    artifact_for_state(
        &root,
        signed_genesis(SPOOL, &authority),
        transitions,
        &next_state,
        &next,
        &subject_key,
        SPOOL,
        SpoolCapabilityAction::Purge as i32,
        NOW - 10,
        NOW + 200,
    )
}

fn assert_case(name: &str, actual: Decision, expected: Decision) {
    assert_eq!(actual, expected, "conformance case {name}");
    println!("CASE name={name} expected={expected:?} actual={actual:?}");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn purge_accept_deny_matrix() {
    assert_case("owner-anchored-purge", decide(&artifact()), Decision::Purge);

    let mut absent = artifact();
    absent.authorization.capability = None;
    assert_case(
        "absent-capability",
        decide(&absent),
        Decision::Deny(Denial::Capability),
    );

    let mut invalid_signature = artifact();
    invalid_signature
        .authorization
        .operation_signature
        .as_mut()
        .expect("signature")
        .signature[0] ^= 1;
    assert_case(
        "invalid-signature",
        decide(&invalid_signature),
        Decision::Deny(Denial::InvalidProof),
    );

    let expired = artifact_with(
        SPOOL,
        SpoolCapabilityAction::Purge as i32,
        NOW - 200,
        NOW - 1,
    );
    assert_case("expired", decide(&expired), Decision::Deny(Denial::Time));

    let mut wrong_spool = artifact();
    wrong_spool.spool_uuid[0] ^= 1;
    assert_case(
        "wrong-spool",
        decide(&wrong_spool),
        Decision::Deny(Denial::OperationBinding),
    );

    let wrong_action = artifact_with(SPOOL, 0, NOW - 10, NOW + 200);
    assert_case(
        "wrong-action-non-purge",
        decide(&wrong_action),
        Decision::Deny(Denial::Capability),
    );

    let mut attenuated = artifact();
    let duplicate = attenuated
        .authorization
        .capability
        .as_ref()
        .expect("bundle")
        .capability_chain[0]
        .clone();
    attenuated
        .authorization
        .capability
        .as_mut()
        .expect("bundle")
        .capability_chain
        .push(duplicate);
    assert_case(
        "attenuation-forged",
        decide(&attenuated),
        Decision::Deny(Denial::DirectOnly),
    );

    let mut forged_genesis = artifact();
    forged_genesis
        .owner_genesis
        .owner_signature
        .as_mut()
        .expect("genesis signature")
        .signature[0] ^= 1;
    assert_case(
        "forged-genesis",
        decide(&forged_genesis),
        Decision::Deny(Denial::InvalidProof),
    );

    assert_case(
        "transition-gap",
        decide(&transition_artifact(Some(BrokenTransition::Gap))),
        Decision::Deny(Denial::InvalidProof),
    );
    assert_case(
        "transition-fork",
        decide(&transition_artifact(Some(BrokenTransition::Fork))),
        Decision::Deny(Denial::InvalidProof),
    );
    assert_case(
        "transition-wrong-signer",
        decide(&transition_artifact(Some(BrokenTransition::WrongSigner))),
        Decision::Deny(Denial::InvalidProof),
    );
    assert_case(
        "rotated-owner-purge",
        decide(&transition_artifact(None)),
        Decision::Purge,
    );

    let wrong_capability_spool = artifact_with(
        OTHER_SPOOL,
        SpoolCapabilityAction::Purge as i32,
        NOW - 10,
        NOW + 200,
    );
    assert_case(
        "capability-for-another-spool",
        decide(&wrong_capability_spool),
        Decision::Deny(Denial::Capability),
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn api_v2_operation_and_genesis_vectors_match() {
    let body = PurgeOperationSigningBody {
        format_version: 2,
        spool_uuid: hex::decode("0198b2d7c40070008000001122334455").expect("spool"),
        purge_identity: Some(PurgeSidecarIdentity {
            blob_hash: "ab".repeat(32),
        }),
        payload_sha256: hex::decode(
            "e3f64d21eed55b60133ac968886406c16a1036fbcbd88720ea9a0ea4b2f6fdcd",
        )
        .expect("payload hash"),
        leaf_capability_id: vec![0x55; 32],
    };
    assert_eq!(
        hex::encode(canonical_purge_operation(&body).expect("canonical operation")),
        "000000020198b2d7c400700080000011223344550000004061626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162616261626162e3f64d21eed55b60133ac968886406c16a1036fbcbd88720ea9a0ea4b2f6fdcd5555555555555555555555555555555555555555555555555555555555555555"
    );
    assert_eq!(
        hex::encode(digest(
            PURGE_OPERATION_DOMAIN,
            &canonical_purge_operation(&body).expect("canonical operation")
        )),
        "17d8c91d6933122aae5de9eb2a806bdb50ab8ba5b99fc3b8bdaf1f91670b0054"
    );

    let owner = TestKey::new(0x1f);
    assert_eq!(
        hex::encode(&owner.wire().public_key),
        "43046bfe4092b3e94994eada15dcc20d8aaa07b658fd3954eb8e0efb8bdca5de"
    );
    let genesis = signed_genesis(
        body.spool_uuid.as_slice().try_into().expect("spool UUID"),
        &owner,
    );
    assert_eq!(
        hex::encode(
            &genesis
                .owner_signature
                .as_ref()
                .expect("signature")
                .signature
        ),
        "f37d8edea82fdda544f8c338d34f3433512231421a0a39771d5cf33b454318bebd7011d7e192e6b548618f9f3aae4835c092af3f8a9a074206ff51046409fb0e"
    );
    let verified = verify_spool_owner_genesis(&genesis).expect("verified genesis vector");
    assert_eq!(verified.spool_uuid(), body.spool_uuid.as_slice());
}

fn recovery_transition(
    state: &VerifiedOwnerState,
    guardian_keys: &[&TestKey],
    next: &TestKey,
    next_policy: RecoveryPolicy,
    valid_from: i64,
) -> SignedOwnerKeyTransition {
    let transition = transition(
        state,
        OwnerKeyTransitionKind::Recover,
        next,
        next_policy,
        state.sequence() + 1,
        state.state_hash().to_vec(),
        valid_from,
        0,
    );
    let body = transition_body(&transition).expect("recovery body");
    let mut authorizations = guardian_keys
        .iter()
        .map(|key| key.sign(OWNER_TRANSITION_DOMAIN, &body))
        .collect::<Vec<_>>();
    authorizations.sort_by(|left, right| left.signer_key_id.cmp(&right.signer_key_id));
    SignedOwnerKeyTransition {
        transition: Some(transition),
        authorizations,
        next_authority_key_proof: Some(next.sign(OWNER_TRANSITION_DOMAIN, &body)),
        next_recovery_key_proofs: Vec::new(),
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn recovery_window_is_signed_preserved_and_statefully_checkable() {
    let authority = TestKey::new(1);
    let paper = TestKey::new(2);
    let social = TestKey::new(3);
    let next = TestKey::new(7);
    let guardians = [
        (&paper, RecoveryGuardianKind::Paper),
        (&social, RecoveryGuardianKind::Social),
    ];
    let policy = recovery_policy(&guardians, Some(100));
    let root = signed_root_with_policy(OWNER_UUID, &authority, &guardians, policy.clone());
    let state = verify_owner_root(&root).expect("root");
    assert_eq!(effective_recovery_window(state.recovery_policy()), 100);

    let recovery =
        recovery_transition(&state, &[&paper, &social], &next, policy.clone(), NOW + 100);
    verify_transition_timelock(&state, &recovery, NOW).expect("scheduled after full window");
    assert!(matches!(
        apply_transition(&state, &recovery, NOW, limits()),
        Err(Error::NotYetValid)
    ));
    apply_transition_with_timelock(&state, &recovery, NOW + 100, NOW, limits())
        .expect("recovery activates after the persisted hold");

    let too_early =
        recovery_transition(&state, &[&paper, &social], &next, policy.clone(), NOW + 99);
    assert_eq!(
        verify_transition_timelock(&state, &too_early, NOW),
        Err(Error::NotYetValid)
    );

    let changed_window = recovery_transition(
        &state,
        &[&paper, &social],
        &next,
        RecoveryPolicy {
            window_secs: Some(99),
            ..policy
        },
        NOW + 100,
    );
    assert!(apply_transition(&state, &changed_window, NOW + 100, limits()).is_err());
}

fn recovery_policy_transition(
    state: &VerifiedOwnerState,
    authority: &TestKey,
    current_guardians: &[&TestKey],
    next_guardians: &[&TestKey],
    next_policy: RecoveryPolicy,
    valid_from: i64,
) -> SignedOwnerKeyTransition {
    let next_proofs = next_policy
        .guardians
        .iter()
        .map(|guardian| {
            let id = key_id(guardian.key.as_ref().expect("next guardian key"));
            next_guardians
                .iter()
                .find(|key| key_id(&key.wire()) == id)
                .expect("next guardian signer")
        })
        .collect::<Vec<_>>();
    let transition = transition(
        state,
        OwnerKeyTransitionKind::RecoveryPolicy,
        authority,
        next_policy,
        state.sequence() + 1,
        state.state_hash().to_vec(),
        valid_from,
        NOW + 200,
    );
    let body = transition_body(&transition).expect("policy body");
    let mut authorizations = vec![authority.sign(OWNER_TRANSITION_DOMAIN, &body)];
    authorizations.extend(
        current_guardians
            .iter()
            .map(|key| key.sign(OWNER_TRANSITION_DOMAIN, &body)),
    );
    authorizations.sort_by(|left, right| left.signer_key_id.cmp(&right.signer_key_id));
    SignedOwnerKeyTransition {
        transition: Some(transition),
        authorizations,
        next_authority_key_proof: None,
        next_recovery_key_proofs: next_proofs
            .iter()
            .map(|key| key.sign(OWNER_TRANSITION_DOMAIN, &body))
            .collect(),
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn only_authorized_policy_transition_can_lower_window() {
    let authority = TestKey::new(1);
    let paper = TestKey::new(2);
    let social = TestKey::new(3);
    let next_paper = TestKey::new(8);
    let next_social = TestKey::new(9);
    let guardians = [
        (&paper, RecoveryGuardianKind::Paper),
        (&social, RecoveryGuardianKind::Social),
    ];
    let root = signed_root_with_policy(
        OWNER_UUID,
        &authority,
        &guardians,
        recovery_policy(&guardians, Some(100)),
    );
    let state = verify_owner_root(&root).expect("root");
    let next_guardians = [
        (&next_paper, RecoveryGuardianKind::Paper),
        (&next_social, RecoveryGuardianKind::Social),
    ];
    let next_policy = recovery_policy(&next_guardians, Some(20));
    let change = recovery_policy_transition(
        &state,
        &authority,
        &[&paper, &social],
        &[&next_paper, &next_social],
        next_policy,
        NOW + 100,
    );
    verify_transition_timelock(&state, &change, NOW).expect("lowering waits current window");
    assert!(matches!(
        apply_transition(&state, &change, NOW, limits()),
        Err(Error::NotYetValid)
    ));
    let changed = apply_transition_with_timelock(&state, &change, NOW + 100, NOW, limits())
        .expect("policy change after the persisted hold");
    assert_eq!(effective_recovery_window(changed.recovery_policy()), 20);

    let mut tampered_window = change.clone();
    tampered_window
        .transition
        .as_mut()
        .expect("transition")
        .next_recovery_policy
        .as_mut()
        .expect("policy")
        .window_secs = Some(19);
    assert!(matches!(
        apply_transition(&state, &tampered_window, NOW + 100, limits()),
        Err(Error::InvalidSignature)
    ));

    let mut wrong_authority = change;
    let body =
        transition_body(wrong_authority.transition.as_ref().expect("transition")).expect("body");
    let authority_key_id = key_id(&authority.wire());
    let authority_proof = wrong_authority
        .authorizations
        .iter_mut()
        .find(|proof| proof.signer_key_id == authority_key_id)
        .expect("authority proof");
    *authority_proof = next_paper.sign(OWNER_TRANSITION_DOMAIN, &body);
    wrong_authority
        .authorizations
        .sort_by(|left, right| left.signer_key_id.cmp(&right.signer_key_id));
    assert!(apply_transition(&state, &wrong_authority, NOW + 100, limits()).is_err());
}

fn base_keyring() -> (CloneAuthorizationKeyring, SignedOwnerKeyTransition) {
    let authority = TestKey::new(1);
    let paper = TestKey::new(2);
    let social = TestKey::new(3);
    let next = TestKey::new(7);
    let root = signed_root(
        OWNER_UUID,
        &authority,
        &[
            (&paper, RecoveryGuardianKind::Paper),
            (&social, RecoveryGuardianKind::Social),
        ],
    );
    let state = verify_owner_root(&root).expect("root");
    let transition = rotation(&state, &authority, &next);
    let current = apply_transition(&state, &transition, NOW, limits()).expect("rotation");
    (
        CloneAuthorizationKeyring {
            format_version: 1,
            spool_uuid: SPOOL.to_vec(),
            canonical_spool_path_segments: path(),
            pin: Some(CloneOwnerPin {
                kind: CloneOwnerPinKind::CloneTofu as i32,
                expected_owner_id: state.owner_id().to_vec(),
                first_seen_unix_seconds: NOW - 20,
            }),
            owner_root: Some(root),
            accepted_transitions: vec![transition.clone()],
            accepted_state_hash: current.state_hash().to_vec(),
            owner_genesis: Some(signed_genesis(SPOOL, &authority)),
            ownership_transfers: Vec::new(),
        },
        transition,
    )
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn keyring_recomputes_genesis_ids_hashes_and_linear_chain() {
    let (keyring, transition) = base_keyring();
    verify_clone_keyring(keyring.clone(), NOW, limits(), &[]).expect("linear keyring");

    let mut fork = keyring.clone();
    fork.accepted_transitions.push(transition);
    assert!(verify_clone_keyring(fork, NOW, limits(), &[]).is_err());

    let mut wrong_spool = keyring.clone();
    wrong_spool.spool_uuid[0] ^= 1;
    assert!(verify_clone_keyring(wrong_spool, NOW, limits(), &[]).is_err());

    let mut wrong_owner_id = keyring.clone();
    wrong_owner_id.pin.as_mut().expect("pin").expected_owner_id[0] ^= 1;
    assert!(verify_clone_keyring(wrong_owner_id, NOW, limits(), &[]).is_err());

    let mut unknown_version = keyring;
    unknown_version.format_version = 2;
    assert!(verify_clone_keyring(unknown_version, NOW, limits(), &[]).is_err());
}

fn signed_transfer(
    source_uuid: [u8; 16],
    source_state: &VerifiedOwnerState,
    source_key: &TestKey,
    destination_uuid: [u8; 16],
    destination_state: &VerifiedOwnerState,
    destination_key: &TestKey,
) -> ResourceOwnershipTransfer {
    let handoff = ResourceTransferHandoff {
        format_version: 1,
        resource_uuid: SPOOL.to_vec(),
        transfer_sequence: 1,
        source_owner_uuid: source_uuid.to_vec(),
        source_owner_key_state_hash: source_state.state_hash().to_vec(),
        destination_owner_uuid: destination_uuid.to_vec(),
        destination_owner_key_state_hash: destination_state.state_hash().to_vec(),
        nonce: vec![0x81; 32],
    };
    let signed_handoff = SignedResourceTransferHandoff {
        handoff: Some(handoff.clone()),
        source_signature: Some(source_key.sign(
            TRANSFER_HANDOFF_DOMAIN,
            &transfer_handoff_body(&handoff).expect("handoff body"),
        )),
    };
    let mut transfer = ResourceOwnershipTransfer {
        acceptance: Some(ResourceTransferAcceptance {
            signed_handoff: Some(signed_handoff),
            destination_signature: None,
        }),
    };
    transfer
        .acceptance
        .as_mut()
        .expect("acceptance")
        .destination_signature = Some(destination_key.sign(
        TRANSFER_ACCEPTANCE_DOMAIN,
        &transfer_acceptance_body(&transfer).expect("acceptance body"),
    ));
    transfer
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn ownership_transfer_still_requires_both_owner_signatures() {
    let source_key = TestKey::new(1);
    let source_paper = TestKey::new(2);
    let source_social = TestKey::new(3);
    let destination_key = TestKey::new(7);
    let destination_paper = TestKey::new(8);
    let destination_social = TestKey::new(9);
    let destination_uuid = [0x33; 16];
    let source_root = signed_root(
        OWNER_UUID,
        &source_key,
        &[
            (&source_paper, RecoveryGuardianKind::Paper),
            (&source_social, RecoveryGuardianKind::Social),
        ],
    );
    let destination_root = signed_root(
        destination_uuid,
        &destination_key,
        &[
            (&destination_paper, RecoveryGuardianKind::Paper),
            (&destination_social, RecoveryGuardianKind::Social),
        ],
    );
    let source_state = verify_owner_root(&source_root).expect("source root");
    let destination_state = verify_owner_root(&destination_root).expect("destination root");
    let source = TransferOwner {
        stable_owner_uuid: &OWNER_UUID,
        state: &source_state,
    };
    let destination = TransferOwner {
        stable_owner_uuid: &destination_uuid,
        state: &destination_state,
    };
    let complete = signed_transfer(
        OWNER_UUID,
        &source_state,
        &source_key,
        destination_uuid,
        &destination_state,
        &destination_key,
    );
    verify_resource_transfer(&complete, &SPOOL, 1, source, destination).expect("complete");
    let mut incomplete = complete;
    incomplete
        .acceptance
        .as_mut()
        .expect("acceptance")
        .destination_signature = None;
    assert!(verify_resource_transfer(&incomplete, &SPOOL, 1, source, destination).is_err());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn protobuf_and_payload_limits_fail_closed() {
    let value = artifact();
    let mut encoded = value.authorization.encode_to_vec();
    encoded.resize(limits().max_bundle_bytes() + 257, 0);
    assert_eq!(
        verify_purge_authorization_bytes(
            &encoded,
            &value.body,
            &value.payload,
            &PurgeContext {
                owner_genesis: &value.owner_genesis,
                current_owner_state_hash: &value.current_state_hash,
                spool_uuid: &value.spool_uuid,
                spool_path_segments: &value.path,
                now_unix_seconds: NOW,
                limits: limits(),
            }
        ),
        Decision::Deny(Denial::OverLimit)
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn embedded_fixture_adapters_agree() {
    let operation =
        conformance::run_fixture(conformance::FIXTURE_V2_JSON).expect("purge fixture runs");
    let transfer = conformance::run_transfer_fixture(conformance::TRANSFER_FIXTURE_V2_JSON)
        .expect("transfer fixture runs");
    let keyring = conformance::run_keyring_fixture(conformance::KEYRING_FIXTURE_V2_JSON)
        .expect("keyring fixture runs");
    assert_eq!(operation.len(), 11);
    assert_eq!(transfer.len(), 3);
    assert_eq!(keyring.len(), 2);
    for outcome in operation {
        assert!(
            outcome.matches,
            "{} expected {:?}, got {:?}",
            outcome.name, outcome.expected, outcome.actual
        );
    }
    for outcome in transfer.into_iter().chain(keyring) {
        assert!(
            outcome.matches,
            "{} expected {}, got {}",
            outcome.name, outcome.expected_accept, outcome.accepted
        );
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn embedded_valid_fixture_components_verify() {
    let fixture: conformance::ConformanceFixture =
        serde_json::from_str(conformance::FIXTURE_V2_JSON).expect("fixture JSON");
    let case = fixture.cases.first().expect("valid case");
    let authorization = SidecarAuthorization::decode(
        hex::decode(&case.authorization_hex)
            .expect("authorization hex")
            .as_slice(),
    )
    .expect("authorization protobuf");
    let genesis = SignedSpoolOwnerGenesis::decode(
        hex::decode(&case.owner_genesis_hex)
            .expect("genesis hex")
            .as_slice(),
    )
    .expect("genesis protobuf");
    verify_spool_owner_genesis(&genesis).expect("genesis proof");
    let verified = verify_authorization_bundle(
        authorization.capability.as_ref().expect("bundle"),
        case.now_unix_seconds,
        limits(),
    )
    .expect("authorization bundle");
    let body = PurgeOperationSigningBody::decode(
        hex::decode(&case.operation_body_hex)
            .expect("operation body hex")
            .as_slice(),
    )
    .expect("operation body protobuf");
    let subject_key = verified
        .capability()
        .capability()
        .subject
        .as_ref()
        .and_then(|subject| subject.key.as_ref())
        .expect("subject key");
    verify_signature(
        subject_key,
        authorization
            .operation_signature
            .as_ref()
            .expect("operation signature"),
        PURGE_OPERATION_DOMAIN,
        &canonical_purge_operation(&body).expect("canonical purge body"),
    )
    .expect("operation proof");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
#[ignore = "maintainer-only fixture regeneration"]
fn print_fixture_json() {
    fn case(name: &str, value: &Artifact, expected: Decision) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "expected": expected,
            "authorization_hex": hex::encode(value.authorization.encode_to_vec()),
            "operation_body_hex": hex::encode(value.body.encode_to_vec()),
            "payload_hex": hex::encode(&value.payload),
            "owner_genesis_hex": hex::encode(value.owner_genesis.encode_to_vec()),
            "current_owner_state_hash_hex": hex::encode(value.current_state_hash),
            "spool_uuid_hex": hex::encode(value.spool_uuid),
            "spool_path_segments": value.path,
            "now_unix_seconds": NOW,
        })
    }

    let valid = artifact();
    let mut absent = artifact();
    absent.authorization.capability = None;
    let mut invalid_signature = artifact();
    invalid_signature
        .authorization
        .operation_signature
        .as_mut()
        .expect("signature")
        .signature[0] ^= 1;
    let expired = artifact_with(SPOOL, 1, NOW - 200, NOW - 1);
    let mut wrong_spool = artifact();
    wrong_spool.spool_uuid[0] ^= 1;
    let wrong_action = artifact_with(SPOOL, 0, NOW - 10, NOW + 200);
    let mut attenuated = artifact();
    let duplicate = attenuated
        .authorization
        .capability
        .as_ref()
        .expect("bundle")
        .capability_chain[0]
        .clone();
    attenuated
        .authorization
        .capability
        .as_mut()
        .expect("bundle")
        .capability_chain
        .push(duplicate);
    let mut forged_genesis = artifact();
    forged_genesis
        .owner_genesis
        .owner_signature
        .as_mut()
        .expect("signature")
        .signature[0] ^= 1;
    let fixture = serde_json::json!({
        "format_version": 2,
        "max_capability_ttl_seconds": 3600,
        "cases": [
            case("owner-anchored-purge", &valid, Decision::Purge),
            case("absent-capability", &absent, Decision::Deny(Denial::Capability)),
            case("invalid-signature", &invalid_signature, Decision::Deny(Denial::InvalidProof)),
            case("expired", &expired, Decision::Deny(Denial::Time)),
            case("wrong-spool", &wrong_spool, Decision::Deny(Denial::OperationBinding)),
            case("wrong-action-non-purge", &wrong_action, Decision::Deny(Denial::Capability)),
            case("attenuation-forged", &attenuated, Decision::Deny(Denial::DirectOnly)),
            case("forged-genesis", &forged_genesis, Decision::Deny(Denial::InvalidProof)),
            case("transition-gap", &transition_artifact(Some(BrokenTransition::Gap)), Decision::Deny(Denial::InvalidProof)),
            case("transition-fork", &transition_artifact(Some(BrokenTransition::Fork)), Decision::Deny(Denial::InvalidProof)),
            case("transition-wrong-signer", &transition_artifact(Some(BrokenTransition::WrongSigner)), Decision::Deny(Denial::InvalidProof))
        ]
    });
    println!(
        "PURGE_FIXTURE_START\n{}\nPURGE_FIXTURE_END",
        serde_json::to_string_pretty(&fixture).expect("JSON")
    );

    let source_key = TestKey::new(1);
    let source_paper = TestKey::new(2);
    let source_social = TestKey::new(3);
    let destination_key = TestKey::new(7);
    let destination_paper = TestKey::new(8);
    let destination_social = TestKey::new(9);
    let destination_uuid = [0x33; 16];
    let source_root = signed_root(
        OWNER_UUID,
        &source_key,
        &[
            (&source_paper, RecoveryGuardianKind::Paper),
            (&source_social, RecoveryGuardianKind::Social),
        ],
    );
    let destination_root = signed_root(
        destination_uuid,
        &destination_key,
        &[
            (&destination_paper, RecoveryGuardianKind::Paper),
            (&destination_social, RecoveryGuardianKind::Social),
        ],
    );
    let source_state = verify_owner_root(&source_root).expect("source");
    let destination_state = verify_owner_root(&destination_root).expect("destination");
    let complete = signed_transfer(
        OWNER_UUID,
        &source_state,
        &source_key,
        destination_uuid,
        &destination_state,
        &destination_key,
    );
    let mut source_only = complete.clone();
    source_only
        .acceptance
        .as_mut()
        .expect("acceptance")
        .destination_signature = None;
    let mut destination_only = complete.clone();
    destination_only
        .acceptance
        .as_mut()
        .expect("acceptance")
        .signed_handoff
        .as_mut()
        .expect("handoff")
        .source_signature = None;
    let transfer_case = |name: &str, transfer: &ResourceOwnershipTransfer, accepted: bool| {
        serde_json::json!({
            "name": name,
            "expected_accept": accepted,
            "transfer_hex": hex::encode(transfer.encode_to_vec()),
            "source_owner_root_hex": hex::encode(source_root.encode_to_vec()),
            "destination_owner_root_hex": hex::encode(destination_root.encode_to_vec()),
            "source_owner_uuid_hex": hex::encode(OWNER_UUID),
            "destination_owner_uuid_hex": hex::encode(destination_uuid),
            "resource_uuid_hex": hex::encode(SPOOL),
            "transfer_sequence": 1,
        })
    };
    let transfers = serde_json::json!({
        "format_version": 2,
        "cases": [
            transfer_case("complete-transfer", &complete, true),
            transfer_case("incomplete-transfer-source-only", &source_only, false),
            transfer_case("incomplete-transfer-destination-only", &destination_only, false)
        ]
    });
    println!(
        "TRANSFER_FIXTURE_START\n{}\nTRANSFER_FIXTURE_END",
        serde_json::to_string_pretty(&transfers).expect("JSON")
    );

    let (linear, transition) = base_keyring();
    let mut forked = linear.clone();
    forked.accepted_transitions.push(transition);
    let keyring_case = |name: &str, value: &CloneAuthorizationKeyring, accepted: bool| {
        serde_json::json!({
            "name": name,
            "expected_accept": accepted,
            "keyring_hex": hex::encode(value.encode_to_vec()),
            "now_unix_seconds": NOW,
        })
    };
    let keyrings = serde_json::json!({
        "format_version": 2,
        "max_capability_ttl_seconds": 3600,
        "cases": [
            keyring_case("linear-self-rooted-keyring", &linear, true),
            keyring_case("forked-keyring", &forked, false)
        ]
    });
    println!(
        "KEYRING_FIXTURE_START\n{}\nKEYRING_FIXTURE_END",
        serde_json::to_string_pretty(&keyrings).expect("JSON")
    );
}
