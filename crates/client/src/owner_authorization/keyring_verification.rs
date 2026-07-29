use crate::owner_authorization::{
    AuthorizationError, Result, VerificationLimits, apply_transition,
    capability::{request_matches_selector, verify_capability_chain},
    keyring::VerifiedCloneKeyring,
    root::verify_owner_root,
    wire::{CloneAuthorizationKeyring, CloneOwnerPinKind},
};

fn validate_clone_path(path: &[String]) -> Result<()> {
    if path.iter().any(|segment| {
        segment.is_empty()
            || matches!(segment.as_str(), "." | "..")
            || segment.contains('/')
            || segment.contains('\0')
    }) {
        return Err(AuthorizationError::Invalid(
            "clone keyring has a non-canonical spool path".to_string(),
        ));
    }
    Ok(())
}

fn verify_pin(keyring: &CloneAuthorizationKeyring) -> Result<()> {
    let pin = keyring
        .pin
        .as_ref()
        .ok_or_else(|| AuthorizationError::Invalid("clone keyring has no owner pin".to_string()))?;
    let pin_kind = CloneOwnerPinKind::try_from(pin.kind)
        .ok()
        .filter(|kind| *kind != CloneOwnerPinKind::Unspecified)
        .ok_or_else(|| AuthorizationError::Invalid("unknown clone pin kind".to_string()))?;
    if !matches!(
        pin_kind,
        CloneOwnerPinKind::LocalCreation | CloneOwnerPinKind::InvitationFingerprint
    ) || pin.expected_owner_id.len() != 32
        || pin.first_seen_unix_seconds < 0
    {
        return Err(AuthorizationError::Invalid(
            "clone owner pin is invalid".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn verify_clone_keyring(
    keyring: CloneAuthorizationKeyring,
    now_unix_seconds: i64,
    limits: VerificationLimits,
) -> Result<VerifiedCloneKeyring> {
    if keyring.format_version != 1
        || keyring.spool_uuid.len() != 16
        || keyring.accepted_state_hash.len() != 32
    {
        return Err(AuthorizationError::Invalid(
            "clone keyring has invalid v1 field lengths".to_string(),
        ));
    }
    validate_clone_path(&keyring.canonical_spool_path_segments)?;
    verify_pin(&keyring)?;

    let pin = keyring.pin.as_ref().expect("verified pin");
    let mut state = verify_owner_root(keyring.owner_root.as_ref().ok_or_else(|| {
        AuthorizationError::Invalid("clone keyring has no owner root".to_string())
    })?)?;
    if pin.expected_owner_id.as_slice() != state.owner_id() {
        return Err(AuthorizationError::Invalid(
            "clone owner pin does not match the signed root".to_string(),
        ));
    }
    for transition in &keyring.accepted_transitions {
        state = apply_transition(&state, transition, now_unix_seconds, limits)?;
    }
    if keyring.accepted_state_hash.as_slice() != state.state_hash() {
        return Err(AuthorizationError::BrokenChain(
            "accepted state hash does not match transition history".to_string(),
        ));
    }

    let spool_uuid: [u8; 16] = keyring.spool_uuid.as_slice().try_into().expect("checked");
    let mut capabilities = Vec::new();
    for signed in &keyring.public_access_capabilities {
        let verified = verify_capability_chain(
            &state,
            std::slice::from_ref(signed),
            now_unix_seconds,
            limits,
        )?;
        let capability = verified[0].capability();
        if !capability.grants.iter().all(|grant| {
            grant.spool.as_ref().is_some_and(|selector| {
                request_matches_selector(
                    selector,
                    &spool_uuid,
                    &keyring.canonical_spool_path_segments,
                )
            })
        }) {
            return Err(AuthorizationError::Invalid(
                "keyring contains a capability for another spool".to_string(),
            ));
        }
        capabilities.extend(verified);
    }
    Ok(VerifiedCloneKeyring {
        wire: keyring,
        owner_state: state,
        capabilities,
    })
}
