use crate::owner_authorization::{
    AuthorizationError, AuthorizationKey, Result, VerificationLimits, VerifiedCapability,
    VerifiedOwnerState,
    canonical::{
        OWNER_CAPABILITY_DOMAIN, capability_body, capability_without_id, digest, key_id, nonce,
    },
    capability::{capability_is_well_formed, grant_covers},
    wire::{CapabilityPrincipal, OwnerCapability, SignedOwnerCapability, SpoolCapabilityGrant},
};

pub(super) struct CapabilityLineage {
    pub(super) owner_id: [u8; 32],
    pub(super) issuer_state_hash: [u8; 32],
    pub(super) parent_capability_id: Vec<u8>,
}

pub(super) fn unsigned_capability(
    lineage: CapabilityLineage,
    subject: CapabilityPrincipal,
    grants: Vec<SpoolCapabilityGrant>,
    not_before_unix_seconds: i64,
    expires_at_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<OwnerCapability> {
    let mut capability = OwnerCapability {
        format_version: 1,
        owner_id: lineage.owner_id.to_vec(),
        issuer_state_hash: lineage.issuer_state_hash.to_vec(),
        parent_capability_id: lineage.parent_capability_id,
        subject: Some(subject),
        grants,
        not_before_unix_seconds,
        expires_at_unix_seconds,
        nonce: nonce(),
        capability_id: vec![0; 32],
    };
    capability_is_well_formed(&capability, limits)?;
    capability.capability_id = digest(
        OWNER_CAPABILITY_DOMAIN,
        &capability_without_id(&capability)?,
    )
    .to_vec();
    Ok(capability)
}

/// Create a direct grant signed by the active owner authority.
pub fn create_direct_capability(
    state: &VerifiedOwnerState,
    authority: &AuthorizationKey,
    subject: CapabilityPrincipal,
    grants: Vec<SpoolCapabilityGrant>,
    not_before_unix_seconds: i64,
    expires_at_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<SignedOwnerCapability> {
    if authority.key_id() != key_id(state.authority_key()) {
        return Err(AuthorizationError::Invalid(
            "direct capability signer is not the active owner authority".to_string(),
        ));
    }
    let capability = unsigned_capability(
        CapabilityLineage {
            owner_id: state.owner_id(),
            issuer_state_hash: state.state_hash(),
            parent_capability_id: Vec::new(),
        },
        subject,
        grants,
        not_before_unix_seconds,
        expires_at_unix_seconds,
        limits,
    )?;
    let signature = authority.sign(OWNER_CAPABILITY_DOMAIN, &capability_body(&capability)?)?;
    Ok(SignedOwnerCapability {
        capability: Some(capability),
        signature: Some(signature),
    })
}

/// Create an attenuated child capability signed by the parent's subject key.
pub fn create_child_capability(
    parent: &VerifiedCapability,
    parent_subject_key: &AuthorizationKey,
    subject: CapabilityPrincipal,
    grants: Vec<SpoolCapabilityGrant>,
    not_before_unix_seconds: i64,
    expires_at_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<SignedOwnerCapability> {
    let parent_body = parent.capability();
    let parent_subject = parent_body.subject.as_ref().ok_or_else(|| {
        AuthorizationError::Invalid("parent capability has no subject".to_string())
    })?;
    let parent_key = parent_subject.key.as_ref().ok_or_else(|| {
        AuthorizationError::Invalid("ANY_ANONYMOUS cannot derive a child".to_string())
    })?;
    if parent_subject_key.key_id() != key_id(parent_key)
        || not_before_unix_seconds < parent_body.not_before_unix_seconds
        || expires_at_unix_seconds > parent_body.expires_at_unix_seconds
        || !grant_covers(&parent_body.grants, &grants)
    {
        return Err(AuthorizationError::CapabilityDenied(
            "child capability widens its parent".to_string(),
        ));
    }
    let capability = unsigned_capability(
        CapabilityLineage {
            owner_id: parent.owner_id(),
            issuer_state_hash: parent.issuer_state_hash(),
            parent_capability_id: parent_body.capability_id.clone(),
        },
        subject,
        grants,
        not_before_unix_seconds,
        expires_at_unix_seconds,
        limits,
    )?;
    let signature =
        parent_subject_key.sign(OWNER_CAPABILITY_DOMAIN, &capability_body(&capability)?)?;
    Ok(SignedOwnerCapability {
        capability: Some(capability),
        signature: Some(signature),
    })
}
