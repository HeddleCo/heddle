#![allow(dead_code)]

use heddle_client::owner_authorization::{
    AuthorizationKey, GuardianSigner, RecoverySetup, VerificationLimits, VerifiedOwnerState,
    create_direct_capability, create_human_owner_root, mint_subject_biscuit, verify_owner_root,
    wire::{
        CapabilityPrincipal, CapabilityPrincipalKind, OwnerAuthorizationBundle,
        SignedOwnerCapability, SpoolCapabilityAction, SpoolCapabilityGrant, SpoolSelector,
    },
};

pub const NOW: i64 = 1_000;

pub fn limits() -> VerificationLimits {
    VerificationLimits::new(300, 3_600, 1024 * 1024).expect("test limits")
}

pub struct Fixture {
    pub authority: AuthorizationKey,
    pub recovery: RecoverySetup,
    pub root: heddle_client::owner_authorization::wire::SignedOwnerRoot,
    pub state: VerifiedOwnerState,
    pub subject: AuthorizationKey,
    pub spool_uuid: [u8; 16],
}

impl Fixture {
    pub fn new() -> Self {
        let authority = AuthorizationKey::from_seed([1; 32]).expect("authority");
        let paper = GuardianSigner::paper(AuthorizationKey::from_seed([2; 32]).expect("paper"));
        let social = GuardianSigner::social(AuthorizationKey::from_seed([3; 32]).expect("social"));
        let recovery =
            RecoverySetup::recommended(vec![paper, social]).expect("recommended recovery");
        let root = create_human_owner_root([4; 16], &authority, &recovery).expect("signed root");
        let state = verify_owner_root(&root).expect("verified root");
        Self {
            authority,
            recovery,
            root,
            state,
            subject: AuthorizationKey::from_seed([5; 32]).expect("subject"),
            spool_uuid: [6; 16],
        }
    }

    pub fn subject(&self) -> CapabilityPrincipal {
        CapabilityPrincipal {
            kind: CapabilityPrincipalKind::Agent as i32,
            principal_id: b"agent-test".to_vec(),
            key: Some(self.subject.verification_key()),
        }
    }

    pub fn grant(&self, actions: &[SpoolCapabilityAction]) -> SpoolCapabilityGrant {
        let mut actions = actions
            .iter()
            .map(|action| *action as i32)
            .collect::<Vec<_>>();
        actions.sort_unstable();
        actions.dedup();
        SpoolCapabilityGrant {
            spool: Some(SpoolSelector {
                root_spool_uuid: self.spool_uuid.to_vec(),
                path_segments: vec!["acme".to_string(), "heddle".to_string()],
                include_descendants: true,
            }),
            actions,
        }
    }

    pub fn direct(&self, actions: &[SpoolCapabilityAction]) -> SignedOwnerCapability {
        create_direct_capability(
            &self.state,
            &self.authority,
            self.subject(),
            vec![self.grant(actions)],
            NOW - 10,
            NOW + 200,
            limits(),
        )
        .expect("direct capability")
    }

    pub fn bundle(&self, capability: SignedOwnerCapability) -> OwnerAuthorizationBundle {
        let biscuit = mint_subject_biscuit(
            capability.capability.as_ref().expect("capability"),
            &self.subject,
        )
        .expect("subject Biscuit");
        OwnerAuthorizationBundle {
            owner_root: Some(self.root.clone()),
            owner_state_chain: Vec::new(),
            capability_chain: vec![capability],
            subject_biscuit: biscuit,
        }
    }
}
