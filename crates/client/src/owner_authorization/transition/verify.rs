use std::collections::{BTreeMap, BTreeSet};

use crate::owner_authorization::{
    AuthorizationError, Result, VerificationLimits, VerifiedOwnerState,
    canonical::{OWNER_TRANSITION_DOMAIN, digest, key_id, transition_body},
    key::verify_signature,
    recovery::validate_wire_policy,
    root::HistoricalAuthority,
    wire::{
        AuthorizationSignature, AuthorizationVerificationKey, OwnerKeyTransitionKind,
        RecoveryPolicy, SignedOwnerKeyTransition,
    },
};

fn verify_threshold(
    policy: &RecoveryPolicy,
    signatures: &[&AuthorizationSignature],
    body: &[u8],
) -> Result<()> {
    let guardians = policy
        .guardians
        .iter()
        .filter_map(|guardian| guardian.key.as_ref())
        .map(|key| (key_id(key), key))
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    for signature in signatures {
        let id: [u8; 32] = signature
            .signer_key_id
            .as_slice()
            .try_into()
            .map_err(|_| AuthorizationError::InvalidSignature)?;
        let key = guardians
            .get(&id)
            .ok_or(AuthorizationError::InvalidSignature)?;
        if !seen.insert(id) {
            return Err(AuthorizationError::InvalidSignature);
        }
        verify_signature(key, signature, OWNER_TRANSITION_DOMAIN, body)?;
    }
    if seen.len() < policy.threshold as usize {
        return Err(AuthorizationError::RecoveryThreshold {
            required: policy.threshold,
            actual: seen.len(),
        });
    }
    Ok(())
}

fn verify_exact_signature<'a>(
    signatures: &'a [AuthorizationSignature],
    key: &AuthorizationVerificationKey,
    body: &[u8],
) -> Result<Vec<&'a AuthorizationSignature>> {
    let expected = key_id(key);
    let matching = signatures
        .iter()
        .filter(|signature| signature.signer_key_id.as_slice() == expected)
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return Err(AuthorizationError::InvalidSignature);
    }
    verify_signature(key, matching[0], OWNER_TRANSITION_DOMAIN, body)?;
    Ok(signatures
        .iter()
        .filter(|signature| signature.signer_key_id.as_slice() != expected)
        .collect())
}

fn verify_next_guardians(
    policy: &RecoveryPolicy,
    proofs: &[AuthorizationSignature],
    body: &[u8],
) -> Result<()> {
    if proofs.len() != policy.guardians.len() {
        return Err(AuthorizationError::Invalid(
            "next recovery proof count does not match policy".to_string(),
        ));
    }
    for (guardian, proof) in policy.guardians.iter().zip(proofs) {
        verify_signature(
            guardian.key.as_ref().ok_or_else(|| {
                AuthorizationError::Invalid("next guardian has no key".to_string())
            })?,
            proof,
            OWNER_TRANSITION_DOMAIN,
            body,
        )?;
    }
    Ok(())
}

/// Verify and apply exactly one owner-state transition offline.
pub fn apply_transition(
    state: &VerifiedOwnerState,
    signed: &SignedOwnerKeyTransition,
    now_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<VerifiedOwnerState> {
    let transition = signed
        .transition
        .as_ref()
        .ok_or_else(|| AuthorizationError::Invalid("signed transition has no body".to_string()))?;
    if transition.format_version != 1
        || transition.owner_id.as_slice() != state.owner_id()
        || transition.previous_state_hash.as_slice() != state.state_hash()
        || transition.sequence != state.sequence() + 1
        || transition.nonce.len() != 32
    {
        return Err(AuthorizationError::BrokenChain(
            "owner id, previous state hash, or sequence does not match".to_string(),
        ));
    }
    if transition.valid_from_unix_seconds < 0
        || transition.previous_key_valid_until_unix_seconds < 0
        || (transition.previous_key_valid_until_unix_seconds > 0
            && transition.previous_key_valid_until_unix_seconds
                < transition.valid_from_unix_seconds)
        || transition
            .previous_key_valid_until_unix_seconds
            .saturating_sub(transition.valid_from_unix_seconds)
            > limits.max_capability_ttl_seconds()
    {
        return Err(AuthorizationError::Invalid(
            "transition handover exceeds the capability TTL ceiling".to_string(),
        ));
    }
    if now_unix_seconds < transition.valid_from_unix_seconds {
        return Err(AuthorizationError::NotYetValid);
    }
    let kind = OwnerKeyTransitionKind::try_from(transition.kind)
        .ok()
        .filter(|kind| *kind != OwnerKeyTransitionKind::Unspecified)
        .ok_or_else(|| AuthorizationError::Invalid("unknown transition kind".to_string()))?;
    let next_authority = transition.next_authority_key.as_ref().ok_or_else(|| {
        AuthorizationError::Invalid("transition has no next authority".to_string())
    })?;
    let next_policy = transition.next_recovery_policy.as_ref().ok_or_else(|| {
        AuthorizationError::Invalid("transition has no next recovery policy".to_string())
    })?;
    validate_wire_policy(next_policy, &key_id(next_authority), false)?;
    let body = transition_body(transition)?;

    match kind {
        OwnerKeyTransitionKind::Rotate => {
            if next_policy != state.recovery_policy()
                || signed.authorizations.len() != 1
                || !signed.next_recovery_key_proofs.is_empty()
            {
                return Err(AuthorizationError::Invalid(
                    "rotation changed recovery policy or has extra proofs".to_string(),
                ));
            }
            verify_signature(
                state.authority_key(),
                &signed.authorizations[0],
                OWNER_TRANSITION_DOMAIN,
                &body,
            )?;
            verify_signature(
                next_authority,
                signed
                    .next_authority_key_proof
                    .as_ref()
                    .ok_or(AuthorizationError::InvalidSignature)?,
                OWNER_TRANSITION_DOMAIN,
                &body,
            )?;
        }
        OwnerKeyTransitionKind::Recover => {
            if next_policy != state.recovery_policy()
                || transition.previous_key_valid_until_unix_seconds != 0
                || !signed.next_recovery_key_proofs.is_empty()
            {
                return Err(AuthorizationError::Invalid(
                    "recovery changed policy or retained compromised authority".to_string(),
                ));
            }
            verify_threshold(
                state.recovery_policy(),
                &signed.authorizations.iter().collect::<Vec<_>>(),
                &body,
            )?;
            verify_signature(
                next_authority,
                signed
                    .next_authority_key_proof
                    .as_ref()
                    .ok_or(AuthorizationError::InvalidSignature)?,
                OWNER_TRANSITION_DOMAIN,
                &body,
            )?;
        }
        OwnerKeyTransitionKind::RecoveryPolicy => {
            if next_authority != state.authority_key() || signed.next_authority_key_proof.is_some()
            {
                return Err(AuthorizationError::Invalid(
                    "recovery-policy transition changed authority".to_string(),
                ));
            }
            let guardian_signatures =
                verify_exact_signature(&signed.authorizations, state.authority_key(), &body)?;
            verify_threshold(state.recovery_policy(), &guardian_signatures, &body)?;
            verify_next_guardians(next_policy, &signed.next_recovery_key_proofs, &body)?;
        }
        OwnerKeyTransitionKind::ClaimDeferredHuman => {
            if !state.claimable_deferred_human
                || now_unix_seconds > state.claimable_until_unix_seconds
                || transition.valid_from_unix_seconds > state.claimable_until_unix_seconds
                || signed.authorizations.len() != 1
            {
                return Err(AuthorizationError::Invalid(
                    "claim transition does not originate from a claimable state".to_string(),
                ));
            }
            verify_signature(
                state.authority_key(),
                &signed.authorizations[0],
                OWNER_TRANSITION_DOMAIN,
                &body,
            )?;
            verify_signature(
                next_authority,
                signed
                    .next_authority_key_proof
                    .as_ref()
                    .ok_or(AuthorizationError::InvalidSignature)?,
                OWNER_TRANSITION_DOMAIN,
                &body,
            )?;
            verify_next_guardians(next_policy, &signed.next_recovery_key_proofs, &body)?;
        }
        OwnerKeyTransitionKind::Unspecified => unreachable!(),
    }

    let next_hash = digest(OWNER_TRANSITION_DOMAIN, &body);
    let mut next = state.clone();
    next.sequence = transition.sequence;
    next.state_hash = next_hash;
    next.authority_key = next_authority.clone();
    next.recovery_policy = next_policy.clone();
    if kind == OwnerKeyTransitionKind::ClaimDeferredHuman {
        next.claimable_deferred_human = false;
    }
    next.issuers
        .get_mut(&state.state_hash())
        .expect("verified current issuer")
        .valid_until = Some(transition.previous_key_valid_until_unix_seconds);
    next.issuers.insert(
        next_hash,
        HistoricalAuthority {
            key: next_authority.clone(),
            valid_until: None,
        },
    );
    Ok(next)
}
