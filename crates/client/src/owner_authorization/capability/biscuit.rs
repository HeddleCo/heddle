use std::collections::BTreeSet;

use biscuit_auth::{Biscuit, PublicKey, builder::Algorithm};

use crate::owner_authorization::{
    AuthorizationError, AuthorizationKey, Result,
    canonical::key_id,
    wire::{CapabilityPrincipalKind, OwnerCapability},
};

fn path_hex(segments: &[String]) -> String {
    let mut bytes = Vec::new();
    for segment in segments {
        bytes.extend_from_slice(&(segment.len() as u32).to_be_bytes());
        bytes.extend_from_slice(segment.as_bytes());
    }
    hex::encode(bytes)
}

fn expected_grants(capability: &OwnerCapability) -> Result<BTreeSet<(String, String, bool, i64)>> {
    let mut expected = BTreeSet::new();
    for grant in &capability.grants {
        let selector = grant.spool.as_ref().ok_or_else(|| {
            AuthorizationError::Invalid("capability grant has no selector".to_string())
        })?;
        for action in &grant.actions {
            expected.insert((
                hex::encode(&selector.root_spool_uuid),
                path_hex(&selector.path_segments),
                selector.include_descendants,
                i64::from(*action),
            ));
        }
    }
    Ok(expected)
}

/// Mint the subject-signed Biscuit carried in an authorization bundle.
pub fn mint_subject_biscuit(
    capability: &OwnerCapability,
    subject_key: &AuthorizationKey,
) -> Result<Vec<u8>> {
    let subject = capability
        .subject
        .as_ref()
        .ok_or_else(|| AuthorizationError::Invalid("capability has no subject".to_string()))?;
    let public = subject.key.as_ref().ok_or_else(|| {
        AuthorizationError::Invalid("ANY_ANONYMOUS has no subject signing key".to_string())
    })?;
    if subject_key.key_id() != key_id(public) {
        return Err(AuthorizationError::Invalid(
            "Biscuit signer does not match the capability subject".to_string(),
        ));
    }
    let mut builder = Biscuit::builder()
        .fact(
            format!(
                "owner_subject({}, \"{}\", \"{}\")",
                subject.kind,
                hex::encode(&subject.principal_id),
                hex::encode(key_id(public))
            )
            .as_str(),
        )
        .map_err(|error| AuthorizationError::Biscuit(error.to_string()))?
        .fact(
            format!(
                "owner_capability(\"{}\")",
                hex::encode(&capability.capability_id)
            )
            .as_str(),
        )
        .map_err(|error| AuthorizationError::Biscuit(error.to_string()))?
        .fact(
            format!(
                "owner_validity({}, {})",
                capability.not_before_unix_seconds, capability.expires_at_unix_seconds
            )
            .as_str(),
        )
        .map_err(|error| AuthorizationError::Biscuit(error.to_string()))?;
    for (spool, path, descendants, action) in expected_grants(capability)? {
        builder = builder
            .fact(format!("owner_grant(\"{spool}\", \"{path}\", {descendants}, {action})").as_str())
            .map_err(|error| AuthorizationError::Biscuit(error.to_string()))?;
    }
    builder
        .build(&subject_key.biscuit_key_pair()?)
        .and_then(|biscuit| biscuit.to_vec())
        .map_err(|error| AuthorizationError::Biscuit(error.to_string()))
}

pub(crate) fn verify_subject_biscuit(capability: &OwnerCapability, bytes: &[u8]) -> Result<()> {
    let subject = capability
        .subject
        .as_ref()
        .ok_or_else(|| AuthorizationError::Invalid("capability has no subject".to_string()))?;
    let kind = CapabilityPrincipalKind::try_from(subject.kind)
        .map_err(|_| AuthorizationError::Invalid("unknown subject kind".to_string()))?;
    if kind == CapabilityPrincipalKind::AnyAnonymous {
        if bytes.is_empty() {
            return Ok(());
        }
        return Err(AuthorizationError::Invalid(
            "ANY_ANONYMOUS bundle must not claim a subject-signed Biscuit".to_string(),
        ));
    }
    let key = subject
        .key
        .as_ref()
        .ok_or_else(|| AuthorizationError::Invalid("capability subject has no key".to_string()))?;
    let public = PublicKey::from_bytes(&key.public_key, Algorithm::Ed25519)
        .map_err(|error| AuthorizationError::Biscuit(error.to_string()))?;
    let biscuit = Biscuit::from(bytes, move |_| Ok(public))
        .map_err(|error| AuthorizationError::Biscuit(error.to_string()))?;
    let mut authorizer = biscuit
        .authorizer()
        .map_err(|error| AuthorizationError::Biscuit(error.to_string()))?;

    let subjects: Vec<(i64, String, String)> = authorizer
        .query("owner_subject($kind, $id, $key) <- owner_subject($kind, $id, $key)")
        .map_err(|error| AuthorizationError::Biscuit(error.to_string()))?;
    let expected_subject = (
        i64::from(subject.kind),
        hex::encode(&subject.principal_id),
        hex::encode(key_id(key)),
    );
    if subjects.as_slice() != [expected_subject] {
        return Err(AuthorizationError::CapabilityDenied(
            "subject Biscuit identity does not match the leaf".to_string(),
        ));
    }
    let ids: Vec<(String,)> = authorizer
        .query("owner_capability($id) <- owner_capability($id)")
        .map_err(|error| AuthorizationError::Biscuit(error.to_string()))?;
    if ids.as_slice() != [(hex::encode(&capability.capability_id),)] {
        return Err(AuthorizationError::CapabilityDenied(
            "subject Biscuit names another capability".to_string(),
        ));
    }
    let validity: Vec<(i64, i64)> = authorizer
        .query("owner_validity($not_before, $expires) <- owner_validity($not_before, $expires)")
        .map_err(|error| AuthorizationError::Biscuit(error.to_string()))?;
    if validity.as_slice()
        != [(
            capability.not_before_unix_seconds,
            capability.expires_at_unix_seconds,
        )]
    {
        return Err(AuthorizationError::CapabilityDenied(
            "subject Biscuit widens the validity interval".to_string(),
        ));
    }
    let grants: BTreeSet<(String, String, bool, i64)> = authorizer
        .query::<_, (String, String, bool, i64), _>(
            "owner_grant($spool, $path, $desc, $action) <- \
             owner_grant($spool, $path, $desc, $action)",
        )
        .map_err(|error| AuthorizationError::Biscuit(error.to_string()))?
        .into_iter()
        .collect();
    if grants != expected_grants(capability)? {
        return Err(AuthorizationError::CapabilityDenied(
            "subject Biscuit grants differ from the leaf capability".to_string(),
        ));
    }
    Ok(())
}
