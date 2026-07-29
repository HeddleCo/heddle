use std::collections::BTreeSet;

use crate::owner_authorization::{
    AuthorizationError, Result, VerificationLimits,
    canonical::key_id,
    wire::{
        AuthorizationKeyAlgorithm, CapabilityPrincipalKind, OwnerCapability, SpoolCapabilityAction,
        SpoolCapabilityGrant, SpoolSelector,
    },
};

pub(crate) fn validate_path_segments(segments: &[String]) -> Result<()> {
    if segments.iter().any(|segment| {
        segment.is_empty()
            || matches!(segment.as_str(), "." | "..")
            || segment.contains('/')
            || segment.contains('\0')
    }) {
        return Err(AuthorizationError::Invalid(
            "spool path contains a non-canonical segment".to_string(),
        ));
    }
    Ok(())
}

fn validate_selector(selector: &SpoolSelector) -> Result<()> {
    if selector.root_spool_uuid.len() != 16 {
        return Err(AuthorizationError::Invalid(
            "spool selector UUID must be 16 bytes".to_string(),
        ));
    }
    validate_path_segments(&selector.path_segments)
}

fn validate_grant(grant: &SpoolCapabilityGrant) -> Result<()> {
    let selector = grant.spool.as_ref().ok_or_else(|| {
        AuthorizationError::Invalid("capability grant has no selector".to_string())
    })?;
    validate_selector(selector)?;
    if grant.actions.is_empty() {
        return Err(AuthorizationError::Invalid(
            "capability grant has no actions".to_string(),
        ));
    }
    let mut previous = None;
    for value in &grant.actions {
        let action = SpoolCapabilityAction::try_from(*value)
            .ok()
            .filter(|action| *action != SpoolCapabilityAction::Unspecified)
            .ok_or_else(|| AuthorizationError::Invalid("unknown capability action".to_string()))?;
        if previous.is_some_and(|prior| prior >= *value) {
            return Err(AuthorizationError::Invalid(
                "capability actions must be unique and sorted".to_string(),
            ));
        }
        if action == SpoolCapabilityAction::Purge && selector.include_descendants {
            return Err(AuthorizationError::Invalid(
                "purge requires an exact spool selector".to_string(),
            ));
        }
        previous = Some(*value);
    }
    Ok(())
}

pub(crate) fn capability_is_well_formed(
    capability: &OwnerCapability,
    limits: VerificationLimits,
) -> Result<()> {
    if capability.format_version != 1
        || capability.owner_id.len() != 32
        || capability.issuer_state_hash.len() != 32
        || capability.nonce.len() != 32
        || capability.capability_id.len() != 32
        || capability.not_before_unix_seconds < 0
        || capability.expires_at_unix_seconds <= capability.not_before_unix_seconds
        || capability
            .expires_at_unix_seconds
            .saturating_sub(capability.not_before_unix_seconds)
            > limits.max_capability_ttl_seconds()
        || capability.grants.is_empty()
    {
        return Err(AuthorizationError::Invalid(
            "owner capability has invalid v1 fields or lifetime".to_string(),
        ));
    }
    let subject = capability.subject.as_ref().ok_or_else(|| {
        AuthorizationError::Invalid("owner capability has no subject".to_string())
    })?;
    let kind = CapabilityPrincipalKind::try_from(subject.kind)
        .ok()
        .filter(|kind| *kind != CapabilityPrincipalKind::Unspecified)
        .ok_or_else(|| AuthorizationError::Invalid("unknown capability principal".to_string()))?;
    match (kind, &subject.key) {
        (CapabilityPrincipalKind::AnyAnonymous, None) if subject.principal_id.is_empty() => {}
        (CapabilityPrincipalKind::AnyAnonymous, _) => {
            return Err(AuthorizationError::Invalid(
                "ANY_ANONYMOUS must omit key and principal id".to_string(),
            ));
        }
        (_, Some(key))
            if !subject.principal_id.is_empty()
                && key.algorithm == AuthorizationKeyAlgorithm::Ed25519 as i32
                && key.public_key.len() == 32 =>
        {
            let _ = key_id(key);
        }
        _ => {
            return Err(AuthorizationError::Invalid(
                "capability subject key or principal id is invalid".to_string(),
            ));
        }
    }
    for grant in &capability.grants {
        validate_grant(grant)?;
    }
    Ok(())
}

fn selector_covers(parent: &SpoolSelector, child: &SpoolSelector) -> bool {
    if parent.root_spool_uuid != child.root_spool_uuid {
        return false;
    }
    if parent.include_descendants {
        child
            .path_segments
            .starts_with(parent.path_segments.as_slice())
    } else {
        parent.path_segments == child.path_segments && !child.include_descendants
    }
}

pub(crate) fn grant_covers(
    parent: &[SpoolCapabilityGrant],
    child: &[SpoolCapabilityGrant],
) -> bool {
    child.iter().all(|child_grant| {
        let Some(child_selector) = child_grant.spool.as_ref() else {
            return false;
        };
        parent.iter().any(|parent_grant| {
            let Some(parent_selector) = parent_grant.spool.as_ref() else {
                return false;
            };
            let actions = parent_grant
                .actions
                .iter()
                .copied()
                .collect::<BTreeSet<_>>();
            actions.contains(&(SpoolCapabilityAction::Grant as i32))
                && child_grant
                    .actions
                    .iter()
                    .all(|action| actions.contains(action))
                && selector_covers(parent_selector, child_selector)
        })
    })
}

pub(crate) fn request_matches_selector(
    granted: &SpoolSelector,
    requested_spool_uuid: &[u8; 16],
    requested_path: &[String],
) -> bool {
    if granted.root_spool_uuid.as_slice() != requested_spool_uuid {
        return false;
    }
    if granted.include_descendants {
        requested_path.starts_with(&granted.path_segments)
    } else {
        requested_path == granted.path_segments
    }
}
