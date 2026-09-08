// SPDX-License-Identifier: MIT OR Apache-2.0

use sha2::{Digest, Sha256};

use crate::{
    Error, Result,
    wire::{
        AuthorizationSignature, AuthorizationVerificationKey, CapabilityPrincipal, OwnerCapability,
        OwnerKeyBinding, OwnerKeyTransition, OwnerRoot, RecoveryGuardian, RecoveryPolicy,
        ResourceOwnershipTransfer, ResourceTransferAuditRecord, ResourceTransferHandoff,
        SpoolCapabilityGrant, SpoolSelector,
    },
};

pub(crate) const OWNER_ROOT_DOMAIN: &[u8] = b"heddle-owner-root-v1";
pub(crate) const OWNER_TRANSITION_DOMAIN: &[u8] = b"heddle-owner-key-transition-v1";
pub(crate) const OWNER_CAPABILITY_DOMAIN: &[u8] = b"heddle-owner-capability-v1";
pub(crate) const OWNER_BINDING_DOMAIN: &[u8] = b"heddle-owner-key-binding-v1";
pub(crate) const TRANSFER_HANDOFF_DOMAIN: &[u8] = b"heddle-resource-transfer-handoff-v1";
pub(crate) const TRANSFER_ACCEPTANCE_DOMAIN: &[u8] = b"heddle-resource-transfer-acceptance-v1";
pub(crate) const TRANSFER_AUDIT_DOMAIN: &[u8] = b"heddle-resource-transfer-audit-v1";
pub(crate) const PURGE_OPERATION_DOMAIN: &[u8] = b"heddle-purge-operation-v2";

pub(crate) fn digest(domain: &[u8], body: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(body);
    hasher.finalize().into()
}

pub(crate) fn key_id(key: &AuthorizationVerificationKey) -> [u8; 32] {
    let mut body = Vec::with_capacity(4 + key.public_key.len());
    body.extend_from_slice(&key.algorithm.to_be_bytes());
    body.extend_from_slice(&key.public_key);
    digest(b"heddle-key-v1", &body)
}

pub(crate) struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    pub(crate) const fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }

    pub(crate) fn raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    pub(crate) fn bool(&mut self, value: bool) {
        self.bytes.push(u8::from(value));
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn i32(&mut self, value: i32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn bytes(&mut self, value: &[u8]) -> Result<()> {
        self.count(value.len())?;
        self.raw(value);
        Ok(())
    }

    pub(crate) fn string(&mut self, value: &str) -> Result<()> {
        self.bytes(value.as_bytes())
    }

    pub(crate) fn count(&mut self, len: usize) -> Result<()> {
        self.u32(
            u32::try_from(len).map_err(|_| {
                Error::Invalid("canonical collection exceeds u32 length".to_owned())
            })?,
        );
        Ok(())
    }
}

fn required<'a, T>(value: &'a Option<T>, field: &str) -> Result<&'a T> {
    value
        .as_ref()
        .ok_or_else(|| Error::Invalid(format!("missing required field {field}")))
}

fn verification_key(encoder: &mut Encoder, key: &AuthorizationVerificationKey) -> Result<()> {
    encoder.i32(key.algorithm);
    encoder.bytes(&key.public_key)
}

fn signature(encoder: &mut Encoder, value: &AuthorizationSignature) -> Result<()> {
    encoder.bytes(&value.signer_key_id)?;
    encoder.bytes(&value.signature)
}

fn guardian(encoder: &mut Encoder, value: &RecoveryGuardian) -> Result<()> {
    encoder.i32(value.kind);
    verification_key(encoder, required(&value.key, "RecoveryGuardian.key")?)
}

fn recovery_policy(encoder: &mut Encoder, policy: &RecoveryPolicy) -> Result<()> {
    let ids = policy
        .guardians
        .iter()
        .map(|value| value.key.as_ref().map(key_id).unwrap_or([0; 32]))
        .collect::<Vec<_>>();
    if ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(Error::Invalid(
            "recovery guardians are not unique and sorted by key id".to_owned(),
        ));
    }
    encoder.u32(policy.threshold);
    encoder.count(policy.guardians.len())?;
    for value in &policy.guardians {
        guardian(encoder, value)?;
    }
    // Absence and an explicit seven-day value have identical contract
    // semantics. Canonical signing therefore commits to the effective value,
    // not protobuf presence encoding.
    encoder.u64(
        policy
            .window_secs
            .unwrap_or(crate::owner::DEFAULT_RECOVERY_WINDOW_SECS),
    );
    Ok(())
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
    encoder.i32(value.action);
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
    for value in &capability.grants {
        grant(&mut encoder, value)?;
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

pub(crate) fn owner_binding_body(binding: &OwnerKeyBinding) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new();
    encoder.u32(binding.format_version);
    encoder.bytes(&binding.stable_owner_uuid)?;
    verification_key(
        &mut encoder,
        required(&binding.root_public_key, "OwnerKeyBinding.root_public_key")?,
    )?;
    encoder.bytes(&binding.root_state_hash)?;
    encoder.i32(binding.kind);
    encoder.u64(binding.binding_epoch);
    encoder.bytes(&binding.challenge_nonce)?;
    Ok(encoder.finish())
}

pub(crate) fn transfer_handoff_body(handoff: &ResourceTransferHandoff) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new();
    encoder.u32(handoff.format_version);
    encoder.bytes(&handoff.resource_uuid)?;
    encoder.u64(handoff.transfer_sequence);
    encoder.bytes(&handoff.source_owner_uuid)?;
    encoder.bytes(&handoff.source_owner_key_state_hash)?;
    encoder.bytes(&handoff.destination_owner_uuid)?;
    encoder.bytes(&handoff.destination_owner_key_state_hash)?;
    encoder.bytes(&handoff.nonce)?;
    Ok(encoder.finish())
}

pub(crate) fn transfer_acceptance_body(transfer: &ResourceOwnershipTransfer) -> Result<Vec<u8>> {
    let acceptance = required(&transfer.acceptance, "ResourceOwnershipTransfer.acceptance")?;
    let signed = required(
        &acceptance.signed_handoff,
        "ResourceTransferAcceptance.signed_handoff",
    )?;
    let handoff = required(&signed.handoff, "SignedResourceTransferHandoff.handoff")?;
    let mut encoder = Encoder::new();
    encoder.bytes(&transfer_handoff_body(handoff)?)?;
    signature(
        &mut encoder,
        required(
            &signed.source_signature,
            "SignedResourceTransferHandoff.source_signature",
        )?,
    )?;
    Ok(encoder.finish())
}

pub(crate) fn transfer_audit_body(record: &ResourceTransferAuditRecord) -> Result<Vec<u8>> {
    let transfer = required(&record.transfer, "ResourceTransferAuditRecord.transfer")?;
    let acceptance = required(&transfer.acceptance, "ResourceOwnershipTransfer.acceptance")?;
    let mut encoder = Encoder::new();
    encoder.bytes(&transfer_acceptance_body(transfer)?)?;
    signature(
        &mut encoder,
        required(
            &acceptance.destination_signature,
            "ResourceTransferAcceptance.destination_signature",
        )?,
    )?;
    encoder.i64(record.committed_at_unix_seconds);
    encoder.bytes(&record.previous_audit_record_hash)?;
    Ok(encoder.finish())
}
