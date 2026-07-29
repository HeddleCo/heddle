use crate::owner_authorization::{
    AuthorizationError, Result, VerificationLimits, VerifiedOwnerState, apply_transition,
    canonical::{OWNER_CAPABILITY_DOMAIN, capability_body, capability_without_id, digest},
    capability::{capability_is_well_formed, grant_covers, verify_subject_biscuit},
    key::verify_signature,
    root::verify_owner_root,
    wire::{OwnerAuthorizationBundle, OwnerCapability, SignedOwnerCapability},
};

/// Capability whose id, lifetime, scope, ancestry, and signature are verified.
#[derive(Clone)]
pub struct VerifiedCapability {
    signed: SignedOwnerCapability,
}

impl VerifiedCapability {
    /// Verified body.
    pub fn capability(&self) -> &OwnerCapability {
        self.signed
            .capability
            .as_ref()
            .expect("verified capability body")
    }

    /// Stable owner id.
    pub fn owner_id(&self) -> [u8; 32] {
        self.capability()
            .owner_id
            .as_slice()
            .try_into()
            .expect("verified owner id")
    }

    /// State hash named by the issuer.
    pub fn issuer_state_hash(&self) -> [u8; 32] {
        self.capability()
            .issuer_state_hash
            .as_slice()
            .try_into()
            .expect("verified state hash")
    }

    /// Signed wire object.
    pub fn signed(&self) -> &SignedOwnerCapability {
        &self.signed
    }
}

/// Fully verified portable authorization bundle.
pub struct VerifiedAuthorizationBundle {
    owner_state: VerifiedOwnerState,
    leaf: VerifiedCapability,
}

impl VerifiedAuthorizationBundle {
    /// Accepted owner state.
    pub fn owner_state(&self) -> &VerifiedOwnerState {
        &self.owner_state
    }

    /// Leaf capability bound to the subject Biscuit.
    pub fn leaf(&self) -> &VerifiedCapability {
        &self.leaf
    }
}

fn verify_capability_id(capability: &OwnerCapability) -> Result<()> {
    let expected = digest(OWNER_CAPABILITY_DOMAIN, &capability_without_id(capability)?);
    if capability.capability_id.as_slice() != expected {
        return Err(AuthorizationError::Invalid(
            "capability id does not match canonical body".to_string(),
        ));
    }
    Ok(())
}

/// Verify a direct capability and all child derivations.
pub fn verify_capability_chain(
    state: &VerifiedOwnerState,
    chain: &[SignedOwnerCapability],
    now_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<Vec<VerifiedCapability>> {
    if chain.is_empty() {
        return Err(AuthorizationError::Invalid(
            "authorization bundle has no capabilities".to_string(),
        ));
    }
    let mut verified: Vec<VerifiedCapability> = Vec::with_capacity(chain.len());
    for signed in chain {
        let capability = signed.capability.as_ref().ok_or_else(|| {
            AuthorizationError::Invalid("signed capability has no body".to_string())
        })?;
        capability_is_well_formed(capability, limits)?;
        verify_capability_id(capability)?;
        if capability.owner_id.as_slice() != state.owner_id()
            || now_unix_seconds < capability.not_before_unix_seconds
            || now_unix_seconds > capability.expires_at_unix_seconds
        {
            return Err(AuthorizationError::Expired);
        }
        let signature = signed
            .signature
            .as_ref()
            .ok_or(AuthorizationError::InvalidSignature)?;
        let signer = if let Some(parent) = verified.last() {
            let parent_body = parent.capability();
            if capability.parent_capability_id != parent_body.capability_id
                || capability.issuer_state_hash != parent_body.issuer_state_hash
                || capability.not_before_unix_seconds < parent_body.not_before_unix_seconds
                || capability.expires_at_unix_seconds > parent_body.expires_at_unix_seconds
                || !grant_covers(&parent_body.grants, &capability.grants)
            {
                return Err(AuthorizationError::CapabilityDenied(
                    "child capability widens or detaches from its parent".to_string(),
                ));
            }
            parent_body
                .subject
                .as_ref()
                .and_then(|subject| subject.key.as_ref())
                .ok_or_else(|| {
                    AuthorizationError::CapabilityDenied(
                        "parent subject has no delegation key".to_string(),
                    )
                })?
        } else {
            if !capability.parent_capability_id.is_empty() {
                return Err(AuthorizationError::BrokenChain(
                    "first capability is not a direct owner grant".to_string(),
                ));
            }
            state.issuer_at(&capability.issuer_state_hash, now_unix_seconds)?
        };
        verify_signature(
            signer,
            signature,
            OWNER_CAPABILITY_DOMAIN,
            &capability_body(capability)?,
        )?;
        verified.push(VerifiedCapability {
            signed: signed.clone(),
        });
    }
    Ok(verified)
}

/// Verify a portable root/state/capability/Biscuit bundle offline.
pub fn verify_authorization_bundle(
    bundle: &OwnerAuthorizationBundle,
    now_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<VerifiedAuthorizationBundle> {
    let mut state = verify_owner_root(bundle.owner_root.as_ref().ok_or_else(|| {
        AuthorizationError::Invalid("authorization bundle has no owner root".to_string())
    })?)?;
    for transition in &bundle.owner_state_chain {
        state = apply_transition(&state, transition, now_unix_seconds, limits)?;
    }
    let chain =
        verify_capability_chain(&state, &bundle.capability_chain, now_unix_seconds, limits)?;
    let leaf = chain
        .last()
        .expect("nonempty verified capability chain")
        .clone();
    verify_subject_biscuit(leaf.capability(), &bundle.subject_biscuit)?;
    Ok(VerifiedAuthorizationBundle {
        owner_state: state,
        leaf,
    })
}
