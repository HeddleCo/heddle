// SPDX-License-Identifier: MIT OR Apache-2.0

use prost::Message;
use sha2::{Digest, Sha256};

use crate::{
    Decision, Denial, Error, Result, VerificationLimits,
    canonical::{Encoder, PURGE_OPERATION_DOMAIN},
    capability::{capability_allows_purge, validate_path_segments, verify_authorization_bundle},
    crypto::verify_signature,
    owner::verify_spool_owner_genesis,
    wire::{PurgeOperationSigningBody, SidecarAuthorization, SignedSpoolOwnerGenesis},
};

/// Caller-established spool/owner context for one purge decision.
pub struct PurgeContext<'a> {
    /// Self-signed genesis evidence already selected by the caller's TOFU pin.
    pub owner_genesis: &'a SignedSpoolOwnerGenesis,
    /// Current owner-state hash from the caller's authoritative state tree.
    pub current_owner_state_hash: &'a [u8; 32],
    /// Exact spool UUID being purged.
    pub spool_uuid: &'a [u8; 16],
    /// Exact canonical spool path used for selector matching.
    pub spool_path_segments: &'a [String],
    /// Caller-supplied evaluation time in Unix seconds.
    pub now_unix_seconds: i64,
    /// Product TTL and fixed v2 parsing ceilings.
    pub limits: VerificationLimits,
}

fn lower_hex(value: &str, expected: usize) -> bool {
    value.len() == expected
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Construct the exact canonical purge-operation v2 body.
pub fn canonical_purge_operation(body: &PurgeOperationSigningBody) -> Result<Vec<u8>> {
    if body.format_version != 2
        || body.spool_uuid.len() != 16
        || body.payload_sha256.len() != 32
        || body.leaf_capability_id.len() != 32
    {
        return Err(Error::Invalid(
            "purge operation has invalid v2 fixed-width fields".to_owned(),
        ));
    }
    let identity = body
        .purge_identity
        .as_ref()
        .ok_or_else(|| Error::Invalid("purge operation has no identity".to_owned()))?;
    if !lower_hex(&identity.blob_hash, 64) {
        return Err(Error::Invalid(
            "purge blob hash is not canonical lower hex".to_owned(),
        ));
    }
    let mut encoder = Encoder::new();
    encoder.u32(body.format_version);
    encoder.raw(&body.spool_uuid);
    encoder.string(&identity.blob_hash)?;
    encoder.raw(&body.payload_sha256);
    encoder.raw(&body.leaf_capability_id);
    Ok(encoder.finish())
}

fn decision_result(
    authorization: &SidecarAuthorization,
    body: &PurgeOperationSigningBody,
    raw_payload: &[u8],
    context: &PurgeContext<'_>,
) -> Result<Decision> {
    if raw_payload.len() > context.limits.max_payload_bytes() {
        return Err(Error::TooLarge {
            limit: context.limits.max_payload_bytes(),
        });
    }
    validate_path_segments(context.spool_path_segments)?;
    let canonical_body = canonical_purge_operation(body)?;
    let payload_hash: [u8; 32] = Sha256::digest(raw_payload).into();
    if body.spool_uuid.as_slice() != context.spool_uuid
        || body.payload_sha256.as_slice() != payload_hash
    {
        return Err(Error::CapabilityDenied(
            "operation spool or payload digest does not match caller input".to_owned(),
        ));
    }

    let genesis = verify_spool_owner_genesis(context.owner_genesis)?;
    if genesis.spool_uuid() != *context.spool_uuid {
        return Err(Error::BrokenChain(
            "genesis binding names another spool".to_owned(),
        ));
    }
    let bundle = authorization
        .capability
        .as_ref()
        .ok_or_else(|| Error::CapabilityDenied("purge capability is absent".to_owned()))?;
    let verified = verify_authorization_bundle(bundle, context.now_unix_seconds, context.limits)?;
    let root_key = bundle
        .owner_root
        .as_ref()
        .and_then(|signed| signed.root.as_ref())
        .and_then(|root| root.authority_key.as_ref())
        .expect("verified owner root authority");
    if root_key != genesis.owner_public_key() {
        return Err(Error::BrokenChain(
            "owner root does not match the pinned spool genesis".to_owned(),
        ));
    }
    if verified.owner_state().state_hash() != *context.current_owner_state_hash {
        return Err(Error::BrokenChain(
            "bundle does not end at the caller's current owner state".to_owned(),
        ));
    }
    let capability = verified.capability().capability();
    if body.leaf_capability_id != capability.capability_id {
        return Err(Error::CapabilityDenied(
            "operation names another leaf capability".to_owned(),
        ));
    }
    if !capability_allows_purge(capability, context.spool_uuid, context.spool_path_segments) {
        return Err(Error::CapabilityDenied(
            "direct capability does not grant purge for this spool target".to_owned(),
        ));
    }
    let subject_key = capability
        .subject
        .as_ref()
        .and_then(|subject| subject.key.as_ref())
        .ok_or_else(|| Error::CapabilityDenied("purge subject has no signing key".to_owned()))?;
    verify_signature(
        subject_key,
        authorization
            .operation_signature
            .as_ref()
            .ok_or(Error::InvalidSignature)?,
        PURGE_OPERATION_DOMAIN,
        &canonical_body,
    )?;
    Ok(Decision::Purge)
}

fn denial(error: &Error) -> Denial {
    match error {
        Error::TooLarge { .. } => Denial::OverLimit,
        Error::InvalidSignature | Error::Biscuit(_) | Error::RecoveryThreshold { .. } => {
            Denial::InvalidProof
        }
        Error::Expired | Error::NotYetValid => Denial::Time,
        Error::BrokenChain(message) if message.contains("current owner state") => {
            Denial::StaleOwner
        }
        Error::BrokenChain(message)
            if message.contains("genesis") || message.contains("pinned spool") =>
        {
            Denial::GenesisBinding
        }
        Error::BrokenChain(_) => Denial::InvalidProof,
        Error::CapabilityDenied(message) if message.contains("direct-only") => Denial::DirectOnly,
        Error::CapabilityDenied(message)
            if message.contains("operation spool")
                || message.contains("payload")
                || message.contains("leaf capability") =>
        {
            Denial::OperationBinding
        }
        Error::CapabilityDenied(_) => Denial::Capability,
        Error::Invalid(_) | Error::NonCanonicalProtobuf | Error::Decode(_) => Denial::Malformed,
    }
}

/// Verify one typed purge authorization and return an allow/deny decision.
#[must_use]
pub fn verify_purge_authorization(
    authorization: &SidecarAuthorization,
    body: &PurgeOperationSigningBody,
    raw_payload: &[u8],
    context: &PurgeContext<'_>,
) -> Decision {
    match decision_result(authorization, body, raw_payload, context) {
        Ok(decision) => decision,
        Err(error) => Decision::Deny(denial(&error)),
    }
}

/// Decode canonical protobuf under v2 ceilings and verify one purge authorization.
#[must_use]
pub fn verify_purge_authorization_bytes(
    authorization_bytes: &[u8],
    body: &PurgeOperationSigningBody,
    raw_payload: &[u8],
    context: &PurgeContext<'_>,
) -> Decision {
    if authorization_bytes.len() > context.limits.max_bundle_bytes().saturating_add(256) {
        return Decision::Deny(Denial::OverLimit);
    }
    let Ok(authorization) = SidecarAuthorization::decode(authorization_bytes) else {
        return Decision::Deny(Denial::Malformed);
    };
    if authorization.encode_to_vec() != authorization_bytes {
        return Decision::Deny(Denial::Malformed);
    }
    verify_purge_authorization(&authorization, body, raw_payload, context)
}
