use super::encode::{Encoder, recovery_policy, verification_key};
use crate::owner_authorization::{
    AuthorizationError, Result,
    wire::{
        AnonymousKeyCredential, CapabilityPrincipal, OwnerCapability, OwnerKeyTransition,
        OwnerRoot, RegisterAnonymousKeyRequest, SpoolCapabilityGrant, SpoolSelector,
    },
};

fn required<'a, T>(value: &'a Option<T>, field: &str) -> Result<&'a T> {
    value
        .as_ref()
        .ok_or_else(|| AuthorizationError::Invalid(format!("missing required field {field}")))
}

pub(crate) fn owner_root_without_id(root: &OwnerRoot) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new();
    encoder.u32(root.format_version);
    encoder.bytes(&root.account_uuid)?;
    verification_key(
        &mut encoder,
        required(&root.authority_key, "OwnerRoot.authority_key")?,
    )?;
    recovery_policy(
        &mut encoder,
        required(&root.recovery_policy, "OwnerRoot.recovery_policy")?,
    )?;
    encoder.bool(root.claimable_deferred_human);
    encoder.bytes(&root.nonce)?;
    encoder.i64(root.claimable_until_unix_seconds);
    Ok(encoder.finish())
}

pub(crate) fn owner_root_body(root: &OwnerRoot) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new();
    encoder.u32(root.format_version);
    encoder.bytes(&root.owner_id)?;
    encoder.bytes(&root.account_uuid)?;
    verification_key(
        &mut encoder,
        required(&root.authority_key, "OwnerRoot.authority_key")?,
    )?;
    recovery_policy(
        &mut encoder,
        required(&root.recovery_policy, "OwnerRoot.recovery_policy")?,
    )?;
    encoder.bool(root.claimable_deferred_human);
    encoder.bytes(&root.nonce)?;
    encoder.i64(root.claimable_until_unix_seconds);
    Ok(encoder.finish())
}

pub(crate) fn transition_body(transition: &OwnerKeyTransition) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new();
    encoder.u32(transition.format_version);
    encoder.bytes(&transition.owner_id)?;
    encoder.bytes(&transition.previous_state_hash)?;
    encoder.u64(transition.sequence);
    encoder.i32(transition.kind);
    verification_key(
        &mut encoder,
        required(
            &transition.next_authority_key,
            "OwnerKeyTransition.next_authority_key",
        )?,
    )?;
    recovery_policy(
        &mut encoder,
        required(
            &transition.next_recovery_policy,
            "OwnerKeyTransition.next_recovery_policy",
        )?,
    )?;
    encoder.i64(transition.valid_from_unix_seconds);
    encoder.i64(transition.previous_key_valid_until_unix_seconds);
    encoder.bytes(&transition.nonce)?;
    Ok(encoder.finish())
}

fn selector(encoder: &mut Encoder, value: &SpoolSelector) -> Result<()> {
    encoder.bytes(&value.root_spool_uuid)?;
    encoder.count(value.path_segments.len())?;
    for segment in &value.path_segments {
        encoder.string(segment)?;
    }
    encoder.bool(value.include_descendants);
    Ok(())
}

fn principal(encoder: &mut Encoder, value: &CapabilityPrincipal) -> Result<()> {
    encoder.i32(value.kind);
    encoder.bytes(&value.principal_id)?;
    match &value.key {
        Some(key) => {
            encoder.bool(true);
            verification_key(encoder, key)?;
        }
        None => encoder.bool(false),
    }
    Ok(())
}

fn grant(encoder: &mut Encoder, value: &SpoolCapabilityGrant) -> Result<()> {
    selector(
        encoder,
        required(&value.spool, "SpoolCapabilityGrant.spool")?,
    )?;
    encoder.count(value.actions.len())?;
    for action in &value.actions {
        encoder.i32(*action);
    }
    Ok(())
}

fn capability_fields(capability: &OwnerCapability, include_id: bool) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new();
    encoder.u32(capability.format_version);
    encoder.bytes(&capability.owner_id)?;
    encoder.bytes(&capability.issuer_state_hash)?;
    encoder.bytes(&capability.parent_capability_id)?;
    principal(
        &mut encoder,
        required(&capability.subject, "OwnerCapability.subject")?,
    )?;
    encoder.count(capability.grants.len())?;
    for grant_value in &capability.grants {
        grant(&mut encoder, grant_value)?;
    }
    encoder.i64(capability.not_before_unix_seconds);
    encoder.i64(capability.expires_at_unix_seconds);
    encoder.bytes(&capability.nonce)?;
    if include_id {
        encoder.bytes(&capability.capability_id)?;
    }
    Ok(encoder.finish())
}

pub(crate) fn capability_without_id(capability: &OwnerCapability) -> Result<Vec<u8>> {
    capability_fields(capability, false)
}

pub(crate) fn capability_body(capability: &OwnerCapability) -> Result<Vec<u8>> {
    capability_fields(capability, true)
}

pub(crate) fn anonymous_body(credential: &AnonymousKeyCredential) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new();
    encoder.u32(credential.format_version);
    encoder.bytes(&credential.anonymous_id)?;
    verification_key(
        &mut encoder,
        required(&credential.key, "AnonymousKeyCredential.key")?,
    )?;
    encoder.i64(credential.issued_at_unix_seconds);
    encoder.i64(credential.expires_at_unix_seconds);
    encoder.bytes(&credential.nonce)?;
    Ok(encoder.finish())
}

pub(crate) fn registration_body(request: &RegisterAnonymousKeyRequest) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new();
    let credential = required(
        &request.credential,
        "RegisterAnonymousKeyRequest.credential",
    )?;
    encoder.bytes(&anonymous_body(credential)?)?;
    match &request.turnstile_token {
        Some(token) => {
            encoder.bool(true);
            encoder.string(token)?;
        }
        None => encoder.bool(false),
    }
    encoder.string(&request.prior_continuity_token)?;
    encoder.string(&request.client_operation_id)?;
    Ok(encoder.finish())
}

pub(crate) fn deferred_bootstrap_body(
    root_hash: &[u8],
    provisioning_capability_id: &[u8],
    client_operation_id: &str,
) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new();
    encoder.bytes(root_hash)?;
    encoder.bytes(provisioning_capability_id)?;
    encoder.string(client_operation_id)?;
    Ok(encoder.finish())
}
