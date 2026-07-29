use crate::owner_authorization::{
    AuthorizationError, AuthorizationKey, GuardianSigner, RecoverySetup, VerificationLimits,
    canonical::{OWNER_CAPABILITY_DOMAIN, capability_body},
    capability::{
        create::{CapabilityLineage, unsigned_capability},
        create_direct_capability, verify_capability_chain,
    },
    create_human_owner_root,
    root::verify_owner_root,
    wire::{
        CapabilityPrincipal, CapabilityPrincipalKind, SignedOwnerCapability, SpoolCapabilityAction,
        SpoolCapabilityGrant, SpoolSelector,
    },
};

fn limits() -> VerificationLimits {
    VerificationLimits::new(300, 3_600, 1024 * 1024).expect("limits")
}

#[test]
fn capability_asserting_a_right_outside_parent_grant_does_not_verify() {
    let authority = AuthorizationKey::from_seed([31; 32]).expect("authority");
    let recovery = RecoverySetup::recommended(vec![
        GuardianSigner::paper(AuthorizationKey::from_seed([32; 32]).expect("paper")),
        GuardianSigner::social(AuthorizationKey::from_seed([33; 32]).expect("social")),
    ])
    .expect("recovery");
    let root = create_human_owner_root([34; 16], &authority, &recovery).expect("root");
    let state = verify_owner_root(&root).expect("state");
    let subject_key = AuthorizationKey::from_seed([35; 32]).expect("subject");
    let subject = CapabilityPrincipal {
        kind: CapabilityPrincipalKind::Agent as i32,
        principal_id: b"parent-agent".to_vec(),
        key: Some(subject_key.verification_key()),
    };
    let selector = SpoolSelector {
        root_spool_uuid: vec![36; 16],
        path_segments: vec!["acme".to_string()],
        include_descendants: true,
    };
    let parent = create_direct_capability(
        &state,
        &authority,
        subject,
        vec![SpoolCapabilityGrant {
            spool: Some(selector.clone()),
            actions: vec![
                SpoolCapabilityAction::Read as i32,
                SpoolCapabilityAction::Grant as i32,
            ],
        }],
        100,
        300,
        limits(),
    )
    .expect("parent");
    let parent_body = parent.capability.as_ref().expect("parent body");
    let child_body = unsigned_capability(
        CapabilityLineage {
            owner_id: state.owner_id(),
            issuer_state_hash: state.state_hash(),
            parent_capability_id: parent_body.capability_id.clone(),
        },
        CapabilityPrincipal {
            kind: CapabilityPrincipalKind::Agent as i32,
            principal_id: b"child-agent".to_vec(),
            key: Some(
                AuthorizationKey::from_seed([37; 32])
                    .expect("child")
                    .verification_key(),
            ),
        },
        vec![SpoolCapabilityGrant {
            spool: Some(selector),
            actions: vec![SpoolCapabilityAction::Write as i32],
        }],
        100,
        300,
        limits(),
    )
    .expect("well-formed but wider child");
    let child = SignedOwnerCapability {
        signature: Some(
            subject_key
                .sign(
                    OWNER_CAPABILITY_DOMAIN,
                    &capability_body(&child_body).expect("canonical child"),
                )
                .expect("sign wider child"),
        ),
        capability: Some(child_body),
    };
    assert!(matches!(
        verify_capability_chain(&state, &[parent, child], 200, limits()),
        Err(AuthorizationError::CapabilityDenied(_))
    ));
}
