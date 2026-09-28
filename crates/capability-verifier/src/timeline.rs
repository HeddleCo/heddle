// SPDX-License-Identifier: MIT OR Apache-2.0
//! Verification of the format-3 owner route for one exact timeline acceptance.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use prost::Message;

use crate::{
    Error, Result, VerificationLimits, VerifiedAuthorizationBundle,
    capability::verify_timeline_bundle_for_state,
    wire::{
        OwnerAuthorizationBundle, TimelineAdmissionAcceptance, TimelineOriginEndorsement,
        timeline_admission_acceptance::Authority,
    },
};

/// Independently accepted state, request, and revocation evidence at admission.
pub struct TimelineAcceptanceContext<'a> {
    /// Current owner state hash from the account's pinned state.
    pub accepted_state_hash: &'a [u8; 32],
    /// Exact canonical Spool path from the verified origin's Thread.
    pub spool_path_segments: &'a [String],
    /// Digest of the actual logical upload request.
    pub request_sha256: &'a [u8; 32],
    /// First position of the actual upload request.
    pub first_position: u64,
    /// Number of events in the actual upload request.
    pub event_count: u32,
    /// Current capability revocation IDs from the trusted registry.
    pub revoked_capability_ids: &'a [Vec<u8>],
    /// Current subject Biscuit revocation IDs from the trusted registry.
    pub revoked_subject_ids: &'a [Vec<u8>],
    /// Admission time, supplied by the caller.
    pub now_unix_seconds: i64,
    /// Admission limits.
    pub limits: VerificationLimits,
}

/// Verify a current owner capability and its subject signature for a verified origin.
///
/// The caller must independently verify origin credential provenance, origin
/// signature, uploader proof, and the current account state and revocation sets.
/// The bundle embedded in `acceptance` is decoded canonically here.
pub fn verify_timeline_acceptance(
    origin: &TimelineOriginEndorsement,
    acceptance: &TimelineAdmissionAcceptance,
    context: &TimelineAcceptanceContext<'_>,
) -> Result<VerifiedAuthorizationBundle> {
    heddle_api::timeline_upload::validate_origin(origin)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    heddle_api::timeline_upload::validate_acceptance(acceptance)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    let bytes = match acceptance.authority.as_ref() {
        Some(Authority::OwnerDerivedCapability(bytes)) if (1..=4096).contains(&bytes.len()) => {
            bytes
        }
        _ => {
            return Err(Error::CapabilityDenied(
                "acceptance has no owner-derived authority".to_owned(),
            ));
        }
    };
    let bundle = OwnerAuthorizationBundle::decode(bytes.as_slice())?;
    if bundle.encode_to_vec() != *bytes {
        return Err(Error::NonCanonicalProtobuf);
    }
    let origin_digest = heddle_api::timeline_upload::origin_digest(origin)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    if acceptance.origin_sha256 != origin_digest
        || acceptance.uploader_device_public_key != origin.uploader_device_public_key
        || acceptance.deployment_public_key != origin.deployment_public_key
        || acceptance.request_sha256 != context.request_sha256
        || acceptance.first_position != context.first_position
        || acceptance.event_count != context.event_count
    {
        return Err(Error::CapabilityDenied(
            "acceptance differs from the verified upload".to_owned(),
        ));
    }
    let principal_uuid = hex::decode(origin.principal_id.replace('-', ""))
        .map_err(|error| Error::Invalid(error.to_string()))?;
    let spool_uuid = hex::decode(origin.spool_id.replace('-', ""))
        .map_err(|error| Error::Invalid(error.to_string()))?;
    let root = bundle
        .owner_root
        .as_ref()
        .and_then(|signed| signed.root.as_ref())
        .ok_or_else(|| Error::Invalid("owner root missing".to_owned()))?;
    if root.account_uuid != principal_uuid {
        return Err(Error::CapabilityDenied(
            "owner root belongs to another principal".to_owned(),
        ));
    }
    let verified = verify_timeline_bundle_for_state(
        &bundle,
        context.accepted_state_hash,
        context.now_unix_seconds,
        context.limits,
        context.revoked_subject_ids,
    )?;
    let capability = verified.capability().capability();
    if context
        .revoked_capability_ids
        .iter()
        .any(|id| id == &capability.capability_id)
    {
        return Err(Error::CapabilityDenied(
            "owner capability is revoked".to_owned(),
        ));
    }
    let grant = &capability.grants[0];
    let selector = grant
        .spool
        .as_ref()
        .ok_or_else(|| Error::Invalid("timeline selector missing".to_owned()))?;
    let scope = grant
        .timeline_acceptance
        .as_ref()
        .ok_or_else(|| Error::Invalid("timeline scope missing".to_owned()))?;
    if selector.root_spool_uuid != spool_uuid
        || selector.path_segments != context.spool_path_segments
        || scope.principal_account_uuid != principal_uuid
        || scope.credential_identity != origin.credential_identity
        || scope.effective_pop_key_sha256 != origin.effective_pop_key_sha256
        || scope.credential_class != origin.credential_class as u32
        || scope.thread_id != origin.thread_id
        || scope.origin_sha256 != origin_digest
    {
        return Err(Error::CapabilityDenied(
            "timeline grant differs from the verified origin or Thread".to_owned(),
        ));
    }
    let subject_key = capability
        .subject
        .as_ref()
        .and_then(|subject| subject.key.as_ref())
        .ok_or_else(|| Error::Invalid("timeline subject key missing".to_owned()))?;
    let key_bytes: &[u8; 32] = subject_key
        .public_key
        .as_slice()
        .try_into()
        .map_err(|_| Error::InvalidSignature)?;
    let signature_bytes: &[u8; 64] = acceptance
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| Error::InvalidSignature)?;
    let message = heddle_api::timeline_upload::acceptance_signing_bytes(acceptance)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    VerifyingKey::from_bytes(key_bytes)
        .map_err(|_| Error::InvalidSignature)?
        .verify(&message, &Signature::from_bytes(signature_bytes))
        .map_err(|_| Error::InvalidSignature)?;
    Ok(verified)
}
