use crate::owner_authorization::{
    AuthorizationError, AuthorizationKey, RecoverySetup, Result, VerificationLimits,
    VerifiedOwnerState,
    canonical::{OWNER_TRANSITION_DOMAIN, nonce, transition_body},
    wire::{
        AuthorizationSignature, OwnerKeyTransition, OwnerKeyTransitionKind,
        SignedOwnerKeyTransition,
    },
};

fn validate_overlap(
    valid_from: i64,
    previous_valid_until: i64,
    limits: VerificationLimits,
) -> Result<()> {
    if valid_from < 0
        || previous_valid_until < 0
        || (previous_valid_until > 0 && previous_valid_until < valid_from)
        || previous_valid_until.saturating_sub(valid_from) > limits.max_capability_ttl_seconds()
    {
        return Err(AuthorizationError::Invalid(
            "previous-key handover exceeds the capability TTL ceiling".to_string(),
        ));
    }
    Ok(())
}

fn transition_body_for(
    state: &VerifiedOwnerState,
    kind: OwnerKeyTransitionKind,
    next_authority: &AuthorizationKey,
    next_recovery_policy: crate::owner_authorization::wire::RecoveryPolicy,
    valid_from: i64,
    previous_valid_until: i64,
    limits: VerificationLimits,
) -> Result<OwnerKeyTransition> {
    validate_overlap(valid_from, previous_valid_until, limits)?;
    Ok(OwnerKeyTransition {
        format_version: 1,
        owner_id: state.owner_id().to_vec(),
        previous_state_hash: state.state_hash().to_vec(),
        sequence: state.sequence() + 1,
        kind: kind as i32,
        next_authority_key: Some(next_authority.verification_key()),
        next_recovery_policy: Some(next_recovery_policy),
        valid_from_unix_seconds: valid_from,
        previous_key_valid_until_unix_seconds: previous_valid_until,
        nonce: nonce(),
    })
}

fn threshold_authorizations(
    policy: &crate::owner_authorization::wire::RecoveryPolicy,
    guardians: &[&AuthorizationKey],
    body: &[u8],
) -> Result<Vec<AuthorizationSignature>> {
    let allowed = policy
        .guardians
        .iter()
        .filter_map(|guardian| guardian.key.as_ref())
        .map(crate::owner_authorization::canonical::key_id)
        .collect::<std::collections::BTreeSet<_>>();
    let mut seen = std::collections::BTreeSet::new();
    let mut signatures = Vec::new();
    for guardian in guardians {
        let id = guardian.key_id();
        if !allowed.contains(&id) || !seen.insert(id) {
            return Err(AuthorizationError::Invalid(
                "recovery signer is absent or duplicated in the active policy".to_string(),
            ));
        }
        signatures.push(guardian.sign(OWNER_TRANSITION_DOMAIN, body)?);
    }
    if signatures.len() < policy.threshold as usize {
        return Err(AuthorizationError::RecoveryThreshold {
            required: policy.threshold,
            actual: signatures.len(),
        });
    }
    signatures.sort_by(|left, right| left.signer_key_id.cmp(&right.signer_key_id));
    Ok(signatures)
}

fn next_guardian_proofs(
    recovery: &RecoverySetup,
    body: &[u8],
) -> Result<Vec<AuthorizationSignature>> {
    let mut proofs = recovery
        .guardians()
        .iter()
        .map(|guardian| guardian.key().sign(OWNER_TRANSITION_DOMAIN, body))
        .collect::<Result<Vec<_>>>()?;
    proofs.sort_by(|left, right| left.signer_key_id.cmp(&right.signer_key_id));
    Ok(proofs)
}

/// Create a hot-key rotation authorized by the current and next authority.
pub fn create_rotation_transition(
    state: &VerifiedOwnerState,
    current_authority: &AuthorizationKey,
    next_authority: &AuthorizationKey,
    valid_from_unix_seconds: i64,
    previous_key_valid_until_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<SignedOwnerKeyTransition> {
    if current_authority.key_id()
        != crate::owner_authorization::canonical::key_id(state.authority_key())
        || current_authority.key_id() == next_authority.key_id()
    {
        return Err(AuthorizationError::Invalid(
            "rotation keys do not match the active state or are unchanged".to_string(),
        ));
    }
    let transition = transition_body_for(
        state,
        OwnerKeyTransitionKind::Rotate,
        next_authority,
        state.recovery_policy().clone(),
        valid_from_unix_seconds,
        previous_key_valid_until_unix_seconds,
        limits,
    )?;
    let body = transition_body(&transition)?;
    Ok(SignedOwnerKeyTransition {
        transition: Some(transition),
        authorizations: vec![current_authority.sign(OWNER_TRANSITION_DOMAIN, &body)?],
        next_authority_key_proof: Some(next_authority.sign(OWNER_TRANSITION_DOMAIN, &body)?),
        next_recovery_key_proofs: Vec::new(),
    })
}

/// Create a threshold recovery with no retired-key overlap.
pub fn create_recovery_transition(
    state: &VerifiedOwnerState,
    recovery_guardians: &[&AuthorizationKey],
    next_authority: &AuthorizationKey,
    valid_from_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<SignedOwnerKeyTransition> {
    let transition = transition_body_for(
        state,
        OwnerKeyTransitionKind::Recover,
        next_authority,
        state.recovery_policy().clone(),
        valid_from_unix_seconds,
        0,
        limits,
    )?;
    let body = transition_body(&transition)?;
    Ok(SignedOwnerKeyTransition {
        transition: Some(transition),
        authorizations: threshold_authorizations(
            state.recovery_policy(),
            recovery_guardians,
            &body,
        )?,
        next_authority_key_proof: Some(next_authority.sign(OWNER_TRANSITION_DOMAIN, &body)?),
        next_recovery_key_proofs: Vec::new(),
    })
}

/// Replace the recovery policy under both active trust anchors.
pub fn create_recovery_policy_transition(
    state: &VerifiedOwnerState,
    current_authority: &AuthorizationKey,
    current_recovery_guardians: &[&AuthorizationKey],
    next_recovery: &RecoverySetup,
    valid_from_unix_seconds: i64,
    previous_key_valid_until_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<SignedOwnerKeyTransition> {
    if current_authority.key_id()
        != crate::owner_authorization::canonical::key_id(state.authority_key())
    {
        return Err(AuthorizationError::Invalid(
            "recovery-policy signer is not the active authority".to_string(),
        ));
    }
    let transition = transition_body_for(
        state,
        OwnerKeyTransitionKind::RecoveryPolicy,
        current_authority,
        next_recovery.to_wire(current_authority)?,
        valid_from_unix_seconds,
        previous_key_valid_until_unix_seconds,
        limits,
    )?;
    let body = transition_body(&transition)?;
    let mut authorizations =
        threshold_authorizations(state.recovery_policy(), current_recovery_guardians, &body)?;
    authorizations.push(current_authority.sign(OWNER_TRANSITION_DOMAIN, &body)?);
    authorizations.sort_by(|left, right| left.signer_key_id.cmp(&right.signer_key_id));
    Ok(SignedOwnerKeyTransition {
        transition: Some(transition),
        authorizations,
        next_authority_key_proof: None,
        next_recovery_key_proofs: next_guardian_proofs(next_recovery, &body)?,
    })
}

/// Claim a deferred-human root with a new human authority and recovery policy.
pub fn create_claim_transition(
    state: &VerifiedOwnerState,
    origin_authority: &AuthorizationKey,
    next_human_authority: &AuthorizationKey,
    next_recovery: &RecoverySetup,
    valid_from_unix_seconds: i64,
    previous_key_valid_until_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<SignedOwnerKeyTransition> {
    if !state.claimable_deferred_human
        || valid_from_unix_seconds > state.claimable_until_unix_seconds
        || origin_authority.key_id()
            != crate::owner_authorization::canonical::key_id(state.authority_key())
    {
        return Err(AuthorizationError::Invalid(
            "deferred claim is not valid for the active origin state".to_string(),
        ));
    }
    let transition = transition_body_for(
        state,
        OwnerKeyTransitionKind::ClaimDeferredHuman,
        next_human_authority,
        next_recovery.to_wire(next_human_authority)?,
        valid_from_unix_seconds,
        previous_key_valid_until_unix_seconds,
        limits,
    )?;
    let body = transition_body(&transition)?;
    Ok(SignedOwnerKeyTransition {
        transition: Some(transition),
        authorizations: vec![origin_authority.sign(OWNER_TRANSITION_DOMAIN, &body)?],
        next_authority_key_proof: Some(next_human_authority.sign(OWNER_TRANSITION_DOMAIN, &body)?),
        next_recovery_key_proofs: next_guardian_proofs(next_recovery, &body)?,
    })
}
